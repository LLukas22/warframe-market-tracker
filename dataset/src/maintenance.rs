//! Storage maintenance: compaction of closed UTC dates (see `compact`) and retention of
//! committed raw responses.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::compact;
use crate::layout::{DATE_PREFIX, PARQUET_DIR, RAW_DIR, RECEIPTS_DIR, Table};
use crate::tx::{DatasetLock, Transaction, durable_write};

#[derive(Clone, Debug)]
#[cfg_attr(feature = "clap", derive(clap::Args))]
pub struct MaintenanceArgs {
    /// Keep raw responses for this many days. 0 disables raw cleanup.
    #[cfg_attr(
        feature = "clap",
        arg(long, env = "WFMT_RAW_RETENTION_DAYS", default_value_t = 14)
    )]
    pub raw_retention_days: u32,

    /// Hours between local storage maintenance passes (no API requests).
    #[cfg_attr(
        feature = "clap",
        arg(
            long,
            env = "WFMT_MAINTENANCE_INTERVAL_HOURS",
            default_value_t = 6,
            value_parser = clap::value_parser!(u64).range(1..=8760)
        )
    )]
    pub maintenance_interval_hours: u64,

    /// Memory a compaction may use to sort one table of a day, in MiB. A larger day is sorted
    /// in ranges of weapons; a weapon too large on its own leaves its day uncompacted.
    #[cfg_attr(
        feature = "clap",
        arg(
            long,
            env = "WFMT_COMPACT_MEMORY_MB",
            default_value_t = DEFAULT_COMPACT_MEMORY_MB,
            value_parser = clap::value_parser!(u64).range(compact::MIN_MEMORY_MB..=1 << 20)
        )
    )]
    pub compact_memory_mb: u64,
}

pub const DEFAULT_COMPACT_MEMORY_MB: u64 = 1024;

#[derive(Default, Debug)]
pub struct Report {
    pub compacted_inputs: usize,
    pub compacted_outputs: usize,
    /// Closed days that could not be compacted. Their files and raw archives are kept.
    pub failed_days: Vec<NaiveDate>,
    pub raw_deleted: usize,
    pub raw_bytes_deleted: u64,
    pub raw_unconfirmed: usize,
}

#[derive(Default, Serialize, Deserialize)]
pub struct RetentionState {
    pub deleted_files: u64,
    pub last_cleanup_at: Option<DateTime<Utc>>,
}

/// Written after every compaction pass. Days before `compacted_before` are closed: no more scrapes
/// write to them. Whether one is ready to publish is up to [`compact::day_complete`].
pub const COMPACTION_STATE: &str = "storage/compaction.json";

