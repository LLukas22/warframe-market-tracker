//! Minimal warframe.market client.
//!
//! The API allows ~3 requests/s but punishes bursts, so every request goes through a single
//! sequential client that keeps a fixed gap between calls and backs off on 429/5xx.

use std::time::{Duration, Instant};

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
            .user_agent(concat!(
                "warframe-market-tracker/",
                env!("CARGO_PKG_VERSION"),
                " (riven price research)"
            ))
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
        if status.is_success() {
            return Ok(true);
        }
        if body.contains("not_exist") {
            return Ok(false);
        }
        bail!("auction entry {id} returned {status}: {body}")
    }

    /// Raw riven auction search body for one weapon.
    pub async fn auctions_raw(&mut self, weapon: &str, sort_by: &str) -> Result<String> {
        self.get(&format!(
            "/v1/auctions/search?type=riven&weapon_url_name={weapon}&sort_by={sort_by}"
        ))
        .await
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
    pub group: String,
    pub riven_type: String,
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
        self.riven_type != "melee" && self.game_ref.to_ascii_lowercase().contains("bow")
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
