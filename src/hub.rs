//! Publishes the data to a Hugging Face dataset repository, set with `WFMT_HF_TOKEN` and
//! `WFMT_HF_DATASET`. Without both, nothing is uploaded.
//!
//! Each finished UTC day is uploaded once maintenance has compacted it, as an exact copy of its
//! `date=` folders: new or changed files are added and files the day no longer has are deleted,
//! in the same commit. `hub/upload.json` remembers the files of every uploaded day, so the hourly
//! check only contacts the Hub when a day is new or has changed. Today is never uploaded, it's
//! still being written. Days that only exist on the Hub stay there.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use futures_util::StreamExt;
use hf_hub::repository::{AddSource, CommitOperation, RepoTreeEntry};
use hf_hub::{HFClient, HFRepository, RepoTypeDataset};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use wfmt_scraper::dataset::{DatasetLock, durable_write};
use wfmt_scraper::maintenance;

use crate::api::HubStatus;

const TABLES: [&str; 3] = ["auctions", "attributes", "gone"];
const STATE: &str = "hub/upload.json";
/// Hard links to the files being uploaded, so compaction can't delete them midway.
const SNAPSHOT: &str = "hub/snapshot";
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
/// Gives the scraper time for its first maintenance pass after a restart.
const FIRST_CHECK: Duration = Duration::from_secs(60);
/// The Hub recommends staying well under 100 files per commit.
const FILES_PER_COMMIT: usize = 60;
const README: &str = "README.md";
/// Larger READMEs were written by someone and are never replaced.
const PLACEHOLDER_MAX: u64 = 4096;

#[derive(Clone)]
struct Config {
    token: String,
    /// `owner/name`.
    dataset: String,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// `None` while uploads are off, an error if only half of the settings are there.
fn config() -> Result<Option<Config>, String> {
    match (env("WFMT_HF_TOKEN"), env("WFMT_HF_DATASET")) {
        (None, None) => Ok(None),
        (Some(token), Some(dataset)) => Ok(Some(Config {
            token,
            dataset: dataset_id(&dataset)?,
        })),
        (None, Some(_)) => Err("WFMT_HF_DATASET is set, but WFMT_HF_TOKEN isn't.".into()),
        (Some(_), None) => Err("WFMT_HF_TOKEN is set, but WFMT_HF_DATASET isn't.".into()),
    }
}

/// Accepts `owner/name` as well as the dataset's URL.
fn dataset_id(value: &str) -> Result<String, String> {
    let id = value.trim_end_matches('/');
    let id = id.strip_prefix("https://huggingface.co/").unwrap_or(id);
    let id = id.strip_prefix("datasets/").unwrap_or(id);
    match id.split('/').collect::<Vec<_>>()[..] {
        [owner, name] if !owner.is_empty() && !name.is_empty() => Ok(id.to_owned()),
        _ => Err(format!(
            "WFMT_HF_DATASET should look like owner/name, not {value:?}."
        )),
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

static LIVE: Mutex<Live> = Mutex::new(Live {
    running: false,
    last_check_at: None,
    outcome: None,
    error: None,
    last_upload_at: None,
    last_day: None,
    commit_url: None,
});
static STARTED: AtomicBool = AtomicBool::new(false);
static FORCE: AtomicBool = AtomicBool::new(false);
static WAKE: Notify = Notify::const_new();

fn live() -> std::sync::MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn publish(state: &State) {
    let mut live = live();
    live.last_upload_at = state.last_upload_at;
    live.last_day = state.days.keys().next_back().cloned();
    live.commit_url = state.commit_url.clone();
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn status() -> HubStatus {
    let config = config();
    let live = live();
    HubStatus {
        dataset: config.as_ref().ok().cloned().flatten().map(|c| c.dataset),
        running: live.running,
        queued: FORCE.load(Ordering::SeqCst),
        last_upload_at: live.last_upload_at,
        last_day: live.last_day.clone(),
        commit_url: live.commit_url.clone(),
        last_check_at: live.last_check_at,
        outcome: live.outcome.clone(),
        error: config.err().or_else(|| live.error.clone()),
    }
}

/// Checks every finished day against the Hub right away, after the current check if one runs.
pub fn request_upload() -> Result<(), String> {
    if config()?.is_none() || !STARTED.load(Ordering::SeqCst) {
        return Err("Uploads to Hugging Face aren't set up on this server.".into());
    }
    FORCE.store(true, Ordering::SeqCst);
    WAKE.notify_one();
    Ok(())
}

/// Starts the hourly check, if uploads are set up.
pub fn start() {
    let Ok(Some(config)) = config() else {
        return;
    };
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let root = crate::data::data_dir();
    match load_state(&root, &config.dataset) {
        Ok(state) => publish(&state),
        Err(error) => live().error = Some(format!("{error:#}")),
    }
    tokio::spawn(run(root, config));
}

async fn run(root: PathBuf, config: Config) {
    let client = match HFClient::builder()
        .token(&config.token)
        .user_agent(wfmt_types::USER_AGENT)
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            live().error = Some(format!("Can't set up the Hugging Face client: {error}"));
            return;
        }
    };
    let (owner, name) = config.dataset.split_once('/').unwrap();
    let repo = client.dataset(owner, name);
    let mut wait = FIRST_CHECK;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = WAKE.notified() => {}
        }
        wait = CHECK_INTERVAL;
        let forced = FORCE.swap(false, Ordering::SeqCst);
        live().running = true;
        let result = sync(&root, &repo, &config.dataset, forced).await;
        let _ = fs::remove_dir_all(root.join(SNAPSHOT));
        let mut live = live();
        live.running = false;
        live.last_check_at = Some(now_ms());
        match result {
            Ok(outcome) => {
                live.outcome = Some(outcome);
                live.error = None;
            }
            Err(error) => {
                eprintln!("hugging face upload failed: {error:#}");
                live.error = Some(format!("{error:#}"));
            }
        }
    }
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
}

