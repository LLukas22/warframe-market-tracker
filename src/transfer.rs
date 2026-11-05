//! `/api/data/export` and `/api/data/import`: moving a whole dataset between servers as one
//! `.tar.gz`. Both need `WFMT_ADMIN_TOKEN`, and stay disabled without it.
//!
//! An import is checked completely before anything changes. It then becomes `transfer/ready`
//! with one rename, and from that point on `apply_ready` finishes the swap, again after a crash
//! if needed. The replaced data moves to `backup-<time>/`, like `reprocess --replace`.

use std::fs::{self, File};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use dioxus::fullstack::axum::Form;
use dioxus::fullstack::axum::body::Body;
use dioxus::fullstack::axum::http::{HeaderMap, StatusCode, header};
use dioxus::fullstack::axum::response::{IntoResponse, Response};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use futures_util::StreamExt;
use parquet::file::reader::SerializedFileReader;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio_util::io::ReaderStream;
use wfmt_scraper::dataset::{DatasetLock, sync_dir, unique_id};
use wfmt_scraper::maintenance::RetentionState;

const MANIFEST: &str = "wfmt-export.json";
const FORMAT: u32 = 1;
/// Everything that makes up a dataset. The scraper's status and locks stay with their server.
const RETENTION: &str = "storage/raw-retention.json";
const ENTRIES: [&str; 7] = [
    "parquet",
    "raw",
    "seen",
    "cache",
    "rivens.jsonl",
    "state.json",
    RETENTION,
];
const TRANSFER_DIR: &str = "transfer";
const READY: &str = "transfer/ready";
const BACKUP_NAME: &str = "backup-name";
const MAX_UPLOAD: u64 = 64 << 30;
const MAX_UNPACKED: u64 = 256 << 30;

/// One export or import at a time.
static BUSY: Mutex<()> = Mutex::const_new(());

/// The response to send if this token doesn't allow a transfer.
async fn rejection(given: &str) -> Option<Response> {
    let denied = crate::admin::check(given).await.err()?;
    Some(text(denied.status(), denied.message()))
}

fn text(status: StatusCode, message: impl Into<String>) -> Response {
    (status, message.into()).into_response()
}

const BUSY_MESSAGE: &str = "Another export or upload is still running.";
const OTHER_SERVER: &str = "Another server is using this data directory. Stop it first.";

