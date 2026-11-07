//! Server side: keeps the scraper's parquet output in memory and answers queries.
//!
//! New parts are picked up incrementally. When compaction replaces loaded files, rebuild
//! the cache from the new snapshot so observations aren't counted twice.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, RwLock, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arrow::array::{
    Array, BooleanArray, Float64Array, Int32Array, Int64Array, StringArray,
    TimestampMillisecondArray,
};
use dioxus::prelude::ServerFnError;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use crate::api::{
    AttrInfo, AttrView, CurseFilter, HistoryPoint, Listing, Overview, PricePoint, Query,
    RivenAuction, SearchResult, Sort, StatEffect, StatEffects, WeaponInfo,
};
use crate::effects;

const RESCAN_EVERY: Duration = Duration::from_secs(5);
const MAX_LISTINGS: usize = 200;
const MAX_GONE: usize = 50;
const WEEK_MS: i64 = 7 * 86_400_000;

// Prices are free text, and sellers park rivens at joke prices like 88888 or 666666. A price is
// invalid when it's far above the weapon's normal high end. The floor protects real expensive
// rivens on cheap weapons. Tuned on real listings: flags about 0.5% of them.
const OUTLIER_PERCENTILE: f64 = 0.9;
const OUTLIER_FACTOR: i64 = 5;
const OUTLIER_FLOOR: i64 = 5000;
const OUTLIER_MIN_LISTINGS: usize = 5;

// Below these there's too little to tell which stats move the price. The bar on explained
// differences keeps out weapons where stats predict nothing, mostly those where only one end
// of the price range has been seen.
const EFFECT_MIN_RIVENS: usize = 30;
const EFFECT_MIN_STAT_RIVENS: u32 = 5;
const EFFECT_MIN_EXPLAINED: f64 = 0.15;
/// 95% interval of a normal distribution.
const INTERVAL: f64 = 1.96;

/// Prices above this are invalid. `None` when there are too few listings to tell.
fn price_limit(prices: &mut [i64]) -> Option<i64> {
    if prices.len() < OUTLIER_MIN_LISTINGS {
        return None;
    }
    prices.sort_unstable();
    let index = ((prices.len() - 1) as f64 * OUTLIER_PERCENTILE).round() as usize;
    // On small weapons the percentile alone could land on a joke price itself.
    let index = index.min(prices.len() - 3);
    Some(
        prices[index]
            .saturating_mul(OUTLIER_FACTOR)
            .max(OUTLIER_FLOOR),
    )
}

static STORE: LazyLock<Store> = LazyLock::new(|| Store::new(data_dir()));

/// The data directory (`WFMT_DATA_DIR`, else `./data`).
pub fn data_dir() -> PathBuf {
    std::env::var_os("WFMT_DATA_DIR").map_or_else(|| PathBuf::from("data"), PathBuf::from)
}

/// Loads the data from scratch on the next request, after an import replaced it.
pub fn reload() {
    STORE.reload();
}

/// Thumbnail path (relative to warframe.market's static assets) for a weapon.
pub async fn weapon_thumb(slug: String) -> Option<String> {
    run(move |store| store.thumb(&slug)).await.ok().flatten()
}

/// Refreshes the store if needed and runs `f` on a blocking thread.
pub async fn run<T, F>(f: F) -> Result<T, ServerFnError>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        STORE.refresh();
        let inner = STORE.inner.read().unwrap();
        f(&inner)
    })
    .await
    .map_err(|e| ServerFnError::new(e.to_string()))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

struct Store {
    dir: PathBuf,
    last_scan: Mutex<Option<Instant>>,
    catalog_mtime: Mutex<Option<SystemTime>>,
    inner: RwLock<Inner>,
    /// A refresh has finished, so there is data worth serving while another one runs.
    ready: AtomicBool,
}

impl Store {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            last_scan: Mutex::new(None),
            catalog_mtime: Mutex::new(None),
            inner: RwLock::new(Inner::default()),
            ready: AtomicBool::new(false),
        }
    }

    /// Forgets everything loaded, for when files were replaced under the same names. Requests
    /// wait for the next load instead of seeing an empty store.
    fn reload(&self) {
        let mut last = self.last_scan.lock().unwrap();
        self.ready.store(false, Ordering::Release);
        let mut inner = self.inner.write().unwrap();
        *inner = Inner {
            version: inner.version + 1,
            ..Inner::default()
        };
        *self.catalog_mtime.lock().unwrap() = None;
        *last = None;
    }

    fn refresh(&self) {
        // The dataset lock below waits for the scraper's writes, which can take minutes during
        // compaction. Only one request waits; the rest serve the data already loaded.
        let last = match self.last_scan.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) if self.ready.load(Ordering::Acquire) => return,
            Err(TryLockError::WouldBlock) => self.last_scan.lock().unwrap(),
            Err(TryLockError::Poisoned(e)) => panic!("refresh panicked earlier: {e}"),
        };
        if last.is_some_and(|t| t.elapsed() < RESCAN_EVERY) {
            return;
        }
        self.scan(last);
        self.ready.store(true, Ordering::Release);
    }

    fn scan(&self, mut last: std::sync::MutexGuard<'_, Option<Instant>>) {
        // Hold the dataset snapshot through discovery and loading. A reader also recovers a
        // transaction left by an interrupted writer before touching any Parquet files.
        let _snapshot = match wfmt_scraper::dataset::DatasetLock::read(&self.dir) {
            Ok(lock) => lock,
            Err(e) => {
                eprintln!("failed to open dataset: {e:#}");
                return;
            }
        };
        *last = Some(Instant::now());
        {
            let mut inner = self.inner.write().unwrap();
            if inner.loaded.iter().any(|p| !p.exists()) {
                *inner = Inner {
                    version: inner.version + 1,
                    ..Inner::default()
                };
                *self.catalog_mtime.lock().unwrap() = None;
            }
        }
        self.refresh_catalog();
        let parquet = self.dir.join("parquet");
        let (pending, pending_gone) = {
            let inner = self.inner.read().unwrap();
            let parts: Vec<(PathBuf, PathBuf)> = part_files(&parquet)
                .into_iter()
                .filter(|(a, _)| !inner.loaded.contains(a))
                .collect();
            let gone: Vec<PathBuf> = gone_files(&parquet)
                .into_iter()
                .filter(|g| !inner.loaded.contains(g))
                .collect();
            (parts, gone)
        };
        if pending.is_empty() && pending_gone.is_empty() {
            return;
        }
        let mut inner = self.inner.write().unwrap();
        for (auctions, attributes) in pending {
            match inner.load(&auctions, &attributes) {
                Ok(()) => {
                    inner.loaded.insert(auctions);
                }
                Err(e) => eprintln!("failed to load {}: {e}", auctions.display()),
            }
        }
        // Gone events refer to auctions, so they load after the auction parts.
        for path in pending_gone {
            match inner.load_gone(&path) {
                Ok(()) => {
                    inner.loaded.insert(path);
                }
                Err(e) => eprintln!("failed to load {}: {e}", path.display()),
            }
        }
        inner.finish_load();
        inner.version += 1;
    }

    /// Loads the scraper's cached `/v2/riven/weapons` so every weapon has a name, group and
    /// image, even before its first scrape.
    fn refresh_catalog(&self) {
        let path = self.dir.join("cache/riven_weapons.json");
        let Ok(mtime) = fs::metadata(&path).and_then(|m| m.modified()) else {
            return;
        };
        let mut last = self.catalog_mtime.lock().unwrap();
        if *last == Some(mtime) {
            return;
        }
        let parsed = fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Catalog>(&b).map_err(|e| e.to_string()));
        match parsed {
            Ok(catalog) => {
                let mut inner = self.inner.write().unwrap();
                for w in catalog.data {
                    let class = weapon_class(&w.riven_type, &w.group);
                    let index = inner.intern_weapon(&w.slug, &w.i18n.en.name, class);
                    let weapon = &mut inner.weapons[index as usize];
                    weapon.name = w.i18n.en.name;
                    weapon.group = w.group;
                    weapon.thumb = w.i18n.en.thumb;
                }
                *last = Some(mtime);
            }
            Err(e) => eprintln!("failed to load {}: {e}", path.display()),
        }
    }
}

