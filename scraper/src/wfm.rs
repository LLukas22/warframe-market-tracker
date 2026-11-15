//! Minimal warframe.market client.
//!
//! The API allows ~3 requests/s but punishes bursts, so every request goes through a single
//! sequential client that keeps a fixed gap between calls and backs off on 429/5xx.

use std::time::{Duration, Instant};
use wfmt_types::{Feed, RivenType, SortBy, WeaponClass, WeaponGroup};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

const API: &str = "https://api.warframe.market";
const MAX_ATTEMPTS: u32 = 6;

pub struct Client {
    http: reqwest::Client,
    api: String,
    platform: String,
    min_interval: Duration,
    last_request: Option<Instant>,
    requests: u64,
}

impl Client {
    pub fn new(platform: String, min_interval: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(wfmt_types::USER_AGENT)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            api: API.to_owned(),
            platform,
            min_interval,
            last_request: None,
            requests: 0,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_api(&mut self, api: String) {
        self.api = api;
    }

    /// Requests sent so far (including retries).
    pub fn requests(&self) -> u64 {
        self.requests
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    async fn get(&mut self, path: &str) -> Result<String> {
        let (status, body) = self.request(path).await?;
        if !status.is_success() {
            bail!("GET {API}{path} returned {status}");
        }
        Ok(body)
    }

    /// Like `get`, but returns client errors (4xx except 429) instead of failing.
    async fn request(&mut self, path: &str) -> Result<(StatusCode, String)> {
        let url = format!("{}{path}", self.api);
        let mut backoff = Duration::from_secs(30);
        for attempt in 1..=MAX_ATTEMPTS {
            if let Some(last) = self.last_request {
                tokio::time::sleep_until((last + self.min_interval).into()).await;
            }
            self.last_request = Some(Instant::now());
            self.requests += 1;

            let response = self
                .http
                .get(&url)
                .header("Accept", "application/json")
                .header("Platform", &self.platform)
                .header("Language", "en")
                .send()
                .await;

            let wait = match response {
                Ok(r)
                    if r.status().is_success()
                        || r.status().is_client_error()
                            && r.status() != StatusCode::TOO_MANY_REQUESTS =>
                {
                    let status = r.status();
                    return Ok((status, r.text().await?));
                }
                Ok(r)
                    if r.status() == StatusCode::TOO_MANY_REQUESTS
                        || r.status().is_server_error() =>
                {
                    let retry_after = r
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    tracing::warn!(status = %r.status(), attempt, %url, "backing off");
                    retry_after.unwrap_or(backoff)
                }
                Ok(r) => bail!("GET {url} returned {}", r.status()),
                Err(e) => {
                    tracing::warn!(error = %e, attempt, %url, "request failed");
                    backoff
                }
            };
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(Duration::from_secs(15 * 60));
        }
        bail!("GET {url} failed after {MAX_ATTEMPTS} attempts")
    }

    /// Opaque version string of the riven manifest; changes when weapons/attributes change.
    pub async fn rivens_version(&mut self) -> Result<String> {
        let body = self.get("/v2/versions").await?;
        let v: Envelope<Versions> = serde_json::from_str(&body).context("parsing /v2/versions")?;
        Ok(v.data.collections.rivens)
    }

    /// Raw `/v2/riven/weapons` body (kept verbatim so it can be cached).
    pub async fn riven_weapons_raw(&mut self) -> Result<String> {
        self.get("/v2/riven/weapons").await
    }

    /// Whether an auction still exists. warframe.market deletes sold and removed auctions and
    /// answers `app.form.not_exist` for them.
    pub async fn auction_exists(&mut self, id: &str) -> Result<bool> {
        let (status, body) = self.request(&format!("/v1/auctions/entry/{id}")).await?;
        entry_status(id, status, &body).map(|entry| matches!(entry, Entry::Listed))
    }

    /// Raw riven auction search body for one weapon.
    pub async fn auctions_raw(&mut self, weapon: &str, sort_by: SortBy) -> Result<String> {
        self.auctions_raw_scoped(weapon, sort_by, None).await
    }

    /// Filtered pages supplement the ordinary search; they cannot establish full coverage.
    pub async fn auctions_raw_scoped(
        &mut self,
        weapon: &str,
        sort_by: SortBy,
        buyout_policy: Option<BuyoutPolicy>,
    ) -> Result<String> {
        let mut path =
            format!("/v1/auctions/search?type=riven&weapon_url_name={weapon}&sort_by={sort_by}");
        if let Some(policy) = buyout_policy {
            path.push_str(&format!("&buyout_policy={}", policy.as_str()));
        }
        self.get(&path).await
    }

    /// Raw body of a site-wide feed. Same shape as a search, but mixes weapons and item types.
    pub async fn feed_raw(&mut self, feed: Feed) -> Result<String> {
        let path = match feed {
            Feed::Recent => "/v1/auctions",
            Feed::Popular => "/v1/auctions/popular",
            Feed::Check => bail!("listing checks aren't polled"),
        };
        self.get(path).await
    }
}

/// Which auctions a search returns, besides all of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuyoutPolicy {
    /// Direct sales: a fixed price, no bidding.
    Direct,
    /// Bidding auctions that also have a buyout price.
    With,
}

impl BuyoutPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::With => "with",
        }
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

