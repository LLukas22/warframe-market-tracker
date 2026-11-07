//! On-disk layout under the data directory:
//!
//! ```text
//! cache/riven_weapons.json             raw /v2/riven/weapons body
//! cache/meta.json                      riven manifest version + last check
//! state.json                           last fetch time per weapon (survives restarts)
//! rivens.jsonl                         riven identity registry (append-only)
//! seen/<weapon>.json                   auctions seen in the last fetch, for gone detection
//! raw/<date>/<weapon>-<ms>.json.gz     raw auction responses, for re-normalizing offline
//! raw/<date>/.committed/*.json        receipts, published with normalized output
//! storage/transaction.json            interrupted dataset publication, if any
//! storage/raw-retention.json          records whether the raw archive is incomplete
//! parquet/auctions/date=<date>/*.parquet  small parts and compacted historical parts
//! parquet/attributes/date=<date>/*.parquet
//! parquet/gone/date=<date>/*.parquet         auctions that disappeared (sold or removed)
//! ```

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use arrow::array::{
    ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, RecordBatch, StringArray,
    TimestampMillisecondArray,
};
use chrono::{DateTime, NaiveDate, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression as ParquetCompression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};

use crate::dataset::{DatasetLock, Transaction, unique_id};
use crate::normalize::{AttributeRow, AuctionRow};
use crate::tracking::GoneRow;

/// Writes `contents` atomically so a crash never leaves a truncated file behind.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing {}", path.display()))?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Default, Serialize, Deserialize)]
pub struct Meta {
    pub rivens_version: Option<String>,
    pub checked_at: Option<DateTime<Utc>>,
}

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    pub last_fetch: HashMap<String, DateTime<Utc>>,
    /// Start of the last scheduled or full manual sweep; the next one is due an interval later.
    #[serde(default)]
    pub last_sweep_started: Option<DateTime<Utc>>,
    /// Sweep in progress, so it resumes after a restart.
    #[serde(default)]
    pub sweep: Option<PersistedSweep>,
    #[serde(default)]
    pub last_sweep: Option<wfmt_types::SweepSummary>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct PersistedSweep {
    pub progress: wfmt_types::SweepProgress,
    /// Weapons still to fetch, in order.
    pub queue: Vec<String>,
}

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Journal of riven identities (see `identity`).
    pub fn registry_path(&self) -> PathBuf {
        self.root.join("rivens.jsonl")
    }

    /// Last known auctions per weapon (see `tracking`).
    pub fn seen_dir(&self) -> PathBuf {
        self.root.join("seen")
    }

    pub fn load_meta(&self) -> Result<Meta> {
        Ok(read_json(&self.root.join("cache/meta.json"))?.unwrap_or_default())
    }

    pub fn save_meta(&self, meta: &Meta) -> Result<()> {
        write_atomic(
            &self.root.join("cache/meta.json"),
            &serde_json::to_vec_pretty(meta)?,
        )
    }

    pub fn load_weapons_raw(&self) -> Result<Option<String>> {
        match fs::read_to_string(self.root.join("cache/riven_weapons.json")) {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save_weapons_raw(&self, body: &str) -> Result<()> {
        write_atomic(&self.root.join("cache/riven_weapons.json"), body.as_bytes())
    }

    pub fn load_state(&self) -> Result<State> {
        Ok(read_json(&self.root.join("state.json"))?.unwrap_or_default())
    }

    pub fn save_state(&self, state: &State) -> Result<()> {
        write_atomic(&self.root.join("state.json"), &serde_json::to_vec(state)?)
    }

    pub fn save_raw(&self, raw: &RawResponse) -> Result<()> {
        let path = self
            .root
            .join("raw")
            .join(raw.scraped_at.date_naive().to_string())
            .join(raw.file_name());
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        serde_json::to_writer(&mut gz, raw)?;
        crate::dataset::durable_write(&path, &gz.finish()?)
    }

    /// New-format archives without a committed output receipt, in observation order.
    pub fn pending_raw(&self) -> Result<Vec<RawResponse>> {
        let mut committed = std::collections::HashSet::new();
        let raw_dir = self.root.join("raw");
        if !raw_dir.exists() {
            return Ok(Vec::new());
        }
        for day in fs::read_dir(&raw_dir)? {
            let receipts = day?.path().join(".committed");
            if !receipts.is_dir() {
                continue;
            }
            for file in fs::read_dir(receipts)? {
                let path = file?.path();
                if path.extension().is_some_and(|e| e == "json") {
                    let names: Vec<String> = serde_json::from_slice(&fs::read(path)?)?;
                    committed.extend(names);
                }
            }
        }
        let mut pending = Vec::new();
        for path in self.raw_files()? {
            if committed.contains(path.file_name().unwrap().to_str().unwrap()) {
                continue;
            }
            let raw = RawResponse::read(&path)?;
            if raw.recoverable {
                pending.push(raw);
            }
        }
        pending.sort_by_key(|r| (r.scraped_at, r.weapon.clone()));
        Ok(pending)
    }

    /// All archived raw responses, oldest first.
    pub fn raw_files(&self) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        let dir = self.root.join("raw");
        if !dir.exists() {
            return Ok(files);
        }
        for day in fs::read_dir(dir)? {
            let day = day?;
            if !day.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(day.path())? {
                let path = file?.path();
                if path.to_string_lossy().ends_with(".json.gz") {
                    files.push(path);
                }
            }
        }
        files.sort();
        Ok(files)
    }
}

