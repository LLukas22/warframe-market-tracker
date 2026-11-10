//! Scraper status and control messages, shared by the scraper engine and the web UI.
//!
//! The engine publishes [`ScraperStatus`] as JSON at [`STATUS_FILE`] and picks up
//! [`Command`]s dropped as JSON files into [`COMMAND_DIR`]. Files outlive the engine, so the
//! UI still has something to show while it restarts. All paths are relative to the data directory, all times are unix ms.

use serde::{Deserialize, Serialize};

/// Where the project lives, and how warframe.market can find out who's behind our requests.
#[macro_export]
macro_rules! repository {
    () => {
        "https://github.com/LLukas22/warframe-market-tracker"
    };
}
pub const REPOSITORY: &str = repository!();

/// Sent with every request to warframe.market, whose rules ask each client to identify itself
/// with a name, version and a link: <https://docs.warframe.market/docs/rules/overview/>.
pub const USER_AGENT: &str = concat!(
    "warframe-market-tracker/",
    env!("CARGO_PKG_VERSION"),
    " (+",
    repository!(),
    ")"
);

pub const STATUS_FILE: &str = "scraper/status.json";
pub const COMMAND_DIR: &str = "scraper/commands";
pub const LOCK_FILE: &str = "scraper/lock";

/// The engine rewrites its status at least this often, so older status means it's not running.
pub const HEARTBEAT_MS: i64 = 15_000;

/// warframe.market's site-wide "most recent" listings. Feed ids name the raw archive, the fetch
/// log entry and the `source` of the rows they produce.
pub const FEED_RECENT: &str = "feed_recent";
/// warframe.market's site-wide "popular" listings.
pub const FEED_POPULAR: &str = "feed_popular";

/// Whether an archive name, log entry or row source is a feed rather than a weapon search.
pub fn is_feed(id: &str) -> bool {
    id == FEED_RECENT || id == FEED_POPULAR
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Command {
    /// Start a sweep over every weapon now (ignored while a sweep runs).
    ScrapeAll,
    /// Stop the running sweep after aborting the current request.
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Idle,
    /// Checking / downloading the weapon list.
    Metadata,
    /// Compacting closed Parquet partitions and pruning committed raw archives.
    Maintenance,
    Scraping,
    /// A fetch failed; waiting before the next one.
    Cooldown,
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Trigger {
    Schedule,
    Manual,
    /// No longer started, but a sweep saved by an older version can still have it.
    Weapons,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurrentFetch {
    pub weapon: String,
    pub started_at: i64,
    /// e.g. "price_desc page", "verifying 3 auctions".
    pub step: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepProgress {
    pub started_at: i64,
    pub trigger: Trigger,
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    pub auctions: u32,
    pub gone: u32,
    pub current: Option<CurrentFetch>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepSummary {
    pub started_at: i64,
    pub finished_at: i64,
    pub trigger: Trigger,
    pub total: u32,
    pub done: u32,
    pub failed: u32,
    pub auctions: u32,
    pub gone: u32,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FetchLog {
    pub t: i64,
    pub weapon: String,
    pub duration_ms: i64,
    pub pages: u32,
    pub auctions: u32,
    pub gone: u32,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub sweep_interval_hours: f64,
    pub request_interval_secs: f64,
    pub sort_by: Vec<String>,
    pub verify_missing: usize,
    pub weapons: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScraperStatus {
    pub pid: u32,
    pub started_at: i64,
    pub updated_at: i64,
    pub phase: Phase,
    pub sweep: Option<SweepProgress>,
    pub next_sweep_at: Option<i64>,
    pub last_sweep: Option<SweepSummary>,
    /// Newest first.
    pub recent: Vec<FetchLog>,
    /// Requests to warframe.market since the engine started.
    pub requests: u64,
    pub settings: Settings,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    #[test]
    fn user_agent_names_the_project_version_and_repository() {
        assert_eq!(
            super::USER_AGENT,
            format!(
                "warframe-market-tracker/{} (+https://github.com/LLukas22/warframe-market-tracker)",
                env!("CARGO_PKG_VERSION")
            )
        );
    }
}
