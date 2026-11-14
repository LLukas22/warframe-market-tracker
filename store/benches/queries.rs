//! Timings of the server's queries on the real dataset in `./data` (or `WFMT_DATA_DIR`), which
//! is far bigger than any test fixture:
//!
//! ```sh
//! cargo bench -p wfmt-store
//! ```
//!
//! Without a dataset there's nothing to measure, and it says so.

use std::path::PathBuf;
use std::sync::OnceLock;

use divan::Bencher;
use wfmt_api::{Comparisons, CurseFilter, Listing, Period, Query, RivenScan, ScanAttr, Sort};
use wfmt_dataset::layout::{PARQUET_DIR, REGISTRY};
use wfmt_store::{Data, Store};

fn main() {
    if !data_dir().join(PARQUET_DIR).exists() {
        eprintln!("no dataset at {}, nothing to measure", data_dir().display());
        return;
    }
    // Loaded here, so the first benchmark doesn't time it.
    fixture();
    divan::main();
}

fn data_dir() -> PathBuf {
    std::env::var_os("WFMT_DATA_DIR").map_or_else(
        || PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../data")),
        PathBuf::from,
    )
}

/// The dataset, loaded once, and the cases the benchmarks pick from it.
struct Fixture {
    store: Store,
    now: i64,
    /// The weapon with the most listings, and one in the middle.
    busiest: String,
    middling: String,
    /// The cheapest current listing of the busiest weapon: one with history and comparisons.
    listing: Listing,
    /// That listing, as a screenshot would read it.
    scan: RivenScan,
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let store = Store::new(data_dir());
        store.refresh();
        let now = chrono::Utc::now().timestamp_millis();
        let (busiest, middling, listing) = {
            let data = store.read();
            let overview = data.overview(now);
            let mut weapons: Vec<_> = overview.weapons.iter().filter(|w| w.listings > 0).collect();
            weapons.sort_by_key(|w| std::cmp::Reverse(w.listings));
            let busiest = weapons[0].slug.clone();
            let middling = weapons[weapons.len() / 2].slug.clone();
            let listing = data
                .search(&weapon(&busiest), now)
                .listings
                .into_iter()
                .next()
                .expect("the busiest weapon has a listing");
            (busiest, middling, listing)
        };
        let scan = RivenScan {
            weapon: Some(busiest.clone()),
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
        Fixture {
            store,
            now,
            busiest,
            middling,
            listing,
            scan,
        }
    })
}

fn data() -> std::sync::RwLockReadGuard<'static, Data> {
    fixture().store.read()
}

fn weapon(slug: &str) -> Query {
    Query {
        weapon: Some(slug.to_owned()),
        ..Query::default()
    }
}

/// Picks one of the fixture's two weapons by name, so the benchmark labels say which.
fn pick(which: &str) -> &'static str {
    match which {
        "busiest" => &fixture().busiest,
        _ => &fixture().middling,
    }
}

#[divan::bench(sample_count = 3, sample_size = 1)]
fn full_load() {
    let store = Store::new(data_dir());
    store.refresh();
    divan::black_box(store);
}

#[divan::bench]
fn refresh_nothing_new() {
    fixture().store.refresh();
}

#[divan::bench]
fn live() -> (u32, u64) {
    data().live()
}

#[divan::bench]
fn overview() -> usize {
    data().overview(fixture().now).weapons.len()
}

#[divan::bench(args = [Period::Today, Period::Week, Period::Month, Period::All])]
fn search_all_weapons(period: Period) -> usize {
    let query = Query {
        period,
        ..Query::default()
    };
    data().search(&query, fixture().now).total
}

#[divan::bench(args = Sort::ALL)]
fn search_all_weapons_sorted(sort: Sort) -> usize {
    let query = Query {
        sort,
        ..Query::default()
    };
    data().search(&query, fixture().now).total
}

#[divan::bench]
fn search_all_weapons_nothing_filtered() -> usize {
    let query = Query {
        include_legacy: true,
        include_unfit: true,
        include_outliers: true,
        direct_only: false,
        min_price: None,
        period: Period::All,
        ..Query::default()
    };
    data().search(&query, fixture().now).total
}

#[divan::bench]
fn search_all_weapons_by_stats() -> usize {
    let query = Query {
        positives: vec!["critical_chance".into(), "critical_damage".into()],
        curse: CurseFilter::Required,
        min_quality: 0.5,
        ..Query::default()
    };
    data().search(&query, fixture().now).total
}

#[divan::bench]
fn search_all_weapons_changed() -> usize {
    let query = Query {
        price_changed: true,
        owner_changed: true,
        ..Query::default()
    };
    data().search(&query, fixture().now).total
}

#[divan::bench(args = [false, true])]
fn search_all_weapons_page_two(gone_tab: bool) -> usize {
    data()
        .search_page(&Query::default(), 50, Some(gone_tab), fixture().now)
        .listings
        .len()
}

#[divan::bench(args = ["busiest", "middling"])]
fn search_weapon(which: &str) -> usize {
    data().search(&weapon(pick(which)), fixture().now).total
}

#[divan::bench(args = ["busiest", "middling"])]
fn search_weapon_all_time(which: &str) -> usize {
    let query = Query {
        period: Period::All,
        ..weapon(pick(which))
    };
    data().search(&query, fixture().now).total
}

#[divan::bench(args = ["busiest", "middling"])]
fn stat_effects(which: &str) -> u32 {
    data().stat_effects(pick(which)).rivens
}

#[divan::bench(sample_count = 10)]
fn stat_effects_every_weapon(bencher: Bencher) {
    let slugs: Vec<String> = data()
        .overview(fixture().now)
        .weapons
        .into_iter()
        .map(|w| w.slug)
        .collect();
    bencher.bench(|| {
        let data = data();
        slugs
            .iter()
            .map(|slug| data.stat_effects(slug).rivens)
            .sum::<u32>()
    });
}

#[divan::bench(args = [false, true])]
fn riven_detail(all_comparisons: bool) -> bool {
    let fixture = fixture();
    let comparisons = Comparisons {
        legacy: all_comparisons,
        unfit: all_comparisons,
        loose: all_comparisons,
    };
    data()
        .riven_detail(
            &fixture.listing.riven_id,
            Some(&fixture.listing.auction_id),
            comparisons,
            fixture.now,
        )
        .is_some()
}

#[divan::bench]
fn riven_history() -> usize {
    data().riven_history(&fixture().listing.riven_id).len()
}

#[divan::bench]
fn search_riven_id_prefix() -> usize {
    let query = Query {
        riven_id: Some(fixture().listing.riven_id[..8].to_owned()),
        ..Query::default()
    };
    data().search(&query, fixture().now).total
}

#[divan::bench]
fn scan_match() -> bool {
    let registry = data_dir().join(REGISTRY);
    data()
        .scan_match(&fixture().scan, &registry, fixture().now)
        .is_some()
}

#[divan::bench]
fn scanned_detail() -> bool {
    data()
        .scanned_detail(&fixture().scan, Comparisons::default(), fixture().now)
        .is_some()
}