/// Everything one fetch of a weapon returned, so `reprocess` can replay it exactly.
#[derive(Serialize, Deserialize)]
pub struct RawResponse {
    pub scraped_at: DateTime<Utc>,
    pub weapon: String,
    pub platform: String,
    /// Search pages in the order they were fetched.
    #[serde(default)]
    pub pages: Vec<RawPage>,
    /// Entry-endpoint checks of auctions that dropped out of the pages.
    #[serde(default)]
    pub verified: Vec<Verified>,
    /// Archives from before multi-page fetching had a single `price_asc` body.
    #[serde(default, skip_serializing)]
    body: Option<String>,
    /// False for an interrupted fetch. Old archives represent completed fetches.
    #[serde(default = "default_complete")]
    pub complete: bool,
    /// Only new archives participate in automatic recovery (legacy receipts may be absent).
    #[serde(default)]
    pub recoverable: bool,
}

fn default_complete() -> bool {
    true
}

#[derive(Serialize, Deserialize)]
pub struct RawPage {
    pub sort_by: String,
    /// Search scope; absent in archives made before supplemental direct-sale sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buyout_policy: Option<String>,
    pub body: String,
}

#[derive(Serialize, Deserialize)]
pub struct Verified {
    pub auction_id: String,
    pub exists: bool,
}

impl RawResponse {
    pub fn file_name(&self) -> String {
        format!(
            "{}-{}.json.gz",
            self.weapon,
            self.scraped_at.timestamp_millis()
        )
    }

    pub fn new(
        scraped_at: DateTime<Utc>,
        weapon: String,
        platform: String,
        pages: Vec<RawPage>,
        verified: Vec<Verified>,
    ) -> Self {
        Self {
            scraped_at,
            weapon,
            platform,
            pages,
            verified,
            body: None,
            complete: true,
            recoverable: true,
        }
    }

    pub fn read(path: &Path) -> Result<Self> {
        let mut json = String::new();
        GzDecoder::new(File::open(path)?).read_to_string(&mut json)?;
        let mut raw: Self =
            serde_json::from_str(&json).with_context(|| format!("parsing {}", path.display()))?;
        if let Some(body) = raw.body.take()
            && raw.pages.is_empty()
        {
            raw.pages.push(RawPage {
                buyout_policy: None,
                sort_by: "price_asc".into(),
                body,
            });
        }
        Ok(raw)
    }
}

