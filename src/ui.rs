use dioxus::prelude::*;

use crate::api::{
    self, AttrInfo, CurseFilter, Listing, LiveStats, Overview, Query, SearchResult, Sort,
    attribute_name, format_value,
};
use crate::chart::{PriceChart, Sparkline, StatEffectsPanel};
use crate::weapons::{WeaponGrid, WeaponHeader, WeaponImage, WeaponPicker};

const CSS: Asset = asset!("/assets/main.css");

/// `YYYY-MM-DD` (UTC) from unix ms.
pub fn format_date(ms: i64) -> String {
    let (y, m, d) = civil_from_days(ms.div_euclid(86_400_000));
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM` (UTC) from unix ms.
pub fn format_datetime(ms: i64) -> String {
    let secs = ms.div_euclid(1000).rem_euclid(86_400);
    format!(
        "{} {:02}:{:02}",
        format_date(ms),
        secs / 3600,
        secs % 3600 / 60
    )
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

pub fn format_plat(p: i64) -> String {
    if p >= 10_000 {
        format!("{:.1}k", p as f64 / 1000.0)
    } else {
        format!("{p}")
    }
}

pub fn format_age(now: i64, then: i64) -> String {
    let mins = (now - then).max(0) / 60_000;
    match mins {
        0..60 => format!("{mins}m"),
        60..1440 => format!("{}h", mins / 60),
        _ => format!("{}d", mins / 1440),
    }
}

/// Sleeps in the browser; on the server (where polling loops never need to fire) it uses tokio.
pub async fn sleep_ms(ms: u32) {
    #[cfg(target_arch = "wasm32")]
    gloo_timers::future::TimeoutFuture::new(ms).await;
    #[cfg(all(not(target_arch = "wasm32"), feature = "server"))]
    tokio::time::sleep(std::time::Duration::from_millis(u64::from(ms))).await;
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "server")))]
    std::future::pending::<()>().await;
}

/// How often the header checks for new scraped data.
const LIVE_POLL_MS: u32 = 5000;

#[component]
pub fn App() -> Element {
    let mut scraper_open = use_signal(|| false);
    let query = use_signal(Query::default);
    use_context_provider(|| query);

    let mut overview = use_resource(api::overview);
    // A failed refresh must not take down a page that already has data.
    let mut loaded = use_signal(|| None::<Overview>);
    use_effect(move || {
        if let Some(Ok(value)) = &*overview.read() {
            loaded.set(Some(value.clone()));
        }
    });

    // New data while scraping: the count updates right away, the rest refetches on a version
    // change, so nothing is requested while the data stands still.
    let mut live = use_resource(api::live_stats);
    let mut stats = use_signal(|| None::<LiveStats>);
    let mut data_version = use_signal(|| 0u64);
    use_context_provider(|| data_version);
    use_effect(move || {
        if let Some(Ok(value)) = &*live.read() {
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
    // Anything done in the header search box leads back to the market.
    use_effect(move || {
        query.read();
        if *scraper_open.peek() {
            scraper_open.set(false);
        }
    });

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
                    button { onclick: move |_| scraper_open.set(false), "Riven Tracker" }
                }
            }
            if let Some(o) = &data {
                WeaponPicker { weapons: o.weapons.clone(), query }
            } else {
                div {}
            }
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
        }
        if scraper_open() {
            crate::scraper::ScraperPanel {}
        } else if let Some(o) = data {
            MarketPage { overview: o, query, data_version }
        } else if let Some(error) = load_error {
            div { class: "fatal", "Failed to load data: {error}" }
        } else {
            div { class: "fatal muted", "Loading…" }
        }
    }
}

/// How many filters differ from the defaults. Sorting and chart buckets don't filter anything.
fn active_filters(q: &Query) -> usize {
    let d = Query::default();
    q.positives.len()
        + [
            q.curse != d.curse,
            q.direct_only != d.direct_only,
            q.include_legacy != d.include_legacy,
            q.include_unfit != d.include_unfit,
            q.include_outliers != d.include_outliers,
            q.min_quality != d.min_quality,
            q.min_price != d.min_price,
            q.max_price != d.max_price,
        ]
        .into_iter()
        .filter(|changed| *changed)
        .count()
}

#[component]
fn MarketPage(overview: Overview, query: Signal<Query>, data_version: Signal<u64>) -> Element {
    let mut query = query;
    // The query the shown results belong to. Only a new search dims the page, not live updates.
    let mut shown_query = use_signal(|| None::<Query>);
    let results = use_resource(move || async move {
        data_version.read();
        let q = query();
        let result = api::search(q.clone()).await;
        shown_query.set(Some(q));
        result
    });

    let selected = query
        .read()
        .weapon
        .as_ref()
        .and_then(|slug| overview.weapons.iter().find(|w| &w.slug == slug).cloned());
    let loading = results.pending() && shown_query().as_ref() != Some(&query());
    let riven_search = query.read().riven_id.clone();
    // Only matters on narrow screens, where the filters would otherwise push the results down.
    let mut filters_open = use_signal(|| false);
    let filter_count = active_filters(&query.read());

    let body = match &*results.read() {
        Some(Ok(r)) => rsx! {
            if let Some(id) = riven_search.clone() {
                div { class: "id-banner",
                    span { class: "muted", "Riven id" }
                    code { "{id}" }
                    span { class: "muted small",
                        if r.total + r.gone_total == 0 {
                            "no riven with this id"
                        } else {
                            "every auction of this riven, other filters ignored"
                        }
                    }
                    button {
                        class: "picker-clear",
                        title: "Back to the normal search",
                        onclick: move |_| query.with_mut(|q| q.riven_id = None),
                        "×"
                    }
                }
            } else if let Some(w) = selected.clone() {
                WeaponHeader { weapon: w, now: r.server_time }
            } else {
                WeaponGrid { weapons: overview.weapons.clone(), query }
            }
            Results {
                result: r.clone(),
                bucket_hours: query.read().bucket_hours,
                show_weapon: selected.is_none(),
                weapons: overview.weapons.clone(),
                expand: riven_search.is_some(),
                weapon: selected.as_ref().map(|w| w.slug.clone()),
            }
        },
        Some(Err(e)) => rsx! { div { class: "error", "Search failed: {e}" } },
        None => rsx! { div { class: "muted", "Searching…" } },
    };

    rsx! {
        document::Title {
            {selected.as_ref().map(|w| format!("{} rivens · Riven Tracker", w.name)).unwrap_or_else(|| "Riven Tracker".into())}
        }
        main {
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
                Filters { overview: overview.clone(), query }
            }
            section { class: if loading { "content loading" } else { "content" }, {body} }
        }
    }
}

#[component]
fn Filters(overview: Overview, query: Signal<Query>) -> Element {
    let mut query = query;
    let q = query();

    let class = q
        .weapon
        .as_ref()
        .and_then(|slug| overview.weapons.iter().find(|w| &w.slug == slug))
        .map(|w| w.class.clone());
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

    let curse_value = match &q.curse {
        CurseFilter::Any => "any".to_owned(),
        CurseFilter::None => "none".to_owned(),
        CurseFilter::Required => "required".to_owned(),
        CurseFilter::Attr(s) => format!("attr:{s}"),
    };
    let sort_value = match q.sort {
        Sort::PriceAsc => "price_asc",
        Sort::PriceDesc => "price_desc",
        Sort::QualityDesc => "quality",
        Sort::Newest => "newest",
    };
    let price_input = |v: Option<i64>| v.map(|v| v.to_string()).unwrap_or_default();

    rsx! {
        div { class: "filters",
            div { class: "filter-group",
                h3 { "Stats" }
                label { "Positive stats (all required)" }
                if !positives.is_empty() {
                    div { class: "chips",
                        for (slug, ok) in positives {
                            span {
                                key: "{slug}",
                                class: if ok { "chip positive" } else { "chip unavailable" },
                                title: if ok { "" } else { "Can't roll on this weapon" },
                                "+{attribute_name(&slug)}"
                                button {
                                    onclick: move |_| query.with_mut(|q| q.positives.retain(|p| p != &slug)),
                                    "×"
                                }
                            }
                        }
                    }
                }
                if q.positives.len() < 3 {
                    select {
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
                        option { value: "", "+ add stat" }
                        for a in positive_options.iter() {
                            option { key: "{a.slug}", value: "{a.slug}", "{a.name}" }
                        }
                    }
                }

                label { "Negative stat" }
                select {
                    value: "{curse_value}",
                    onchange: move |e| {
                        let v = e.value();
                        let curse = match v.as_str() {
                            "none" => CurseFilter::None,
                            "required" => CurseFilter::Required,
                            s if s.starts_with("attr:") => CurseFilter::Attr(s[5..].to_owned()),
                            _ => CurseFilter::Any,
                        };
                        query.with_mut(|q| q.curse = curse);
                    },
                    option { value: "any", "Doesn't matter" }
                    option { value: "none", "No negative" }
                    option { value: "required", "Has a negative" }
                    optgroup { label: "Specific negative",
                        for a in attrs.iter() {
                            option { key: "{a.slug}", value: "attr:{a.slug}", "−{a.name}" }
                        }
                    }
                }

                label { "Min roll quality: {(q.min_quality * 100.0).round()}%" }
                input {
                    r#type: "range",
                    min: "0",
                    max: "100",
                    step: "5",
                    value: "{(q.min_quality * 100.0).round()}",
                    oninput: move |e| {
                        let v = e.value().parse::<f64>().unwrap_or(0.0) / 100.0;
                        query.with_mut(|q| q.min_quality = v);
                    },
                }
            }

            div { class: "filter-group",
                h3 { "Price" }
                div { class: "row",
                    input {
                        r#type: "number",
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

            div { class: "filter-group",
                h3 { "Data quality" }
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: q.include_legacy,
                        onchange: move |e| query.with_mut(|q| q.include_legacy = e.checked()),
                    }
                    "Include pre-rework stats"
                }
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: q.include_unfit,
                        onchange: move |e| query.with_mut(|q| q.include_unfit = e.checked()),
                    }
                    "Include unverifiable stats"
                }
                label { class: "check",
                    input {
                        r#type: "checkbox",
                        checked: q.include_outliers,
                        onchange: move |e| query.with_mut(|q| q.include_outliers = e.checked()),
                    }
                    "Include invalid prices"
                }
            }

            div { class: "filter-group",
                h3 { "Display" }
                label { "Sort listings" }
                select {
                    value: "{sort_value}",
                    onchange: move |e| {
                        let sort = match e.value().as_str() {
                            "price_desc" => Sort::PriceDesc,
                            "quality" => Sort::QualityDesc,
                            "newest" => Sort::Newest,
                            _ => Sort::PriceAsc,
                        };
                        query.with_mut(|q| q.sort = sort);
                    },
                    option { value: "price_asc", "Cheapest first" }
                    option { value: "price_desc", "Most expensive first" }
                    option { value: "quality", "Best rolls first" }
                    option { value: "newest", "Newest first" }
                }
                label { "Chart buckets" }
                select {
                    value: "{q.bucket_hours}",
                    onchange: move |e| {
                        let v = e.value().parse().unwrap_or(24);
                        query.with_mut(|q| q.bucket_hours = v);
                    },
                    option { value: "6", "6 hours" }
                    option { value: "12", "12 hours" }
                    option { value: "24", "1 day" }
                    option { value: "168", "1 week" }
                }
            }

            button {
                class: "reset",
                onclick: move |_| {
                    query.with_mut(|q| {
                        let weapon = q.weapon.take();
                        *q = Query { weapon, ..Query::default() };
                    })
                },
                "Reset filters"
            }
        }
    }
}

#[component]
fn Results(
    result: SearchResult,
    bucket_hours: u32,
    show_weapon: bool,
    weapons: Vec<crate::api::WeaponInfo>,
    /// Open every row (used for riven id searches).
    expand: bool,
    /// The weapon being looked at, if any.
    weapon: Option<String>,
) -> Element {
    let mut query = use_context::<Signal<Query>>();
    let shown = result.listings.len();
    let now = result.server_time;
    let mut show_all_gone = use_signal(|| false);
    let gone: Vec<Listing> = if show_all_gone() {
        result.gone.clone()
    } else {
        result.gone.iter().take(8).cloned().collect()
    };
    let has_image = |l: &Listing| {
        weapons
            .iter()
            .any(|w| w.slug == l.weapon_slug && w.has_image)
    };
    rsx! {
        div { class: "section-head",
            h2 { "Price over time" }
            span { class: "muted small",
                "all listings matching the filters, per {bucket_label(bucket_hours)}"
                if !expand { " · invalid prices excluded" }
            }
        }
        PriceChart { points: result.history.clone(), bucket_hours }
        if let Some(slug) = weapon.filter(|_| !expand) {
            StatEffectsPanel { key: "{slug}", weapon: slug.clone() }
        }
        div { class: "section-head",
            h2 { "Current listings" }
            span { class: "muted small",
                "{result.total} found"
                if result.total > shown {
                    " · showing the first {shown}"
                }
            }
            if result.outliers_hidden > 0 {
                button {
                    class: "link-button",
                    title: "Prices far above the normal high end of the weapon. Click to list them.",
                    onclick: move |_| query.with_mut(|q| q.include_outliers = true),
                    "{result.outliers_hidden} with invalid prices hidden"
                    if let Some(limit) = result.price_limit {
                        " (over {format_plat(limit)}p)"
                    }
                }
            }
        }
        if result.listings.is_empty() {
            div { class: "empty-state", "Nothing matches. Try loosening the filters." }
        }
        div { class: "listings",
            for l in result.listings.iter() {
                ListingRow {
                    key: "{l.auction_id}",
                    listing: l.clone(),
                    now,
                    show_weapon,
                    has_image: has_image(l),
                    open_initially: expand,
                }
            }
        }

        div { class: "section-head",
            h2 { "Recently disappeared" }
            span { class: "muted small",
                "{result.gone_total} listings went away (sold or delisted) · warframe.market has no sold signal, so this is the closest proxy"
            }
        }
        if result.gone.is_empty() {
            div { class: "empty-state",
                "Nothing yet. Disappearances show up from the second scrape of a weapon on."
            }
        }
        div { class: "listings gone",
            for l in gone {
                ListingRow {
                    key: "gone-{l.auction_id}",
                    has_image: has_image(&l),
                    listing: l,
                    now,
                    show_weapon,
                    open_initially: expand,
                }
            }
        }
        if result.gone.len() > 8 {
            button {
                class: "show-all",
                onclick: move |_| show_all_gone.toggle(),
                if show_all_gone() { "Show fewer" } else { "Show {result.gone.len()} most recent" }
            }
        }
    }
}

fn bucket_label(hours: u32) -> String {
    match hours {
        24 => "day".into(),
        168 => "week".into(),
        h => format!("{h} hours"),
    }
}

#[component]
fn ListingRow(
    listing: Listing,
    now: i64,
    show_weapon: bool,
    has_image: bool,
    open_initially: bool,
) -> Element {
    let mut open = use_signal(|| open_initially);
    let l = &listing;
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
    let rank = l
        .rank
        .map(|r| format!("R{r}"))
        .unwrap_or_else(|| "R?".into());
    let rolls = l.re_rolls.unwrap_or(0);
    let mr = l.mastery_level.unwrap_or(0);
    let url = format!("https://warframe.market/auction/{}", l.auction_id);
    let age = format_age(now, l.created);

    rsx! {
        div { class: if open() { "listing open" } else { "listing" },
            div { class: "listing-main", onclick: move |_| open.toggle(),
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
                    div { class: "riven-name", "{l.weapon_name} {l.riven_name}" }
                    div { class: "muted small",
                        "{rank} · MR{mr} · {rolls} rolls · {l.polarity}"
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
                div { class: "attrs",
                    for a in l.attributes.iter() {
                        div {
                            key: "{a.slug}",
                            class: if a.positive { "attr positive" } else { "attr negative" },
                            span { class: "attr-value", "{format_value(&a.slug, a.value)}" }
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
            if open() {
                ListingDetail { listing: listing.clone(), url }
            }
        }
    }
}

#[component]
fn ListingDetail(listing: Listing, url: String) -> Element {
    let id = listing.riven_id.clone();
    let history = use_resource(move || {
        let id = id.clone();
        async move { api::riven_history(id).await }
    });
    let history_view = match &*history.read() {
        Some(Ok(auctions)) => {
            let rows = auctions.iter().map(|a| {
                let spark: Vec<(i64, i64)> = a
                    .points
                    .iter()
                    .filter_map(|p| p.price.map(|v| (p.t, v)))
                    .collect();
                let first = a.points.first().and_then(|p| p.price);
                let last = a.points.last().and_then(|p| p.price);
                let status = match (a.gone_at, &a.gone_reason) {
                    (Some(t), reason) => format!(
                        "gone {} UTC{}",
                        format_datetime(t),
                        reason
                            .as_ref()
                            .map(|r| format!(" ({r})"))
                            .unwrap_or_default()
                    ),
                    (None, _) => format!("last seen {} UTC", format_datetime(a.last_seen)),
                };
                let price = match (first, last) {
                    (Some(f), Some(l)) if f != l => format!("{f}p → {l}p"),
                    (_, Some(l)) => format!("{l}p"),
                    _ => "—".into(),
                };
                let current = a.auction_id == listing.auction_id;
                rsx! {
                    tr { key: "{a.auction_id}", class: if current { "current" } else { "" },
                        td { "{a.owner_name}" }
                        td { "{format_date(a.created)}" }
                        td { "{price}" }
                        td { Sparkline { points: spark } }
                        td { class: "muted", "{status}" }
                    }
                }
            });
            rsx! {
                div { class: "detail-history",
                    h4 {
                        if auctions.len() > 1 {
                            "This riven was listed {auctions.len()} times"
                        } else {
                            "Listing history"
                        }
                    }
                    table { class: "history-table",
                        thead {
                            tr {
                                th { "Seller" }
                                th { "Listed" }
                                th { "Price" }
                                th { "" }
                                th { "Status" }
                            }
                        }
                        tbody { {rows} }
                    }
                }
            }
        }
        Some(Err(e)) => rsx! { div { class: "error", "{e}" } },
        None => rsx! {},
    };

    rsx! {
        div { class: "detail",
            {history_view}
            table {
                thead {
                    tr {
                        th { "Stat" }
                        th { "Listed" }
                        th { "At rank 8" }
                        th { "Roll" }
                    }
                }
                tbody {
                    for a in listing.attributes.iter() {
                        tr { key: "{a.slug}",
                            td { class: if a.positive { "pos" } else { "neg" },
                                if a.positive { "+" } else { "−" }
                                " {attribute_name(&a.slug)}"
                            }
                            td { "{format_value(&a.slug, a.value)}" }
                            td {
                                {a.value_rank8.map(|v| format_value(&a.slug, v)).unwrap_or_else(|| "—".into())}
                            }
                            td {
                                {a.quality.map(|q| format!("{:.0}%", q * 100.0)).unwrap_or_else(|| "—".into())}
                            }
                        }
                    }
                }
            }
            div { class: "detail-id",
                span { class: "muted small", "Riven id" }
                RivenId { id: listing.riven_id.clone(), short: false }
                FindRivenButton { id: listing.riven_id.clone() }
            }
            div { class: "muted small",
                "first seen {format_datetime(listing.first_seen)} UTC · seller reputation {listing.owner_reputation.unwrap_or(0)} · "
                a { href: "{url}", target: "_blank", rel: "noopener", "open on warframe.market ↗" }
            }
        }
    }
}

/// A riven id with a copy button. Clicks don't bubble up to the listing row.
#[component]
fn RivenId(id: String, short: bool) -> Element {
    let mut copied = use_signal(|| false);
    let shown = if short {
        id.chars().take(8).collect()
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
                        sleep_ms(1500).await;
                        copied.set(false);
                    });
                },
                if copied() { "copied" } else { "copy" }
            }
        }
    }
}

/// Switches the search to this riven only.
#[component]
fn FindRivenButton(id: String) -> Element {
    let mut query = use_context::<Signal<Query>>();
    let active = query.read().riven_id.as_deref() == Some(id.as_str());
    rsx! {
        if !active {
            button {
                class: "link-button",
                onclick: move |_| {
                    let id = id.clone();
                    query.with_mut(|q| q.riven_id = Some(id));
                },
                "show only this riven"
            }
        }
    }
}