#[derive(serde::Deserialize)]
struct Catalog {
    data: Vec<CatalogWeapon>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogWeapon {
    slug: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    riven_type: String,
    i18n: CatalogI18n,
}

#[derive(serde::Deserialize)]
struct CatalogI18n {
    en: CatalogName,
}

#[derive(serde::Deserialize)]
struct CatalogName {
    name: String,
    thumb: Option<String>,
}

/// Same mapping as the scraper's `WeaponClass::from_wfm`.
fn weapon_class(riven_type: &str, group: &str) -> &'static str {
    if group == "archgun" {
        return "archgun";
    }
    match riven_type {
        "rifle" => "rifle",
        "shotgun" => "shotgun",
        "pistol" | "kitgun" => "pistol",
        "melee" | "zaw" => "melee",
        _ => "unknown",
    }
}

/// Pairs of (auctions, attributes) part files that both exist, oldest first.
fn part_files(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(days) = fs::read_dir(dir.join("auctions")) else {
        return out;
    };
    for day in days.flatten() {
        let Ok(parts) = fs::read_dir(day.path()) else {
            continue;
        };
        for part in parts.flatten() {
            let path = part.path();
            if path.extension().is_none_or(|e| e != "parquet") {
                continue;
            }
            let attrs = dir
                .join("attributes")
                .join(day.file_name())
                .join(part.file_name());
            if attrs.exists() {
                out.push((path, attrs));
            }
        }
    }
    out.sort();
    out
}

/// Parquet parts of the `gone` table, oldest first.
fn gone_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(days) = fs::read_dir(dir.join("gone")) else {
        return out;
    };
    for day in days.flatten() {
        let Ok(parts) = fs::read_dir(day.path()) else {
            continue;
        };
        out.extend(
            parts
                .flatten()
                .map(|p| p.path())
                .filter(|p| p.extension().is_some_and(|e| e == "parquet")),
        );
    }
    out.sort();
    out
}

struct Weapon {
    slug: String,
    name: String,
    class: String,
    /// warframe.market group: primary, secondary, melee, archgun, zaw, kitgun, sentinel.
    group: String,
    thumb: Option<String>,
    last_scrape: i64,
    /// Prices above this are invalid, see [`price_limit`].
    price_limit: Option<i64>,
}

struct Attr {
    attr: u16,
    positive: bool,
    value: f64,
    value_rank8: Option<f64>,
    quality: Option<f64>,
}

/// Latest known state of an auction.
struct Auction {
    id: String,
    weapon: u16,
    /// Index into `Inner::rivens`.
    riven: u32,
    /// When and why the auction disappeared.
    gone: Option<(i64, String)>,
    riven_name: String,
    polarity: String,
    mastery_level: Option<i64>,
    re_rolls: Option<i64>,
    rank: Option<i32>,
    legacy_stats: Option<bool>,
    fit_ok: bool,
    mean_quality: Option<f64>,
    owner_name: String,
    owner_reputation: Option<i64>,
    created: i64,
    first_seen: i64,
    last_seen: i64,
    attrs: Vec<Attr>,
    // Latest prices.
    buyout_price: Option<i64>,
    starting_price: Option<i64>,
    top_bid: Option<i64>,
    direct_sell: bool,
}

/// One auction seen in one scrape.
struct Observation {
    t: i64,
    auction: u32,
    price: Option<i64>,
    direct_sell: bool,
}

/// An auction that disappeared (sold or removed).
struct GoneEvent {
    t: i64,
    auction: u32,
    price: Option<i64>,
    direct_sell: bool,
}

fn price_of(buyout: Option<i64>, top_bid: Option<i64>, starting: Option<i64>) -> Option<i64> {
    buyout.or(top_bid).or(starting)
}

fn auction_price(a: &Auction) -> Option<i64> {
    price_of(a.buyout_price, a.top_bid, a.starting_price)
}

