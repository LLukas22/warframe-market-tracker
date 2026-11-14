//! Storage maintenance on archives and parts the scraper wrote: raw retention and broken
//! compactions must never lose data that isn't safely published yet.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use arrow::array::{Int64Array, RecordBatch};
use chrono::{DateTime, TimeDelta, Utc};
use parquet::arrow::ArrowWriter;
use wfmt_dataset::maintenance::{MaintenanceArgs, maintain, prune_feeds, retention_state};
use wfmt_dataset::sink::ParquetSink;
use wfmt_scraper::archive::{RawResponse, Store};

/// The path, and the directory that is removed when this is dropped.
struct TempRoot(PathBuf, #[allow(dead_code)] tempfile::TempDir);

impl TempRoot {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        Ok(Self(temp.path().to_path_buf(), temp))
    }
}

fn options() -> MaintenanceArgs {
    MaintenanceArgs {
        raw_retention_days: 14,
        maintenance_interval_hours: 6,
        compact_target_mb: 128,
    }
}

fn write_part(root: &Path, table: &str, date: &str, name: &str, value: i64) -> Result<()> {
    let batch = RecordBatch::try_from_iter([
        (
            "value",
            Arc::new(Int64Array::from(vec![value])) as arrow::array::ArrayRef,
        ),
        ("extra", Arc::new(Int64Array::from(vec![42]))),
    ])?;
    let dir = root
        .join("parquet")
        .join(table)
        .join(format!("date={date}"));
    fs::create_dir_all(&dir)?;
    let mut writer = ArrowWriter::try_new(File::create(dir.join(name))?, batch.schema(), None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}

fn raw(at: &str, weapon: &str) -> Result<RawResponse> {
    Ok(RawResponse::new(
        at.parse()?,
        weapon.into(),
        "pc".into(),
        Vec::new(),
        Vec::new(),
    ))
}

#[test]
fn feed_archives_expire_in_hours_only_once_committed() -> Result<()> {
    let temp = TempRoot::new()?;
    let root = &temp.0;
    let store = Store::new(root.clone());
    let mut sink = ParquetSink::new(root.join("parquet"));
    let now: DateTime<Utc> = "2026-11-03T12:00:00Z".parse()?;
    let expired = raw("2026-11-03T05:59:59Z", wfmt_types::Feed::Recent.as_str())?;
    let yesterday = raw("2026-11-02T23:00:00Z", wfmt_types::Feed::Popular.as_str())?;
    let fresh = raw("2026-11-03T06:00:01Z", wfmt_types::Feed::Recent.as_str())?;
    let weapon = raw("2026-11-03T01:00:00Z", "kulstar")?;
    let uncommitted = raw("2026-11-03T02:00:00Z", wfmt_types::Feed::Popular.as_str())?;
    for raw in [&yesterday, &expired, &fresh, &weapon] {
        store.save_raw(raw)?;
        sink.prepare(raw.scraped_at)?;
        sink.acknowledge_raw(raw.file_name());
        sink.flush()?;
    }
    store.save_raw(&uncommitted)?;

    assert_eq!(prune_feeds(root, now, TimeDelta::hours(6))?, 2);
    let exists = |raw: &RawResponse| {
        root.join("raw")
            .join(raw.scraped_at.date_naive().to_string())
            .join(raw.file_name())
            .exists()
    };
    assert!(!exists(&expired) && !exists(&yesterday));
    assert!(exists(&fresh), "still inside the window");
    assert!(exists(&weapon), "weapon archives keep their own retention");
    assert!(exists(&uncommitted), "still needed for crash recovery");
    assert_eq!(retention_state(root)?.deleted_files, 2);
    // A deleted archive stays committed, so a restart doesn't look for it.
    assert_eq!(store.pending_raw()?.len(), 1);
    assert_eq!(prune_feeds(root, now, TimeDelta::hours(6))?, 0);
    assert_eq!(retention_state(root)?.deleted_files, 2);
    Ok(())
}

#[test]
fn retention_requires_a_successful_flush_and_keeps_recent_or_unconfirmed_raw() -> Result<()> {
    let temp = TempRoot::new()?;
    let root = &temp.0;
    let store = Store::new(root.clone());
    let mut sink = ParquetSink::new(root.join("parquet"));
    let old = raw("2026-10-01T12:00:00Z", "old")?;
    let pending = raw("2026-10-01T12:00:01Z", "pending")?;
    let recent = raw("2026-11-02T12:00:00Z", "recent")?;
    for raw in [&old, &pending, &recent] {
        store.save_raw(raw)?;
    }
    sink.prepare(old.scraped_at)?;
    sink.acknowledge_raw(old.file_name());
    // Force a failure before publication; the archive must remain protected.
    fs::create_dir_all(root.join("storage"))?;
    fs::write(root.join("storage/transaction.json"), b"corrupt")?;
    assert!(sink.flush().is_err());
    fs::remove_file(root.join("storage/transaction.json"))?;
    let now = "2026-11-03T12:00:00Z".parse()?;
    assert_eq!(maintain(root, now, &options())?.raw_deleted, 0);
    sink.flush()?;
    sink.prepare(recent.scraped_at)?;
    sink.acknowledge_raw(recent.file_name());
    sink.flush()?;
    let mut disabled = options();
    disabled.raw_retention_days = 0;
    assert_eq!(maintain(root, now, &disabled)?.raw_deleted, 0);
    let report = maintain(root, now, &options())?;
    assert_eq!(report.raw_deleted, 1);
    assert_eq!(report.raw_unconfirmed, 1);
    assert!(
        root.join("raw/2026-10-01")
            .join(pending.file_name())
            .exists()
    );
    assert!(
        root.join("raw/2026-11-02")
            .join(recent.file_name())
            .exists()
    );
    assert!(!root.join("raw/2026-10-01").join(old.file_name()).exists());
    assert_eq!(retention_state(root)?.deleted_files, 1);
    assert_eq!(maintain(root, now, &options())?.raw_deleted, 0);
    Ok(())
}

#[test]
fn broken_compaction_keeps_source_files_and_raw_archives() -> Result<()> {
    let temp = TempRoot::new()?;
    let root = &temp.0;
    let store = Store::new(root.clone());
    let old = raw("2026-10-01T12:00:00Z", "old")?;
    store.save_raw(&old)?;
    let mut sink = ParquetSink::new(root.join("parquet"));
    sink.prepare(old.scraped_at)?;
    sink.acknowledge_raw(old.file_name());
    sink.flush()?;
    write_part(root, "auctions", "2026-10-01", "part-0.parquet", 1)?;
    // Missing attributes partner: fail closed before publishing or pruning anything.
    assert!(maintain(root, "2026-11-03T00:00:00Z".parse()?, &options()).is_err());
    assert!(
        root.join("parquet/auctions/date=2026-10-01/part-0.parquet")
            .exists()
    );
    assert!(root.join("raw/2026-10-01").join(old.file_name()).exists());
    Ok(())
}
