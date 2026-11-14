//! `WFMT_HF_PULL_DATASET`: the server downloads its data from a Hugging Face dataset, one that
//! another tracker uploads to, instead of scraping warframe.market itself.
//!
//! Every hour it asks the Hub for the dataset's newest commit, and when that changed, downloads
//! the days that are new or different, oldest first. Each day replaces the local copy of that day
//! in one dataset transaction, so the app never sees half a day. Days the dataset doesn't have
//! stay as they are. The weapon list comes along, for names, groups and images.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::NaiveDate;
use hf_hub::{HFRepository, RepoTypeDataset};
use parquet::file::reader::SerializedFileReader;
use serde::{Deserialize, Serialize};
use wfmt_scraper::dataset::{DatasetLock, Transaction, durable_write};

use super::{CATALOG, Config, LOCAL_CATALOG, day_files, every_hour, live, now_ms};
use wfmt_types::layout::{DATE_PREFIX, PARQUET_DIR, Table};

const STATE: &str = "hub/pull.json";
/// Where files are downloaded to before they're moved into the data.
const DOWNLOAD: &str = "hub/download";

/// What has been downloaded so far.
#[derive(Default, Serialize, Deserialize)]
struct State {
    dataset: String,
    /// The commit everything was downloaded from.
    commit: Option<String>,
    /// When the last download changed anything.
    updated_at: Option<i64>,
    /// The newest day in the dataset.
    last_day: Option<String>,
}

