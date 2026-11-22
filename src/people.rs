//! Sellers on warframe.market: a sortable list of everyone seen, and a page per seller with
//! the rivens they listed and who those passed between.

use dioxus::prelude::*;

use crate::api::{
    self, Overview, PEOPLE_PAGE_SIZE, Page, PeopleQuery, PeopleResult, PeopleSort, Period, Person,
    PersonDetail, PersonFilter,
};
use crate::ui::{PeriodPicker, Route, format_age, format_date, period_label, sleep_ms};

/// How long typing in the search box pauses before it searches.
const SEARCH_DEBOUNCE_MS: u32 = 250;

fn sort_label(sort: PeopleSort) -> &'static str {
    match sort {
        PeopleSort::Auctions => "Most auctions",
        PeopleSort::Active => "Most listed now",
        PeopleSort::Resales => "Most resales",
        PeopleSort::SoldOn => "Most sold on",
        PeopleSort::Reputation => "Highest reputation",
        PeopleSort::LastSeen => "Recently seen",
    }
}

/// A seller's profile on warframe.market.
pub fn profile_url(name: &str) -> String {
    // Names are letters, digits, `-`, `_` and `.`; anything else is escaped to be safe.
    let mut url = String::from("https://warframe.market/profile/");
    for b in name.trim().bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            url.push(char::from(b));
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    url
}

/// A seller's name, linking to their warframe.market profile, with an icon for their page here.
/// Clicks stay on the links, so a clickable row around it doesn't open too.
#[component]
pub fn SellerName(name: String, #[props(default)] style: Option<String>) -> Element {
    if name.trim().is_empty() {
        return rsx! { span { class: "seller-name muted", "unknown seller" } };
    }
    rsx! {
        span { class: "seller-name",
            a {
                href: profile_url(&name),
                target: "_blank",
                rel: "noopener noreferrer",
                style,
                title: "{name} on warframe.market (opens in a new tab)",
                onclick: move |e: MouseEvent| e.stop_propagation(),
                "{name}"
            }
            Link {
                class: "seller-page",
                to: Route::PersonPage { name: name.clone() },
                title: "{name}'s rivens and resales on Riven Tracker",
                aria_label: "{name}'s rivens and resales on Riven Tracker",
                onclick: move |e: MouseEvent| e.stop_propagation(),
                svg { view_box: "0 0 16 16", width: "12", height: "12", fill: "currentColor", "aria-hidden": "true",
                    path { d: "M8 8a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm0 1.5c-3 0-5.5 1.6-5.5 3.6V14h11v-.9c0-2-2.5-3.6-5.5-3.6Z" }
                }
            }
        }
    }
}

/// Rows of a list, loaded a page at a time.
trait Paged: Clone + 'static {
    fn len(&self) -> usize;
    fn total(&self) -> usize;
    /// Adds the next page, taking its totals.
    fn append(&mut self, next: Self);
}

impl Paged for PeopleResult {
    fn len(&self) -> usize {
        self.people.len()
    }
    fn total(&self) -> usize {
        self.total
    }
    fn append(&mut self, next: Self) {
        self.people.extend(next.people);
        self.total = next.total;
        self.everyone = next.everyone;
    }
}

impl Paged for PersonDetail {
    fn len(&self) -> usize {
        self.rivens.len()
    }
    fn total(&self) -> usize {
        self.total
    }
    fn append(&mut self, next: Self) {
        self.rivens.extend(next.rivens);
        self.person = next.person;
        self.total = next.total;
    }
}

/// A list shown in pages. What's shown is always one answer from the server, for one key,
/// with the pages loaded onto it; nothing else holds rows of its own, so a new answer always
/// replaces the old one everywhere. New data refetches everything loaded, a new key one page.
struct Pages<K: 'static, R: 'static> {
    /// The key the shown rows are for, and the rows.
    shown: Signal<Option<(K, R)>>,
    /// Why the first pages couldn't be loaded.
    error: Signal<Option<String>>,
    loading_more: Signal<bool>,
    more_error: Signal<Option<String>>,
    load_more: Callback<()>,
    resource: Resource<()>,
}

impl<K: 'static, R: 'static> Clone for Pages<K, R> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<K: 'static, R: 'static> Copy for Pages<K, R> {}

