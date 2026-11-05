use anyhow::Result;
use chrono::{TimeDelta, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use wfmt_scraper::{
    dataset::unique_id,
    engine::{Engine, EngineArgs},
    identity::Registry,
    maintenance::MaintenanceArgs,
    normalize::Normalizer,
    pipeline::Pipeline,
    store::{ParquetSink, RawPage, RawResponse, State, Store},
    tracking::Tracker,
    wfm,
};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!("wfmt-recovery-{}", unique_id())))
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn args() -> EngineArgs {
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
        no_raw: false,
        weapons: vec![],
    }
}

fn fixture() -> serde_json::Value {
    let mut body = String::new();
    flate2::read::GzDecoder::new(&include_bytes!("fixtures/kulstar_auctions.json.gz")[..])
        .read_to_string(&mut body)
        .unwrap();
    serde_json::from_str::<serde_json::Value>(&body).unwrap()["payload"]["auctions"][0].clone()
}

fn rows(root: &Path, table: &str) -> usize {
    let dir = root.join("parquet").join(table);
    if !dir.exists() {
        return 0;
    }
    fs::read_dir(dir)
        .unwrap()
        .flat_map(|day| fs::read_dir(day.unwrap().path()).unwrap())
        .map(|p| {
            ParquetRecordBatchReaderBuilder::try_new(fs::File::open(p.unwrap().path()).unwrap())
                .unwrap()
                .metadata()
                .file_metadata()
                .num_rows() as usize
        })
        .sum()
}

#[test]
fn restart_replays_uncommitted_gone_with_tracking_and_does_not_duplicate_history() -> Result<()> {
    let root = Temp::new();
    let store = Store::new(root.0.clone());
    let metadata = r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#;
    store.save_weapons_raw(metadata)?;
    let weapon = wfm::parse_weapons(metadata)?.remove(0);
    let cutoff = "2026-10-22T00:00:00Z".parse()?;
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new(cutoff),
        registry: Registry::open(store.registry_path())?,
        tracker: Tracker::open(store.seen_dir()),
    };
    let mut sink = ParquetSink::new(root.0.join("parquet"));
    let t = Utc::now();
    let auction = fixture();
    let id = auction["id"].as_str().unwrap();
    let raw = |at, auctions| {
        RawResponse::new(
            at,
            "kulstar".into(),
            "pc".into(),
            vec![RawPage {
                sort_by: "price_asc".into(),
                body: serde_json::json!({"payload":{"auctions":auctions}}).to_string(),
            }],
            vec![],
        )
    };
    let first = raw(t, vec![auction.clone()]);
    store.save_raw(&first)?;
    pipeline.process(&weapon, &first, &mut sink)?;
    sink.acknowledge_raw(&first);
    assert!(!store.seen_dir().join("kulstar.json").exists());
    sink.flush()?;
    let seen_before = fs::read(store.seen_dir().join("kulstar.json"))?;
    assert_eq!(rows(&root.0, "auctions"), 1);
    let second = raw(t + TimeDelta::hours(4), Vec::<serde_json::Value>::new());
    store.save_raw(&second)?;
    assert_eq!(pipeline.process(&weapon, &second, &mut sink)?.gone, 1);
    sink.acknowledge_raw(&second);
    // Simulate the scheduler having persisted progress followed by a process crash.
    let mut state = State::default();
    state.last_fetch.insert("kulstar".into(), second.scraped_at);
    store.save_state(&state)?;
    assert_eq!(
        fs::read(store.seen_dir().join("kulstar.json"))?,
        seen_before
    );
    assert_eq!(rows(&root.0, "gone"), 0);
    drop((pipeline, sink));
    assert_eq!(store.pending_raw()?.len(), 1);
    drop(Engine::new(root.0.clone(), cutoff, args())?);
    assert!(store.pending_raw()?.is_empty());
    assert_eq!(rows(&root.0, "gone"), 1);
    let seen: serde_json::Value =
        serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
    assert!(seen.get(id).is_none());
    drop(Engine::new(root.0.clone(), cutoff, args())?);
    assert_eq!(rows(&root.0, "gone"), 1);
    assert_eq!(rows(&root.0, "auctions"), 1);
    Ok(())
}

#[test]
fn malformed_present_id_is_preserved_and_unknown_ids_do_not_infer_absence() -> Result<()> {
    let root = Temp::new();
    let store = Store::new(root.0.clone());
    let weapon =
        wfm::parse_weapons(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?.remove(0);
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new("2026-10-22T00:00:00Z".parse()?),
        registry: Registry::open(store.registry_path())?,
        tracker: Tracker::open(store.seen_dir()),
    };
    let mut sink = ParquetSink::new(root.0.join("parquet"));
    let t = Utc::now();
    let a = fixture();
    let mut b = a.clone();
    b["id"] = "second".into();
    let raw = |at, auctions| {
        RawResponse::new(
            at,
            "kulstar".into(),
            "pc".into(),
            vec![RawPage {
                sort_by: "price_asc".into(),
                body: serde_json::json!({"payload":{"auctions":auctions}}).to_string(),
            }],
            vec![],
        )
    };
    pipeline.process(&weapon, &raw(t, vec![a.clone(), b]), &mut sink)?;
    sink.flush()?;
    let bad = raw(
        t + TimeDelta::days(5),
        vec![
            serde_json::json!({"id": a["id"]}),
            serde_json::json!({"id": null}),
        ],
    );
    let summary = pipeline.process(&weapon, &bad, &mut sink)?;
    assert_eq!(summary.skipped, 2);
    assert!(!summary.complete);
    assert_eq!(summary.gone, 0);
    sink.flush()?;
    let seen: serde_json::Value =
        serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
    assert!(seen.get(a["id"].as_str().unwrap()).is_some());
    assert!(seen.get("second").is_some());
    Ok(())
}

#[test]
fn legacy_archives_without_receipts_are_not_automatically_replayed() -> Result<()> {
    let root = Temp::new();
    let store = Store::new(root.0.clone());
    let mut raw = RawResponse::new(Utc::now(), "kulstar".into(), "pc".into(), vec![], vec![]);
    let mut legacy = serde_json::to_value(&raw)?;
    legacy.as_object_mut().unwrap().remove("recoverable");
    legacy.as_object_mut().unwrap().remove("complete");
    raw = serde_json::from_value(legacy)?;
    assert!(!raw.recoverable && raw.complete);
    store.save_raw(&raw)?;
    assert!(store.pending_raw()?.is_empty());
    Ok(())
}
