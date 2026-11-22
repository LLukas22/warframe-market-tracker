//! The top of a riven's page: what it is, what it costs, and how well each stat rolled.

use dioxus::prelude::*;

use crate::api::{
    self, AttrView, Availability, Listing, Overview, attribute_name, format_value, roll_range,
};
use crate::ui::{RivenId, format_age, format_plat};
use crate::weapons::WeaponImage;

/// `now` is `None` for a scanned riven, which has no price, seller or sightings.
#[component]
pub fn RivenSummary(listing: Listing, now: Option<i64>) -> Element {
    let loaded = use_context::<Signal<Option<Overview>>>();
    let l = &listing;
    let has_image = loaded()
        .and_then(|o| {
            o.weapons
                .iter()
                .find(|w| w.slug == l.weapon_slug)
                .map(|w| w.has_image)
        })
        .unwrap_or(false);
    let mr = l
        .mastery_level
        .map(|m| format!("MR{m}"))
        .unwrap_or_else(|| "MR?".into());
    let kind = if l.direct_sell {
        "buyout"
    } else if l.top_bid.is_some() {
        "top bid"
    } else {
        "starting bid"
    };
    let invalid_limit = l.price_limit.map(format_plat).unwrap_or_default();
    let url = format!("https://warframe.market/auction/{}", l.auction_id);
    let price = l.price.map(format_plat).unwrap_or_else(|| "—".into());

    rsx! {
        section { class: "riven-summary",
            div { class: "riven-head",
                WeaponImage { slug: l.weapon_slug.clone(), has_image, name: l.weapon_name.clone(), size: "md" }
                div { class: "riven-title",
                    h2 { "{l.weapon_name} {l.riven_name}" }
                    div { class: "riven-meta muted small",
                        Polarity { polarity: l.polarity.clone() }
                        span { "{mr}" }
                        Rerolls { count: l.re_rolls.unwrap_or(0) }
                    }
                    div { class: "riven-tags",
                        if now.is_none() {
                            span { class: "tag info", "from a screenshot" }
                        }
                        if let (Some(now), Some(t)) = (now, l.gone_at) {
                            span { class: "tag gone", title: "{l.gone_reason.clone().unwrap_or_default()}",
                                "gone {format_age(now, t)} ago"
                            }
                        } else if let Some(now) = now {
                            span { class: "tag info",
                                title: "When this price was last observed. Listings outside the fetched pages stay unconfirmed until checked or expired.",
                                if l.unconfirmed { "unconfirmed · " }
                                "seen {format_age(now, l.last_seen)} ago"
                            }
                        }
                        if l.copies > 1 {
                            span { class: "tag info", title: "Same riven seen in {l.copies} auctions",
                                if l.sellers > 1 { "{l.sellers} sellers" } else { "listed {l.copies}×" }
                            }
                        }
                        if l.legacy_stats == Some(true) {
                            span { class: "tag warn", "pre-rework" }
                        }
                        if !l.fit_ok {
                            span { class: "tag bad", title: "These values don't fit the riven formula, so rolls can't be worked out.", "unverified" }
                        }
                        if l.price_outlier {
                            span { class: "tag bad",
                                title: "Far above this weapon's normal high prices (limit {invalid_limit}p). Not counted in charts or medians.",
                                "invalid price"
                            }
                        }
                    }
                }
                if let Some(now) = now {
                    div { class: "riven-offer",
                        div { class: "offer-main",
                            div {
                                span { class: "plat", "{price}p" }
                                span { class: "muted small", " {kind}" }
                            }
                            div { class: "muted small", "listed {format_age(now, l.created)} ago" }
                            div { class: "offer-links small",
                                if l.gone_at.is_none() {
                                    AvailabilityCheck { key: "{l.auction_id}", auction_id: l.auction_id.clone() }
                                }
                                a { href: "{url}", target: "_blank", rel: "noopener", "warframe.market ↗" }
                            }
                        }
                        div { class: "offer-seller",
                            span { class: "offer-label", "Seller" }
                            div { class: "offer-seller-name",
                                crate::people::SellerName { name: l.owner_name.clone() }
                            }
                            if let Some(rep) = l.owner_reputation { span { class: "muted small", "reputation {rep}" } }
                        }
                    }
                }
            }

            div { class: "stat-panel",
                div { class: "stat-panel-head",
                    h3 { "Stats at max rank" }
                    if let Some(q) = l.mean_quality {
                        span { class: "muted small", title: "Average roll of the positive stats",
                            "average roll " strong { "{q * 100.0:.0}%" }
                        }
                    }
                }
                for a in l.attributes.iter() {
                    StatScale { key: "{a.slug}", attr: a.clone() }
                }
            }

            if now.is_some() {
                div { class: "riven-foot muted small",
                    span { "Riven id " RivenId { id: l.riven_id.clone() } }
                }
            }
        }
    }
}

