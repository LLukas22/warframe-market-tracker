use dioxus::prelude::*;

use crate::api::{
    self, AttrInfo, CurseFilter, DAY_MS, HOUR_MS, Listing, LiveStats, Overview, Period, Query,
    SIMILAR_LIMIT, SearchResult, Sort, attribute_name, format_value,
};
use crate::chart::{PriceChart, StatEffectsPanel};
use crate::weapons::{WeaponGrid, WeaponHeader, WeaponImage, WeaponPicker};
use wfmt_types::{MINUTE_MS, SECOND_MS, riven};

const CSS: Asset = asset!("/assets/main.css");

/// `YYYY-MM-DD` (UTC) from unix ms.
pub fn format_date(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(DAY_MS));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `HH:MM` (UTC) from unix ms.
pub fn format_time(ms: i64) -> String {
    let time = ms.rem_euclid(DAY_MS);
    format!("{:02}:{:02}", time / HOUR_MS, time % HOUR_MS / MINUTE_MS)
}

/// `YYYY-MM-DD HH:MM` (UTC) from unix ms.
pub fn format_datetime(ms: i64) -> String {
    format!("{} {}", format_date(ms), format_time(ms))
}

/// `MM-DD HH:MM` (UTC) from unix ms.
pub fn format_short_datetime(ms: i64) -> String {
    let (_, m, d) = civil_from_days(ms.div_euclid(DAY_MS));
    format!("{m:02}-{d:02} {}", format_time(ms))
}

/// Unix ms of the UTC midnight starting a `YYYY-MM-DD` date, as date inputs give it.
pub fn parse_date(s: &str) -> Option<i64> {
    let mut parts = s.trim().splitn(3, '-').map(str::parse::<i64>);
    let (y, m, d) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's civil date -> days algorithm.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146_097 + doe - 719_468) * DAY_MS)
}

// Howard Hinnant's days -> civil date algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// From here on prices are shortened to thousands, `12.5k`.
const PLAT_IN_THOUSANDS: i64 = 10_000;

pub fn format_plat(p: i64) -> String {
    if p >= PLAT_IN_THOUSANDS {
        format!("{:.1}k", p as f64 / 1000.0)
    } else {
        format!("{p}")
    }
}

pub fn format_age(now: i64, then: i64) -> String {
    let age = (now - then).max(0);
    if age < HOUR_MS {
        format!("{}m", age / MINUTE_MS)
    } else if age < DAY_MS {
        format!("{}h", age / HOUR_MS)
    } else {
        format!("{}d", age / DAY_MS)
    }
}

/// Sleeps in the browser; on the server (where polling loops never need to fire) it uses tokio.
pub async fn sleep_ms(ms: u32) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms).await;
    #[cfg(all(not(target_arch = "wasm32"), feature = "server"))]
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "server")))]
    {
        let _ = ms;
        std::future::pending::<()>().await;
    }
}

/// How often the header checks for new scraped data.
const LIVE_POLL_MS: u32 = 5 * SECOND_MS as u32;

#[derive(Clone, Debug, PartialEq, Routable)]
pub(crate) enum Route {
    #[layout(Shell)]
    #[route("/")]
    Market {},
    #[route("/weapons/:slug")]
    Weapon { slug: String },
    #[route("/search/:id")]
    Search { id: String },
    #[route("/riven/:id?:auction")]
    RivenPage { id: String, auction: String },
    /// An editable riven read from a screenshot (see `RivenScan::to_param`).
    #[route("/scanned?:scan")]
    ScannedRivenPage { scan: String },
}

#[component]
pub fn App() -> Element {
    rsx! { Router::<Route> {} }
}

/// How many past requests of each kind the view cache keeps.
const VIEW_CACHE_SIZE: usize = 12;
/// How long a cached answer is used without asking the server, while the data version is
/// unchanged. Ages like "seen 5m ago" are relative to the answer's server time, so this also
/// bounds how far those lag.
const FRESH_MS: f64 = MINUTE_MS as f64;
/// How often a request waits to see whether the same one, already under way, has answered.
const IN_FLIGHT_POLL_MS: u32 = 20;

/// Milliseconds on the browser's clock. Only compared with itself; on the server, where the
/// cache only carries the embedded first answers, everything counts as fresh.
fn client_now() -> f64 {
    #[cfg(target_arch = "wasm32")]
    return js_sys::Date::now();
    #[cfg(not(target_arch = "wasm32"))]
    0.0
}

/// Data for a page's first render, embedded in the server's HTML (see `api::seed`). The
/// hydrating browser reads it back, so its first render matches the server's; later mounts
/// get `None` and load as usual. Like any hook, call it unconditionally.
#[track_caller]
pub(crate) fn use_seed<T>(seed: impl Fn() -> Option<T>) -> Option<T>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Clone + 'static,
{
    use_server_cached(seed)
}

struct Entry<K, V> {
    key: K,
    value: V,
    version: u64,
    at: f64,
}

/// Recent answers, shared by every page. Pages remount on navigation; with this, a revisited
/// view renders its last data at once, and needs no request at all while that data is fresh.
/// It also lets a hover start loading a weapon before the click (see `prefetch_weapon`), and
/// makes concurrent loads of the same thing share one request.
#[derive(Clone, Copy)]
pub(crate) struct ViewCache {
    /// The data version (see `Shell`): answers from an older one are stale.
    version: Signal<u64>,
    searches: Signal<Vec<Entry<Query, SearchResult>>>,
    effects: Signal<Vec<Entry<String, api::StatEffects>>>,
    /// Requests under way, by `flight_key`.
    in_flight: Signal<Vec<String>>,
}

/// Removes a request from `in_flight` when it ends, also when its task is cancelled.
struct Flight(Signal<Vec<String>>, String);

impl Drop for Flight {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = self.0.try_write() {
            in_flight.retain(|k| k != &self.1);
        }
    }
}

// Everything reads with `peek`: the cache feeds views but never triggers renders itself.
impl ViewCache {
    fn new(version: Signal<u64>) -> Self {
        Self {
            version,
            searches: Signal::new(Vec::new()),
            effects: Signal::new(Vec::new()),
            in_flight: Signal::new(Vec::new()),
        }
    }

    /// The last answer to `query`, however old.
    fn search(&self, query: &Query) -> Option<SearchResult> {
        self.lookup(self.searches, query, false)
    }

    fn store_search(&self, query: Query, result: SearchResult) {
        self.remember(self.searches, query, result, *self.version.peek());
    }

    /// Answers `query` from the cache if fresh, else from the server, sharing a request
    /// already under way.
    async fn load_search(self, query: Query) -> Result<SearchResult, ServerFnError> {
        let flight = format!("search {query:?}");
        let fetch = api::search(query.clone());
        self.load(self.searches, query, flight, fetch).await
    }

    pub(crate) fn effects(&self, weapon: &str) -> Option<api::StatEffects> {
        self.lookup(self.effects, &weapon.to_owned(), false)
    }

    pub(crate) fn store_effects(&self, weapon: String, effects: api::StatEffects) {
        self.remember(self.effects, weapon, effects, *self.version.peek());
    }

    pub(crate) async fn load_effects(
        self,
        weapon: String,
    ) -> Result<api::StatEffects, ServerFnError> {
        let flight = format!("effects {weapon}");
        let fetch = api::stat_effects(weapon.clone());
        self.load(self.effects, weapon, flight, fetch).await
    }

    /// Starts loading what a weapon's page shows, with the current filters, so opening it
    /// right after (a hover is a good hint) shows it without waiting.
    pub(crate) fn prefetch_weapon(self, filters: Query, slug: String) {
        let query = Query {
            weapon: Some(slug.clone()),
            riven_id: None,
            ..filters
        };
        if self.lookup(self.searches, &query, true).is_none() {
            // Not tied to the hovered component, which navigation is about to unmount.
            dioxus::core::spawn_forever(async move {
                let _ = self.load_search(query).await;
            });
        }
        if self.lookup(self.effects, &slug, true).is_none() {
            dioxus::core::spawn_forever(async move {
                let _ = self.load_effects(slug).await;
            });
        }
    }

