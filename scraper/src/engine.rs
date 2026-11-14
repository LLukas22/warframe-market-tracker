//! The scraping engine: runs sweeps over all weapons on a schedule, takes commands, and
//! publishes its status. Runs inside the web server.
//!
//! A *sweep* fetches a queue of weapons one by one. Scheduled sweeps start every
//! `--sweep-interval-hours` and skip weapons fetched in the last half interval; manual sweeps
//! fetch everything. The queue is persisted in `state.json`, so a restart resumes the sweep.
//!
//! Control goes through files (see `wfmt_types`), so any process can watch and steer the engine.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use clap::Args;
use tokio::sync::watch;
use wfmt_types::{
    COMMAND_DIR, Command, CurrentFetch, Feed, FetchLog, HOUR_MS, LOCK_FILE, Phase, STATUS_FILE,
    ScraperStatus, Settings, SortBy, SweepProgress, SweepSummary, Trigger,
};

use crate::archive::{Meta, PersistedSweep, RawPage, RawResponse, State, Store, Verified};
use crate::normalize::Normalizer;
use crate::pipeline::Pipeline;
use crate::tracking::Tracker;
use crate::wfm::{self, BuyoutPolicy, Client, Weapon};
use wfmt_dataset::identity::Registry;
use wfmt_dataset::maintenance::{self, MaintenanceArgs};
use wfmt_dataset::sink::ParquetSink;

const RECENT: usize = 50;
const ERROR_COOLDOWN: Duration = Duration::from_secs(60);
const FEED_PRUNE_INTERVAL: Duration = Duration::from_hours(1);
/// How often the status is republished while maintenance blocks the main loop.
const MAINTENANCE_STATUS_INTERVAL: Duration = Duration::from_secs(5);
/// Wait after a failed weapon list download before trying again.
const METADATA_RETRY_DELAY: TimeDelta = TimeDelta::minutes(10);
/// Scheduled sweeps skip weapons fetched within this share of the sweep interval, by a manual
/// sweep for example.
const SWEEP_FRESH_SHARE: f64 = 0.5;

#[derive(Args, Clone, Debug)]
pub struct EngineArgs {
    #[command(flatten)]
    pub storage: MaintenanceArgs,
    #[arg(long, env = "WFMT_PLATFORM", default_value = "pc")]
    pub platform: String,

    /// Minimum seconds between two requests to warframe.market.
    #[arg(long, env = "WFMT_REQUEST_INTERVAL_SECS", default_value_t = 5.0)]
    pub request_interval_secs: f64,

    /// Hours between scheduled sweeps over all weapons.
    #[arg(long, env = "WFMT_SWEEP_INTERVAL_HOURS", default_value_t = 4.0)]
    pub sweep_interval_hours: f64,

    /// Seconds between polls of the site-wide "most recent" listings. 0 disables.
    #[arg(long, env = "WFMT_RECENT_INTERVAL_SECS", default_value_t = 180)]
    pub recent_interval_secs: u64,

    /// Seconds between polls of the site-wide "popular" listings. 0 disables.
    #[arg(long, env = "WFMT_POPULAR_INTERVAL_SECS", default_value_t = 600)]
    pub popular_interval_secs: u64,

    /// Hours to keep raw feed responses, checked hourly. 0 keeps them as long as other raw
    /// responses (`WFMT_RAW_RETENTION_DAYS`).
    #[arg(long, env = "WFMT_FEED_RAW_RETENTION_HOURS", default_value_t = 6)]
    pub feed_raw_retention_hours: u64,

    /// Hours between checks of the riven manifest version (weapon list).
    #[arg(long, env = "WFMT_METADATA_INTERVAL_HOURS", default_value_t = 24.0)]
    pub metadata_interval_hours: f64,

    /// Write a parquet part after this many weapons. Readers only see written parts, so this
    /// sets how live the web UI is during a sweep (about 10 s per weapon).
    #[arg(long, env = "WFMT_FLUSH_EVERY", default_value_t = 3)]
    pub flush_every: usize,

    /// Sort orders to fetch, in order. Listings missing from a fetch are only marked gone when
    /// both `price_asc` and `price_desc` returned the same short page.
    #[arg(
        long,
        env = "WFMT_SORT_BY",
        value_delimiter = ',',
        default_values = ["price_asc", "price_desc"]
    )]
    pub sort_by: Vec<SortBy>,

    /// Per weapon fetch, ask warframe.market whether up to this many listings that dropped out
    /// of the pages still exist, most recently seen first (one request each). Without checks
    /// they count as listed until they expire after four days. 0 disables.
    #[arg(long, env = "WFMT_VERIFY_MISSING", default_value_t = 2)]
    pub verify_missing: usize,

    /// Do not archive raw responses (disables `reprocess`).
    #[arg(long)]
    pub no_raw: bool,

    /// Only track these weapon slugs (comma separated).
    #[arg(long, env = "WFMT_WEAPONS", value_delimiter = ',')]
    pub weapons: Vec<String>,
}

impl EngineArgs {
    /// Defaults, overridden by `WFMT_*` environment variables, or what's wrong with those.
    pub fn from_env() -> Result<Self> {
        #[derive(clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: EngineArgs,
        }
        let parsed = <Wrapper as clap::Parser>::try_parse_from(["engine"])
            .map_err(|e| anyhow::anyhow!("{}", e.render().to_string().trim()))?;
        Ok(parsed.args)
    }
}

fn fatal(error: FetchError) -> anyhow::Error {
    match error {
        FetchError::Remote(e) | FetchError::Storage(e) => e,
    }
}

fn hours(h: f64) -> TimeDelta {
    TimeDelta::milliseconds((h * HOUR_MS as f64) as i64)
}

fn ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

/// Held for the lifetime of the engine; only one engine per data directory.
pub struct Lock {
    _file: File,
}

