//! Listings the web app checked on warframe.market, kept in `checks.jsonl` in the data
//! directory and applied over the dataset.
//!
//! A check that found a listing still for sale counts as a sighting: it's no longer
//! unconfirmed, and isn't gone unless it went after the check. One that found it gone marks it
//! gone (`verified`) and counts as a removal in the charts, like a removal the scraper found.
//!
//! The log is what this server knows, whether it mirrors a dataset or scrapes one. A scraping
//! server also records each check in its dataset; that gone row carries the check's time, so it
//! isn't counted a second time.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use wfmt_dataset::schema::GoneReason;

use crate::{AuctionId, Data, Gone, GoneEvent, Store, auction_price};

/// The log, in the data directory.
pub const CHECKS_FILE: &str = "checks.jsonl";

/// One line of the log.
#[derive(Serialize, Deserialize)]
struct Check {
    auction: String,
    /// Unix ms.
    at: i64,
    listed: bool,
}

/// What the data says about one listing, for deciding whether to check it again.
#[derive(Clone, Debug, PartialEq)]
pub struct ListingStatus {
    pub weapon: String,
    /// When and why it went, if it did.
    pub gone: Option<(i64, String)>,
    /// When a check last found it still listed.
    pub checked_at: Option<i64>,
}

impl Store {
    /// Records a check of `auction` at `at` (unix ms) and applies it right away.
    pub fn record_check(&self, auction: &str, at: i64, listed: bool) -> Result<()> {
        let mut line = serde_json::to_string(&Check {
            auction: auction.to_owned(),
            at,
            listed,
        })?;
        line.push('\n');
        let path = self.dir.join(CHECKS_FILE);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))?;
        let mut inner = self.write();
        if inner.apply_checks(&self.dir) {
            inner.finish_checks();
            inner.version += 1;
        }
        Ok(())
    }
}

impl Data {
    /// The log has lines this data hasn't applied yet.
    pub(crate) fn checks_pending(&self, dir: &Path) -> bool {
        fs::metadata(dir.join(CHECKS_FILE)).is_ok_and(|m| m.len() != self.checks_len)
    }

    /// Applies the lines of the log added since the last call. Returns whether anything changed;
    /// [`Data::finish_checks`] then has to run, unless a full `finish_load` follows.
    pub(crate) fn apply_checks(&mut self, dir: &Path) -> bool {
        let path = dir.join(CHECKS_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
            Err(e) => {
                tracing::error!("failed to read {}: {e}", path.display());
                return false;
            }
        };
        // A shorter log was replaced, so start over on it. A line still being written waits.
        let start = if (text.len() as u64) < self.checks_len {
            0
        } else {
            self.checks_len as usize
        };
        let Some(end) = text[start..].rfind('\n').map(|i| start + i + 1) else {
            return false;
        };
        let mut changed = false;
        for line in text[start..end].lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str::<Check>(line) {
                Ok(check) => changed |= self.apply_check(&check),
                Err(e) => tracing::warn!("skipping a bad line in {}: {e}", path.display()),
            }
        }
        self.checks_len = end as u64;
        changed
    }

    fn apply_check(&mut self, check: &Check) -> bool {
        let Some(&index) = self.auction_index.get(&check.auction) else {
            return false;
        };
        let a = &mut self.auctions[index];
        if check.listed {
            let newer = check.at > a.checked_at;
            a.checked_at = a.checked_at.max(check.at);
            return newer;
        }
        // Already gone, or seen again after the check.
        if a.current_gone().is_some() || check.at < a.seen() {
            return false;
        }
        a.gone = Some(Gone {
            at: check.at,
            reason: GoneReason::Verified,
        });
        let event = GoneEvent {
            t: check.at,
            auction: index,
            price: auction_price(a),
            direct_sell: a.direct_sell,
        };
        self.gone.push(event);
        self.check_gone.insert((index, check.at));
        true
    }

    /// Whether a gone row from the dataset is a removal a check already counted.
    pub(crate) fn counted_by_check(&mut self, auction: AuctionId, at: i64) -> bool {
        self.check_gone.remove(&(auction, at))
    }

    /// What applying checks changed, without the full `finish_load`.
    pub(crate) fn finish_checks(&mut self) {
        self.sort_gone();
        self.rows = self.build_rows();
    }

    /// What the data says about the listing with this auction id, `None` if it isn't known.
    pub fn listing_status(&self, auction: &str) -> Option<ListingStatus> {
        let a = &self.auctions[*self.auction_index.get(auction)?];
        Some(ListingStatus {
            weapon: self.weapons[a.weapon].slug.clone(),
            gone: a
                .current_gone()
                .map(|g| (g.at, g.reason.as_str().to_owned())),
            checked_at: (a.checked_at > 0).then_some(a.checked_at),
        })
    }
}
