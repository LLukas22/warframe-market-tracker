//! One riven's observations: separate price series and seller intervals per auction.
use crate::api::{self, RivenAuction};
use crate::chart::{INITIAL_WIDTH, RESIZE_THRESHOLD};
use crate::ui::{Route, format_age, format_datetime, format_day_month, format_plat, format_time};
use dioxus::prelude::*;

const COLORS: [&str; 6] = [
    "#b48cff", "#5fd39b", "#f0b85a", "#72baff", "#ff7a7a", "#e897db",
];

// Chart geometry, in CSS pixels: the plot spans `LEFT..width - RIGHT` and
// `TOP..TOP + PLOT_HEIGHT`, with the date labels below it.
const MIN_WIDTH: f64 = 280.0;
const HEIGHT: f64 = 240.0;
const LEFT: f64 = 58.0;
const RIGHT: f64 = 18.0;
const TOP: f64 = 18.0;
const PLOT_HEIGHT: f64 = 190.0;
/// Gap between the y axis labels and the plot.
const Y_LABEL_GAP: f64 = 8.0;
/// Shifts a label down by about half its height, to center it on its grid line.
const LABEL_BASELINE: f64 = 4.0;
const DATE_LABEL_Y: f64 = HEIGHT - 8.0;
const POINT_RADIUS: f64 = 3.0;
const FOCUSED_POINT_RADIUS: f64 = 6.0;
/// Invisible radius around each point that takes taps and hovers.
const HIT_RADIUS: f64 = 11.0;
/// Least room between the centers of two date labels; a price change closer than that to a
/// label already placed gets none.
const MIN_LABEL_GAP: f64 = 64.0;
/// Sellers shown in the ownership flow before "show all": the most recent ones.
const COLLAPSED_STAGES: usize = 5;

/// One seller's hold of the riven: their listings of it in a row, relists included.
#[derive(Debug, PartialEq)]
struct Stage<'a> {
    seller: &'a str,
    listings: Vec<&'a RivenAuction>,
    /// When their first listing went up, see [`listed_at`].
    since: i64,
    last_seen: i64,
    /// When they stopped holding it, see [`holds`]; `None` while it's listed.
    ended: Option<i64>,
    first_price: Option<i64>,
    last_price: Option<i64>,
}

/// The riven's sellers in order: consecutive listings by one seller make one stage, so a
/// relist doesn't look like a resale. `auctions` is in listing order, see [`in_listing_order`].
fn stages(auctions: &[RivenAuction]) -> Vec<Stage<'_>> {
    let held = holds(auctions);
    let price = |a: &RivenAuction, last: bool| {
        let mut prices = a.points.iter().filter_map(|p| p.price.filter(|v| *v > 0));
        if last {
            prices.next_back()
        } else {
            prices.next()
        }
    };
    let mut stages: Vec<Stage> = Vec::new();
    for (i, a) in auctions.iter().enumerate() {
        match stages.last_mut() {
            Some(stage) if stage.seller.eq_ignore_ascii_case(&a.owner_name) => {
                stage.listings.push(a);
                stage.last_seen = stage.last_seen.max(a.last_seen);
                stage.ended = held[i];
                stage.first_price = stage.first_price.or(price(a, false));
                stage.last_price = price(a, true).or(stage.last_price);
            }
            _ => stages.push(Stage {
                seller: &a.owner_name,
                listings: vec![a],
                since: listed_at(a),
                last_seen: a.last_seen,
                ended: held[i],
                first_price: price(a, false),
                last_price: price(a, true),
            }),
        }
    }
    stages
}

/// When a listing went up: when its seller made it, or when it was first seen if that's
/// earlier or unknown. Tracking began after many listings were made.
fn listed_at(a: &RivenAuction) -> i64 {
    if a.created > 0 {
        a.created.min(a.first_seen)
    } else {
        a.first_seen
    }
}

/// The riven's listings oldest first, by when they went up: the order of first sightings
/// says little when tracking found several at once.
fn in_listing_order(mut auctions: Vec<RivenAuction>) -> Vec<RivenAuction> {
    auctions.sort_by(|a, b| {
        listed_at(a)
            .cmp(&listed_at(b))
            .then_with(|| a.auction_id.cmp(&b.auction_id))
    });
    auctions
}