fn files_below(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        if entry.file_type()?.is_dir() {
            files_below(&entry.path(), out)?;
        } else {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Writes the dataset at `root` to `out`. Nothing may be writing to it meanwhile.
fn write_archive(root: &Path, out: &Path) -> Result<()> {
    ensure!(
        root.join("parquet").is_dir()
            && root.join("rivens.jsonl").is_file()
            && root.join("state.json").is_file(),
        "There is no scraped data to export yet."
    );
    let gz = GzEncoder::new(File::create(out)?, Compression::fast());
    let mut tar = tar::Builder::new(gz);

    let manifest = serde_json::to_vec(&serde_json::json!({
        "format": FORMAT,
        "created_at": Utc::now().to_rfc3339(),
    }))?;
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, MANIFEST, manifest.as_slice())?;

    for entry in ENTRIES {
        let path = root.join(entry);
        let mut files = Vec::new();
        if path.is_dir() {
            files_below(&path, &mut files)?;
        } else if path.exists() {
            files.push(path);
        }
        for file in files {
            // Left by a write that never finished, or one still running for the image cache.
            if file.extension().is_some_and(|e| e == "tmp") {
                continue;
            }
            tar.append_path_with_name(&file, file.strip_prefix(root)?)?;
        }
    }
    tar.into_inner()?.finish()?.sync_all()?;
    Ok(())
}

fn allowed(path: &Path) -> bool {
    path.components().all(|c| matches!(c, Component::Normal(_)))
        && (path == Path::new(MANIFEST) || ENTRIES.iter().any(|e| path.starts_with(e)))
}

/// Unpacks into the empty directory `stage`. Only regular files on known paths are accepted, so
/// an archive can't write anywhere else.
fn unpack(archive: &Path, stage: &Path) -> Result<()> {
    let mut gz = GzDecoder::new(BufReader::new(File::open(archive)?));
    {
        let mut tar = tar::Archive::new(&mut gz);
        let mut remaining = MAX_UNPACKED;
        for entry in tar.entries()? {
            let mut entry = entry.context("The file is damaged or not an export")?;
            let path = entry.path()?.into_owned();
            let kind = entry.header().entry_type();
            ensure!(
                kind.is_file() && allowed(&path),
                "Unexpected entry in the export: {}",
                path.display()
            );
            let destination = stage.join(&path);
            fs::create_dir_all(destination.parent().unwrap())?;
            let mut file = File::create_new(&destination)
                .with_context(|| format!("Bad entry in the export: {}", path.display()))?;
            let written = io::copy(&mut (&mut entry).take(remaining), &mut file)?;
            ensure!(
                written == entry.size(),
                "The export unpacks to more than {} GiB, which is the limit",
                MAX_UNPACKED >> 30
            );
            remaining -= written;
        }
    }
    // Tar stops at its end marker, the gzip checksum comes after it.
    io::copy(&mut gz, &mut io::sink()).context("The file is damaged or incomplete")?;
    Ok(())
}

/// Checks an unpacked export and fills in what a dataset may lack.
fn validate(stage: &Path) -> Result<()> {
    let manifest = fs::read(stage.join(MANIFEST)).context("This is not a data export")?;
    let manifest: serde_json::Value =
        serde_json::from_slice(&manifest).context("This is not a data export")?;
    ensure!(
        manifest["format"] == FORMAT,
        "This export has a format this version can't read"
    );
    ensure!(
        stage.join("parquet").is_dir()
            && stage.join("rivens.jsonl").is_file()
            && stage.join("state.json").is_file(),
        "This export is incomplete"
    );

    let mut files = Vec::new();
    files_below(&stage.join("parquet"), &mut files)?;
    let mut parquet = 0;
    for file in files
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "parquet"))
    {
        SerializedFileReader::new(File::open(file)?)
            .with_context(|| format!("Damaged file in the export: {}", file.display()))?;
        parquet += 1;
    }
    ensure!(parquet > 0, "This export has no scraped data");

    for dir in ["raw", "seen", "cache"] {
        fs::create_dir_all(stage.join(dir))?;
    }
    // Always present, so that applying it also replaces this server's record.
    let retention = stage.join(RETENTION);
    if !retention.exists() {
        fs::create_dir_all(retention.parent().unwrap())?;
        fs::write(&retention, serde_json::to_vec(&RetentionState::default())?)?;
    }
    Ok(())
}

fn sync_tree(dir: &Path) -> Result<()> {
    let mut files = Vec::new();
    files_below(dir, &mut files)?;
    for file in &files {
        File::open(file)?.sync_all()?;
    }
    let mut dirs: Vec<&Path> = files.iter().filter_map(|f| f.parent()).collect();
    dirs.dedup();
    for dir in dirs {
        sync_dir(dir)?;
    }
    sync_dir(dir)?;
    Ok(())
}

/// Everything that can reject an upload. Loading a real engine on the result means a dataset the
/// scraper can't open never replaces one it can.
fn prepare(archive: &Path, stage: &Path) -> Result<()> {
    unpack(archive, stage)?;
    validate(stage)?;
    drop(crate::worker::open(stage.to_owned()).context("The scraper can't open this export")?);
    sync_tree(stage)
}

/// The commit point of an import: after this rename it finishes even across a crash.
fn commit(root: &Path, stage: &Path) -> Result<String> {
    let backup = format!("backup-{}", Utc::now().format("%Y%m%dT%H%M%S"));
    let name = stage.join(BACKUP_NAME);
    fs::write(&name, &backup)?;
    File::open(&name)?.sync_all()?;
    sync_dir(stage)?;
    fs::rename(stage, root.join(READY))?;
    sync_dir(&root.join(TRANSFER_DIR))?;
    Ok(backup)
}

/// Swaps in a committed import. Every step can run again: an entry still in `ready` means the
/// one at its destination is the old one.
fn apply_ready(root: &Path) -> Result<()> {
    let ready = root.join(READY);
    if !ready.exists() {
        return Ok(());
    }
    let _lock = DatasetLock::write(root)?;
    let backup = root.join(fs::read_to_string(ready.join(BACKUP_NAME))?.trim());
    for entry in ENTRIES {
        let new = ready.join(entry);
        if !new.exists() {
            continue;
        }
        let current = root.join(entry);
        let parent = current.parent().unwrap();
        fs::create_dir_all(parent)?;
        if current.exists() {
            let old = backup.join(entry);
            fs::create_dir_all(old.parent().unwrap())?;
            fs::rename(&current, &old)?;
            sync_dir(old.parent().unwrap())?;
        }
        fs::rename(&new, &current)?;
        sync_dir(parent)?;
    }
    sync_dir(root)?;
    fs::remove_dir_all(&ready)?;
    sync_dir(&root.join(TRANSFER_DIR))?;
    Ok(())
}

