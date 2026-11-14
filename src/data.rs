//! Server side: keeps the scraper's parquet output in memory and answers queries.
//!
//! New parts are picked up incrementally. When compaction replaces loaded files, rebuild
//! the cache from the new snapshot so observations aren't counted twice.

// FxHash instead of SipHash: the indexes are hashed millions of times per load and per
// search, and their keys come from our own dataset rather than from requests.
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    LazyLock, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard,
    TryLockError,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use arrow::array::{
    Array, BooleanArray, Float64Array, Int32Array, Int64Array, StringArray,
    TimestampMillisecondArray,
};
use arrow::record_batch::RecordBatch;
use dioxus::prelude::ServerFnError;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};

use crate::api::{
    AttrInfo, AttrView, Comparisons, CurseFilter, EFFECT_MIN_RIVENS, EFFECT_MIN_STAT_RIVENS,
    HOUR_MS, HistoryPoint, Listing, MIN_PRICE, Overview, PricePoint, Query, RivenAuction,
    RivenDetail, RivenScan, SIMILAR_LIMIT, SearchResult, SimilarRiven, Sort, StatEffect,
    StatEffects, WEEK_MS, WeaponInfo,
};
use crate::effects;
use wfmt_scraper::identity::{IdAttr, IdInput, Registry};
use wfmt_scraper::riven::{self, AttrInput, RivenInput};
use wfmt_scraper::tracking::GoneReason;
use wfmt_types::layout::PARQUET_DIR;
use wfmt_types::{RivenType, WeaponClass, WeaponGroup};

const RESCAN_EVERY: Duration = Duration::from_secs(5);
const LISTING_PAGE_SIZE: usize = 50;
/// Rows per batch when reading parquet files: big enough to amortize the per-batch work,
/// small enough to keep memory flat.
const READ_BATCH_SIZE: usize = 8192;

// Prices are free text, and sellers park rivens at joke prices like 88888 or 666666. A price is
// invalid when it's far above the weapon's normal high end. The floor protects real expensive
// rivens on cheap weapons. Tuned on real listings: flags about 0.5% of them.
const OUTLIER_PERCENTILE: f64 = 0.9;
const OUTLIER_FACTOR: i64 = 5;
const OUTLIER_FLOOR: i64 = 5000;
const OUTLIER_MIN_LISTINGS: usize = 5;
/// The percentile never picks one of this many top prices, which could be jokes themselves.
const OUTLIER_SKIP_TOP: usize = 2;

// The bar on explained differences keeps out weapons where stats predict nothing, mostly
// those where only one end of the price range has been seen. See `EFFECT_MIN_RIVENS` too.
const EFFECT_MIN_EXPLAINED: f64 = 0.15;
/// 95% interval of a normal distribution.
const INTERVAL: f64 = 1.96;

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

/// Loads the data before the first request arrives, so the first visitor doesn't wait for it.
pub fn warm() {
    STORE.refresh();
}

/// Runs `f` on the loaded data only if that takes no waiting: no refresh, no lock held by a
/// reload. For server rendering, which must never hold a page back; the browser fetches
/// whatever this couldn't answer.
pub fn try_now<T>(f: impl FnOnce(&Inner) -> T) -> Option<T> {
    if !STORE.ready.load(Ordering::Acquire) {
        return None;
    }
    let inner = STORE.inner.try_read().ok()?;
    Some(f(&inner))
}

/// How long a memoized answer may be reused. Answers depend on the clock too (periods, ages).
const MEMO_TTL: Duration = Duration::from_secs(30);
const SEARCH_MEMO_SIZE: usize = 8;

/// Every open page refetches the overview after each scraper batch, and the start page
/// searches all weapons; both scan every auction (~10–45 ms on 100k auctions), while a
/// single weapon's search takes well under a millisecond. So only those two are memoized,
/// per data version.
static OVERVIEW_MEMO: Mutex<Option<(u64, Instant, Overview)>> = Mutex::new(None);
static SEARCH_MEMO: Mutex<Vec<(u64, Instant, Query, SearchResult)>> = Mutex::new(Vec::new());

