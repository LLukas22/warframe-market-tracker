//! The store's answers must follow from the rows alone, not from the order they are stored in.
//! Compaction sorts each day, which reorders rows with equal timestamps, so the same rows are
//! written here in several orders and splits into parts, and every answer must stay the same.

use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};
use wfmt_api::{Comparisons, Period, Query, Sort};
use wfmt_dataset::schema::{AttributeRow, AuctionRow, GoneReason, GoneRow};
use wfmt_dataset::sink::ParquetSink;
use wfmt_store::{Data, Store};
use wfmt_types::{RivenType, WeaponClass};

const HOUR: i64 = 3_600_000;
/// 2026-11-01T00:00:00Z. Every row falls on this day.
const DAY: i64 = 1_793_491_200_000;
const NOW: i64 = DAY + 24 * HOUR;

#[derive(Clone)]
struct Sighting {
    auction: &'static str,
    riven: &'static str,
    t: i64,
    price: i64,
    created: i64,
    /// Positive stats, in the riven's order.
    stats: Vec<&'static str>,
}

const STATS: [&str; 6] = [
    "critical_chance",
    "critical_damage",
    "multishot",
    "damage",
    "toxin_damage",
    "status_chance",
];

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

#[derive(Clone)]
struct Disappearance {
    auction: &'static str,
    riven: &'static str,
    t: i64,
    price: i64,
    created: i64,
    first_seen: i64,
}

fn time(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap()
}

/// The rows. Several cases where only the stored order could tell rows apart:
/// - `r-twin`: two copies of one riven, sighted at the same time at different prices, and gone
///   at the same time at different prices. `a2` is the relist (created later).
/// - `r-same`: two copies that are equally recent in every respect but their id.
/// - `r-d`: gone at the same time as `r-twin`, so the gone tab orders equal times.
/// - `c*`: ordinary listings sighted at the same times as everything else.
fn rows() -> (Vec<Sighting>, Vec<Disappearance>) {
    let mut sightings = Vec::new();
    let mut with_stats = |auction, riven, t, price, created, stats: Vec<&'static str>| {
        sightings.push(Sighting {
            auction,
            riven,
            t,
            price,
            created,
            stats,
        });
    };
    // Enough rivens with different stats for the stat effects, so the stats are first met in a
    // different order whenever the rows are.
    let combinations: Vec<[usize; 3]> = (0..6)
        .flat_map(|a| (a + 1..6).flat_map(move |b| (b + 1..6).map(move |c| [a, b, c])))
        .collect();
    for i in 0..40 {
        let mut stats: Vec<&'static str> = combinations[i % combinations.len()]
            .iter()
            .map(|&j| STATS[j])
            .collect();
        stats.rotate_left(i % 3);
        let price = 100
            + 150 * i64::from(stats.contains(&"multishot"))
            + 80 * i64::from(stats.contains(&"damage"))
            + 20 * i64::from(stats.contains(&"toxin_damage"))
            + 7 * i as i64;
        for t in [DAY + HOUR, DAY + 2 * HOUR] {
            with_stats(
                leak(format!("e{i:02}")),
                leak(format!("r-e{i:02}")),
                t,
                price,
                DAY - 24 * HOUR,
                stats.clone(),
            );
        }
    }
    // Four stats: only three-stat rivens share at most three, so its similar rivens include
    // some missing two of them, which gives those lists an order to get wrong.
    with_stats(
        "w1",
        "r-wide",
        DAY + HOUR,
        500,
        DAY - 24 * HOUR,
        vec![
            "toxin_damage",
            "multishot",
            "critical_chance",
            "status_chance",
        ],
    );
    // Two equally recent copies at the same price but with different stats: which one the stat
    // effects use must not depend on the order.
    with_stats(
        "t1",
        "r-tie",
        DAY + HOUR,
        400,
        DAY - 24 * HOUR,
        vec!["critical_chance", "multishot"],
    );
    with_stats(
        "t2",
        "r-tie",
        DAY + HOUR,
        400,
        DAY - 24 * HOUR,
        vec!["critical_chance", "damage"],
    );
    let mut s = |auction, riven, t, price, created| {
        with_stats(
            auction,
            riven,
            t,
            price,
            created,
            vec!["critical_chance", "critical_damage"],
        );
    };
    s("a1", "r-twin", DAY + HOUR, 100, DAY - 48 * HOUR);
    s("a2", "r-twin", DAY + HOUR, 300, DAY - 24 * HOUR);
    s("d1", "r-d", DAY + HOUR, 200, DAY - 24 * HOUR);
    for t in [DAY + HOUR, DAY + 2 * HOUR] {
        s("b1", "r-same", t, 200, DAY - 24 * HOUR);
        s("b2", "r-same", t, 250, DAY - 24 * HOUR);
    }
    for (i, (auction, riven)) in [
        ("c1", "r-c1"),
        ("c2", "r-c2"),
        ("c3", "r-c3"),
        ("c4", "r-c4"),
        ("c5", "r-c5"),
    ]
    .into_iter()
    .enumerate()
    {
        for t in [DAY + HOUR, DAY + 2 * HOUR, DAY + 5 * HOUR] {
            s(auction, riven, t, 150 + 50 * i as i64, DAY - 24 * HOUR);
        }
    }
    let gone = vec![
        Disappearance {
            auction: "a1",
            riven: "r-twin",
            t: DAY + 5 * HOUR,
            price: 100,
            created: DAY - 48 * HOUR,
            first_seen: DAY + HOUR,
        },
        Disappearance {
            auction: "a2",
            riven: "r-twin",
            t: DAY + 5 * HOUR,
            price: 300,
            created: DAY - 24 * HOUR,
            first_seen: DAY + HOUR,
        },
        Disappearance {
            auction: "d1",
            riven: "r-d",
            t: DAY + 5 * HOUR,
            price: 200,
            created: DAY - 24 * HOUR,
            first_seen: DAY + HOUR,
        },
    ];
    (sightings, gone)
}

