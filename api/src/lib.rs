//! What the browser and the server exchange: queries, the answers to them, and the small
//! helpers both sides use to show them. Builds for the browser (no server dependencies).

use serde::{Deserialize, Serialize};
use wfmt_types::{WeaponClass, WeaponGroup};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum CurseFilter {
    /// Don't care.
    #[default]
    Any,
    /// No negative attribute.
    None,
    /// Some negative attribute.
    Required,
    /// This specific negative attribute.
    Attr(String),
}

impl CurseFilter {
    /// How a `<select>` names it.
    pub fn key(&self) -> String {
        match self {
            Self::Any => "any".into(),
            Self::None => "none".into(),
            Self::Required => "required".into(),
            Self::Attr(slug) => format!("attr:{slug}"),
        }
    }

    /// The inverse of [`CurseFilter::key`]; anything else means `Any`.
    pub fn from_key(key: &str) -> Self {
        match key {
            "none" => Self::None,
            "required" => Self::Required,
            _ => key
                .strip_prefix("attr:")
                .map_or(Self::Any, |slug| Self::Attr(slug.to_owned())),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Sort {
    #[default]
    PriceAsc,
    PriceDesc,
    QualityDesc,
    Newest,
}

impl Sort {
    pub const ALL: [Self; 4] = [
        Self::PriceAsc,
        Self::PriceDesc,
        Self::QualityDesc,
        Self::Newest,
    ];

    /// How a `<select>` names it.
    pub fn key(self) -> &'static str {
        match self {
            Self::PriceAsc => "price_asc",
            Self::PriceDesc => "price_desc",
            Self::QualityDesc => "quality",
            Self::Newest => "newest",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.key() == key)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// Show every auction of the riven(s) whose id starts with this, ignoring all other filters.
    pub riven_id: Option<String>,
    pub weapon: Option<String>,
    /// All of these must be present as positive attributes.
    pub positives: Vec<String>,
    pub curse: CurseFilter,
    /// Only direct sells (fixed buyout price), no bidding auctions.
    pub direct_only: bool,
    /// Include listings whose stats are still from before the disposition rework.
    pub include_legacy: bool,
    /// Include listings whose stats could not be fitted (placeholders, typos).
    pub include_unfit: bool,
    /// List rivens with invalid prices too. Charts and medians never count them.
    pub include_outliers: bool,
    /// The same riven has been observed at two different known prices.
    #[serde(default)]
    pub price_changed: bool,
    /// The same riven has appeared under different nonempty seller names.
    #[serde(default)]
    pub owner_changed: bool,
    /// Minimum mean roll quality of the positive attributes, 0..1.
    pub min_quality: f64,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub sort: Sort,
    /// The stretch of time covered by the price chart and the disappeared listings.
    #[serde(default)]
    pub period: Period,
}

pub use wfmt_types::{DAY_MS, HOUR_MS, WEEK_MS};

/// Asking prices below this aren't real offers (a 1p starting bid, say), so price statistics
/// and the default search leave them out.
pub const MIN_PRICE: i64 = 10;

/// Days are UTC, like every date the app shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Period {
    /// Since midnight.
    Today,
    /// The last 7 days.
    #[default]
    Week,
    /// The last 30 days.
    Month,
    All,
    /// Whole days, both inclusive, as the unix ms of their midnight. `None` is open-ended.
    Custom {
        from: Option<i64>,
        to: Option<i64>,
    },
}

impl Period {
    /// Half-open `[start, end)` in unix ms; `None` is unbounded.
    pub fn bounds(self, now: i64) -> (Option<i64>, Option<i64>) {
        match self {
            Period::Today => (Some(now.saturating_sub(now.rem_euclid(DAY_MS))), None),
            Period::Week => (Some(now.saturating_sub(WEEK_MS)), None),
            Period::Month => (Some(now.saturating_sub(30 * DAY_MS)), None),
            Period::All => (None, None),
            Period::Custom { from, to } => (from, to.map(|t| t.saturating_add(DAY_MS))),
        }
    }

    /// Rejects ranges a request could send but no form makes.
    pub fn validate(self) -> Result<(), &'static str> {
        if let Self::Custom { from, to } = self {
            if to.is_some_and(|t| t.checked_add(DAY_MS).is_none()) {
                return Err("The end date is out of range");
            }
            if matches!((from, to), (Some(f), Some(t)) if f > t) {
                return Err("The start date must not follow the end date");
            }
        }
        Ok(())
    }

    /// An open custom range is just "all time", and a reversed one is put the right way round.
    pub fn custom(from: Option<i64>, to: Option<i64>) -> Self {
        match (from, to) {
            (None, None) => Period::All,
            (Some(f), Some(t)) if f > t => Period::Custom {
                from: Some(t),
                to: Some(f),
            },
            _ => Period::Custom { from, to },
        }
    }
}

impl Default for Query {
    fn default() -> Self {
        Self {
            riven_id: None,
            weapon: None,
            positives: Vec::new(),
            curse: CurseFilter::Any,
            direct_only: true,
            include_legacy: false,
            include_unfit: false,
            include_outliers: false,
            price_changed: false,
            owner_changed: false,
            min_quality: 0.0,
            min_price: Some(MIN_PRICE),
            max_price: None,
            sort: Sort::PriceAsc,
            period: Period::Week,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeaponInfo {
    pub slug: String,
    pub name: String,
    /// `None` for a weapon type the stat model doesn't know.
    pub class: Option<WeaponClass>,
    pub group: WeaponGroup,
    pub has_image: bool,
    /// Listings seen in the most recent scrape of this weapon (0 = not scraped yet).
    pub listings: u32,
    /// Median / cheapest buyout of verified, post-rework listings of at least 10p.
    pub median_price: Option<i64>,
    pub min_price: Option<i64>,
    /// Verified listings that disappeared (sold or removed) in the last 7 days.
    pub gone_7d: u32,
    /// Median last price of those.
    pub gone_median_7d: Option<i64>,
    pub last_scrape: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttrInfo {
    pub slug: String,
    pub name: String,
    /// Weapon classes this attribute has been seen on, `None` for weapons without one.
    pub classes: Vec<Option<WeaponClass>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Overview {
    pub weapons: Vec<WeaponInfo>,
    pub attributes: Vec<AttrInfo>,
    pub auctions: u32,
    pub observations: u32,
    pub last_scrape: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AttrView {
    pub slug: String,
    pub positive: bool,
    pub value: f64,
    pub value_rank8: Option<f64>,
    pub quality: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Listing {
    pub auction_id: String,
    /// Stable identity of the riven across relists and resales.
    pub riven_id: String,
    /// The price is far above this weapon's normal high end (a joke or placeholder price).
    pub price_outlier: bool,
    /// Prices above this count as invalid for this weapon.
    pub price_limit: Option<i64>,
    /// Auctions this riven has been seen in (relists, resales, duplicates).
    pub copies: u32,
    /// Distinct sellers that listed this riven.
    pub sellers: u32,
    /// When the auction disappeared (sold or removed), if it did.
    pub gone_at: Option<i64>,
    pub gone_reason: Option<String>,
    /// Last actual search observation; prices may have changed since then.
    pub last_seen: i64,
    pub unconfirmed: bool,
    pub weapon_slug: String,
    pub weapon_name: String,
    pub riven_name: String,
    pub price: Option<i64>,
    pub buyout_price: Option<i64>,
    pub starting_price: Option<i64>,
    pub top_bid: Option<i64>,
    pub direct_sell: bool,
    pub rank: Option<i32>,
    pub re_rolls: Option<i64>,
    pub mastery_level: Option<i64>,
    pub polarity: String,
    pub legacy_stats: Option<bool>,
    pub fit_ok: bool,
    pub mean_quality: Option<f64>,
    pub owner_name: String,
    pub owner_reputation: Option<i64>,
    pub created: i64,
    pub first_seen: i64,
    pub attributes: Vec<AttrView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HistoryPoint {
    /// Bucket start, unix ms.
    pub t: i64,
    pub count: u32,
    pub min: i64,
    pub p25: i64,
    pub median: i64,
    pub p75: i64,
    /// Matching listings that disappeared in this bucket (≈ sold) and their median last price.
    pub gone_count: u32,
    pub gone_median: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchResult {
    /// Current listings, one per riven (the cheapest copy).
    pub listings: Vec<Listing>,
    pub total: usize,
    /// Most recently disappeared matching listings, newest first.
    pub gone: Vec<Listing>,
    pub gone_total: usize,
    /// Current rivens left out because of an invalid price.
    pub outliers_hidden: usize,
    /// The invalid price limit of the searched weapon.
    pub price_limit: Option<i64>,
    pub history: Vec<HistoryPoint>,
    /// Width of the history buckets, chosen from the length of the period.
    pub bucket_hours: u32,
    pub server_time: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PricePoint {
    pub t: i64,
    pub price: Option<i64>,
}

/// One auction of a riven, for its listing history.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RivenAuction {
    pub auction_id: String,
    pub owner_name: String,
    pub created: i64,
    pub first_seen: i64,
    pub last_seen: i64,
    pub gone_at: Option<i64>,
    pub gone_reason: Option<String>,
    pub points: Vec<PricePoint>,
}

/// A distinct historical riven, represented by its latest eligible listing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SimilarRiven {
    pub listing: Listing,
    pub same_stats: bool,
    pub shared_positives: usize,
    pub missing_positives: Vec<String>,
    pub added_positives: Vec<String>,
    pub same_negative: bool,
    /// Mean absolute quality difference on shared signed stats, 0..1.
    /// None unless both listings are fitted and every compared stat has quality.
    pub roll_distance: Option<f64>,
}

/// How many of the closest similar rivens a riven's page gets.
pub const SIMILAR_LIMIT: usize = 10;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RivenDetail {
    pub listing: Listing,
    pub similar: Vec<SimilarRiven>,
    pub similar_total: usize,
    pub comparisons_hidden: usize,
    pub server_time: i64,
}

/// Which other rivens a riven page compares with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparisons {
    /// Include listings whose stats are still from before the disposition rework.
    pub legacy: bool,
    /// Include listings whose stats could not be fitted.
    pub unfit: bool,
    /// Include rivens that share a single positive stat; otherwise two are needed.
    pub loose: bool,
}

/// One stat read from a screenshot, as warframe.market lists it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScanAttr {
    pub slug: String,
    pub value: f64,
    pub positive: bool,
}

/// A riven read from a screenshot by the OCR module (`ocr/`). Deserializes its JSON directly.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RivenScan {
    #[serde(default)]
    pub weapon: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mastery_rank: Option<i64>,
    /// `None` when the card shows no count, which means it was never rerolled.
    #[serde(default)]
    pub rerolls: Option<i64>,
    #[serde(default)]
    pub attributes: Vec<ScanAttr>,
    /// Shown with the scan; not part of the page URL.
    #[serde(default, skip_serializing)]
    pub warnings: Vec<String>,
}

impl RivenScan {
    /// Compact form for the page URL: `weapon~name~mr~rerolls~pslug:value,nslug:value`. The
    /// router decodes the whole query before splitting it, so this avoids `&`, `=` and `%`.
    pub fn to_param(&self) -> String {
        let attrs: Vec<String> = self
            .attributes
            .iter()
            .map(|a| {
                format!(
                    "{}{}:{}",
                    if a.positive { 'p' } else { 'n' },
                    a.slug,
                    a.value
                )
            })
            .collect();
        let clean = |s: &Option<String>| {
            s.as_deref()
                .unwrap_or("")
                .chars()
                .filter(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
                .collect::<String>()
        };
        let number = |n: Option<i64>| n.map(|n| n.to_string()).unwrap_or_default();
        format!(
            "{}~{}~{}~{}~{}",
            clean(&self.weapon),
            clean(&self.name),
            number(self.mastery_rank),
            number(self.rerolls),
            attrs.join(",")
        )
    }

    pub fn from_param(param: &str) -> Option<Self> {
        let mut parts = param.split('~');
        let mut text = || parts.next().map(str::to_owned);
        let (weapon, name, mastery, rerolls, attrs) = (text()?, text()?, text()?, text()?, text()?);
        let non_empty = |s: String| (!s.is_empty()).then_some(s);
        let attributes = attrs
            .split(',')
            .filter(|a| !a.is_empty())
            .map(|a| {
                let (sign, rest) = a.split_at_checked(1)?;
                let (slug, value) = rest.rsplit_once(':')?;
                Some(ScanAttr {
                    slug: slug.to_owned(),
                    value: value.parse().ok()?,
                    positive: match sign {
                        "p" => true,
                        "n" => false,
                        _ => return None,
                    },
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            weapon: non_empty(weapon),
            name: non_empty(name),
            mastery_rank: mastery.parse().ok(),
            rerolls: rerolls.parse().ok(),
            attributes,
            warnings: Vec::new(),
        })
    }
}

/// How one stat moves the price of a weapon's rivens, compared with rivens without it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatEffect {
    pub slug: String,
    /// Price change as a fraction: 1.0 is +100%, -0.5 is half the price.
    pub effect: f64,
    /// Approximate model uncertainty, not a calibrated interval for an isolated effect.
    pub low: f64,
    pub high: f64,
    /// Rivens with this stat.
    pub rivens: u32,
    /// Individually identifiable in the observed design, with a nonzero model estimate.
    pub certain: bool,
}

/// Below this many verified rivens there's too little to tell which stats move the price.
pub const EFFECT_MIN_RIVENS: usize = 30;
/// Stats on fewer rivens than this are left out of the estimate.
pub const EFFECT_MIN_STAT_RIVENS: u32 = 5;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StatEffects {
    /// Rivens the estimate is based on.
    pub rivens: u32,
    pub low_price: Option<i64>,
    pub high_price: Option<i64>,
    /// Share of price differences the stats explain on held-out rivens.
    pub explained: Option<f64>,
    /// Both empty when the stats don't explain prices well enough to show anything.
    pub positive: Vec<StatEffect>,
    pub negative: Vec<StatEffect>,
    /// Stats left out for appearing on too few rivens.
    pub rare_stats: u32,
}

pub use wfmt_types::attributes::{format_value, name_of as attribute_name};

/// The values a stat could have had on this riven: worst roll to best, or for a negative,
/// mildest to harshest. A roll scales the stat's base value by `ROLL_MIN` to `ROLL_MAX`, and
/// `quality` says where in that range this one landed, so the range follows from the listed value.
pub fn roll_range(slug: &str, value: f64, quality: f64) -> (f64, f64) {
    use wfmt_types::riven::{ROLL_MAX, ROLL_MIN};
    let roll = ROLL_MIN + (ROLL_MAX - ROLL_MIN) * quality.clamp(0.0, 1.0);
    let at = |w: f64| {
        if wfmt_types::attributes::unit_of(slug) == wfmt_types::Unit::Multiply && value > 0.0 {
            1.0 + (value - 1.0) * w / roll
        } else {
            value * w / roll
        }
    };
    (at(ROLL_MIN), at(ROLL_MAX))
}

/// Riven ids are this many hex digits (see the scraper's identity registry).
pub const RIVEN_ID_LEN: usize = 16;
/// Shorter prefixes match too many rivens to be worth searching for.
const MIN_RIVEN_ID_PREFIX: usize = 6;

/// Whether a search box input looks like a riven id (or a prefix of one).
pub fn looks_like_riven_id(text: &str) -> bool {
    let t = text.trim();
    (MIN_RIVEN_ID_PREFIX..=RIVEN_ID_LEN).contains(&t.len())
        && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Cheap enough to poll: what the header shows, and when to fetch market data again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveStats {
    pub auctions: u32,
    /// Changes whenever new scraped data has been loaded.
    pub version: u64,
    pub scraping: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScraperSnapshot {
    pub status: Option<wfmt_types::ScraperStatus>,
    pub running: bool,
    /// The server has an admin token, so scrapes can be controlled and data moved.
    pub admin: bool,
    /// `WFMT_ENABLE_SCRAPER` isn't set: this server only hosts its data.
    pub disabled: bool,
    pub hub: HubStatus,
    pub server_time: i64,
}

/// Uploads of finished days to a Hugging Face dataset, or downloads of them when `pull` is set.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HubStatus {
    /// `owner/name`, when uploads or downloads are set up.
    pub dataset: Option<String>,
    /// The server downloads its data from the dataset instead of uploading to it.
    pub pull: bool,
    pub running: bool,
    /// A check was requested and starts after the current one.
    pub queued: bool,
    /// The last upload, or when pulling, the last download that changed anything.
    pub last_upload_at: Option<i64>,
    /// The newest day on the Hub.
    pub last_day: Option<String>,
    /// The commit last uploaded, or downloaded from.
    pub commit_url: Option<String>,
    pub last_check_at: Option<i64>,
    /// What the last check did.
    pub outcome: Option<String>,
    pub error: Option<String>,
}

#[cfg(test)]
mod period_tests {
    use super::*;

    #[test]
    fn deserialized_ranges_are_validated_and_arithmetic_never_overflows() {
        let period: Period = serde_json::from_value(serde_json::json!({
            "Custom": { "from": null, "to": i64::MAX }
        }))
        .unwrap();
        assert!(period.validate().is_err());
        assert_eq!(period.bounds(0).1, Some(i64::MAX));
        assert!(
            Period::Custom {
                from: Some(2),
                to: Some(1)
            }
            .validate()
            .is_err()
        );
        let valid = Period::Custom {
            from: Some(0),
            to: Some(DAY_MS),
        };
        assert!(valid.validate().is_ok());
        assert_eq!(valid.bounds(0), (Some(0), Some(2 * DAY_MS)));
        for period in [Period::Today, Period::Week, Period::Month] {
            let _ = period.bounds(i64::MIN);
        }
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn select_keys_round_trip() {
        for sort in Sort::ALL {
            assert_eq!(Sort::from_key(sort.key()), Some(sort));
        }
        for curse in [
            CurseFilter::Any,
            CurseFilter::None,
            CurseFilter::Required,
            CurseFilter::Attr("zoom".into()),
        ] {
            assert_eq!(CurseFilter::from_key(&curse.key()), curse);
        }
    }
}
