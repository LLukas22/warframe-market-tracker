//! Fetches Warframe's weekly riven trade report for every platform, a few times a day, and keeps
//! each new week (see `wfmt_dataset::weekly`). Every server does this, scraping or not: the
//! report is one small static file per platform, and Digital Extremes only ever publishes the
//! current week, so the history exists only where someone saved it. A server that pulls its data
//! from the Hub also downloads the weeks it missed from there, and one that uploads publishes
//! its weeks along with the days.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use wfmt_api::{TradeStats, TradeWeek, WeaponTrades, WeeklyStatus};
use wfmt_dataset::weekly::{self, PLATFORMS, Row};

/// The report changes once a week, early on Monday UTC.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const TIMEOUT: Duration = Duration::from_secs(30);
/// Weeks a weapon's page shows at most.
const WEEKS_SHOWN: usize = 12;

#[derive(Default)]
struct Live {
    running: bool,
    last_check_at: Option<i64>,
    outcome: Option<String>,
    error: Option<String>,
}

/// The stored rows, and the files they were read from.
type Cache = Option<(BTreeMap<String, u64>, Arc<Vec<Row>>)>;

pub struct Weekly {
    http: reqwest::Client,
    live: Mutex<Live>,
    started: AtomicBool,
    cache: Mutex<Cache>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Weekly {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(wfmt_types::USER_AGENT)
                .timeout(TIMEOUT)
                .build()?,
            live: Mutex::new(Live::default()),
            started: AtomicBool::new(false),
            cache: Mutex::new(None),
        })
    }

    /// Checks for a new report now and every few hours, once per server.
    pub fn start(&'static self, root: PathBuf) {
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            loop {
                lock(&self.live).running = true;
                let result = self.check(&root).await;
                self.finish(result);
                tokio::time::sleep(CHECK_INTERVAL).await;
            }
        });
    }

    fn finish(&self, result: Result<String>) {
        let mut live = lock(&self.live);
        live.running = false;
        live.last_check_at = Some(chrono::Utc::now().timestamp_millis());
        match result {
            Ok(outcome) => {
                live.outcome = Some(outcome);
                live.error = None;
            }
            Err(error) => {
                tracing::warn!("weekly riven report: {error:#}");
                live.error = Some(format!("{error:#}"));
            }
        }
    }

    /// Fetches every platform's report. Fails only if none could be fetched.
    async fn check(&self, root: &Path) -> Result<String> {
        let mut saved = Vec::new();
        let mut failed = Vec::new();
        for (platform, _) in PLATFORMS {
            match self.fetch(root, platform).await {
                Ok(Some(date)) => saved.push(format!("{} {date}", platform_label(platform))),
                Ok(None) => {}
                Err(error) => failed.push(format!("{}: {error:#}", platform_label(platform))),
            }
        }
        if failed.len() == PLATFORMS.len() {
            anyhow::bail!("{}", failed.join("; "));
        }
        let mut outcome = if saved.is_empty() {
            "No new report.".to_owned()
        } else {
            format!("Saved {}.", saved.join(", "))
        };
        if !failed.is_empty() {
            outcome = format!("{outcome} Failed: {}.", failed.join("; "));
        }
        Ok(outcome)
    }

    /// Fetches one platform's report and saves it if it's new, answering with its week.
    async fn fetch(&self, root: &Path, platform: &'static str) -> Result<Option<NaiveDate>> {
        let url = weekly::url(platform).context("unknown platform")?;
        let response = self.http.get(&url).send().await?.error_for_status()?;
        let published_at = response
            .headers()
            .get(reqwest::header::LAST_MODIFIED)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| DateTime::parse_from_rfc2822(value).ok())
            .map(|time| time.with_timezone(&Utc));
        let raw = response.bytes().await?;
        let entries = weekly::parse(std::str::from_utf8(&raw).context("not text")?)?;
        let date = published_at.map_or_else(|| Utc::now().date_naive(), |t| t.date_naive());
        let root = root.to_owned();
        tokio::task::spawn_blocking(move || {
            // The same report again, perhaps without a date or with a new one.
            if let Some((latest, stored)) = weekly::latest_entries(&root, platform)?
                && (latest >= date || stored == entries)
            {
                return Ok(None);
            }
            weekly::save(&root, platform, date, published_at, &entries, &raw)?;
            tracing::info!(platform, %date, "saved the weekly riven report");
            Ok(Some(date))
        })
        .await?
    }

    /// Every stored row, read again when the files changed: new weeks arrive from the fetch
    /// above and from the Hub.
    fn rows(&self, root: &Path) -> Result<Arc<Vec<Row>>> {
        let files = weekly::files(root)?;
        let mut cache = lock(&self.cache);
        if let Some((known, rows)) = &*cache
            && *known == files
        {
            return Ok(rows.clone());
        }
        let mut rows = Vec::new();
        for path in files.keys() {
            match weekly::read(&root.join(path)) {
                Ok(file) => rows.extend(file),
                Err(error) => tracing::warn!(path, "can't read a weekly riven report: {error:#}"),
            }
        }
        let rows = Arc::new(rows);
        *cache = Some((files, rows.clone()));
        Ok(rows)
    }

    /// The reports' rows of the weapon named `name` on `platform`. `None` without any report.
    pub fn trades(&self, root: &Path, name: &str, platform: &str) -> Result<Option<WeaponTrades>> {
        Ok(weapon_trades(&self.rows(root)?, name, platform))
    }

    /// Each weapon's in-game trade popularity in the newest PC report, by lowercase name: see
    /// `trade_popularity`.
    pub fn popularity(&self, root: &Path) -> Result<HashMap<String, u8>> {
        Ok(trade_popularity(&self.rows(root)?))
    }

    pub fn status(&self, root: &Path) -> WeeklyStatus {
        let live = lock(&self.live);
        let mut status = WeeklyStatus {
            running: live.running,
            last_check_at: live.last_check_at,
            outcome: live.outcome.clone(),
            error: live.error.clone(),
            ..WeeklyStatus::default()
        };
        drop(live);
        match weekly::files(root) {
            Ok(files) => {
                status.weeks = files.len();
                let mut newest: BTreeMap<&str, NaiveDate> = BTreeMap::new();
                for (platform, date) in files.keys().filter_map(|p| weekly::parse_path(p)) {
                    let entry = newest.entry(platform).or_insert(date);
                    *entry = (*entry).max(date);
                }
                status.newest = PLATFORMS
                    .iter()
                    .filter_map(|(p, _)| newest.get(p).map(|d| ((*p).to_owned(), d.to_string())))
                    .collect();
            }
            Err(error) => status.error = Some(format!("{error:#}")),
        }
        status
    }
}