fn median(mut v: Vec<i64>) -> Option<i64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use std::io::Read;
    use wfmt_scraper::{
        identity::Registry,
        maintenance::{MaintenanceArgs, maintain},
        normalize::Normalizer,
        pipeline::Pipeline,
        store::{ParquetSink, RawPage, RawResponse},
        tracking::Tracker,
        wfm::Weapon,
    };

    #[test]
    fn price_limit_catches_joke_prices_but_keeps_real_high_ends() {
        // (weapon's prices, limit). Shaped like real listings.
        let mut staircase = vec![100; 140];
        staircase.extend([800; 9]);
        staircase.extend([1800, 2000, 3000, 11111, 22222, 33333, 55555, 77777, 88888]);
        let limit = price_limit(&mut staircase).unwrap();
        assert_eq!(limit, 5000, "cheap weapon: the floor applies");
        assert_eq!(staircase.iter().filter(|p| **p > limit).count(), 6);

        // A weapon with a genuinely expensive high end keeps it.
        let mut expensive = vec![4000; 45];
        expensive.extend([9000, 13000, 15000, 696969]);
        let limit = price_limit(&mut expensive).unwrap();
        assert_eq!(limit, 20000);
        assert_eq!(expensive.iter().filter(|p| **p > limit).count(), 1);

        // A real good roll on a cheap weapon is never flagged.
        assert!(price_limit(&mut [25, 40, 60, 80, 1500]).unwrap() >= 1500);
        // Two jokes among few listings still can't drag the reference up with them.
        let mut small = vec![100; 9];
        small.extend([66666, 88888]);
        assert_eq!(price_limit(&mut small), Some(5000));
        // Too few listings to say anything.
        assert_eq!(price_limit(&mut [10, 20, 99999]), None);
    }

    fn add_auction(inner: &mut Inner, weapon: u16, id: &str, price: i64, last_seen: i64) -> u32 {
        let riven = inner.intern_riven(id);
        let index = inner.auctions.len() as u32;
        inner.auctions.push(Auction {
            id: id.into(),
            weapon,
            riven,
            gone: None,
            riven_name: String::new(),
            polarity: String::new(),
            mastery_level: None,
            re_rolls: None,
            rank: None,
            legacy_stats: Some(false),
            fit_ok: true,
            mean_quality: None,
            owner_name: id.into(),
            owner_reputation: None,
            created: 0,
            first_seen: last_seen,
            last_seen,
            attrs: Vec::new(),
            buyout_price: Some(price),
            starting_price: Some(price),
            top_bid: None,
            direct_sell: true,
        });
        inner.auction_index.insert(id.into(), index);
        inner.riven_auctions[riven as usize].push(index);
        inner.observations.push(Observation {
            t: last_seen,
            auction: index,
            price: Some(price),
            direct_sell: true,
        });
        index
    }

    /// A weapon with normal prices at 100p plus a joke-priced riven, and a joke riven that's gone.
    fn store_with_outlier() -> Inner {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        inner.weapons[weapon as usize].last_scrape = 2000;
        for i in 0..9 {
            add_auction(&mut inner, weapon, &format!("normal{i}"), 100, 2000);
        }
        add_auction(&mut inner, weapon, "joke", 66666, 2000);
        let gone = add_auction(&mut inner, weapon, "gonejoke", 88888, 1000);
        inner.auctions[gone as usize].gone = Some((1500, "missing".into()));
        inner.gone.push(GoneEvent {
            t: 1500,
            auction: gone,
            price: Some(88888),
            direct_sell: true,
        });
        inner.finish_load();
        inner
    }

    /// Rivens where +crit doubles the price and -zoom raises it by half, plus a neutral stat
    /// and optional joke prices.
    fn store_with_stat_effects(count: usize, strength: f64) -> Inner {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        inner.weapons[weapon as usize].last_scrape = 2000;
        let crit = inner.intern_attr("critical_chance");
        let zoom = inner.intern_attr("zoom");
        let toxin = inner.intern_attr("toxin_damage");
        let attr = |attr, positive| Attr {
            attr,
            positive,
            value: 1.0,
            value_rank8: None,
            quality: None,
        };
        // Prime steps spread the stats independently of each other.
        for i in 0..count {
            let (has_crit, has_zoom, has_toxin) = (i % 2 == 0, i % 3 == 0, i % 5 < 2);
            let wobble = 1.0 + ((i * 7) % 11) as f64 / 50.0;
            let mut price = 100.0 * wobble;
            if has_crit {
                price *= 1.0 + strength;
            }
            if has_zoom {
                price *= 1.0 + strength / 2.0;
            }
            let index = add_auction(&mut inner, weapon, &format!("r{i}"), price as i64, 2000);
            let attrs = &mut inner.auctions[index as usize].attrs;
            attrs.extend(has_crit.then(|| attr(crit, true)));
            attrs.extend(has_zoom.then(|| attr(zoom, false)));
            attrs.extend(has_toxin.then(|| attr(toxin, true)));
        }
        inner.finish_load();
        inner
    }

    #[test]
    fn stat_effects_split_positive_and_negative_and_find_the_right_stats() {
        let inner = store_with_stat_effects(150, 1.0);
        let result = inner.stat_effects("w");
        assert_eq!(result.rivens, 150);
        assert!(result.explained.unwrap() > 0.5);

        let names: Vec<&str> = result.positive.iter().map(|s| s.slug.as_str()).collect();
        assert_eq!(names, ["critical_chance", "toxin_damage"]);
        let crit = &result.positive[0];
        // True effects are +100% and +50%. The penalty shrinks them, keeping their order.
        assert!(
            crit.certain && (0.5..1.0).contains(&crit.effect),
            "{crit:?}"
        );
        assert!(crit.low < crit.effect && crit.effect < crit.high);
        assert!(result.positive[1].effect.abs() < 0.1);

        assert_eq!(result.negative.len(), 1);
        let zoom = &result.negative[0];
        assert_eq!(zoom.slug, "zoom");
        assert!(
            zoom.certain && (0.25..0.5).contains(&zoom.effect),
            "{zoom:?}"
        );
        assert!(crit.effect > zoom.effect);
    }

    #[test]
    fn stat_effects_ignore_invalid_prices_and_hold_back_weak_signals() {
        // A joke price mustn't change anything about the estimate.
        let clean = store_with_stat_effects(150, 1.0).stat_effects("w");
        let mut joked = store_with_stat_effects(150, 1.0);
        let weapon = joked.weapon_index["w"];
        let index = add_auction(&mut joked, weapon, "joke", 888888, 2000);
        let attrs = vec![Attr {
            attr: joked.attr_index["toxin_damage"],
            positive: true,
            value: 1.0,
            value_rank8: None,
            quality: None,
        }];
        joked.auctions[index as usize].attrs = attrs;
        joked.finish_load();
        assert_eq!(joked.stat_effects("w").positive, clean.positive);

        // Too few rivens, prices unrelated to stats, and unknown weapons show no effects.
        let few = store_with_stat_effects(20, 1.0).stat_effects("w");
        assert_eq!(few.rivens, 20);
        assert!(few.positive.is_empty() && few.negative.is_empty());
        let flat = store_with_stat_effects(150, 0.0).stat_effects("w");
        assert!(flat.positive.is_empty() && flat.negative.is_empty() && flat.rivens == 150);
        assert_eq!(flat.explained.map(|e| e < 0.15), Some(true));
        assert_eq!(clean.rare_stats, 0);
        assert_eq!(Inner::default().stat_effects("nope").rivens, 0);
    }

    #[test]
    fn capped_fetch_keeps_unconfirmed_listings_and_respects_gone_events() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        inner.weapons[weapon as usize].last_scrape = 2000;
        let a = add_auction(&mut inner, weapon, "old", 100, 1000);
        inner.finish_load();
        let query = Query {
            weapon: Some("w".into()),
            ..Query::default()
        };
        let result = inner.search(&query);
        assert_eq!(result.total, 1);
        assert!(result.listings[0].unconfirmed);
        assert_eq!(result.listings[0].last_seen, 1000);
        inner.auctions[a as usize].gone = Some((2000, "expired".into()));
        assert_eq!(inner.search(&query).total, 0);
    }

    #[test]
    fn history_uses_latest_price_per_copy_before_choosing_cheapest() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        let a = add_auction(&mut inner, weapon, "a", 100, 1000);
        let b = add_auction(&mut inner, weapon, "b", 150, 2000);
        inner.auctions[b as usize].riven = inner.auctions[a as usize].riven;
        inner.observations.push(Observation {
            t: 3000,
            auction: a,
            price: Some(200),
            direct_sell: true,
        });
        inner.finish_load();
        let query = Query {
            weapon: Some("w".into()),
            ..Query::default()
        };
        let history = inner.search(&query).history;
        assert_eq!(history.len(), 1);
        assert_eq!((history[0].count, history[0].min), (1, 150));
        // An earlier matching price must not survive a later out-of-range price.
        let filtered = Query {
            max_price: Some(175),
            ..query
        };
        assert_eq!(inner.search(&filtered).history[0].min, 150);
    }

    #[test]
    fn cooccurring_stats_are_not_reported_as_certain() {
        let mut inner = store_with_stat_effects(150, 1.0);
        let copy = inner.intern_attr("duplicate_crit");
        let crit = inner.attr_index["critical_chance"];
        for a in &mut inner.auctions {
            if a.attrs.iter().any(|attr| attr.attr == crit) {
                a.attrs.push(Attr {
                    attr: copy,
                    positive: true,
                    value: 1.0,
                    value_rank8: None,
                    quality: None,
                });
            }
        }
        let result = inner.stat_effects("w");
        assert!(result.explained.unwrap() > EFFECT_MIN_EXPLAINED);
        let aliases: Vec<_> = result
            .positive
            .iter()
            .filter(|s| s.slug == "critical_chance" || s.slug == "duplicate_crit")
            .collect();
        assert_eq!(aliases.len(), 2);
        assert!(aliases.iter().all(|s| !s.certain));
    }

    #[test]
    fn invalid_prices_stay_out_of_listings_charts_and_medians() {
        let inner = store_with_outlier();
        let query = Query {
            weapon: Some("w".into()),
            min_price: None,
            ..Query::default()
        };

        let result = inner.search(&query);
        assert_eq!(result.total, 9);
        assert_eq!(result.outliers_hidden, 1);
        assert_eq!(result.price_limit, Some(5000));
        assert!(result.gone.is_empty());
        assert!(
            result
                .history
                .iter()
                .all(|p| p.p75 == 100 && p.gone_count == 0)
        );
        let info = &inner.overview().weapons[0];
        assert_eq!((info.median_price, info.gone_7d), (Some(100), 0));

        // Asking for them adds flagged listings, but the analysis stays clean.
        let shown = inner.search(&Query {
            include_outliers: true,
            sort: Sort::PriceDesc,
            ..query.clone()
        });
        assert_eq!((shown.total, shown.outliers_hidden), (10, 0));
        assert!(shown.listings[0].price_outlier && !shown.listings[1].price_outlier);
        assert_eq!(shown.gone.len(), 1);
        assert!(
            shown
                .history
                .iter()
                .all(|p| p.p75 == 100 && p.gone_count == 0)
        );
    }

    #[test]
    fn riven_id_search_shows_an_invalid_price_with_its_chart() {
        let inner = store_with_outlier();
        let id = inner.auctions[inner.auction_index["joke"] as usize].riven;
        let result = inner.search(&Query {
            riven_id: Some(inner.rivens[id as usize].clone()),
            ..Query::default()
        });
        assert_eq!(result.total, 1);
        assert!(result.listings[0].price_outlier);
        assert_eq!(result.history.last().map(|p| p.median), Some(66666));
    }

    #[test]
    fn requests_serve_loaded_data_while_another_refresh_runs() -> anyhow::Result<()> {
        use std::sync::Arc;

        let root = std::env::temp_dir().join(format!("wfmt-web-refresh-{}", now_ms()));
        fs::create_dir_all(&root)?;
        let refresh_in_background = |store: &Arc<Store>| {
            let store = store.clone();
            std::thread::spawn(move || store.refresh())
        };

        // Nothing loaded yet: a request has to wait, or it would serve an empty page.
        let store = Arc::new(Store::new(root.clone()));
        let running = store.last_scan.lock().unwrap();
        let first = refresh_in_background(&store);
        std::thread::sleep(Duration::from_millis(100));
        assert!(!first.is_finished());
        drop(running);
        first.join().unwrap();

        // Once loaded, requests return right away instead of queueing behind a slow refresh.
        let running = store.last_scan.lock().unwrap();
        refresh_in_background(&store).join().unwrap();
        drop(running);

        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn cache_reloads_compacted_history_without_duplicate_observations() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("wfmt-web-storage-{}", now_ms()));
        fs::create_dir_all(&root)?;
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../scraper/tests/fixtures/kulstar_auctions.json.gz")[..],
        )
        .read_to_string(&mut body)?;
        let weapon = Weapon {
            slug: "kulstar".into(),
            game_ref: String::new(),
            group: "secondary".into(),
            riven_type: "pistol".into(),
            disposition: Some(1.3),
            i18n: serde_json::json!({"en":{"name":"Kulstar"}}),
        };
        let mut pipeline = Pipeline {
            normalizer: Normalizer::new("2026-10-22T00:00:00Z".parse()?),
            registry: Registry::open(root.join("rivens.jsonl"))?,
            tracker: Tracker::open(root.join("seen")),
        };
        let mut sink = ParquetSink::new(root.join("parquet"));
        for at in ["2026-11-01T00:00:00Z", "2026-11-01T12:00:00Z"] {
            let raw = RawResponse::new(
                at.parse()?,
                "kulstar".into(),
                "pc".into(),
                vec![RawPage {
                    buyout_policy: None,
                    sort_by: "price_asc".into(),
                    body: body.clone(),
                }],
                Vec::new(),
            );
            pipeline.process(&weapon, &raw, &mut sink)?;
            sink.flush()?;
        }
        let store = Store::new(root.clone());
        store.refresh();
        let observations = store.inner.read().unwrap().observations.len();
        let auctions = store.inner.read().unwrap().auctions.len();
        assert!(observations > auctions && auctions > 0);
        let options = MaintenanceArgs {
            raw_retention_days: 14,
            maintenance_interval_hours: 6,
            compact_target_mb: 128,
        };
        let report = maintain(&root, "2026-11-03T00:00:00Z".parse()?, &options)?;
        assert_eq!(report.compacted_inputs, 4);
        *store.last_scan.lock().unwrap() = None;
        store.refresh();
        {
            let inner = store.inner.read().unwrap();
            assert_eq!(inner.observations.len(), observations);
            assert_eq!(inner.auctions.len(), auctions);
            assert_eq!(inner.loaded.len(), 1);
            assert!(inner.auctions.iter().all(|a| !a.attrs.is_empty()));
        }
        // New parts still load incrementally after a compaction-driven cache rebuild.
        let raw = RawResponse::new(
            "2026-11-02T00:00:00Z".parse()?,
            "kulstar".into(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: "price_asc".into(),
                body,
            }],
            Vec::new(),
        );
        pipeline.process(&weapon, &raw, &mut sink)?;
        sink.flush()?;
        *store.last_scan.lock().unwrap() = None;
        store.refresh();
        assert_eq!(
            store.inner.read().unwrap().observations.len(),
            observations + auctions
        );
        fs::remove_dir_all(root)?;
        Ok(())
    }
}