/// Refreshes the store if needed and runs `f` on a blocking thread.
pub async fn run<T, F>(f: F) -> Result<T, ServerFnError>
where
    T: Send + 'static,
    F: FnOnce(&Inner) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        STORE.refresh();
        f(&STORE.read())
    })
    .await
    .map_err(ServerFnError::new)
}

/// Tests can pin the clock, so answers on the real dataset compare across runs.
#[cfg(test)]
static FROZEN_NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

fn now_ms() -> i64 {
    #[cfg(test)]
    {
        let frozen = FROZEN_NOW.load(Ordering::Relaxed);
        if frozen != 0 {
            return frozen;
        }
    }
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

    // Poisoned locks are taken anyway. Only a load or a reload writes to `inner`, both under
    // `last_scan`; if one panicked, `refresh` notices the poisoned `last_scan` and starts over.
    fn read(&self) -> RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(PoisonError::into_inner)
    }

    fn catalog_mtime(&self) -> MutexGuard<'_, Option<SystemTime>> {
        self.catalog_mtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Forgets everything loaded, for when files were replaced under the same names. Requests
    /// wait for the next load instead of seeing an empty store.
    fn reload(&self) {
        let mut last = self
            .last_scan
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.reset(&mut last);
    }

    fn reset(&self, last: &mut Option<Instant>) {
        self.ready.store(false, Ordering::Release);
        let mut inner = self.write();
        *inner = Inner {
            version: inner.version + 1,
            ..Inner::default()
        };
        *self.catalog_mtime() = None;
        *last = None;
    }

    fn refresh(&self) {
        // The dataset lock below waits for the scraper's writes, which can take minutes during
        // compaction. Only one request waits; the rest serve the data already loaded.
        let mut last = match self.last_scan.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::WouldBlock) if self.ready.load(Ordering::Acquire) => return,
            Err(TryLockError::WouldBlock) => self
                .last_scan
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            Err(TryLockError::Poisoned(e)) => e.into_inner(),
        };
        if self.last_scan.is_poisoned() || self.inner.is_poisoned() {
            // A load or reload panicked part way, so what's loaded can't be trusted.
            tracing::error!("an earlier load of the data panicked, loading it again");
            self.reset(&mut last);
            self.inner.clear_poison();
            self.last_scan.clear_poison();
        }
        if last.is_some_and(|t| t.elapsed() < RESCAN_EVERY) {
            return;
        }
        self.scan(last);
        self.ready.store(true, Ordering::Release);
    }

    fn scan(&self, mut last: MutexGuard<'_, Option<Instant>>) {
        // Hold the dataset snapshot through discovery and loading. A reader also recovers a
        // transaction left by an interrupted writer before touching any Parquet files.
        let _snapshot = match wfmt_scraper::dataset::DatasetLock::read(&self.dir) {
            Ok(lock) => lock,
            Err(e) => {
                tracing::error!("failed to open dataset: {e:#}");
                return;
            }
        };
        *last = Some(Instant::now());
        let rebuild = self.read().loaded.iter().any(|p| !p.exists());
        if rebuild {
            // Build the replacement aside and swap it in: clearing the live store first would
            // serve empty pages for the second or so the full load takes.
            let mut fresh = Inner::default();
            *self.catalog_mtime() = None;
            if let Some(catalog) = self.changed_catalog() {
                apply_catalog(&mut fresh, catalog);
            }
            Self::load_pending(&self.dir, &mut fresh);
            let mut inner = self.write();
            fresh.version = inner.version + 1;
            *inner = fresh;
            return;
        }
        // Only take the write lock, which stalls every reader, when there's something to apply.
        if let Some(catalog) = self.changed_catalog() {
            apply_catalog(&mut self.write(), catalog);
        }
        let parquet = self.dir.join(PARQUET_DIR);
        let has_new = {
            let inner = self.read();
            part_files(&parquet)
                .iter()
                .any(|(a, _)| !inner.loaded.contains(a))
                || gone_files(&parquet)
                    .iter()
                    .any(|g| !inner.loaded.contains(g))
        };
        if has_new {
            let mut inner = self.write();
            Self::load_pending(&self.dir, &mut inner);
            inner.version += 1;
        }
    }

    /// Loads the parts `inner` hasn't loaded yet.
    fn load_pending(dir: &Path, inner: &mut Inner) {
        let parquet = dir.join(PARQUET_DIR);
        let pending: Vec<(PathBuf, PathBuf)> = part_files(&parquet)
            .into_iter()
            .filter(|(a, _)| !inner.loaded.contains(a))
            .collect();
        let pending_gone: Vec<PathBuf> = gone_files(&parquet)
            .into_iter()
            .filter(|g| !inner.loaded.contains(g))
            .collect();
        for (auctions, attributes) in pending {
            match inner.load(&auctions, &attributes) {
                Ok(()) => {
                    inner.loaded.insert(auctions);
                }
                Err(e) => tracing::error!("failed to load {}: {e:#}", auctions.display()),
            }
        }
        // Gone events refer to auctions, so they load after the auction parts.
        for path in pending_gone {
            match inner.load_gone(&path) {
                Ok(()) => {
                    inner.loaded.insert(path);
                }
                Err(e) => tracing::error!("failed to load {}: {e:#}", path.display()),
            }
        }
        inner.finish_load();
    }

    /// Reads the scraper's cached `/v2/riven/weapons` if it changed since the last call. It
    /// gives every weapon a name, group and image, even before its first scrape.
    fn changed_catalog(&self) -> Option<Catalog> {
        let path = self.dir.join("cache/riven_weapons.json");
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let mut last = self.catalog_mtime();
        if *last == Some(mtime) {
            return None;
        }
        let parsed = fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<Catalog>(&b).map_err(|e| e.to_string()));
        match parsed {
            Ok(catalog) => {
                *last = Some(mtime);
                Some(catalog)
            }
            Err(e) => {
                tracing::error!("failed to load {}: {e}", path.display());
                None
            }
        }
    }
}