fn auction_row(s: &Sighting) -> AuctionRow {
    AuctionRow {
        scraped_at: time(s.t),
        platform: "pc".into(),
        auction_id: s.auction.into(),
        riven_id: s.riven.into(),
        source: "price_asc".into(),
        weapon_slug: "w".into(),
        weapon_name: "W".into(),
        weapon_class: Some(WeaponClass::Rifle),
        riven_type: RivenType::Rifle,
        is_bow: false,
        riven_name: format!("{}-name", s.riven),
        polarity: "madurai".into(),
        mastery_level: Some(10),
        re_rolls: Some(1),
        n_positive: s.stats.len() as i32,
        n_negative: 0,
        has_spliced: false,
        listed_rank: 8,
        rank: Some(8),
        disposition: Some(1.0),
        legacy_stats: Some(false),
        fit_ok: true,
        fit_ambiguous: Some(false),
        fit_error: None,
        mean_positive_quality: Some(0.5),
        buyout_price: Some(s.price),
        starting_price: Some(s.price),
        top_bid: None,
        is_direct_sell: true,
        minimal_reputation: None,
        visible: true,
        closed: false,
        private: false,
        crossplay: true,
        created: time(s.created),
        updated: time(s.created),
        owner_id: format!("{}-owner", s.auction),
        owner_name: format!("{}-owner", s.auction),
        owner_reputation: Some(1),
        owner_status: "ingame".into(),
        owner_last_seen: None,
        note: String::new(),
    }
}

fn attribute_rows(s: &Sighting) -> Vec<AttributeRow> {
    s.stats
        .iter()
        .copied()
        .enumerate()
        .map(|(position, attribute)| AttributeRow {
            scraped_at: time(s.t),
            auction_id: s.auction.into(),
            weapon_slug: "w".into(),
            position: position as i32,
            attribute: attribute.into(),
            positive: true,
            spliced: Some(false),
            value: 100.0,
            value_rank8: Some(100.0),
            roll: Some(0.5),
            quality: Some(0.5),
            coefficient: Some(1.0),
            layout_multiplier: Some(1.0),
        })
        .collect()
}

fn gone_row(g: &Disappearance) -> GoneRow {
    GoneRow {
        detected_at: time(g.t),
        weapon_slug: "w".into(),
        auction_id: g.auction.into(),
        reason: GoneReason::Verified,
        riven_id: g.riven.into(),
        last_price: Some(g.price),
        direct_sell: true,
        owner_name: format!("{}-owner", g.auction),
        created: time(g.created),
        first_seen: time(g.first_seen),
        last_seen: time(g.first_seen),
    }
}

/// Writes the rows in this order, split round-robin into `parts` flushes.
fn write(root: &Path, sightings: &[Sighting], gone: &[Disappearance], parts: usize) -> Result<()> {
    let mut sink = ParquetSink::new(root.join("parquet"));
    for part in 0..parts {
        sink.prepare(time(DAY))?;
        for s in sightings.iter().skip(part).step_by(parts) {
            sink.auctions.push(auction_row(s));
            sink.attributes.extend(attribute_rows(s));
        }
        for g in gone.iter().skip(part).step_by(parts) {
            sink.gone.push(gone_row(g));
        }
        sink.flush()?;
    }
    Ok(())
}

fn query(sort: Sort) -> Query {
    Query {
        weapon: Some("w".into()),
        period: Period::All,
        direct_only: false,
        min_price: None,
        include_legacy: true,
        include_unfit: true,
        include_outliers: true,
        sort,
        ..Query::default()
    }
}