/// Takes the engine lock, or returns `None` if another process holds it.
pub fn try_lock(data_dir: &Path) -> Result<Option<Lock>> {
    let path = data_dir.join(LOCK_FILE);
    fs::create_dir_all(path.parent().unwrap())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(Lock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}

/// Queues a command for whichever engine runs on this data directory.
pub fn send_command(data_dir: &Path, command: &Command) -> Result<()> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = data_dir.join(COMMAND_DIR);
    fs::create_dir_all(&dir)?;
    let name = format!(
        "{}-{}-{}",
        Utc::now().timestamp_millis(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, serde_json::to_vec(command)?)?;
    fs::rename(tmp, dir.join(format!("{name}.json")))?;
    Ok(())
}

// Polled by the engine itself: no task can outlive it and consume a successor's commands.
struct Commands {
    dir: PathBuf,
    pending: VecDeque<Command>,
    poll: tokio::time::Interval,
}

impl Commands {
    fn new(dir: PathBuf) -> Self {
        let mut poll = tokio::time::interval(Duration::from_secs(1));
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        Self {
            dir,
            pending: VecDeque::new(),
            poll,
        }
    }

    fn try_recv(&mut self) -> Option<Command> {
        self.pending.pop_front()
    }

    async fn recv(&mut self) -> Option<Command> {
        loop {
            if let Some(command) = self.try_recv() {
                return Some(command);
            }
            self.poll.tick().await;
            self.pending.extend(take_commands(&self.dir));
        }
    }
}

fn take_commands(data_dir: &Path) -> Vec<Command> {
    let Ok(entries) = fs::read_dir(data_dir.join(COMMAND_DIR)) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    files
        .into_iter()
        .filter_map(|path| {
            let command = fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok());
            let _ = fs::remove_file(&path);
            if command.is_none() {
                tracing::warn!(path = %path.display(), "ignoring unreadable command");
            }
            command
        })
        .collect()
}

