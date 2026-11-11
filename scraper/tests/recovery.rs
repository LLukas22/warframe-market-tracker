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
    maintenance::{self, MaintenanceArgs},
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
        recent_interval_secs: 180,
        popular_interval_secs: 600,
        feed_raw_retention_hours: 6,
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
    // Both price orders returning the same short page: the search returned everything.
    let raw = |at, auctions| {
        let body = serde_json::json!({"payload":{"auctions":auctions}}).to_string();
        RawResponse::new(
            at,
            "kulstar".into(),
            "pc".into(),
            ["price_asc", "price_desc"]
                .map(|sort_by| RawPage {
                    buyout_policy: None,
                    sort_by: sort_by.into(),
                    body: body.clone(),
                })
                .into(),
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
fn feeds_add_listings_without_inferring_absence_and_replay_after_a_crash() -> Result<()> {
    let root = Temp::new();
    let store = Store::new(root.0.clone());
    let metadata = r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#;
    store.save_weapons_raw(metadata)?;
    let weapons = wfm::parse_weapons(metadata)?;
    let cutoff = "2026-10-22T00:00:00Z".parse()?;
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new(cutoff),
        registry: Registry::open(store.registry_path())?,
        tracker: Tracker::open(store.seen_dir()),
    };
    let mut sink = ParquetSink::new(root.0.join("parquet"));
    let t = Utc::now();
    let raw = |at, id: &str, auctions: Vec<serde_json::Value>| {
        RawResponse::new(
            at,
            id.into(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: id.into(),
                body: serde_json::json!({"payload":{"auctions":auctions}}).to_string(),
            }],
            vec![],
        )
    };
    let listing = |id: &str, buyout: serde_json::Value| {
        let mut auction = fixture();
        auction["id"] = id.into();
        auction["buyout_price"] = buyout;
        auction["top_bid"] = serde_json::Value::Null;
        auction["item"]["weapon_url_name"] = "kulstar".into();
        auction
    };

    let sweep = raw(t, "kulstar", vec![listing("swept", 500.into())]);
    pipeline.process(&weapons[0], &sweep, &mut sink)?;
    sink.flush()?;
    assert_eq!(rows(&root.0, "auctions"), 1);

    // A short feed page leaves out the swept listing, which must not count as gone.
    let mut other_weapon = listing("other", 700.into());
    other_weapon["item"]["weapon_url_name"] = "not_tracked".into();
    let lich =
        serde_json::json!({"id": "lich", "item": {"type": "lich", "weapon_url_name": "kulstar"}});
    let feed = raw(
        t + TimeDelta::minutes(3),
        wfmt_types::FEED_RECENT,
        vec![
            listing("new", 600.into()),
            listing("starting_only", serde_json::Value::Null),
            other_weapon,
            lich,
        ],
    );
    let summary = pipeline.process_feed(&weapons, &feed, &mut sink)?;
    assert_eq!(summary.auctions, 1);
    assert_eq!(summary.gone, 0);
    sink.flush()?;
    assert_eq!(rows(&root.0, "auctions"), 2);

    // The next poll overlaps: only the repriced listing adds a row.
    let unchanged = raw(
        t + TimeDelta::minutes(6),
        wfmt_types::FEED_RECENT,
        vec![listing("new", 600.into())],
    );
    assert_eq!(
        pipeline
            .process_feed(&weapons, &unchanged, &mut sink)?
            .auctions,
        0
    );
    let repriced = raw(
        t + TimeDelta::minutes(9),
        wfmt_types::FEED_POPULAR,
        vec![listing("new", 550.into())],
    );
    assert_eq!(
        pipeline
            .process_feed(&weapons, &repriced, &mut sink)?
            .auctions,
        1
    );
    sink.flush()?;
    assert_eq!(rows(&root.0, "auctions"), 3);
    assert_eq!(rows(&root.0, "gone"), 0);
    let seen: serde_json::Value =
        serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
    assert!(seen.get("swept").is_some() && seen.get("new").is_some());
    assert!(seen.get("starting_only").is_none());
    assert!(!store.seen_dir().join("not_tracked.json").exists());
    drop((pipeline, sink));

    // A poll archived right before a crash is replayed on the next start.
    let crashed = raw(
        t + TimeDelta::minutes(12),
        wfmt_types::FEED_RECENT,
        vec![listing("recovered", 800.into())],
    );
    store.save_raw(&crashed)?;
    drop(Engine::new(root.0.clone(), cutoff, args())?);
    assert!(store.pending_raw()?.is_empty());
    assert_eq!(rows(&root.0, "auctions"), 4);
    let seen: serde_json::Value =
        serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
    assert!(seen.get("recovered").is_some() && seen.get("swept").is_some());
    Ok(())
}