/// Until when each listing's seller held the riven, `None` for the newest listing while it's
/// up. One riven is in one seller's hands, so a listing is over once the riven is listed
/// anew, even if its seller left it up; else when it went. `auctions` is in listing order.
fn holds(auctions: &[RivenAuction]) -> Vec<Option<i64>> {
    auctions
        .iter()
        .enumerate()
        .map(|(i, a)| match auctions.get(i + 1) {
            Some(next) => Some(
                a.gone_at
                    .unwrap_or(i64::MAX)
                    .min(listed_at(next))
                    .max(listed_at(a)),
            ),
            None => a.gone_at,
        })
        .collect()
}

/// Until when each listing's line runs: until its seller stopped holding the riven, or now
/// while it's listed, and on to its last sighting when it was left up after that.
fn listing_ends(auctions: &[RivenAuction], now: i64) -> Vec<i64> {
    auctions
        .iter()
        .zip(holds(auctions))
        .map(|(a, held)| held.unwrap_or(now).max(a.last_seen))
        .collect()
}

fn bounds(auctions: &[RivenAuction], ends: &[i64]) -> Option<(i64, i64)> {
    Some((
        auctions.iter().map(|a| a.first_seen).min()?,
        ends.iter().copied().max()?,
    ))
}

/// Each seller's color, in the order they first listed the riven, for the chart and the cards.
fn seller_color(sellers: &[&str], name: &str) -> &'static str {
    let i = sellers
        .iter()
        .position(|s| s.eq_ignore_ascii_case(name))
        .unwrap_or(0);
    COLORS[i % COLORS.len()]
}

fn sellers(auctions: &[RivenAuction]) -> Vec<&str> {
    let mut sellers: Vec<&str> = Vec::new();
    for a in auctions {
        if !sellers
            .iter()
            .any(|s| s.eq_ignore_ascii_case(&a.owner_name))
        {
            sellers.push(&a.owner_name);
        }
    }
    sellers
}

/// The dashed lines from one seller's last price to the next seller's first, as (x1, y1, x2,
/// y2). A next listing that went up before the last one went joins it where it started.
fn handover_lines(
    auctions: &[RivenAuction],
    ends: &[i64],
    x: impl Fn(i64) -> f64,
    y: impl Fn(i64) -> f64,
) -> Vec<(f64, f64, f64, f64)> {
    let priced = |a: &RivenAuction| {
        let mut points = a
            .points
            .iter()
            .filter_map(|p| Some((p.t, p.price.filter(|v| *v > 0)?)));
        Some((points.next()?, points.next_back()))
    };
    auctions
        .windows(2)
        .zip(ends)
        .filter(|(pair, _)| !pair[0].owner_name.eq_ignore_ascii_case(&pair[1].owner_name))
        .filter_map(|(pair, &end)| {
            let ((_, first_price), last) = priced(&pair[0])?;
            let sold_at = last.map_or(first_price, |(_, p)| p);
            let ((listed, price), _) = priced(&pair[1])?;
            let left = end.min(listed);
            Some((x(left), y(sold_at), x(listed), y(price)))
        })
        .collect()
}

/// Keep discontinuities and single observations; never connect different auctions.
fn price_path(
    a: &RivenAuction,
    end: i64,
    x: impl Fn(i64) -> f64,
    y: impl Fn(i64) -> f64,
) -> String {
    let mut path = String::new();
    let mut previous = None;
    for p in &a.points {
        match p.price.filter(|v| *v > 0) {
            Some(price) => {
                if let Some(last) = previous {
                    path.push_str(&format!(
                        " L{:.1},{:.1} L{:.1},{:.1}",
                        x(p.t),
                        y(last),
                        x(p.t),
                        y(price)
                    ));
                } else {
                    path.push_str(&format!(" M{:.1},{:.1}", x(p.t), y(price)));
                }
                previous = Some(price);
            }
            None => previous = None,
        }
    }
    // The last price held until the listing went, which may be after its last point.
    if let (Some(price), Some(p)) = (previous, a.points.last())
        && end > p.t
    {
        path.push_str(&format!(" L{:.1},{:.1}", x(end), y(price)));
    }
    path
}