struct Weapon {
    slug: String,
    name: String,
    /// `None` when warframe.market's weapon type is one the stat model doesn't know.
    class: Option<WeaponClass>,
    group: WeaponGroup,
    /// Bows show fire rate doubled, which the stat model needs to know.
    is_bow: bool,
    thumb: Option<String>,
    last_scrape: i64,
    /// Prices above this are invalid, see [`price_limit`].
    price_limit: Option<i64>,
}

struct Attr {
    attr: AttrId,
    positive: bool,
    value: f64,
    value_rank8: Option<f64>,
    quality: Option<f64>,
}

/// Latest known state of an auction.
struct Auction {
    id: String,
    weapon: WeaponId,
    riven: RivenId,
    gone: Option<Gone>,
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

impl Auction {
    /// `gone`, unless the auction has been seen again since.
    fn current_gone(&self) -> Option<Gone> {
        self.gone.filter(|g| g.at >= self.last_seen)
    }
}

/// When and why an auction disappeared.
#[derive(Clone, Copy)]
struct Gone {
    at: i64,
    reason: GoneReason,
}

/// One auction seen in one scrape.
/// 24 bytes rather than the 32 an `Option<i64>` price takes: charts stream through up to a
/// million of these per request.
struct Observation {
    t: i64,
    /// The price, or `Observation::NO_PRICE`.
    price: i64,
    auction: AuctionId,
    direct_sell: bool,
}

impl Observation {
    const NO_PRICE: i64 = i64::MIN;

    fn new(t: i64, auction: AuctionId, price: Option<i64>, direct_sell: bool) -> Self {
        Self {
            t,
            price: price.unwrap_or(Self::NO_PRICE),
            auction,
            direct_sell,
        }
    }