/// Every row of a table as JSON objects, ordered by time and auction.
fn table(root: &Path, name: &str) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for day in fs::read_dir(root.join("parquet").join(name)).unwrap() {
        for part in fs::read_dir(day.unwrap().path()).unwrap() {
            let file = fs::File::open(part.unwrap().path()).unwrap();
            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .unwrap()
                .build()
                .unwrap();
            for batch in reader {
                let batch = batch.unwrap();
                let schema = batch.schema();
                let options = arrow::util::display::FormatOptions::default();
                // Timestamps as plain milliseconds: exact, and the formatter can't handle "UTC".
                let arrays: Vec<_> = batch
                    .columns()
                    .iter()
                    .map(|c| match c.data_type() {
                        arrow::datatypes::DataType::Timestamp(..) => {
                            arrow::compute::cast(c, &arrow::datatypes::DataType::Int64).unwrap()
                        }
                        _ => c.clone(),
                    })
                    .collect();
                let columns: Vec<_> = arrays
                    .iter()
                    .map(|c| arrow::util::display::ArrayFormatter::try_new(c, &options).unwrap())
                    .collect();
                for row in 0..batch.num_rows() {
                    let mut values = serde_json::Map::new();
                    for (i, column) in batch.columns().iter().enumerate() {
                        let value = if column.is_null(row) {
                            serde_json::Value::Null
                        } else {
                            columns[i].value(row).to_string().into()
                        };
                        values.insert(schema.field(i).name().clone(), value);
                    }
                    out.push(serde_json::Value::Object(values));
                }
            }
        }
    }
    out.sort_by_key(|r| {
        (
            r["scraped_at"].to_string(),
            r["auction_id"].to_string(),
            r["position"].to_string(),
        )
    });
    out
}

fn part_count(root: &Path, name: &str) -> usize {
    fs::read_dir(root.join("parquet").join(name))
        .unwrap()
        .flat_map(|day| fs::read_dir(day.unwrap().path()).unwrap())
        .count()
}