    async fn load<K, V>(
        self,
        entries: Signal<Vec<Entry<K, V>>>,
        key: K,
        flight: String,
        fetch: impl Future<Output = Result<V, ServerFnError>>,
    ) -> Result<V, ServerFnError>
    where
        K: PartialEq + 'static,
        V: Clone + 'static,
    {
        // Wait for a request under way instead of sending the same one again. If it fails,
        // it leaves no answer and no flight, and this one asks itself.
        loop {
            if let Some(value) = self.lookup(entries, &key, true) {
                return Ok(value);
            }
            if !self.in_flight.peek().contains(&flight) {
                break;
            }
            sleep_ms(IN_FLIGHT_POLL_MS).await;
        }
        let version = *self.version.peek();
        let mut in_flight = self.in_flight;
        in_flight.write().push(flight.clone());
        let _flight = Flight(in_flight, flight);
        let result = fetch.await;
        if let Ok(value) = &result {
            self.remember(entries, key, value.clone(), version);
        }
        result
    }

    fn lookup<K: PartialEq + 'static, V: Clone + 'static>(
        &self,
        entries: Signal<Vec<Entry<K, V>>>,
        key: &K,
        fresh_only: bool,
    ) -> Option<V> {
        let version = *self.version.peek();
        let now = client_now();
        entries
            .peek()
            .iter()
            .find(|e| &e.key == key)
            .filter(|e| !fresh_only || (e.version == version && now - e.at < FRESH_MS))
            .map(|e| e.value.clone())
    }

    fn remember<K: PartialEq + 'static, V: 'static>(
        &self,
        mut entries: Signal<Vec<Entry<K, V>>>,
        key: K,
        value: V,
        version: u64,
    ) {
        let mut entries = entries.write();
        entries.retain(|e| e.key != key);
        entries.insert(
            0,
            Entry {
                key,
                value,
                version,
                at: client_now(),
            },
        );
        entries.truncate(VIEW_CACHE_SIZE);
    }
}

#[component]
fn Shell() -> Element {
    let mut scraper_open = use_signal(|| false);
    let query = use_signal(Query::default);
    use_context_provider(|| query);
    // Bumped when the server's data changes; cached answers from before are stale.
    let mut data_version = use_signal(|| 0u64);
    use_context_provider(|| data_version);
    use_context_provider(|| ViewCache::new(data_version));
    use_context_provider(crate::weapons::GridState::new);

    // Never suspend on data: suspending held back the server response and blanked the page
    // on every refresh. The catalog comes embedded in the server's HTML when the server has
    // it at hand (see `api::seed`), else the page frame renders and it fills in on arrival.
    let seed = use_seed(api::seed::overview);
    let first = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(seed.clone())));
    let mut overview = use_resource(move || {
        let first = first.take();
        async move {
            match first {
                Some(overview) => Ok(overview),
                None => api::overview().await,
            }
        }
    });
    // A failed refresh must not take down a page that already has data.
    let mut loaded = use_signal(|| seed);
    use_context_provider(|| loaded);
    use_effect(move || {
        if let Some(Ok(value)) = &*overview.read()
            && loaded.peek().as_ref() != Some(value)
        {
            loaded.set(Some(value.clone()));
        }
    });

    // New data while scraping: the count updates right away, the rest refetches on a version
    // change, so nothing is requested while the data stands still. Unchanged polls don't
    // touch any state, so they don't re-render anything either.
    let mut live = use_resource(api::live_stats);
    let mut stats = use_signal(|| None::<LiveStats>);
    use_effect(move || {
        if let Some(Ok(value)) = &*live.read()
            && stats.peek().as_ref() != Some(value)
        {
            let changed = stats
                .peek()
                .as_ref()
                .is_some_and(|s| s.version != value.version);
            stats.set(Some(value.clone()));
            if changed {
                overview.restart();
                data_version += 1;
            }
        }
    });
    use_future(move || async move {
        loop {
            sleep_ms(LIVE_POLL_MS).await;
            live.restart();
        }
    });
    let current_route = use_route::<Route>();
    use_effect(use_reactive!(|current_route| {
        let _ = current_route;
        scraper_open.set(false);
    }));

    let data = loaded();
    let current = stats();
    let scraping = current.as_ref().is_some_and(|s| s.scraping);
    let auctions = current
        .as_ref()
        .map(|s| s.auctions)
        .or(data.as_ref().map(|o| o.auctions))
        .map_or("—".to_owned(), |n| crate::scraper::count(u64::from(n)));
    let load_error = match &*overview.read() {
        Some(Err(e)) if data.is_none() => Some(e.to_string()),
        _ => None,
    };

    rsx! {
        document::Stylesheet { href: CSS }
        document::Title { "Riven Tracker" }
        header {
            div { class: "brand",
                h1 {
                    Link { to: Route::Market {}, onclick: move |_| scraper_open.set(false), "Riven Tracker" }
                }
            }
            if let Some(o) = &data {
                WeaponPicker { weapons: o.weapons.clone() }
            } else {
                div {}
            }
            div { class: "header-actions",
                button {
                    class: if scraper_open() { "auction-count active" } else { "auction-count" },
                    title: if scraping { "Unique auctions seen so far, updating live while scraping. Click for scraper status." } else { "Unique auctions seen so far. Click for scraper status." },
                    aria_label: "{auctions} auctions seen, open scraper status",
                    aria_pressed: scraper_open(),
                    onclick: move |_| scraper_open.toggle(),
                    if scraping { span { class: "live-dot" } }
                    strong { "{auctions}" }
                    span { class: "auction-count-label", " auctions seen" }
                }
                a {
                    class: "github-badge",
                    href: "https://github.com/LLukas22/warframe-market-tracker",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    aria_label: "View Riven Tracker on GitHub (opens in a new tab)",
                    svg {
                        view_box: "0 0 24 24",
                        width: "18",
                        height: "18",
                        fill: "currentColor",
                        "aria-hidden": "true",
                        path { d: "M12 .297C5.37.297 0 5.67 0 12.297c0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.043-1.61-4.043-1.61-.546-1.387-1.333-1.756-1.333-1.756-1.09-.745.083-.729.083-.729 1.205.084 1.838 1.237 1.838 1.237 1.07 1.835 2.809 1.305 3.495.998.108-.776.418-1.305.762-1.605-2.665-.3-5.467-1.334-5.467-5.931 0-1.31.469-2.381 1.236-3.221-.124-.303-.536-1.524.117-3.176 0 0 1.008-.322 3.301 1.23a11.52 11.52 0 0 1 3.003-.404c1.02.005 2.047.138 3.006.404 2.291-1.552 3.297-1.23 3.297-1.23.655 1.652.243 2.873.12 3.176.77.84 1.235 1.911 1.235 3.221 0 4.609-2.807 5.628-5.479 5.921.431.372.815 1.102.815 2.222 0 1.606-.015 2.898-.015 3.293 0 .322.216.696.825.578C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" }
                    }
                }
            }
        }
        if scraper_open() {
            crate::scraper::ScraperPanel {}
        } else if let Some(error) = load_error {
            div { class: "fatal", "Failed to load data: {error}" }
        } else {
            // Pages start their own requests right away and render placeholders for the
            // catalog until it is loaded, instead of waiting for it one after the other.
            Outlet::<Route> {}
        }
    }
}

/// Weapon and ID selection belongs to the URL; the shared signal holds filter preferences.
fn query_for_route(mut query: Query, route: &Route) -> Query {
    query.weapon = match route {
        Route::Weapon { slug } => Some(slug.clone()),
        _ => None,
    };
    query.riven_id = match route {
        Route::Search { id } => Some(id.clone()),
        _ => None,
    };
    query
}

#[component]
fn Weapon(slug: String) -> Element {
    rsx! { Market { key: "weapon-{slug}" } }
}

#[component]
fn Search(id: String) -> Element {
    rsx! { Market { key: "search-{id}" } }
}

#[component]
fn Market() -> Element {
    let route = use_route::<Route>();
    let loaded = use_context::<Signal<Option<Overview>>>();
    let query = use_context::<Signal<Query>>();
    let data_version = use_context::<Signal<u64>>();
    let overview = loaded();
    let unknown = matches!(
        (&overview, &route),
        (Some(o), Route::Weapon { slug }) if !o.weapons.iter().any(|w| &w.slug == slug)
    );
    rsx! {
        if unknown {
            main { class: "riven-page",
                document::Title { "Weapon not found · Riven Tracker" }
                h2 { "Weapon not found" }
                p {
                    "This weapon is not in the catalog. Choose a weapon from the search box or "
                    Link { to: Route::Market {}, "browse all weapons" }
                    "."
                }
            }
        } else {
            MarketPage { key: "{route}", overview, query, data_version, route }
        }
    }
}

