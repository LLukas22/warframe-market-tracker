//! Publishes the data to a Hugging Face dataset repository, set with `WFMT_HF_TOKEN` and
//! `WFMT_HF_DATASET`. Without both, nothing is uploaded. `WFMT_HF_PULL_DATASET` does the
//! opposite, see [`pull`]: the server downloads its data from a dataset instead of scraping it.
//!
//! Each finished UTC day is uploaded once maintenance has compacted it, as an exact copy of its
//! `date=` folders: new or changed files are added and files the day no longer has are deleted,
//! in the same commit. `hub/upload.json` remembers the files of every uploaded day, so the hourly
//! check only contacts the Hub when a day is new or has changed. Today is never uploaded, it's
//! still being written. Days that only exist on the Hub stay there. The weapon list goes along
//! as `riven_weapons.json`, so servers that pull the data have names, groups and images.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use wfmt_dataset::layout::{DATE_PREFIX, PARQUET_DIR, Table};

use anyhow::{Context, Result, anyhow, bail};
use chrono::NaiveDate;
use futures_util::StreamExt;
use hf_hub::repository::{AddSource, CommitOperation, RepoTreeEntry};
use hf_hub::{HFClient, HFRepository, RepoTypeDataset};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use wfmt_dataset::maintenance;
use wfmt_dataset::{DatasetLock, durable_write};

use crate::api::HubStatus;

mod pull;

const STATE: &str = "hub/upload.json";
/// Hard links to the files being uploaded, so compaction can't delete them midway.
const SNAPSHOT: &str = "hub/snapshot";
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Gives the scraper time for its first maintenance pass after a restart.
const FIRST_CHECK: Duration = Duration::from_secs(60);
/// The Hub recommends staying well under 100 files per commit.
const FILES_PER_COMMIT: usize = 60;
const README: &str = "README.md";
/// The weapon list in the repository, and where the scraper keeps it.
const CATALOG: &str = "riven_weapons.json";
const LOCAL_CATALOG: &str = "cache/riven_weapons.json";
/// Larger READMEs were written by someone and are never replaced.
const PLACEHOLDER_MAX: u64 = 4096;

/// A dataset on the Hub.
#[derive(Clone)]
pub struct Repo {
    /// Needed for uploads, and for pulling a private dataset.
    token: Option<String>,
    /// `owner/name`.
    dataset: String,
}

/// What the server does with the Hub.
pub enum Mode {
    Off,
    Upload(Repo),
    Pull(Repo),
}

impl Mode {
    /// From `WFMT_HF_TOKEN`, `WFMT_HF_DATASET` and `WFMT_HF_PULL_DATASET`, or what's wrong
    /// with them. A scraping server has its own data, so it ignores `WFMT_HF_PULL_DATASET`.
    pub fn from_settings(
        token: Option<String>,
        upload: Option<String>,
        pull: Option<String>,
        scraping: bool,
    ) -> Result<Self> {
        if let Some(pull) = pull.as_deref().filter(|_| !scraping) {
            if upload.is_some() {
                bail!(
                    "WFMT_HF_DATASET and WFMT_HF_PULL_DATASET are both set. A server only uploads \
                     what it scrapes, so set WFMT_ENABLE_SCRAPER=true or clear one of them."
                );
            }
            return Ok(Mode::Pull(Repo {
                token,
                dataset: dataset_id("WFMT_HF_PULL_DATASET", pull)?,
            }));
        }
        match (token, upload) {
            (None, None) => Ok(Mode::Off),
            (Some(token), Some(dataset)) => Ok(Mode::Upload(Repo {
                token: Some(token),
                dataset: dataset_id("WFMT_HF_DATASET", &dataset)?,
            })),
            (None, Some(_)) => bail!("WFMT_HF_DATASET is set, but WFMT_HF_TOKEN isn't."),
            // The token is there for pulling a private dataset.
            (Some(_), None) if pull.is_some() => Ok(Mode::Off),
            (Some(_), None) => bail!("WFMT_HF_TOKEN is set, but WFMT_HF_DATASET isn't."),
        }
    }
}