/// A stretch of one listing at one price: the index of its first point, its price, and until
/// when it held, which is the next price's first sighting, or its own last one before a gap
/// or the listing's end. The chart marks only these starts, so an unchanged price is a plain
/// line rather than a row of dots.
#[derive(Debug, PartialEq)]
struct PriceRun {
    start: usize,
    price: i64,
    until: i64,
}

fn price_runs(a: &RivenAuction, end: i64) -> Vec<PriceRun> {
    let mut runs: Vec<PriceRun> = Vec::new();
    let mut previous = None;
    for (i, p) in a.points.iter().enumerate() {
        let price = p.price.filter(|v| *v > 0);
        match (price, previous) {
            (Some(price), Some(last)) if price == last => {
                if let Some(run) = runs.last_mut() {
                    run.until = p.t;
                }
            }
            (Some(price), last) => {
                if let (Some(run), Some(_)) = (runs.last_mut(), last) {
                    run.until = p.t;
                }
                runs.push(PriceRun {
                    start: i,
                    price,
                    until: p.t,
                });
            }
            (None, _) => {}
        }
        previous = price;
    }
    if let (Some(run), Some(_)) = (runs.last_mut(), previous) {
        run.until = run.until.max(end);
    }
    runs
}

/// Where the date axis is labelled: the first and last sighting, and each price change with
/// room for it, as (time, x, anchor). Days, or times of day when it all fits in one day.
fn axis_labels(
    first: i64,
    last: i64,
    changes: impl IntoIterator<Item = i64>,
    x: impl Fn(i64) -> f64,
) -> Vec<(String, f64, &'static str)> {
    let label = |t: i64| {
        if last - first < api::DAY_MS {
            format_time(t)
        } else {
            format_day_month(t)
        }
    };
    if first == last {
        return vec![(label(first), x(first), "middle")];
    }
    let mut placed = vec![x(first), x(last)];
    let mut labels = vec![
        (label(first), x(first), "start"),
        (label(last), x(last), "end"),
    ];
    let mut changes: Vec<i64> = changes.into_iter().collect();
    changes.sort_unstable();
    for t in changes {
        let at = x(t);
        // The edge labels hang inward, so they need the gap from their far end too.
        let clear = placed.iter().all(|&p| (p - at).abs() >= MIN_LABEL_GAP)
            && at - x(first) >= MIN_LABEL_GAP * 1.5
            && x(last) - at >= MIN_LABEL_GAP * 1.5;
        if clear {
            placed.push(at);
            labels.push((label(t), at, "middle"));
        }
    }
    labels
}

#[component]
pub fn RivenJourney(id: String, selected_auction: String, now: i64) -> Element {
    let version = use_context::<Signal<u64>>();
    let history_id = id.clone();
    let mut history = use_resource(move || {
        version.read();
        let id = history_id.clone();
        async move { api::riven_history(id).await }
    });
    rsx! {
        match &*history.read() {
            None => rsx! { p { class: "muted", role: "status", "Loading history…" } },
            Some(Err(error)) => rsx! {
                p { class: "error", "Could not load history: {error}" }
                button { onclick: move |_| history.restart(), "Retry history" }
            },
            Some(Ok(auctions)) => rsx! {
                JourneyChart { id: id.clone(), auctions: in_listing_order(auctions.clone()), selected_auction, now }
            },
        }
    }
}

