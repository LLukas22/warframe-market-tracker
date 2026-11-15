//! Whether a listing is still on warframe.market, checked when someone opens its riven page.
//!
//! warframe.market's API only answers browsers on its own site, so the server asks. Every
//! result is kept: in the store's check log (see `wfmt_store::CHECKS_FILE`), and on a scraping
//! server in the dataset too, through the scraper (`Command::Check`). A listing found still
//! listed isn't asked about again for [`CACHE_MS`], or [`REFRESH_MS`] when the page's refresh
//! button asks. A listing already gone isn't asked about at all.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::Mutex;
use wfmt_api::Availability as Answer;
use wfmt_scraper::wfm::{self, Entry};
use wfmt_store::ListingStatus;
use wfmt_types::{Command, ListingCheck, MINUTE_MS, SECOND_MS};

use crate::app::{App, now_ms};

/// How long a check that found the listing still there answers for it.
pub const CACHE_MS: i64 = 15 * MINUTE_MS;
/// How long it answers for a refresh, so a held-down button can't flood warframe.market.
pub const REFRESH_MS: i64 = 15 * SECOND_MS;
/// Gap between two checks, whoever asks: warframe.market punishes bursts.
const GAP: Duration = Duration::from_secs(1);
/// Someone waits for the answer, so don't wait as long as the scraper does.
const TIMEOUT: Duration = Duration::from_secs(10);

pub struct Availability {
    http: reqwest::Client,
    /// Taken for each check, one at a time; holds when the last request went out.
    last_request: Mutex<Option<Instant>>,
}

impl Availability {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent(wfmt_types::USER_AGENT)
                .timeout(TIMEOUT)
                .build()?,
            last_request: Mutex::new(None),
        })
    }

    /// Answers from the data if it can, else asks warframe.market and records the answer.
    pub async fn check(&self, app: &'static App, auction: &str, refresh: bool) -> Result<Answer> {
        let max_age = if refresh { REFRESH_MS } else { CACHE_MS };
        if let Some(answer) = known(&status(app, auction).await?, max_age, now_ms()) {
            return Ok(answer);
        }
        let mut last_request = self.last_request.lock().await;
        // Someone else may have checked it while this waited its turn.
        let status = status(app, auction).await?;
        if let Some(answer) = known(&status, max_age, now_ms()) {
            return Ok(answer);
        }
        if let Some(last) = *last_request {
            tokio::time::sleep_until((last + GAP).into()).await;
        }
        *last_request = Some(Instant::now());
        let platform = &app.scraper.settings().engine.platform;
        let (entry, body) = wfm::check_entry(&self.http, platform, auction).await?;
        let checked_at = now_ms();
        let listed = entry == Entry::Listed;

        let id = auction.to_owned();
        tokio::task::spawn_blocking(move || app.store.record_check(&id, checked_at, listed))
            .await??;
        if app.scraper.is_running() {
            let check = ListingCheck {
                auction_id: auction.to_owned(),
                weapon: status.weapon,
                checked_at,
                body: listed.then_some(body),
            };
            if let Err(error) = app.scraper.send(Command::Check(check)) {
                tracing::warn!("couldn't pass a listing check to the scraper: {error:#}");
            }
        }
        Ok(answer(listed, checked_at, true, None, checked_at))
    }
}

async fn status(app: &'static App, auction: &str) -> Result<ListingStatus> {
    let id = auction.to_owned();
    app.query(move |data| data.listing_status(&id))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .context("this listing isn't in the data")
}

/// The answer the data already has: the listing went, or a check found it within `max_age`.
fn known(status: &ListingStatus, max_age: i64, now: i64) -> Option<Answer> {
    if let Some((at, reason)) = &status.gone {
        return Some(answer(false, *at, false, Some(reason.clone()), now));
    }
    let at = status.checked_at.filter(|at| now - at < max_age)?;
    Some(answer(true, at, false, None, now))
}

fn answer(
    listed: bool,
    checked_at: i64,
    live: bool,
    gone_reason: Option<String>,
    now: i64,
) -> Answer {
    Answer {
        listed,
        checked_at,
        live,
        gone_reason: if listed {
            None
        } else {
            Some(gone_reason.unwrap_or_else(|| "verified".into()))
        },
        server_time: now,
        refresh_at: if listed { checked_at + REFRESH_MS } else { 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(gone: Option<(i64, &str)>, checked_at: Option<i64>) -> ListingStatus {
        ListingStatus {
            weapon: "torid".into(),
            gone: gone.map(|(at, reason)| (at, reason.into())),
            checked_at,
        }
    }

    #[test]
    fn gone_listings_are_never_checked_again() {
        let answer = known(&status(Some((5, "missing")), None), CACHE_MS, 10).unwrap();
        assert!(!answer.listed);
        assert_eq!(answer.checked_at, 5);
        assert_eq!(answer.gone_reason.as_deref(), Some("missing"));
        assert_eq!(answer.refresh_at, 0);
    }

    #[test]
    fn a_recent_check_answers_until_it_is_too_old() {
        let checked = status(None, Some(1_000));
        let now = 1_000 + CACHE_MS - 1;
        let answer = known(&checked, CACHE_MS, now).unwrap();
        assert!(answer.listed && !answer.live);
        assert!(known(&checked, CACHE_MS, 1_000 + CACHE_MS).is_none());
        // A refresh only waits a few seconds.
        assert!(known(&checked, REFRESH_MS, 1_000 + REFRESH_MS).is_none());
        assert!(known(&status(None, None), CACHE_MS, now).is_none());
    }
}
