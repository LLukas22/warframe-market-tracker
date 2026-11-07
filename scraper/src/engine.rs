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
use tokio::sync::{mpsc, watch};
use wfmt_types::{
    COMMAND_DIR, Command, CurrentFetch, FetchLog, LOCK_FILE, Phase, STATUS_FILE, ScraperStatus,
    Settings, SweepProgress, SweepSummary, Trigger,
};

use crate::identity::Registry;
use crate::maintenance::{self, MaintenanceArgs};
use crate::normalize::Normalizer;
use crate::pipeline::Pipeline;
use crate::store::{
    Meta, ParquetSink, PersistedSweep, RawPage, RawResponse, State, Store, Verified,
};
use crate::tracking::{PAGE_CAP, Tracker};
use crate::wfm::{self, Client, Weapon};

const RECENT: usize = 50;
const ERROR_COOLDOWN: Duration = Duration::from_secs(60);

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

    /// Hours between checks of the riven manifest version (weapon list).
    #[arg(long, env = "WFMT_METADATA_INTERVAL_HOURS", default_value_t = 24.0)]
    pub metadata_interval_hours: f64,

    /// Write a parquet part after this many weapons. Readers only see written parts, so this
    /// sets how live the web UI is during a sweep (about 10 s per weapon).
    #[arg(long, env = "WFMT_FLUSH_EVERY", default_value_t = 3)]
    pub flush_every: usize,

    /// Sort orders to page through, in order. The next one is only requested when the
    /// previous page was full (100 auctions), so small weapons cost a single request.
    #[arg(
        long,
        env = "WFMT_SORT_BY",
        value_delimiter = ',',
        default_values = ["price_asc", "price_desc"]
    )]
    pub sort_by: Vec<String>,

    /// Per weapon fetch, check up to this many auctions that dropped out of the pages but were
    /// priced outside the covered range (one request each). 0 disables.
    #[arg(long, env = "WFMT_VERIFY_MISSING", default_value_t = 0)]
    pub verify_missing: usize,

    /// Do not archive raw responses (disables `reprocess`).
    #[arg(long)]
    pub no_raw: bool,

    /// Only track these weapon slugs (comma separated).
    #[arg(long, env = "WFMT_WEAPONS", value_delimiter = ',')]
    pub weapons: Vec<String>,
}

impl EngineArgs {
    /// Defaults, overridden by `WFMT_*` environment variables.
    pub fn from_env() -> Self {
        #[derive(clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            args: EngineArgs,
        }
        <Wrapper as clap::Parser>::parse_from(["engine"]).args
    }
}

fn hours(h: f64) -> TimeDelta {
    TimeDelta::milliseconds((h * 3_600_000.0) as i64)
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
    processing: bool,
    status: ScraperStatus,
}