#[derive(Default)]
pub struct Inner {
    /// Bumped whenever the loaded data changes, so clients know when to fetch again.
    version: u64,
    loaded: HashSet<PathBuf>,
    weapons: Vec<Weapon>,
    weapon_index: HashMap<String, u16>,
    attrs: Vec<String>,
    attr_index: HashMap<String, u16>,
    auctions: Vec<Auction>,
    auction_index: HashMap<String, u32>,
    observations: Vec<Observation>,
    rivens: Vec<String>,
    riven_index: HashMap<String, u32>,
    /// Auctions per riven.
    riven_auctions: Vec<Vec<u32>>,
    /// Sorted by time.
    gone: Vec<GoneEvent>,
}

type LoadResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn read_batches(path: &Path) -> LoadResult<ParquetRecordBatchReader> {
    Ok(ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?
        .with_batch_size(8192)
        .build()?)
}

macro_rules! column {
    ($batch:expr, $name:literal, $ty:ty) => {
        $batch
            .column_by_name($name)
            .and_then(|c| c.as_any().downcast_ref::<$ty>())
            .ok_or(concat!("missing column ", $name))?
    };
}

fn opt<A: Array, T>(a: &A, i: usize, get: impl Fn(&A, usize) -> T) -> Option<T> {
    a.is_valid(i).then(|| get(a, i))
}