/// Every answer the dataset gives, as JSON.
fn answers(data: &Data) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    let mut add = |name: String, value: serde_json::Value| out.push((name, value));
    add(
        "overview".into(),
        serde_json::to_value(data.overview(NOW)).unwrap(),
    );
    for sort in Sort::ALL {
        let q = query(sort);
        add(
            format!("search {}", sort.key()),
            serde_json::to_value(data.search(&q, NOW).unwrap()).unwrap(),
        );
        add(
            format!("gone {}", sort.key()),
            serde_json::to_value(data.search_page(&q, 0, Some(true), NOW).unwrap()).unwrap(),
        );
    }
    let all_weapons = Query {
        weapon: None,
        ..query(Sort::PriceAsc)
    };
    add(
        "search all weapons".into(),
        serde_json::to_value(data.search(&all_weapons, NOW).unwrap()).unwrap(),
    );
    let (sightings, _) = rows();
    let mut rivens: Vec<&str> = sightings.iter().map(|s| s.riven).collect();
    rivens.sort_unstable();
    rivens.dedup();
    add(
        "stat effects".into(),
        serde_json::to_value(data.stat_effects("w")).unwrap(),
    );
    let every_comparison = Comparisons {
        legacy: true,
        unfit: true,
        loose: true,
    };
    for riven in rivens {
        add(
            format!("detail {riven}"),
            serde_json::to_value(data.riven_detail(riven, None, every_comparison, NOW)).unwrap(),
        );
        add(
            format!("history {riven}"),
            serde_json::to_value(data.riven_history(riven).unwrap()).unwrap(),
        );
        let by_id = Query {
            riven_id: Some(riven.into()),
            ..query(Sort::PriceAsc)
        };
        add(
            format!("id search {riven}"),
            serde_json::to_value(data.search(&by_id, NOW).unwrap()).unwrap(),
        );
    }
    out
}

fn load(
    sightings: &[Sighting],
    gone: &[Disappearance],
    parts: usize,
) -> Result<Vec<(String, serde_json::Value)>> {
    let temp = tempfile::tempdir()?;
    write(temp.path(), sightings, gone, parts)?;
    let store = Store::new(temp.path().to_path_buf());
    store.refresh();
    let data = store.read();
    Ok(answers(&data))
}

#[test]
fn answers_do_not_depend_on_row_order() -> Result<()> {
    let (sightings, gone) = rows();
    let reference = load(&sightings, &gone, 1)?;

    let reversed = |v: &[Sighting]| v.iter().rev().cloned().collect::<Vec<_>>();
    let mut by_id_desc = sightings.clone();
    by_id_desc.sort_by(|a, b| b.auction.cmp(a.auction).then(a.t.cmp(&b.t)));
    let mut gone_by_id_desc = gone.clone();
    gone_by_id_desc.sort_by(|a, b| b.auction.cmp(a.auction));
    // The order compaction writes: by weapon, auction, time.
    let mut sorted = sightings.clone();
    sorted.sort_by(|a, b| a.auction.cmp(b.auction).then(a.t.cmp(&b.t)));
    let mut gone_sorted = gone.clone();
    gone_sorted.sort_by(|a, b| a.auction.cmp(b.auction).then(a.t.cmp(&b.t)));
    let gone_reversed: Vec<_> = gone.iter().rev().cloned().collect();

    let variants: Vec<(&str, Vec<Sighting>, Vec<Disappearance>, usize)> = vec![
        ("reversed", reversed(&sightings), gone_reversed.clone(), 1),
        (
            "by id, descending",
            by_id_desc.clone(),
            gone_by_id_desc.clone(),
            1,
        ),
        ("sorted", sorted.clone(), gone_sorted.clone(), 1),
        ("three parts", sightings.clone(), gone.clone(), 3),
        (
            "three parts, reversed",
            reversed(&sightings),
            gone_reversed,
            3,
        ),
        (
            "two parts, by id descending",
            by_id_desc,
            gone_by_id_desc,
            2,
        ),
        ("two parts, sorted", sorted, gone_sorted, 2),
    ];
    for (name, sightings, gone, parts) in variants {
        let answers = load(&sightings, &gone, parts)?;
        assert_eq!(answers.len(), reference.len());
        for ((query, expected), (_, actual)) in reference.iter().zip(&answers) {
            assert_eq!(actual, expected, "{query} differs when the rows are {name}");
        }
    }
    Ok(())
}

