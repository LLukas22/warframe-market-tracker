//! The top of a riven's page: what it is, what it costs, and how well each stat rolled.

use dioxus::prelude::*;

use crate::api::{AttrView, Listing, Overview, attribute_name, format_value, roll_range};
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
    let rolls = l.re_rolls.unwrap_or(0);
    let polarity = if l.polarity.is_empty() {
        String::new()
    } else {
        format!(" · {}", l.polarity)
    };
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
                    div { class: "muted small", "{mr} · {rolls} rolls{polarity}" }
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