impl Inner {
    fn intern_attr(&mut self, slug: &str) -> u16 {
        if let Some(i) = self.attr_index.get(slug) {
            return *i;
        }
        let i = self.attrs.len() as u16;
        self.attrs.push(slug.to_owned());
        self.attr_index.insert(slug.to_owned(), i);
        i
    }

    fn intern_riven(&mut self, id: &str) -> u32 {
        if let Some(i) = self.riven_index.get(id) {
            return *i;
        }
        let i = self.rivens.len() as u32;
        self.rivens.push(id.to_owned());
        self.riven_index.insert(id.to_owned(), i);
        self.riven_auctions.push(Vec::new());
        i
    }

    fn intern_weapon(&mut self, slug: &str, name: &str, class: &str) -> u16 {
        if let Some(i) = self.weapon_index.get(slug) {
            return *i;
        }
        let i = self.weapons.len() as u16;
        self.weapons.push(Weapon {
            slug: slug.to_owned(),
            name: name.to_owned(),
            class: class.to_owned(),
            group: String::new(),
            thumb: None,
            last_scrape: 0,
            price_limit: None,
        });
        self.weapon_index.insert(slug.to_owned(), i);
        i
    }

    fn load(&mut self, auctions_path: &Path, attributes_path: &Path) -> LoadResult<()> {
        // Attributes keyed by (auction id, scraped_at).
        let mut attrs: HashMap<(String, i64), Vec<Attr>> = HashMap::new();
        for b in read_batches(attributes_path)? {
            let b = b?;
            let scraped = column!(b, "scraped_at", TimestampMillisecondArray);
            let id = column!(b, "auction_id", StringArray);
            let position = column!(b, "position", Int32Array);
            let attr = column!(b, "attribute", StringArray);
            let positive = column!(b, "positive", BooleanArray);
            let value = column!(b, "value", Float64Array);
            let rank8 = column!(b, "value_rank8", Float64Array);
            let quality = column!(b, "quality", Float64Array);
            let mut rows: Vec<usize> = (0..b.num_rows()).collect();
            rows.sort_by_key(|&i| position.value(i));
            for i in rows {
                let a = Attr {
                    attr: self.intern_attr(attr.value(i)),
                    positive: positive.value(i),
                    value: value.value(i),
                    value_rank8: opt(rank8, i, Float64Array::value),
                    quality: opt(quality, i, Float64Array::value),
                };
                attrs
                    .entry((id.value(i).to_owned(), scraped.value(i)))
                    .or_default()
                    .push(a);
            }
        }

        for b in read_batches(auctions_path)? {
            let b = b?;
            let scraped = column!(b, "scraped_at", TimestampMillisecondArray);
            let id = column!(b, "auction_id", StringArray);
            let weapon_slug = column!(b, "weapon_slug", StringArray);
            let weapon_name = column!(b, "weapon_name", StringArray);
            let weapon_class = column!(b, "weapon_class", StringArray);
            let riven_name = column!(b, "riven_name", StringArray);
            let polarity = column!(b, "polarity", StringArray);
            let mastery = column!(b, "mastery_level", Int64Array);
            let re_rolls = column!(b, "re_rolls", Int64Array);
            let rank = column!(b, "rank", Int32Array);
            let legacy = column!(b, "legacy_stats", BooleanArray);
            let fit_ok = column!(b, "fit_ok", BooleanArray);
            let mean_q = column!(b, "mean_positive_quality", Float64Array);
            let buyout = column!(b, "buyout_price", Int64Array);
            let starting = column!(b, "starting_price", Int64Array);
            let top_bid = column!(b, "top_bid", Int64Array);
            let direct = column!(b, "is_direct_sell", BooleanArray);
            let visible = column!(b, "visible", BooleanArray);
            let closed = column!(b, "closed", BooleanArray);
            let created = column!(b, "created", TimestampMillisecondArray);
            let owner_name = column!(b, "owner_name", StringArray);
            let owner_rep = column!(b, "owner_reputation", Int64Array);
            // Older parts predate riven ids; fall back to one riven per auction.
            let riven_ids = b
                .column_by_name("riven_id")
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            for i in 0..b.num_rows() {
                if !visible.value(i) || closed.value(i) {
                    continue;
                }
                let t = scraped.value(i);
                let class = if weapon_class.is_valid(i) {
                    weapon_class.value(i)
                } else {
                    "unknown"
                };
                let weapon = self.intern_weapon(weapon_slug.value(i), weapon_name.value(i), class);
                let w = &mut self.weapons[weapon as usize];
                w.last_scrape = w.last_scrape.max(t);

                let auction_id = id.value(i);
                let riven_key = riven_ids
                    .filter(|c| c.is_valid(i))
                    .map_or(auction_id, |c| c.value(i));
                let riven = self.intern_riven(riven_key);
                let buyout_price = opt(buyout, i, Int64Array::value);
                let starting_price = opt(starting, i, Int64Array::value);
                let top = opt(top_bid, i, Int64Array::value);
                let fresh = Auction {
                    id: auction_id.to_owned(),
                    weapon,
                    riven,
                    gone: None,
                    riven_name: riven_name.value(i).to_owned(),
                    polarity: polarity.value(i).to_owned(),
                    mastery_level: opt(mastery, i, Int64Array::value),
                    re_rolls: opt(re_rolls, i, Int64Array::value),
                    rank: opt(rank, i, Int32Array::value),
                    legacy_stats: opt(legacy, i, BooleanArray::value),
                    fit_ok: fit_ok.value(i),
                    mean_quality: opt(mean_q, i, Float64Array::value),
                    owner_name: owner_name.value(i).to_owned(),
                    owner_reputation: opt(owner_rep, i, Int64Array::value),
                    created: created.value(i),
                    first_seen: t,
                    last_seen: t,
                    attrs: attrs
                        .remove(&(auction_id.to_owned(), t))
                        .unwrap_or_default(),
                    buyout_price,
                    starting_price,
                    top_bid: top,
                    direct_sell: direct.value(i),
                };

                let index = match self.auction_index.get(auction_id) {
                    Some(&index) => {
                        let existing = &mut self.auctions[index as usize];
                        if t >= existing.last_seen {
                            let first_seen = existing.first_seen.min(t);
                            let old_riven = existing.riven;
                            *existing = fresh;
                            existing.first_seen = first_seen;
                            if old_riven != riven {
                                self.riven_auctions[old_riven as usize].retain(|&a| a != index);
                                self.riven_auctions[riven as usize].push(index);
                            }
                        } else {
                            existing.first_seen = existing.first_seen.min(t);
                        }
                        index
                    }
                    None => {
                        let index = self.auctions.len() as u32;
                        self.auctions.push(fresh);
                        self.auction_index.insert(auction_id.to_owned(), index);
                        self.riven_auctions[riven as usize].push(index);
                        index
                    }
                };
                self.observations.push(Observation {
                    t,
                    auction: index,
                    price: price_of(buyout_price, top, starting_price),
                    direct_sell: direct.value(i),
                });
            }
        }
        Ok(())
    }

