//! Types shared between browser and server, plus the server functions.

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Sort {
    #[default]
    PriceAsc,
    PriceDesc,
    QualityDesc,
    Newest,
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
    /// Minimum mean roll quality of the positive attributes, 0..1.
    pub min_quality: f64,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub sort: Sort,
    /// Width of the price history buckets.
    pub bucket_hours: u32,
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
            min_quality: 0.0,
            min_price: Some(10),
            max_price: None,
            sort: Sort::PriceAsc,
            bucket_hours: 24,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WeaponInfo {
    pub slug: String,
    pub name: String,
    pub class: String,
    /// warframe.market group: primary, secondary, melee, archgun, zaw, kitgun, sentinel.
    pub group: String,
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
    /// Weapon classes this attribute has been seen on.
    pub classes: Vec<String>,
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

#[get("/api/overview")]
pub async fn overview() -> Result<Overview, ServerFnError> {
    crate::data::run(|store| store.overview()).await
}

#[post("/api/search")]
pub async fn search(query: Query) -> Result<SearchResult, ServerFnError> {
    crate::data::run(move |store| store.search(&query)).await
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RivenDetail {
    pub listing: Listing,
    pub similar: Vec<SimilarRiven>,
    pub similar_total: usize,
    pub comparisons_hidden: usize,
    pub server_time: i64,
}

#[post("/api/riven-detail")]
pub async fn riven_detail(
    id: String,
    auction_id: Option<String>,
    include_legacy: bool,
    include_unfit: bool,
    include_loose: bool,
) -> Result<Option<RivenDetail>, ServerFnError> {
    crate::data::run(move |store| {
        store.riven_detail(
            &id,
            auction_id.as_deref(),
            include_legacy,
            include_unfit,
            include_loose,
        )
    })
    .await
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

/// The riven a scan was matched to, if it has been listed before.
#[post("/api/scan/match")]
pub async fn scan_match(scan: RivenScan) -> Result<Option<String>, ServerFnError> {
    crate::data::run(move |store| store.scan_match(&scan)).await
}

/// The riven page for a scanned riven that isn't in the data: the scan graded by the stat
/// model, plus similar rivens. `None` for an unknown weapon.
#[post("/api/scan/detail")]
pub async fn scanned_detail(
    scan: RivenScan,
    include_legacy: bool,
    include_unfit: bool,
    include_loose: bool,
) -> Result<Option<RivenDetail>, ServerFnError> {
    crate::data::run(move |store| {
        store.scanned_detail(&scan, include_legacy, include_unfit, include_loose)
    })
    .await
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

#[post("/api/stat-effects")]
pub async fn stat_effects(weapon: String) -> Result<StatEffects, ServerFnError> {
    crate::data::run(move |store| store.stat_effects(&weapon)).await
}

#[get("/api/riven/{id}")]
pub async fn riven_history(id: String) -> Result<Vec<RivenAuction>, ServerFnError> {
    crate::data::run(move |store| store.riven_history(&id)).await
}

/// Human readable attribute name for a warframe.market slug.
pub fn attribute_name(slug: &str) -> String {
    let name = match slug {
        "chance_to_gain_extra_combo_count" => "Additional Combo Count Chance",
        "chance_to_gain_combo_count" => "Chance to Gain Combo Count",
        "ammo_maximum" => "Ammo Maximum",
        "damage_vs_corpus" => "Damage to Corpus",
        "damage_vs_grineer" => "Damage to Grineer",
        "damage_vs_infested" => "Damage to Infested",
        "cold_damage" => "Cold",
        "combo_duration" => "Combo Duration",
        "critical_chance" => "Critical Chance",
        "critical_chance_on_slide_attack" => "Critical Chance on Slide Attack",
        "critical_damage" => "Critical Damage",
        "base_damage_/_melee_damage" => "Damage",
        "electric_damage" => "Electricity",
        "heat_damage" => "Heat",
        "finisher_damage" => "Finisher Damage",
        "fire_rate_/_attack_speed" => "Fire Rate / Attack Speed",
        "projectile_speed" => "Projectile Speed",
        "channeling_damage" => "Initial Combo",
        "impact_damage" => "Impact",
        "magazine_capacity" => "Magazine Capacity",
        "channeling_efficiency" => "Heavy Attack Efficiency",
        "multishot" => "Multishot",
        "toxin_damage" => "Toxin",
        "punch_through" => "Punch Through",
        "puncture_damage" => "Puncture",
        "reload_speed" => "Reload Speed",
        "range" => "Range",
        "slash_damage" => "Slash",
        "status_chance" => "Status Chance",
        "status_duration" => "Status Duration",
        "recoil" => "Weapon Recoil",
        "zoom" => "Zoom",
        "ammo_efficiency" => "Ammo Efficiency",
        "blast" => "Blast",
        "corrosive" => "Corrosive",
        "damage_to_orokin" => "Damage to Orokin",
        "damage_to_scaldra" => "Damage to Scaldra",
        "damage_to_techrot" => "Damage to Techrot",
        "gas" => "Gas",
        "heavy_attack_wind_up_speed" => "Heavy Attack Wind Up Speed",
        "magnetic" => "Magnetic",
        "melee_damage_on_heavy_attack" => "Melee Damage on Heavy Attack",
        "parry_angle" => "Parry Angle",
        "radiation" => "Radiation",
        "magazine_reloaded_s_when_holstered" => "Reload While Holstered",
        "slam_attack_damage" => "Slam Attack Damage",
        "status_damage" => "Status Damage",
        "viral" => "Viral",
        "weak_point_damage" => "Weak Point Damage",
        "weak_point_critical_chance" => "Weak Point Critical Chance",
        other => return other.replace('_', " "),
    };
    name.to_owned()
}

/// Formats a listed value the way it shows in game.
pub fn format_value(slug: &str, value: f64) -> String {
    match slug {
        s if s.starts_with("damage_vs_") || s.starts_with("damage_to_") => format!("x{value:.2}"),
        "punch_through" | "range" => format!("{value:+.1}m"),
        "combo_duration" => format!("{value:+.1}s"),
        "channeling_damage" => format!("{value:+.1}"),
        _ => format!("{value:+.1}%"),
    }
}

/// Whether a search box input looks like a riven id (or a prefix of one).
pub fn looks_like_riven_id(text: &str) -> bool {
    let t = text.trim();
    (6..=16).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Cheap enough to poll: what the header shows, and when to fetch market data again.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LiveStats {
    pub auctions: u32,
    /// Changes whenever new scraped data has been loaded.
    pub version: u64,
    pub scraping: bool,
}

#[get("/api/live")]
pub async fn live_stats() -> Result<LiveStats, ServerFnError> {
    let (auctions, version) = crate::data::run(|store| store.live()).await?;
    let dir = crate::data::data_dir();
    let running = wfmt_scraper::engine::try_lock(&dir)
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .is_none();
    let scraping =
        running && wfmt_scraper::engine::read_status(&dir).is_some_and(|s| s.sweep.is_some());
    Ok(LiveStats {
        auctions,
        version,
        scraping,
    })
}

#[get("/api/scraper")]
pub async fn scraper_status() -> Result<ScraperSnapshot, ServerFnError> {
    let dir = crate::data::data_dir();
    let running = wfmt_scraper::engine::try_lock(&dir)
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .is_none();
    Ok(ScraperSnapshot {
        status: wfmt_scraper::engine::read_status(&dir),
        running,
        admin: crate::admin::enabled(),
        server_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    })
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScraperSnapshot {
    pub status: Option<wfmt_types::ScraperStatus>,
    pub running: bool,
    /// The server has an admin token, so scrapes can be controlled and data moved.
    pub admin: bool,
    pub server_time: i64,
}

#[post("/api/scraper/control")]
pub async fn scraper_control(cancel: bool, token: String) -> Result<(), ServerFnError> {
    crate::admin::check(&token)
        .await
        .map_err(|denied| ServerFnError::ServerError {
            message: denied.message().into(),
            code: denied.status().as_u16(),
            details: None,
        })?;
    let dir = crate::data::data_dir();
    if wfmt_scraper::engine::try_lock(&dir)
        .map_err(|e| ServerFnError::new(e.to_string()))?
        .is_some()
    {
        return Err(ServerFnError::new("The scraper isn't running right now."));
    }
    let command = if cancel {
        wfmt_types::Command::Cancel
    } else {
        wfmt_types::Command::ScrapeAll
    };
    wfmt_scraper::engine::send_command(&dir, &command)
        .map_err(|e| ServerFnError::new(e.to_string()))
}