/// Buffers rows and writes them as one parquet file per table per flush,
/// partitioned by scrape date.
pub struct ParquetSink {
    dir: PathBuf,
    date: Option<NaiveDate>,
    pub auctions: Vec<AuctionRow>,
    pub attributes: Vec<AttributeRow>,
    pub gone: Vec<GoneRow>,
    raw_files: Vec<String>,
    pending_id: Option<String>,
    checkpoints: HashMap<PathBuf, Vec<u8>>,
}

impl ParquetSink {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            date: None,
            auctions: Vec::new(),
            attributes: Vec::new(),
            gone: Vec::new(),
            raw_files: Vec::new(),
            pending_id: None,
            checkpoints: HashMap::new(),
        }
    }

    /// Must be called before pushing rows for `scraped_at`, so files never straddle a date.
    pub fn prepare(&mut self, scraped_at: DateTime<Utc>) -> Result<()> {
        // Resolve a failed publication before allowing any more rows into its buffer.
        if self.pending_id.is_some() {
            self.flush()?;
        }
        let date = scraped_at.date_naive();
        if self.date.is_some_and(|d| d != date) {
            self.flush()?;
        }
        self.date = Some(date);
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        let Some(date) = self.date else {
            return Ok(());
        };
        if self.auctions.is_empty()
            && self.attributes.is_empty()
            && self.gone.is_empty()
            && self.raw_files.is_empty()
            && self.checkpoints.is_empty()
        {
            return Ok(());
        }
        let root = self
            .dir
            .parent()
            .context("parquet directory needs a parent")?;
        let _lock = DatasetLock::write(root)?;
        let stamp = self.pending_id.get_or_insert_with(unique_id).clone();
        let mut tables = Vec::new();
        // Auctions and attributes are always written as a pair (the web UI joins them).
        if !self.auctions.is_empty() || !self.attributes.is_empty() {
            tables.push(("auctions", auctions_batch(&self.auctions)?));
            tables.push(("attributes", attributes_batch(&self.attributes)?));
        }
        if !self.gone.is_empty() {
            tables.push(("gone", gone_batch(&self.gone)?));
        }
        let receipt = PathBuf::from("raw")
            .join(date.to_string())
            .join(".committed")
            .join(format!("{stamp}.json"));
        let destinations: Vec<_> = tables
            .iter()
            .map(|(table, _)| {
                PathBuf::from("parquet")
                    .join(table)
                    .join(format!("date={date}"))
                    .join(format!("part-{stamp}.parquet"))
            })
            .collect();
        // A prior attempt may have failed after journaling. The lock recovered it, so don't
        // append the same buffered observations a second time.
        let published = root.join(&receipt).exists();
        if !published {
            let mut tx = Transaction::new(root)?;
            for ((_, batch), destination) in tables.into_iter().zip(destinations) {
                write_parquet(&tx.stage(destination), &batch)?;
            }
            for (path, bytes) in &self.checkpoints {
                fs::write(tx.stage(path.clone()), bytes)?;
            }
            // Also serves as an idempotent commit marker for --no-raw and empty fetches.
            fs::write(tx.stage(receipt), serde_json::to_vec(&self.raw_files)?)?;
            tx.commit()?;
        }
        tracing::info!(
            auctions = self.auctions.len(),
            attributes = self.attributes.len(),
            gone = self.gone.len(),
            %date,
            "wrote parquet"
        );
        self.auctions.clear();
        self.attributes.clear();
        self.gone.clear();
        self.raw_files.clear();
        self.checkpoints.clear();
        self.pending_id = None;
        Ok(())
    }

    pub fn checkpoint(&mut self, path: PathBuf, bytes: Vec<u8>) {
        self.checkpoints.insert(path, bytes);
    }

    /// Call only after the raw archive was saved and processing succeeded. The receipt is
    /// published in the same transaction as all three normalized tables, including empty fetches.
    pub fn acknowledge_raw(&mut self, raw: &RawResponse) {
        self.raw_files.push(raw.file_name());
    }
}