fn platform_label(platform: &str) -> &str {
    match platform {
        "pc" => "PC",
        "ps4" => "PlayStation",
        "xb1" => "Xbox",
        "swi" => "Switch",
        other => other,
    }
}

fn stats(row: &Row) -> TradeStats {
    let e = &row.entry;
    TradeStats {
        median: e.median,
        avg: e.avg,
        stddev: e.stddev,
        min: e.min,
        max: e.max,
        pop: e.pop,
    }
}

/// A report's `pop` is relative to its most traded riven, rerolled or not. A weapon's
/// popularity adds up both of its rivens', scaled so the most traded weapon is 100.
fn trade_popularity(rows: &[Row]) -> HashMap<String, u8> {
    let Some(newest) = rows
        .iter()
        .filter(|r| r.platform == "pc")
        .map(|r| r.date)
        .max()
    else {
        return HashMap::new();
    };
    let mut sums: HashMap<String, f64> = HashMap::new();
    for row in rows
        .iter()
        .filter(|r| r.platform == "pc" && r.date == newest)
    {
        if let Some(name) = &row.entry.compatibility {
            *sums.entry(name.to_lowercase()).or_default() += row.entry.pop;
        }
    }
    let top = sums.values().copied().fold(0.0, f64::max);
    if top <= 0.0 {
        return HashMap::new();
    }
    sums.into_iter()
        .map(|(name, sum)| (name, (sum / top * 100.0).round().clamp(1.0, 100.0) as u8))
        .collect()
}

