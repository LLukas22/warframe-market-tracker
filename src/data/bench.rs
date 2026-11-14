//! Timings of every server computation on the real dataset in `./data` (or `WFMT_DATA_DIR`).
//! Not a correctness test, so it's ignored by default. Run it optimized, as the server runs:
//!
//! ```sh
//! cargo test --release --no-default-features --features server --bin warframe-market-tracker \
//!     data::bench -- --ignored --nocapture
//! ```
//!
//! Each case reports the median and minimum of several runs, plus the JSON size of its answer.

use std::hint::black_box;
use std::time::Instant;

use super::load::read_batches;
use super::*;
use crate::api::{Period, RivenScan, ScanAttr};

/// Runs `f` until about 300 ms or 200 runs have passed (at least 3) and prints its timings.
fn bench<T: serde::Serialize>(name: &str, mut f: impl FnMut() -> T) {
    let mut times = Vec::new();
    let started = Instant::now();
    let mut out = None;
    while times.len() < 3 || (times.len() < 200 && started.elapsed() < Duration::from_millis(300)) {
        let t = Instant::now();
        out = Some(black_box(f()));
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    times.sort_by(f64::total_cmp);
    let bytes = serde_json::to_vec(&out).map_or(0, |v| v.len());
    eprintln!(
        "{name:44} median {:>8.3} ms   min {:>8.3} ms   runs {:>3}   {:>7} B",
        times[times.len() / 2],
        times[0],
        times.len(),
        bytes,
    );
}

#[test]
#[ignore = "benchmark on the real dataset"]
fn server_computations() {
    let dir = data_dir();
    if !dir.join("parquet").exists() {
        eprintln!("no dataset at {}, nothing to measure", dir.display());
        return;
    }

    let started = Instant::now();
    let parquet = dir.join("parquet");
    let mut rows = 0;
    for (auctions, attributes) in part_files(&parquet) {
        for path in [auctions, attributes] {
            for batch in read_batches(&path).unwrap() {
                rows += batch.unwrap().num_rows();
            }
        }
    }
    eprintln!(
        "{:44} {:>8.1} ms   ({rows} rows)",
        "parquet decode only",
        started.elapsed().as_secs_f64() * 1000.0
    );

    let started = Instant::now();
    let store = Store::new(dir.clone());
    store.refresh();
    eprintln!(
        "{:44} {:>8.1} ms",
        "full load (parquet → memory)",
        started.elapsed().as_secs_f64() * 1000.0
    );
    {
        let inner = store.inner.read().unwrap();
        eprintln!(
            "auctions {}, observations {}, gone events {}, rivens {}, weapons {}",
            inner.auctions.len(),
            inner.observations.len(),
            inner.gone.len(),
            inner.rivens.len(),
            inner.weapons.len()
        );
    }
    {
        let mut inner = store.inner.write().unwrap();
        let t = Instant::now();
        inner.finish_load();
        eprintln!(
            "{:44} {:>8.1} ms",
            "finish_load (after every batch)",
            t.elapsed().as_secs_f64() * 1000.0
        );
    }
    bench("refresh, nothing new", || store.refresh());

    let inner = store.inner.read().unwrap();
    bench("live", || inner.live());
    bench("overview", || inner.overview());

    let overview = inner.overview();
    let mut busiest: Vec<_> = overview.weapons.iter().filter(|w| w.listings > 0).collect();
    busiest.sort_by_key(|w| std::cmp::Reverse(w.listings));
    let weapon = busiest[0].slug.clone();
    let quiet = busiest[busiest.len() / 2].slug.clone();
    eprintln!("busiest weapon: {weapon}, a middling one: {quiet}");

    let all = Query::default();
    bench("search: all weapons (7 days)", || inner.search(&all));
    for period in [Period::Today, Period::Month, Period::All] {
        let q = Query {
            period,
            ..Query::default()
        };
        bench(&format!("search: all weapons ({period:?})"), || {
            inner.search(&q)
        });
    }
    for sort in [Sort::PriceDesc, Sort::QualityDesc, Sort::Newest] {
        let q = Query {
            sort,
            ..Query::default()
        };
        bench(&format!("search: all weapons, sort {sort:?}"), || {
            inner.search(&q)
        });
    }
    let q = Query {
        include_legacy: true,
        include_unfit: true,
        include_outliers: true,
        direct_only: false,
        min_price: None,
        period: Period::All,
        ..Query::default()
    };
    bench("search: all weapons, nothing filtered, All", || {
        inner.search(&q)
    });
    let q = Query {
        positives: vec!["critical_chance".into(), "critical_damage".into()],
        curse: CurseFilter::Required,
        min_quality: 0.5,
        ..Query::default()
    };
    bench("search: all weapons, stats + curse + quality", || {
        inner.search(&q)
    });
    let q = Query {
        price_changed: true,
        owner_changed: true,
        ..Query::default()
    };
    bench("search: all weapons, price/owner changed", || {
        inner.search(&q)
    });
    bench("search page 2: all weapons", || {
        inner.search_page(&all, 50, Some(false))
    });
    bench("search page 2: all weapons, gone tab", || {
        inner.search_page(&all, 50, Some(true))
    });

    for slug in [&weapon, &quiet] {
        let q = Query {
            weapon: Some(slug.clone()),
            ..Query::default()
        };
        bench(&format!("search: {slug}"), || inner.search(&q));
        let q_all = Query {
            weapon: Some(slug.clone()),
            period: Period::All,
            ..Query::default()
        };
        bench(&format!("search: {slug} (All)"), || inner.search(&q_all));
        bench(&format!("search page 2: {slug}"), || {
            inner.search_page(&q, 50, Some(false))
        });
        bench(&format!("stat_effects: {slug}"), || {
            inner.stat_effects(slug)
        });
    }
    bench("stat_effects: every weapon", || {
        overview
            .weapons
            .iter()
            .map(|w| inner.stat_effects(&w.slug).rivens)
            .sum::<u32>()
    });

    // A riven with history and comparisons: the cheapest current one of the busiest weapon.
    let listing = inner
        .search(&Query {
            weapon: Some(weapon.clone()),
            ..Query::default()
        })
        .listings
        .into_iter()
        .next()
        .expect("the busiest weapon has a listing");
    bench("riven_detail", || {
        inner.riven_detail(
            &listing.riven_id,
            Some(&listing.auction_id),
            Comparisons::default(),
        )
    });
    bench("riven_detail, all options", || {
        inner.riven_detail(
            &listing.riven_id,
            Some(&listing.auction_id),
            Comparisons {
                legacy: true,
                unfit: true,
                loose: true,
            },
        )
    });
    bench("riven_history", || inner.riven_history(&listing.riven_id));
    let q = Query {
        riven_id: Some(listing.riven_id[..8].to_owned()),
        ..Query::default()
    };
    bench("search: riven id prefix", || inner.search(&q));

    let scan = RivenScan {
        weapon: Some(weapon.clone()),
        name: Some(listing.riven_name.clone()),
        mastery_rank: listing.mastery_level,
        rerolls: listing.re_rolls,
        attributes: listing
            .attributes
            .iter()
            .map(|a| ScanAttr {
                slug: a.slug.clone(),
                value: a.value,
                positive: a.positive,
            })
            .collect(),
        warnings: Vec::new(),
    };
    bench("scan_match", || inner.scan_match(&scan));
    bench("scanned_detail", || {
        inner.scanned_detail(&scan, Comparisons::default())
    });
}
