//! One riven's observations: separate price series and seller intervals per auction.
use crate::api::{self, RivenAuction};
use crate::chart::{INITIAL_WIDTH, RESIZE_THRESHOLD};
use crate::ui::{Route, format_date, format_datetime, format_plat};
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
/// Listings shown in the sightings list before "show all".
const COLLAPSED_SIGHTINGS: usize = 4;

fn bounds(auctions: &[RivenAuction]) -> Option<(i64, i64)> {
    Some((
        auctions.iter().map(|a| a.first_seen).min()?,
        auctions.iter().map(|a| a.last_seen).max()?,
    ))
}

/// Keep discontinuities and single observations; never connect different auctions.
fn price_path(a: &RivenAuction, x: impl Fn(i64) -> f64, y: impl Fn(i64) -> f64) -> String {
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
    path
}

#[component]
pub fn RivenJourney(id: String, selected_auction: String) -> Element {
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
            Some(Ok(auctions)) => rsx! { JourneyChart { id: id.clone(), auctions: auctions.clone(), selected_auction } },
        }
    }
}

#[component]
fn JourneyChart(id: String, auctions: Vec<RivenAuction>, selected_auction: String) -> Element {
    let mut width = use_signal(|| INITIAL_WIDTH);
    let mut focus = use_signal(|| None::<(usize, usize)>);
    let mut show_all = use_signal(|| false);
    let Some((first, last)) = bounds(&auctions) else {
        return rsx! { p { class: "empty-state", "No observations recorded yet." } };
    };
    let mut sellers: Vec<&str> = Vec::new();
    for a in &auctions {
        if !sellers.contains(&a.owner_name.as_str()) {
            sellers.push(&a.owner_name);
        }
    }
    let color = |a: &RivenAuction| {
        COLORS[sellers.iter().position(|s| *s == a.owner_name).unwrap_or(0) % COLORS.len()]
    };
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
    let focused = focus().and_then(|(a, p)| {
        auctions
            .get(a)
            .and_then(|auction| auction.points.get(p).map(|point| (auction, point)))
    });
    let displayed = if show_all() {
        auctions.len()
    } else {
        auctions.len().min(COLLAPSED_SIGHTINGS)
    };
    rsx! {
        section { class: "journey",
            div { class: "journey-summary",
                div { span { "First seen" } strong { "{format_date(first)}" } small { "{format_datetime(first)} UTC" } }
                div { span { "Last seen" } strong { "{format_date(last)}" } small { "{format_datetime(last)} UTC" } }
                div { span { "Listing history" } strong { "{auctions.len()} listings" } small { "{sellers.len()} distinct sellers observed" } }
            }
            div { class: "section-head", h2 { "Price history" } span { class: "muted small", "one line per listing" } }
            div { class: "chart journey-chart",
                onresize: move |event| { if let Ok(size) = event.get_content_box_size() {
                    if size.width >= MIN_WIDTH && (size.width - width()).abs() > RESIZE_THRESHOLD { width.set(size.width); }
                } },
                if max_price > 0 {
                    svg { view_box: "0 0 {w} {HEIGHT}", role: "img", "aria-label": "Observed price history, with a separate series for each listing",
                        for v in ticks {
                            line { class: "grid", x1: "{plot_left}", x2: "{plot_right}", y1: "{y(v as i64)}", y2: "{y(v as i64)}" }
                            text { class: "axis", x: "{plot_left - Y_LABEL_GAP}", y: "{y(v as i64) + LABEL_BASELINE}", text_anchor: "end", "{format_plat(v as i64)}" }
                        }
                        for (ai, a) in auctions.iter().enumerate() {
                            path { d: "{price_path(a, x, y)}", fill: "none", stroke: "{color(a)}", stroke_width: "2", vector_effect: "non-scaling-stroke" }
                            for (pi, p) in a.points.iter().enumerate().filter(|(_, p)| p.price.is_some_and(|p| p > 0)) {
                                circle { cx: "{x(p.t)}", cy: "{y(p.price.unwrap())}", r: if focus() == Some((ai, pi)) { FOCUSED_POINT_RADIUS } else { POINT_RADIUS }, fill: "{color(a)}" }
                                circle { class: "journey-hit", cx: "{x(p.t)}", cy: "{y(p.price.unwrap())}", r: HIT_RADIUS, fill: "transparent",
                                    tabindex: "0", role: "button", "aria-label": "{format_datetime(p.t)} UTC, {p.price.unwrap()} platinum, seller {a.owner_name}",
                                    onmouseenter: move |_| focus.set(Some((ai, pi))),
                                    onfocus: move |_| focus.set(Some((ai, pi))),
                                    onclick: move |_| focus.set(Some((ai, pi))),
                                    title { "{format_datetime(p.t)} UTC · {p.price.unwrap()}p · {a.owner_name}" }
                                }
                            }
                        }
                        text { class: "axis", x: "{plot_left}", y: "{DATE_LABEL_Y}", "{format_date(first)}" }
                        if last != first { text { class: "axis", x: "{plot_right}", y: "{DATE_LABEL_Y}", text_anchor: "end", "{format_date(last)}" } }
                    }
                } else { p { class: "empty-state", "No usable price observations yet." } }
                div { class: "journey-readout", aria_live: "polite",
                    if let Some((a, p)) = focused {
                        strong { "{p.price.map(format_plat).unwrap_or_default()}p" }
                        span { "{a.owner_name} · {format_datetime(p.t)} UTC" }
                    } else { span { class: "muted", "Hover or tap a point for details." } }
                }
            }
            div { class: "section-head", h2 { "Sellers & sightings" } }
            p { class: "muted small", "Each bar spans one listing, first to last sighting. A new seller may mean a resale, but that can't be confirmed." }
            div { class: "sighting-axis", span { "{format_date(first)}" } span { "{format_date(last)}" } }
            ol { class: "sighting-list",
                for (i, a) in auctions.iter().enumerate().rev().take(displayed) {
                    li { class: if a.auction_id == selected_auction { "sighting selected" } else { "sighting" },
                        div { class: "sighting-heading",
                            strong { style: "color: {color(a)}", "{a.owner_name}" }
                            span { class: "muted small",
                                if i == 0 { "First listing observed" }
                                else if auctions[..i].iter().any(|old| old.owner_name == a.owner_name) { "Seller seen before · another listing" }
                                else { "Different seller first observed" }
                            }
                            Link { to: Route::RivenPage { id: id.clone(), auction: a.auction_id.clone() },
                                if a.auction_id == selected_auction { "Selected listing" } else { "View listing" }
                            }
                        }
                        div { class: "sighting-track",
                            span { style: format!("left: {}%; width: {}%; background: {}", if first == last { 50.0 } else { (a.first_seen - first) as f64 / span * 100.0 }, (a.last_seen - a.first_seen) as f64 / span * 100.0, color(a)) }
                        }
                        div { class: "sighting-dates",
                            span { "{format_datetime(a.first_seen)} → {format_datetime(a.last_seen)} UTC" }
                            span { if let Some(t) = a.gone_at { "Removed / expired {format_date(t)}" } else { "Last observed; availability unconfirmed" } }
                        }
                    }
                }
            }
            if auctions.len() > COLLAPSED_SIGHTINGS { button { class: "link-button", onclick: move |_| show_all.toggle(),
                if show_all() { "Show recent listings" } else { "Show all {auctions.len()} listings" }
            } }
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

    #[test]
    fn sighting_bounds_cover_all_listings_not_just_the_selected_one() {
        assert_eq!(bounds(&[]), None);
        assert_eq!(
            bounds(&[auction(20, 80, &[]), auction(10, 30, &[])]),
            Some((10, 80))
        );
        assert_eq!(bounds(&[auction(10, 10, &[])]), Some((10, 10)));
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
        let path = price_path(&a, |t| t as f64, |p| p as f64);
        assert_eq!(
            path,
            " M1.0,100.0 L2.0,100.0 L2.0,80.0 M4.0,60.0 L5.0,60.0 L5.0,60.0"
        );
        let single = auction(1, 1, &[(1, Some(100))]);
        assert_eq!(
            price_path(&single, |t| t as f64, |p| p as f64),
            " M1.0,100.0"
        );
        let empty = auction(1, 2, &[(1, Some(0)), (2, None)]);
        assert!(price_path(&empty, |t| t as f64, |p| p as f64).is_empty());
    }
}