#[component]
fn RivenPage(id: String, auction: String) -> Element {
    // Key the resource owner so navigating between two rivens resets all page state.
    rsx! { RivenPageContent { key: "{id}/{auction}", id, auction } }
}

#[component]
fn RivenPageContent(id: String, auction: String) -> Element {
    let id_for_notice = id.clone();
    let version = use_context::<Signal<u64>>();
    let include_legacy = use_signal(|| false);
    let include_unfit = use_signal(|| false);
    let include_loose = use_signal(|| false);
    let mut similar_tab = use_signal(|| false);
    let (seed_id, seed_auction) = (id.clone(), auction.clone());
    let seed = use_seed(move || {
        api::seed::riven_detail(
            &seed_id,
            Some(seed_auction.as_str()).filter(|a| !a.is_empty()),
        )
    });
    // The embedded answer is for the default options; it stands in for their first request.
    let first = use_hook(|| std::rc::Rc::new(std::cell::Cell::new(seed.clone())));
    let mut detail = use_resource(move || {
        version.read();
        let legacy = include_legacy();
        let unfit = include_unfit();
        let loose = include_loose();
        let first = first.take().filter(|_| !(legacy || unfit || loose));
        let id = id.clone();
        let auction = (!auction.is_empty()).then(|| auction.clone());
        async move {
            match first {
                Some(detail) => Ok(detail),
                None => {
                    let comparisons = api::Comparisons {
                        legacy,
                        unfit,
                        loose,
                    };
                    api::riven_detail(id, auction, comparisons).await
                }
            }
        }
    });
    let shown = detail.read().clone().or_else(|| seed.clone().map(Ok));
    rsx! {
        main { class: "riven-page",
            crate::scan::ScanNotice { target: id_for_notice, matched: true }
            match &shown {
                None => rsx! { p { role: "status", "Loading riven…" } },
                Some(Err(error)) => rsx! {
                    p { class: "error", "Could not load this riven: {error}" }
                    button { onclick: move |_| detail.restart(), "Retry" }
                },
                Some(Ok(None)) => rsx! {
                    h2 { "Riven not found" }
                    p { "This riven or selected auction is not in the stored dataset." }
                },
                Some(Ok(Some(data))) => {
                    let selected = &data.listing;
                    rsx! {
                        document::Title { "{selected.weapon_name} {selected.riven_name} · Riven Tracker" }
                        crate::riven_view::RivenSummary { listing: selected.clone(), now: Some(data.server_time) }
                        div { class: "listing-bar",
                            div { class: "listing-tabs", role: "tablist", aria_label: "Riven sections",
                                button {
                                    role: "tab", id: "history-tab", aria_selected: !similar_tab(), aria_controls: "history-panel",
                                    class: if !similar_tab() { "active" } else { "" },
                                    onclick: move |_| similar_tab.set(false),
                                    "History"
                                }
                                button {
                                    role: "tab", id: "similar-tab", aria_selected: similar_tab(), aria_controls: "similar-panel",
                                    class: if similar_tab() { "active" } else { "" },
                                    onclick: move |_| similar_tab.set(true),
                                    "Similar rivens ({data.similar_total})"
                                }
                            }
                        }
                        // Both stay mounted, so switching tabs keeps their state and loaded data.
                        div { id: "history-panel", role: "tabpanel", aria_labelledby: "history-tab", hidden: similar_tab(),
                            crate::journey::RivenJourney {
                                id: selected.riven_id.clone(), selected_auction: selected.auction_id.clone(),
                            }
                        }
                        div { id: "similar-panel", role: "tabpanel", aria_labelledby: "similar-tab", hidden: !similar_tab(),
                            SimilarRivens {
                                selected: selected.clone(), similar: data.similar.clone(),
                                similar_total: data.similar_total, comparisons_hidden: data.comparisons_hidden,
                                now: data.server_time, updating: detail.pending(),
                                include_legacy, include_unfit, include_loose,
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn ScannedRivenPage(scan: String) -> Element {
    // Not keyed by the scan: edits change the URL, and remounting would lose the editor's focus.
    rsx! { ScannedRivenContent { scan } }
}

#[component]
fn ScannedRivenContent(scan: String) -> Element {
    let navigator = use_navigator();
    let version = use_context::<Signal<u64>>();
    let mut draft = use_signal(|| api::RivenScan::from_param(&scan));
    // The scanner's own reading, which its warnings belong to.
    let mut original = use_signal(|| scan.clone());
    // A new scan changes the URL under this page. Edits do too, but they already match it.
    use_effect(use_reactive!(|scan| {
        if draft.peek().as_ref().map(|d| d.to_param()).as_deref() != Some(scan.as_str()) {
            draft.set(api::RivenScan::from_param(&scan));
            original.set(scan);
        }
    }));
    let include_legacy = use_signal(|| false);
    let include_unfit = use_signal(|| false);
    let include_loose = use_signal(|| false);
    let mut detail = use_resource(move || {
        version.read();
        let scan = draft();
        let (legacy, unfit, loose) = (include_legacy(), include_unfit(), include_loose());
        async move {
            match scan {
                Some(scan) => {
                    let comparisons = api::Comparisons {
                        legacy,
                        unfit,
                        loose,
                    };
                    api::scanned_detail(scan, comparisons).await
                }
                None => Ok(None),
            }
        }
    });
    // A corrected riven may turn out to have been listed before.
    let matched = use_resource(move || {
        let scan = draft();
        async move {
            match scan {
                Some(scan) => api::scan_match(scan).await.ok().flatten(),
                None => None,
            }
        }
    });
    // Keep showing the last result while an edit is graded, so the page doesn't jump.
    let mut shown = use_signal(|| None::<Result<Option<api::RivenDetail>, String>>);
    use_effect(move || {
        if let Some(result) = &*detail.read() {
            shown.set(Some(result.clone().map_err(|e| e.to_string())));
        }
    });
    let onchange = move |next: api::RivenScan| {
        let param = next.to_param();
        draft.set(Some(next));
        navigator.replace(Route::ScannedRivenPage { scan: param });
    };
    let listed_before = matched.read().clone().flatten();

    rsx! {
        main { class: "riven-page",
            crate::scan::ScanNotice { target: original(), matched: false }
            if let Some(current) = draft() {
                crate::scan::ScanEditor { scan: current, onchange }
            } else {
                h2 { "Riven not found" }
                p { "This scan link is broken. Try scanning the screenshot again." }
            }
            if let Some(id) = listed_before {
                div { class: "scan-note",
                    "This riven has been listed on warframe.market before. "
                    Link { to: Route::RivenPage { id, auction: String::new() }, "Open its page and price history →" }
                }
            }
            div { class: if detail.pending() { "scan-results updating" } else { "scan-results" },
                match shown() {
                    None => rsx! { p { role: "status", "Grading riven…" } },
                    Some(Err(error)) => rsx! {
                        p { class: "error", "Could not grade this riven: {error}" }
                        button { onclick: move |_| detail.restart(), "Retry" }
                    },
                    Some(Ok(None)) => rsx! {
                        p { class: "muted", "Choose the weapon above to grade this riven." }
                    },
                    Some(Ok(Some(data))) => {
                        let selected = &data.listing;
                        rsx! {
                            document::Title { "{selected.weapon_name} {selected.riven_name} · Riven Tracker" }
                            if !selected.fit_ok {
                                p { class: "scan-note warn",
                                    "These values don't fit any rank of this riven, so a number or stat was probably misread. Check them against your riven above. Similar rivens are still shown, without roll comparisons."
                                }
                            }
                            crate::riven_view::RivenSummary { listing: selected.clone(), now: None }
                            h2 { class: "riven-section-title", "Similar rivens ({data.similar_total})" }
                            SimilarRivens {
                                selected: selected.clone(), similar: data.similar.clone(),
                                similar_total: data.similar_total, comparisons_hidden: data.comparisons_hidden,
                                now: data.server_time, updating: detail.pending(),
                                include_legacy, include_unfit, include_loose,
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Other rivens of the same weapon, closest first, with the comparison settings.
#[component]
fn SimilarRivens(
    selected: Listing,
    similar: Vec<api::SimilarRiven>,
    similar_total: usize,
    comparisons_hidden: usize,
    now: i64,
    updating: bool,
    include_legacy: Signal<bool>,
    include_unfit: Signal<bool>,
    include_loose: Signal<bool>,
) -> Element {
    let mut include_legacy = include_legacy;
    let mut include_unfit = include_unfit;
    let mut include_loose = include_loose;
    rsx! {
        section { class: "similar-rivens",
            p { class: "muted small",
                "Same weapon, closest stats first. Prices are asking prices, not confirmed sales."
            }
            details { class: "similar-options",
                summary { "Comparison settings" }
                label { class: "check",
                    input { r#type: "checkbox", checked: include_legacy(),
                        onchange: move |e| include_legacy.set(e.checked()) }
                    " Include pre-rework / unknown-era stats"
                }
                label { class: "check",
                    input { r#type: "checkbox", checked: include_unfit(),
                        onchange: move |e| include_unfit.set(e.checked()) }
                    " Include unverified stats"
                }
                label { class: "check",
                    input { r#type: "checkbox", checked: include_loose(),
                        onchange: move |e| include_loose.set(e.checked()) }
                    " Include loose matches with one shared positive"
                }
                if comparisons_hidden > 0 {
                    p { class: "muted small", "{comparisons_hidden} potential matches hidden by the era / verification settings." }
                }
            }
            if updating { p { role: "status", "Updating comparisons…" } }
            if similar_total > similar.len() {
                p { class: "muted small", "Showing the {similar.len()} closest of {similar_total}." }
            }
            if similar.is_empty() {
                p { "No matching rivens in the stored history with these settings yet." }
            }
            for matched in similar.iter() {
                article { key: "{matched.listing.riven_id}", class: "similar-match",
                    div { class: "match-reason",
                        span { class: "match-label", "{match_label(matched)}" }
                        if !matched.same_stats {
                            span { class: "stat-difference", "{match_differences(&selected, matched)}" }
                        }
                        if let Some(distance) = matched.roll_distance {
                            span { class: "muted match-quality", title: "Average normalized roll difference on shared stats", "Roll difference {distance * 100.0:.1} pp" }
                        }
                    }
                    ListingRow {
                        listing: matched.listing.clone(), now: now,
                        show_weapon: false, has_image: false,
                    }
                }
            }
            if !include_loose() && similar_total < SIMILAR_LIMIT {
                button { class: "link-button", onclick: move |_| include_loose.set(true),
                    "Broaden to one shared positive stat"
                }
            }
        }
    }
}

fn match_label(m: &api::SimilarRiven) -> String {
    if m.shared_positives == 1 {
        format!(
            "Loose match · 1 of {} positives",
            1 + m.missing_positives.len()
        )
    } else if m.same_stats {
        "Same stats".into()
    } else if m.missing_positives.is_empty() {
        format!("All {} positives match", m.shared_positives)
    } else {
        format!(
            "{} of {} positives match",
            m.shared_positives,
            m.shared_positives + m.missing_positives.len()
        )
    }
}

fn match_differences(selected: &Listing, m: &api::SimilarRiven) -> String {
    let names = |slugs: &[String]| {
        slugs
            .iter()
            .map(|s| attribute_name(s))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    if !m.missing_positives.is_empty() {
        parts.push(format!("Missing {}", names(&m.missing_positives)));
    }
    if !m.added_positives.is_empty() {
        parts.push(format!("Adds {}", names(&m.added_positives)));
    }
    if !m.same_negative {
        let curse = |l: &Listing| {
            let negatives: Vec<_> = l
                .attributes
                .iter()
                .filter(|a| !a.positive)
                .map(|a| a.slug.clone())
                .collect();
            if negatives.is_empty() {
                "none".into()
            } else {
                names(&negatives)
            }
        };
        parts.push(format!(
            "Negative: {} → {}",
            curse(selected),
            curse(&m.listing)
        ));
    }
    parts.join(" · ")
}

/// How many filters differ from the defaults. Sorting doesn't filter anything.
fn active_filters(q: &Query) -> usize {
    let d = Query::default();
    q.positives.len()
        + [
            q.period != d.period,
            q.curse != d.curse,
            q.direct_only != d.direct_only,
            q.include_legacy != d.include_legacy,
            q.include_unfit != d.include_unfit,
            q.include_outliers != d.include_outliers,
            q.price_changed != d.price_changed,
            q.owner_changed != d.owner_changed,
            q.min_quality != d.min_quality,
            q.min_price != d.min_price,
            q.max_price != d.max_price,
        ]
        .into_iter()
        .filter(|changed| *changed)
        .count()
}

#[component]
fn MarketPage(
    overview: Option<Overview>,
    query: Signal<Query>,
    data_version: Signal<u64>,
    route: Route,
) -> Element {
    let requested = query_for_route(query(), &route);
    let request_route = route.clone();
    // A plain resource, not a server future: suspending here held back the whole server
    // response until the search finished, and blanked the page on every filter change or
    // live update. Now the frame renders first and the results stream in. The query rides
    // along with the result so the page knows which filters the shown results belong to.
    let cache = use_context::<ViewCache>();
    let seed_route = route.clone();
    let seed =
        use_seed(move || api::seed::search(&query_for_route(query.peek().clone(), &seed_route)));
    use_hook(|| {
        if let Some(result) = seed {
            cache.store_search(requested.clone(), result);
        }
    });
    let results = use_resource(move || {
        data_version.read();
        let q = query_for_route(query(), &request_route);
        async move {
            let result = cache.load_search(q.clone()).await;
            (q, result)
        }
    });
    // What to show: the fetched result if it answers the current query, else the cached
    // answer to it, else the previous result (dimmed below) while the new one loads.
    let current = match results.read().clone() {
        Some((q, r)) if q == requested => Some((q, r)),
        previous => cache
            .search(&requested)
            .map(|r| (requested.clone(), Ok(r)))
            .or(previous),
    };
    let shown_query = current.as_ref().map(|(q, _)| q.clone());
    let server_time = current
        .as_ref()
        .and_then(|(_, r)| r.as_ref().ok())
        .map(|r| r.server_time);

    let selected = requested.weapon.as_ref().and_then(|slug| {
        overview
            .as_ref()
            .and_then(|o| o.weapons.iter().find(|w| &w.slug == slug).cloned())
    });
    let weapons = overview
        .as_ref()
        .map(|o| o.weapons.clone())
        .unwrap_or_default();
    // Live updates refresh in place; only a changed search dims the stale results.
    let loading = results.pending() && shown_query.as_ref().is_some_and(|q| q != &requested);
    let riven_search = requested.riven_id.clone();
    // Only matters on narrow screens, where the filters would otherwise push the results down.
    let mut filters_open = use_signal(|| false);
    let filter_count = active_filters(&query.read());

    let head = if let Some(id) = riven_search.clone() {
        let matches = current
            .as_ref()
            .and_then(|(_, r)| r.as_ref().ok())
            .map(|r| r.total + r.gone_total);
        rsx! {
            div { class: "id-banner",
                span { class: "muted", "Riven id" }
                code { "{id}" }
                span { class: "muted small",
                    match matches {
                        None => "searching…",
                        Some(0) => "no riven with this id",
                        Some(_) => "select a riven for its history and similar listings",
                    }
                }
                Link {
                    to: Route::Market {},
                    class: "picker-clear",
                    title: "All weapons",
                    "×"
                }
            }
        }
    } else if let Some(w) = selected.clone() {
        rsx! { WeaponHeader { weapon: w, now: server_time } }
    } else if requested.weapon.is_some() {
        rsx! { div { class: "skeleton skeleton-header", aria_hidden: "true" } }
    } else if let Some(o) = &overview {
        rsx! { WeaponGrid { weapons: o.weapons.clone() } }
    } else {
        rsx! {
            div { class: "skeleton-grid", aria_hidden: "true",
                for i in 0..12 { div { key: "{i}", class: "skeleton skeleton-card" } }
            }
        }
    };

    let body = match &current {
        Some((shown, Ok(r))) => rsx! {
            Results {
                key: "{shown:?}",
                search_query: shown.clone(),
                result: r.clone(),
                show_weapon: requested.weapon.is_none(),
                weapons,
                id_search: riven_search.is_some(),
                weapon: requested.weapon.clone(),
            }
        },
        Some((_, Err(e))) => rsx! { div { class: "error", "Search failed: {e}" } },
        None => rsx! {
            div { class: "results-skeleton", role: "status", aria_label: "Loading listings",
                if riven_search.is_none() {
                    div { class: "skeleton skeleton-chart" }
                }
                for i in 0..6 { div { key: "{i}", class: "skeleton skeleton-row" } }
            }
        },
    };

    rsx! {
        document::Title {
            {selected.as_ref().map(|w| format!("{} rivens · Riven Tracker", w.name)).unwrap_or_else(|| "Riven Tracker".into())}
        }
        main { class: "market-page",
            aside { class: if filters_open() { "filters-open" } else { "" },
                button {
                    class: "filters-toggle",
                    aria_expanded: filters_open(),
                    onclick: move |_| filters_open.toggle(),
                    span {
                        "Filters"
                        if filter_count > 0 { " · {filter_count} changed" }
                    }
                    span { class: "filters-toggle-icon", if filters_open() { "▴" } else { "▾" } }
                }
                if let Some(o) = overview.clone() {
                    Filters {
                        overview: o,
                        query,
                        weapon: requested.weapon.clone(),
                        now: server_time,
                    }
                } else {
                    div { class: "filters", aria_hidden: "true",
                        for i in 0..6 { div { key: "{i}", class: "skeleton skeleton-filter" } }
                    }
                }
            }
            section { class: "content",
                {head}
                div { class: if loading { "results loading" } else { "results" },
                    aria_busy: loading,
                    {body}
                }
            }
        }
    }
}

/// Period presets, in the order they're offered.
const PERIODS: [(Period, &str); 4] = [
    (Period::Today, "Today"),
    (Period::Week, "7 days"),
    (Period::Month, "30 days"),
    (Period::All, "All"),
];

/// UTC midnight of the day containing `ms`.
fn day_start(ms: i64) -> i64 {
    ms - ms.rem_euclid(DAY_MS)
}

#[component]
fn Filters(
    overview: Overview,
    query: Signal<Query>,
    weapon: Option<String>,
    /// Server time of the shown results, to show which days a period preset covers.
    now: Option<i64>,
) -> Element {
    let mut query = query;
    let q = query();
    // The slider's position while dragging, before it's applied.
    let mut quality_drag = use_signal(|| None::<f64>);
    let quality = quality_drag().unwrap_or(q.min_quality);

    let class = weapon
        .as_ref()
        .and_then(|slug| overview.weapons.iter().find(|w| &w.slug == slug))
        .map(|w| w.class);
    let available = |slug: &str| {
        class.as_ref().is_none_or(|c| {
            overview
                .attributes
                .iter()
                .any(|a| a.slug == slug && a.classes.contains(c))
        })
    };
    let attrs: Vec<AttrInfo> = overview
        .attributes
        .iter()
        .filter(|a| available(&a.slug))
        .cloned()
        .collect();
    let positive_options: Vec<AttrInfo> = attrs
        .iter()
        .filter(|a| !q.positives.contains(&a.slug))
        .cloned()
        .collect();
    let positives: Vec<(String, bool)> = q
        .positives
        .iter()
        .map(|p| (p.clone(), available(p)))
        .collect();

    let curse_value = q.curse.key();
    let price_input = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();

    // The date fields show the days a preset covers, and editing one keeps the other as shown.
    let (shown_from, shown_to) = match (q.period, now) {
        (Period::Custom { from, to }, _) => (from, to),
        (Period::All, _) | (_, None) => (None, None),
        (period, Some(now)) => (period.bounds(now).0.map(day_start), Some(day_start(now))),
    };
    let date_input = |v: Option<i64>| v.map(format_date).unwrap_or_default();
    let today = now.map(format_date).unwrap_or_default();

    let changed = active_filters(&q);
    let d = Query::default();
    let more_changed = [
        q.price_changed != d.price_changed,
        q.owner_changed != d.owner_changed,
        q.include_legacy != d.include_legacy,
        q.include_unfit != d.include_unfit,
        q.include_outliers != d.include_outliers,
    ]
    .into_iter()
    .filter(|c| *c)
    .count();
    let mut more_open = use_signal(|| more_changed > 0);
    let is_custom = matches!(q.period, Period::Custom { .. });
    let mut custom_open = use_signal(|| is_custom);

    rsx! {
        div { class: "filters",
            div { class: "filters-head",
                h2 { "Filters" }
                if changed > 0 {
                    button {
                        class: "link-button",
                        // Sorting isn't a filter, so it survives a reset.
                        onclick: move |_| query.with_mut(|q| *q = Query { sort: q.sort, ..Query::default() }),
                        "Reset ({changed})"
                    }
                }
            }

            div { class: "filter-group",
                h3 { "Period" }
                div { class: "segmented", role: "group", aria_label: "Period",
                    for (period, label) in PERIODS {
                        button {
                            key: "{label}",
                            class: if q.period == period { "active" } else { "" },
                            aria_pressed: q.period == period,
                            onclick: move |_| query.with_mut(|q| q.period = period),
                            "{label}"
                        }
                    }
                }
                button {
                    class: if is_custom { "custom-toggle active" } else { "custom-toggle" },
                    aria_expanded: custom_open(),
                    aria_controls: "custom-range",
                    onclick: move |_| custom_open.toggle(),
                    span { "Custom range" }
                    span { class: "muted", if custom_open() { "▴" } else { "▾" } }
                }
                if custom_open() {
                    div { id: "custom-range", class: "row date-range",
                        input {
                            r#type: "date",
                            aria_label: "From",
                            title: "From (UTC)",
                            value: "{date_input(shown_from)}",
                            max: if let Some(to) = shown_to { format_date(to) } else { today.clone() },
                            onchange: move |e| {
                                query.with_mut(|q| q.period = Period::custom(parse_date(&e.value()), shown_to));
                            },
                        }
                        span { class: "muted", "–" }
                        input {
                            r#type: "date",
                            aria_label: "To",
                            title: "To (UTC, inclusive)",
                            value: "{date_input(shown_to)}",
                            min: "{date_input(shown_from)}",
                            max: "{today}",
                            onchange: move |e| {
                                query.with_mut(|q| q.period = Period::custom(shown_from, parse_date(&e.value())));
                            },
                        }
                    }
                }
                p { class: "muted small", "Applies to the chart and to gone listings. Days are UTC." }
            }

            div { class: "filter-group",
                h3 { "Stats" }
                if !positives.is_empty() {
                    div { class: "chips",
                        for (slug, ok) in positives {
                            span {
                                key: "{slug}",
                                class: if ok { "chip positive" } else { "chip unavailable" },
                                title: if ok { "" } else { "Can't roll on this weapon" },
                                "+{attribute_name(&slug)}"
                                button {
                                    aria_label: "Remove {attribute_name(&slug)}",
                                    onclick: move |_| query.with_mut(|q| q.positives.retain(|p| p != &slug)),
                                    "×"
                                }
                            }
                        }
                    }
                }
                if q.positives.len() < *riven::POSITIVES.end() {
                    select {
                        aria_label: "Add a positive stat",
                        value: "",
                        onchange: move |e| {
                            let v = e.value();
                            if !v.is_empty() {
                                query.with_mut(|q| {
                                    if !q.positives.contains(&v) {
                                        q.positives.push(v);
                                    }
                                });
                            }
                        },
                        option { value: "",
                            if q.positives.is_empty() { "+ Positive stat (all required)" } else { "+ Another positive stat" }
                        }
                        for a in positive_options.iter() {
                            option { key: "{a.slug}", value: "{a.slug}", "{a.name}" }
                        }
                    }
                }
                select {
                    aria_label: "Negative stat",
                    value: "{curse_value}",
                    onchange: move |e| {
                        let curse = CurseFilter::from_key(&e.value());
                        query.with_mut(|q| q.curse = curse);
                    },
                    option { value: CurseFilter::Any.key(), "Any negative" }
                    option { value: CurseFilter::None.key(), "No negative" }
                    option { value: CurseFilter::Required.key(), "Has a negative" }
                    optgroup { label: "Specific negative",
                        for a in attrs.iter() {
                            option {
                                key: "{a.slug}",
                                value: CurseFilter::Attr(a.slug.clone()).key(),
                                "−{a.name}"
                            }
                        }
                    }
                }
                label { class: "slider-label",
                    span { "Min roll quality" }
                    span { class: "slider-value", "{(quality * 100.0).round()}%" }
                }
                // Searches when released, not for every step passed while dragging.
                input {
                    r#type: "range",
                    aria_label: "Min roll quality",
                    min: "0",
                    max: "100",
                    step: "5",
                    value: "{(quality * 100.0).round()}",
                    oninput: move |e| {
                        quality_drag.set(Some(e.value().parse::<f64>().unwrap_or(0.0) / 100.0));
                    },
                    onchange: move |e| {
                        let v = e.value().parse::<f64>().unwrap_or(0.0) / 100.0;
                        quality_drag.set(None);
                        query.with_mut(|q| q.min_quality = v);
                    },
                }
            }

            div { class: "filter-group",
                h3 { "Price" }
                div { class: "row",
                    input {
                        r#type: "number",
                        aria_label: "Min price",
                        placeholder: "min",
                        min: "0",
                        value: "{price_input(q.min_price)}",
                        onchange: move |e| {
                            let v = e.value().parse().ok();
                            query.with_mut(|q| q.min_price = v);
                        },
                    }
                    span { class: "muted", "–" }
                    input {
                        r#type: "number",
                        aria_label: "Max price",
                        placeholder: "max",
                        min: "0",
                        value: "{price_input(q.max_price)}",
                        onchange: move |e| {
                            let v = e.value().parse().ok();
                            query.with_mut(|q| q.max_price = v);
                        },
                    }
                }
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: q.direct_only,
                        onchange: move |e| query.with_mut(|q| q.direct_only = e.checked()),
                    }
                    "Buyout only (no bidding)"
                }
            }

            div { class: "filter-more",
                button {
                    class: "filter-more-toggle",
                    aria_expanded: more_open(),
                    aria_controls: "filter-more-body",
                    onclick: move |_| more_open.toggle(),
                    span {
                        "More options"
                        if more_changed > 0 { span { class: "muted", " · {more_changed} on" } }
                    }
                    span { class: "muted", if more_open() { "▴" } else { "▾" } }
                }
                if more_open() {
                    div { id: "filter-more-body", class: "filter-group",
                        h3 { "Only rivens whose" }
                        label { class: "check", title: "Different prices observed for this riven, including relistings.",
                            input {
                                r#type: "checkbox",
                                checked: q.price_changed,
                                onchange: move |e| query.with_mut(|q| q.price_changed = e.checked()),
                            }
                            "Price changed"
                        }
                        label { class: "check", title: "Listed by different sellers. Ownership transfers are not confirmed.",
                            input {
                                r#type: "checkbox",
                                checked: q.owner_changed,
                                onchange: move |e| query.with_mut(|q| q.owner_changed = e.checked()),
                            }
                            "Owner changed"
                        }
                        h3 { class: "spaced", "Also include" }
                        label { class: "check", title: "Stats recorded before the disposition rework.",
                            input {
                                r#type: "checkbox",
                                checked: q.include_legacy,
                                onchange: move |e| query.with_mut(|q| q.include_legacy = e.checked()),
                            }
                            "Pre-rework stats"
                        }
                        label { class: "check", title: "Stats that don't fit the weapon, such as placeholders and typos.",
                            input {
                                r#type: "checkbox",
                                checked: q.include_unfit,
                                onchange: move |e| query.with_mut(|q| q.include_unfit = e.checked()),
                            }
                            "Unverifiable stats"
                        }
                        label { class: "check", title: "Listed, but never counted in charts or medians.",
                            input {
                                r#type: "checkbox",
                                checked: q.include_outliers,
                                onchange: move |e| query.with_mut(|q| q.include_outliers = e.checked()),
                            }
                            "Invalid prices"
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn Results(
    result: SearchResult,
    search_query: Query,
    show_weapon: bool,
    weapons: Vec<crate::api::WeaponInfo>,
    /// ID searches choose a riven, then open the shared detail page.
    id_search: bool,
    /// The weapon being looked at, if any.
    weapon: Option<String>,
) -> Element {
    let mut query = use_context::<Signal<Query>>();
    let now = result.server_time;
    let mut gone_tab = use_signal(|| false);
    let has_image = |l: &Listing| {
        weapons
            .iter()
            .any(|w| w.slug == l.weapon_slug && w.has_image)
    };
    if id_search {
        let mut seen = std::collections::HashSet::new();
        let matches: Vec<_> = result
            .listings
            .iter()
            .chain(&result.gone)
            .filter(|l| seen.insert(l.riven_id.clone()))
            .collect();
        return rsx! {
            h2 { "Matching rivens" }
            for listing in matches {
                ListingRow { key: "{listing.riven_id}", listing: listing.clone(), now,
                    show_weapon: true, has_image: has_image(listing) }
            }
        };
    }
    rsx! {
        div { class: "section-head",
            h2 { "Price over time" }
            span { class: "muted small",
                "{period_label(search_query.period)} · per {bucket_label(result.bucket_hours)}"
                " · invalid prices excluded"
            }
        }
        PriceChart { points: result.history.clone(), bucket_hours: result.bucket_hours }
        if let Some(slug) = weapon {
            StatEffectsPanel { key: "{slug}", weapon: slug.clone() }
        }
        div { class: "listing-bar",
            div { class: "listing-tabs", role: "tablist", aria_label: "Listing status",
                button {
                    role: "tab", id: "active-tab", aria_selected: !gone_tab(), aria_controls: "listing-panel",
                    class: if !gone_tab() { "active" } else { "" },
                    onclick: move |_| gone_tab.set(false),
                    "Active ({result.total})"
                }
                button {
                    role: "tab", id: "gone-tab", aria_selected: gone_tab(), aria_controls: "listing-panel",
                    class: if gone_tab() { "active" } else { "" },
                    onclick: move |_| gone_tab.set(true),
                    "Gone / sold ({result.gone_total})"
                }
            }
            // Gone listings are always newest first.
            if !gone_tab() {
                select {
                    class: "compact",
                    aria_label: "Sort listings",
                    value: search_query.sort.key(),
                    onchange: move |e| {
                        let sort = Sort::from_key(&e.value()).unwrap_or_default();
                        query.with_mut(|q| q.sort = sort);
                    },
                    for sort in Sort::ALL {
                        option { key: "{sort.key()}", value: sort.key(), "{sort_label(sort)}" }
                    }
                }
            }
        }
        if gone_tab() {
            p { class: "muted small",
                if search_query.period != Period::All { "Disappeared {period_label(search_query.period)}. " }
                "Disappeared listings may be sold or delisted; warframe.market does not confirm sales."
            }
        }
        if result.outliers_hidden > 0 && !gone_tab() {
            button {
                class: "link-button",
                onclick: move |_| query.with_mut(|q| q.include_outliers = true),
                "{result.outliers_hidden} with invalid prices hidden"
            }
        }
        {rsx! { ListingFeed {
            key: "{gone_tab()}",
            query: search_query,
            gone: gone_tab(),
            initial: if gone_tab() { result.gone.clone() } else { result.listings.clone() },
            total: if gone_tab() { result.gone_total } else { result.total },
            now, show_weapon, weapons,
        } }}
    }
}

/// A feed owns its requests, so changing weapon, filters or tab cancels stale loads.
#[component]
fn ListingFeed(
    query: Query,
    gone: bool,
    initial: Vec<Listing>,
    total: usize,
    now: i64,
    show_weapon: bool,
    weapons: Vec<crate::api::WeaponInfo>,
) -> Element {
    let mut rows = use_signal(|| initial);
    let mut offset = use_signal(|| rows.peek().len());
    let mut more = use_signal(|| *offset.peek() < total);
    let mut loading = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let load = EventHandler::new(move |()| {
        if loading() || !more() {
            return;
        }
        loading.set(true);
        error.set(None);
        let query = query.clone();
        spawn(async move {
            match api::listing_page(query, offset(), gone).await {
                Ok(page) => {
                    let (batch, total) = if gone {
                        (page.gone, page.gone_total)
                    } else {
                        (page.listings, page.total)
                    };
                    let count = batch.len();
                    offset += count;
                    more.set(count > 0 && offset() < total);
                    rows.with_mut(|rows| {
                        for listing in batch {
                            if !rows.iter().any(|l| l.auction_id == listing.auction_id) {
                                rows.push(listing);
                            }
                        }
                    });
                }
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    });
    rsx! {
        div { id: "listing-panel", role: "tabpanel",
            aria_labelledby: if gone { "gone-tab" } else { "active-tab" },
            aria_busy: loading(),
            div { class: if gone { "listings gone" } else { "listings" },
                for l in rows.read().iter() {
                    ListingRow { key: "{l.auction_id}", listing: l.clone(), now, show_weapon,
                        has_image: weapons.iter().any(|w| w.slug == l.weapon_slug && w.has_image),
                    }
                }
            }
            if rows.read().is_empty() {
                div { class: "empty-state", "No listings match this tab. Try loosening the filters." }
            }
            if loading() {
                p { class: "listing-load", role: "status", "Loading more listings…" }
            } else if let Some(message) = error() {
                div { class: "listing-load", role: "alert",
                    p { class: "error", "Could not load more listings: {message}" }
                    button { onclick: move |_| load.call(()), "Retry" }
                }
            } else if more() {
                LoadMore { key: "{offset()}", onload: load }
            } else if !rows.read().is_empty() {
                p { class: "listing-load muted small", "All matching listings loaded." }
            }
        }
    }
}

/// Observe one sentinel per page; disconnect on firing or when the feed is removed.
#[component]
fn LoadMore(onload: EventHandler<()>) -> Element {
    rsx! {
        div { id: "listing-sentinel", class: "listing-load",
            onmounted: move |_| async move {
                let mut eval = document::eval(r#"
                    const node = document.getElementById('listing-sentinel');
                    if (!node || !window.IntersectionObserver) return;
                    await new Promise(resolve => {
                        const cleanup = () => { observer.disconnect(); removed.disconnect(); resolve(); };
                        const observer = new IntersectionObserver(entries => {
                            if (entries.some(entry => entry.isIntersecting)) {
                                dioxus.send(true);
                                cleanup();
                            }
                        }, { rootMargin: '400px' });
                        const removed = new MutationObserver(() => {
                            if (!node.isConnected) cleanup();
                        });
                        removed.observe(document.body, {childList: true, subtree: true});
                        observer.observe(node);
                    });
                "#);
                if let Ok(true) = eval.recv::<bool>().await { onload.call(()); }
            },
            button { onclick: move |_| onload.call(()), "Load more listings" }
        }
    }
}

fn sort_label(sort: Sort) -> &'static str {
    match sort {
        Sort::PriceAsc => "Cheapest first",
        Sort::PriceDesc => "Most expensive first",
        Sort::QualityDesc => "Best rolls first",
        Sort::Newest => "Newest first",
    }
}

/// The period as a phrase, e.g. "in the last 7 days".
fn period_label(period: Period) -> String {
    match period {
        Period::Today => "today".into(),
        Period::Week => "in the last 7 days".into(),
        Period::Month => "in the last 30 days".into(),
        Period::All => "all time".into(),
        Period::Custom { from, to } => match (from, to) {
            (Some(f), Some(t)) if f == t => format!("on {}", format_date(f)),
            (Some(f), Some(t)) => format!("from {} to {}", format_date(f), format_date(t)),
            (Some(f), None) => format!("since {}", format_date(f)),
            (None, Some(t)) => format!("until {}", format_date(t)),
            (None, None) => "all time".into(),
        },
    }
}

fn bucket_label(hours: u32) -> String {
    match hours {
        1 => "hour".into(),
        24 => "day".into(),
        168 => "week".into(),
        h if h % 24 == 0 => format!("{} days", h / 24),
        h => format!("{h} hours"),
    }
}

#[component]
fn ListingRow(listing: Listing, now: i64, show_weapon: bool, has_image: bool) -> Element {
    let navigator = use_navigator();
    let l = &listing;
    let target = Route::RivenPage {
        id: l.riven_id.clone(),
        auction: l.auction_id.clone(),
    };
    let row_target = target.clone();
    let price = l.price.map(format_plat).unwrap_or_else(|| "—".into());
    let kind = if l.direct_sell {
        "buyout"
    } else if l.top_bid.is_some() {
        "top bid"
    } else {
        "starting bid"
    };
    let quality = l.mean_quality.map(|q| format!("{:.0}%", q * 100.0));
    let invalid_limit = l.price_limit.map(format_plat).unwrap_or_default();
    let rolls = l.re_rolls.unwrap_or(0);
    let mr = l.mastery_level.unwrap_or(0);
    let age = format_age(now, l.created);

    rsx! {
        div { class: "listing",
            div { class: "listing-main", onclick: move |_| { navigator.push(row_target.clone()); },
                if show_weapon {
                    WeaponImage {
                        slug: l.weapon_slug.clone(),
                        has_image,
                        name: l.weapon_name.clone(),
                        size: "sm",
                    }
                }
                div { class: "price",
                    span { class: "plat", "{price}p" }
                    span { class: "muted small", "{kind}" }
                }
                div { class: "name",
                    div { class: "riven-name",
                        Link { to: target, onclick: move |e: MouseEvent| e.stop_propagation(), "{l.weapon_name} {l.riven_name}" }
                    }
                    div { class: "muted small",
                        "MR{mr} · {rolls} rolls · {l.polarity}"
                    }
                    RivenId { id: l.riven_id.clone(), short: true }
                    div {
                        if let Some(t) = l.gone_at {
                            span { class: "tag gone", title: "{l.gone_reason.clone().unwrap_or_default()}",
                                "gone {format_age(now, t)} ago"
                            }
                        }
                        if l.gone_at.is_none() {
                            span { class: "tag info",
                                title: "Last observed asking price; listings outside the fetched pages remain unconfirmed until verified or expired.",
                                if l.unconfirmed { "unconfirmed · " }
                                "seen {format_age(now, l.last_seen)} ago"
                            }
                        }
                        if l.copies > 1 {
                            span {
                                class: "tag info",
                                title: "Same riven seen in {l.copies} auctions",
                                if l.sellers > 1 { "{l.sellers} sellers" } else { "listed {l.copies}×" }
                            }
                        }
                        if l.legacy_stats == Some(true) {
                            span { class: "tag warn", "pre-rework" }
                        }
                        if !l.fit_ok {
                            span { class: "tag bad", "unverified" }
                        }
                        if l.price_outlier {
                            span {
                                class: "tag bad",
                                title: "Far above this weapon's normal high prices (limit {invalid_limit}p). Not counted in charts or medians.",
                                "invalid price"
                            }
                        }
                    }
                }
                // Buyers max a riven anyway, so stats show at rank 8 when they could be verified.
                div { class: "attrs", title: "Stats at max rank",
                    for a in l.attributes.iter() {
                        div {
                            key: "{a.slug}",
                            class: if a.positive { "attr positive" } else { "attr negative" },
                            span { class: "attr-value", "{format_value(&a.slug, a.value_rank8.unwrap_or(a.value))}" }
                            span { class: "attr-name", "{attribute_name(&a.slug)}" }
                            if let Some(q) = a.quality {
                                span { class: "quality", title: "roll {q * 100.0:.0}%",
                                    span { class: "quality-bar", style: "width: {q * 100.0:.0}%" }
                                }
                            }
                        }
                    }
                }
                div { class: "meta",
                    if let Some(q) = quality {
                        div { class: "roll", title: "average roll of the positive stats", "{q} roll" }
                    }
                    div { class: "small seller", "{l.owner_name}" }
                    div { class: "muted small", "listed {age} ago" }
                }
            }
        }
    }
}

/// Enough of a riven id to tell rivens apart in a list.
const SHORT_RIVEN_ID_LEN: usize = 8;
/// How long the copy button shows that it copied.
const COPIED_NOTICE_MS: u32 = 1500;

/// A riven id with a copy button. Clicks don't bubble up to the listing row.
#[component]
pub(crate) fn RivenId(id: String, short: bool) -> Element {
    let mut copied = use_signal(|| false);
    let shown = if short {
        id.chars().take(SHORT_RIVEN_ID_LEN).collect()
    } else {
        id.clone()
    };
    let script = format!("navigator.clipboard.writeText({id:?})");
    rsx! {
        span {
            class: "riven-id",
            title: "Riven id {id}: the same for every listing of this riven",
            onclick: move |e| e.stop_propagation(),
            code { "{shown}" }
            button {
                class: if copied() { "copy copied" } else { "copy" },
                title: "Copy riven id",
                onclick: move |e| {
                    e.stop_propagation();
                    document::eval(&script);
                    copied.set(true);
                    spawn(async move {
                        sleep_ms(COPIED_NOTICE_MS).await;
                        copied.set(false);
                    });
                },
                if copied() { "copied" } else { "copy" }
            }
        }
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn weapon_and_search_routes_round_trip() {
        for slug in ["torid", "arca_plasmor", "kuva_bramma"] {
            let route = Route::Weapon { slug: slug.into() };
            assert_eq!(route.to_string(), format!("/weapons/{slug}"));
            assert_eq!(route.to_string().parse::<Route>().unwrap(), route);
        }
        let route = Route::Search {
            id: "1234abcd".into(),
        };
        assert_eq!(route.to_string(), "/search/1234abcd");
        assert_eq!(route.to_string().parse::<Route>().unwrap(), route);
    }

    #[test]
    fn url_controls_weapon_selection_without_losing_filters() {
        let filters = Query {
            // Stale selections must never override a URL reached via Back/Forward.
            weapon: Some("old_weapon".into()),
            riven_id: Some("old_id".into()),
            price_changed: true,
            owner_changed: true,
            min_price: Some(250),
            ..Query::default()
        };
        for path in [
            "/weapons/torid",
            "/weapons/arca_plasmor",
            "/weapons/torid",
            "/",
            "/search/1234abcd",
        ] {
            let route = path.parse::<Route>().unwrap();
            let query = query_for_route(filters.clone(), &route);
            assert!(query.price_changed && query.owner_changed);
            assert_eq!(query.min_price, Some(250));
            match route {
                Route::Weapon { slug } => {
                    assert_eq!(query.weapon, Some(slug));
                    assert_eq!(query.riven_id, None);
                }
                Route::Search { id } => {
                    assert_eq!(query.weapon, None);
                    assert_eq!(query.riven_id, Some(id));
                }
                _ => assert!(query.weapon.is_none() && query.riven_id.is_none()),
            }
        }
        assert_eq!(filters.weapon.as_deref(), Some("old_weapon"));
    }

    #[test]
    fn edited_scans_survive_the_page_url_and_check_their_layout() {
        use crate::api::{RivenScan, ScanAttr};
        let attr = |slug: &str, value: f64, positive: bool| ScanAttr {
            slug: slug.into(),
            value,
            positive,
        };
        let mut scan = RivenScan {
            weapon: Some("okina".into()),
            name: Some("argi-cronicon".into()),
            mastery_rank: Some(16),
            rerolls: None,
            attributes: vec![
                attr("fire_rate_/_attack_speed", 82.2, true),
                attr("damage_vs_grineer", 1.61, true),
                attr("zoom", -45.1, false),
            ],
            warnings: Vec::new(),
        };
        assert!(crate::scan::valid_layout(&scan.attributes));
        let route = Route::ScannedRivenPage {
            scan: scan.to_param(),
        };
        let Route::ScannedRivenPage { scan: param } = route.to_string().parse().unwrap() else {
            panic!("not a scanned riven route");
        };
        assert_eq!(RivenScan::from_param(&param), Some(scan.clone()));

        // One positive, or two negatives, isn't a riven.
        scan.attributes.remove(0);
        assert!(!crate::scan::valid_layout(&scan.attributes));
        scan.attributes.push(attr("range", 1.0, true));
        assert!(crate::scan::valid_layout(&scan.attributes));
        scan.attributes[2].positive = false;
        assert!(!crate::scan::valid_layout(&scan.attributes));
    }

    #[test]
    fn roll_ranges_span_the_worst_to_the_best_roll() {
        let close = |(a, b): (f64, f64), (x, y): (f64, f64)| {
            assert!(
                (a - x).abs() < 1e-9 && (b - y).abs() < 1e-9,
                "{a},{b} vs {x},{y}"
            );
        };
        // A perfect roll is the top of its range; the bottom is 0.9 / 1.1 of it.
        close(
            api::roll_range("critical_chance", 110.0, 1.0),
            (90.0, 110.0),
        );
        close(api::roll_range("critical_chance", 90.0, 0.0), (90.0, 110.0));
        close(
            api::roll_range("critical_chance", 100.0, 0.5),
            (90.0, 110.0),
        );
        // Negatives run from mildest to harshest.
        close(api::roll_range("zoom", -50.0, 0.5), (-45.0, -55.0));
        // Faction damage scales the change, not the multiplier.
        close(api::roll_range("damage_vs_grineer", 1.5, 0.5), (1.45, 1.55));
        close(
            api::roll_range("damage_vs_grineer", 0.95, 0.5),
            (0.955, 0.945),
        );
    }

    #[test]
    fn date_inputs_round_trip_and_reject_garbage() {
        for ms in [0i64, 1_700_000_000_000, 951_782_400_000, 4_107_542_400_000] {
            let day = ms - ms.rem_euclid(DAY_MS);
            assert_eq!(parse_date(&format_date(ms)), Some(day));
        }
        assert_eq!(parse_date("2024-02-29"), Some(1_709_164_800_000));
        for bad in ["", "2024", "2024-13-01", "2024-01-xx", "2024-00-10"] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    #[test]
    fn periods_resolve_to_utc_days_and_pick_a_bucket_size() {
        let now: i64 = 1_700_000_000_000 + 5 * 3_600_000;
        let midnight = now - now.rem_euclid(DAY_MS);
        assert_eq!(Period::Today.bounds(now), (Some(midnight), None));
        assert_eq!(Period::Week.bounds(now), (Some(now - 7 * DAY_MS), None));
        assert_eq!(Period::All.bounds(now), (None, None));
        // The end day is inclusive.
        assert_eq!(
            Period::custom(Some(midnight), Some(midnight)).bounds(now),
            (Some(midnight), Some(midnight + DAY_MS))
        );
        assert_eq!(Period::custom(None, None), Period::All);
        assert_eq!(
            Period::custom(Some(midnight), Some(midnight - DAY_MS)),
            Period::Custom {
                from: Some(midnight - DAY_MS),
                to: Some(midnight)
            }
        );

        assert_eq!(Query::default().period, Period::Week);
        let q = Query {
            period: Period::All,
            ..Query::default()
        };
        assert_eq!(active_filters(&q), 1);
    }

    #[test]
    fn history_filters_are_counted_and_reset_with_defaults() {
        let mut q = Query::default();
        assert_eq!(active_filters(&q), 0);
        q.price_changed = true;
        assert_eq!(active_filters(&q), 1);
        q.owner_changed = true;
        assert_eq!(active_filters(&q), 2);
        q = Query {
            weapon: Some("torid".into()),
            ..Query::default()
        };
        assert_eq!(active_filters(&q), 0);
        assert!(!q.price_changed && !q.owner_changed);
    }

    #[test]
    fn detail_links_round_trip_and_allow_no_auction() {
        let route = Route::RivenPage {
            id: "1234abcd".into(),
            auction: "auction-1".into(),
        };
        assert_eq!(route.to_string().parse::<Route>().unwrap(), route);
        assert_eq!(
            "/riven/1234abcd".parse::<Route>().unwrap(),
            Route::RivenPage {
                id: "1234abcd".into(),
                auction: String::new()
            }
        );
        assert_eq!("/".parse::<Route>().unwrap(), Route::Market {});
    }

    #[test]
    fn scanned_riven_links_round_trip() {
        let scan = api::RivenScan {
            weapon: Some("arca_plasmor".into()),
            name: Some("Manti-acritox".into()),
            mastery_rank: Some(16),
            rerolls: None,
            attributes: vec![
                api::ScanAttr {
                    slug: "damage_vs_corpus".into(),
                    value: 1.31,
                    positive: true,
                },
                api::ScanAttr {
                    slug: "fire_rate_/_attack_speed".into(),
                    value: 13.5,
                    positive: true,
                },
                api::ScanAttr {
                    slug: "recoil".into(),
                    value: 6.5,
                    positive: false,
                },
                api::ScanAttr {
                    slug: "ammo_maximum".into(),
                    value: -47.2,
                    positive: false,
                },
            ],
            warnings: vec!["not in the url".into()],
        };
        let route = Route::ScannedRivenPage {
            scan: scan.to_param(),
        };
        let parsed = route.to_string().parse::<Route>().unwrap();
        assert_eq!(parsed, route);
        let Route::ScannedRivenPage { scan: param } = parsed else {
            unreachable!()
        };
        assert_eq!(
            api::RivenScan::from_param(&param),
            Some(api::RivenScan {
                warnings: Vec::new(),
                ..scan
            })
        );
        assert_eq!(api::RivenScan::from_param("garbage"), None);
    }
}
