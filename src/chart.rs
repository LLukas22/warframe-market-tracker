//! Minimal SVG line chart for price history.

use dioxus::prelude::*;

use crate::api::{
    self, DAY_MS, EFFECT_MIN_RIVENS, EFFECT_MIN_STAT_RIVENS, HOUR_MS, HistoryPoint, StatEffect,
    TradeStats, TradeWeek, attribute_name,
};
use crate::ui::{format_date, format_datetime, format_plat, format_short_datetime, format_time};

/// Horizontal room, in pixels, each point needs for its median marker to show.
const MIN_MARKER_SPACING: f64 = 14.0;

/// Charts are drawn at this width until they've been measured.
pub(crate) const INITIAL_WIDTH: f64 = 900.0;

thread_local! {
    /// The price chart's last measured width, shared by every chart instance.
    static LAST_WIDTH: std::cell::Cell<f64> = const { std::cell::Cell::new(INITIAL_WIDTH) };
}
const H: f64 = 260.0;
const LEFT: f64 = 56.0;
const RIGHT: f64 = 16.0;
const TOP: f64 = 12.0;
const BOTTOM: f64 = 28.0;
/// Room each time label on the x axis needs, and how many labels there may be.
const X_LABEL_WIDTH: f64 = 120.0;
const X_LABELS: std::ops::RangeInclusive<f64> = 2.0..=6.0;
/// The y axis aims for about this many steps.
const Y_STEPS: f64 = 4.0;
/// Resizes smaller than this are measurement noise, not worth a redraw.
pub(crate) const RESIZE_THRESHOLD: f64 = 0.5;

/// Rounds `max` up to a "nice" axis maximum and returns (max, step).
pub(crate) fn nice_axis(max: f64) -> (f64, f64) {
    let max = max.max(1.0);
    let raw = max / Y_STEPS;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .into_iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag);
    ((max / step).ceil() * step, step)
}

fn period_start(t: i64, hourly: bool) -> String {
    if hourly {
        format!("{} UTC", format_datetime(t))
    } else {
        format_date(t)
    }
}

type TradeKind = fn(&TradeWeek) -> Option<&TradeStats>;

/// The weapon's in-game trades in a report: of rivens never rerolled, and rerolled.
const TRADE_KINDS: [(&str, TradeKind); 2] = [
    ("unrolled", |w| w.unrolled.as_ref()),
    ("rerolled", |w| w.rerolled.as_ref()),
];
/// An in-game median may stretch the y axis to this many times the asking prices' top; a
/// higher one isn't drawn, as it would squash the asking prices.
const TRADE_STRETCH: f64 = 1.5;

/// A median keeps its half platinum.
fn format_trade(median: f64) -> String {
    if median.fract().abs() < 0.05 {
        format_plat(median.round() as i64)
    } else {
        format!("{median:.1}")
    }
}

