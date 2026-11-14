//! Streaming compaction of closed UTC dates and retention of committed raw responses.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use clap::Args;
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};

use crate::dataset::{DatasetLock, PARQUET_ZSTD_LEVEL, Transaction, durable_write, unique_id};

#[derive(Args, Clone, Debug)]
pub struct MaintenanceArgs {
    /// Keep raw responses for this many days. 0 disables raw cleanup.
    #[arg(long, env = "WFMT_RAW_RETENTION_DAYS", default_value_t = 14)]
    pub raw_retention_days: u32,

    /// Hours between local storage maintenance passes (no API requests).
    #[arg(long, env = "WFMT_MAINTENANCE_INTERVAL_HOURS", default_value_t = 6, value_parser = clap::value_parser!(u64).range(1..=8760))]
    pub maintenance_interval_hours: u64,

    /// Approximate combined input size of a compacted part, in MiB.
    #[arg(long, env = "WFMT_COMPACT_TARGET_MB", default_value_t = 128, value_parser = clap::value_parser!(u64).range(1..=4096))]
    pub compact_target_mb: u64,
}

#[derive(Default, Debug)]
pub struct Report {
    pub compacted_inputs: usize,
    pub compacted_outputs: usize,
    pub raw_deleted: usize,
    pub raw_bytes_deleted: u64,
    pub raw_unconfirmed: usize,
}

#[derive(Default, Serialize, Deserialize)]
pub struct RetentionState {
    pub deleted_files: u64,
    pub last_cleanup_at: Option<DateTime<Utc>>,
}

/// Written after every compaction pass. Days before `compacted_before` won't change again unless
/// the data is reprocessed or replaced, so they are ready to be published.
pub const COMPACTION_STATE: &str = "storage/compaction.json";

const MIB: u64 = 1024 * 1024;
// Compacted files are read in batches and written in bounded row groups, so memory use stays
// flat however big a day gets.
const COMPACT_BATCH_ROWS: usize = 8192;
const ROW_GROUP_MAX_ROWS: usize = 64 * 1024;
const ROW_GROUP_MAX_BYTES: usize = 64 * MIB as usize;

#[derive(Serialize, Deserialize)]
struct CompactionState {
    compacted_before: NaiveDate,
}