/// One stat at rank 8, on a scale from its worst possible roll to its best. For a negative the
/// scale runs from mildest to harshest, so the fill is how much of the penalty it got.
#[component]
fn StatScale(attr: AttrView) -> Element {
    let a = &attr;
    let value = a.value_rank8.unwrap_or(a.value);
    let kind = if a.positive { "positive" } else { "negative" };
    let Some(q) = a.quality.filter(|_| a.value_rank8.is_some()) else {
        return rsx! {
            div { class: "stat-row {kind}",
                div { class: "stat-label",
                    span { class: "stat-value", "{format_value(&a.slug, value)}" }
                    span { class: "stat-name", title: "{attribute_name(&a.slug)}", "{attribute_name(&a.slug)}" }
                    SplicedMark { slug: a.slug.clone() }
                    span { class: "muted small", "range unknown" }
                }
            }
        };
    };
    let (low, high) = roll_range(&a.slug, value, q);
    let pct = q * 100.0;
    let (verdict, tip) = if a.positive {
        (
            format!("{pct:.0}% roll"),
            "0% is the lowest value this stat can roll, 100% the highest.",
        )
    } else {
        (
            format!("{pct:.0}% of max penalty"),
            "0% is the mildest this negative can roll, 100% the harshest.",
        )
    };
    let label = |v: f64| format_value(&a.slug, v).trim_start_matches('+').to_owned();
    rsx! {
        div { class: "stat-row {kind}", title: "{tip}",
            div { class: "stat-label",
                span { class: "stat-value", "{format_value(&a.slug, value)}" }
                span { class: "stat-name", title: "{attribute_name(&a.slug)}", "{attribute_name(&a.slug)}" }
                SplicedMark { slug: a.slug.clone() }
                span { class: "stat-verdict",
                    if let Some((letter, delta)) = api::grading::roll_grade(q, a.positive) {
                        strong { title: "Favorable deviation from the midpoint of the roll range", "{letter} ({delta:+.1}%)" }
                    }
                    "{verdict}"
                }
            }
            div { class: "stat-scale",
                span { class: "stat-end", "{label(low)}" }
                div { class: "stat-track",
                    role: "meter",
                    aria_valuemin: "0",
                    aria_valuemax: "100",
                    aria_valuenow: "{pct:.0}",
                    aria_label: "{attribute_name(&a.slug)} roll",
                    span { class: "stat-fill", style: "width: {pct:.1}%" }
                    span { class: "stat-marker", style: "left: {pct:.1}%" }
                }
                span { class: "stat-end", "{label(high)}" }
            }
        }
    }
}

/// A riven's polarity as its symbol, with the name as a tooltip. Nothing when it isn't known,
/// as for a scanned riven.
#[component]
pub fn Polarity(polarity: String) -> Element {
    // Simplified outlines of the symbols on the mod cards, on a 24×24 grid.
    let shape = match polarity.as_str() {
        "madurai" => "M4 3h4.5l5 14.5L11.5 21zM14.5 3H20l-5 11.5z",
        "vazarin" => {
            "M4 3h16v6c0 5.5-3.6 9.6-8 12-4.4-2.4-8-6.5-8-12zM9 7.5v1.8c0 2.8 1.2 5 3 6.6 1.8-1.6 3-3.8 3-6.6V7.5z"
        }
        "naramon" => "M3 7.5h18v4H3zM8 14h8v3.5H8z",
        _ => return rsx! {},
    };
    let mut name = polarity.clone();
    name[..1].make_ascii_uppercase();
    rsx! {
        svg {
            class: "polarity {polarity}",
            view_box: "0 0 24 24",
            role: "img",
            "aria-label": "{name} polarity",
            title { "{name} polarity" }
            path { d: shape, fill: "currentColor", fill_rule: "evenodd" }
        }
    }
}

