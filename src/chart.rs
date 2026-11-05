//! Minimal SVG line chart for price history.

use dioxus::prelude::*;

use crate::api::{self, HistoryPoint, StatEffect, attribute_name};
use crate::ui::{format_date, format_plat};

const W: f64 = 900.0;
const H: f64 = 260.0;
const LEFT: f64 = 56.0;
const RIGHT: f64 = 16.0;
const TOP: f64 = 12.0;
const BOTTOM: f64 = 28.0;

/// Rounds `max` up to a "nice" axis maximum and returns (max, step).
fn nice_axis(max: f64) -> (f64, f64) {
    let max = max.max(1.0);
    let raw = max / 4.0;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag);
    ((max / step).ceil() * step, step)
}

#[component]
pub fn PriceChart(points: Vec<HistoryPoint>, bucket_hours: u32) -> Element {
    let mut hovered = use_signal(|| None::<usize>);

    if points.is_empty() {
        return rsx! { div { class: "chart empty", "No price history for this search yet." } };
    }

    let bucket_ms = i64::from(bucket_hours.max(1)) * 3_600_000;
    let t0 = points.first().unwrap().t;
    let t1 = points.last().unwrap().t.max(t0 + bucket_ms);
    let top = points
        .iter()
        .map(|p| p.p75.max(p.gone_median.unwrap_or(0)))
        .max()
        .unwrap_or(1);
    let (y_max, y_step) = nice_axis(top as f64);

    let x = move |t: i64| LEFT + (t - t0) as f64 / (t1 - t0) as f64 * (W - LEFT - RIGHT);
    let y = move |v: i64| TOP + (1.0 - v as f64 / y_max) * (H - TOP - BOTTOM);

    let line = |f: fn(&HistoryPoint) -> i64| {
        points
            .iter()
            .map(|p| format!("{:.1},{:.1}", x(p.t), y(f(p))))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let median = line(|p| p.median);
    let min = line(|p| p.min);
    let band = points
        .iter()
        .map(|p| format!("{:.1},{:.1}", x(p.t), y(p.p75)))
        .chain(
            points
                .iter()
                .rev()
                .map(|p| format!("{:.1},{:.1}", x(p.t), y(p.p25))),
        )
        .collect::<Vec<_>>()
        .join(" ");

    let y_ticks: Vec<f64> = (0..)
        .map(|i| i as f64 * y_step)
        .take_while(|v| *v <= y_max + 1e-9)
        .collect();
    let x_ticks: Vec<i64> = {
        let n = points.len().min(6);
        (0..n)
            .map(|i| points[i * (points.len() - 1) / (n - 1).max(1)].t)
            .collect()
    };

    let tooltip = hovered().and_then(|i| points.get(i)).map(|p| {
        let left = x(p.t) / W * 100.0;
        rsx! {
            div { class: "tooltip", style: "left: {left:.1}%",
                div { class: "tooltip-date", "{format_date(p.t)}" }
                div { "median {format_plat(p.median)}" }
                div { "p25–p75 {format_plat(p.p25)} – {format_plat(p.p75)}" }
                div { "cheapest {format_plat(p.min)}" }
                div { class: "muted", "{p.count} rivens listed" }
                if let Some(g) = p.gone_median {
                    div { class: "tooltip-gone", "{p.gone_count} disappeared · median {format_plat(g)}" }
                }
            }
        }
    });

    rsx! {
        div { class: "chart",
            svg {
                view_box: "0 0 {W} {H}",
                preserve_aspect_ratio: "none",
                onmouseleave: move |_| hovered.set(None),
                for v in y_ticks {
                    line {
                        class: "grid",
                        x1: "{LEFT}",
                        x2: "{W - RIGHT}",
                        y1: "{y(v as i64)}",
                        y2: "{y(v as i64)}",
                    }
                    text {
                        class: "axis",
                        x: "{LEFT - 6.0}",
                        y: "{y(v as i64) + 4.0}",
                        text_anchor: "end",
                        "{format_plat(v as i64)}"
                    }
                }
                for t in x_ticks {
                    text {
                        class: "axis",
                        x: "{x(t)}",
                        y: "{H - 8.0}",
                        text_anchor: "middle",
                        "{format_date(t)}"
                    }
                }
                polygon { class: "band", points: "{band}" }
                polyline { class: "min-line", points: "{min}" }
                polyline { class: "median-line", points: "{median}" }
                for p in points.iter().filter(|p| p.gone_median.is_some()) {
                    circle {
                        key: "gone-{p.t}",
                        class: "gone-dot",
                        cx: "{x(p.t)}",
                        cy: "{y(p.gone_median.unwrap_or_default())}",
                        r: "{(3.0 + (p.gone_count as f64).sqrt() * 1.5).min(9.0):.1}",
                    }
                }
                for (i, p) in points.iter().enumerate() {
                    circle {
                        key: "{p.t}",
                        class: if hovered() == Some(i) { "dot active" } else { "dot" },
                        cx: "{x(p.t)}",
                        cy: "{y(p.median)}",
                        r: "4",
                        onmouseenter: move |_| hovered.set(Some(i)),
                    }
                }
            }
            {tooltip}
            div { class: "legend",
                span { class: "swatch median" } "median"
                span { class: "swatch band" } "25–75%"
                span { class: "swatch min" } "cheapest"
                span { class: "swatch gone" } "disappeared (≈ sold), size = count"
            }
        }
    }
}

/// Tiny inline price sparkline for one auction.
#[component]
pub fn Sparkline(points: Vec<(i64, i64)>) -> Element {
    if points.len() < 2 {
        return rsx! {};
    }
    let (w, h) = (160.0, 32.0);
    let t0 = points.first().unwrap().0;
    let t1 = points.last().unwrap().0.max(t0 + 1);
    let lo = points.iter().map(|p| p.1).min().unwrap();
    let hi = points.iter().map(|p| p.1).max().unwrap().max(lo + 1);
    let path = points
        .iter()
        .map(|(t, v)| {
            format!(
                "{:.1},{:.1}",
                (t - t0) as f64 / (t1 - t0) as f64 * w,
                2.0 + (1.0 - (v - lo) as f64 / (hi - lo) as f64) * (h - 4.0)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    rsx! {
        svg { class: "sparkline", view_box: "0 0 {w} {h}", width: "{w}", height: "{h}",
            polyline { points: "{path}" }
        }
    }
}

/// How many times the price, e.g. `×4.6`.
fn format_multiplier(effect: f64) -> String {
    format!("×{:.1}", 1.0 + effect)
}

/// Which stats sellers price higher or lower on a weapon, positive and negative stats apart.
/// A ranking rather than a price estimate, so it shows positions instead of numbers.
#[component]
pub fn StatEffectsPanel(weapon: String) -> Element {
    let data_version = use_context::<Signal<u64>>();
    let result = use_resource(move || {
        let weapon = weapon.clone();
        async move {
            data_version.read();
            api::stat_effects(weapon).await
        }
    });
    let result = result.read();
    let Some(Ok(data)) = &*result else {
        return rsx! {};
    };
    let price_range = match (data.low_price, data.high_price) {
        (Some(low), Some(high)) => format!("{}–{}p", format_plat(low), format_plat(high)),
        _ => "no prices".to_owned(),
    };

    if data.positive.is_empty() && data.negative.is_empty() {
        return rsx! {
            div { class: "section-head",
                h2 { "Which stats are wanted" }
            }
            div { class: "effects-empty",
                if (data.rivens as usize) < 30 {
                    "Not enough verified listings yet: {data.rivens} of the 30 needed."
                } else {
                    "No clear pattern yet. The {data.rivens} verified listings cost {price_range}, and their stats don't predict those prices. This often means only one end of the price range has been seen so far."
                }
            }
        };
    }

    // Log scale, shared by both sides: doubling and halving sit equally far from the middle.
    let scale = data
        .positive
        .iter()
        .chain(&data.negative)
        .filter(|s| s.certain)
        .map(|s| s.effect.ln_1p().abs() * 1.25)
        .fold(2f64.ln(), f64::max);

    rsx! {
        div { class: "section-head",
            h2 { "Which stats are wanted" }
            span { class: "muted small",
                "from the asking prices of {data.rivens} verified rivens ({price_range})"
            }
        }
        div { class: "effects",
            div { class: "effects-grid",
                EffectGroup {
                    title: "Positive stats",
                    note: "compared with rivens without the stat",
                    stats: data.positive.clone(),
                    scale,
                }
                EffectGroup {
                    title: "Negative stats",
                    note: "compared with rivens without a negative",
                    stats: data.negative.clone(),
                    scale,
                }
            }
            div { class: "legend",
                span { class: "swatch effect-up" } "sellers ask more"
                span { class: "swatch effect-down" } "sellers ask less"
                span { class: "swatch effect-range-swatch" } "model uncertainty"
            }
            p { class: "muted small effects-note",
                "Estimates describe associations in sampled asking prices. Stats that cannot be separated are marked unclear; ranges show approximate model uncertainty."
            }
            if data.rare_stats > 0 {
                p { class: "muted small effects-note",
                    "{data.rare_stats} stats on fewer than 5 rivens aren't shown."
                }
            }
        }
    }
}

#[component]
fn EffectGroup(title: String, note: String, stats: Vec<StatEffect>, scale: f64) -> Element {
    let (clear, unclear): (Vec<StatEffect>, Vec<StatEffect>) =
        stats.iter().cloned().partition(|s| s.certain);
    let unclear_names = unclear
        .iter()
        .map(|s| attribute_name(&s.slug))
        .collect::<Vec<_>>()
        .join(", ");
    rsx! {
        section {
            h3 { "{title}" }
            p { class: "muted small", "{note}" }
            if stats.is_empty() {
                p { class: "muted small", "Too few rivens to tell." }
            } else if clear.is_empty() {
                p { class: "muted small", "No stat clearly raises or lowers the price." }
            } else {
                div { class: "effect-axis muted small",
                    span {}
                    span { class: "effect-axis-labels",
                        span { "lower price" }
                        span { "higher price" }
                    }
                }
                ul { class: "effect-list",
                    for s in clear {
                        EffectRow { key: "{s.slug}", stat: s, scale }
                    }
                }
            }
            if !unclear.is_empty() {
                details { class: "effect-unclear-list",
                    summary { "{unclear.len()} with no clear effect" }
                    p { class: "muted small", "{unclear_names}" }
                }
            }
        }
    }
}

#[component]
fn EffectRow(stat: StatEffect, scale: f64) -> Element {
    let name = attribute_name(&stat.slug);
    let position = |effect: f64| (50.0 + effect.ln_1p() / scale * 50.0).clamp(0.0, 100.0);
    let (low, dot, high) = (
        position(stat.low),
        position(stat.effect),
        position(stat.high),
    );
    let width = high - low;
    let center = if width > 0.0 {
        (dot - low) / width * 100.0
    } else {
        50.0
    };
    let kind = if stat.effect >= 0.0 {
        "effect-up"
    } else {
        "effect-down"
    };
    let tooltip = format!(
        "{name}: priced around {} compared with rivens without it (model range {} to {}), from {} rivens.",
        format_multiplier(stat.effect),
        format_multiplier(stat.low),
        format_multiplier(stat.high),
        stat.rivens
    );
    rsx! {
        li { class: "effect-row", title: "{tooltip}",
            span { class: "effect-name", "{name}" }
            span { class: "effect-track {kind}",
                span {
                    class: "effect-range",
                    style: "left: {low:.2}%; width: {width:.2}%; --center: {center:.1}%",
                }
                span { class: "effect-dot", style: "left: {dot:.2}%" }
            }
        }
    }
}