/// Every day before this has been compacted, or `None` before the first maintenance pass.
pub fn compacted_before(root: &Path) -> Result<Option<NaiveDate>> {
    match fs::read(root.join(COMPACTION_STATE)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice::<CompactionState>(&bytes)?.compacted_before,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn retention_state(root: &Path) -> Result<RetentionState> {
    match fs::read(root.join("storage/raw-retention.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RetentionState::default()),
        Err(e) => Err(e.into()),
    }
}

/// No network access. Caller should also hold the engine lock to exclude reprocessing.
pub fn maintain(root: &Path, now: DateTime<Utc>, args: &MaintenanceArgs) -> Result<Report> {
    ensure!(
        args.compact_target_mb > 0,
        "compaction target must be positive"
    );
    let _lock = DatasetLock::write(root)?;
    let mut report = Report::default();
    let target = args
        .compact_target_mb
        .checked_mul(MIB)
        .context("compaction target is too large")?;
    let parquet = root.join("parquet");
    // Keep today's append-only parts untouched. Group by schema as well as date so older
    // schema versions remain readable without inventing values for missing columns.
    let mut days = dated_dirs(&parquet.join("auctions"), "date=")?;
    days.extend(dated_dirs(&parquet.join("gone"), "date=")?);
    days.sort();
    days.dedup();
    for date in days.into_iter().filter(|d| *d < now.date_naive()) {
        for tables in [&["auctions", "attributes"][..], &["gone"][..]] {
            let partition = PathBuf::from("parquet")
                .join(tables[0])
                .join(format!("date={date}"));
            let mut schemas: BTreeMap<String, Vec<Vec<PathBuf>>> = BTreeMap::new();
            for path in parquet_files(&root.join(&partition))? {
                let mut bundle = Vec::new();
                let mut schema_key = String::new();
                for table in tables {
                    let relative = PathBuf::from("parquet")
                        .join(table)
                        .join(format!("date={date}"))
                        .join(path.file_name().unwrap());
                    ensure!(
                        root.join(&relative).exists(),
                        "missing paired table {}",
                        relative.display()
                    );
                    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(
                        root.join(&relative),
                    )?)?;
                    schema_key.push_str(&format!("{:?};", reader.schema()));
                    bundle.push(relative);
                }
                schemas.entry(schema_key).or_default().push(bundle);
            }
            for bundles in schemas.values() {
                let mut group = Vec::new();
                let mut bytes = 0;
                for bundle in bundles {
                    let size: u64 = bundle
                        .iter()
                        .map(|p| fs::metadata(root.join(p)).map(|m| m.len()))
                        .collect::<std::io::Result<Vec<_>>>()?
                        .into_iter()
                        .sum();
                    if bytes + size > target && !group.is_empty() {
                        compact_group(root, date, tables, &group, &mut report)?;
                        group.clear();
                        bytes = 0;
                    }
                    group.push(bundle.clone());
                    bytes += size;
                }
                compact_group(root, date, tables, &group, &mut report)?;
            }
        }
    }
    durable_write(
        &root.join(COMPACTION_STATE),
        &serde_json::to_vec(&CompactionState {
            compacted_before: now.date_naive(),
        })?,
    )?;
    if args.raw_retention_days > 0 {
        let cutoff = now
            .date_naive()
            .checked_sub_signed(TimeDelta::days(i64::from(args.raw_retention_days)))
            .context("raw retention window is too large")?;
        for date in dated_dirs(&root.join("raw"), "")?
            .into_iter()
            .filter(|d| *d < cutoff)
        {
            prune_day(root, date, now, &mut report)?;
        }
    }
    tracing::info!(?report, "storage maintenance finished");
    Ok(report)
}

fn compact_group(
    root: &Path,
    date: NaiveDate,
    tables: &[&str],
    bundles: &[Vec<PathBuf>],
    report: &mut Report,
) -> Result<()> {
    if bundles.len() < 2 {
        return Ok(());
    }
    let id = unique_id();
    let mut tx = Transaction::new(root)?;
    for (i, table) in tables.iter().enumerate() {
        let destination = PathBuf::from("parquet")
            .join(table)
            .join(format!("date={date}"))
            .join(format!("compact-{id}.parquet"));
        let stage = tx.stage(destination);
        let mut writer: Option<ArrowWriter<File>> = None;
        let mut expected_rows = 0;
        for bundle in bundles {
            let builder =
                ParquetRecordBatchReaderBuilder::try_new(File::open(root.join(&bundle[i]))?)?;
            expected_rows += builder.metadata().file_metadata().num_rows();
            if writer.is_none() {
                let props = WriterProperties::builder()
                    .set_compression(Compression::ZSTD(ZstdLevel::try_new(PARQUET_ZSTD_LEVEL)?))
                    .set_max_row_group_row_count(Some(ROW_GROUP_MAX_ROWS))
                    .set_max_row_group_bytes(Some(ROW_GROUP_MAX_BYTES))
                    .build();
                writer = Some(ArrowWriter::try_new(
                    File::create(&stage)?,
                    builder.schema().clone(),
                    Some(props),
                )?);
            }
            for batch in builder.with_batch_size(COMPACT_BATCH_ROWS).build()? {
                writer.as_mut().unwrap().write(&batch?)?;
            }
            tx.delete(bundle[i].clone());
        }
        writer.unwrap().close()?;
        File::open(&stage)?.sync_all()?;
        let check = ParquetRecordBatchReaderBuilder::try_new(File::open(&stage)?)?;
        ensure!(
            check.metadata().file_metadata().num_rows() == expected_rows,
            "compaction row count mismatch"
        );
    }
    tx.commit()?;
    report.compacted_inputs += bundles.len() * tables.len();
    report.compacted_outputs += tables.len();
    Ok(())
}

/// Names of the raw archives whose rows were durably published, per the given receipts.
fn confirmed_archives(receipts: &[PathBuf]) -> Result<HashSet<String>> {
    let mut confirmed = HashSet::new();
    for path in receipts {
        let names: Vec<String> = serde_json::from_slice(&fs::read(path)?)
            .with_context(|| format!("reading raw receipt {}", path.display()))?;
        for name in names {
            ensure!(
                Path::new(&name).file_name().and_then(|n| n.to_str()) == Some(&name),
                "invalid raw receipt filename"
            );
            confirmed.insert(name);
        }
    }
    Ok(confirmed)
}

/// When a feed archive was fetched, in unix ms. `None` for anything that isn't a feed archive.
fn feed_archive_time(name: &str) -> Option<i64> {
    let (id, time) = name.strip_suffix(".json.gz")?.rsplit_once('-')?;
    wfmt_types::is_feed(id).then(|| time.parse().ok())?
}

/// Deletes committed feed archives older than `retention`, returning how many. They overlap
/// heavily, so unlike weapon archives they aren't worth keeping for days. Receipts stay until
/// their day is pruned, as they can also cover weapon archives. Deletions count towards
/// [`RetentionState`], since the rows they produced can no longer be rebuilt either.
pub fn prune_feeds(root: &Path, now: DateTime<Utc>, retention: TimeDelta) -> Result<u64> {
    let _lock = DatasetLock::write(root)?;
    let cutoff = (now - retention).timestamp_millis();
    let mut expired = Vec::new();
    for date in dated_dirs(&root.join("raw"), "")? {
        let relative = PathBuf::from("raw").join(date.to_string());
        let directory = root.join(&relative);
        let mut names = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if feed_archive_time(&name).is_some_and(|t| t < cutoff) {
                names.push(name);
            }
        }
        if names.is_empty() {
            continue;
        }
        let confirmed = confirmed_archives(&json_files(&directory.join(".committed"))?)?;
        expired.extend(
            names
                .into_iter()
                .filter(|name| confirmed.contains(name))
                .map(|name| relative.join(name)),
        );
    }
    if expired.is_empty() {
        return Ok(0);
    }
    let deleted = expired.len() as u64;
    let mut tx = Transaction::new(root)?;
    for path in expired {
        tx.delete(path);
    }
    let mut state = retention_state(root)?;
    state.deleted_files += deleted;
    state.last_cleanup_at = Some(now);
    fs::write(
        tx.stage("storage/raw-retention.json".into()),
        serde_json::to_vec(&state)?,
    )?;
    tx.commit()?;
    Ok(deleted)
}

fn prune_day(root: &Path, date: NaiveDate, now: DateTime<Utc>, report: &mut Report) -> Result<()> {
    let relative = PathBuf::from("raw").join(date.to_string());
    let directory = root.join(&relative);
    let receipts_dir = directory.join(".committed");
    let receipts = json_files(&receipts_dir)?;
    let confirmed = confirmed_archives(&receipts)?;
    let mut tx = Transaction::new(root)?;
    let mut deleted = 0;
    let mut bytes = 0;
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".json.gz") {
            continue;
        }
        if confirmed.contains(&name) {
            bytes += entry.metadata()?.len();
            tx.delete(relative.join(name));
            deleted += 1;
        } else {
            // Legacy archives and interrupted/failed fetches have no durable receipt.
            report.raw_unconfirmed += 1;
        }
    }
    if deleted > 0 {
        let mut state = retention_state(root)?;
        state.deleted_files += deleted;
        state.last_cleanup_at = Some(now);
        fs::write(
            tx.stage("storage/raw-retention.json".into()),
            serde_json::to_vec(&state)?,
        )?;
    }
    for path in receipts {
        tx.delete(path.strip_prefix(root)?.into());
    }
    tx.commit()?;
    report.raw_deleted += deleted as usize;
    report.raw_bytes_deleted += bytes;
    if receipts_dir.exists() && fs::read_dir(&receipts_dir)?.next().is_none() {
        fs::remove_dir(&receipts_dir)?;
    }
    if fs::read_dir(&directory)?.next().is_none() {
        fs::remove_dir(directory)?;
    }
    Ok(())
}