fn weapon_trades(rows: &[Row], name: &str, platform: &str) -> Option<WeaponTrades> {
    if rows.is_empty() {
        return None;
    }
    let platform = PLATFORMS
        .iter()
        .find(|(p, _)| *p == platform)
        .map_or("pc", |(p, _)| p);
    let name = name.trim().to_lowercase();
    let of_weapon = |row: &&Row| {
        row.entry
            .compatibility
            .as_deref()
            .is_some_and(|c| c.to_lowercase() == name)
    };
    let platforms = PLATFORMS
        .iter()
        .filter(|(p, _)| rows.iter().filter(of_weapon).any(|r| r.platform == *p))
        .map(|(p, _)| (*p).to_owned())
        .collect();
    let mut weeks: BTreeMap<NaiveDate, TradeWeek> = BTreeMap::new();
    for row in rows
        .iter()
        .filter(of_weapon)
        .filter(|r| r.platform == platform)
    {
        let week = weeks.entry(row.date).or_insert_with(|| TradeWeek {
            date: row.date.to_string(),
            unrolled: None,
            rerolled: None,
        });
        let slot = if row.entry.rerolled {
            &mut week.rerolled
        } else {
            &mut week.unrolled
        };
        *slot = Some(stats(row));
    }
    Some(WeaponTrades {
        platform: platform.to_owned(),
        platforms,
        weeks: weeks.into_values().rev().take(WEEKS_SHOWN).collect(),
        latest_report: rows
            .iter()
            .filter(|r| r.platform == platform)
            .map(|r| r.date)
            .max()
            .map(|d| d.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wfmt_dataset::weekly::Entry;

    fn row(platform: &str, day: u32, weapon: Option<&str>, rerolled: bool, median: f64) -> Row {
        Row {
            date: NaiveDate::from_ymd_opt(2026, 11, day).unwrap(),
            platform: platform.into(),
            published_at: None,
            entry: Entry {
                item_type: "Rifle Riven Mod".into(),
                compatibility: weapon.map(Into::into),
                rerolled,
                avg: median,
                stddev: 0.0,
                min: median,
                max: median,
                // Popular as it is dear, to tell the rows apart.
                pop: median,
                median,
            },
        }
    }

    #[test]
    fn a_weapons_weeks_are_found_by_name_newest_first() {
        let rows = [
            row("pc", 9, Some("AX-52"), false, 20.0),
            row("pc", 16, Some("AX-52"), false, 30.0),
            row("pc", 16, Some("AX-52"), true, 100.0),
            row("pc", 16, None, false, 8.0),
            row("ps4", 16, Some("AX-52"), true, 50.0),
            row("pc", 16, Some("Acceltra"), false, 90.0),
        ];
        let trades = weapon_trades(&rows, "ax-52", "pc").unwrap();
        assert_eq!(trades.platforms, ["pc", "ps4"]);
        assert_eq!(trades.latest_report.as_deref(), Some("2026-11-16"));
        let medians: Vec<_> = trades
            .weeks
            .iter()
            .map(|w| {
                (
                    w.date.as_str(),
                    w.unrolled.as_ref().map(|s| s.median),
                    w.rerolled.as_ref().map(|s| s.median),
                )
            })
            .collect();
        assert_eq!(
            medians,
            [
                ("2026-11-16", Some(30.0), Some(100.0)),
                ("2026-11-09", Some(20.0), None)
            ]
        );

        // AX-52 traded 30 + 100, Acceltra 90.
        let popularity = trade_popularity(&rows);
        assert_eq!(popularity.get("ax-52"), Some(&100));
        assert_eq!(popularity.get("acceltra"), Some(&69));
        assert_eq!(popularity.len(), 2);

        let switch = weapon_trades(&rows, "AX-52", "swi").unwrap();
        assert!(switch.weeks.is_empty() && switch.latest_report.is_none());
        assert_eq!(weapon_trades(&rows, "AX-52", "mac").unwrap().platform, "pc");
        assert_eq!(weapon_trades(&[], "AX-52", "pc"), None);
    }
}