/// Accepts `owner/name` as well as the dataset's URL.
fn dataset_id(name: &str, value: &str) -> Result<String> {
    let id = value.trim_end_matches('/');
    let id = id.strip_prefix("https://huggingface.co/").unwrap_or(id);
    let id = id.strip_prefix("datasets/").unwrap_or(id);
    match id.split('/').collect::<Vec<_>>()[..] {
        [owner, name] if !owner.is_empty() && !name.is_empty() => Ok(id.to_owned()),
        _ => bail!("{name} should look like owner/name, not {value:?}."),
    }
}

/// What has been uploaded so far. Starts over when the dataset changes.
#[derive(Default, Serialize, Deserialize)]
struct State {
    dataset: String,
    /// The files of each uploaded day by path in the repository, with their sizes.
    days: BTreeMap<String, BTreeMap<String, u64>>,
    last_upload_at: Option<i64>,
    commit_url: Option<String>,
    /// The dataset has a README, ours or someone else's. Checked again when forced.
    #[serde(default)]
    card_checked: bool,
    /// [`fingerprint`] of the weapon list last uploaded.
    #[serde(default)]
    catalog: Option<u64>,
}

fn load_state(root: &Path, dataset: &str) -> Result<State> {
    let state = match fs::read(root.join(STATE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading the upload state")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(e.into()),
    };
    Ok(if state.dataset == dataset {
        state
    } else {
        State {
            dataset: dataset.to_owned(),
            ..State::default()
        }
    })
}

fn save_state(root: &Path, state: &State) -> Result<()> {
    durable_write(&root.join(STATE), &serde_json::to_vec(state)?)
}

#[derive(Default)]
struct Live {
    running: bool,
    last_check_at: Option<i64>,
    outcome: Option<String>,
    error: Option<String>,
    last_upload_at: Option<i64>,
    last_day: Option<String>,
    commit_url: Option<String>,
}

/// The uploads or downloads, and how they're going.
pub struct Hub {
    mode: Mode,
    live: Mutex<Live>,
    started: AtomicBool,
    /// A check was asked for, see [`Hub::request_upload`].
    force: AtomicBool,
    wake: Notify,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl Hub {
    pub fn new(mode: Mode) -> Self {
        Self {
            mode,
            live: Mutex::new(Live::default()),
            started: AtomicBool::new(false),
            force: AtomicBool::new(false),
            wake: Notify::new(),
        }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(&self, state: &State) {
        let mut live = self.live();
        live.last_upload_at = state.last_upload_at;
        live.last_day = state.days.keys().next_back().cloned();
        live.commit_url = state.commit_url.clone();
    }

    pub fn status(&self) -> HubStatus {
        let (dataset, pull) = match &self.mode {
            Mode::Upload(repo) => (Some(repo.dataset.clone()), false),
            Mode::Pull(repo) => (Some(repo.dataset.clone()), true),
            Mode::Off => (None, false),
        };
        let live = self.live();
        HubStatus {
            dataset,
            pull,
            running: live.running,
            queued: self.force.load(Ordering::SeqCst),
            last_upload_at: live.last_upload_at,
            last_day: live.last_day.clone(),
            commit_url: live.commit_url.clone(),
            last_check_at: live.last_check_at,
            outcome: live.outcome.clone(),
            error: live.error.clone(),
        }
    }

    /// Checks every finished day against the Hub right away, after the current check if one
    /// runs. When pulling, checks the dataset for new days instead.
    pub fn request_upload(&self) -> Result<()> {
        if matches!(self.mode, Mode::Off) || !self.started.load(Ordering::SeqCst) {
            bail!("Hugging Face isn't set up on this server.");
        }
        self.force.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        Ok(())
    }

    /// Starts the hourly upload or download check on the data in `root`, if either is set up.
    pub fn start(&'static self, root: PathBuf) {
        let (repo, pulling) = match &self.mode {
            Mode::Upload(repo) => (repo.clone(), false),
            Mode::Pull(repo) => (repo.clone(), true),
            Mode::Off => return,
        };
        if self.started.swap(true, Ordering::SeqCst) {
            return;
        }
        if pulling {
            pull::start(self, root, repo);
            return;
        }
        match load_state(&root, &repo.dataset) {
            Ok(state) => self.publish(&state),
            Err(error) => self.live().error = Some(format!("{error:#}")),
        }
        tokio::spawn(self.run(root, repo));
    }

    /// Runs `check` now and then, and whenever [`Hub::request_upload`] asks, keeping the status
    /// up to date. `check` gets whether it was asked for.
    async fn every_hour<F, Fut>(&self, first: Duration, mut check: F)
    where
        F: FnMut(bool) -> Fut,
        Fut: std::future::Future<Output = Result<String>>,
    {
        let mut wait = first;
        loop {
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = self.wake.notified() => {}
            }
            wait = CHECK_INTERVAL;
            let forced = self.force.swap(false, Ordering::SeqCst);
            self.live().running = true;
            let result = check(forced).await;
            let mut live = self.live();
            live.running = false;
            live.last_check_at = Some(now_ms());
            match result {
                Ok(outcome) => {
                    live.outcome = Some(outcome);
                    live.error = None;
                }
                Err(error) => {
                    tracing::warn!("hugging face sync failed: {error:#}");
                    live.error = Some(format!("{error:#}"));
                }
            }
        }
    }

    async fn run(&self, root: PathBuf, repo: Repo) {
        let client = match repository(&repo) {
            Ok(client) => client,
            Err(error) => {
                self.live().error = Some(format!("{error:#}"));
                return;
            }
        };
        self.every_hour(FIRST_CHECK, |forced| {
            let (root, client, dataset) = (root.clone(), client.clone(), repo.dataset.clone());
            async move {
                let result = sync(self, &root, &client, &dataset, forced).await;
                let _ = fs::remove_dir_all(root.join(SNAPSHOT));
                result
            }
        })
        .await;
    }
}

fn repository(config: &Repo) -> Result<HFRepository<RepoTypeDataset>> {
    let mut builder = HFClient::builder().user_agent(wfmt_types::USER_AGENT);
    if let Some(token) = &config.token {
        builder = builder.token(token);
    }
    let client = builder
        .build()
        .context("Can't set up the Hugging Face client")?;
    let (owner, name) = config
        .dataset
        .split_once('/')
        .ok_or_else(|| anyhow!("{} isn't owner/name", config.dataset))?;
    Ok(client.dataset(owner, name))
}

struct Day {
    date: String,
    /// Path in the repository -> size.
    files: BTreeMap<String, u64>,
}

struct Prepared {
    state: State,
    /// Changed or new days, linked into the snapshot.
    pending: Vec<Day>,
    /// No maintenance pass has finished yet, so nothing is known to be final.
    uncompacted: bool,
    /// [`fingerprint`] of the weapon list, copied into the snapshot, if the scraper has one.
    catalog: Option<u64>,
}

/// 64-bit FNV-1a: tells whether the weapon list changed since it was uploaded.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The dates with data in any table.
fn dates(root: &Path) -> Result<BTreeSet<NaiveDate>> {
    let mut dates = BTreeSet::new();
    for table in Table::ALL {
        let dir = root.join(PARQUET_DIR).join(table.as_str());
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if let Some(date) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix(DATE_PREFIX))
                .and_then(|date| date.parse().ok())
                && entry.file_type()?.is_dir()
            {
                dates.insert(date);
            }
        }
    }
    Ok(dates)
}