pub fn read_status(data_dir: &Path) -> Option<ScraperStatus> {
    let bytes = fs::read(data_dir.join(STATUS_FILE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

struct FetchResult {
    pages: u32,
    auctions: u32,
    gone: u32,
}

/// Why a fetch failed, which decides what the engine does next.
#[derive(Debug)]
enum FetchError {
    /// warframe.market failed or sent something unusable. Logged, and retried later.
    Remote(anyhow::Error),
    /// Archiving or processing a response failed. Fatal: the engine stops without advancing
    /// its state, and a restart replays the raw journal.
    Storage(anyhow::Error),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Remote(e) | Self::Storage(e) => write!(f, "{e:#}"),
        }
    }
}

impl std::error::Error for FetchError {}

type FetchOutcome = std::result::Result<FetchResult, FetchError>;

pub struct Engine {
    args: EngineArgs,
    data_dir: PathBuf,
    client: Client,
    store: Store,
    sink: ParquetSink,
    pipeline: Pipeline,
    meta: Meta,
    state: State,
    weapons: Vec<Weapon>,
    since_flush: usize,
    status: ScraperStatus,
    /// What `status.error` is about: a feed, a weapon or a step.
    error_source: Option<String>,
}

impl Engine {
    pub fn new(data_dir: PathBuf, rework_cutoff: DateTime<Utc>, args: EngineArgs) -> Result<Self> {
        // Complete interrupted publication before loading tracking/identity state.
        drop(wfmt_dataset::DatasetLock::write(&data_dir)?);
        let store = Store::new(data_dir.clone());
        let client = Client::new(
            args.platform.clone(),
            Duration::from_secs_f64(args.request_interval_secs),
        )?;
        let now = ms(Utc::now());
        let state = store.load_state()?;
        let mut engine = Self {
            sink: ParquetSink::new(store.root().join(wfmt_dataset::layout::PARQUET_DIR)),
            meta: store.load_meta()?,
            pipeline: Pipeline {
                normalizer: Normalizer::new(rework_cutoff),
                registry: Registry::open(store.registry_path())?,
                tracker: Tracker::open(store.seen_dir()),
            },
            weapons: Vec::new(),
            since_flush: 0,
            error_source: None,
            status: ScraperStatus {
                pid: std::process::id(),
                started_at: now,
                updated_at: now,
                phase: Phase::Idle,
                sweep: None,
                next_sweep_at: None,
                last_sweep: state.last_sweep.clone(),
                recent: Vec::new(),
                requests: 0,
                settings: Settings {
                    sweep_interval_hours: args.sweep_interval_hours,
                    request_interval_secs: args.request_interval_secs,
                    sort_by: args.sort_by.clone(),
                    verify_missing: args.verify_missing,
                    weapons: 0,
                },
                error: None,
            },
            state,
            data_dir,
            client,
            store,
            args,
        };
        if let Some(body) = engine.store.load_weapons_raw()? {
            engine.weapons = engine.track(wfm::parse_weapons(&body)?);
        }
        engine.recover_pending()?;
        Ok(engine)
    }

    fn track(&self, mut weapons: Vec<Weapon>) -> Vec<Weapon> {
        if !self.args.weapons.is_empty() {
            weapons.retain(|w| self.args.weapons.contains(&w.slug));
        }
        weapons
    }

    fn publish(&mut self) {
        let now = Utc::now();
        self.status.updated_at = ms(now);
        self.status.requests = self.client.requests();
        self.status.sweep = self.state.sweep.as_ref().map(|s| s.progress.clone());
        self.status.next_sweep_at = Some(ms(self.next_sweep_at()));
        self.status.last_sweep = self.state.last_sweep.clone();
        self.status.settings.weapons = self.weapons.len();
        let path = self.data_dir.join(STATUS_FILE);
        let write = || -> Result<()> {
            fs::create_dir_all(path.parent().unwrap())?;
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, serde_json::to_vec(&self.status)?)?;
            fs::rename(tmp, &path)?;
            Ok(())
        };
        if let Err(e) = write() {
            tracing::warn!(error = %e, "failed to write scraper status");
        }
    }

    fn set_phase(&mut self, phase: Phase) {
        self.status.phase = phase;
        self.publish();
    }

    fn set_step(&mut self, step: String) {
        if let Some(current) = self
            .state
            .sweep
            .as_mut()
            .and_then(|s| s.progress.current.as_mut())
        {
            current.step = step;
        }
        self.publish();
    }

    fn save_state(&self) {
        if let Err(e) = self.store.save_state(&self.state) {
            tracing::error!(error = %e, "failed to save state");
        }
    }

    fn next_sweep_at(&self) -> DateTime<Utc> {
        let interval = hours(self.args.sweep_interval_hours);
        if let Some(t) = self.state.last_sweep_started {
            return t + interval;
        }
        // No sweep yet (fresh data dir, or state from the old per-weapon scheduler).
        self.weapons
            .iter()
            .map(|w| {
                self.state
                    .last_fetch
                    .get(&w.slug)
                    .map_or(DateTime::<Utc>::MIN_UTC, |t| *t + interval)
            })
            .min()
            .unwrap_or_else(Utc::now)
    }

    fn metadata_due(&self, now: DateTime<Utc>) -> bool {
        self.weapons.is_empty()
            || self
                .meta
                .checked_at
                .is_none_or(|t| now - t >= hours(self.args.metadata_interval_hours))
    }

    /// Checks the riven manifest version and only downloads the weapon list when it changed.
    async fn refresh_metadata(&mut self) -> Result<()> {
        let version = self.client.rivens_version().await?;
        let cached = self.store.load_weapons_raw()?;
        let body = match cached {
            Some(body) if self.meta.rivens_version.as_deref() == Some(version.as_str()) => body,
            _ => {
                tracing::info!(%version, "riven manifest changed, downloading weapon list");
                let body = self.client.riven_weapons_raw().await?;
                self.store.save_weapons_raw(&body)?;
                body
            }
        };
        self.weapons = self.track(wfm::parse_weapons(&body)?);
        self.meta = Meta {
            rivens_version: Some(version),
            checked_at: Some(Utc::now()),
        };
        self.store.save_meta(&self.meta)?;
        tracing::info!(weapons = self.weapons.len(), "weapon list ready");
        Ok(())
    }

    fn start_sweep(&mut self, trigger: Trigger, queue: Vec<String>) {
        let now = Utc::now();
        self.state.last_sweep_started = Some(now);
        if queue.is_empty() {
            self.save_state();
            return;
        }
        tracing::info!(?trigger, weapons = queue.len(), "sweep started");
        self.state.sweep = Some(PersistedSweep {
            progress: SweepProgress {
                started_at: ms(now),
                trigger,
                total: queue.len() as u32,
                done: 0,
                failed: 0,
                auctions: 0,
                gone: 0,
                current: None,
            },
            queue: queue.into(),
        });
        self.save_state();
        self.set_phase(Phase::Scraping);
    }

    fn start_scheduled(&mut self) {
        let now = Utc::now();
        let fresh = now - hours(self.args.sweep_interval_hours * SWEEP_FRESH_SHARE);
        let mut due: Vec<(Option<DateTime<Utc>>, String)> = self
            .weapons
            .iter()
            .map(|w| (self.state.last_fetch.get(&w.slug).copied(), w.slug.clone()))
            .filter(|(t, _)| t.is_none_or(|t| t < fresh))
            .collect();
        due.sort();
        self.start_sweep(Trigger::Schedule, due.into_iter().map(|(_, s)| s).collect());
    }

    fn finish_sweep(&mut self, cancelled: bool) -> Result<()> {
        self.sink.flush()?;
        let Some(sweep) = self.state.sweep.take() else {
            return Ok(());
        };
        let p = sweep.progress;
        tracing::info!(done = p.done, total = p.total, cancelled, "sweep finished");
        self.state.last_sweep = Some(SweepSummary {
            started_at: p.started_at,
            finished_at: ms(Utc::now()),
            trigger: p.trigger,
            total: p.total,
            done: p.done,
            failed: p.failed,
            auctions: p.auctions,
            gone: p.gone,
            cancelled,
        });
        self.save_state();
        self.since_flush = 0;
        self.set_phase(Phase::Idle);
        Ok(())
    }

    fn handle(&mut self, command: Command) -> Result<()> {
        tracing::info!(?command, "command");
        match command {
            Command::ScrapeAll => {
                if self.state.sweep.is_none() {
                    let all = self.weapons.iter().map(|w| w.slug.clone()).collect();
                    self.start_sweep(Trigger::Manual, all);
                }
            }
            Command::Cancel => self.finish_sweep(true)?,
        }
        Ok(())
    }

    /// Replay only new, uncommitted archives, before fetching any newer observations.
    fn recover_pending(&mut self) -> Result<()> {
        self.sink.flush()?;
        let pending = self.store.pending_raw()?;
        if pending.is_empty() {
            return Ok(());
        }
        let body = self.store.load_weapons_raw()?.ok_or_else(|| {
            anyhow::anyhow!("pending raw responses require the cached weapon list")
        })?;
        // Recovery includes weapons excluded by the current `WFMT_WEAPONS` filter, except for
        // feeds, which return every weapon no matter what was being tracked.
        let weapons = wfm::parse_weapons(&body)?;
        let tracked = self.track(weapons.clone());
        for raw in pending {
            if raw.feed().is_some() {
                tracing::info!(feed = %raw.weapon, "recovering uncommitted raw fetch");
                self.pipeline.process_feed(&tracked, &raw, &mut self.sink)?;
                self.sink.acknowledge_raw(raw.file_name());
                self.sink.flush()?;
                continue;
            }
            let weapon = weapons
                .iter()
                .find(|w| w.slug == raw.weapon)
                .ok_or_else(|| {
                    anyhow::anyhow!("pending raw weapon {} missing from cache", raw.weapon)
                })?;
            tracing::info!(weapon = %raw.weapon, "recovering uncommitted raw fetch");
            self.pipeline.process(weapon, &raw, &mut self.sink)?;
            self.sink.acknowledge_raw(raw.file_name());
            self.sink.flush()?;
        }
        Ok(())
    }

    /// Archive each response before parsing it or awaiting another network operation.
    fn archive_progress(&self, raw: &RawResponse) -> Result<()> {
        if !self.args.no_raw {
            self.store.save_raw(raw)?;
        }
        Ok(())
    }

    async fn fetch(&mut self, weapon: &Weapon) -> FetchOutcome {
        let mut raw = RawResponse::new(
            Utc::now(),
            weapon.slug.clone(),
            self.client.platform().to_owned(),
            Vec::new(),
            Vec::new(),
        );
        raw.complete = false;
        let mut fetch_error = None;
        let mut null_buyouts_descending = false;
        for sort_by in self.args.sort_by.clone() {
            self.set_step(format!("{sort_by} page"));
            match self.client.auctions_raw(&weapon.slug, sort_by).await {
                Ok(body) => {
                    let parsed = wfm::parse_auctions(&body);
                    null_buyouts_descending |= sort_by == SortBy::PriceDesc
                        && parsed.as_ref().is_ok_and(|(auctions, _)| {
                            auctions.iter().any(|a| a.buyout_price.is_none())
                        });
                    raw.pages.push(RawPage {
                        sort_by: sort_by.into(),
                        buyout_policy: None,
                        body,
                    });
                    // Archived even when it doesn't parse, as a record of what came back.
                    self.archive_progress(&raw).map_err(FetchError::Storage)?;
                    // Even a short page needs the other orders: only both price orders
                    // returning the same listings proves the search returned everything.
                    if let Err(e) = parsed {
                        fetch_error = Some(e);
                        break;
                    }
                }
                Err(e) => {
                    fetch_error = Some(e);
                    break;
                }
            }
        }
        if raw.pages.is_empty() {
            return Err(FetchError::Remote(fetch_error.unwrap_or_else(|| {
                anyhow::anyhow!("no search sort orders configured")
            })));
        }
        // Null buyouts sort before expensive buyouts on descending pages. On popular
        // weapons they can occupy the entire cap, starving price analysis of good rolls.
        // Capped responses can contain only 98–99 entries. Only proven complete
        // coverage lets us skip supplementation when descending pages contain null buyouts.
        let coverage = crate::pipeline::parse_pages(&raw.pages)
            .map_err(FetchError::Remote)?
            .coverage;
        let needs_buyouts = !coverage.is_complete() && null_buyouts_descending;
        if fetch_error.is_none() && needs_buyouts {
            // The API separates direct sales from bidding auctions with buyouts.
            for policy in [BuyoutPolicy::Direct, BuyoutPolicy::With] {
                self.set_step(format!("price_desc {} buyout page", policy.as_str()));
                match self
                    .client
                    .auctions_raw_scoped(&weapon.slug, SortBy::PriceDesc, Some(policy))
                    .await
                {
                    Ok(body) => {
                        let parsed = wfm::parse_auctions(&body);
                        raw.pages.push(RawPage {
                            sort_by: SortBy::PriceDesc.into(),
                            buyout_policy: Some(policy),
                            body,
                        });
                        self.archive_progress(&raw).map_err(FetchError::Storage)?;
                        if let Err(e) = parsed {
                            fetch_error = Some(e);
                        }
                    }
                    Err(e) => fetch_error = Some(e),
                }
                if fetch_error.is_some() {
                    break;
                }
            }
        }
        raw.complete = fetch_error.is_none();
        if raw.complete && self.args.verify_missing > 0 {
            let uncertain = self
                .pipeline
                .uncertain(&weapon.slug, &raw)
                .map_err(FetchError::Remote)?;
            for (i, id) in uncertain
                .into_iter()
                .take(self.args.verify_missing)
                .enumerate()
            {
                self.set_step(format!("verifying auction {}", i + 1));
                match self.client.auction_exists(&id).await {
                    Ok(exists) => {
                        raw.verified.push(Verified {
                            auction_id: id,
                            exists,
                        });
                        self.archive_progress(&raw).map_err(FetchError::Storage)?;
                    }
                    Err(e) => tracing::warn!(auction = %id, error = %e, "verify failed"),
                }
            }
        }
        let summary = self.process(weapon, &raw).map_err(FetchError::Storage)?;
        tracing::info!(weapon = %weapon.slug, pages = raw.pages.len(),
            auctions = summary.auctions, complete = summary.complete,
            fitted = summary.fitted, gone = summary.gone, skipped = summary.skipped, "fetched");
        if let Some(error) = fetch_error {
            return Err(FetchError::Remote(error));
        }
        Ok(FetchResult {
            pages: raw.pages.len() as u32,
            auctions: summary.auctions as u32,
            gone: summary.gone as u32,
        })
    }

    /// Archives and processes a fetched weapon.
    fn process(&mut self, weapon: &Weapon, raw: &RawResponse) -> Result<crate::pipeline::Summary> {
        self.archive_progress(raw)?;
        let summary = self.pipeline.process(weapon, raw, &mut self.sink)?;
        if self.args.no_raw {
            // Without a raw journal, publish before the scheduler can persist progress.
            self.sink.flush()?;
        } else {
            self.sink.acknowledge_raw(raw.file_name());
        }
        Ok(summary)
    }

    /// Polls one site-wide feed and publishes what it found right away, since the engine may
    /// sit idle for hours afterwards.
    async fn fetch_feed(&mut self, feed: Feed) -> FetchOutcome {
        let remote = async {
            let body = self.client.feed_raw(feed).await?;
            wfm::parse_auctions(&body)?;
            anyhow::Ok(body)
        };
        let body = remote.await.map_err(FetchError::Remote)?;
        let raw = RawResponse::new(
            Utc::now(),
            feed.as_str().to_owned(),
            self.client.platform().to_owned(),
            vec![RawPage {
                sort_by: feed.into(),
                buyout_policy: None,
                body,
            }],
            Vec::new(),
        );
        let summary = self.process_feed(&raw).map_err(FetchError::Storage)?;
        tracing::info!(
            feed = feed.as_str(),
            auctions = summary.auctions,
            skipped = summary.skipped,
            "fetched feed"
        );
        Ok(FetchResult {
            pages: 1,
            auctions: summary.auctions as u32,
            gone: 0,
        })
    }

    /// Archives, processes and publishes a fetched feed.
    fn process_feed(&mut self, raw: &RawResponse) -> Result<crate::pipeline::Summary> {
        self.archive_progress(raw)?;
        let summary = self
            .pipeline
            .process_feed(&self.weapons, raw, &mut self.sink)?;
        if !self.args.no_raw {
            self.sink.acknowledge_raw(raw.file_name());
        }
        self.sink.flush()?;
        self.since_flush = 0;
        Ok(summary)
    }

    /// Shows `error` in the status, as being about `source`.
    fn set_error(&mut self, source: &str, error: impl std::fmt::Display) {
        self.status.error = Some(format!("{source}: {error:#}"));
        self.error_source = Some(source.to_owned());
    }

    fn clear_error(&mut self) {
        self.status.error = None;
        self.error_source = None;
    }

    fn push_log(&mut self, name: &str, started: Instant, result: &FetchOutcome) {
        let entry = FetchLog {
            t: ms(Utc::now()),
            weapon: name.to_owned(),
            duration_ms: started.elapsed().as_millis() as i64,
            pages: result.as_ref().map_or(0, |r| r.pages),
            auctions: result.as_ref().map_or(0, |r| r.auctions),
            gone: result.as_ref().map_or(0, |r| r.gone),
            error: result.as_ref().err().map(ToString::to_string),
        };
        self.status.recent.insert(0, entry);
        self.status.recent.truncate(RECENT);
    }

    fn log_fetch(&mut self, weapon: &str, started: Instant, result: &FetchOutcome) {
        self.push_log(weapon, started, result);
        if let Some(sweep) = self.state.sweep.as_mut() {
            let p = &mut sweep.progress;
            p.done += 1;
            p.current = None;
            match result {
                Ok(r) => {
                    p.auctions += r.auctions;
                    p.gone += r.gone;
                }
                Err(_) => p.failed += 1,
            }
        }
    }

    /// Runs until `shutdown` fires.
    pub async fn run(mut self, shutdown: watch::Receiver<bool>) -> Result<()> {
        let mut inbox = Inbox {
            commands: Commands::new(self.data_dir.clone()),
            deferred: VecDeque::new(),
            shutdown,
        };
        // Drop stale commands from before this engine started.
        take_commands(&self.data_dir);
        if let Some(sweep) = &self.state.sweep {
            tracing::info!(remaining = sweep.queue.len(), "resuming sweep");
            self.status.phase = Phase::Scraping;
        }
        self.publish();

        let mut schedule = Schedule::new(&self.args);
        while !*inbox.shutdown.borrow() {
            if let Next::Stop = self.step(&mut inbox, &mut schedule).await? {
                break;
            }
        }

        tracing::info!("scraper stopping");
        self.sink.flush()?;
        self.save_state();
        self.status.phase = Phase::Stopped;
        self.publish();
        Ok(())
    }

    /// One turn of the main loop: the most urgent of maintenance, the weapon list, commands,
    /// a feed poll, the next weapon of a sweep, or waiting.
    async fn step(&mut self, inbox: &mut Inbox, schedule: &mut Schedule) -> Result<Next> {
        if Instant::now() >= schedule.next_maintenance {
            self.maintain().await?;
            schedule.next_maintenance =
                Instant::now() + Duration::from_hours(self.args.storage.maintenance_interval_hours);
            return Ok(Next::Continue);
        }
        if self.args.feed_raw_retention_hours > 0 && Instant::now() >= schedule.next_feed_prune {
            schedule.next_feed_prune = Instant::now() + FEED_PRUNE_INTERVAL;
            self.prune_feeds();
        }
        while let Some(command) = inbox.commands.try_recv() {
            inbox.deferred.push_back(command);
        }
        let now = Utc::now();
        let sweep_due = self.state.sweep.is_none() && now >= self.next_sweep_at();

        // Make sure the weapon list is fresh before anything that needs it.
        let needs_weapons = self.state.sweep.is_some()
            || sweep_due
            || !inbox.deferred.is_empty()
            || self.weapons.is_empty();
        let retry_metadata = schedule
            .metadata_failed_at
            .is_none_or(|t| now - t > METADATA_RETRY_DELAY);
        if needs_weapons && self.metadata_due(now) && retry_metadata {
            return Ok(self.update_weapons(inbox, schedule, now).await);
        }

        while let Some(command) = inbox.deferred.pop_front() {
            self.handle(command)?;
        }

        // At most one feed between weapons, even if multiple slow feeds are overdue.
        let sweep_waiting = self.state.sweep.is_some() || sweep_due;
        if !self.weapons.is_empty()
            && !(sweep_waiting && schedule.feed_since_weapon)
            && let Some(feed) = schedule.due_feed()
        {
            schedule.feed_since_weapon = true;
            self.poll_feed(inbox, &mut schedule.feeds[feed]).await?;
            return Ok(Next::Continue);
        }

        if self.state.sweep.is_none() {
            if sweep_due && !self.weapons.is_empty() {
                self.start_scheduled();
                return Ok(Next::Continue);
            }
            return Ok(self.idle(inbox, now).await);
        }
        self.next_weapon(inbox, schedule).await
    }

    /// Runs storage maintenance, publishing the status meanwhile. Commands stay queued, and
    /// shutdown waits for the local transaction to finish safely.
    async fn maintain(&mut self) -> Result<()> {
        self.sink.flush()?;
        self.set_phase(Phase::Maintenance);
        let dir = self.data_dir.clone();
        let args = self.args.storage.clone();
        let mut task =
            tokio::task::spawn_blocking(move || maintenance::maintain(&dir, Utc::now(), &args));
        let result = loop {
            tokio::select! {
                result = &mut task => break result?,
                _ = tokio::time::sleep(MAINTENANCE_STATUS_INTERVAL) => self.publish(),
            }
        };
        if let Err(e) = result {
            tracing::error!(error = %e, "storage maintenance failed");
            self.set_error("storage maintenance", e);
        }
        self.set_phase(self.working_phase());
        Ok(())
    }

    fn prune_feeds(&mut self) {
        let retention = hours(self.args.feed_raw_retention_hours as f64);
        match maintenance::prune_feeds(&self.data_dir, Utc::now(), retention) {
            Ok(deleted) => tracing::debug!(deleted, "pruned feed archives"),
            Err(e) => {
                tracing::error!(error = %e, "feed archive cleanup failed");
                self.set_error("feed archive cleanup", e);
            }
        }
    }

    /// Downloads the weapon list, unless shutdown comes first.
    async fn update_weapons(
        &mut self,
        inbox: &mut Inbox,
        schedule: &mut Schedule,
        now: DateTime<Utc>,
    ) -> Next {
        self.set_phase(Phase::Metadata);
        tokio::select! {
            result = self.refresh_metadata() => match result {
                Ok(()) => schedule.metadata_failed_at = None,
                Err(e) => {
                    tracing::warn!(error = %e, "metadata refresh failed");
                    schedule.metadata_failed_at = Some(now);
                    self.set_error("weapon list", e);
                }
            },
            _ = inbox.shutdown.changed() => return Next::Stop,
        }
        self.set_phase(self.working_phase());
        Next::Continue
    }

    /// Polls a feed. Fails only when storing what came back fails.
    async fn poll_feed(&mut self, inbox: &mut Inbox, feed: &mut FeedPoll) -> Result<()> {
        let started = Instant::now();
        let result = inbox.interruptible(self.fetch_feed(feed.id)).await;
        // Backoff can outlast the poll interval. Start the next interval after completion (or
        // cancellation), so a failed feed cannot immediately repeat.
        feed.next = Instant::now() + feed.interval;
        let result = match result {
            Ok(result) => result,
            // A backing-off poll must not hold up a cancel for minutes.
            Err(Interrupt::Cancel) => {
                inbox.deferred.push_back(Command::Cancel);
                return Ok(());
            }
            Err(Interrupt::Shutdown) => return Ok(()),
        };
        match &result {
            // Same as a weapon fetch: restart recovers the raw journal.
            Err(FetchError::Storage(_)) => return result.map(|_| ()).map_err(fatal),
            Ok(_) => {
                if self.error_source.as_deref() == Some(feed.id.as_str()) {
                    self.clear_error();
                }
            }
            Err(e) => {
                // No cooldown: the next poll is minutes away anyway.
                tracing::warn!(feed = feed.id.as_str(), error = %e, "feed fetch failed");
                self.set_error(feed.id.as_str(), e);
            }
        }
        self.push_log(feed.id.as_str(), started, &result);
        self.publish();
        Ok(())
    }

    /// Waits for commands, the next sweep, or the heartbeat.
    async fn idle(&mut self, inbox: &mut Inbox, now: DateTime<Utc>) -> Next {
        if self.status.phase != Phase::Idle {
            self.set_phase(Phase::Idle);
        }
        let heartbeat = TimeDelta::milliseconds(wfmt_types::HEARTBEAT_MS / 2);
        let wake = self.next_sweep_at().min(now + heartbeat);
        let sleep = tokio::time::sleep((wake - now).to_std().unwrap_or_default());
        tokio::select! {
            () = sleep => self.publish(),
            Some(command) = inbox.commands.recv() => inbox.deferred.push_back(command),
            _ = inbox.shutdown.changed() => return Next::Stop,
        }
        Next::Continue
    }

    /// Fetches the next weapon of the running sweep, or finishes the sweep.
    async fn next_weapon(&mut self, inbox: &mut Inbox, schedule: &mut Schedule) -> Result<Next> {
        let sweep = self
            .state
            .sweep
            .as_mut()
            .expect("only called during a sweep");
        let Some(slug) = sweep.queue.pop_front() else {
            self.finish_sweep(false)?;
            return Ok(Next::Continue);
        };
        schedule.feed_since_weapon = false;
        let Some(weapon) = self.weapons.iter().find(|w| w.slug == slug).cloned() else {
            tracing::warn!(weapon = %slug, "weapon no longer exists, skipping");
            sweep.progress.total = sweep.progress.total.saturating_sub(1);
            return Ok(Next::Continue);
        };
        sweep.progress.current = Some(CurrentFetch {
            weapon: slug.clone(),
            started_at: ms(Utc::now()),
            step: "starting".into(),
        });
        self.clear_error();
        self.set_phase(Phase::Scraping);

        let started = Instant::now();
        let result = match inbox.interruptible(self.fetch(&weapon)).await {
            Ok(result) => result,
            Err(Interrupt::Cancel) => {
                self.recover_pending()?;
                self.finish_sweep(true)?;
                return Ok(Next::Continue);
            }
            Err(Interrupt::Shutdown) => {
                // Put the weapon back so the sweep resumes with it.
                if let Some(s) = self.state.sweep.as_mut() {
                    s.queue.push_front(slug);
                    s.progress.current = None;
                }
                self.save_state();
                return Ok(Next::Stop);
            }
        };

        if let Err(FetchError::Storage(_)) = result {
            // Storage/processing failures are fatal: do not advance state or publish
            // partially updated in-memory tracking. Restart recovers the raw journal.
            return result.map(|_| Next::Stop).map_err(fatal);
        }
        if let Err(e) = &result {
            tracing::error!(weapon = %slug, error = %e, "fetch failed");
            self.set_error(&slug, e);
        }
        // Mark failures too, so a broken weapon is not retried in a tight loop.
        self.state.last_fetch.insert(slug.clone(), Utc::now());
        self.log_fetch(&slug, started, &result);
        self.save_state();
        self.since_flush += 1;
        if self.since_flush >= self.args.flush_every {
            self.sink.flush()?;
            self.since_flush = 0;
        }
        self.publish();

        if result.is_err() {
            self.set_phase(Phase::Cooldown);
            tokio::select! {
                () = tokio::time::sleep(ERROR_COOLDOWN) => {}
                Some(command) = inbox.commands.recv() => inbox.deferred.push_back(command),
                _ = inbox.shutdown.changed() => return Ok(Next::Stop),
            }
            self.set_phase(Phase::Scraping);
        }
        Ok(Next::Continue)
    }

    /// The phase between steps: scraping while a sweep runs, idle otherwise.
    fn working_phase(&self) -> Phase {
        if self.state.sweep.is_some() {
            Phase::Scraping
        } else {
            Phase::Idle
        }
    }
}