#[component]
fn JourneyChart(
    id: String,
    auctions: Vec<RivenAuction>,
    selected_auction: String,
    now: i64,
) -> Element {
    let mut width = use_signal(|| INITIAL_WIDTH);
    let mut focus = use_signal(|| None::<(usize, usize)>);
    let ends = listing_ends(&auctions, now);
    let held = holds(&auctions);
    let left_up = held
        .iter()
        .zip(&ends)
        .any(|(h, &end)| h.is_some_and(|t| t < end));
    let Some((first, last)) = bounds(&auctions, &ends) else {
        return rsx! { p { class: "empty-state", "No observations recorded yet." } };
    };
    let sellers = sellers(&auctions);
    let color = |a: &RivenAuction| seller_color(&sellers, &a.owner_name);
    let w = width().max(MIN_WIDTH);
    let span = (last - first).max(1) as f64;
    let (plot_left, plot_right) = (LEFT, w - RIGHT);
    let x = |t| {
        if first == last {
            (plot_left + plot_right) / 2.0
        } else {
            plot_left + (t - first) as f64 / span * (plot_right - plot_left)
        }
    };
    let max_price = auctions
        .iter()
        .flat_map(|a| &a.points)
        .filter_map(|p| p.price)
        .max()
        .unwrap_or(0);
    let (max, step) = crate::chart::nice_axis(max_price as f64);
    let y = |p| TOP + (1.0 - p as f64 / max) * PLOT_HEIGHT;
    let ticks: Vec<_> = (0..)
        .map(|i| i as f64 * step)
        .take_while(|v| *v <= max)
        .collect();
    let focused = focus().and_then(|(a, r)| {
        let auction = auctions.get(a)?;
        let run = price_runs(auction, ends[a]).into_iter().nth(r)?;
        Some((auction, auction.points[run.start].t, run))
    });
    let labels = axis_labels(
        first,
        last,
        auctions.iter().zip(&ends).flat_map(|(a, &end)| {
            price_runs(a, end)
                .into_iter()
                .map(|run| a.points[run.start].t)
        }),
        x,
    );
    rsx! {
        section { class: "journey",
            div { class: "section-head",
                h2 { "Price history" }
                span { class: "muted small",
                    if auctions.len() == 1 { "1 listing" } else { "{auctions.len()} listings, one line each" }
                    if sellers.len() > 1 { " · {sellers.len()} sellers" }
                }
            }
            div { class: "chart journey-chart",
                onresize: move |event| {
                    if let Ok(size) = event.get_content_box_size()
                        && size.width >= MIN_WIDTH
                        && (size.width - width()).abs() > RESIZE_THRESHOLD
                    {
                        width.set(size.width);
                    }
                },
                if max_price > 0 {
                    svg { view_box: "0 0 {w} {HEIGHT}", role: "img", "aria-label": "Observed price history, with a separate series for each listing",
                        for v in ticks {
                            line { class: "grid", x1: "{plot_left}", x2: "{plot_right}", y1: "{y(v as i64)}", y2: "{y(v as i64)}" }
                            text { class: "axis", x: "{plot_left - Y_LABEL_GAP}", y: "{y(v as i64) + LABEL_BASELINE}", text_anchor: "end", "{format_plat(v as i64)}" }
                        }
                        for (x1, y1, x2, y2) in handover_lines(&auctions, &ends, x, y) {
                            line { class: "journey-handover", x1: "{x1}", y1: "{y1}", x2: "{x2}", y2: "{y2}" }
                        }
                        // A listing left up after the riven was listed anew is drawn faded from
                        // then on: the seller most likely no longer has it.
                        defs {
                            for (ai, until) in held.iter().enumerate() {
                                if let Some(until) = until.filter(|&t| t < ends[ai]) {
                                    clipPath { id: "journey-held-{ai}",
                                        rect { x: "0", y: "0", width: "{x(until).max(0.0)}", height: "{HEIGHT}" }
                                    }
                                    clipPath { id: "journey-left-{ai}",
                                        rect { x: "{x(until).max(0.0)}", y: "0", width: "{w}", height: "{HEIGHT}" }
                                    }
                                }
                            }
                        }
                        for (ai, a) in auctions.iter().enumerate() {
                            if held[ai].is_some_and(|t| t < ends[ai]) {
                                path { class: "journey-left-up", d: "{price_path(a, ends[ai], x, y)}", fill: "none", stroke: "{color(a)}", stroke_width: "2", vector_effect: "non-scaling-stroke", clip_path: "url(#journey-left-{ai})" }
                                path { d: "{price_path(a, ends[ai], x, y)}", fill: "none", stroke: "{color(a)}", stroke_width: "2", vector_effect: "non-scaling-stroke", clip_path: "url(#journey-held-{ai})" }
                            } else {
                                path { d: "{price_path(a, ends[ai], x, y)}", fill: "none", stroke: "{color(a)}", stroke_width: "2", vector_effect: "non-scaling-stroke" }
                            }
                            for (ri, run) in price_runs(a, ends[ai]).into_iter().enumerate() {
                                {
                                    let t = a.points[run.start].t;
                                    rsx! {
                                        circle { cx: "{x(t)}", cy: "{y(run.price)}", r: if focus() == Some((ai, ri)) { FOCUSED_POINT_RADIUS } else { POINT_RADIUS }, fill: "{color(a)}" }
                                        circle { class: "journey-hit", cx: "{x(t)}", cy: "{y(run.price)}", r: HIT_RADIUS, fill: "transparent",
                                            tabindex: "0", role: "button", "aria-label": "{run.price} platinum from {format_datetime(t)} UTC, seller {a.owner_name}",
                                            onmouseenter: move |_| focus.set(Some((ai, ri))),
                                            onfocus: move |_| focus.set(Some((ai, ri))),
                                            onclick: move |_| focus.set(Some((ai, ri))),
                                        }
                                    }
                                }
                            }
                        }
                        for (text, at, anchor) in labels {
                            line { class: "axis-tick", x1: "{at}", x2: "{at}", y1: "{TOP + PLOT_HEIGHT}", y2: "{TOP + PLOT_HEIGHT + 4.0}" }
                            text { class: "axis", x: "{at}", y: "{DATE_LABEL_Y}", text_anchor: "{anchor}", "{text}" }
                        }
                    }
                } else { p { class: "empty-state", "No usable price observations yet." } }
                div { class: "journey-readout", aria_live: "polite",
                    if let Some((a, from, run)) = focused {
                        strong { "{format_plat(run.price)}p" }
                        span {
                            "{a.owner_name} · {format_datetime(from)}"
                            if run.until > from { " → {format_datetime(run.until)}" }
                            " UTC"
                        }
                    } else {
                        span { class: "muted",
                            "Dots mark a new price. Hover or tap one for details."
                            if left_up { " Dotted: a listing left up after the riven was listed again." }
                        }
                    }
                }
            }
            OwnershipFlow { id: id.clone(), auctions: auctions.clone(), selected_auction, now }
        }
    }
}