#[derive(Deserialize)]
struct Versions {
    collections: Collections,
}

#[derive(Deserialize)]
struct Collections {
    rivens: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Weapon {
    pub slug: String,
    #[serde(default)]
    pub game_ref: String,
    #[serde(default)]
    pub group: WeaponGroup,
    pub riven_type: RivenType,
    /// Stale on warframe.market since the rework; only useful as the legacy value.
    #[serde(default)]
    pub disposition: Option<f64>,
    #[serde(default)]
    pub i18n: serde_json::Value,
}

impl Weapon {
    pub fn name(&self) -> &str {
        self.i18n["en"]["name"].as_str().unwrap_or(&self.slug)
    }

    pub fn is_bow(&self) -> bool {
        wfmt_types::market::is_bow(self.riven_type, &self.game_ref)
    }

    /// `None` for a riven type the stat model doesn't know.
    pub fn class(&self) -> Option<WeaponClass> {
        WeaponClass::of(self.riven_type, self.group)
    }
}

pub fn parse_weapons(body: &str) -> Result<Vec<Weapon>> {
    let v: Envelope<Vec<Weapon>> = serde_json::from_str(body).context("parsing riven weapons")?;
    Ok(v.data)
}

#[derive(Clone, Debug, Deserialize)]
pub struct Auction {
    pub id: String,
    #[serde(default)]
    pub starting_price: Option<i64>,
    #[serde(default)]
    pub buyout_price: Option<i64>,
    #[serde(default)]
    pub top_bid: Option<serde_json::Value>,
    #[serde(default)]
    pub minimal_reputation: Option<i64>,
    #[serde(default)]
    pub is_direct_sell: bool,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub crossplay: bool,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub note: String,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub owner: Owner,
    pub item: Riven,
}

impl Auction {
    pub fn top_bid(&self) -> Option<i64> {
        self.top_bid.as_ref().and_then(|v| v.as_i64())
    }

    /// A positive seller buyout, otherwise an actual positive bid (excluding starting prices).
    pub fn market_price(&self) -> Option<i64> {
        self.buyout_price
            .filter(|p| *p > 0)
            .or_else(|| self.top_bid().filter(|p| *p > 0))
    }

