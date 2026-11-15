//! The top of a riven's page: what it is, what it costs, and how well each stat rolled.

use dioxus::prelude::*;

use crate::api::{
    self, AttrView, Availability, Listing, Overview, attribute_name, format_value, roll_range,
};
use crate::ui::{RivenId, format_age, format_date, format_plat};
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
                        div {
                            span { class: "plat", "{price}p" }
                            span { class: "muted small", " {kind}" }
                        }
                        div { class: "small", "{l.owner_name}"
                            if let Some(rep) = l.owner_reputation { span { class: "muted", " · rep {rep}" } }
                        }
                        div { class: "muted small", "listed {format_age(now, l.created)} ago" }
                        a { class: "small", href: "{url}", target: "_blank", rel: "noopener", "warframe.market ↗" }
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
                    span { "Riven id " RivenId { id: l.riven_id.clone(), short: false } }
                    span { "first seen {format_date(l.first_seen)}" }
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
                span { class: "stat-verdict", "{verdict}" }
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

/// Whether the listing is still on warframe.market. Checked once the page is open, and again
/// on request; the server answers from a recent check when it has one. A listing the data
/// already has as gone isn't checked.
#[component]
pub fn AvailabilityPanel(
    auction_id: String,
    gone_at: Option<i64>,
    gone_reason: Option<String>,
    now: i64,
) -> Element {
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
    let known_gone = gone_at.is_some();
    // Effects only run in the browser, so the page renders without waiting for the check.
    use_effect(move || {
        if !known_gone {
            first(false);
        }
    });

    // What to show: the latest answer, else the data's own removal, else that it's checking.
    let shown = match answer() {
        Some(result) => Some(result),
        None => gone_at.map(|at| {
            Ok(Availability {
                listed: false,
                checked_at: at,
                live: false,
                gone_reason: gone_reason.clone(),
                server_time: now,
                refresh_at: 0,
            })
        }),
    };
    let ago = |now: i64, then: i64| {
        if now - then < wfmt_types::MINUTE_MS {
            "just now".to_owned()
        } else {
            format!("{} ago", format_age(now, then))
        }
    };
    let (class, title, detail) = match &shown {
        None => (
            "checking",
            "Checking warframe.market…".to_owned(),
            "Looking up whether this listing is still for sale.".to_owned(),
        ),
        Some(Err(error)) => (
            "failed",
            "Couldn't check warframe.market".to_owned(),
            error.clone(),
        ),
        Some(Ok(a)) if a.listed => (
            "listed",
            "Available".to_owned(),
            format!(
                "Still listed on warframe.market · checked {}",
                ago(a.server_time, a.checked_at)
            ),
        ),
        Some(Ok(a)) => (
            "gone",
            "Gone".to_owned(),
            match a.gone_reason.as_deref() {
                Some("expired") => format!(
                    "Not seen for days, so counted as gone {}",
                    ago(a.server_time, a.checked_at)
                ),
                Some("missing") => format!(
                    "Sold or removed · missing from warframe.market's search {}",
                    ago(a.server_time, a.checked_at)
                ),
                _ => format!(
                    "Sold or removed · warframe.market confirmed it {}",
                    ago(a.server_time, a.checked_at)
                ),
            },
        ),
    };
    let can_refresh = !matches!(&shown, Some(Ok(a)) if !a.listed);

    rsx! {
        section { class: "availability {class}", "aria-live": "polite",
            span { class: "availability-icon", "aria-hidden": "true",
                match class {
                    "checking" => rsx! { span { class: "spinner" } },
                    "listed" => rsx! {
                        svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.5",
                            stroke_linecap: "round", stroke_linejoin: "round",
                            circle { cx: "12", cy: "12", r: "9.5" }
                            path { d: "M7.5 12.5l3 3 6-6.5" }
                        }
                    },
                    "gone" => rsx! {
                        svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.5",
                            stroke_linecap: "round", stroke_linejoin: "round",
                            circle { cx: "12", cy: "12", r: "9.5" }
                            path { d: "M8.5 8.5l7 7M15.5 8.5l-7 7" }
                        }
                    },
                    _ => rsx! {
                        svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.5",
                            stroke_linecap: "round", stroke_linejoin: "round",
                            path { d: "M12 3.5l9.5 17h-19z" }
                            path { d: "M12 10v4.5M12 17.5v.5" }
                        }
                    },
                }
            }
            div { class: "availability-text",
                strong { "{title}" }
                span { class: "muted small", "{detail}" }
            }
            if can_refresh {
                button {
                    class: "availability-refresh",
                    r#type: "button",
                    disabled: busy(),
                    title: "Ask warframe.market again",
                    onclick: move |_| check(true),
                    svg { view_box: "0 0 24 24", fill: "none", stroke: "currentColor", stroke_width: "2.5",
                        stroke_linecap: "round", stroke_linejoin: "round", "aria-hidden": "true",
                        class: if busy() { "spinning" } else { "" },
                        path { d: "M20 12a8 8 0 1 1-2.6-5.9" }
                        path { d: "M20 4v4.5h-4.5" }
                    }
                    if busy() { "Checking…" } else { "Check again" }
                }
            }
        }
    }
}