/// What the ties resolve to, written out rather than taken from a run.
#[test]
fn simultaneous_copies_resolve_to_the_newest() -> Result<()> {
    let (sightings, gone) = rows();
    let temp = tempfile::tempdir()?;
    write(temp.path(), &sightings, &gone, 1)?;
    let store = Store::new(temp.path().to_path_buf());
    store.refresh();
    let data = store.read();
    let q = query(Sort::PriceAsc);

    // One gone entry per riven: the relist `a2` stands for `r-twin`, not `a1`.
    let gone_tab = data.search_page(&q, 0, Some(true), NOW).unwrap();
    let mut gone_ids: Vec<&str> = gone_tab
        .gone
        .iter()
        .map(|l| l.auction_id.as_str())
        .collect();
    gone_ids.sort_unstable();
    assert_eq!(gone_ids, ["a2", "d1"]);

    // The chart's gone median in that hour counts `a2`'s 300 and `d1`'s 200, not `a1`'s 100.
    let result = data.search(&q, NOW).unwrap();
    let point = result
        .history
        .iter()
        .find(|p| p.t == DAY + 5 * HOUR)
        .expect("a chart point for the hour the listings went");
    assert_eq!(point.gone_count, 2);
    assert_eq!(point.gone_median, Some(300));

    // Equally recent copies: the higher id is the newest, so `b2` is the one listed.
    let listed: Vec<&str> = result
        .listings
        .iter()
        .filter(|l| l.riven_id == "r-same")
        .map(|l| l.auction_id.as_str())
        .collect();
    assert_eq!(listed, ["b2"]);

    // Stats that two rivens don't share are listed by name; their ids follow the load order.
    let every_comparison = Comparisons {
        legacy: true,
        unfit: true,
        loose: true,
    };
    let mut longest = 0;
    let rivens = (0..40)
        .map(|i| format!("r-e{i:02}"))
        .chain(["r-wide".to_owned()]);
    for riven in rivens {
        let detail = data
            .riven_detail(&riven, None, every_comparison, NOW)
            .expect("a riven page");
        for similar in &detail.similar {
            for list in [&similar.missing_positives, &similar.added_positives] {
                assert!(list.is_sorted(), "{list:?} isn't sorted");
                longest = longest.max(list.len());
            }
        }
    }
    assert!(longest >= 2, "no list long enough to have an order");

    // The stat effects have enough rivens to say something, so the order test covers them.
    let effects = data.stat_effects("w");
    assert!(effects.rivens >= 40, "{} rivens", effects.rivens);
    assert!(!effects.positive.is_empty(), "no stat effects to compare");
    Ok(())
}

#[test]
fn parquet_chart_carries_prices_and_keeps_withdrawals() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut sink = ParquetSink::new(temp.path().join("parquet"));
    let base = Sighting {
        auction: "variable",
        riven: "r-variable",
        t: DAY,
        price: 100,
        created: DAY,
        stats: vec!["damage"],
    };
    // The opening bucket includes a price from before the period. In the next
    // bucket the last sighting withdraws its price, and the next bucket restores it.
    let start = DAY + 24 * HOUR;
    for (t, price) in [
        (DAY, Some(100)),
        (start + HOUR, Some(200)),
        (start + HOUR + 1, None),
        (start + 2 * HOUR, Some(300)),
    ] {
        let sighting = Sighting { t, ..base.clone() };
        sink.prepare(time(t))?;
        let mut auction = auction_row(&sighting);
        auction.buyout_price = price;
        auction.starting_price = None;
        auction.top_bid = None;
        sink.auctions.push(auction);
        sink.attributes.extend(attribute_rows(&sighting));
        // A constant second listing gives the opening and withdrawn-price buckets a point.
        let steady = Sighting {
            auction: "steady",
            riven: "r-steady",
            price: 500,
            ..sighting
        };
        sink.auctions.push(auction_row(&steady));
        sink.attributes.extend(attribute_rows(&steady));
        sink.flush()?;
    }
    // There must be a scrape in the opening bucket to set the beginning of this chart.
    let steady = Sighting {
        auction: "steady",
        riven: "r-steady",
        t: start,
        price: 500,
        ..base
    };
    sink.prepare(time(start))?;
    sink.auctions.push(auction_row(&steady));
    sink.attributes.extend(attribute_rows(&steady));
    sink.flush()?;

    let store = Store::new(temp.path().to_owned());
    store.refresh();
    let data = store.read();
    let mut q = query(Sort::Newest);
    q.period = Period::Custom {
        from: Some(start),
        to: None,
    };
    let page = data.search(&q, start + 3 * HOUR)?;
    let points: Vec<_> = page.history.iter().map(|p| (p.t, p.count, p.min)).collect();
    assert_eq!(
        points,
        [
            (start, 2, 100),
            (start + HOUR, 1, 500),
            (start + 2 * HOUR, 2, 300)
        ]
    );
    let journey = data.riven_history("r-variable")?;
    let prices: Vec<_> = journey[0].points.iter().map(|p| p.price).collect();
    assert_eq!(prices, [Some(100), Some(200), None, Some(300)]);
    Ok(())
}