const MIB: u64 = 1024 * 1024;

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
///
/// Compacts every closed day that isn't in its final form yet (see `compact`). A day that fails
/// is logged and skipped, keeping its files and its raw archives, and the pass goes on with the
/// others; it then ends with an error naming the failed days.
pub fn maintain(root: &Path, now: DateTime<Utc>, args: &MaintenanceArgs) -> Result<Report> {
    ensure!(
        args.compact_memory_mb >= compact::MIN_MEMORY_MB,
        "the compaction memory budget must be at least {} MiB",
        compact::MIN_MEMORY_MB
    );
    let memory = usize::try_from(
        args.compact_memory_mb
            .checked_mul(MIB)
            .context("compaction memory budget is too large")?,
    )?;
    let _lock = DatasetLock::write(root)?;
    let mut report = Report::default();
    let parquet = root.join(PARQUET_DIR);
    // Today's append-only parts stay untouched until the day is over.
    let mut days = Vec::new();
    for table in Table::ALL {
        days.extend(dated_dirs(&parquet.join(table.as_str()), DATE_PREFIX)?);
    }
    days.sort();
    days.dedup();
    for date in days.into_iter().filter(|d| *d < now.date_naive()) {
        for tables in Table::GROUPS {
            match compact::compact_group(root, date, tables, memory) {
                Ok(Some(compacted)) => {
                    report.compacted_inputs += compacted.inputs;
                    report.compacted_outputs += compacted.outputs;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(%date, error = %format!("{e:#}"), "compaction failed");
                    if report.failed_days.last() != Some(&date) {
                        report.failed_days.push(date);
                    }
                }
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
        let failed = report.failed_days.clone();
        for date in dated_dirs(&root.join(RAW_DIR), "")?
            .into_iter()
            .filter(|d| *d < cutoff && !failed.contains(d))
        {
            prune_day(root, date, now, &mut report)?;
        }
    }
    tracing::info!(?report, "storage maintenance finished");
    if !report.failed_days.is_empty() {
        let days: Vec<String> = report.failed_days.iter().map(ToString::to_string).collect();
        bail!(
            "could not compact {}; their files and raw archives were kept",
            days.join(", ")
        );
    }
    Ok(report)
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
    wfmt_types::Feed::from_id(id).and_then(|_| time.parse().ok())
}

/// Deletes committed feed archives older than `retention`, returning how many. They overlap
/// heavily, so unlike weapon archives they aren't worth keeping for days. Receipts stay until
/// their day is pruned, as they can also cover weapon archives. Deletions count towards
/// [`RetentionState`], since the rows they produced can no longer be rebuilt either.
pub fn prune_feeds(root: &Path, now: DateTime<Utc>, retention: TimeDelta) -> Result<u64> {
    let _lock = DatasetLock::write(root)?;
    let cutoff = (now - retention).timestamp_millis();
    let mut expired = Vec::new();
    for date in dated_dirs(&root.join(RAW_DIR), "")? {
        let relative = PathBuf::from(RAW_DIR).join(date.to_string());
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
        let confirmed = confirmed_archives(&json_files(&directory.join(RECEIPTS_DIR))?)?;
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
    let relative = PathBuf::from(RAW_DIR).join(date.to_string());
    let directory = root.join(&relative);
    let receipts_dir = directory.join(RECEIPTS_DIR);
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

fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    files_with_extension(dir, "json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compact::tests::{auction, gone_row, read_all, rows_text, write_parts};
    use crate::compact::{day_complete, table_files};

    fn options() -> MaintenanceArgs {
        MaintenanceArgs {
            raw_retention_days: 14,
            maintenance_interval_hours: 6,
            compact_memory_mb: DEFAULT_COMPACT_MEMORY_MB,
        }
    }

    fn rows(root: &Path, table: Table, date: NaiveDate) -> Vec<String> {
        let mut rows = rows_text(&read_all(table, &table_files(root, table, date).unwrap()));
        rows.sort();
        rows
    }

    #[test]
    fn closed_days_are_compacted_once_and_open_days_left_alone() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        let now = "2026-11-03T12:00:00Z".parse()?;
        assert_eq!(compacted_before(root)?, None);
        for date in ["2026-11-01", "2026-11-03"] {
            write_parts(
                root,
                (0..3)
                    .map(|i| {
                        let at = format!("{date}T0{i}:00:00Z");
                        (
                            vec![auction("soma", &format!("s{i}"), &at, Some(i))],
                            vec![gone_row("soma", &format!("g{i}"), &at, None)],
                        )
                    })
                    .collect(),
            );
        }
        let closed: NaiveDate = "2026-11-01".parse()?;
        let open: NaiveDate = "2026-11-03".parse()?;
        let before: Vec<_> = Table::ALL.iter().map(|&t| rows(root, t, closed)).collect();

        let report = maintain(root, now, &options())?;
        assert_eq!(report.compacted_inputs, 9);
        assert_eq!(report.compacted_outputs, 3);
        assert!(day_complete(root, closed)?);
        for (&table, before) in Table::ALL.iter().zip(before) {
            assert_eq!(rows(root, table, closed), before);
            assert_eq!(
                table_files(root, table, open)?.len(),
                3,
                "today stays as written"
            );
        }
        assert_eq!(compacted_before(root)?, Some(open));
        assert_eq!(maintain(root, now, &options())?.compacted_inputs, 0);
        Ok(())
    }

    #[test]
    fn a_failed_day_keeps_its_files_and_the_others_are_compacted() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();
        for date in ["2026-11-01", "2026-11-02"] {
            write_parts(
                root,
                (0..2)
                    .map(|i| {
                        let at = format!("{date}T0{i}:00:00Z");
                        (
                            vec![auction("soma", &format!("s{i}"), &at, Some(1))],
                            Vec::new(),
                        )
                    })
                    .collect(),
            );
        }
        // An auctions part without its attributes partner.
        let broken = table_files(root, Table::Attributes, "2026-11-01".parse()?)?;
        fs::remove_file(&broken[0])?;

        let error = maintain(root, "2026-11-03T00:00:00Z".parse()?, &options()).unwrap_err();
        assert!(error.to_string().contains("2026-11-01"), "{error:#}");
        assert_eq!(
            table_files(root, Table::Auctions, "2026-11-01".parse()?)?.len(),
            2
        );
        assert!(day_complete(root, "2026-11-02".parse()?)?);
        Ok(())
    }
}