impl Engine {
    pub fn new(data_dir: PathBuf, rework_cutoff: DateTime<Utc>, args: EngineArgs) -> Result<Self> {
        // Complete interrupted publication before loading tracking/identity state.
        drop(crate::dataset::DatasetLock::write(&data_dir)?);
        let store = Store::new(data_dir.clone());
        let client = Client::new(
            args.platform.clone(),
            Duration::from_secs_f64(args.request_interval_secs),
        )?;
        let now = ms(Utc::now());
        let state = store.load_state()?;
        let mut engine = Self {
            sink: ParquetSink::new(store.root().join("parquet")),
            meta: store.load_meta()?,
            pipeline: Pipeline {
                normalizer: Normalizer::new(rework_cutoff),
                registry: Registry::open(store.registry_path())?,
                tracker: Tracker::open(store.seen_dir()),
            },
            weapons: Vec::new(),
            since_flush: 0,
            processing: false,
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
            queue,
        });
        self.save_state();
        self.set_phase(Phase::Scraping);
    }

    fn start_scheduled(&mut self) {
        let now = Utc::now();
        let fresh = now - hours(self.args.sweep_interval_hours / 2.0);
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
        // Recovery includes weapons excluded by the current `WFMT_WEAPONS` filter.
        let weapons = wfm::parse_weapons(&body)?;
        for raw in pending {
            let weapon = weapons
                .iter()
                .find(|w| w.slug == raw.weapon)
                .ok_or_else(|| {
                    anyhow::anyhow!("pending raw weapon {} missing from cache", raw.weapon)
                })?;
            tracing::info!(weapon = %raw.weapon, "recovering uncommitted raw fetch");
            self.pipeline.process(weapon, &raw, &mut self.sink)?;
            self.sink.acknowledge_raw(&raw);
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

    async fn fetch(&mut self, weapon: &Weapon) -> Result<FetchResult> {
        let mut raw = RawResponse::new(
            Utc::now(),
            weapon.slug.clone(),
            self.client.platform().to_owned(),
            Vec::new(),
            Vec::new(),
        );
        raw.complete = false;
        let mut fetch_error = None;
        for sort_by in self.args.sort_by.clone() {
            self.set_step(format!("{sort_by} page"));
            match self.client.auctions_raw(&weapon.slug, &sort_by).await {
                Ok(body) => {
                    raw.pages.push(RawPage {
                        sort_by,
                        buyout_policy: None,
                        body,
                    });
                    self.processing = true;
                    self.archive_progress(&raw)?;
                    self.processing = false;
                    match wfm::parse_auctions(&raw.pages.last().unwrap().body) {
                        Ok((auctions, skipped)) if auctions.len() + skipped < PAGE_CAP => break,
                        Ok(_) => {}
                        Err(e) => {
                            fetch_error = Some(e);
                            break;
                        }
                    }
                }
                Err(e) => {
                    fetch_error = Some(e);
                    break;
                }
            }
        }
        if raw.pages.is_empty() {
            return Err(
                fetch_error.unwrap_or_else(|| anyhow::anyhow!("no search sort orders configured"))
            );
        }
        // Null buyouts sort before expensive buyouts on descending pages. On popular
        // weapons they can occupy the entire cap, starving price analysis of good rolls.
        let needs_buyouts = raw.pages.iter().any(|page| {
            page.sort_by == "price_desc"
                && wfm::parse_auctions(&page.body).is_ok_and(|(auctions, skipped)| {
                    auctions.len() + skipped >= PAGE_CAP
                        && auctions.iter().any(|a| a.buyout_price.is_none())
                })
        });
        if fetch_error.is_none() && needs_buyouts {
            // The API separates direct sales from bidding auctions with buyouts.
            for policy in ["direct", "with"] {
                self.set_step(format!("price_desc {policy} buyout page"));
                match self
                    .client
                    .auctions_raw_scoped(&weapon.slug, "price_desc", Some(policy))
                    .await
                {
                    Ok(body) => {
                        raw.pages.push(RawPage {
                            sort_by: "price_desc".into(),
                            buyout_policy: Some(policy.into()),
                            body,
                        });
                        self.processing = true;
                        self.archive_progress(&raw)?;
                        self.processing = false;
                        if let Err(e) = wfm::parse_auctions(&raw.pages.last().unwrap().body) {
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
            let uncertain = self.pipeline.uncertain(&weapon.slug, &raw.pages)?;
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
                        self.processing = true;
                        self.archive_progress(&raw)?;
                        self.processing = false;
                    }
                    Err(e) => tracing::warn!(auction = %id, error = %e, "verify failed"),
                }
            }
        }
        self.processing = true;
        self.archive_progress(&raw)?;
        let summary = self.pipeline.process(weapon, &raw, &mut self.sink)?;
        if !self.args.no_raw {
            self.sink.acknowledge_raw(&raw);
        }
        // Without a raw journal, publish before the scheduler can persist progress.
        if self.args.no_raw {
            self.sink.flush()?;
        }
        self.processing = false;
        tracing::info!(weapon = %weapon.slug, pages = raw.pages.len(),
            auctions = summary.auctions, complete = summary.complete,
            fitted = summary.fitted, gone = summary.gone, skipped = summary.skipped, "fetched");
        if let Some(error) = fetch_error {
            return Err(error);
        }
        Ok(FetchResult {
            pages: raw.pages.len() as u32,
            auctions: summary.auctions as u32,
            gone: summary.gone as u32,
        })
    }

    fn log_fetch(&mut self, weapon: &str, started: Instant, result: &Result<FetchResult>) {
        let entry = FetchLog {
            t: ms(Utc::now()),
            weapon: weapon.to_owned(),
            duration_ms: started.elapsed().as_millis() as i64,
            pages: result.as_ref().map_or(0, |r| r.pages),
            auctions: result.as_ref().map_or(0, |r| r.auctions),
            gone: result.as_ref().map_or(0, |r| r.gone),
            error: result.as_ref().err().map(|e| format!("{e:#}")),
        };
        self.status.recent.insert(0, entry);
        self.status.recent.truncate(RECENT);
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
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) -> Result<()> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let dir = self.data_dir.clone();
        // Drop stale commands from before this engine started.
        take_commands(&dir);
        tokio::spawn(async move {
            // Ends with the engine, or it would take commands meant for the next one.
            while !tx.is_closed() {
                tokio::time::sleep(Duration::from_secs(1)).await;
                for command in take_commands(&dir) {
                    if tx.send(command).is_err() {
                        return;
                    }
                }
            }
        });

        let mut deferred: VecDeque<Command> = VecDeque::new();
        if let Some(sweep) = &self.state.sweep {
            tracing::info!(remaining = sweep.queue.len(), "resuming sweep");
            self.status.phase = Phase::Scraping;
        }
        self.publish();

        let mut shutting_down = false;
        let mut metadata_failed_at: Option<DateTime<Utc>> = None;
        let mut next_maintenance = Instant::now();
        loop {
            if *shutdown.borrow() {
                break;
            }
            if Instant::now() >= next_maintenance {
                self.sink.flush()?;
                self.set_phase(Phase::Maintenance);
                let dir = self.data_dir.clone();
                let args = self.args.storage.clone();
                let mut task = tokio::task::spawn_blocking(move || {
                    maintenance::maintain(&dir, Utc::now(), &args)
                });
                // Keep status alive while disk work runs. Commands stay queued and shutdown
                // waits for the local transaction to finish safely.
                loop {
                    tokio::select! {
                        result = &mut task => {
                            match result {
                                Ok(Ok(_)) => {},
                                Ok(Err(e)) => {
                                    tracing::error!(error = %e, "storage maintenance failed");
                                    self.status.error = Some(format!("storage maintenance: {e:#}"));
                                }
                                Err(e) => return Err(e.into()),
                            }
                            break;
                        }
                        _ = tokio::time::sleep(Duration::from_secs(5)) => self.publish(),
                    }
                }
                next_maintenance = Instant::now()
                    + Duration::from_secs(
                        self.args
                            .storage
                            .maintenance_interval_hours
                            .saturating_mul(3600),
                    );
                self.set_phase(if self.state.sweep.is_some() {
                    Phase::Scraping
                } else {
                    Phase::Idle
                });
                continue;
            }
            while let Ok(command) = rx.try_recv() {
                deferred.push_back(command);
            }
            let now = Utc::now();
            let sweep_due = self.state.sweep.is_none() && now >= self.next_sweep_at();

            // Make sure the weapon list is fresh before anything that needs it.
            let needs_weapons = self.state.sweep.is_some()
                || sweep_due
                || !deferred.is_empty()
                || self.weapons.is_empty();
            let retry_metadata =
                metadata_failed_at.is_none_or(|t| now - t > TimeDelta::minutes(10));
            if needs_weapons && self.metadata_due(now) && retry_metadata {
                self.set_phase(Phase::Metadata);
                tokio::select! {
                    result = self.refresh_metadata() => match result {
                        Ok(()) => metadata_failed_at = None,
                        Err(e) => {
                            tracing::warn!(error = %e, "metadata refresh failed");
                            metadata_failed_at = Some(now);
                            self.status.error = Some(format!("weapon list: {e:#}"));
                        }
                    },
                    _ = shutdown.changed() => break,
                }
                let phase = if self.state.sweep.is_some() {
                    Phase::Scraping
                } else {
                    Phase::Idle
                };
                self.set_phase(phase);
                continue;
            }

            while let Some(command) = deferred.pop_front() {
                self.handle(command)?;
            }

            if self.state.sweep.is_none() {
                if sweep_due && !self.weapons.is_empty() {
                    self.start_scheduled();
                    continue;
                }
                if self.status.phase != Phase::Idle {
                    self.set_phase(Phase::Idle);
                }
                // Idle: wake for commands, the next sweep, or the heartbeat.
                let heartbeat = TimeDelta::milliseconds(wfmt_types::HEARTBEAT_MS / 2);
                let wake = self.next_sweep_at().min(now + heartbeat);
                let sleep = tokio::time::sleep((wake - now).to_std().unwrap_or_default());
                tokio::select! {
                    _ = sleep => self.publish(),
                    Some(command) = rx.recv() => deferred.push_back(command),
                    _ = shutdown.changed() => break,
                }
                continue;
            }

            // Sweep running: next weapon.
            let sweep = self.state.sweep.as_mut().unwrap();
            if sweep.queue.is_empty() {
                self.finish_sweep(false)?;
                continue;
            }
            let slug = sweep.queue.remove(0);
            let Some(weapon) = self.weapons.iter().find(|w| w.slug == slug).cloned() else {
                tracing::warn!(weapon = %slug, "weapon no longer exists, skipping");
                sweep.progress.total = sweep.progress.total.saturating_sub(1);
                continue;
            };
            sweep.progress.current = Some(CurrentFetch {
                weapon: slug.clone(),
                started_at: ms(Utc::now()),
                step: "starting".into(),
            });
            self.status.error = None;
            self.set_phase(Phase::Scraping);

            let started = Instant::now();
            let mut cancelled = false;
            let result = {
                let fetch = self.fetch(&weapon);
                tokio::pin!(fetch);
                loop {
                    tokio::select! {
                        result = &mut fetch => break Some(result),
                        Some(command) = rx.recv() => {
                            if command == Command::Cancel {
                                cancelled = true;
                                break None;
                            }
                            deferred.push_back(command);
                        }
                        _ = shutdown.changed() => {
                            shutting_down = true;
                            break None;
                        }
                    }
                }
            };

            let Some(result) = result else {
                if shutting_down {
                    // Put the weapon back so the sweep resumes with it.
                    if let Some(s) = self.state.sweep.as_mut() {
                        s.queue.insert(0, slug);
                        s.progress.current = None;
                    }
                    self.save_state();
                    break;
                }
                if cancelled {
                    self.recover_pending()?;
                    self.finish_sweep(true)?;
                }
                continue;
            };

            if self.processing {
                // Storage/processing failures are fatal: do not advance state or publish
                // partially updated in-memory tracking. Restart recovers the raw journal.
                return result.map(|_| ());
            }
            if let Err(e) = &result {
                tracing::error!(weapon = %slug, error = %e, "fetch failed");
                self.status.error = Some(format!("{slug}: {e:#}"));
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
                    _ = tokio::time::sleep(ERROR_COOLDOWN) => {}
                    Some(command) = rx.recv() => deferred.push_back(command),
                    _ = shutdown.changed() => break,
                }
                self.set_phase(Phase::Scraping);
            }
        }

        tracing::info!("scraper stopping");
        self.sink.flush()?;
        self.save_state();
        self.status.phase = Phase::Stopped;
        self.publish();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

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
            metadata_interval_hours: 24.0,
            flush_every: 3,
            sort_by: vec!["price_asc".into(), "price_desc".into()],
            verify_missing: 0,
            no_raw,
            weapons: vec![],
        }
    }

    #[tokio::test]
    async fn archives_first_page_before_second_request_and_recovers_failed_fetch() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("wfmt-partial-{}", crate::dataset::unique_id()));
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
        assert!(engine.fetch(&weapon).await.is_err());
        server.join().unwrap()?;
        assert!(!engine.processing, "network failures remain retryable");
        assert_eq!(engine.sink.auctions.len(), PAGE_CAP);
        assert!(!store.seen_dir().join("kulstar.json").exists());
        drop(engine); // Crash before flushing the successful first page.
        drop(Engine::new(root.clone(), cutoff, options(false))?);
        assert!(store.pending_raw()?.is_empty());
        let seen: std::collections::HashMap<String, crate::tracking::Seen> =
            serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
        assert_eq!(seen.len(), PAGE_CAP);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[tokio::test]
    async fn capped_null_buyouts_fetch_direct_sales_without_claiming_full_coverage() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("wfmt-direct-{}", crate::dataset::unique_id()));
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
        let cheap: serde_json::Value = serde_json::from_str(&body)?;
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
        assert_eq!(raw.pages[2].buyout_policy.as_deref(), Some("direct"));
        assert_eq!(raw.pages[3].buyout_policy.as_deref(), Some("with"));
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
        assert_eq!(parsed.auctions.len(), 202);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[tokio::test]
    async fn no_raw_commits_tracking_before_fetch_returns() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("wfmt-no-raw-{}", crate::dataset::unique_id()));
        let store = Store::new(root.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let mut engine = Engine::new(root.clone(), "2026-10-22T00:00:00Z".parse()?, options(true))?;
        let listener = TcpListener::bind("127.0.0.1:0")?;
        engine
            .client
            .set_api(format!("http://{}", listener.local_addr()?));
        let server = std::thread::spawn(move || -> Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(Duration::from_secs(5)))?;
            let mut request = [0; 4096];
            stream.read(&mut request)?;
            let body = r#"{"payload":{"auctions":[]}}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )?;
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
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