/// Who held the riven, oldest first: one card per seller, an arrow for each handover.
#[component]
fn OwnershipFlow(
    id: String,
    auctions: Vec<RivenAuction>,
    selected_auction: String,
    now: i64,
) -> Element {
    let mut show_all = use_signal(|| false);
    let stages = stages(&auctions);
    let colors = sellers(&auctions);
    let hidden = if show_all() {
        0
    } else {
        stages.len().saturating_sub(COLLAPSED_STAGES)
    };
    let sellers = colors.len();
    rsx! {
        div { class: "section-head",
            h2 { if sellers == 1 { "Seller" } else { "Sellers" } }
        }
        if sellers > 1 {
            p { class: "muted small",
                "Oldest first. A new seller most likely bought it, though warframe.market doesn't confirm trades."
            }
        }
        if hidden > 0 {
            button { class: "link-button ownership-more", onclick: move |_| show_all.set(true),
                "Show {hidden} earlier "
                if hidden == 1 { "seller" } else { "sellers" }
            }
        }
        ol { class: "ownership-flow",
            for (i, stage) in stages.iter().enumerate().skip(hidden) {
                li { key: "{i}", class: "ownership-step",
                    if i > 0 {
                        Handover {
                            // Its last sighting: a removal is often found out later.
                            left_at: stages[i - 1].last_seen,
                            listed_at: stage.since,
                            sold_at: stages[i - 1].last_price,
                            relisted_at: stage.first_price,
                        }
                    }
                    StageCard {
                        id: id.clone(),
                        seller: stage.seller.to_owned(),
                        color: seller_color(&colors, stage.seller),
                        auction: stage.listings.last().map(|a| a.auction_id.clone()).unwrap_or_default(),
                        selected: stage.listings.iter().any(|a| a.auction_id == selected_auction),
                        first_seen: stage.since,
                        ended: stage.ended,
                        now,
                        price: stage.last_price,
                    }
                }
            }
        }
    }
}

