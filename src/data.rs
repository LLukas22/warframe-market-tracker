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
    RivenAuction, RivenDetail, RivenScan, SearchResult, SimilarRiven, Sort, StatEffect,
    StatEffects, WeaponInfo,
};
use crate::effects;
use wfmt_scraper::identity::{IdAttr, IdInput, Registry};
use wfmt_scraper::riven::{self, AttrInput, RivenInput, WeaponClass};

const RESCAN_EVERY: Duration = Duration::from_secs(5);
const LISTING_PAGE_SIZE: usize = 50;
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
                    weapon.is_bow =
                        w.riven_type != "melee" && w.game_ref.to_ascii_lowercase().contains("bow");
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
    #[serde(default)]
    game_ref: String,
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
    /// Bows show fire rate doubled, which the stat model needs to know.
    is_bow: bool,
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

/// Stat overlap dominates roll closeness; prices and raw percentages never affect rank.
#[derive(Debug)]
struct Similarity {
    shared: usize,
    missing: Vec<u16>,
    added: Vec<u16>,
    same_negative: bool,
    roll_distance: Option<f64>,
}

fn similarity(a: &Auction, b: &Auction) -> Option<Similarity> {
    let stats = |a: &Auction, positive| {
        a.attrs
            .iter()
            .filter(|x| x.positive == positive)
            .map(|x| x.attr)
            .collect::<HashSet<_>>()
    };
    let positives = stats(a, true);
    let other_positives = stats(b, true);
    let shared = positives.intersection(&other_positives).count();
    if shared == 0 {
        return None;
    }
    let mut missing: Vec<_> = positives.difference(&other_positives).copied().collect();
    let mut added: Vec<_> = other_positives.difference(&positives).copied().collect();
    missing.sort_unstable();
    added.sort_unstable();
    let same_negative = stats(a, false) == stats(b, false);
    let roll_distance = if a.fit_ok && b.fit_ok {
        a.attrs
            .iter()
            .filter_map(|x| {
                b.attrs
                    .iter()
                    .find(|y| y.attr == x.attr && y.positive == x.positive)
                    .map(|y| {
                        x.quality
                            .zip(y.quality)
                            .filter(|(x, y)| x.is_finite() && y.is_finite())
                            .map(|(x, y)| (x - y).abs())
                    })
            })
            .collect::<Option<Vec<_>>>()
            .filter(|values| !values.is_empty())
            .map(|values| values.iter().sum::<f64>() / values.len() as f64)
    } else {
        None
    };
    Some(Similarity {
        shared,
        missing,
        added,
        same_negative,
        roll_distance,
    })
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
    use crate::api::{DAY_MS, Period};
    use std::io::Read;

    /// The fixtures are dated around 1970, outside the default period.
    fn all_time() -> Query {
        Query {
            period: Period::All,
            ..Query::default()
        }
    }
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

    fn comparison_auction(
        inner: &mut Inner,
        weapon: u16,
        id: &str,
        quality: Option<f64>,
        negative: Option<u16>,
        time: i64,
    ) -> u32 {
        let index = add_auction(inner, weapon, id, 100, time);
        let critical = inner.intern_attr("critical_chance");
        let damage = inner.intern_attr("critical_damage");
        inner.auctions[index as usize].attrs = [(critical, true), (damage, true)]
            .into_iter()
            .chain(negative.map(|n| (n, false)))
            .map(|(attr, positive)| Attr {
                attr,
                positive,
                value: 100.0,
                value_rank8: Some(100.0),
                quality,
            })
            .collect();
        index
    }

    #[test]
    fn comparisons_rank_stats_then_normalized_rolls_and_ignore_price_and_rank() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        let zoom = inner.intern_attr("zoom");
        let recoil = inner.intern_attr("recoil");
        let source = comparison_auction(&mut inner, weapon, "source", Some(0.5), Some(zoom), 1000);
        comparison_auction(
            &mut inner,
            weapon,
            "different-negative",
            Some(0.5),
            Some(recoil),
            5000,
        );
        comparison_auction(&mut inner, weapon, "far", Some(0.9), Some(zoom), 4000);
        let near = comparison_auction(&mut inner, weapon, "near", Some(0.51), Some(zoom), 2000);
        inner.auctions[near as usize].rank = Some(0);
        inner.auctions[near as usize].attrs[0].value = 10.0;
        inner.auctions[near as usize].buyout_price = Some(999999);
        comparison_auction(&mut inner, weapon, "unknown", None, Some(zoom), 6000);
        comparison_auction(&mut inner, weapon, "no-negative", Some(0.5), None, 7000);
        let incompatible = comparison_auction(
            &mut inner,
            weapon,
            "other-positives",
            Some(0.5),
            Some(zoom),
            1000,
        );
        inner.auctions[incompatible as usize].attrs.remove(0);
        let detail = inner
            .riven_detail("source", None, false, false, false)
            .unwrap();
        let ids: Vec<_> = detail
            .similar
            .iter()
            .map(|m| m.listing.auction_id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "near",
                "far",
                "unknown",
                "different-negative",
                "no-negative"
            ]
        );
        assert!(detail.similar[0].roll_distance.unwrap() < 0.011);
        assert_eq!(detail.similar[2].roll_distance, None);
        inner.auctions[source as usize].fit_ok = false;
        assert!(
            inner
                .riven_detail("source", None, false, false, false)
                .unwrap()
                .similar
                .iter()
                .all(|m| m.roll_distance.is_none())
        );
    }

    #[test]
    fn partial_matches_rank_overlap_before_curse_quality_and_recency() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        let zoom = inner.intern_attr("zoom");
        let recoil = inner.intern_attr("recoil");
        let toxin = inner.intern_attr("toxin_damage");
        let heat = inner.intern_attr("heat_damage");
        let extra = |attr, quality| Attr {
            attr,
            positive: true,
            value: 100.0,
            value_rank8: Some(100.0),
            quality,
        };
        let source = comparison_auction(&mut inner, weapon, "source", Some(0.5), Some(zoom), 1000);
        inner.auctions[source as usize]
            .attrs
            .push(extra(toxin, Some(0.5)));
        let exact = comparison_auction(&mut inner, weapon, "exact", Some(0.9), Some(zoom), 1000);
        inner.auctions[exact as usize]
            .attrs
            .push(extra(toxin, Some(0.9)));
        let curse = comparison_auction(
            &mut inner,
            weapon,
            "different-curse",
            Some(0.5),
            Some(recoil),
            1000,
        );
        inner.auctions[curse as usize]
            .attrs
            .push(extra(toxin, Some(0.5)));
        comparison_auction(&mut inner, weapon, "subset", None, Some(recoil), 1000);
        let swap = comparison_auction(&mut inner, weapon, "swap", Some(0.5), Some(zoom), 5000);
        inner.auctions[swap as usize]
            .attrs
            .push(extra(heat, Some(0.5)));
        let loose = comparison_auction(&mut inner, weapon, "loose", Some(0.5), Some(zoom), 6000);
        inner.auctions[loose as usize].attrs[0].attr = heat;
        let unrelated =
            comparison_auction(&mut inner, weapon, "unrelated", Some(0.5), Some(zoom), 6000);
        // Negative critical chance is not a matching positive critical chance.
        inner.auctions[unrelated as usize].attrs[0].positive = false;
        inner.auctions[unrelated as usize].attrs[1].attr = heat;
        let normal = inner
            .riven_detail("source", None, false, false, false)
            .unwrap();
        let ids = |result: &RivenDetail| {
            result
                .similar
                .iter()
                .map(|m| m.listing.auction_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&normal), ["exact", "different-curse", "subset", "swap"]);
        let subset = &normal.similar[2];
        assert_eq!(subset.shared_positives, 2);
        assert_eq!(subset.missing_positives, ["toxin_damage"]);
        assert!(subset.added_positives.is_empty());
        assert_eq!(subset.roll_distance, None);
        assert_eq!(normal.similar[3].added_positives, ["heat_damage"]);
        assert_eq!(normal.similar[3].missing_positives, ["toxin_damage"]);
        let broad = inner
            .riven_detail("source", None, false, false, true)
            .unwrap();
        assert_eq!(
            ids(&broad),
            ["exact", "different-curse", "subset", "swap", "loose"]
        );
        assert_eq!(broad.similar.last().unwrap().shared_positives, 1);
        // The reverse comparison also admits supersets, even with unavailable quality.
        let reverse = inner
            .riven_detail("subset", None, false, false, false)
            .unwrap();
        let superset = reverse
            .similar
            .iter()
            .find(|m| m.listing.auction_id == "source")
            .unwrap();
        assert!(superset.missing_positives.is_empty());
        assert_eq!(superset.added_positives, ["toxin_damage"]);
        assert_eq!(superset.shared_positives, 2);
    }

    #[test]
    fn comparisons_include_gone_deduplicate_relists_and_validate_selected_auction() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        let other_weapon = inner.intern_weapon("other", "Other", "rifle");
        comparison_auction(&mut inner, weapon, "source", Some(0.5), None, 1000);
        let old = comparison_auction(&mut inner, weapon, "old", Some(0.5), None, 1000);
        let new = comparison_auction(&mut inner, weapon, "new", Some(0.6), None, 2000);
        let riven = inner.auctions[old as usize].riven;
        let old_new_riven = inner.auctions[new as usize].riven;
        inner.riven_auctions[old_new_riven as usize].clear();
        inner.auctions[new as usize].riven = riven;
        inner.riven_auctions[riven as usize].push(new);
        inner.auctions[new as usize].gone = Some((3000, "missing".into()));
        comparison_auction(
            &mut inner,
            other_weapon,
            "wrong-weapon",
            Some(0.5),
            None,
            1000,
        );
        let legacy = comparison_auction(&mut inner, weapon, "legacy", Some(0.5), None, 1000);
        inner.auctions[legacy as usize].legacy_stats = Some(true);
        let unfit = comparison_auction(&mut inner, weapon, "unfit", Some(0.5), None, 1000);
        inner.auctions[unfit as usize].fit_ok = false;
        let result = inner
            .riven_detail("source", None, false, false, false)
            .unwrap();
        assert_eq!(result.similar_total, 1);
        assert_eq!(result.comparisons_hidden, 2);
        assert_eq!(result.similar[0].listing.auction_id, "new");
        assert_eq!(result.similar[0].listing.gone_at, Some(3000));
        assert_eq!(result.similar[0].listing.copies, 2);
        assert_eq!(
            inner
                .riven_detail("source", None, true, true, false)
                .unwrap()
                .similar_total,
            3
        );
        assert!(
            inner
                .riven_detail("missing", None, false, false, false)
                .is_none()
        );
        assert!(
            inner
                .riven_detail("source", Some("old"), false, false, false)
                .is_none()
        );
        assert!(
            inner
                .riven_detail("source", Some("missing"), false, false, false)
                .is_none()
        );
        assert_eq!(
            inner
                .riven_detail("old", None, false, false, false)
                .unwrap()
                .listing
                .auction_id,
            "new"
        );
        assert_eq!(
            inner
                .riven_detail("old", Some("old"), false, false, false)
                .unwrap()
                .listing
                .auction_id,
            "old"
        );
        assert!(
            inner
                .riven_detail("old", None, true, true, false)
                .unwrap()
                .similar
                .iter()
                .all(|m| m.listing.riven_id != "old")
        );
    }

    #[test]
    fn riven_history_orders_observations_and_clears_stale_removal_status() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        let index = add_auction(&mut inner, weapon, "riven", 200, 3000);
        inner.observations.push(Observation {
            t: 1000,
            auction: index,
            price: Some(100),
            direct_sell: true,
        });
        inner.auctions[index as usize].gone = Some((2000, "missing".into()));
        let history = inner.riven_history("riven");
        assert_eq!(
            history[0].points.iter().map(|p| p.t).collect::<Vec<_>>(),
            [1000, 3000]
        );
        assert_eq!(history[0].gone_at, None);
        assert_eq!(history[0].gone_reason, None);
    }

    #[test]
    fn comparisons_are_bounded_with_deterministic_recency_ties() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        comparison_auction(&mut inner, weapon, "source", Some(0.5), None, 1000);
        assert!(
            inner
                .riven_detail("source", None, false, false, false)
                .unwrap()
                .similar
                .is_empty()
        );
        for i in 0..25 {
            comparison_auction(
                &mut inner,
                weapon,
                &format!("match-{i:02}"),
                Some(0.5),
                None,
                2000 + i,
            );
        }
        let result = inner
            .riven_detail("source", None, false, false, false)
            .unwrap();
        assert_eq!(result.similar_total, 25);
        assert_eq!(result.similar.len(), 10);
        let broad = inner
            .riven_detail("source", None, false, false, true)
            .unwrap();
        assert_eq!(broad.similar.len(), 10);
        assert_eq!(broad.similar_total, 25);
        assert_eq!(result.similar[0].listing.auction_id, "match-24");
        inner.auctions.last_mut().unwrap().last_seen = 2023;
        let result = inner
            .riven_detail("source", None, false, false, false)
            .unwrap();
        assert_eq!(result.similar[0].listing.auction_id, "match-23");
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
    fn history_filters_combine_and_paginate_active_and_gone_rivens() {
        fn pair(
            inner: &mut Inner,
            weapon: u16,
            id: &str,
            old_price: i64,
            new_price: i64,
            seller: &str,
        ) {
            let old = add_auction(inner, weapon, &format!("{id}-old"), old_price, 1000);
            let new = add_auction(inner, weapon, &format!("{id}-new"), new_price, 3000);
            let old_riven = inner.auctions[old as usize].riven;
            let new_riven = inner.auctions[new as usize].riven;
            inner.riven_auctions[new_riven as usize].clear();
            inner.auctions[new as usize].riven = old_riven;
            inner.riven_auctions[old_riven as usize].push(new);
            inner.auctions[old as usize].owner_name = "Alice".into();
            inner.auctions[new as usize].owner_name = seller.into();
            inner.auctions[old as usize].gone = Some((2000, "missing".into()));
            inner.gone.push(GoneEvent {
                t: 2000,
                auction: old,
                price: Some(old_price),
                direct_sell: true,
            });
        }
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        pair(&mut inner, weapon, "price", 100, 200, "Alice");
        pair(&mut inner, weapon, "owner", 100, 100, "Bob");
        pair(&mut inner, weapon, "same", 100, 100, " alice ");
        pair(&mut inner, weapon, "unknown", 100, 100, "");
        for i in 0..57 {
            pair(&mut inner, weapon, &format!("both{i}"), 100, 200, "Bob");
        }
        let unchanged = inner.auction_index["same-new"];
        // Repeated observations and an unknown price don't count as price changes.
        inner.observations.push(Observation {
            t: 4000,
            auction: unchanged,
            price: None,
            direct_sell: true,
        });
        inner.observations.push(Observation {
            t: 5000,
            auction: unchanged,
            price: Some(100),
            direct_sell: true,
        });
        inner.finish_load();
        for (price_changed, owner_changed, expected) in [
            (false, false, 61),
            (true, false, 58),
            (false, true, 58),
            (true, true, 57),
        ] {
            let query = Query {
                price_changed,
                owner_changed,
                ..all_time()
            };
            let result = inner.search(&query);
            assert_eq!((result.total, result.gone_total), (expected, expected));
            for gone in [false, true] {
                let first = inner.search_page(&query, 0, Some(gone));
                let second = inner.search_page(&query, 50, Some(gone));
                let rows: Vec<_> = if gone {
                    first.gone.into_iter().chain(second.gone).collect()
                } else {
                    first.listings.into_iter().chain(second.listings).collect()
                };
                assert_eq!(rows.len(), expected);
                assert_eq!(
                    rows.iter()
                        .map(|l| &l.riven_id)
                        .collect::<HashSet<_>>()
                        .len(),
                    expected
                );
                if price_changed && owner_changed {
                    assert!(rows.iter().all(|l| l.riven_id.starts_with("both")));
                }
            }
        }
        let id = inner.search(&Query {
            riven_id: Some("same".into()),
            price_changed: true,
            owner_changed: true,
            ..all_time()
        });
        assert_eq!(
            (id.total, id.gone_total),
            (1, 1),
            "ID searches ignore filters"
        );

        // A change within one auction is sufficient, and refresh rebuilds the cached flags.
        inner.observations.push(Observation {
            t: 6000,
            auction: unchanged,
            price: Some(125),
            direct_sell: true,
        });
        inner.finish_load();
        assert_eq!(
            inner
                .search(&Query {
                    price_changed: true,
                    ..all_time()
                })
                .total,
            59
        );
        inner.observations.pop();
        inner.finish_load();
        assert_eq!(
            inner
                .search(&Query {
                    price_changed: true,
                    ..all_time()
                })
                .total,
            58
        );
    }

    #[test]
    fn period_limits_chart_and_gone_listings_but_not_current_ones() {
        let now = now_ms();
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        // Seen long ago and gone long ago, seen recently and gone recently, and still listed.
        let old = add_auction(&mut inner, weapon, "old", 100, now - 60 * DAY_MS);
        let recent = add_auction(&mut inner, weapon, "recent", 200, now - 2 * DAY_MS);
        add_auction(&mut inner, weapon, "current", 300, now - 3_600_000);
        for (auction, t) in [(old, now - 59 * DAY_MS), (recent, now - DAY_MS)] {
            inner.auctions[auction as usize].gone = Some((t, "missing".into()));
            inner.gone.push(GoneEvent {
                t,
                auction,
                price: Some(100),
                direct_sell: true,
            });
        }
        inner.finish_load();
        let search = |period| {
            inner.search(&Query {
                period,
                ..all_time()
            })
        };

        let all = search(Period::All);
        assert_eq!((all.total, all.gone_total), (1, 2));
        assert_eq!(all.bucket_hours, 24);
        assert_eq!(all.history.iter().map(|p| p.count).sum::<u32>(), 3);

        let week = search(Period::Week);
        assert_eq!((week.total, week.gone_total), (1, 1));
        // Only two days of the week have data, so the buckets fit those.
        assert_eq!(week.bucket_hours, 1);
        assert_eq!(week.history.iter().map(|p| p.count).sum::<u32>(), 2);
        assert!(
            week.history
                .iter()
                .all(|p| p.t >= now - 7 * DAY_MS - 3_600_000)
        );

        let hours = |h: i64| bucket_hours_for(h * 3_600_000);
        assert_eq!(
            [
                hours(24),
                hours(168),
                hours(720),
                hours(2400),
                hours(9000),
                hours(20000)
            ],
            [1, 3, 12, 24, 168, 168]
        );

        let today = search(Period::Today);
        assert_eq!(today.bucket_hours, 1);
        assert_eq!(today.gone_total, 0);

        // Whole days, both inclusive.
        let day = now - 60 * DAY_MS;
        let day = day - day.rem_euclid(DAY_MS);
        let custom = search(Period::Custom {
            from: Some(day),
            to: Some(day + DAY_MS),
        });
        assert_eq!(custom.gone_total, 1);
        assert_eq!(custom.history.iter().map(|p| p.count).sum::<u32>(), 1);
        assert_eq!(custom.bucket_hours, 1);

        // An id search shows the riven's whole past whatever the period.
        let id = inner.auctions[old as usize].riven;
        let found = inner.search(&Query {
            riven_id: Some(inner.rivens[id as usize].clone()),
            period: Period::Today,
            ..all_time()
        });
        assert_eq!(found.gone_total, 1);
    }

    #[test]
    fn old_search_requests_default_history_filters_to_off() {
        let mut value = serde_json::to_value(Query::default()).unwrap();
        value.as_object_mut().unwrap().remove("price_changed");
        value.as_object_mut().unwrap().remove("owner_changed");
        assert_eq!(
            serde_json::from_value::<Query>(value).unwrap(),
            Query::default()
        );
    }

    #[test]
    fn listing_pages_reach_every_match_beyond_the_old_caps() {
        let mut inner = Inner::default();
        let weapon = inner.intern_weapon("w", "W", "rifle");
        for i in 0..237 {
            add_auction(&mut inner, weapon, &format!("active{i:03}"), 100, 1000);
        }
        for i in 0..113 {
            let index = add_auction(&mut inner, weapon, &format!("gone{i:03}"), 100, 1000);
            inner.auctions[index as usize].gone = Some((2000 + i, "missing".into()));
            inner.gone.push(GoneEvent {
                t: 2000 + i,
                auction: index,
                price: Some(100),
                direct_sell: true,
            });
        }
        inner.finish_load();
        for sort in [
            Sort::PriceAsc,
            Sort::PriceDesc,
            Sort::QualityDesc,
            Sort::Newest,
        ] {
            let query = Query {
                weapon: Some("w".into()),
                sort,
                ..all_time()
            };
            let first = inner.search(&query);
            assert_eq!((first.total, first.gone_total), (237, 113));
            assert_eq!((first.listings.len(), first.gone.len()), (50, 50));
            for (gone, expected) in [(false, 237), (true, 113)] {
                let mut ids = HashSet::new();
                for offset in (0..expected).step_by(LISTING_PAGE_SIZE) {
                    let page = inner.search_page(&query, offset, Some(gone));
                    assert!(page.history.is_empty());
                    let rows = if gone {
                        assert!(page.listings.is_empty());
                        page.gone
                    } else {
                        assert!(page.gone.is_empty());
                        page.listings
                    };
                    assert_eq!(rows.len(), (expected - offset).min(LISTING_PAGE_SIZE));
                    for row in rows {
                        assert_eq!(row.gone_at.is_some(), gone);
                        assert!(ids.insert(row.auction_id), "duplicate across pages");
                    }
                }
                assert_eq!(ids.len(), expected);
                let end = inner.search_page(&query, expected, Some(gone));
                assert!(end.listings.is_empty() && end.gone.is_empty());
            }
            let filtered = inner.search_page(
                &Query {
                    min_price: Some(101),
                    ..query
                },
                0,
                Some(false),
            );
            assert_eq!(filtered.total, 0);
        }
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
            ..all_time()
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
            ..all_time()
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
            ..all_time()
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
            ..all_time()
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
    fn feed_sightings_do_not_make_other_listings_unconfirmed() -> anyhow::Result<()> {
        let root = std::env::temp_dir().join(format!("wfmt-web-feed-{}", now_ms()));
        fs::create_dir_all(&root)?;
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../scraper/tests/fixtures/kulstar_auctions.json.gz")[..],
        )
        .read_to_string(&mut body)?;
        let page: serde_json::Value = serde_json::from_str(&body)?;
        let mut sighted = page["payload"]["auctions"][0].clone();
        sighted["buyout_price"] = 123_456.into();
        sighted["item"]["weapon_url_name"] = "kulstar".into();
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
        let swept_at: chrono::DateTime<chrono::Utc> = "2026-11-01T00:00:00Z".parse()?;
        let raw = |at, id: &str, body: String| {
            RawResponse::new(
                at,
                id.into(),
                "pc".into(),
                vec![RawPage {
                    buyout_policy: None,
                    sort_by: id.into(),
                    body,
                }],
                Vec::new(),
            )
        };
        pipeline.process(&weapon, &raw(swept_at, "kulstar", body), &mut sink)?;
        sink.flush()?;
        let feed = serde_json::json!({"payload": {"auctions": [sighted]}}).to_string();
        pipeline.process_feed(
            std::slice::from_ref(&weapon),
            &raw(
                swept_at + chrono::TimeDelta::hours(1),
                wfmt_types::FEED_RECENT,
                feed,
            ),
            &mut sink,
        )?;
        sink.flush()?;
        let store = Store::new(root.clone());
        store.refresh();
        {
            let inner = store.inner.read().unwrap();
            assert_eq!(inner.weapons[0].last_scrape, swept_at.timestamp_millis());
            assert!(
                inner
                    .auctions
                    .iter()
                    .any(|a| a.buyout_price == Some(123_456))
            );
        }
        // A later feed withdraws all prices. The reader must replace the old price
        // and retain an unknown-price observation so the history can show a gap.
        sighted["buyout_price"] = serde_json::Value::Null;
        sighted["starting_price"] = serde_json::Value::Null;
        sighted["top_bid"] = serde_json::Value::Null;
        let withdrawn_at = swept_at + chrono::TimeDelta::hours(2);
        pipeline.process_feed(
            std::slice::from_ref(&weapon),
            &raw(
                withdrawn_at,
                wfmt_types::FEED_RECENT,
                serde_json::json!({"payload": {"auctions": [sighted]}}).to_string(),
            ),
            &mut sink,
        )?;
        sink.flush()?;
        let reloaded = Store::new(root.clone());
        reloaded.refresh();
        {
            let inner = reloaded.inner.read().unwrap();
            let index = inner.auction_index[sighted["id"].as_str().unwrap()];
            let auction = &inner.auctions[index as usize];
            assert_eq!(auction_price(auction), None);
            assert_eq!(auction.last_seen, withdrawn_at.timestamp_millis());
            assert!(inner.is_current(auction), "withdrawal is not a sale");
            assert!(inner.observations.iter().any(|o| {
                o.auction == index && o.t == withdrawn_at.timestamp_millis() && o.price.is_none()
            }));
        }
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

/// Chart bucket width for a time span: the finest that keeps the chart at 100 points or fewer.
fn bucket_hours_for(span_ms: i64) -> u32 {
    [1, 3, 6, 12, 24, 72, 168]
        .into_iter()
        .find(|h| span_ms <= 100 * i64::from(*h) * 3_600_000)
        .unwrap_or(168)
}

#[derive(Clone, Copy, Default)]
struct RivenChanges {
    price: bool,
    owner: bool,
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
    /// Rebuilt on refresh so history filters don't rescan observations on every page.
    riven_changes: Vec<RivenChanges>,
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
        self.riven_changes.push(RivenChanges::default());
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
            is_bow: false,
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
            let sources = b
                .column_by_name("source")
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
                // A feed sighting says nothing about the weapon's other listings, so it must not
                // make them look unconfirmed.
                if !sources.is_some_and(|c| c.is_valid(i) && wfmt_types::is_feed(c.value(i))) {
                    let w = &mut self.weapons[weapon as usize];
                    w.last_scrape = w.last_scrape.max(t);
                }

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

        self.riven_changes = vec![RivenChanges::default(); self.rivens.len()];
        let mut first_prices = vec![None; self.rivens.len()];
        for o in &self.observations {
            let riven = self.auctions[o.auction as usize].riven as usize;
            // Missing prices aren't evidence of a change; compare known numeric prices only.
            if let Some(price) = o.price {
                let first = first_prices[riven].get_or_insert(price);
                self.riven_changes[riven].price |= *first != price;
            }
        }
        for (riven, auctions) in self.riven_auctions.iter().enumerate() {
            let mut sellers = auctions
                .iter()
                .map(|&i| self.auctions[i as usize].owner_name.trim())
                .filter(|name| !name.is_empty());
            if let Some(first) = sellers.next() {
                self.riven_changes[riven].owner =
                    sellers.any(|name| !name.eq_ignore_ascii_case(first));
            }
        }

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
        let changes = self.riven_changes[a.riven as usize];
        if (q.price_changed && !changes.price) || (q.owner_changed && !changes.owner) {
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
        self.search_page(q, 0, None)
    }

    /// `None` includes both first pages and history; subsequent pages fetch only one tab.
    pub fn search_page(&self, q: &Query, offset: usize, gone_tab: Option<bool>) -> SearchResult {
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
                        bucket_hours: 24,
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
            .skip(offset)
            .take(if gone_tab == Some(true) {
                0
            } else if id_search && gone_tab.is_none() {
                usize::MAX
            } else {
                LISTING_PAGE_SIZE
            })
            .map(|a| self.listing(a))
            .collect();

        // The period limits the chart and the disappeared listings; current listings are current.
        // An id search shows the riven's whole past.
        let now = now_ms();
        let (start, end) = if id_search {
            (None, None)
        } else {
            q.period.bounds(now)
        };
        let in_period = |t: i64| start.is_none_or(|s| t >= s) && end.is_none_or(|e| t < e);

        // Disappeared listings (≈ sold), newest first, one per riven.
        let gone_events: Vec<&GoneEvent> = self
            .gone
            .iter()
            .filter(|g| {
                in_period(g.t)
                    && matching[g.auction as usize]
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
            .skip(offset)
            .take(if gone_tab == Some(false) {
                0
            } else if id_search && gone_tab.is_none() {
                usize::MAX
            } else {
                LISTING_PAGE_SIZE
            })
            .map(|g| self.listing(&self.auctions[g.auction as usize]))
            .collect();

        if gone_tab.is_some() {
            return SearchResult {
                listings,
                total,
                gone,
                gone_total,
                outliers_hidden,
                price_limit: weapon.and_then(|w| self.weapons[w as usize].price_limit),
                history: Vec::new(),
                bucket_hours: 24,
                server_time: now,
            };
        }

        // Observations are sorted by time, so the period is one slice of them.
        let observations = {
            let lo = start.map_or(0, |s| self.observations.partition_point(|o| o.t < s));
            let hi = end.map_or(self.observations.len(), |e| {
                self.observations.partition_point(|o| o.t < e)
            });
            &self.observations[lo..hi.max(lo)]
        };
        // Bucket width follows the time the data covers: hours for a day, days for months.
        let span_start = observations.first().map(|o| o.t).or(start).unwrap_or(now);
        let bucket_hours = bucket_hours_for(end.unwrap_or(now).min(now) - span_start);

        // Price history: per bucket, the latest price of every matching riven (cheapest copy),
        // plus the last prices of rivens that disappeared in that bucket.
        let bucket = i64::from(bucket_hours) * 3_600_000;
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
        for o in observations {
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
            bucket_hours,
            server_time: now,
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

    pub fn riven_detail(
        &self,
        id: &str,
        auction_id: Option<&str>,
        include_legacy: bool,
        include_unfit: bool,
        include_loose: bool,
    ) -> Option<RivenDetail> {
        let riven = *self.riven_index.get(id)?;
        let copies = &self.riven_auctions[riven as usize];
        let selected = if let Some(id) = auction_id {
            let index = *self.auction_index.get(id)?;
            let auction = &self.auctions[index as usize];
            if auction.riven != riven {
                return None;
            }
            auction
        } else {
            copies
                .iter()
                .map(|&i| &self.auctions[i as usize])
                .max_by(|a, b| a.last_seen.cmp(&b.last_seen).then(a.id.cmp(&b.id)))?
        };
        let (similar, similar_total, comparisons_hidden) =
            self.similar_to(selected, include_legacy, include_unfit, include_loose);
        Some(RivenDetail {
            listing: self.listing(selected),
            similar,
            similar_total,
            comparisons_hidden,
            server_time: now_ms(),
        })
    }

    /// Other rivens of the same weapon, closest first: the first 10, how many there are, and
    /// how many the era / verification settings hid.
    fn similar_to(
        &self,
        selected: &Auction,
        include_legacy: bool,
        include_unfit: bool,
        include_loose: bool,
    ) -> (Vec<SimilarRiven>, usize, usize) {
        // Pick a representative before scoring, so old relists cannot win twice or
        // make a riven appear closer than its latest eligible observation.
        let mut hidden = HashSet::new();
        let mut representatives: HashMap<u32, &Auction> = HashMap::new();
        for a in &self.auctions {
            if a.weapon != selected.weapon || a.riven == selected.riven {
                continue;
            }
            if (!include_legacy && a.legacy_stats != Some(false)) || (!include_unfit && !a.fit_ok) {
                if similarity(selected, a).is_some_and(|score| include_loose || score.shared >= 2) {
                    hidden.insert(a.riven);
                }
                continue;
            }
            representatives
                .entry(a.riven)
                .and_modify(|current| {
                    if (a.last_seen, &a.id) > (current.last_seen, &current.id) {
                        *current = a;
                    }
                })
                .or_insert(a);
        }
        let mut matches: Vec<_> = representatives
            .values()
            .filter_map(|a| {
                let score = similarity(selected, a)?;
                (include_loose || score.shared >= 2).then_some((*a, score))
            })
            .collect();
        matches.sort_by(|(a, score_a), (b, score_b)| {
            score_b
                .shared
                .cmp(&score_a.shared)
                .then_with(|| score_a.added.len().cmp(&score_b.added.len()))
                .then_with(|| score_b.same_negative.cmp(&score_a.same_negative))
                .then_with(|| {
                    a.attrs
                        .len()
                        .abs_diff(selected.attrs.len())
                        .cmp(&b.attrs.len().abs_diff(selected.attrs.len()))
                })
                .then_with(|| {
                    score_a
                        .roll_distance
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&score_b.roll_distance.unwrap_or(f64::INFINITY))
                })
                .then_with(|| b.last_seen.cmp(&a.last_seen))
                .then_with(|| a.id.cmp(&b.id))
        });
        let similar_total = matches.len();
        // Count distinct candidates hidden by quality/era settings, excluding any
        // riven that also has an eligible representative in this result set.
        for (a, _) in &matches {
            hidden.remove(&a.riven);
        }
        let similar = matches
            .into_iter()
            .take(10)
            .map(|(a, score)| SimilarRiven {
                listing: self.listing(a),
                same_stats: score.missing.is_empty()
                    && score.added.is_empty()
                    && score.same_negative,
                shared_positives: score.shared,
                missing_positives: score
                    .missing
                    .iter()
                    .map(|&i| self.attrs[i as usize].clone())
                    .collect(),
                added_positives: score
                    .added
                    .iter()
                    .map(|&i| self.attrs[i as usize].clone())
                    .collect(),
                same_negative: score.same_negative,
                roll_distance: score.roll_distance,
            })
            .collect();
        (similar, similar_total, hidden.len())
    }

    /// A scan as an auction that isn't stored, with its stats fitted like a listing's. `None`
    /// for an unknown weapon. Attributes the data has never seen are left out of the auction
    /// (they can't be compared), but stay in the fit.
    fn scanned_auction(&self, scan: &RivenScan) -> Option<(Auction, Option<riven::Fit>)> {
        let weapon = *self.weapon_index.get(scan.weapon.as_deref()?)?;
        let w = &self.weapons[weapon as usize];
        let class = match w.class.as_str() {
            "rifle" => WeaponClass::Rifle,
            "shotgun" => WeaponClass::Shotgun,
            "pistol" => WeaponClass::Pistol,
            "archgun" => WeaponClass::Archgun,
            "melee" => WeaponClass::Melee,
            _ => return None,
        };
        let inputs: Vec<AttrInput> = scan
            .attributes
            .iter()
            .map(|a| AttrInput {
                slug: &a.slug,
                value: a.value,
                positive: a.positive,
            })
            .collect();
        // Screenshots don't show the rank reliably; the fit tries every rank anyway.
        let fit = riven::fit(&RivenInput {
            class,
            is_bow: w.is_bow,
            listed_rank: i64::from(riven::MAX_RANK),
            attributes: &inputs,
            legacy_disposition: None,
            prefer_legacy: false,
        })
        .ok();
        let attrs: Vec<Attr> = scan
            .attributes
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let fitted = fit.as_ref().map(|f| &f.attributes[i]);
                Some(Attr {
                    attr: *self.attr_index.get(&a.slug)?,
                    positive: a.positive,
                    value: a.value,
                    value_rank8: fitted.map(|f| f.value_rank8),
                    quality: fitted.map(|f| f.quality),
                })
            })
            .collect();
        let positives: Vec<f64> = attrs
            .iter()
            .filter(|a| a.positive)
            .filter_map(|a| a.quality)
            .collect();
        let now = now_ms();
        let auction = Auction {
            id: String::new(),
            weapon,
            riven: u32::MAX,
            gone: None,
            riven_name: scan.name.clone().unwrap_or_default().to_lowercase(),
            polarity: String::new(),
            mastery_level: scan.mastery_rank,
            // A card shows no count until the riven is rerolled.
            re_rolls: Some(scan.rerolls.unwrap_or(0)),
            rank: fit.as_ref().map(|f| i32::from(f.rank)),
            legacy_stats: fit.as_ref().map(|f| f.legacy),
            fit_ok: fit.is_some(),
            mean_quality: (!positives.is_empty())
                .then(|| positives.iter().sum::<f64>() / positives.len() as f64),
            owner_name: String::new(),
            owner_reputation: None,
            created: now,
            first_seen: now,
            last_seen: now,
            attrs,
            buyout_price: None,
            starting_price: None,
            top_bid: None,
            direct_sell: false,
        };
        Some((auction, fit))
    }

    /// The id of the stored riven a scan shows, if it has been listed before. Matches the way
    /// listings are identified, except that polarity isn't read from screenshots.
    pub fn scan_match(&self, scan: &RivenScan) -> Option<String> {
        let (auction, fit) = self.scanned_auction(scan)?;
        let fit = fit?;
        let attrs: Vec<IdAttr> = scan
            .attributes
            .iter()
            .zip(&fit.attributes)
            .map(|(a, f)| IdAttr {
                slug: &a.slug,
                positive: a.positive,
                value: a.value,
                roll: Some((f.roll, f.roll_tolerance)),
            })
            .collect();
        let input = IdInput {
            weapon: &self.weapons[auction.weapon as usize].slug,
            polarity: "",
            mastery_level: auction.mastery_level,
            re_rolls: auction.re_rolls,
            listed_rank: i64::from(fit.rank),
            owner: "",
            attributes: &attrs,
        };
        let journal = data_dir().join("rivens.jsonl");
        let id = match Registry::find_in_journal(&journal, &input) {
            Ok(id) => id?,
            Err(e) => {
                eprintln!("failed to read {}: {e:#}", journal.display());
                return None;
            }
        };
        self.riven_index.contains_key(&id).then_some(id)
    }

    /// The riven page for a scanned riven that isn't in the data.
    pub fn scanned_detail(
        &self,
        scan: &RivenScan,
        include_legacy: bool,
        include_unfit: bool,
        include_loose: bool,
    ) -> Option<RivenDetail> {
        let (auction, fit) = self.scanned_auction(scan)?;
        let (similar, similar_total, comparisons_hidden) =
            self.similar_to(&auction, include_legacy, include_unfit, include_loose);
        let w = &self.weapons[auction.weapon as usize];
        let listing = Listing {
            auction_id: String::new(),
            riven_id: String::new(),
            price_outlier: false,
            price_limit: w.price_limit,
            copies: 0,
            sellers: 0,
            gone_at: None,
            gone_reason: None,
            last_seen: auction.last_seen,
            unconfirmed: false,
            weapon_slug: w.slug.clone(),
            weapon_name: w.name.clone(),
            riven_name: auction.riven_name.clone(),
            price: None,
            buyout_price: None,
            starting_price: None,
            top_bid: None,
            direct_sell: false,
            rank: auction.rank,
            re_rolls: auction.re_rolls,
            mastery_level: auction.mastery_level,
            polarity: String::new(),
            legacy_stats: auction.legacy_stats,
            fit_ok: auction.fit_ok,
            mean_quality: auction.mean_quality,
            owner_name: String::new(),
            owner_reputation: None,
            created: auction.created,
            first_seen: auction.first_seen,
            attributes: scan
                .attributes
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let fitted = fit.as_ref().map(|f| &f.attributes[i]);
                    AttrView {
                        slug: a.slug.clone(),
                        positive: a.positive,
                        value: a.value,
                        value_rank8: fitted.map(|f| f.value_rank8),
                        quality: fitted.map(|f| f.quality),
                    }
                })
                .collect(),
        };
        Some(RivenDetail {
            listing,
            similar,
            similar_total,
            comparisons_hidden,
            server_time: now_ms(),
        })
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
                    points: {
                        let mut observations = points.remove(&i).unwrap_or_default();
                        observations.sort_by_key(|p| p.t);
                        observations
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then(a.auction_id.cmp(&b.auction_id))
        });
        out
    }
}