    fn load_gone(&mut self, path: &Path) -> LoadResult<()> {
        for b in read_batches(path)? {
            let b = b?;
            let detected = column!(b, "detected_at", TimestampMillisecondArray);
            let id = column!(b, "auction_id", StringArray);
            let reason = column!(b, "reason", StringArray);
            let price = column!(b, "last_price", Int64Array);
            let direct = column!(b, "direct_sell", BooleanArray);
            for i in 0..b.num_rows() {
                let Some(&auction) = self.auction_index.get(id.value(i)) else {
                    continue;
                };
                let t = detected.value(i);
                let listing = &mut self.auctions[auction as usize];
                if listing
                    .gone
                    .as_ref()
                    .is_none_or(|(previous, _)| t >= *previous)
                {
                    listing.gone = Some((t, reason.value(i).to_owned()));
                }
                self.gone.push(GoneEvent {
                    t,
                    auction,
                    price: opt(price, i, Int64Array::value),
                    direct_sell: direct.value(i),
                });
            }
        }
        Ok(())
    }

    fn finish_load(&mut self) {
        self.observations.sort_by_key(|o| o.t);
        self.gone.sort_by_key(|g| g.t);

        let mut prices: Vec<Vec<i64>> = vec![Vec::new(); self.weapons.len()];
        for a in &self.auctions {
            prices[a.weapon as usize].extend(auction_price(a));
        }
        for (weapon, mut prices) in self.weapons.iter_mut().zip(prices) {
            weapon.price_limit = price_limit(&mut prices);
        }
    }

    fn price_outlier(&self, weapon: u16, price: Option<i64>) -> bool {
        match (self.weapons[weapon as usize].price_limit, price) {
            (Some(limit), Some(price)) => price > limit,
            _ => false,
        }
    }

    /// A gone event still holds if the auction wasn't seen again afterwards.
    fn gone_valid(&self, g: &GoneEvent) -> bool {
        self.auctions[g.auction as usize].last_seen <= g.t
    }

    fn is_current(&self, a: &Auction) -> bool {
        // Absence from capped pages is unresolved, not evidence of removal.
        a.gone.as_ref().is_none_or(|(t, _)| a.last_seen > *t)
    }

    fn thumb(&self, slug: &str) -> Option<String> {
        let index = *self.weapon_index.get(slug)?;
        self.weapons[index as usize].thumb.clone()
    }

    pub fn live(&self) -> (u32, u64) {
        (self.auctions.len() as u32, self.version)
    }

    pub fn overview(&self) -> Overview {
        let mut listings = vec![0u32; self.weapons.len()];
        // Sane current prices per weapon, for the median/cheapest shown in the weapon list.
        let mut prices: Vec<Vec<i64>> = vec![Vec::new(); self.weapons.len()];
        let mut classes: Vec<HashSet<&str>> = vec![HashSet::new(); self.attrs.len()];
        for a in &self.auctions {
            let class = self.weapons[a.weapon as usize].class.as_str();
            for attr in &a.attrs {
                classes[attr.attr as usize].insert(class);
            }
            if !self.is_current(a) {
                continue;
            }
            listings[a.weapon as usize] += 1;
            let price = price_of(a.buyout_price, a.top_bid, a.starting_price);
            if let Some(p) = price.filter(|p| *p >= 10)
                && !self.price_outlier(a.weapon, price)
                && a.direct_sell
                && a.fit_ok
                && a.legacy_stats != Some(true)
            {
                prices[a.weapon as usize].push(p);
            }
        }
        let week_ago = now_ms() - WEEK_MS;
        let mut gone_prices: Vec<Vec<i64>> = vec![Vec::new(); self.weapons.len()];
        for g in self
            .gone
            .iter()
            .filter(|g| g.t >= week_ago && self.gone_valid(g))
        {
            let a = &self.auctions[g.auction as usize];
            if let Some(p) = g.price.filter(|p| *p >= 10)
                && !self.price_outlier(a.weapon, g.price)
                && g.direct_sell
                && a.fit_ok
                && a.legacy_stats != Some(true)
            {
                gone_prices[a.weapon as usize].push(p);
            }
        }
        let mut weapons: Vec<WeaponInfo> = self
            .weapons
            .iter()
            .zip(listings)
            .zip(prices)
            .zip(gone_prices)
            .map(|(((w, listings), mut prices), gone)| {
                prices.sort_unstable();
                WeaponInfo {
                    gone_7d: gone.len() as u32,
                    gone_median_7d: median(gone),
                    slug: w.slug.clone(),
                    name: w.name.clone(),
                    class: w.class.clone(),
                    group: w.group.clone(),
                    has_image: w.thumb.is_some(),
                    listings,
                    median_price: prices.get(prices.len() / 2).copied(),
                    min_price: prices.first().copied(),
                    last_scrape: w.last_scrape,
                }
            })
            .collect();
        weapons.sort_by(|a, b| a.name.cmp(&b.name));
        let mut attributes: Vec<AttrInfo> = self
            .attrs
            .iter()
            .zip(classes)
            .map(|(slug, classes)| {
                let mut classes: Vec<String> = classes.into_iter().map(str::to_owned).collect();
                classes.sort();
                AttrInfo {
                    slug: slug.clone(),
                    name: crate::api::attribute_name(slug),
                    classes,
                }
            })
            .collect();
        attributes.sort_by(|a, b| a.name.cmp(&b.name));
        Overview {
            weapons,
            attributes,
            auctions: self.auctions.len() as u32,
            observations: self.observations.len() as u32,
            last_scrape: self.weapons.iter().map(|w| w.last_scrape).max(),
        }
    }

    fn matches(&self, a: &Auction, q: &Query, weapon: Option<u16>) -> bool {
        if let Some(prefix) = &q.riven_id {
            return self.rivens[a.riven as usize].starts_with(prefix.as_str());
        }
        if weapon.is_some_and(|w| w != a.weapon) {
            return false;
        }
        if !q.include_unfit && !a.fit_ok {
            return false;
        }
        if !q.include_legacy && a.legacy_stats == Some(true) {
            return false;
        }
        if q.min_quality > 0.0 && a.mean_quality.is_none_or(|m| m < q.min_quality) {
            return false;
        }
        let has = |slug: &str, positive: bool| {
            self.attr_index.get(slug).is_some_and(|&id| {
                a.attrs
                    .iter()
                    .any(|x| x.attr == id && x.positive == positive)
            })
        };
        if !q.positives.iter().all(|p| has(p, true)) {
            return false;
        }
        let has_curse = a.attrs.iter().any(|x| !x.positive);
        match &q.curse {
            CurseFilter::Any => true,
            CurseFilter::None => !has_curse,
            CurseFilter::Required => has_curse,
            CurseFilter::Attr(slug) => has(slug, false),
        }
    }