    fn price(&self) -> Option<i64> {
        (self.price != Self::NO_PRICE).then_some(self.price)
    }
}

/// An auction that disappeared (sold or removed).
struct GoneEvent {
    t: i64,
    auction: AuctionId,
    price: Option<i64>,
    direct_sell: bool,
}

fn price_of(buyout: Option<i64>, top_bid: Option<i64>, starting: Option<i64>) -> Option<i64> {
    buyout.or(top_bid).or(starting)
}

fn auction_price(a: &Auction) -> Option<i64> {
    price_of(a.buyout_price, a.top_bid, a.starting_price)
}

/// Orders the copies of one riven: a relist is newer than the auction it replaced.
fn recency(a: &Auction) -> (i64, i64, i64, &str) {
    (a.created, a.first_seen, a.last_seen, &a.id)
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
    weapons: Table<WeaponId, Weapon>,
    weapon_index: HashMap<String, WeaponId>,
    /// Attribute slugs.
    attrs: Table<AttrId, String>,
    attr_index: HashMap<String, AttrId>,
    auctions: Table<AuctionId, Auction>,
    auction_index: HashMap<String, AuctionId>,
    observations: Vec<Observation>,
    /// Riven ids, as the scraper's registry gives them.
    rivens: Table<RivenId, String>,
    riven_index: HashMap<String, RivenId>,
    /// Auctions per riven.
    riven_auctions: Table<RivenId, Vec<AuctionId>>,
    /// Rebuilt on refresh so history filters don't rescan observations on every page.
    riven_changes: Table<RivenId, RivenChanges>,
    /// Sorted by time.
    gone: Vec<GoneEvent>,
    /// Per auction, what searches filter on (see `Row`). Rebuilt by `finish_load`.
    rows: Table<AuctionId, Row>,
}

/// The fields searches filter and sort on, for one auction, packed tight. A search visits
/// every auction, and its chart up to a million observations at random auctions; reading
/// these 40 bytes instead of the full auction (a few hundred, with pointers to follow) keeps
/// that in cache. Filters on specific stats still check the auction's attributes.
#[derive(Clone, Copy)]
struct Row {
    /// `auction_price`, or `Row::NO_PRICE`.
    price: i64,
    /// The weapon's invalid-price limit, or `i64::MAX`.
    limit: i64,
    created: i64,
    /// NaN when unknown, so no minimum is met.
    mean_quality: f64,
    riven: RivenId,
    weapon: WeaponId,
    flags: RowFlags,
}

/// Yes/no facts about an auction, one bit each.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct RowFlags(u8);

impl RowFlags {
    /// The stats were fitted by the stat model.
    const FIT_OK: Self = Self(1);
    /// The stats are from before the disposition rework.
    const LEGACY: Self = Self(2);
    const DIRECT: Self = Self(4);
    /// Still listed (see `Inner::is_current`).
    const CURRENT: Self = Self(8);
    /// Has a negative stat.
    const CURSED: Self = Self(16);

    fn set(&mut self, flag: Self, on: bool) {
        if on {
            self.0 |= flag.0;
        }
    }
}

impl Row {
    const NO_PRICE: i64 = i64::MIN;

    fn has(&self, flag: RowFlags) -> bool {
        self.flags.0 & flag.0 != 0
    }

    fn price(&self) -> Option<i64> {
        (self.price != Self::NO_PRICE).then_some(self.price)
    }

    /// `Inner::price_outlier` for the auction's own price.
    fn outlier(&self) -> bool {
        self.price != Self::NO_PRICE && self.price > self.limit
    }

    /// `Inner::price_outlier` for another price of the same weapon.
    fn outlier_price(&self, price: Option<i64>) -> bool {
        price.is_some_and(|p| p > self.limit)
    }
}

mod detail;
mod ids;
mod load;
mod query;

use ids::{AttrId, AuctionId, Id, RivenId, Table, WeaponId};

use load::{Catalog, apply_catalog, gone_files, part_files};

#[cfg(test)]
mod bench;

#[cfg(test)]
mod golden;

#[cfg(test)]
mod storage_tests;

#[cfg(test)]
mod loading_tests;