/// The arrow between two sellers: how much more (or less) the next one asked, and how long
/// after the first one's last sighting they listed it.
#[component]
fn Handover(
    left_at: i64,
    listed_at: i64,
    sold_at: Option<i64>,
    relisted_at: Option<i64>,
) -> Element {
    let markup = match (sold_at, relisted_at) {
        (Some(a), Some(b)) if b != a => {
            let sign = if b > a { "+" } else { "−" };
            Some((format!("{sign}{}p", format_plat((b - a).abs())), b > a))
        }
        _ => None,
    };
    let gap = if listed_at > left_at {
        format!("{} later", format_age(listed_at, left_at))
    } else {
        "while still listed".to_owned()
    };
    rsx! {
        div { class: "stage-handover", aria_label: "then, {gap}",
            if let Some((change, up)) = markup {
                span { class: if up { "stage-markup up" } else { "stage-markup" },
                    title: "The next seller's first asking price, against the last one before",
                    "{change}"
                }
            }
            span { class: "stage-arrow", aria_hidden: "true", "→" }
            span { class: "muted small", "{gap}" }
        }
    }
}

/// When a stage ran, as days: `3 Nov – 12 Nov`, `3 Nov – now` while still listed, or one day
/// with its times. The length follows, `9d`.
fn stage_range(first_seen: i64, ended: Option<i64>, now: i64) -> (String, String) {
    let end = ended.unwrap_or(now).max(first_seen);
    let range = if ended.is_none() {
        format!("{} – now", format_day_month(first_seen))
    } else if first_seen.div_euclid(api::DAY_MS) == end.div_euclid(api::DAY_MS) {
        format!(
            "{}, {}–{}",
            format_day_month(first_seen),
            format_time(first_seen),
            format_time(end)
        )
    } else {
        format!(
            "{} – {}",
            format_day_month(first_seen),
            format_day_month(end)
        )
    };
    (range, format_age(end, first_seen))
}