fn dated_dirs(dir: &Path, prefix: &str) -> Result<Vec<NaiveDate>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(date) = entry
                .file_name()
                .to_str()
                .and_then(|n| n.strip_prefix(prefix))
                .and_then(|n| NaiveDate::parse_from_str(n, "%Y-%m-%d").ok())
        {
            out.push(date);
        }
    }
    out.sort();
    Ok(out)
}

fn files_with_extension(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && path.extension().is_some_and(|e| e == extension) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

fn parquet_files(dir: &Path) -> Result<Vec<PathBuf>> {
    files_with_extension(dir, "parquet")
}
fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    files_with_extension(dir, "json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ParquetSink, RawResponse, Store};
    use arrow::array::{Int64Array, RecordBatch};
    use arrow::compute::concat_batches;
    use std::sync::Arc;

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

    fn write_part(
        root: &Path,
        table: &str,
        date: &str,
        name: &str,
        value: i64,
        legacy: bool,
    ) -> Result<()> {
        let mut columns = vec![(
            "value",
            Arc::new(Int64Array::from(vec![value])) as arrow::array::ArrayRef,
        )];
        if !legacy {
            columns.push(("extra", Arc::new(Int64Array::from(vec![42]))));
        }
        let batch = RecordBatch::try_from_iter(columns)?;
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

    fn read_table(root: &Path, table: &str, date: &str) -> Result<RecordBatch> {
        let mut batches = Vec::new();
        for path in parquet_files(
            &root
                .join("parquet")
                .join(table)
                .join(format!("date={date}")),
        )? {
            for batch in ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?.build()? {
                batches.push(batch?);
            }
        }
        Ok(concat_batches(&batches[0].schema(), &batches)?)
    }

    #[test]
    fn compaction_preserves_rows_pairs_and_open_dates_and_is_idempotent() -> Result<()> {
        let temp = TempRoot::new()?;
        let root = &temp.0;
        let now = "2026-11-03T12:00:00Z".parse()?;
        assert_eq!(compacted_before(root)?, None);
        for date in ["2026-11-01", "2026-11-03", "2026-11-04"] {
            for table in ["auctions", "attributes", "gone"] {
                for index in 0..3 {
                    write_part(
                        root,
                        table,
                        date,
                        &format!("part-{index}.parquet"),
                        index,
                        false,
                    )?;
                }
            }
        }
        let before: Vec<_> = ["auctions", "attributes", "gone"]
            .iter()
            .map(|table| read_table(root, table, "2026-11-01"))
            .collect::<Result<_>>()?;
        let report = maintain(root, now, &options())?;
        assert_eq!(report.compacted_inputs, 9);
        assert_eq!(report.compacted_outputs, 3);
        for (i, table) in ["auctions", "attributes", "gone"].iter().enumerate() {
            assert_eq!(before[i], read_table(root, table, "2026-11-01")?);
            for date in ["2026-11-03", "2026-11-04"] {
                assert_eq!(
                    parquet_files(
                        &root
                            .join("parquet")
                            .join(table)
                            .join(format!("date={date}"))
                    )?
                    .len(),
                    3
                );
            }
        }
        let auctions = parquet_files(&root.join("parquet/auctions/date=2026-11-01"))?;
        let attributes = parquet_files(&root.join("parquet/attributes/date=2026-11-01"))?;
        assert_eq!(auctions[0].file_name(), attributes[0].file_name());
        assert_eq!(compacted_before(root)?, Some("2026-11-03".parse()?));
        assert_eq!(maintain(root, now, &options())?.compacted_inputs, 0);
        Ok(())
    }

    #[test]
    fn compaction_keeps_schema_generations_separate() -> Result<()> {
        let temp = TempRoot::new()?;
        for table in ["auctions", "attributes"] {
            for (index, legacy) in [true, true, false, false].iter().enumerate() {
                write_part(
                    &temp.0,
                    table,
                    "2026-11-01",
                    &format!("part-{index}.parquet"),
                    index as i64,
                    *legacy,
                )?;
            }
        }
        let report = maintain(&temp.0, "2026-11-03T00:00:00Z".parse()?, &options())?;
        assert_eq!(report.compacted_inputs, 8);
        assert_eq!(report.compacted_outputs, 4);
        let files = parquet_files(&temp.0.join("parquet/auctions/date=2026-11-01"))?;
        assert_eq!(files.len(), 2);
        for file in files {
            let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(file)?)?;
            assert_eq!(reader.metadata().file_metadata().num_rows(), 2);
        }
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
        let expired = raw("2026-11-03T05:59:59Z", wfmt_types::FEED_RECENT)?;
        let yesterday = raw("2026-11-02T23:00:00Z", wfmt_types::FEED_POPULAR)?;
        let fresh = raw("2026-11-03T06:00:01Z", wfmt_types::FEED_RECENT)?;
        let weapon = raw("2026-11-03T01:00:00Z", "kulstar")?;
        let uncommitted = raw("2026-11-03T02:00:00Z", wfmt_types::FEED_POPULAR)?;
        for raw in [&yesterday, &expired, &fresh, &weapon] {
            store.save_raw(raw)?;
            sink.prepare(raw.scraped_at)?;
            sink.acknowledge_raw(raw);
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
        sink.acknowledge_raw(&old);
        // Force a failure before publication; the archive must remain protected.
        fs::create_dir_all(root.join("storage"))?;
        fs::write(root.join("storage/transaction.json"), b"corrupt")?;
        assert!(sink.flush().is_err());
        fs::remove_file(root.join("storage/transaction.json"))?;
        let now = "2026-11-03T12:00:00Z".parse()?;
        assert_eq!(maintain(root, now, &options())?.raw_deleted, 0);
        sink.flush()?;
        sink.prepare(recent.scraped_at)?;
        sink.acknowledge_raw(&recent);
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
        sink.acknowledge_raw(&old);
        sink.flush()?;
        write_part(root, "auctions", "2026-10-01", "part-0.parquet", 1, false)?;
        // Missing attributes partner: fail closed before publishing or pruning anything.
        assert!(maintain(root, "2026-11-03T00:00:00Z".parse()?, &options()).is_err());
        assert!(
            root.join("parquet/auctions/date=2026-10-01/part-0.parquet")
                .exists()
        );
        assert!(root.join("raw/2026-10-01").join(old.file_name()).exists());
        Ok(())
    }
}
