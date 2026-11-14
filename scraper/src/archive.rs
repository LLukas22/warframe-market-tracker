//! What the scraper keeps under the data directory besides the dataset itself (whose files,
//! including the raw archives' place, are listed in `wfmt_dataset::layout`):
//!
//! ```text
//! cache/riven_weapons.json             raw /v2/riven/weapons body
//! cache/meta.json                      riven manifest version + last check
//! state.json                           last fetch time per weapon (survives restarts)
//! seen/<weapon>.json                   auctions seen in the last fetch, for gone detection
//! raw/<date>/<weapon>-<ms>.json.gz     raw auction responses, for re-normalizing offline
//! ```

use wfmt_dataset::layout::{RAW_DIR, RECEIPTS_DIR};
use wfmt_types::{Feed, SortBy};

use crate::wfm::BuyoutPolicy;
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};

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
    pub queue: std::collections::VecDeque<String>,
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
        self.root.join(wfmt_dataset::layout::REGISTRY)
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
            .join(RAW_DIR)
            .join(raw.scraped_at.date_naive().to_string())
            .join(raw.file_name());
        let mut gz = GzEncoder::new(Vec::new(), Compression::default());
        serde_json::to_writer(&mut gz, raw)?;
        wfmt_dataset::durable_write(&path, &gz.finish()?)
    }

    /// New-format archives without a committed output receipt, in observation order.
    pub fn pending_raw(&self) -> Result<Vec<RawResponse>> {
        let mut committed = std::collections::HashSet::new();
        let raw_dir = self.root.join(RAW_DIR);
        if !raw_dir.exists() {
            return Ok(Vec::new());
        }
        for day in fs::read_dir(&raw_dir)? {
            let receipts = day?.path().join(RECEIPTS_DIR);
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
        let dir = self.root.join(RAW_DIR);
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
    /// Named `sort_by` in archives since feeds were added; a feed page has its feed's id.
    pub sort_by: PageKind,
    /// Search scope; absent in archives made before supplemental direct-sale sampling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buyout_policy: Option<BuyoutPolicy>,
    pub body: String,
}

/// What a raw page came from. Stored as the plain name of either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PageKind {
    /// A weapon's search, in this order.
    Search(SortBy),
    Feed(Feed),
}

impl PageKind {
    /// How rows name the page they were seen on (their `source`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Search(order) => order.as_str(),
            Self::Feed(feed) => feed.as_str(),
        }
    }
}

impl From<SortBy> for PageKind {
    fn from(order: SortBy) -> Self {
        Self::Search(order)
    }
}

impl From<Feed> for PageKind {
    fn from(feed: Feed) -> Self {
        Self::Feed(feed)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Verified {
    pub auction_id: String,
    pub exists: bool,
}

impl RawResponse {
    /// The feed this is a poll of, `None` for a weapon's search.
    pub fn feed(&self) -> Option<Feed> {
        Feed::from_id(&self.weapon)
    }

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
                sort_by: SortBy::PriceAsc.into(),
                body,
            });
        }
        Ok(raw)
    }
}