fn write_parquet(path: &Path, batch: &RecordBatch) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    let props = WriterProperties::builder()
        .set_compression(ParquetCompression::ZSTD(ZstdLevel::try_new(3)?))
        .build();
    let mut writer = ArrowWriter::try_new(File::create(&tmp)?, batch.schema(), Some(props))?;
    writer.write(batch)?;
    writer.close()?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn ts<'a>(it: impl Iterator<Item = Option<&'a DateTime<Utc>>>) -> ArrayRef {
    Arc::new(
        TimestampMillisecondArray::from(
            it.map(|t| t.map(DateTime::timestamp_millis))
                .collect::<Vec<_>>(),
        )
        .with_timezone("UTC"),
    )
}

fn strs<'a>(it: impl Iterator<Item = Option<&'a str>>) -> ArrayRef {
    Arc::new(StringArray::from(it.collect::<Vec<_>>()))
}

fn i64s(it: impl Iterator<Item = Option<i64>>) -> ArrayRef {
    Arc::new(Int64Array::from(it.collect::<Vec<_>>()))
}

fn i32s(it: impl Iterator<Item = Option<i32>>) -> ArrayRef {
    Arc::new(Int32Array::from(it.collect::<Vec<_>>()))
}

fn f64s(it: impl Iterator<Item = Option<f64>>) -> ArrayRef {
    Arc::new(Float64Array::from(it.collect::<Vec<_>>()))
}

fn bools(it: impl Iterator<Item = Option<bool>>) -> ArrayRef {
    Arc::new(BooleanArray::from(it.collect::<Vec<_>>()))
}

fn auctions_batch(rows: &[AuctionRow]) -> Result<RecordBatch> {
    let r = || rows.iter();
    Ok(RecordBatch::try_from_iter([
        ("scraped_at", ts(r().map(|x| Some(&x.scraped_at)))),
        ("platform", strs(r().map(|x| Some(x.platform.as_str())))),
        ("auction_id", strs(r().map(|x| Some(x.auction_id.as_str())))),
        ("riven_id", strs(r().map(|x| Some(x.riven_id.as_str())))),
        ("source", strs(r().map(|x| Some(x.source.as_str())))),
        (
            "weapon_slug",
            strs(r().map(|x| Some(x.weapon_slug.as_str()))),
        ),
        (
            "weapon_name",
            strs(r().map(|x| Some(x.weapon_name.as_str()))),
        ),
        ("weapon_class", strs(r().map(|x| x.weapon_class))),
        ("riven_type", strs(r().map(|x| Some(x.riven_type.as_str())))),
        ("is_bow", bools(r().map(|x| Some(x.is_bow)))),
        ("riven_name", strs(r().map(|x| Some(x.riven_name.as_str())))),
        ("polarity", strs(r().map(|x| Some(x.polarity.as_str())))),
        ("mastery_level", i64s(r().map(|x| x.mastery_level))),
        ("re_rolls", i64s(r().map(|x| x.re_rolls))),
        ("n_positive", i32s(r().map(|x| Some(x.n_positive)))),
        ("n_negative", i32s(r().map(|x| Some(x.n_negative)))),
        ("has_spliced", bools(r().map(|x| Some(x.has_spliced)))),
        ("listed_rank", i64s(r().map(|x| Some(x.listed_rank)))),
        ("rank", i32s(r().map(|x| x.rank))),
        ("disposition", f64s(r().map(|x| x.disposition))),
        ("legacy_stats", bools(r().map(|x| x.legacy_stats))),
        ("fit_ok", bools(r().map(|x| Some(x.fit_ok)))),
        ("fit_ambiguous", bools(r().map(|x| x.fit_ambiguous))),
        ("fit_error", strs(r().map(|x| x.fit_error.as_deref()))),
        (
            "mean_positive_quality",
            f64s(r().map(|x| x.mean_positive_quality)),
        ),
        ("buyout_price", i64s(r().map(|x| x.buyout_price))),
        ("starting_price", i64s(r().map(|x| x.starting_price))),
        ("top_bid", i64s(r().map(|x| x.top_bid))),
        ("is_direct_sell", bools(r().map(|x| Some(x.is_direct_sell)))),
        (
            "minimal_reputation",
            i64s(r().map(|x| x.minimal_reputation)),
        ),
        ("visible", bools(r().map(|x| Some(x.visible)))),
        ("closed", bools(r().map(|x| Some(x.closed)))),
        ("private", bools(r().map(|x| Some(x.private)))),
        ("crossplay", bools(r().map(|x| Some(x.crossplay)))),
        ("created", ts(r().map(|x| Some(&x.created)))),
        ("updated", ts(r().map(|x| Some(&x.updated)))),
        ("owner_id", strs(r().map(|x| Some(x.owner_id.as_str())))),
        ("owner_name", strs(r().map(|x| Some(x.owner_name.as_str())))),
        ("owner_reputation", i64s(r().map(|x| x.owner_reputation))),
        (
            "owner_status",
            strs(r().map(|x| Some(x.owner_status.as_str()))),
        ),
        (
            "owner_last_seen",
            ts(r().map(|x| x.owner_last_seen.as_ref())),
        ),
        ("note", strs(r().map(|x| Some(x.note.as_str())))),
    ])?)
}