fn load_state(root: &Path, dataset: &str) -> Result<State> {
    let state: State = match fs::read(root.join(STATE)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading the download state")?,
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

fn commit_url(dataset: &str, commit: &str) -> String {
    format!("https://huggingface.co/datasets/{dataset}/commit/{commit}")
}

fn publish(state: &State) {
    let mut live = live();
    live.last_upload_at = state.updated_at;
    live.last_day = state.last_day.clone();
    live.commit_url = state
        .commit
        .as_deref()
        .map(|commit| commit_url(&state.dataset, commit));
}

pub(super) fn start(root: PathBuf, config: Config) {
    match load_state(&root, &config.dataset) {
        Ok(state) => publish(&state),
        Err(error) => live().error = Some(format!("{error:#}")),
    }
    tokio::spawn(async move {
        let repo = match super::repository(&config) {
            Ok(repo) => repo,
            Err(error) => {
                live().error = Some(format!("{error:#}"));
                return;
            }
        };
        // There's nothing to show until the first download, so start right away.
        every_hour(std::time::Duration::ZERO, |forced| {
            let (root, repo, dataset) = (root.clone(), repo.clone(), config.dataset.clone());
            async move {
                let result = pull(&root, &repo, &dataset, forced).await;
                let _ = fs::remove_dir_all(root.join(DOWNLOAD));
                result
            }
        })
        .await;
    });
}

/// The date of a table file in the repository, `None` for anything else.
fn table_file(path: &str) -> Option<NaiveDate> {
    let mut parts = path.split('/');
    let (table, folder, name) = (parts.next()?, parts.next()?, parts.next()?);
    let date = folder.strip_prefix(DATE_PREFIX)?.parse().ok()?;
    let valid = parts.next().is_none()
        && table.parse::<Table>().is_ok()
        && name.ends_with(".parquet")
        && !name.starts_with('.');
    valid.then_some(date)
}

#[derive(Clone, Debug, PartialEq)]
struct DayPlan {
    date: String,
    /// Paths in the repository, which are also the paths below `parquet/`.
    fetch: Vec<String>,
    delete: Vec<String>,
}

/// The days whose local copy differs from the dataset's, oldest first, and what changes them.
fn plan(root: &Path, remote: &BTreeMap<String, u64>) -> Result<Vec<DayPlan>> {
    let mut days: BTreeMap<NaiveDate, BTreeMap<&str, u64>> = BTreeMap::new();
    for (path, size) in remote {
        if let Some(date) = table_file(path) {
            days.entry(date).or_default().insert(path, *size);
        }
    }
    let _lock = DatasetLock::read(root)?;
    let mut plans = Vec::new();
    for (date, files) in days {
        let date = date.to_string();
        let local = day_files(root, &date)?;
        let fetch: Vec<String> = files
            .iter()
            .filter(|(path, size)| local.get(**path) != Some(*size))
            .map(|(path, _)| (*path).to_owned())
            .collect();
        let delete: Vec<String> = local
            .keys()
            .filter(|path| !files.contains_key(path.as_str()))
            .cloned()
            .collect();
        if !fetch.is_empty() || !delete.is_empty() {
            plans.push(DayPlan {
                date,
                fetch,
                delete,
            });
        }
    }
    Ok(plans)
}

/// Replaces the local copy of a day with the files downloaded for it, in one transaction.
fn apply(root: &Path, download: &Path, day: &DayPlan) -> Result<()> {
    for path in &day.fetch {
        let file = download.join(path);
        SerializedFileReader::new(File::open(&file)?)
            .with_context(|| format!("{path} from the dataset is damaged"))?;
    }
    let _lock = DatasetLock::write(root)?;
    let mut tx = Transaction::new(root)?;
    for path in &day.fetch {
        let staged = tx.stage(Path::new(PARQUET_DIR).join(path));
        fs::rename(download.join(path), staged)?;
    }
    for path in &day.delete {
        tx.delete(Path::new(PARQUET_DIR).join(path));
    }
    tx.commit()
}

/// Moves the downloaded weapon list into place, if it differs. `true` if it did.
fn apply_catalog(root: &Path, downloaded: &Path) -> Result<bool> {
    let bytes = fs::read(downloaded)?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .context("The weapon list in the dataset is damaged")?;
    let local = root.join(LOCAL_CATALOG);
    if fs::read(&local).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    durable_write(&local, &bytes)?;
    Ok(true)
}

async fn download(
    repo: &HFRepository<RepoTypeDataset>,
    commit: &str,
    dir: &Path,
    path: &str,
    size: u64,
) -> Result<()> {
    let file = repo
        .download_file()
        .filename(path)
        .local_dir(dir.to_owned())
        .revision(commit)
        .send()
        .await
        .with_context(|| format!("Downloading {path} failed"))?;
    let got = fs::metadata(&file)?.len();
    ensure!(
        got == size,
        "Downloading {path} failed: got {got} bytes instead of {size}"
    );
    Ok(())
}

async fn pull(
    root: &Path,
    repo: &HFRepository<RepoTypeDataset>,
    dataset: &str,
    forced: bool,
) -> Result<String> {
    let mut state = load_state(root, dataset)?;
    let info = repo.info().send().await.with_context(|| {
        format!(
            "Can't reach the dataset {dataset}. Check its name, and WFMT_HF_TOKEN if it's private"
        )
    })?;
    let commit = info
        .sha
        .context("The Hub didn't say which commit the dataset is at")?;
    if !forced && state.commit.as_deref() == Some(commit.as_str()) {
        return Ok("Up to date.".into());
    }
    let remote = super::remote_files(repo, Some(&commit))
        .await
        .with_context(|| format!("Can't list the files of {dataset}"))?;
    let plans = {
        let (root, remote) = (root.to_owned(), remote.clone());
        tokio::task::spawn_blocking(move || plan(&root, &remote)).await??
    };

    let dir = root.join(DOWNLOAD);
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    let total = plans.len();
    for (done, day) in plans.iter().enumerate() {
        live().outcome = Some(format!(
            "Downloading {} ({} of {total})…",
            day.date,
            done + 1
        ));
        for path in &day.fetch {
            download(repo, &commit, &dir, path, remote[path]).await?;
        }
        {
            let (root, dir, day) = (root.to_owned(), dir.clone(), day.clone());
            tokio::task::spawn_blocking(move || apply(&root, &dir, &day)).await??;
        }
        state.updated_at = Some(now_ms());
        state.last_day = state.last_day.clone().max(Some(day.date.clone()));
        publish(&state);
    }

    let mut catalog_updated = false;
    if let Some(size) = remote.get(CATALOG) {
        download(repo, &commit, &dir, CATALOG, *size).await?;
        catalog_updated = apply_catalog(root, &dir.join(CATALOG))?;
        if catalog_updated {
            state.updated_at = Some(now_ms());
        }
    }

    state.last_day = remote
        .keys()
        .filter_map(|path| table_file(path))
        .max()
        .map(|date| date.to_string());
    state.commit = Some(commit);
    durable_write(&root.join(STATE), &serde_json::to_vec(&state)?)?;
    publish(&state);

    Ok(match (total, catalog_updated) {
        (0, false) if forced => "Everything was already here.".into(),
        (0, false) => "Up to date.".into(),
        (0, true) => "Updated the weapon list.".into(),
        (n, _) => format!("Downloaded {}.", super::days_text(n)),
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
    fn only_table_files_are_downloaded() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 8);
        assert_eq!(
            table_file("auctions/date=2026-11-08/compact-1.parquet"),
            date
        );
        assert_eq!(table_file("gone/date=2026-11-08/part-1.parquet"), date);
        for other in [
            "README.md",
            CATALOG,
            "auctions/date=2026-11-08/notes.txt",
            "auctions/date=2026-11-08/.parquet",
            "auctions/date=soon/compact-1.parquet",
            "auctions/date=2026-11-08/deeper/compact-1.parquet",
            "secrets/date=2026-11-08/compact-1.parquet",
            "../auctions/date=2026-11-08/compact-1.parquet",
        ] {
            assert_eq!(table_file(other), None, "{other}");
        }
    }

    #[test]
    fn plan_mirrors_the_datasets_days_and_keeps_the_rest() {
        let root = TempRoot::new();
        // Already here.
        root.write(
            "parquet/auctions/date=2026-11-07/compact-1.parquet",
            b"same",
        );
        // Replaced by the dataset's compacted parts.
        root.write("parquet/auctions/date=2026-11-08/part-1.parquet", b"part");
        root.write("parquet/attributes/date=2026-11-08/part-1.parquet", b"part");
        // Not in the dataset, stays.
        root.write("parquet/auctions/date=2026-11-10/part-2.parquet", b"today");

        let remote = BTreeMap::from([
            ("README.md".to_owned(), 10),
            (CATALOG.to_owned(), 10),
            ("auctions/date=2026-11-07/compact-1.parquet".to_owned(), 4),
            ("auctions/date=2026-11-08/compact-2.parquet".to_owned(), 9),
            ("attributes/date=2026-11-08/compact-2.parquet".to_owned(), 9),
            ("gone/date=2026-11-09/part-3.parquet".to_owned(), 2),
        ]);
        let plans = plan(&root.0, &remote).unwrap();
        assert_eq!(
            plans,
            [
                DayPlan {
                    date: "2026-11-08".into(),
                    fetch: vec![
                        "attributes/date=2026-11-08/compact-2.parquet".into(),
                        "auctions/date=2026-11-08/compact-2.parquet".into(),
                    ],
                    delete: vec![
                        "attributes/date=2026-11-08/part-1.parquet".into(),
                        "auctions/date=2026-11-08/part-1.parquet".into(),
                    ],
                },
                DayPlan {
                    date: "2026-11-09".into(),
                    fetch: vec!["gone/date=2026-11-09/part-3.parquet".into()],
                    delete: vec![],
                },
            ]
        );
    }

    fn parquet_bytes() -> Vec<u8> {
        use arrow::array::{ArrayRef, Int32Array, RecordBatch};
        use std::sync::Arc;
        let column: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let batch = RecordBatch::try_from_iter([("x", column)]).unwrap();
        let mut bytes = Vec::new();
        let mut writer =
            parquet::arrow::ArrowWriter::try_new(&mut bytes, batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        bytes
    }

    #[test]
    fn a_downloaded_day_replaces_the_local_one() {
        let root = TempRoot::new();
        root.write("parquet/auctions/date=2026-11-08/part-1.parquet", b"part");
        root.write("parquet/auctions/date=2026-11-09/part-2.parquet", b"keep");
        let new = "auctions/date=2026-11-08/compact-2.parquet";
        root.write(&format!("{DOWNLOAD}/{new}"), &parquet_bytes());
        let day = DayPlan {
            date: "2026-11-08".into(),
            fetch: vec![new.into()],
            delete: vec!["auctions/date=2026-11-08/part-1.parquet".into()],
        };
        apply(&root.0, &root.0.join(DOWNLOAD), &day).unwrap();
        assert_eq!(
            fs::read(root.0.join("parquet").join(new)).unwrap(),
            parquet_bytes()
        );
        assert!(
            !root
                .0
                .join("parquet/auctions/date=2026-11-08/part-1.parquet")
                .exists()
        );
        assert!(
            root.0
                .join("parquet/auctions/date=2026-11-09/part-2.parquet")
                .exists()
        );
        assert!(!root.0.join(DOWNLOAD).join(new).exists());
    }

    #[test]
    fn a_damaged_download_changes_nothing() {
        let root = TempRoot::new();
        root.write("parquet/auctions/date=2026-11-08/part-1.parquet", b"part");
        let new = "auctions/date=2026-11-08/compact-2.parquet";
        root.write(&format!("{DOWNLOAD}/{new}"), b"not parquet");
        let day = DayPlan {
            date: "2026-11-08".into(),
            fetch: vec![new.into()],
            delete: vec!["auctions/date=2026-11-08/part-1.parquet".into()],
        };
        assert!(apply(&root.0, &root.0.join(DOWNLOAD), &day).is_err());
        assert!(
            root.0
                .join("parquet/auctions/date=2026-11-08/part-1.parquet")
                .exists()
        );
        assert!(!root.0.join("parquet").join(new).exists());
    }

    #[test]
    fn the_weapon_list_is_only_replaced_when_it_changed() {
        let root = TempRoot::new();
        let downloaded = root.0.join(DOWNLOAD).join(CATALOG);
        root.write(&format!("{DOWNLOAD}/{CATALOG}"), br#"{"data":[]}"#);
        assert!(apply_catalog(&root.0, &downloaded).unwrap());
        assert_eq!(
            fs::read(root.0.join(LOCAL_CATALOG)).unwrap(),
            br#"{"data":[]}"#
        );
        assert!(!apply_catalog(&root.0, &downloaded).unwrap());
        root.write(&format!("{DOWNLOAD}/{CATALOG}"), b"<html>");
        assert!(apply_catalog(&root.0, &downloaded).is_err());
        assert_eq!(
            fs::read(root.0.join(LOCAL_CATALOG)).unwrap(),
            br#"{"data":[]}"#
        );
    }
}