/// Asking prices over time. `trades` are the weapon's weeks in Warframe's weekly riven report,
/// drawn as in-game medians, see `trades::trade_steps`.
#[component]
pub fn PriceChart(
    points: Vec<HistoryPoint>,
    bucket_hours: u32,
    #[props(default)] trades: Vec<TradeWeek>,
) -> Element {
    let mut hovered = use_signal(|| None::<usize>);
    // Start at the last measured width: a remounted chart drawn at a guessed width
    // visibly rescales once it is measured.
    let mut width = use_signal(|| LAST_WIDTH.get());
    // Match SVG units to CSS pixels so labels and markers keep their proportions.
    let w = width();

    if points.is_empty() {
        return rsx! { div { class: "chart empty", "No prices recorded for this search in this period." } };
    }

    let bucket_ms = i64::from(bucket_hours.max(1)) * HOUR_MS;
    let t0 = points.first().unwrap().t;
    let t1 = points.last().unwrap().t.max(t0 + bucket_ms);
    let steps = crate::trades::trade_steps(&trades, t0, t1);
    let asking_top = points
        .iter()
        .map(|p| p.p75.max(p.gone_median.unwrap_or(0)))
        .max()
        .unwrap_or(1);
    let trade_top = steps
        .iter()
        .flat_map(|(_, _, w)| TRADE_KINDS.map(|(_, get)| get(w).map(|s| s.median)))
        .flatten()
        .filter(|m| *m <= asking_top as f64 * TRADE_STRETCH)
        .fold(0.0, f64::max)
        .ceil() as i64;
    let top = asking_top.max(trade_top);
    let (y_max, y_step) = nice_axis(top as f64);

    // Hourly buckets need times; a chart within one day needs nothing else.
    let hourly = i64::from(bucket_hours) * HOUR_MS < DAY_MS;
    let one_day = t1 - t0 < DAY_MS;
    let axis_label = move |t: i64| match (hourly, one_day) {
        (false, _) => format_date(t),
        (true, true) => format_time(t),
        (true, false) => format_short_datetime(t),
    };
    let x = move |t: i64| LEFT + (t - t0) as f64 / (t1 - t0) as f64 * (w - LEFT - RIGHT);
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
    // Evenly spaced in time, on bucket boundaries: points can be unevenly spaced, and
    // labelling them by index would crowd the labels wherever data is dense.
    let t_last = points.last().unwrap().t;
    let x_ticks: Vec<i64> = {
        let capacity = ((w - LEFT - RIGHT) / X_LABEL_WIDTH)
            .floor()
            .clamp(*X_LABELS.start(), *X_LABELS.end()) as i64;
        let buckets = (t_last - t0) / bucket_ms + 1;
        let n = buckets.min(capacity).max(1);
        let mut ticks: Vec<i64> = (0..n)
            .map(|i| {
                let t = t0 + (t_last - t0) * i / (n - 1).max(1);
                t0 + ((t - t0) as f64 / bucket_ms as f64).round() as i64 * bucket_ms
            })
            .collect();
        ticks.dedup();
        ticks
    };

    // One column per point, so a tap or hover anywhere near a point selects it.
    let columns: Vec<(f64, f64)> = (0..points.len())
        .map(|i| {
            let middle = |a: usize, b: usize| (x(points[a].t) + x(points[b].t)) / 2.0;
            let start = if i == 0 { LEFT } else { middle(i - 1, i) };
            let end = if i + 1 == points.len() {
                w - RIGHT
            } else {
                middle(i, i + 1)
            };
            (start, end)
        })
        .collect();

    let roomy = (w - LEFT - RIGHT) / points.len() as f64 >= MIN_MARKER_SPACING;

    // The in-game medians as step lines, one per kind, broken where a week has none or one
    // too high to draw.
    let trade_lines: Vec<(String, &'static str, String)> = TRADE_KINDS
        .iter()
        .flat_map(|&(kind, get)| {
            let mut runs: Vec<Vec<String>> = vec![Vec::new()];
            for (start, end, week) in &steps {
                match get(week).map(|s| s.median).filter(|m| *m <= y_max) {
                    Some(m) => {
                        let ty = TOP + (1.0 - m / y_max) * (H - TOP - BOTTOM);
                        let run = runs.last_mut().unwrap();
                        run.push(format!("{:.1},{ty:.1}", x(*start)));
                        run.push(format!("{:.1},{ty:.1}", x(*end)));
                    }
                    None => runs.push(Vec::new()),
                }
            }
            runs.into_iter()
                .filter(|run| !run.is_empty())
                .enumerate()
                .map(move |(i, run)| (format!("{kind}-{i}"), kind, run.join(" ")))
        })
        .collect();

    let tooltip = hovered().and_then(|i| points.get(i)).map(|p| {
        let left = x(p.t) / w * 100.0;
        rsx! {
            div { class: "tooltip", style: "left: clamp(110px, {left:.1}%, calc(100% - 110px))",
                div { class: "tooltip-date", "{period_start(p.t, hourly)}" }
                div { "median {format_plat(p.median)}" }
                div { "p25–p75 {format_plat(p.p25)} – {format_plat(p.p75)}" }
                div { class: "muted small", "of the {p.recent} newest prices" }
                div { "cheapest {format_plat(p.min)}" }
                div { class: "muted", "{p.count} rivens listed" }
                if let Some((_, _, week)) = steps.iter().find(|(s, e, _)| (*s..*e).contains(&p.t)) {
                    div { class: "tooltip-trade",
                        "in-game median "
                        {TRADE_KINDS
                            .iter()
                            .filter_map(|&(kind, get)| get(week).map(|s| {
                                let m = format_trade(s.median);
                                if kind == "rerolled" { format!("↻ {m}") } else { m }
                            }))
                            .collect::<Vec<_>>()
                            .join(" · ")}
                    }
                }
                if let Some(g) = p.gone_median {
                    div { class: "tooltip-gone", "{p.gone_count} disappeared · median {format_plat(g)}" }
                }
            }
        }
    });

    rsx! {
        div { class: "chart",
            onresize: move |event| {
                if let Ok(size) = event.get_content_box_size()
                    && size.width > LEFT + RIGHT
                    && (size.width - width()).abs() > RESIZE_THRESHOLD
                {
                    LAST_WIDTH.set(size.width);
                    width.set(size.width);
                }
            },
            svg {
                view_box: "0 0 {w} {H}",
                preserve_aspect_ratio: "xMinYMin meet",
                // A chart rendered by the server and hydrated may never get a resize event,
                // and would keep the guessed width, shrunk to fit: measure it once mounted.
                onmounted: move |event| async move {
                    if let Ok(rect) = event.get_client_rect().await {
                        let measured = rect.width();
                        if measured > LEFT + RIGHT && (measured - width()).abs() > RESIZE_THRESHOLD {
                            LAST_WIDTH.set(measured);
                            width.set(measured);
                        }
                    }
                },
                onmouseleave: move |_| hovered.set(None),
                for v in y_ticks {
                    line {
                        class: "grid",
                        x1: "{LEFT}",
                        x2: "{w - RIGHT}",
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
                        text_anchor: if t == t0 { "start" } else if t >= t_last { "end" } else { "middle" },
                        "{axis_label(t)}"
                    }
                }
                polygon { class: "band", points: "{band}" }
                // Under the lines, so they never hide the median.
                for p in points.iter().filter(|p| p.gone_median.is_some()) {
                    circle {
                        key: "gone-{p.t}",
                        class: "gone-dot",
                        cx: "{x(p.t)}",
                        cy: "{y(p.gone_median.unwrap_or_default())}",
                        r: "{(2.0 + (p.gone_count as f64).sqrt() * 0.6).min(6.0):.1}",
                    }
                }
                for (key, kind, line) in trade_lines {
                    polyline { key: "trade-{key}", class: "trade-line {kind}", points: "{line}" }
                }
                polyline { class: "min-line", points: "{min}" }
                polyline { class: "median-line", points: "{median}" }
                // Markers only where points have room; packed tight they'd merge into a
                // chain of circles. The hovered one always shows.
                for (i, p) in points.iter().enumerate().filter(|&(i, _)| roomy || hovered() == Some(i)) {
                    circle {
                        key: "{p.t}",
                        class: if hovered() == Some(i) { "dot active" } else { "dot" },
                        cx: "{x(p.t)}",
                        cy: "{y(p.median)}",
                        r: "4",
                    }
                }
                for (i, (start, end)) in columns.into_iter().enumerate() {
                    rect {
                        key: "hit-{i}",
                        class: "hit",
                        x: "{start:.1}",
                        y: "{TOP}",
                        width: "{(end - start).max(0.0):.1}",
                        height: "{H - TOP - BOTTOM}",
                        onmouseenter: move |_| hovered.set(Some(i)),
                        onclick: move |_| hovered.set(Some(i)),
                    }
                }
            }
            {tooltip}
            div { class: "legend",
                span { class: "legend-item",
                    title: "Of recent asking prices: those listed or changed in the 2 days before each point, or the 20 newest where there were fewer. The cheapest is of every listed riven.",
                    span { class: "swatch median" }
                    "median"
                }
                span { class: "legend-item", span { class: "swatch band" } "25–75%" }
                span { class: "legend-item", span { class: "swatch min" } "cheapest" }
                span { class: "legend-item", span { class: "swatch gone" } "disappeared (≈ sold), size = count" }
                if !steps.is_empty() {
                    span { class: "legend-item",
                        title: "What rivens for this weapon sold for in in-game trades on PC: the medians in Digital Extremes' weekly report, each from the week it covers until the next. The listings are asking prices.",
                        span { class: "swatch trade" }
                        "in-game unrolled"
                        span { class: "swatch trade rerolled" }
                        "↻ rerolled"
                    }
                }
            }
            PriceData { points, hourly }
        }
    }
}

#[component]
fn PriceData(points: Vec<HistoryPoint>, hourly: bool) -> Element {
    rsx! {
        details { class: "chart-data",
            summary { "View price data" }
            div { class: "chart-table-scroll", tabindex: "0", role: "region", aria_label: "Price data",
                table {
                    caption { "Asking prices in platinum by UTC period" }
                    thead { tr {
                        for heading in ["Period (UTC)", "Median", "25th percentile", "75th percentile", "Cheapest", "Listed", "Newest prices", "Disappeared", "Disappeared median"] {
                            th { scope: "col", "{heading}" }
                        }
                    } }
                    tbody {
                        for p in &points {
                            tr { key: "{p.t}",
                                th { scope: "row", "{period_start(p.t, hourly)}" }
                                td { "{format_plat(p.median)}" }
                                td { "{format_plat(p.p25)}" }
                                td { "{format_plat(p.p75)}" }
                                td { "{format_plat(p.min)}" }
                                td { "{p.count}" }
                                td { "{p.recent}" }
                                td { "{p.gone_count}" }
                                td { {p.gone_median.map(format_plat).unwrap_or_else(|| "—".into())} }
                            }
                        }
                    }
                }
            }
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
    let cache = use_context::<crate::ui::ViewCache>();
    let seed = crate::ui::use_seed(weapon.clone(), |weapon| api::seed::stat_effects(weapon));
    // Seeded from the cache so returning to a weapon doesn't pop this panel in late and
    // push the listings down.
    let cached = use_hook(|| {
        if let Some(data) = seed {
            cache.store_effects(weapon.clone(), data);
        }
        cache.effects(&weapon)
    });
    let result = use_resource(move || {
        data_version.read();
        let weapon = weapon.clone();
        async move { cache.load_effects(weapon).await }
    });
    let fetched = result.read();
    let data = match &*fetched {
        Some(Ok(data)) => data.clone(),
        _ => match cached {
            Some(data) => data,
            None => return rsx! {},
        },
    };
    let data = &data;
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
                if (data.rivens as usize) < EFFECT_MIN_RIVENS {
                    "Not enough verified listings yet: {data.rivens} of the {EFFECT_MIN_RIVENS} needed."
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
        .map(|s| s.effect.ln_1p().abs() * EFFECT_SCALE_HEADROOM)
        .fold(MIN_EFFECT_SCALE, f64::max);

    rsx! {
        div { class: "section-head",
            h2 { "Which stats are wanted" }
            span {
                class: "muted small",
                title: "From asking prices, so these are associations rather than proven effects. Bars show the model's uncertainty. Stats on fewer than {EFFECT_MIN_STAT_RIVENS} rivens aren't shown.",
                "{data.rivens} rivens · {price_range}"
            }
        }
        div { class: "effects",
            div { class: "effects-grid",
                EffectGroup {
                    title: "Positive stats",
                    stats: data.positive.clone(),
                    scale,
                }
                EffectGroup {
                    title: "Negative stats",
                    stats: data.negative.clone(),
                    scale,
                }
            }
        }
    }
}

#[component]
fn EffectGroup(title: String, stats: Vec<StatEffect>, scale: f64) -> Element {
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
            if stats.is_empty() {
                p { class: "muted small", "Too few rivens to tell." }
            } else if clear.is_empty() {
                p { class: "muted small", "No stat clearly raises or lowers the price." }
            } else {
                div { class: "effect-axis muted small",
                    span {}
                    span { class: "effect-axis-labels",
                        span { "lower" }
                        span { "higher" }
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
                    summary { "{unclear.len()} unclear" }
                    p { class: "muted small", "{unclear_names}" }
                }
            }
        }
    }
}

/// Room past the largest certain effect, so its bar doesn't end at the edge.
const EFFECT_SCALE_HEADROOM: f64 = 1.25;
/// The scale always reaches at least a doubling and a halving of the price.
const MIN_EFFECT_SCALE: f64 = std::f64::consts::LN_2;

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
        li {
            details {
                summary { class: "effect-row",
                    span { class: "effect-name",
                        "{name}"
                        crate::riven_view::SplicedMark { slug: stat.slug.clone() }
                    }
                    span { class: "effect-track {kind}",
                        span {
                            class: "effect-range",
                            style: "left: {low:.2}%; width: {width:.2}%",
                        }
                        span { class: "effect-dot", style: "left: {dot:.2}%" }
                    }
                }
                p { class: "effect-detail", "{tooltip}" }
            }
        }
    }
}