#[test]
fn feed_rows_keep_every_change_and_outlive_their_raw_archives() -> Result<()> {
    let root = Temp::new();
    let store = Store::new(root.0.clone());
    let metadata = r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#;
    store.save_weapons_raw(metadata)?;
    let weapons = wfm::parse_weapons(metadata)?;
    let cutoff = "2026-10-22T00:00:00Z".parse()?;
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new(cutoff),
        registry: Registry::open(store.registry_path())?,
        tracker: Tracker::open(store.seen_dir()),
    };
    let mut sink = ParquetSink::new(root.0.join("parquet"));
    let listing = |buyout: i64, bid: i64, updated: &str| {
        let mut auction = fixture();
        auction["id"] = "watched".into();
        auction["buyout_price"] = buyout.into();
        auction["top_bid"] = bid.into();
        auction["updated"] = updated.into();
        auction["item"]["weapon_url_name"] = "kulstar".into();
        auction
    };
    let start: chrono::DateTime<Utc> = "2026-11-01T10:00:00Z".parse()?;
    let first_update = "2026-11-01T09:00:00+00:00";
    // Minutes after the start, the listing as polled, whether it deserves a row, and why.
    let polls = [
        (0, listing(600, 100, first_update), true),  // new
        (3, listing(600, 100, first_update), false), // overlap
        (6, listing(600, 200, first_update), true),  // bid under an unchanged buyout
        (9, listing(600, 200, first_update), false),
        (12, listing(650, 200, first_update), true), // buyout
        (15, listing(650, 200, "2026-11-01T10:14:00+00:00"), true), // any other edit
        (74, listing(650, 200, "2026-11-01T10:14:00+00:00"), false), // within the hour
        (75, listing(650, 200, "2026-11-01T10:14:00+00:00"), true), // hourly sighting
    ];
    let mut raws = Vec::new();
    let mut expected = 0;
    for (minutes, auction, wanted) in &polls {
        let raw = RawResponse::new(
            start + TimeDelta::minutes(*minutes),
            wfmt_types::FEED_POPULAR.into(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: wfmt_types::FEED_POPULAR.into(),
                body: serde_json::json!({"payload":{"auctions":[auction]}}).to_string(),
            }],
            vec![],
        );
        store.save_raw(&raw)?;
        let written = pipeline.process_feed(&weapons, &raw, &mut sink)?.auctions;
        assert_eq!(written == 1, *wanted, "poll at +{minutes} min");
        sink.acknowledge_raw(&raw);
        sink.flush()?;
        expected += usize::from(*wanted);
        raws.push(raw);
    }

    // A sweep part must merge with the feed parts, which requires an identical schema.
    let mut swept = listing(650, 200, "2026-11-01T10:14:00+00:00");
    swept["id"] = "swept".into();
    let sweep = RawResponse::new(
        start + TimeDelta::hours(2),
        "kulstar".into(),
        "pc".into(),
        vec![RawPage {
            buyout_policy: None,
            sort_by: "price_asc".into(),
            body: serde_json::json!({"payload":{"auctions":[
                swept,
                listing(650, 200, "2026-11-01T10:14:00+00:00")
            ]}})
            .to_string(),
        }],
        vec![],
    );
    store.save_raw(&sweep)?;
    pipeline.process(&weapons[0], &sweep, &mut sink)?;
    sink.acknowledge_raw(&sweep);
    sink.flush()?;
    expected += 2;

    // A later poll recording nothing still files its receipt under its own date.
    let next_day = RawResponse::new(
        start + TimeDelta::days(1),
        wfmt_types::FEED_RECENT.into(),
        "pc".into(),
        vec![RawPage {
            buyout_policy: None,
            sort_by: wfmt_types::FEED_RECENT.into(),
            body: r#"{"payload":{"auctions":[]}}"#.into(),
        }],
        vec![],
    );
    store.save_raw(&next_day)?;
    pipeline.process_feed(&weapons, &next_day, &mut sink)?;
    sink.acknowledge_raw(&next_day);
    sink.flush()?;

    assert_eq!(rows(&root.0, "gone"), 0);
    let auctions = table(&root.0, "auctions");
    let attributes = table(&root.0, "attributes");
    assert_eq!(auctions.len(), expected);
    let source = auctions.iter().filter(|r| r["source"] == "feed_popular");
    let prices: Vec<(i64, i64)> = source
        .map(|r| {
            (
                r["buyout_price"].as_str().unwrap().parse().unwrap(),
                r["top_bid"].as_str().unwrap().parse().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        prices,
        [(600, 100), (600, 200), (650, 200), (650, 200), (650, 200)]
    );
    let stats = fixture()["item"]["attributes"].as_array().unwrap().len();
    assert!(stats > 0);
    assert_eq!(
        attributes.len(),
        expected * stats,
        "every row keeps its stats"
    );
    assert_eq!(
        part_count(&root.0, "auctions"),
        part_count(&root.0, "attributes")
    );

    // Deleting every feed archive and restarting neither loses rows nor replays anything.
    drop((pipeline, sink));
    let now = start + TimeDelta::days(2);
    assert_eq!(
        maintenance::prune_feeds(&root.0, now, TimeDelta::hours(6))?,
        polls.len() as u64 + 1
    );
    assert_eq!(
        store.raw_files()?.len(),
        1,
        "only the weapon archive is left"
    );
    drop(Engine::new(root.0.clone(), cutoff, args())?);
    assert_eq!(table(&root.0, "auctions"), auctions);

    // Compaction merges the day into one part per table with every value intact.
    let report = maintenance::maintain(&root.0, now, &args().storage)?;
    assert!(report.compacted_inputs > 0);
    assert_eq!(part_count(&root.0, "auctions"), 1);
    assert_eq!(part_count(&root.0, "attributes"), 1);
    assert_eq!(table(&root.0, "auctions"), auctions);
    assert_eq!(table(&root.0, "attributes"), attributes);

    // The next process still knows what it already recorded.
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new(cutoff),
        registry: Registry::open(store.registry_path())?,
        tracker: Tracker::open(store.seen_dir()),
    };
    let mut sink = ParquetSink::new(root.0.join("parquet"));
    let again = RawResponse::new(
        start + TimeDelta::hours(2) + TimeDelta::minutes(10),
        wfmt_types::FEED_POPULAR.into(),
        "pc".into(),
        vec![RawPage {
            buyout_policy: None,
            sort_by: wfmt_types::FEED_POPULAR.into(),
            body: serde_json::json!({"payload":{"auctions":[
                listing(650, 200, "2026-11-01T10:14:00+00:00")
            ]}})
            .to_string(),
        }],
        vec![],
    );
    assert_eq!(
        pipeline.process_feed(&weapons, &again, &mut sink)?.auctions,
        0
    );
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
    // Both price orders returning the same short page: the search returned everything.
    let raw = |at, auctions| {
        let body = serde_json::json!({"payload":{"auctions":auctions}}).to_string();
        RawResponse::new(
            at,
            "kulstar".into(),
            "pc".into(),
            ["price_asc", "price_desc"]
                .map(|sort_by| RawPage {
                    buyout_policy: None,
                    sort_by: sort_by.into(),
                    body: body.clone(),
                })
                .into(),
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
    // Two malformed entries on each of the two pages.
    assert_eq!(summary.skipped, 4);
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
fn starting_only_prices_must_exceed_the_mean_without_causing_false_sales() -> Result<()> {
    let root = Temp::new();
    let store = Store::new(root.0.clone());
    let weapon =
        wfmt_scraper::wfm::parse_weapons(r#"{"data":[{"slug":"kulstar","rivenType":"pistol"}]}"#)?
            .remove(0);
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new("2026-10-22T00:00:00Z".parse()?),
        registry: Registry::open(store.registry_path())?,
        tracker: Tracker::open(store.seen_dir()),
    };
    let mut sink = ParquetSink::new(root.0.join("parquet"));
    let t = Utc::now();
    let raw = |at, auctions| {
        RawResponse::new(
            at,
            "kulstar".into(),
            "pc".into(),
            vec![RawPage {
                sort_by: "price_asc".into(),
                buyout_policy: None,
                body: serde_json::json!({"payload": {"auctions": auctions}}).to_string(),
            }],
            vec![],
        )
    };
    let mut original = fixture();
    original["buyout_price"] = 500.into();
    pipeline.process(&weapon, &raw(t, vec![original.clone()]), &mut sink)?;
    sink.flush()?;
    original["buyout_price"] = serde_json::Value::Null;
    original["top_bid"] = serde_json::Value::Null;
    original["starting_price"] = 1.into();
    let mut bid = original.clone();
    bid["id"] = "actual-bid".into();
    bid["top_bid"] = 250.into();
    let mut buyout = original.clone();
    buyout["id"] = "auction-buyout".into();
    buyout["buyout_price"] = 750.into();
    buyout["is_direct_sell"] = false.into();
    let mut zero = original.clone();
    zero["id"] = "zero-price".into();
    zero["buyout_price"] = 0.into();
    zero["top_bid"] = 0.into();
    zero["starting_price"] = 0.into();
    let mut below = original.clone();
    below["id"] = "below-average".into();
    below["starting_price"] = 499.into();
    let mut equal = original.clone();
    equal["id"] = "equal-average".into();
    equal["starting_price"] = 500.into();
    let mut above = original.clone();
    above["id"] = "above-average".into();
    above["starting_price"] = 501.into();
    let duplicate_bid = bid.clone();
    let mut next = raw(
        t + TimeDelta::days(5),
        vec![
            original.clone(),
            bid,
            buyout,
            zero,
            below,
            equal,
            above.clone(),
        ],
    );
    next.pages.push(RawPage {
        sort_by: "price_desc".into(),
        buyout_policy: None,
        body: serde_json::json!({"payload": {"auctions": [duplicate_bid]}}).to_string(),
    });
    let summary = pipeline.process(&weapon, &next, &mut sink)?;
    assert_eq!(summary.auctions, 3);
    assert_eq!(
        summary.gone, 0,
        "a listing losing its price is still present"
    );
    assert_eq!(sink.auctions.len(), 3);
    assert!(
        sink.auctions
            .iter()
            .any(|a| a.auction_id == "actual-bid" && a.top_bid == Some(250))
    );
    assert!(
        sink.auctions
            .iter()
            .any(|a| a.auction_id == "auction-buyout" && a.buyout_price == Some(750))
    );
    sink.flush()?;
    let seen: serde_json::Value =
        serde_json::from_slice(&fs::read(store.seen_dir().join("kulstar.json"))?)?;
    assert!(seen.get(original["id"].as_str().unwrap()).is_some());
    assert!(seen.get("zero-price").is_none());
    assert!(seen.get("below-average").is_none());
    assert!(seen.get("equal-average").is_none());
    assert!(seen.get("above-average").is_some());
    // A starting price cannot set its own benchmark when no buyouts or bids were fetched.
    let summary = pipeline.process(
        &weapon,
        &raw(t + TimeDelta::days(6), vec![above]),
        &mut sink,
    )?;
    assert_eq!(summary.auctions, 0);
    assert!(sink.auctions.is_empty());
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