    /// The price a buyer pays: buyout, else the current bid, else the starting bid.
    pub fn price(&self) -> Option<i64> {
        self.buyout_price.or(self.top_bid()).or(self.starting_price)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Owner {
    pub id: String,
    #[serde(default)]
    pub ingame_name: String,
    #[serde(default)]
    pub reputation: Option<i64>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub last_seen: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Riven {
    #[serde(default)]
    pub weapon_url_name: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub re_rolls: Option<i64>,
    #[serde(default)]
    pub mod_rank: i64,
    #[serde(default)]
    pub mastery_level: Option<i64>,
    #[serde(default)]
    pub polarity: String,
    pub attributes: Vec<RivenAttribute>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RivenAttribute {
    pub url_name: String,
    pub value: f64,
    pub positive: bool,
}

#[derive(Deserialize)]
struct AuctionsBody {
    payload: AuctionsPayload,
}

#[derive(Deserialize)]
struct AuctionsPayload {
    auctions: Vec<serde_json::Value>,
}

/// What the entry endpoint says about one auction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Still for sale.
    Listed,
    /// Sold, removed, closed or hidden.
    Gone,
}

/// Reads an entry endpoint answer. Fails on anything but a listing or a deleted one.
fn entry_status(id: &str, status: StatusCode, body: &str) -> Result<Entry> {
    if status.is_success() {
        return Ok(if entry_is_listed(body) {
            Entry::Listed
        } else {
            Entry::Gone
        });
    }
    if status.is_client_error() && body.contains("not_exist") {
        return Ok(Entry::Gone);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        bail!("warframe.market is rate limiting requests, try again in a minute");
    }
    bail!("auction entry {id} returned {status}")
}

/// Looks one auction up once, without the scraper's queue or retries: for the web app, where
/// someone waits for the answer. Returns the body too, to record a listing that's still there.
pub async fn check_entry(
    http: &reqwest::Client,
    platform: &str,
    id: &str,
) -> Result<(Entry, String)> {
    // Auction ids are hex; anything else would change the URL.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric()) {
        bail!("not an auction id: {id:?}");
    }
    let response = http
        .get(format!("{API}/v1/auctions/entry/{id}"))
        .header("Accept", "application/json")
        .header("Platform", platform)
        .header("Language", "en")
        .send()
        .await
        .context("reaching warframe.market")?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("reading warframe.market's answer")?;
    Ok((entry_status(id, status, &body)?, body))
}

/// An entry endpoint body as a search page holding just that auction, so it can be recorded
/// like a feed. `None` if the body has no auction.
pub fn entry_as_page(body: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(body).ok()?;
    let auction = value.pointer_mut("/payload/auction")?.take();
    auction
        .is_object()
        .then(|| serde_json::json!({ "payload": { "auctions": [auction] } }).to_string())
}

/// Whether an auction entry body shows a listing still for sale. A closed or hidden auction
/// can't be bought, so it counts as gone. A body without those fields counts as listed.
fn entry_is_listed(body: &str) -> bool {
    let auction = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.pointer("/payload/auction").cloned());
    let flag = |name: &str| auction.as_ref().and_then(|a| a.get(name)?.as_bool());
    flag("closed") != Some(true) && flag("visible") != Some(false)
}

#[cfg(test)]
#[test]
fn closed_or_hidden_entries_are_not_listed() {
    let entry = |fields: &str| format!(r#"{{"payload":{{"auction":{{"id":"a"{fields}}}}}}}"#);
    assert!(entry_is_listed(&entry(r#","closed":false,"visible":true"#)));
    assert!(entry_is_listed(&entry("")));
    assert!(!entry_is_listed(&entry(r#","closed":true,"visible":true"#)));
    assert!(!entry_is_listed(&entry(
        r#","closed":false,"visible":false"#
    )));
}

#[cfg(test)]
#[test]
fn entry_answers_tell_listed_from_gone_and_page_like_a_search() {
    let listed = r#"{"payload":{"auction":{"id":"a","closed":false,"visible":true}}}"#;
    let not_exist = r#"{"error": {"auction": ["app.form.not_exist"]}}"#;
    assert_eq!(
        entry_status("a", StatusCode::OK, listed).unwrap(),
        Entry::Listed
    );
    assert_eq!(
        entry_status("a", StatusCode::BAD_REQUEST, not_exist).unwrap(),
        Entry::Gone
    );
    let closed = r#"{"payload":{"auction":{"id":"a","closed":true}}}"#;
    assert_eq!(
        entry_status("a", StatusCode::OK, closed).unwrap(),
        Entry::Gone
    );
    // Being turned away isn't an answer about the listing.
    assert!(entry_status("a", StatusCode::TOO_MANY_REQUESTS, "").is_err());
    assert!(entry_status("a", StatusCode::BAD_GATEWAY, not_exist).is_err());

    let page = entry_as_page(listed).unwrap();
    let (_, _, ids) = parse_auction_page(&page).unwrap();
    assert!(ids.contains("a"));
    assert_eq!(entry_as_page(not_exist), None);
}

/// Parses an auction search body. Individual malformed auctions are skipped, not fatal.
pub fn parse_auctions(body: &str) -> Result<(Vec<Auction>, usize)> {
    let (auctions, skipped, _) = parse_auction_page(body)?;
    Ok((auctions, skipped))
}

/// Retain IDs even when a listing's remaining fields cannot be decoded.
pub fn parse_auction_page(
    body: &str,
) -> Result<(Vec<Auction>, usize, std::collections::HashSet<String>)> {
    let body: AuctionsBody = serde_json::from_str(body).context("parsing auction search")?;
    let total = body.payload.auctions.len();
    let present = body
        .payload
        .auctions
        .iter()
        .filter_map(|v| v.get("id").and_then(|id| id.as_str()).map(str::to_owned))
        .collect();
    let auctions: Vec<Auction> = body
        .payload
        .auctions
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let skipped = total - auctions.len();
    Ok((auctions, skipped, present))
}