fn day_files(root: &Path, date: &str) -> Result<BTreeMap<String, u64>> {
    let mut files = BTreeMap::new();
    for table in Table::ALL {
        let folder = table.day_folder(date);
        let dir = root.join(PARQUET_DIR).join(&folder);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|n| n.ends_with(".parquet")) else {
                continue;
            };
            let meta = entry.metadata()?;
            if meta.is_file() {
                files.insert(format!("{folder}/{name}"), meta.len());
            }
        }
    }
    Ok(files)
}

/// Finds the compacted days that differ from what was uploaded, every compacted day if forced,
/// and links their files into the snapshot.
fn prepare(root: &Path, dataset: &str, forced: bool) -> Result<Prepared> {
    let state = load_state(root, dataset)?;
    let snapshot = root.join(SNAPSHOT);
    if snapshot.exists() {
        fs::remove_dir_all(&snapshot)?;
    }
    let Some(before) = maintenance::compacted_before(root)? else {
        return Ok(Prepared {
            state,
            pending: Vec::new(),
            uncompacted: true,
            catalog: None,
        });
    };
    // Replaced in one rename, so a plain read sees either the old list or the new one.
    let catalog = match fs::read(root.join(LOCAL_CATALOG)) {
        Ok(bytes) => {
            fs::create_dir_all(&snapshot)?;
            fs::write(snapshot.join(CATALOG), &bytes)?;
            Some(fingerprint(&bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let _lock = DatasetLock::read(root)?;
    let mut pending = Vec::new();
    for date in dates(root)?.into_iter().filter(|d| *d < before) {
        let date = date.to_string();
        let files = day_files(root, &date)?;
        if files.is_empty() || (!forced && state.days.get(&date) == Some(&files)) {
            continue;
        }
        for path in files.keys() {
            let link = snapshot.join(path);
            fs::create_dir_all(link.parent().unwrap())?;
            let source = root.join(PARQUET_DIR).join(path);
            if fs::hard_link(&source, &link).is_err() {
                fs::copy(&source, &link)
                    .with_context(|| format!("copying {} for upload", source.display()))?;
            }
        }
        pending.push(Day { date, files });
    }
    Ok(Prepared {
        state,
        pending,
        uncompacted: false,
        catalog,
    })
}

/// Every file in the repository with its size, at `revision` or the newest commit.
async fn remote_files(
    repo: &HFRepository<RepoTypeDataset>,
    revision: Option<&str>,
) -> Result<BTreeMap<String, u64>> {
    let stream = repo
        .list_tree()
        .recursive(true)
        .maybe_revision(revision.map(str::to_owned))
        .send()?;
    let mut stream = std::pin::pin!(stream);
    let mut files = BTreeMap::new();
    while let Some(entry) = stream.next().await {
        if let RepoTreeEntry::File {
            path, size, lfs, ..
        } = entry?
        {
            files.insert(path, lfs.and_then(|l| l.size).unwrap_or(size));
        }
    }
    Ok(files)
}

/// What makes the Hub's copy of `day` match the local one.
fn day_operations(
    day: &Day,
    remote: &BTreeMap<String, u64>,
    snapshot: &Path,
) -> Vec<CommitOperation> {
    let mut operations: Vec<_> = day
        .files
        .iter()
        .filter(|(path, size)| remote.get(*path) != Some(*size))
        .map(|(path, _)| CommitOperation::add_file(path, snapshot.join(path)))
        .collect();
    let folders: Vec<String> = Table::ALL
        .iter()
        .map(|table| format!("{}/", table.day_folder(&day.date)))
        .collect();
    operations.extend(
        remote
            .keys()
            .filter(|path| folders.iter().any(|f| path.starts_with(f.as_str())))
            .filter(|path| !day.files.contains_key(*path))
            .map(|path| CommitOperation::Delete {
                path_in_repo: path.clone(),
            }),
    );
    operations
}

/// The settings of a README with nothing else in it, like the placeholder the Hub creates with
/// a new dataset, which only holds its license. `None` for a README someone wrote.
fn placeholder_settings(readme: &str) -> Option<Vec<String>> {
    let text = readme.trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        return Some(Vec::new());
    }
    let (settings, body) = text.strip_prefix("---")?.split_once("\n---")?;
    let settings: Vec<String> = settings
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    let has_tables = settings.iter().any(|line| line.starts_with("configs:"));
    (body.trim().is_empty() && !has_tables).then_some(settings)
}

/// The dataset card, keeping the settings of the placeholder it replaces.
fn dataset_card(dataset: &str, settings: &[String]) -> String {
    let repository = wfmt_types::REPOSITORY;
    let mut header: Vec<String> = settings.to_vec();
    if !settings.iter().any(|line| line.starts_with("pretty_name:")) {
        header.push("pretty_name: Warframe riven auctions".into());
    }
    header.push("configs:".into());
    for &table in Table::ALL {
        header.push(format!("- config_name: {table}"));
        header.push(format!("  data_files: \"{table}/*/*.parquet\""));
        if table == Table::Auctions {
            header.push("  default: true".into());
        }
    }
    let header = header.join("\n");
    format!(
        r#"---
{header}
---

# Warframe riven auctions

Riven mod auctions from [warframe.market](https://warframe.market), collected by
[Warframe Market Tracker]({repository}). The tracker fetches every weapon's auctions every few
hours and follows the site's newest listings in between. Each finished UTC day is added as its own
`date=` folder a few hours after midnight UTC.

## Tables

- `auctions`: one row per sighting of a listing, at `scraped_at`. Prices are asking prices or
  bids, not confirmed sales.
- `attributes`: the riven's stats for each `auctions` row, joined on `scraped_at` and
  `auction_id`.
- `gone`: listings that disappeared (sold or removed), at `detected_at`.

`riven_weapons.json` is warframe.market's weapon list, with names, groups and images. It isn't a
table, it's there for trackers that use this dataset instead of scraping.

## Usage

```python
from datasets import load_dataset

auctions = load_dataset("{dataset}", "auctions", split="train")
```

Or query the files directly, for example with DuckDB:

```sql
SELECT * FROM 'hf://datasets/{dataset}/auctions/*/*.parquet';
```

How listings are collected and normalized, the source code and issues:
[{repository}]({repository}).
"#
    )
}

/// The README to upload: a new one if there is none or only the Hub's placeholder.
async fn card_update(
    repo: &HFRepository<RepoTypeDataset>,
    remote: &BTreeMap<String, u64>,
    dataset: &str,
) -> Result<Option<String>> {
    let settings = match remote.get(README) {
        None => Vec::new(),
        Some(size) if *size > PLACEHOLDER_MAX => return Ok(None),
        Some(_) => {
            let bytes = repo
                .download_file_to_bytes()
                .filename(README)
                .send()
                .await
                .context("Can't read the dataset's README")?;
            match placeholder_settings(&String::from_utf8_lossy(&bytes)) {
                Some(settings) => settings,
                None => return Ok(None),
            }
        }
    };
    Ok(Some(dataset_card(dataset, &settings)))
}

fn readme(card: &str) -> CommitOperation {
    CommitOperation::Add {
        path_in_repo: README.into(),
        source: AddSource::Bytes(card.as_bytes().to_vec().into()),
    }
}

/// "a", "a and b", "a, b and c".
fn listing(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [only] => only.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

fn days_text(count: usize) -> String {
    if count == 1 {
        "1 day".into()
    } else {
        format!("{count} days")
    }
}

async fn sync(
    hub: &Hub,
    root: &Path,
    repo: &HFRepository<RepoTypeDataset>,
    dataset: &str,
    forced: bool,
) -> Result<String> {
    let prepared = {
        let (root, dataset) = (root.to_owned(), dataset.to_owned());
        tokio::task::spawn_blocking(move || prepare(&root, &dataset, forced)).await??
    };
    let Prepared {
        mut state,
        pending,
        uncompacted,
        catalog,
    } = prepared;
    if uncompacted {
        return Ok("Waiting for the scraper's first maintenance pass.".into());
    }
    let check_card = forced || !state.card_checked;
    let catalog_changed = catalog.is_some() && catalog != state.catalog;
    if pending.is_empty() && !check_card && !catalog_changed {
        return Ok("Up to date.".into());
    }
    let remote = remote_files(repo, None).await.with_context(|| {
        format!(
            "Can't list the dataset {dataset}. It has to exist already, and the token needs \
             write access to it"
        )
    })?;
    let snapshot = root.join(SNAPSHOT);
    // A README problem shouldn't hold up the data. It's checked again next time.
    let mut card_problem = None;
    let mut card = None;
    if check_card {
        match card_update(repo, &remote, dataset).await {
            Ok(update) => card = update,
            Err(error) => card_problem = Some(format!("{error:#}")),
        }
    }
    let card_added = card.is_some();
    let upload_catalog = catalog.is_some() && (catalog_changed || !remote.contains_key(CATALOG));
    // The card and the weapon list go along with the first day, or on their own.
    let mut extras: Vec<CommitOperation> = card.iter().map(|card| readme(card)).collect();
    if upload_catalog {
        extras.push(CommitOperation::add_file(CATALOG, snapshot.join(CATALOG)));
    }

    // Days that need changes, grouped into commits of a reasonable size.
    let mut batches: Vec<(Vec<Day>, Vec<CommitOperation>)> = Vec::new();
    let mut unchanged = 0;
    for day in pending {
        let operations = day_operations(&day, &remote, &snapshot);
        if operations.is_empty() {
            state.days.insert(day.date, day.files);
            unchanged += 1;
            continue;
        }
        match batches.last_mut() {
            Some((days, ops)) if ops.len() + operations.len() <= FILES_PER_COMMIT => {
                days.push(day);
                ops.extend(operations);
            }
            _ => batches.push((vec![day], operations)),
        }
    }
    if unchanged > 0 {
        save_state(root, &state)?;
        hub.publish(&state);
    }
    if batches.is_empty() && !extras.is_empty() {
        let message = match (card_added, upload_catalog) {
            (true, true) => "Add the dataset card and the weapon list",
            (true, false) => "Add the dataset card",
            _ => "Update the weapon list",
        };
        repo.create_commit()
            .operations(std::mem::take(&mut extras))
            .commit_message(message)
            .send()
            .await
            .with_context(|| format!("{message} failed"))?;
        if upload_catalog {
            state.catalog = catalog;
            save_state(root, &state)?;
        }
    }

    let mut uploaded = 0;
    for (days, mut operations) in batches {
        let (first, last) = (&days[0].date, &days[days.len() - 1].date);
        let message = if first == last {
            format!("Add {first}")
        } else {
            format!("Add {first} to {last}")
        };
        let with_extras = !extras.is_empty();
        operations.append(&mut extras);
        let info = repo
            .create_commit()
            .operations(operations)
            .commit_message(message.clone())
            .send()
            .await
            .with_context(|| format!("{message} failed"))?;
        uploaded += days.len();
        for day in days {
            state.days.insert(day.date, day.files);
        }
        state.last_upload_at = Some(now_ms());
        state.commit_url = info.commit_url;
        if with_extras && upload_catalog {
            state.catalog = catalog;
        }
        save_state(root, &state)?;
        hub.publish(&state);
    }
    if check_card && card_problem.is_none() && !state.card_checked {
        state.card_checked = true;
        save_state(root, &state)?;
    }
    let mut done = Vec::new();
    if uploaded > 0 {
        done.push(days_text(uploaded));
    }
    if card_added {
        done.push("the dataset card".into());
    }
    if upload_catalog {
        done.push("the weapon list".into());
    }
    let outcome = if !done.is_empty() {
        format!("Uploaded {}.", listing(&done))
    } else if forced {
        "Everything was already on the Hub.".into()
    } else {
        "Up to date.".into()
    };
    Ok(match card_problem {
        Some(problem) => format!("{outcome} {problem}."),
        None => outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path, and the directory that is removed when this is dropped.
    struct TempRoot(PathBuf, #[allow(dead_code)] tempfile::TempDir);

    impl TempRoot {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            Self(temp.path().to_path_buf(), temp)
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }

    #[test]
    fn dataset_accepts_ids_and_urls() {
        let id = |value| dataset_id("WFMT_HF_DATASET", value);
        assert_eq!(id("me/rivens").unwrap(), "me/rivens");
        assert_eq!(
            id("https://huggingface.co/datasets/me/rivens/").unwrap(),
            "me/rivens"
        );
        assert!(id("rivens").is_err());
        assert!(
            id("me/rivens/extra")
                .unwrap_err()
                .to_string()
                .starts_with("WFMT_HF_DATASET ")
        );
    }

    #[test]
    fn scraping_turns_the_pull_off() {
        let some = |v: &str| Some(v.to_owned());
        let name = |mode: Result<Mode>| match mode {
            Ok(Mode::Off) => "off".to_owned(),
            Ok(Mode::Upload(c)) => format!("upload {}", c.dataset),
            Ok(Mode::Pull(c)) => format!("pull {}", c.dataset),
            Err(_) => "error".to_owned(),
        };
        let pull = some("LLukas22/wf-riven-data");
        assert_eq!(
            name(Mode::from_settings(None, None, pull.clone(), false)),
            "pull LLukas22/wf-riven-data"
        );
        assert_eq!(
            name(Mode::from_settings(None, None, pull.clone(), true)),
            "off"
        );
        assert_eq!(
            name(Mode::from_settings(some("t"), None, pull.clone(), true)),
            "off"
        );
        assert_eq!(
            name(Mode::from_settings(
                some("t"),
                some("me/rivens"),
                pull.clone(),
                true
            )),
            "upload me/rivens"
        );
        assert_eq!(
            name(Mode::from_settings(
                some("t"),
                some("me/rivens"),
                pull,
                false
            )),
            "error"
        );
        assert_eq!(
            name(Mode::from_settings(some("t"), None, None, true)),
            "error"
        );
    }

    #[test]
    fn only_compacted_changed_days_are_prepared() {
        let root = TempRoot::new();
        for date in ["2026-11-07", "2026-11-08", "2026-11-09"] {
            for table in ["auctions", "attributes"] {
                root.write(
                    &format!("parquet/{table}/date={date}/compact-1.parquet"),
                    b"rows",
                );
            }
        }
        root.write("parquet/gone/date=2026-11-08/part-2.parquet", b"gone");
        root.write("parquet/auctions/date=2026-11-08/part-3.tmp", b"half");

        assert!(prepare(&root.0, "me/rivens", false).unwrap().uncompacted);

        root.write(
            maintenance::COMPACTION_STATE,
            br#"{"compacted_before":"2026-11-09"}"#,
        );
        let prepared = prepare(&root.0, "me/rivens", false).unwrap();
        let dates: Vec<_> = prepared.pending.iter().map(|d| d.date.as_str()).collect();
        assert_eq!(dates, ["2026-11-07", "2026-11-08"]);
        assert_eq!(
            prepared.pending[1].files.keys().collect::<Vec<_>>(),
            [
                "attributes/date=2026-11-08/compact-1.parquet",
                "auctions/date=2026-11-08/compact-1.parquet",
                "gone/date=2026-11-08/part-2.parquet",
            ]
        );
        assert_eq!(
            fs::read(
                root.0
                    .join(SNAPSHOT)
                    .join("gone/date=2026-11-08/part-2.parquet")
            )
            .unwrap(),
            b"gone"
        );

        // Uploaded days are skipped until their files change, or the check is forced.
        let mut state = prepared.state;
        for day in prepared.pending {
            state.days.insert(day.date, day.files);
        }
        save_state(&root.0, &state).unwrap();
        assert!(
            prepare(&root.0, "me/rivens", false)
                .unwrap()
                .pending
                .is_empty()
        );
        assert_eq!(
            prepare(&root.0, "me/rivens", true).unwrap().pending.len(),
            2
        );
        root.write("parquet/gone/date=2026-11-07/compact-4.parquet", b"more");
        let changed = prepare(&root.0, "me/rivens", false).unwrap().pending;
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].date, "2026-11-07");

        // A different dataset starts over.
        assert_eq!(
            prepare(&root.0, "me/other", false).unwrap().pending.len(),
            2
        );
    }

    #[test]
    fn operations_mirror_the_local_day() {
        let day = Day {
            date: "2026-11-08".into(),
            files: BTreeMap::from([
                ("auctions/date=2026-11-08/compact-2.parquet".into(), 10),
                ("gone/date=2026-11-08/part-1.parquet".into(), 3),
            ]),
        };
        let remote = BTreeMap::from([
            ("README.md".into(), 1),
            ("auctions/date=2026-11-08/part-1.parquet".into(), 4),
            ("auctions/date=2026-11-07/compact-1.parquet".into(), 9),
            ("gone/date=2026-11-08/part-1.parquet".into(), 3),
        ]);
        let operations = day_operations(&day, &remote, Path::new("/snap"));
        let summary: Vec<String> = operations
            .iter()
            .map(|op| match op {
                CommitOperation::Add { path_in_repo, .. } => format!("+{path_in_repo}"),
                CommitOperation::Delete { path_in_repo } => format!("-{path_in_repo}"),
            })
            .collect();
        assert_eq!(
            summary,
            [
                "+auctions/date=2026-11-08/compact-2.parquet",
                "-auctions/date=2026-11-08/part-1.parquet",
            ]
        );
    }

    #[test]
    fn card_lists_every_table_and_links_back() {
        let card = dataset_card("LLukas22/wf-riven-data", &[]);
        for table in Table::ALL {
            assert!(card.contains(&format!("data_files: \"{table}/*/*.parquet\"")));
        }
        assert!(card.starts_with("---\npretty_name: Warframe riven auctions\nconfigs:\n"));
        assert!(card.contains("load_dataset(\"LLukas22/wf-riven-data\", \"auctions\""));
        assert!(card.contains("(https://github.com/LLukas22/warframe-market-tracker)"));
    }

    #[test]
    fn only_placeholder_readmes_are_replaced_keeping_their_settings() {
        // What the Hub creates for a new dataset with a license.
        let settings = placeholder_settings("---\nlicense: cc-by-nc-sa-4.0\n---\n").unwrap();
        assert_eq!(settings, ["license: cc-by-nc-sa-4.0"]);
        let card = dataset_card("me/rivens", &settings);
        assert!(card.starts_with("---\nlicense: cc-by-nc-sa-4.0\npretty_name: "));

        assert_eq!(placeholder_settings("  \n"), Some(Vec::new()));
        assert!(placeholder_settings("---\nlicense: mit\n---\n\n# My rivens\n").is_none());
        assert!(placeholder_settings("# My rivens\n").is_none());
        let configured = "---\nconfigs:\n- config_name: all\n  data_files: \"*/*\"\n---\n";
        assert!(placeholder_settings(configured).is_none());

        let named = placeholder_settings("---\npretty_name: Rivens\n---").unwrap();
        assert!(!dataset_card("me/rivens", &named).contains("Warframe riven auctions\nconfigs"));
    }
}