fn attributes_batch(rows: &[AttributeRow]) -> Result<RecordBatch> {
    let r = || rows.iter();
    Ok(RecordBatch::try_from_iter([
        ("scraped_at", ts(r().map(|x| Some(&x.scraped_at)))),
        ("auction_id", strs(r().map(|x| Some(x.auction_id.as_str())))),
        (
            "weapon_slug",
            strs(r().map(|x| Some(x.weapon_slug.as_str()))),
        ),
        ("position", i32s(r().map(|x| Some(x.position)))),
        ("attribute", strs(r().map(|x| Some(x.attribute.as_str())))),
        ("positive", bools(r().map(|x| Some(x.positive)))),
        ("spliced", bools(r().map(|x| x.spliced))),
        ("value", f64s(r().map(|x| Some(x.value)))),
        ("value_rank8", f64s(r().map(|x| x.value_rank8))),
        ("roll", f64s(r().map(|x| x.roll))),
        ("quality", f64s(r().map(|x| x.quality))),
        ("coefficient", f64s(r().map(|x| x.coefficient))),
        ("layout_multiplier", f64s(r().map(|x| x.layout_multiplier))),
    ])?)
}

fn gone_batch(rows: &[GoneRow]) -> Result<RecordBatch> {
    let r = || rows.iter();
    Ok(RecordBatch::try_from_iter([
        ("detected_at", ts(r().map(|x| Some(&x.detected_at)))),
        (
            "weapon_slug",
            strs(r().map(|x| Some(x.weapon_slug.as_str()))),
        ),
        ("auction_id", strs(r().map(|x| Some(x.auction_id.as_str())))),
        (
            "riven_id",
            strs(r().map(|x| Some(x.seen.riven_id.as_str()))),
        ),
        ("reason", strs(r().map(|x| Some(x.reason.as_str())))),
        ("last_price", i64s(r().map(|x| x.seen.price))),
        ("direct_sell", bools(r().map(|x| Some(x.seen.direct_sell)))),
        (
            "owner_name",
            strs(r().map(|x| Some(x.seen.owner_name.as_str()))),
        ),
        ("created", ts(r().map(|x| Some(&x.seen.created)))),
        ("first_seen", ts(r().map(|x| Some(&x.seen.first_seen)))),
        ("last_seen", ts(r().map(|x| Some(&x.seen.last_seen)))),
    ])?)
}
