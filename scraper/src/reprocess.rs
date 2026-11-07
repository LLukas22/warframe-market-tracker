//! Offline rebuild of a dataset from its raw archive, for when the normalization rules change.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};

use crate::dataset::DatasetLock;
use crate::engine;
use crate::identity::Registry;
use crate::maintenance;
use crate::normalize::Normalizer;
use crate::pipeline::Pipeline;
use crate::store::{ParquetSink, RawResponse, Store};
use crate::tracking::Tracker;
use crate::wfm::{self, Weapon};

/// Rebuilds parquet files, riven ids and gone detection from the archived raw responses, without
/// touching the API. The result goes to `out` (default `<data_dir>/reprocessed`). With `replace`
/// it is swapped into `data_dir` and the old files move to `backup-*`; the server must be stopped.
pub fn reprocess(
    data_dir: &Path,
    rework_cutoff: DateTime<Utc>,
    out: Option<PathBuf>,
    replace: bool,
) -> Result<()> {
    let store = Store::new(data_dir.to_owned());
    let _engine_lock =
        if replace {
            Some(engine::try_lock(store.root())?.context(
                "the server is running on this data directory, stop it before --replace",
            )?)
        } else {
            None
        };
    let source_lock = DatasetLock::read(store.root())?;
    if replace && maintenance::retention_state(store.root())?.deleted_files > 0 {
        bail!(
            "raw retention has removed part of the archive; --replace would discard historical Parquet data. Use --out to rebuild only retained raw responses, or restore a complete raw backup into a separate data directory"
        );
    }
    let body = store
        .load_weapons_raw()?
        .context("no cached weapon list; start the server first")?;
    let weapons: HashMap<String, Weapon> = wfm::parse_weapons(&body)?
        .into_iter()
        .map(|w| (w.slug.clone(), w))
        .collect();

    let out = out.unwrap_or_else(|| store.root().join("reprocessed"));
    if out.exists() && std::fs::read_dir(&out)?.next().is_some() {
        bail!("{} is not empty, remove it first", out.display());
    }
    let result = Store::new(out.clone());
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new(rework_cutoff),
        registry: Registry::open(result.registry_path())?,
        tracker: Tracker::open(result.seen_dir()),
    };
    let mut sink = ParquetSink::new(result.root().join("parquet"));

    // Chronological order matters for ids and gone detection.
    let mut files = store.raw_files()?;
    if replace && files.is_empty() {
        bail!(
            "no archived raw responses; refusing to replace historical data with an empty rebuild"
        );
    }
    let stamp = |p: &PathBuf| -> i64 {
        p.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.trim_end_matches(".json.gz").rsplit('-').next())
            .and_then(|ms| ms.parse().ok())
            .unwrap_or_default()
    };
    files.sort_by_key(|p| (stamp(p), p.clone()));
    tracing::info!(files = files.len(), out = %out.display(), "reprocessing raw responses");

    let mut gone = 0;
    for (i, path) in files.iter().enumerate() {
        let raw = RawResponse::read(path)?;
        let Some(weapon) = weapons.get(&raw.weapon) else {
            if replace {
                bail!(
                    "weapon {} missing from cached list; refusing an incomplete replacement",
                    raw.weapon
                );
            }
            tracing::warn!(weapon = %raw.weapon, "weapon missing from cached list, skipping");
            continue;
        };
        let summary = pipeline
            .process(weapon, &raw, &mut sink)
            .with_context(|| format!("processing {}", path.display()))?;
        sink.acknowledge_raw(&raw);
        gone += summary.gone;
        if (i + 1) % 250 == 0 {
            sink.flush()?;
        }
    }
    sink.flush()?;
    pipeline.registry.flush()?;
    tracing::info!(rivens = pipeline.registry.len(), gone, "reprocess finished");

    if replace {
        drop(source_lock);
        let _dataset_lock = DatasetLock::write(store.root())?;
        let backup = store
            .root()
            .join(format!("backup-{}", Utc::now().format("%Y%m%dT%H%M%S")));
        std::fs::create_dir_all(&backup)?;
        for name in ["parquet", "rivens.jsonl", "seen"] {
            let current = store.root().join(name);
            if current.exists() {
                std::fs::rename(&current, backup.join(name))?;
            }
            let fresh = result.root().join(name);
            if fresh.exists() {
                std::fs::rename(&fresh, &current)?;
            }
        }
        // The successful rebuild also confirms archives written before receipts existed.
        let receipts = result.root().join("raw");
        if receipts.exists() {
            for day in std::fs::read_dir(&receipts)? {
                let day = day?;
                let destination = store
                    .root()
                    .join("raw")
                    .join(day.file_name())
                    .join(".committed");
                std::fs::create_dir_all(&destination)?;
                for receipt in std::fs::read_dir(day.path().join(".committed"))? {
                    let receipt = receipt?;
                    std::fs::rename(receipt.path(), destination.join(receipt.file_name()))?;
                }
            }
            std::fs::remove_dir_all(receipts)?;
        }
        std::fs::remove_dir_all(result.root().join("storage"))?;
        std::fs::remove_dir(result.root())?;
        tracing::info!(backup = %backup.display(), "replaced parquet, riven registry and seen state");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maintenance::MaintenanceArgs;
    use crate::store::RawPage;
    use std::fs;

    struct TestDir {
        data_dir: PathBuf,
        rework_cutoff: DateTime<Utc>,
    }

    fn test_cli() -> TestDir {
        TestDir {
            data_dir: std::env::temp_dir()
                .join(format!("wfmt-reprocess-{}", crate::dataset::unique_id())),
            rework_cutoff: "2026-10-22T00:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn replacement_refuses_partial_or_empty_archive() -> Result<()> {
        let cli = test_cli();
        let store = Store::new(cli.data_dir.clone());
        store.save_weapons_raw(r#"{"data":[]}"#)?;
        fs::create_dir_all(cli.data_dir.join("parquet"))?;
        let history = cli.data_dir.join("parquet/history");
        fs::write(&history, b"historical data")?;
        assert!(
            reprocess(&cli.data_dir, cli.rework_cutoff, None, true)
                .unwrap_err()
                .to_string()
                .contains("empty rebuild")
        );
        fs::write(
            cli.data_dir.join("storage/raw-retention.json"),
            br#"{"deleted_files":1,"last_cleanup_at":null}"#,
        )?;
        assert!(
            reprocess(&cli.data_dir, cli.rework_cutoff, None, true)
                .unwrap_err()
                .to_string()
                .contains("discard historical")
        );
        assert_eq!(fs::read(history)?, b"historical data");
        fs::remove_dir_all(cli.data_dir)?;
        Ok(())
    }

    #[test]
    fn replacement_confirms_legacy_archives_for_retention() -> Result<()> {
        let cli = test_cli();
        let store = Store::new(cli.data_dir.clone());
        store.save_weapons_raw(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?;
        let raw = RawResponse::new(
            "2026-10-01T00:00:00Z".parse()?,
            "kulstar".into(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: "price_asc".into(),
                body: r#"{"payload":{"auctions":[]}}"#.into(),
            }],
            Vec::new(),
        );
        store.save_raw(&raw)?;
        reprocess(&cli.data_dir, cli.rework_cutoff, None, true)?;
        assert!(!cli.data_dir.join("reprocessed").exists());
        assert!(
            fs::read_dir(cli.data_dir.join("raw/2026-10-01/.committed"))?
                .next()
                .is_some()
        );
        let report = maintenance::maintain(
            &cli.data_dir,
            "2026-11-03T00:00:00Z".parse()?,
            &MaintenanceArgs {
                raw_retention_days: 14,
                maintenance_interval_hours: 6,
                compact_target_mb: 128,
            },
        )?;
        assert_eq!(report.raw_deleted, 1);
        fs::remove_dir_all(cli.data_dir)?;
        Ok(())
    }
}