/// One seller in the flow: who, their asking price, and when. The price opens their listing.
#[component]
fn StageCard(
    id: String,
    seller: String,
    /// Their line's color in the price chart.
    color: String,
    /// The stage's newest listing, which the card opens.
    auction: String,
    selected: bool,
    first_seen: i64,
    ended: Option<i64>,
    now: i64,
    price: Option<i64>,
) -> Element {
    let price = price
        .map(|p| format!("{}p", format_plat(p)))
        .unwrap_or_else(|| "—".into());
    let (range, length) = stage_range(first_seen, ended, now);
    let navigator = use_navigator();
    let to = Route::RivenPage { id, auction };
    let to2 = to.clone();
    let title = format!(
        "{} → {}",
        format_datetime(first_seen),
        ended.map_or_else(|| "still listed".to_owned(), format_datetime)
    );
    rsx! {
        // The whole card opens the listing, from the keyboard too. The seller's name and icon
        // keep their own links.
        div {
            class: if selected { "stage-card selected" } else { "stage-card openable" },
            role: if !selected { "link" },
            tabindex: if !selected { "0" },
            aria_label: if !selected { "Open {seller}'s listing" },
            onclick: move |_| {
                if !selected {
                    navigator.push(to.clone());
                }
            },
            onkeydown: move |e: KeyboardEvent| {
                if !selected && e.key() == Key::Enter {
                    navigator.push(to2.clone());
                }
            },
            div { class: "stage-seller",
                span { class: "stage-swatch", style: "background: {color}", aria_hidden: "true" }
                crate::people::SellerName { name: seller.clone() }
            }
            strong { class: "stage-price", "{price}" }
            div { class: "muted small", title: "{title}",
                "{range}"
                " · {length}"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::PricePoint;

    fn auction(first: i64, last: i64, prices: &[(i64, Option<i64>)]) -> RivenAuction {
        RivenAuction {
            auction_id: "test".into(),
            owner_name: "seller".into(),
            created: 0,
            first_seen: first,
            last_seen: last,
            gone_at: None,
            gone_reason: None,
            points: prices
                .iter()
                .map(|&(t, price)| PricePoint { t, price })
                .collect(),
        }
    }

    fn listed(
        owner: &str,
        first: i64,
        last: i64,
        gone: Option<i64>,
        prices: &[i64],
    ) -> RivenAuction {
        RivenAuction {
            owner_name: owner.into(),
            gone_at: gone,
            ..auction(
                first,
                last,
                &prices
                    .iter()
                    .enumerate()
                    .map(|(i, &p)| (first + i as i64, Some(p)))
                    .collect::<Vec<_>>(),
            )
        }
    }

    #[test]
    fn relists_by_one_seller_make_one_stage_and_a_new_seller_a_new_one() {
        let auctions = [
            listed("Alice", 10, 20, Some(25), &[100, 90]),
            listed("Bob", 40, 50, Some(55), &[150]),
            listed("bob", 60, 70, None, &[140, 130]),
            listed("Alice", 80, 90, None, &[200]),
        ];
        let stages = stages(&auctions);
        let summary: Vec<_> = stages
            .iter()
            .map(|s| {
                (
                    s.seller,
                    s.listings.len(),
                    s.since,
                    s.last_seen,
                    s.ended,
                    s.first_price,
                    s.last_price,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("Alice", 1, 10, 20, Some(25), Some(100), Some(90)),
                // Names ignore case, as on warframe.market. Their relist was never seen going,
                // but it had gone by the time Alice listed the riven again.
                ("Bob", 2, 40, 70, Some(80), Some(150), Some(130)),
                // Bought back: a stage of its own.
                ("Alice", 1, 80, 90, None, Some(200), Some(200)),
            ]
        );
        assert!(super::stages(&[]).is_empty());
    }

    #[test]
    fn price_runs_mark_only_new_prices() {
        let a = auction(
            0,
            60,
            &[
                (0, Some(30)),
                (10, Some(30)),
                (20, Some(30)),
                (30, Some(10)),
                (40, Some(10)),
                (50, None),
                (60, Some(10)),
            ],
        );
        assert_eq!(
            price_runs(&a, 60),
            vec![
                PriceRun {
                    start: 0,
                    price: 30,
                    until: 30
                },
                PriceRun {
                    start: 3,
                    price: 10,
                    until: 40
                },
                PriceRun {
                    start: 6,
                    price: 10,
                    until: 60
                },
            ]
        );
        // A price unchanged until a later sighting holds until then.
        let later = auction(0, 90, &[(0, Some(30)), (10, Some(30))]);
        assert_eq!(price_runs(&later, 90)[0].until, 90);
        assert!(price_path(&later, 90, |t| t as f64, |p| p as f64).ends_with("L90.0,30.0"));
    }

    #[test]
    fn axis_labels_mark_price_changes_with_room() {
        let day = api::DAY_MS;
        // 2026-11-03 00:00 UTC, over 20 days on a 1000 px axis: 50 px a day.
        let first = 1_793_664_000_000;
        let x = |t: i64| (t - first) as f64 / day as f64 * 50.0;
        let labels = axis_labels(
            first,
            first + 20 * day,
            [first, first + 9 * day, first + 10 * day, first + 19 * day],
            x,
        );
        let texts: Vec<_> = labels.iter().map(|(t, _, a)| (t.as_str(), *a)).collect();
        // The first change is the first sighting, the third too close to the second and the
        // last too close to the end.
        assert_eq!(
            texts,
            [("3 Nov", "start"), ("23 Nov", "end"), ("12 Nov", "middle")]
        );
        // Within one day, times of day.
        let labels = axis_labels(first, first + 3 * api::HOUR_MS, [], x);
        assert_eq!(labels[1].0, "03:00");
    }

    #[test]
    fn handover_lines_join_one_sellers_last_price_to_the_next_ones_first() {
        let named = |name: &str, first, last, gone, prices: &[(i64, Option<i64>)]| {
            let mut a = auction(first, last, prices);
            a.owner_name = name.into();
            a.gone_at = gone;
            a
        };
        let id = |v: i64| v as f64;
        let auctions = [
            named("a", 0, 10, Some(20), &[(0, Some(700)), (10, Some(650))]),
            // A relist by the same seller, in another case: no handover.
            named("A", 30, 30, Some(40), &[(30, Some(650))]),
            named("b", 50, 50, None, &[(50, Some(700))]),
            // Listed before b's listing went: joins it where it started.
            named("c", 60, 60, None, &[(60, Some(660))]),
        ];
        assert_eq!(
            handover_lines(&auctions, &listing_ends(&auctions, 100), id, id),
            [(40.0, 650.0, 50.0, 700.0), (60.0, 700.0, 60.0, 660.0)]
        );
    }

    #[test]
    fn stage_ranges_read_as_days() {
        let day = api::DAY_MS;
        // 2026-11-03 19:01 UTC.
        let start = 1_793_732_460_000;
        assert_eq!(format_day_month(start), "3 Nov");
        assert_eq!(
            stage_range(start, Some(start + 9 * day), start + 20 * day),
            ("3 Nov – 12 Nov".to_owned(), "9d".to_owned())
        );
        // Still listed: until now, however long ago its last sighting.
        assert_eq!(
            stage_range(start, None, start + 2 * day),
            ("3 Nov – now".to_owned(), "2d".to_owned())
        );
        assert_eq!(
            stage_range(start, Some(start + 2 * api::HOUR_MS), start + day).0,
            "3 Nov, 19:01–21:01"
        );
    }

    #[test]
    fn sighting_bounds_cover_all_listings_not_just_the_selected_one() {
        let bounds = |auctions: &[RivenAuction]| bounds(auctions, &listing_ends(auctions, 100));
        assert_eq!(bounds(&[]), None);
        let mut gone = auction(20, 80, &[]);
        gone.gone_at = Some(90);
        // The newest listing went: until then, not until now.
        assert_eq!(
            bounds(&[auction(10, 15, &[]), gone.clone()]),
            Some((10, 90))
        );
        assert_eq!(bounds(&[gone, auction(85, 85, &[])]), Some((20, 100)));
    }

    #[test]
    fn a_listing_is_held_until_the_riven_is_listed_anew() {
        let mut gone = auction(5, 10, &[]);
        gone.gone_at = Some(30);
        let auctions = [
            // Never seen going, but the riven was listed again: over by then. Its line still
            // runs to its last sighting.
            auction(0, 10, &[]),
            // Found gone after the next listing appeared: over when it did.
            gone,
            auction(20, 25, &[]),
            // The newest, still listed: until now.
            auction(40, 50, &[]),
        ];
        assert_eq!(holds(&auctions), [Some(5), Some(20), Some(40), None]);
        assert_eq!(listing_ends(&auctions, 100), [10, 20, 40, 100]);
        // Seen long after the riven was listed anew: a listing the old seller left up. Its
        // seller held the riven until then; the line runs on, faded, to its last sighting.
        let left_up = [auction(0, 60, &[]), auction(20, 25, &[])];
        assert_eq!(holds(&left_up), [Some(20), None]);
        assert_eq!(listing_ends(&left_up, 100), [60, 100]);
    }

    #[test]
    fn listings_are_ordered_by_when_they_went_up_not_first_seen() {
        // Tracking found all three at once; the sellers made them weeks apart.
        let made = |id: &str, created: i64| RivenAuction {
            auction_id: id.into(),
            created,
            ..auction(100, 200, &[])
        };
        let ordered = in_listing_order(vec![made("c", 30), made("a", 10), made("b", 20)]);
        assert_eq!(
            ordered
                .iter()
                .map(|a| a.auction_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert_eq!(listed_at(&ordered[0]), 10);
        // An unknown creation time falls back to the first sighting.
        assert_eq!(listed_at(&made("d", 0)), 100);
        // The older ones were over once the newest went up, though seen till the end.
        assert_eq!(holds(&ordered), [Some(20), Some(30), None]);
    }

    #[test]
    fn price_series_step_at_observations_and_break_at_unknown_prices() {
        let a = auction(
            1,
            5,
            &[
                (1, Some(100)),
                (2, Some(80)),
                (3, None),
                (4, Some(60)),
                (5, Some(60)),
            ],
        );
        let path = price_path(&a, 5, |t| t as f64, |p| p as f64);
        assert_eq!(
            path,
            " M1.0,100.0 L2.0,100.0 L2.0,80.0 M4.0,60.0 L5.0,60.0 L5.0,60.0"
        );
        let single = auction(1, 1, &[(1, Some(100))]);
        assert_eq!(
            price_path(&single, 1, |t| t as f64, |p| p as f64),
            " M1.0,100.0"
        );
        let empty = auction(1, 2, &[(1, Some(0)), (2, None)]);
        assert!(price_path(&empty, 2, |t| t as f64, |p| p as f64).is_empty());
    }
}