/// Run once before the scraper starts: finishes an interrupted import and removes leftovers.
pub async fn recover(root: PathBuf) -> Result<()> {
    let Some(lock) = crate::worker::lock(&root).await? else {
        return Ok(());
    };
    tokio::task::spawn_blocking(move || {
        let _lock = lock;
        apply_ready(&root)?;
        let dir = root.join(TRANSFER_DIR);
        if dir.exists() {
            fs::remove_dir_all(dir)?;
        }
        Ok(())
    })
    .await?
}

#[derive(serde::Deserialize)]
pub struct ExportForm {
    token: String,
}

/// Builds the archive with the scraper stopped, then lets it continue during the download. The
/// file is already unlinked, so a cancelled download leaves nothing behind.
async fn build_export(root: PathBuf) -> Result<File> {
    crate::worker::stop().await;
    let result = async {
        let Some(lock) = crate::worker::lock(&root).await? else {
            bail!(OTHER_SERVER);
        };
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let _snapshot = DatasetLock::read(&root)?;
            fs::create_dir_all(root.join(TRANSFER_DIR))?;
            let path = root
                .join(TRANSFER_DIR)
                .join(format!("export-{}.tar.gz", unique_id()));
            let written = write_archive(&root, &path).and_then(|()| Ok(File::open(&path)?));
            let _ = fs::remove_file(&path);
            written
        })
        .await?
    }
    .await;
    if let Err(error) = crate::worker::start().await {
        eprintln!("failed to restart the scraper: {error:#}");
    }
    result
}

pub async fn export(Form(form): Form<ExportForm>) -> Response {
    if let Some(response) = rejection(&form.token).await {
        return response;
    }
    let Ok(_busy) = BUSY.try_lock() else {
        return text(StatusCode::CONFLICT, BUSY_MESSAGE);
    };
    let file = match build_export(crate::data::data_dir()).await {
        Ok(file) => file,
        Err(error) => return text(StatusCode::CONFLICT, format!("{error:#}")),
    };
    let length = file.metadata().map_or(0, |m| m.len());
    let name = format!("wfmt-data-{}.tar.gz", Utc::now().format("%Y-%m-%d"));
    (
        [
            (header::CONTENT_TYPE, "application/gzip".to_owned()),
            (header::CONTENT_LENGTH, length.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file))),
    )
        .into_response()
}

/// Saves the request body to `path`, without ever holding it all in memory.
async fn receive(body: Body, path: &Path) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let mut file = tokio::fs::File::create(path).await?;
    let mut stream = body.into_data_stream();
    let mut received = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("The upload was interrupted")?;
        received += chunk.len() as u64;
        ensure!(
            received <= MAX_UPLOAD,
            "The file is larger than {} GiB, which is the limit",
            MAX_UPLOAD >> 30
        );
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Replaces the data with the prepared `stage`. The scraper continues afterwards either way.
async fn swap(root: PathBuf, stage: PathBuf) -> Result<String> {
    crate::worker::stop().await;
    let result = async {
        let Some(lock) = crate::worker::lock(&root).await? else {
            bail!(OTHER_SERVER);
        };
        let root = root.clone();
        tokio::task::spawn_blocking(move || {
            let _lock = lock;
            let backup = commit(&root, &stage)?;
            apply_ready(&root)?;
            crate::data::reload();
            Ok(backup)
        })
        .await?
    }
    .await;
    let restart = crate::worker::start().await;
    let backup = result?;
    restart.with_context(|| {
        format!("Imported, previous data is in {backup}, but the scraper didn't restart")
    })?;
    Ok(backup)
}

async fn import_upload(body: Body, root: PathBuf) -> Result<String> {
    let id = unique_id();
    let transfer = root.join(TRANSFER_DIR);
    let upload = transfer.join(format!("upload-{id}.tar.gz"));
    let stage = transfer.join(format!("stage-{id}"));
    let result = async {
        receive(body, &upload).await?;
        fs::create_dir_all(&stage)?;
        let (archive, unpacked) = (upload.clone(), stage.clone());
        tokio::task::spawn_blocking(move || prepare(&archive, &unpacked)).await??;
        fs::remove_file(&upload)?;
        swap(root, stage.clone()).await
    }
    .await;
    let _ = fs::remove_file(&upload);
    let _ = fs::remove_dir_all(&stage);
    result
}