    fn price_ok(q: &Query, price: Option<i64>, direct_sell: bool) -> bool {
        let Some(price) = price else {
            return false;
        };
        (!q.direct_only || direct_sell)
            && q.min_price.is_none_or(|m| price >= m)
            && q.max_price.is_none_or(|m| price <= m)
    }

    fn listing(&self, a: &Auction) -> Listing {
        let w = &self.weapons[a.weapon as usize];
        let copies = &self.riven_auctions[a.riven as usize];
        let sellers: HashSet<&str> = copies
            .iter()
            .map(|&i| self.auctions[i as usize].owner_name.as_str())
            .collect();
        Listing {
            auction_id: a.id.clone(),
            riven_id: self.rivens[a.riven as usize].clone(),
            price_outlier: self.price_outlier(a.weapon, auction_price(a)),
            price_limit: w.price_limit,
            copies: copies.len() as u32,
            sellers: sellers.len() as u32,
            gone_at: a
                .gone
                .as_ref()
                .filter(|(t, _)| *t >= a.last_seen)
                .map(|(t, _)| *t),
            gone_reason: a
                .gone
                .as_ref()
                .filter(|(t, _)| *t >= a.last_seen)
                .map(|(_, r)| r.clone()),
            last_seen: a.last_seen,
            unconfirmed: a.last_seen < w.last_scrape,
            weapon_slug: w.slug.clone(),
            weapon_name: w.name.clone(),
            riven_name: a.riven_name.clone(),
            price: price_of(a.buyout_price, a.top_bid, a.starting_price),
            buyout_price: a.buyout_price,
            starting_price: a.starting_price,
            top_bid: a.top_bid,
            direct_sell: a.direct_sell,
            rank: a.rank,
            re_rolls: a.re_rolls,
            mastery_level: a.mastery_level,
            polarity: a.polarity.clone(),
            legacy_stats: a.legacy_stats,
            fit_ok: a.fit_ok,
            mean_quality: a.mean_quality,
            owner_name: a.owner_name.clone(),
            owner_reputation: a.owner_reputation,
            created: a.created,
            first_seen: a.first_seen,
            attributes: a
                .attrs
                .iter()
                .map(|x| AttrView {
                    slug: self.attrs[x.attr as usize].clone(),
                    positive: x.positive,
                    value: x.value,
                    value_rank8: x.value_rank8,
                    quality: x.quality,
                })
                .collect(),
        }
    }

    pub fn search(&self, q: &Query) -> SearchResult {
        // An id search shows everything about that riven, whatever the other filters say.
        let id_query;
        let q = match &q.riven_id {
            Some(prefix) => {
                id_query = Query {
                    riven_id: Some(prefix.trim().to_ascii_lowercase()),
                    weapon: None,
                    direct_only: false,
                    min_price: None,
                    max_price: None,
                    ..q.clone()
                };
                &id_query
            }
            None => q,
        };
        let weapon = match &q.weapon {
            Some(slug) => match self.weapon_index.get(slug) {
                Some(&w) => Some(w),
                None => {
                    return SearchResult {
                        listings: Vec::new(),
                        total: 0,
                        gone: Vec::new(),
                        gone_total: 0,
                        outliers_hidden: 0,
                        price_limit: None,
                        history: Vec::new(),
                        server_time: now_ms(),
                    };
                }
            },
            None => None,
        };
        let matching: Vec<bool> = self
            .auctions
            .iter()
            .map(|a| self.matches(a, q, weapon))
            .collect();

        // Current listings, one per riven: the cheapest copy.
        let price = |a: &Auction| auction_price(a).unwrap_or(0);
        let mut best: HashMap<u32, &Auction> = HashMap::new();
        for (a, m) in self.auctions.iter().zip(&matching) {
            if !*m || !self.is_current(a) || !Self::price_ok(q, auction_price(a), a.direct_sell) {
                continue;
            }
            best.entry(a.riven)
                .and_modify(|b| {
                    if price(a) < price(b) {
                        *b = a;
                    }
                })
                .or_insert(a);
        }
        // Invalid prices are hidden unless asked for. An id search shows everything, and the
        // charts below only count them for that one riven: they'd swamp any analysis.
        let id_search = q.riven_id.is_some();
        let show_outliers = q.include_outliers || id_search;
        let (mut current, hidden): (Vec<&Auction>, Vec<&Auction>) = best
            .into_values()
            .partition(|a| show_outliers || !self.price_outlier(a.weapon, auction_price(a)));
        let outliers_hidden = hidden.len();
        match q.sort {
            Sort::PriceAsc => current.sort_by_key(|a| (price(a), a.id.as_str())),
            Sort::PriceDesc => {
                current.sort_by_key(|a| (std::cmp::Reverse(price(a)), a.id.as_str()))
            }
            Sort::QualityDesc => current.sort_by(|a, b| {
                b.mean_quality
                    .unwrap_or(-1.0)
                    .total_cmp(&a.mean_quality.unwrap_or(-1.0))
                    .then(a.id.cmp(&b.id))
            }),
            Sort::Newest => current.sort_by_key(|a| (std::cmp::Reverse(a.created), a.id.as_str())),
        }
        let total = current.len();
        let listings = current
            .into_iter()
            .take(MAX_LISTINGS)
            .map(|a| self.listing(a))
            .collect();

        // Disappeared listings (≈ sold), newest first, one per riven.
        let gone_events: Vec<&GoneEvent> = self
            .gone
            .iter()
            .filter(|g| {
                matching[g.auction as usize]
                    && self.gone_valid(g)
                    && Self::price_ok(q, g.price, g.direct_sell)
            })
            .collect();
        let gone_outlier =
            |g: &GoneEvent| self.price_outlier(self.auctions[g.auction as usize].weapon, g.price);
        let mut seen_rivens = HashSet::new();
        let recent_gone: Vec<&GoneEvent> = gone_events
            .iter()
            .rev()
            .filter(|g| show_outliers || !gone_outlier(g))
            .filter(|g| seen_rivens.insert(self.auctions[g.auction as usize].riven))
            .copied()
            .collect();
        let gone_total = recent_gone.len();
        let gone = recent_gone
            .into_iter()
            .take(MAX_GONE)
            .map(|g| self.listing(&self.auctions[g.auction as usize]))
            .collect();

        // Price history: per bucket, the latest price of every matching riven (cheapest copy),
        // plus the last prices of rivens that disappeared in that bucket.
        let bucket = i64::from(q.bucket_hours.max(1)) * 3_600_000;
        let bucket_of = |t: i64| t - t.rem_euclid(bucket);
        let mut gone_by_bucket: HashMap<i64, HashMap<u32, i64>> = HashMap::new();
        for g in gone_events.iter().filter(|g| id_search || !gone_outlier(g)) {
            if let Some(p) = g.price {
                gone_by_bucket
                    .entry(bucket_of(g.t))
                    .or_default()
                    .insert(self.auctions[g.auction as usize].riven, p);
            }
        }
        let mut history = Vec::new();
        let mut current_bucket = None;
        // Keep every copy until the bucket closes; its latest observation can rise or
        // cease to match the price filter. Only then choose the cheapest per riven.
        let mut prices: HashMap<u32, Option<i64>> = HashMap::new();
        let mut flush = |t: i64, prices: &mut HashMap<u32, Option<i64>>| {
            if prices.is_empty() {
                return;
            }
            let mut cheapest: HashMap<u32, i64> = HashMap::new();
            for (auction, price) in prices.drain() {
                if let Some(price) = price {
                    cheapest
                        .entry(self.auctions[auction as usize].riven)
                        .and_modify(|p| *p = (*p).min(price))
                        .or_insert(price);
                }
            }
            let mut v: Vec<i64> = cheapest.into_values().collect();
            if v.is_empty() {
                return;
            }
            v.sort_unstable();
            let pct = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
            let gone: Vec<i64> = gone_by_bucket
                .remove(&t)
                .map(|g| g.into_values().collect())
                .unwrap_or_default();
            history.push(HistoryPoint {
                t,
                count: v.len() as u32,
                min: v[0],
                p25: pct(0.25),
                median: pct(0.5),
                p75: pct(0.75),
                gone_count: gone.len() as u32,
                gone_median: median(gone),
            });
        };
        for o in &self.observations {
            if !matching[o.auction as usize] {
                continue;
            }
            let b = bucket_of(o.t);
            if current_bucket != Some(b) {
                if let Some(prev) = current_bucket {
                    flush(prev, &mut prices);
                }
                current_bucket = Some(b);
            }
            let eligible = Self::price_ok(q, o.price, o.direct_sell)
                && (id_search
                    || !self.price_outlier(self.auctions[o.auction as usize].weapon, o.price));
            prices.insert(o.auction, o.price.filter(|_| eligible));
        }
        if let Some(prev) = current_bucket {
            flush(prev, &mut prices);
        }

        SearchResult {
            listings,
            total,
            gone,
            gone_total,
            outliers_hidden,
            price_limit: weapon.and_then(|w| self.weapons[w as usize].price_limit),
            history,
            server_time: now_ms(),
        }
    }