/// What the main loop does after a step.
enum Next {
    Continue,
    Stop,
}

/// What the running engine listens to.
struct Inbox {
    commands: Commands,
    /// Commands taken while busy, handled at the next chance.
    deferred: VecDeque<Command>,
    shutdown: watch::Receiver<bool>,
}

/// Why a fetch was abandoned.
enum Interrupt {
    Cancel,
    Shutdown,
}

impl Inbox {
    /// Runs `work` until it's done, or until a cancel or shutdown abandons it. Other commands
    /// that arrive meanwhile are kept for later.
    async fn interruptible<T>(
        &mut self,
        work: impl std::future::Future<Output = T>,
    ) -> std::result::Result<T, Interrupt> {
        tokio::pin!(work);
        loop {
            tokio::select! {
                result = &mut work => return Ok(result),
                Some(command) = self.commands.recv() => {
                    if command == Command::Cancel {
                        return Err(Interrupt::Cancel);
                    }
                    self.deferred.push_back(command);
                }
                _ = self.shutdown.changed() => return Err(Interrupt::Shutdown),
            }
        }
    }
}

/// A site-wide feed polled on its own interval.
struct FeedPoll {
    id: Feed,
    interval: Duration,
    next: Instant,
}

/// When things are due. In memory only: doing something once more after a restart costs next
/// to nothing.
struct Schedule {
    next_maintenance: Instant,
    next_feed_prune: Instant,
    metadata_failed_at: Option<DateTime<Utc>>,
    /// A feed was polled since the last weapon, so a waiting sweep goes next.
    feed_since_weapon: bool,
    feeds: Vec<FeedPoll>,
}