impl<K: Clone + PartialEq + 'static, R: Paged> Pages<K, R> {
    /// The shown rows are for another key than `key`, which is loading.
    fn stale(&self, key: &K) -> bool {
        self.shown.read().as_ref().is_some_and(|(k, _)| k != key)
    }
}

/// `key` reads the signals the list depends on; `fetch` asks the server for a page of it.
fn use_pages<K, R, Fut>(
    mut key: impl FnMut() -> K + 'static,
    fetch: fn(K, Page) -> Fut,
) -> Pages<K, R>
where
    K: Clone + PartialEq + 'static,
    R: Paged,
    Fut: Future<Output = Result<R, ServerFnError>> + 'static,
{
    let version = use_context::<Signal<u64>>();
    let mut shown = use_signal(|| None::<(K, R)>);
    let mut error = use_signal(|| None::<String>);
    // Restarting cancels the request under way, so an older answer never lands last.
    let resource = use_resource(move || {
        version.read();
        let key = key();
        let loaded = shown
            .peek()
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map_or(0, |(_, r)| r.len());
        async move {
            match fetch(key.clone(), Page::new(0, loaded.max(PEOPLE_PAGE_SIZE))).await {
                Ok(answer) => {
                    shown.set(Some((key, answer)));
                    error.set(None);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        }
    });
    let mut loading_more = use_signal(|| false);
    let mut more_error = use_signal(|| None::<String>);
    let load_more = use_callback(move |()| {
        let Some((key, offset)) = shown.peek().as_ref().map(|(k, r)| (k.clone(), r.len())) else {
            return;
        };
        if loading_more() {
            return;
        }
        loading_more.set(true);
        more_error.set(None);
        spawn(async move {
            match fetch(key.clone(), Page::new(offset, PEOPLE_PAGE_SIZE)).await {
                // Only onto the rows it continues: not after a new key, or a refresh.
                Ok(next) => shown.with_mut(|shown| {
                    if let Some((k, rows)) = shown
                        && *k == key
                        && rows.len() == offset
                    {
                        rows.append(next);
                    }
                }),
                Err(e) => more_error.set(Some(e.to_string())),
            }
            loading_more.set(false);
        });
    });
    Pages {
        shown,
        error,
        loading_more,
        more_error,
        load_more,
        resource,
    }
}

/// "Load more" under a list, with its loading and error states.
#[component]
fn MoreRows(
    loading: bool,
    error: Option<String>,
    more: bool,
    what: &'static str,
    onload: Callback<()>,
) -> Element {
    rsx! {
        if loading {
            p { class: "listing-load", role: "status", "Loading more {what}…" }
        } else if let Some(message) = error {
            div { class: "listing-load", role: "alert",
                p { class: "error", "Could not load more {what}: {message}" }
                button { onclick: move |_| onload.call(()), "Retry" }
            }
        } else if more {
            div { class: "listing-load",
                button { onclick: move |_| onload.call(()), "Load more {what}" }
            }
        }
    }
}

#[component]
pub fn PeoplePage() -> Element {
    let mut query = use_context::<Signal<PeopleQuery>>();
    // What's typed, ahead of the search it starts.
    let mut draft = use_signal(|| query.peek().search.clone());
    let pages = use_pages(move || query.cloned(), api::people);
    let asked = query();
    let shown = pages.shown.read().clone();
    let loading = pages.resource.pending() && pages.stale(&asked);
    let server_time = shown.as_ref().map(|(_, r)| r.server_time);
    let mut set_sort = move |sort: PeopleSort| query.with_mut(|q| q.sort = sort);
    // The number of sellers sits in the search box; a search adds how many it matched.
    let placeholder = match &shown {
        Some((_, r)) => format!(
            "Search {} sellers…",
            crate::scraper::count(r.everyone as u64)
        ),
        None => "Search sellers…".to_owned(),
    };
    let matches = shown
        .as_ref()
        .filter(|(q, _)| !q.search.trim().is_empty())
        .map(|(_, r)| match r.total {
            1 => "1 match".to_owned(),
            n => format!("{} matches", crate::scraper::count(n as u64)),
        });

    rsx! {
        document::Title { "People · Riven Tracker" }
        main { class: "people-page",
            div { class: "alerts-intro",
                h1 { "People" }
                p { class: "muted",
                    "Sellers on warframe.market. A resale is a riven someone else listed first, most likely bought and relisted."
                }
            }
            div { class: "people-toolbar",
                div { class: "people-search-row",
                input {
                    r#type: "search",
                    class: "people-search",
                    aria_label: "Search sellers",
                    title: "Sellers with a listing or a riven sold on {period_label(asked.period)}",
                    placeholder,
                    maxlength: "100",
                    value: "{draft}",
                    oninput: move |e| {
                        let value = e.value();
                        draft.set(value.clone());
                        spawn(async move {
                            sleep_ms(SEARCH_DEBOUNCE_MS).await;
                            if *draft.peek() == value && query.peek().search != value {
                                query.with_mut(|q| q.search = value);
                            }
                        });
                    },
                }
                // The column headers sort on wide screens; narrow ones hide some columns.
                select {
                    class: "compact people-sort",
                    aria_label: "Sort people",
                    value: asked.sort.key(),
                    onchange: move |e| set_sort(PeopleSort::from_key(&e.value()).unwrap_or_default()),
                    for s in PeopleSort::ALL {
                        option { key: "{s.key()}", value: s.key(), "{sort_label(s)}" }
                    }
                }
                }
                PeoplePeriodPicker { period: asked.period, now: server_time }
                if let Some(matches) = matches {
                    span { class: "people-count muted small", role: "status", "{matches}" }
                }
            }
            match (shown, pages.error.read().clone()) {
                (None, Some(e)) => rsx! { div { class: "error", "Could not load people: {e}" } },
                (None, None) => rsx! {
                    div { class: "results-skeleton", role: "status", aria_label: "Loading people",
                        for i in 0..8 { div { key: "{i}", class: "skeleton skeleton-row" } }
                    }
                },
                (Some((q, result)), error) => rsx! {
                    if let Some(e) = error {
                        p { class: "error", role: "alert", "Could not refresh the list: {e}" }
                    }
                    div { class: if loading { "results loading" } else { "results" }, aria_busy: loading,
                        PeopleTable { query: q, result, set_sort }
                    }
                    MoreRows {
                        loading: *pages.loading_more.read(),
                        error: pages.more_error.read().clone(),
                        more: pages.shown.read().as_ref().is_some_and(|(_, r)| r.len() < r.total()),
                        what: "sellers",
                        onload: pages.load_more,
                    }
                },
            }
        }
    }
}

/// The market's period choices, for the counts on the people pages. Both pages share the
/// period, through the people list's query.
#[component]
fn PeoplePeriodPicker(period: Period, now: Option<i64>) -> Element {
    let mut query = use_context::<Signal<PeopleQuery>>();
    rsx! {
        div { class: "people-period",
            title: "Counts what sellers did in this period. Days are UTC.",
            PeriodPicker {
                period,
                now,
                onchange: move |period| query.with_mut(|q| q.period = period),
            }
        }
    }
}

/// A count, or a quiet dash for none.
#[component]
fn Count(value: u32, #[props(default)] class: &'static str) -> Element {
    rsx! {
        if value == 0 {
            td { class: "num none {class}", aria_label: "0", "–" }
        } else {
            td { class: "num {class}", "{value}" }
        }
    }
}

/// The loaded sellers. Holds no rows of its own: it shows whatever answer it's given.
#[component]
fn PeopleTable(
    query: PeopleQuery,
    result: PeopleResult,
    set_sort: EventHandler<PeopleSort>,
) -> Element {
    let navigator = use_navigator();
    let now = result.server_time;
    let sort = query.sort;
    // A column header that sorts by it.
    let header =
        move |label: &'static str, by: PeopleSort, title: &'static str, class: &'static str| {
            rsx! {
                th {
                    class: "num {class}",
                    scope: "col",
                    "aria-sort": if sort == by { "descending" } else { "none" },
                    button {
                        class: if sort == by { "sort-header active" } else { "sort-header" },
                        title,
                        onclick: move |_| set_sort.call(by),
                        "{label}"
                        // Always there, so the columns don't shift when the sort moves.
                        span { class: "sort-arrow", aria_hidden: "true", "↓" }
                    }
                }
            }
        };
    rsx! {
        if result.people.is_empty() {
            div { class: "empty-state",
                if result.everyone == 0 {
                    "Nobody listed a riven {period_label(query.period)}. Try a longer period."
                } else {
                    "No seller matches this search."
                }
            }
        } else {
            div { class: "people-table-wrap",
                table { class: "people-table",
                    thead {
                        tr {
                            th { scope: "col", "Seller" }
                            {header("Auctions", PeopleSort::Auctions, "Auctions listed in the period, relists of the same riven included", "")}
                            {header("Listed now", PeopleSort::Active, "Rivens they still list now, of those listed in the period", "secondary")}
                            {header("Resales", PeopleSort::Resales, "Rivens that came to them from another seller in the period", "")}
                            {header("Sold on", PeopleSort::SoldOn, "Rivens that went from them to another seller in the period", "optional")}
                            {header("Rep", PeopleSort::Reputation, "warframe.market reputation, when last seen", "optional")}
                            {header("Seen", PeopleSort::LastSeen, "When one of their auctions was last seen", "optional")}
                        }
                    }
                    tbody {
                        for p in result.people.iter() {
                            PersonRow { key: "{p.name}", person: p.clone(), now,
                                onopen: move |name: String| { navigator.push(Route::PersonPage { name }); },
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn PersonRow(person: Person, now: i64, onopen: EventHandler<String>) -> Element {
    let p = &person;
    let name = p.name.clone();
    rsx! {
        tr { class: "person-row", onclick: move |_| onopen.call(name.clone()),
            td { class: "person-name",
                Link { to: Route::PersonPage { name: p.name.clone() },
                    onclick: move |e: MouseEvent| e.stop_propagation(),
                    "{p.name}"
                }
            }
            Count { value: p.auctions }
            Count { value: p.active, class: "secondary" }
            Count { value: p.resales, class: "resales" }
            Count { value: p.sold_on, class: "optional" }
            match p.reputation {
                Some(rep) => rsx! { td { class: "num optional muted", "{rep}" } },
                None => rsx! { td { class: "num optional none", "–" } },
            }
            td { class: "num optional muted", "{format_age(now, p.last_seen)}" }
        }
    }
}

fn filter_label(filter: PersonFilter) -> &'static str {
    match filter {
        PersonFilter::Active => "Listed now",
        PersonFilter::All => "All",
        PersonFilter::Resales => "Resales",
        PersonFilter::SoldOn => "Sold on",
    }
}

fn filter_title(filter: PersonFilter) -> &'static str {
    match filter {
        PersonFilter::Active => "Rivens they still list now, of those listed in the period",
        PersonFilter::All => {
            "Rivens they listed in the period, one row each: their newest auction of it"
        }
        PersonFilter::Resales => {
            "Rivens that came to them from another seller in the period, most likely bought"
        }
        PersonFilter::SoldOn => {
            "Rivens that went from them to another seller in the period, most likely sold"
        }
    }
}

fn filter_count(filter: PersonFilter, p: &Person) -> u32 {
    match filter {
        PersonFilter::Active => p.active,
        PersonFilter::All => p.rivens,
        PersonFilter::Resales => p.resales,
        PersonFilter::SoldOn => p.sold_on,
    }
}

#[component]
pub fn PersonPage(name: String) -> Element {
    // Keyed so moving from one seller to another resets the page. Dioxus only remounts on a
    // new key in a list, hence the list of one.
    rsx! {
        for key in [name.clone()] {
            PersonContent { key: "{key}", name: name.clone() }
        }
    }
}

/// What a seller's page shows: whose, which tab, which period.
type PersonKey = (String, PersonFilter, Period);

#[component]
fn PersonContent(name: String) -> Element {
    let query = use_context::<Signal<PeopleQuery>>();
    let mut filter = use_signal(PersonFilter::default);
    // Everyone's page opens on what they list now, unless they list nothing.
    let mut chose = use_signal(|| false);
    let key_name = name.clone();
    let pages = use_pages(
        move || -> PersonKey { (key_name.clone(), filter(), query().period) },
        |(name, filter, period), page| async move {
            api::person(name, filter, period, page)
                .await?
                .ok_or_else(|| ServerFnError::new(NOT_FOUND))
        },
    );
    use_effect(move || {
        if let Some(((_, PersonFilter::Active, _), d)) = &*pages.shown.read()
            && d.person.active == 0
            && !*chose.peek()
        {
            filter.set(PersonFilter::All);
        }
    });
    let asked: PersonKey = (name.clone(), filter(), query().period);
    let loading = pages.resource.pending() && pages.stale(&asked);
    let shown = pages.shown.read().clone();
    let error = pages.error.read().clone();
    rsx! {
        main { class: "riven-page person-page",
            match (shown, error) {
                (None, Some(e)) if e.contains(NOT_FOUND) => rsx! {
                    document::Title { "Seller not found · Riven Tracker" }
                    h2 { "Seller not found" }
                    p {
                        "No auction by “{name}” is in the stored data. "
                        Link { to: Route::PeoplePage {}, "Browse all sellers" }
                        "."
                    }
                },
                (None, Some(e)) => rsx! {
                    p { class: "error", "Could not load this seller: {e}" }
                    button { onclick: move |_| { let mut resource = pages.resource; resource.restart(); }, "Retry" }
                },
                (None, None) => rsx! { p { role: "status", "Loading seller…" } },
                (Some((_, data)), error) => rsx! {
                    document::Title { "{data.person.name} · Riven Tracker" }
                    PersonHeader { person: data.person.clone(), now: data.server_time }
                    if let Some(e) = error {
                        p { class: "error", role: "alert", "Could not refresh this seller: {e}" }
                    }
                    div { class: "listing-bar",
                        div { class: "listing-tabs", role: "tablist", aria_label: "Rivens",
                            for f in PersonFilter::ALL {
                                button {
                                    key: "{f:?}",
                                    role: "tab",
                                    title: filter_title(f),
                                    aria_selected: filter() == f,
                                    aria_controls: "person-rivens",
                                    class: if filter() == f { "active" } else { "" },
                                    onclick: move |_| { chose.set(true); filter.set(f); },
                                    "{filter_label(f)} "
                                    span { class: "tab-count", "{filter_count(f, &data.person)}" }
                                }
                            }
                        }
                        PeoplePeriodPicker { period: asked.2, now: Some(data.server_time) }
                    }
                    div { class: if loading { "results loading" } else { "results" }, aria_busy: loading,
                        PersonRivens { detail: data.clone() }
                    }
                    MoreRows {
                        loading: *pages.loading_more.read(),
                        error: pages.more_error.read().clone(),
                        more: data.rivens.len() < data.total,
                        what: "rivens",
                        onload: pages.load_more,
                    }
                },
            }
        }
    }
}

/// Stands in for a missing seller in the page's errors, which `use_pages` keeps as text.
const NOT_FOUND: &str = "no such seller";

#[component]
fn PersonHeader(person: Person, now: i64) -> Element {
    let p = &person;
    let platform = (!p.platform.is_empty()).then(|| p.platform.to_uppercase());
    rsx! {
        div { class: "person-header",
            h2 { class: "person-title", "{p.name}" }
            div { class: "muted small person-facts",
                if let Some(rep) = p.reputation { span { "Reputation {rep}" } }
                span { title: "Auctions listed in the chosen period", "{p.auctions} auctions" }
                span { "Last seen {format_age(now, p.last_seen)} ago" }
                if let Some(platform) = platform { span { "{platform}" } }
                span { "Listing since {format_date(p.first_listed)}" }
                a { href: profile_url(&p.name), target: "_blank", rel: "noopener noreferrer", "warframe.market profile ↗" }
            }
        }
    }
}

/// A seller's loaded rivens for one tab. Holds no rows of its own, like `PeopleTable`.
#[component]
fn PersonRivens(detail: PersonDetail) -> Element {
    let overview = use_context::<Signal<Option<Overview>>>();
    let has_image = move |slug: &str| {
        overview
            .read()
            .as_ref()
            .is_some_and(|o| o.weapons.iter().any(|w| w.slug == slug && w.has_image))
    };
    let now = detail.server_time;
    rsx! {
        div { id: "person-rivens", class: "listings", role: "tabpanel",
            if detail.rivens.is_empty() {
                div { class: "empty-state", "No rivens here for this period." }
            }
            for r in detail.rivens.iter() {
                crate::ui::ListingRow {
                    key: "{r.listing.auction_id}",
                    listing: r.listing.clone(),
                    now,
                    show_weapon: true,
                    has_image: has_image(&r.listing.weapon_slug),
                    show_seller: false,
                    bought_from: r.bought_from.clone(),
                    sold_to: r.sold_to.clone(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::profile_url;

    #[test]
    fn profile_links_keep_plain_names_and_escape_the_rest() {
        assert_eq!(
            profile_url("Westvyilii."),
            "https://warframe.market/profile/Westvyilii."
        );
        assert_eq!(
            profile_url(" a b/c "),
            "https://warframe.market/profile/a%20b%2Fc"
        );
    }
}