/// The dates with data in any table.
fn dates(root: &Path) -> Result<BTreeSet<NaiveDate>> {
    let mut dates = BTreeSet::new();
    for table in TABLES {
        let dir = root.join("parquet").join(table);
        if !dir.is_dir() {
            continue;
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            if let Some(date) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.strip_prefix("date="))
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
    for table in TABLES {
        let folder = format!("{table}/date={date}");
        let dir = root.join("parquet").join(&folder);
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
        });
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
            let source = root.join("parquet").join(path);
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
    })
}

/// Every file in the repository with its size.
async fn remote_files(repo: &HFRepository<RepoTypeDataset>) -> Result<BTreeMap<String, u64>> {
    let stream = repo.list_tree().recursive(true).send()?;
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
    let folders = TABLES.map(|table| format!("{table}/date={}/", day.date));
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
    for table in TABLES {
        header.push(format!("- config_name: {table}"));
        header.push(format!("  data_files: \"{table}/*/*.parquet\""));
        if table == "auctions" {
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

fn days_text(count: usize) -> String {
    if count == 1 {
        "1 day".into()
    } else {
        format!("{count} days")
    }
}

async fn sync(
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
    } = prepared;
    if uncompacted {
        return Ok("Waiting for the scraper's first maintenance pass.".into());
    }
    let check_card = forced || !state.card_checked;
    if pending.is_empty() && !check_card {
        return Ok("Up to date.".into());
    }
    let remote = remote_files(repo).await.with_context(|| {
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
        publish(&state);
    }
    if batches.is_empty()
        && let Some(card) = &card
    {
        repo.create_commit()
            .operations(vec![readme(card)])
            .commit_message("Add the dataset card")
            .send()
            .await
            .context("Adding the dataset card failed")?;
    }

    let mut uploaded = 0;
    for (days, mut operations) in batches {
        let (first, last) = (&days[0].date, &days[days.len() - 1].date);
        let message = if first == last {
            format!("Add {first}")
        } else {
            format!("Add {first} to {last}")
        };
        if let Some(card) = card.take() {
            operations.push(readme(&card));
        }
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
        save_state(root, &state)?;
        publish(&state);
    }
    if check_card && card_problem.is_none() && !state.card_checked {
        state.card_checked = true;
        save_state(root, &state)?;
    }
    let outcome = match (uploaded, card_added, forced) {
        (0, true, _) => "Added the dataset card.".into(),
        (0, false, true) => "Everything was already on the Hub.".into(),
        (0, false, false) => "Up to date.".into(),
        (n, true, _) => format!("Uploaded {} and the dataset card.", days_text(n)),
        (n, false, _) => format!("Uploaded {}.", days_text(n)),
    };
    Ok(match card_problem {
        Some(problem) => format!("{outcome} {problem}."),
        None => outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir()
                .join(format!("wfmt-hub-{}", wfmt_scraper::dataset::unique_id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn write(&self, path: &str, bytes: &[u8]) {
            let path = self.0.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn dataset_accepts_ids_and_urls() {
        assert_eq!(dataset_id("me/rivens").unwrap(), "me/rivens");
        assert_eq!(
            dataset_id("https://huggingface.co/datasets/me/rivens/").unwrap(),
            "me/rivens"
        );
        assert!(dataset_id("rivens").is_err());
        assert!(dataset_id("me/rivens/extra").is_err());
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
        for table in TABLES {
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