impl Schedule {
    fn new(args: &EngineArgs) -> Self {
        // Disabled feeds are dropped; the first poll happens as soon as the weapon list is
        // ready.
        let feeds = [
            (Feed::Recent, args.recent_interval_secs),
            (Feed::Popular, args.popular_interval_secs),
        ]
        .into_iter()
        .filter(|(_, secs)| *secs > 0)
        .map(|(id, secs)| FeedPoll {
            id,
            interval: Duration::from_secs(secs),
            next: Instant::now(),
        })
        .collect();
        Self {
            next_maintenance: Instant::now(),
            next_feed_prune: Instant::now(),
            metadata_failed_at: None,
            feed_since_weapon: false,
            feeds,
        }
    }

    /// The overdue feed with the oldest deadline, so one feed can't starve the other.
    fn due_feed(&self) -> Option<usize> {
        let now = Instant::now();
        (0..self.feeds.len())
            .filter(|&i| now >= self.feeds[i].next)
            .min_by_key(|&i| self.feeds[i].next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracking::PAGE_CAP;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn dropped_command_poll_cannot_consume_a_successors_commands() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let mut old = Commands::new(root.clone());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), old.recv())
                .await
                .is_err()
        );
        drop(old);
        send_command(&root, &Command::Cancel)?;
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let mut next = Commands::new(root.clone());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), next.recv()).await?,
            Some(Command::Cancel)
        );
        Ok(())
    }

    fn options(no_raw: bool) -> EngineArgs {
        EngineArgs {
            storage: MaintenanceArgs {
                raw_retention_days: 14,
                maintenance_interval_hours: 6,
                compact_target_mb: 128,
            },
            platform: "pc".into(),
            request_interval_secs: 0.0,
            sweep_interval_hours: 4.0,
            recent_interval_secs: 180,
            popular_interval_secs: 600,
            feed_raw_retention_hours: 6,
            metadata_interval_hours: 24.0,
            flush_every: 3,
            sort_by: vec![wfmt_types::SortBy::PriceAsc, wfmt_types::SortBy::PriceDesc],
            verify_missing: 0,
            no_raw,
            weapons: vec![],
        }
    }

    #[tokio::test]
    async fn archives_first_page_before_second_request_and_recovers_failed_fetch() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let cutoff = "2026-10-22T00:00:00Z".parse()?;
        let mut engine = Engine::new(root.clone(), cutoff, options(false))?;
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../tests/fixtures/kulstar_auctions.json.gz")[..],
        )
        .read_to_string(&mut body)?;
        assert_eq!(wfm::parse_auctions(&body)?.0.len(), PAGE_CAP);
        let listener = TcpListener::bind("127.0.0.1:0")?;
        engine
            .client
            .set_api(format!("http://{}", listener.local_addr()?));
        let server_root = root.clone();
        let server = std::thread::spawn(move || -> Result<()> {
            for i in 0..2 {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf)?;
                    anyhow::ensure!(n > 0, "request ended before headers");
                    request.extend_from_slice(&buf[..n]);
                }
                if i == 1 {
                    // The first response is durable before the client asks for page two.
                    let pending = Store::new(server_root.clone()).pending_raw()?;
                    assert_eq!(pending.len(), 1);
                    assert_eq!(pending[0].pages.len(), 1);
                    assert!(!pending[0].complete);
                }
                let (status, response) = if i == 0 {
                    ("200 OK", body.as_str())
                } else {
                    ("400 Bad Request", "{}")
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
            }
            Ok(())
        });
        let weapon = engine.weapons[0].clone();
        assert!(
            matches!(engine.fetch(&weapon).await, Err(FetchError::Remote(_))),
            "network failures remain retryable"
        );
        server.join().unwrap()?;
        assert_eq!(engine.sink.auctions.len(), PAGE_CAP);
        assert!(!store.seen_dir().join("kulstar.json").exists());
        drop(engine); // Crash before flushing the successful first page.
        drop(Engine::new(root.clone(), cutoff, options(false))?);
        assert!(store.pending_raw()?.is_empty());
        let seen: std::collections::HashMap<String, crate::tracking::Seen> =
            serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
        assert_eq!(seen.len(), PAGE_CAP);
        Ok(())
    }
    #[tokio::test]
    async fn running_engine_prunes_expired_feed_archives() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let raw = RawResponse::new(
            Utc::now() - TimeDelta::hours(7),
            wfmt_types::Feed::Recent.as_str().into(),
            "pc".into(),
            Vec::new(),
            Vec::new(),
        );
        store.save_raw(&raw)?;
        let mut sink = ParquetSink::new(root.join("parquet"));
        sink.prepare(raw.scraped_at)?;
        sink.acknowledge_raw(raw.file_name());
        sink.flush()?;

        let mut engine = Engine::new(
            root.clone(),
            "2026-10-22T00:00:00Z".parse()?,
            options(false),
        )?;
        // Nothing listens here: the cleanup must not depend on the API being reachable.
        engine.client.set_api("http://127.0.0.1:1".into());
        let (stop, shutdown) = watch::channel(false);
        let running = tokio::spawn(engine.run(shutdown));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !store.raw_files()?.is_empty() {
            assert!(Instant::now() < deadline, "archive was not pruned");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.send(true)?;
        running.await??;
        Ok(())
    }

    #[tokio::test]
    async fn slow_failed_feeds_allow_sweep_and_other_feed_to_run() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let mut args = options(false);
        args.recent_interval_secs = 1;
        args.popular_interval_secs = 1;
        let mut engine = Engine::new(root.clone(), "2026-10-22T00:00:00Z".parse()?, args)?;
        engine.meta.checked_at = Some(Utc::now());
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        engine
            .client
            .set_api(format!("http://{}", listener.local_addr()?));
        let (stop, shutdown) = watch::channel(false);
        let server = std::thread::spawn(move || -> Result<Vec<String>> {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut paths = Vec::new();
            while paths.len() < 4 {
                let (mut stream, _) = match listener.accept() {
                    Ok(stream) => stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        anyhow::ensure!(
                            Instant::now() < deadline,
                            "scheduler stopped making requests"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf)?;
                    anyhow::ensure!(n > 0, "incomplete request");
                    request.extend_from_slice(&buf[..n]);
                }
                let path = String::from_utf8_lossy(&request)
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .to_owned();
                let (status, body) = if path.contains("/search?") {
                    ("200 OK", r#"{"payload":{"auctions":[]}}"#)
                } else {
                    // Simulate a failed poll lasting longer than its scheduling interval.
                    std::thread::sleep(Duration::from_millis(1100));
                    ("400 Bad Request", "{}")
                };
                paths.push(path);
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            stop.send(true)?;
            Ok(paths)
        });
        tokio::time::timeout(Duration::from_secs(20), engine.run(shutdown)).await??;
        let paths = server.join().unwrap()?;
        assert_eq!(paths[0], "/v1/auctions");
        assert!(paths[1].contains("/search?") && paths[1].contains("price_asc"));
        assert!(paths[2].contains("/search?") && paths[2].contains("price_desc"));
        assert_eq!(paths[3], "/v1/auctions/popular");
        Ok(())
    }

    #[tokio::test]
    async fn feed_poll_requests_its_endpoint_and_commits_right_away() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let mut engine = Engine::new(
            root.clone(),
            "2026-10-22T00:00:00Z".parse()?,
            options(false),
        )?;
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../tests/fixtures/kulstar_auctions.json.gz")[..],
        )
        .read_to_string(&mut body)?;
        let page: serde_json::Value = serde_json::from_str(&body)?;
        let mut auction = page["payload"]["auctions"][0].clone();
        auction["item"]["weapon_url_name"] = "kulstar".into();
        auction["buyout_price"] = 500.into();
        let body = serde_json::json!({"payload": {"auctions": [auction]}}).to_string();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        engine
            .client
            .set_api(format!("http://{}", listener.local_addr()?));
        let server = std::thread::spawn(move || -> Result<()> {
            for (path, response) in [
                ("/v1/auctions/popular ", body.as_str()),
                ("/v1/auctions ", "not json"),
            ] {
                let (mut stream, _) = listener.accept()?;
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf)?;
                    anyhow::ensure!(n > 0, "request ended before headers");
                    request.extend_from_slice(&buf[..n]);
                }
                assert!(String::from_utf8_lossy(&request).starts_with(&format!("GET {path}")));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
            }
            Ok(())
        });
        let polled = engine.fetch_feed(wfmt_types::Feed::Popular).await?;
        assert_eq!(polled.auctions, 1);
        assert!(store.pending_raw()?.is_empty(), "committed by the poll");
        assert!(store.seen_dir().join("kulstar.json").exists());

        // A garbled response is retried at the next poll, never archived or treated as fatal.
        assert!(matches!(
            engine.fetch_feed(wfmt_types::Feed::Recent).await,
            Err(FetchError::Remote(_))
        ));
        assert_eq!(store.raw_files()?.len(), 1);
        server.join().unwrap()?;
        Ok(())
    }

    #[tokio::test]
    async fn capped_null_buyouts_fetch_direct_sales_without_claiming_full_coverage() -> Result<()> {
        for page_size in [98, 99, PAGE_CAP] {
            check_null_buyout_supplement(page_size).await?;
        }
        Ok(())
    }

    async fn check_null_buyout_supplement(page_size: usize) -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let mut engine = Engine::new(
            root.clone(),
            "2026-10-22T00:00:00Z".parse()?,
            options(false),
        )?;
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../tests/fixtures/kulstar_auctions.json.gz")[..],
        )
        .read_to_string(&mut body)?;
        let mut cheap: serde_json::Value = serde_json::from_str(&body)?;
        cheap["payload"]["auctions"]
            .as_array_mut()
            .unwrap()
            .truncate(page_size);
        let mut bids = cheap.clone();
        for (i, auction) in bids["payload"]["auctions"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .enumerate()
        {
            auction["id"] = format!("bid-{i}").into();
            auction["buyout_price"] = serde_json::Value::Null;
            auction["top_bid"] = serde_json::Value::Null;
            auction["is_direct_sell"] = false.into();
        }
        let mut high = cheap["payload"]["auctions"][0].clone();
        high["id"] = "expensive-direct".into();
        high["starting_price"] = 5000.into();
        high["buyout_price"] = 5000.into();
        high["is_direct_sell"] = true.into();
        let mut buyout = high.clone();
        buyout["id"] = "expensive-buyout-auction".into();
        buyout["is_direct_sell"] = false.into();
        let direct = serde_json::json!({"payload": {"auctions": [high]}});
        let with_buyout = serde_json::json!({"payload": {"auctions": [buyout]}});
        bids["payload"]["auctions"][0]["top_bid"] = 1250.into();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        engine
            .client
            .set_api(format!("http://{}", listener.local_addr()?));
        let server = std::thread::spawn(move || -> Result<()> {
            for (i, response) in [cheap, bids, direct, with_buyout].iter().enumerate() {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = Vec::new();
                let mut buf = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf)?;
                    anyhow::ensure!(n > 0, "request ended before headers");
                    request.extend_from_slice(&buf[..n]);
                }
                let request = String::from_utf8(request)?;
                assert_eq!(request.contains("buyout_policy=direct"), i == 2);
                assert_eq!(request.contains("buyout_policy=with"), i == 3);
                assert!(request.contains(if i == 0 {
                    "sort_by=price_asc"
                } else {
                    "sort_by=price_desc"
                }));
                let response = response.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                    response.len()
                )?;
            }
            Ok(())
        });
        engine.fetch(&engine.weapons[0].clone()).await?;
        server.join().unwrap()?;
        assert_eq!(engine.client.requests(), 4);
        assert!(
            engine
                .sink
                .auctions
                .iter()
                .any(|a| a.auction_id == "expensive-direct" && a.buyout_price == Some(5000))
        );
        let raw = RawResponse::read(&store.raw_files()?[0])?;
        assert!(raw.complete);
        assert_eq!(raw.pages.len(), 4);
        assert_eq!(raw.pages[2].buyout_policy, Some(BuyoutPolicy::Direct));
        assert_eq!(raw.pages[3].buyout_policy, Some(BuyoutPolicy::With));
        assert!(
            engine
                .sink
                .auctions
                .iter()
                .any(|a| a.auction_id == "bid-0" && a.top_bid == Some(1250))
        );
        assert!(!engine.sink.auctions.iter().any(|a| a.auction_id == "bid-1"));
        assert!(
            engine
                .sink
                .auctions
                .iter()
                .any(|a| a.auction_id == "expensive-buyout-auction" && !a.is_direct_sell)
        );
        let parsed = crate::pipeline::parse_pages(&raw.pages)?;
        assert!(
            !parsed.coverage.is_complete(),
            "a short direct-sale page cannot remove unseen bidding auctions"
        );
        assert!(!parsed.coverage.no_buyout);
        assert_eq!(parsed.auctions.len(), page_size * 2 + 2);
        Ok(())
    }

    #[tokio::test]
    async fn no_raw_commits_tracking_before_fetch_returns() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().to_path_buf();
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let mut engine = Engine::new(root.clone(), "2026-10-22T00:00:00Z".parse()?, options(true))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        engine
            .client
            .set_api(format!("http://{}", listener.local_addr()?));
        let server = std::thread::spawn(move || -> Result<()> {
            // A short page still needs the other order to prove the search was complete.
            for sort_by in [wfmt_types::SortBy::PriceAsc, wfmt_types::SortBy::PriceDesc] {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(5)))?;
                let mut request = [0; 4096];
                let n = stream.read(&mut request)?;
                let request = String::from_utf8_lossy(&request[..n]);
                anyhow::ensure!(request.contains(&format!("sort_by={sort_by}")), "{request}");
                let body = r#"{"payload":{"auctions":[]}}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )?;
            }
            Ok(())
        });
        let weapon = engine.weapons[0].clone();
        engine.fetch(&weapon).await?;
        server.join().unwrap()?;
        // Even an empty successful fetch commits its tracking snapshot immediately.
        assert!(store.seen_dir().join("kulstar.json").exists());
        assert!(store.raw_files()?.is_empty());
        assert!(engine.sink.auctions.is_empty());
        assert!(store.load_state()?.last_fetch.is_empty());
        Ok(())
    }
}