/// How often a riven was rerolled, behind the cycle symbol the mod card uses for it.
#[component]
pub fn Rerolls(count: i64) -> Element {
    let label = if count == 1 {
        "1 reroll".to_owned()
    } else {
        format!("{count} rerolls")
    };
    rsx! {
        span { class: "rerolls", role: "img", title: "{label}", "aria-label": "{label}",
            svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.5",
                stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
                path { d: "M20 12a8 8 0 1 1-2.6-5.9" }
                path { d: "M20 4v4.5h-4.5" }
            }
            span { "aria-hidden": "true", "{count}" }
        }
    }
}

/// One line in the price box: whether the listing is still on warframe.market. Checked once the
/// page is open, and again on request; the server answers from a recent check when it has one.
/// Only for listings the data doesn't already have as gone, whose tag says so.
#[component]
pub fn AvailabilityCheck(auction_id: String) -> Element {
    let mut answer = use_signal(|| None::<Result<Availability, String>>);
    let mut busy = use_signal(|| false);
    let mut check = move |refresh: bool| {
        let id = auction_id.clone();
        busy.set(true);
        spawn(async move {
            let result = api::listing_availability(id, refresh)
                .await
                .map_err(crate::scraper::error_message);
            answer.set(Some(result));
            busy.set(false);
        });
    };
    let mut first = check.clone();
    // Effects only run in the browser, so the page renders without waiting for the check.
    use_effect(move || first(false));

    let ago = |a: &Availability| {
        if a.server_time - a.checked_at < wfmt_types::MINUTE_MS {
            "just now".to_owned()
        } else {
            format!("{} ago", format_age(a.server_time, a.checked_at))
        }
    };
    let (class, text, title) = match &*answer.read() {
        None => (
            "checking",
            "checking warframe.market…".to_owned(),
            String::new(),
        ),
        Some(Err(error)) => ("failed", "couldn't check".to_owned(), error.clone()),
        Some(Ok(a)) if a.listed => (
            "listed",
            "still listed".to_owned(),
            format!("warframe.market still lists it, checked {}", ago(a)),
        ),
        Some(Ok(a)) => (
            "gone",
            "sold or removed".to_owned(),
            format!("Gone from warframe.market, found {}", ago(a)),
        ),
    };
    let gone = class == "gone";

    rsx! {
        div { class: "availability small {class}", "aria-live": "polite", title: "{title}",
            span { class: "availability-dot", aria_hidden: "true" }
            span { "{text}" }
            if !gone {
                button {
                    class: "availability-refresh",
                    r#type: "button",
                    disabled: busy(),
                    title: "Ask warframe.market again",
                    aria_label: "Check again",
                    onclick: move |_| check(true),
                    svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.5",
                        stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
                        class: if busy() { "spinning" } else { "" },
                        path { d: "M20 12a8 8 0 1 1-2.6-5.9" }
                        path { d: "M20 4v4.5h-4.5" }
                    }
                }
            }
        }
    }
}

/// Marks a stat only the Riven Splicer gives, after its name. Nothing for other stats.
#[component]
pub fn SplicedMark(slug: String) -> Element {
    if !api::is_spliced(&slug) {
        return rsx! {};
    }
    rsx! {
        span {
            class: "spliced-mark",
            role: "img",
            title: "Spliced: only the Riven Splicer gives this stat",
            "aria-label": "spliced",
            svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.2",
                stroke_linecap: "round", "aria-hidden": "true",
                path { d: "M8 3c0 4.5 8 4.5 8 9s-8 4.5-8 9" }
                path { d: "M16 3c0 4.5-8 4.5-8 9s8 4.5 8 9" }
                path { d: "M9.5 6.5h5M9.5 17.5h5" }
            }
        }
    }
}