pub async fn import(headers: HeaderMap, body: Body) -> Response {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if let Some(response) = rejection(token).await {
        return response;
    }
    let Ok(_busy) = BUSY.try_lock() else {
        return text(StatusCode::CONFLICT, BUSY_MESSAGE);
    };
    match import_upload(body, crate::data::data_dir()).await {
        Ok(backup) => text(
            StatusCode::OK,
            format!("Imported. The previous data was moved to {backup}."),
        ),
        Err(error) => text(StatusCode::BAD_REQUEST, format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;

    use arrow::array::{ArrayRef, Int32Array, RecordBatch};
    use parquet::arrow::ArrowWriter;

    struct Temp(PathBuf);

    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("wfmt-transfer-{}", unique_id()));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn write_parquet(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let column: ArrayRef = Arc::new(Int32Array::from(vec![1, 2, 3]));
        let batch = RecordBatch::try_from_iter([("x", column)]).unwrap();
        let mut writer =
            ArrowWriter::try_new(File::create(path).unwrap(), batch.schema(), None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    fn dataset(root: &Path, marker: &str) {
        write_parquet(&root.join("parquet/auctions/date=2026-11-01/part-1.parquet"));
        write(&root.join("rivens.jsonl"), b"");
        write(&root.join("state.json"), br#"{"last_fetch":{}}"#);
        write(&root.join("seen/rubico.json"), marker.as_bytes());
        write(&root.join("raw/2026-11-01/.committed/a.json"), b"[]");
        write(&root.join("cache/images/rubico.png"), marker.as_bytes());
    }

    fn exported(root: &Path, out: &Path) -> PathBuf {
        let archive = out.join("export.tar.gz");
        write_archive(root, &archive).unwrap();
        archive
    }

    #[test]
    fn export_contains_the_dataset_and_nothing_else() {
        let (source, out, stage) = (Temp::new(), Temp::new(), Temp::new());
        dataset(&source.0, "one");
        write(&source.0.join("scraper/status.json"), b"{}");
        write(&source.0.join("storage/lock"), b"");
        write(&source.0.join("backup-old/state.json"), b"{}");
        write(&source.0.join("cache/images/half.tmp"), b"");

        unpack(&exported(&source.0, &out.0), &stage.0).unwrap();
        validate(&stage.0).unwrap();

        assert_eq!(fs::read(stage.0.join("seen/rubico.json")).unwrap(), b"one");
        assert!(stage.0.join("raw/2026-11-01/.committed/a.json").exists());
        assert!(stage.0.join(RETENTION).exists());
        for excluded in [
            "scraper",
            "storage/lock",
            "backup-old",
            "cache/images/half.tmp",
        ] {
            assert!(!stage.0.join(excluded).exists(), "{excluded} was exported");
        }
    }

    #[test]
    fn nothing_to_export_is_an_error() {
        let (source, out) = (Temp::new(), Temp::new());
        assert!(write_archive(&source.0, &out.0.join("x.tar.gz")).is_err());
    }

    fn raw_tar(out: &Path, name: &str, kind: tar::EntryType) -> PathBuf {
        let path = out.join("evil.tar.gz");
        let mut tar = tar::Builder::new(GzEncoder::new(
            File::create(&path).unwrap(),
            Compression::fast(),
        ));
        let mut header = tar::Header::new_gnu();
        // `set_path` refuses these, a hostile archive wouldn't.
        header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(kind);
        header.set_cksum();
        tar.append(&header, &b"x"[..]).unwrap();
        tar.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn archives_cant_write_outside_the_dataset() {
        for (name, kind) in [
            ("../evil", tar::EntryType::Regular),
            ("parquet/../../evil", tar::EntryType::Regular),
            ("/tmp/wfmt-evil", tar::EntryType::Regular),
            ("backup-1/evil", tar::EntryType::Regular),
            ("storage/lock", tar::EntryType::Regular),
            ("parquet/link", tar::EntryType::Symlink),
        ] {
            let (out, stage) = (Temp::new(), Temp::new());
            let archive = raw_tar(&out.0, name, kind);
            assert!(unpack(&archive, &stage.0).is_err(), "{name} was accepted");
            assert_eq!(fs::read_dir(&stage.0).unwrap().count(), 0, "{name}");
        }
    }

    #[test]
    fn damaged_or_foreign_uploads_are_rejected() {
        let (source, out) = (Temp::new(), Temp::new());
        dataset(&source.0, "one");
        let archive = exported(&source.0, &out.0);
        let bytes = fs::read(&archive).unwrap();

        // Cut anywhere, including right before the gzip checksum.
        for keep in [bytes.len() / 2, bytes.len() - 4] {
            let (cut, stage) = (out.0.join("cut.tar.gz"), Temp::new());
            fs::write(&cut, &bytes[..keep]).unwrap();
            assert!(
                unpack(&cut, &stage.0).is_err(),
                "cut at {keep} was accepted"
            );
        }

        let stage = Temp::new();
        write(&stage.0.join("state.json"), b"{}");
        assert!(validate(&stage.0).is_err(), "no manifest");

        let stage = Temp::new();
        dataset(&stage.0, "one");
        write(&stage.0.join(MANIFEST), br#"{"format":2}"#);
        assert!(validate(&stage.0).is_err(), "future format");

        let stage = Temp::new();
        dataset(&stage.0, "one");
        write(&stage.0.join(MANIFEST), br#"{"format":1}"#);
        let mut part = File::options()
            .write(true)
            .open(
                stage
                    .0
                    .join("parquet/auctions/date=2026-11-01/part-1.parquet"),
            )
            .unwrap();
        part.write_all(b"garbage").unwrap();
        part.set_len(7).unwrap();
        assert!(validate(&stage.0).is_err(), "broken parquet");
    }

    #[test]
    fn prepare_rejects_state_the_scraper_cant_open() {
        let (source, out) = (Temp::new(), Temp::new());
        dataset(&source.0, "one");
        let good = Temp::new();
        prepare(&exported(&source.0, &out.0), &good.0).unwrap();

        write(&source.0.join("state.json"), b"not json");
        let bad = Temp::new();
        assert!(prepare(&exported(&source.0, &out.0), &bad.0).is_err());
    }

    fn committed(root: &Temp) {
        dataset(&root.0, "old");
        write(&root.0.join("storage/lock"), b"");
        write(&root.0.join("scraper/status.json"), b"keep");
        let stage = root.0.join(TRANSFER_DIR).join("stage");
        dataset(&stage, "new");
        write(&stage.join(MANIFEST), br#"{"format":1}"#);
        validate(&stage).unwrap();
        fs::write(stage.join(BACKUP_NAME), "backup-test").unwrap();
        fs::rename(stage, root.0.join(READY)).unwrap();
    }

    fn assert_swapped(root: &Temp) {
        assert_eq!(fs::read(root.0.join("seen/rubico.json")).unwrap(), b"new");
        assert_eq!(
            fs::read(root.0.join("backup-test/seen/rubico.json")).unwrap(),
            b"old"
        );
        assert_eq!(
            fs::read(root.0.join("scraper/status.json")).unwrap(),
            b"keep"
        );
        assert!(root.0.join("storage/lock").exists());
        assert!(!root.0.join(READY).exists());
    }

    #[test]
    fn import_replaces_the_data_and_keeps_a_backup() {
        let root = Temp::new();
        committed(&root);
        apply_ready(&root.0).unwrap();
        assert_swapped(&root);
    }

    #[test]
    fn interrupted_import_finishes_on_every_possible_crash_point() {
        let move_old_away = |root: &Temp, entry: &str| {
            if root.0.join(entry).exists() {
                let old = root.0.join("backup-test").join(entry);
                fs::create_dir_all(old.parent().unwrap()).unwrap();
                fs::rename(root.0.join(entry), old).unwrap();
            }
        };
        for finished in 0..ENTRIES.len() {
            // The crash comes either between two entries or halfway through the next one.
            for halfway in [false, true] {
                let root = Temp::new();
                committed(&root);
                for entry in &ENTRIES[..finished] {
                    move_old_away(&root, entry);
                    let current = root.0.join(entry);
                    fs::create_dir_all(current.parent().unwrap()).unwrap();
                    fs::rename(root.0.join(READY).join(entry), current).unwrap();
                }
                if halfway {
                    move_old_away(&root, ENTRIES[finished]);
                }
                apply_ready(&root.0).unwrap();
                assert_swapped(&root);
                apply_ready(&root.0).unwrap();
                assert_swapped(&root);
            }
        }
    }
}