    /// Which stats raise or lower the asking price of this weapon's rivens. Positive and negative
    /// versions of a stat count separately. Uses every listing seen rather than the latest scrape,
    /// so that the analysis has as much of the price range as possible.
    pub fn stat_effects(&self, weapon: &str) -> StatEffects {
        let Some(&weapon) = self.weapon_index.get(weapon) else {
            return StatEffects::default();
        };
        // Verified buyouts only, one per riven: the cheapest copy, ordered by the riven's hash so
        // the held-out rivens are an arbitrary but repeatable selection.
        let mut cheapest: HashMap<u32, (&Auction, i64)> = HashMap::new();
        for a in self.auctions.iter().filter(|a| a.weapon == weapon) {
            let Some(price) = a.buyout_price.filter(|p| *p >= 10) else {
                continue;
            };
            if !a.direct_sell
                || !a.fit_ok
                || a.legacy_stats == Some(true)
                || self.price_outlier(weapon, Some(price))
            {
                continue;
            }
            let entry = cheapest.entry(a.riven).or_insert((a, price));
            if price < entry.1 {
                *entry = (a, price);
            }
        }
        let mut rivens: Vec<(u32, &Auction, i64)> =
            cheapest.into_iter().map(|(r, (a, p))| (r, a, p)).collect();
        rivens.sort_by(|a, b| self.rivens[a.0 as usize].cmp(&self.rivens[b.0 as usize]));

        let mut result = StatEffects {
            rivens: rivens.len() as u32,
            low_price: rivens.iter().map(|r| r.2).min(),
            high_price: rivens.iter().map(|r| r.2).max(),
            ..StatEffects::default()
        };
        if rivens.len() < EFFECT_MIN_RIVENS {
            return result;
        }

        let mut counts: HashMap<(u16, bool), u32> = HashMap::new();
        for (_, a, _) in &rivens {
            for attr in &a.attrs {
                *counts.entry((attr.attr, attr.positive)).or_default() += 1;
            }
        }
        result.rare_stats = counts
            .values()
            .filter(|c| **c < EFFECT_MIN_STAT_RIVENS)
            .count() as u32;
        let mut stats: Vec<(u16, bool)> = counts
            .iter()
            .filter(|(_, c)| **c >= EFFECT_MIN_STAT_RIVENS)
            .map(|(s, _)| *s)
            .collect();
        stats.sort_unstable();
        if stats.is_empty() {
            return result;
        }

        let x: Vec<Vec<f64>> = rivens
            .iter()
            .map(|(_, a, _)| {
                stats
                    .iter()
                    .map(|(attr, positive)| {
                        let has = a
                            .attrs
                            .iter()
                            .any(|s| s.attr == *attr && s.positive == *positive);
                        f64::from(u8::from(has))
                    })
                    .collect()
            })
            .collect();
        let y: Vec<f64> = rivens.iter().map(|r| (r.2 as f64).ln()).collect();

        result.explained = effects::holdout_r2(&x, &y);
        if result.explained.is_none_or(|r| r < EFFECT_MIN_EXPLAINED) {
            return result;
        }
        let Some(model) = effects::fit(&x, &y) else {
            return result;
        };
        for (i, (attr, positive)) in stats.iter().enumerate() {
            let (b, se) = (model.coefficients[i], model.standard_errors[i]);
            let effect = StatEffect {
                slug: self.attrs[*attr as usize].clone(),
                effect: b.exp_m1(),
                low: (b - INTERVAL * se).exp_m1(),
                high: (b + INTERVAL * se).exp_m1(),
                rivens: counts[&(*attr, *positive)],
                certain: model.identifiable[i] && b.abs() > INTERVAL * se,
            };
            if *positive {
                result.positive.push(effect);
            } else {
                result.negative.push(effect);
            }
        }
        for group in [&mut result.positive, &mut result.negative] {
            group.sort_by(|a, b| b.effect.total_cmp(&a.effect));
        }
        result
    }

    /// Every auction this riven appeared in, oldest first, with its price history.
    pub fn riven_history(&self, riven_id: &str) -> Vec<RivenAuction> {
        let Some(&riven) = self.riven_index.get(riven_id) else {
            return Vec::new();
        };
        let auctions = &self.riven_auctions[riven as usize];
        let mut points: HashMap<u32, Vec<PricePoint>> = HashMap::new();
        for o in self
            .observations
            .iter()
            .filter(|o| auctions.contains(&o.auction))
        {
            points.entry(o.auction).or_default().push(PricePoint {
                t: o.t,
                price: o.price,
            });
        }
        let mut out: Vec<RivenAuction> = auctions
            .iter()
            .map(|&i| {
                let a = &self.auctions[i as usize];
                RivenAuction {
                    auction_id: a.id.clone(),
                    owner_name: a.owner_name.clone(),
                    created: a.created,
                    first_seen: a.first_seen,
                    last_seen: a.last_seen,
                    gone_at: a.gone.as_ref().map(|(t, _)| *t),
                    gone_reason: a.gone.as_ref().map(|(_, r)| r.clone()),
                    points: points.remove(&i).unwrap_or_default(),
                }
            })
            .collect();
        out.sort_by_key(|a| a.first_seen);
        out
    }
}
