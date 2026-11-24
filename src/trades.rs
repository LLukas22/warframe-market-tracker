//! In-game trade prices from Warframe's weekly riven report (see `wfmt_dataset::weekly`), drawn
//! into a weapon's price chart.

use dioxus::prelude::*;

use crate::api::{self, HistoryPoint, TradeWeek};
use crate::chart::PriceChart;

/// The tracker follows warframe.market's PC listings, so the chart compares them with PC trades.
const PLATFORM: &str = "pc";

/// The price chart, with the weapon's in-game trade prices once they've loaded. Without a
/// weapon, or before they arrive, it's the plain chart.
#[component]
pub fn TradesChart(
    points: Vec<HistoryPoint>,
    bucket_hours: u32,
    weapon: Option<String>,
) -> Element {
    // The report changes once a week, so once per weapon page is enough.
    let trades = use_resource(use_reactive!(|weapon| async move {
        match weapon {
            Some(name) => api::weapon_trades(name, PLATFORM.to_owned())
                .await
                .ok()
                .flatten()
                .map(|t| t.weeks)
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }));
    let trades = trades.read().clone().unwrap_or_default();
    rsx! { PriceChart { points, bucket_hours, trades } }
}

/// The weeks of trade prices that fall within `t0..t1`, as `(start, end, week)`, oldest first.
/// A report covers the week before it was published, and stays the latest word on in-game
/// prices until the next one, so each week reaches to where the next begins, the last to `t1`.
pub(crate) fn trade_steps(weeks: &[TradeWeek], t0: i64, t1: i64) -> Vec<(i64, i64, TradeWeek)> {
    let mut dated: Vec<(i64, &TradeWeek)> = weeks
        .iter()
        .filter_map(|w| crate::ui::parse_date(&w.date).map(|t| (t - 7 * api::DAY_MS, w)))
        .collect();
    dated.sort_by_key(|(start, _)| *start);
    let ends: Vec<i64> = dated
        .iter()
        .skip(1)
        .map(|(s, _)| *s)
        .chain([i64::MAX])
        .collect();
    dated
        .into_iter()
        .zip(ends)
        .filter_map(|((start, week), end)| {
            let (start, end) = (start.max(t0), end.min(t1));
            (start < end).then(|| (start, end, week.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{DAY_MS, TradeStats};

    fn week(date: &str) -> TradeWeek {
        let stats = TradeStats {
            median: 1.0,
            avg: 1.0,
            stddev: 0.0,
            min: 1.0,
            max: 1.0,
            pop: 1.0,
        };
        TradeWeek {
            date: date.into(),
            unrolled: Some(stats),
            rerolled: None,
        }
    }

    #[test]
    fn each_week_reaches_the_next_and_the_last_reaches_the_end() {
        let day = |d: &str| crate::ui::parse_date(d).unwrap();
        let weeks = [week("2026-11-16"), week("2026-11-09"), week("2026-10-19")];
        let (t0, t1) = (day("2026-11-01"), day("2026-11-22"));
        let steps: Vec<(i64, i64, String)> = trade_steps(&weeks, t0, t1)
            .into_iter()
            .map(|(s, e, w)| (s, e, w.date))
            .collect();
        assert_eq!(
            steps,
            [
                // Its week began before the chart and lasted until the next one's.
                (t0, day("2026-11-02"), "2026-10-19".to_owned()),
                (
                    day("2026-11-02"),
                    day("2026-11-09"),
                    "2026-11-09".to_owned()
                ),
                (day("2026-11-09"), t1, "2026-11-16".to_owned()),
            ]
        );
        // A chart of the last day still shows the newest report.
        let last_day = trade_steps(&weeks, t1 - DAY_MS, t1);
        assert_eq!(last_day.len(), 1);
        assert_eq!(last_day[0].2.date, "2026-11-16");
        assert!(trade_steps(&weeks, day("2026-01-01"), day("2026-02-01")).is_empty());
    }
}
