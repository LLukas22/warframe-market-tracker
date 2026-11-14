//! Detects confirmed removals and expires long-unseen auctions.
//!
//! Capped search pages cannot prove absence: sellers can change prices between fetches,
//! and sequential sort orders are not an atomic snapshot. Only a complete, successfully
//! decoded fetch (see [`returned_everything`]) or an entry-endpoint check can establish
//! removal. Other missing auctions remain unresolved until they reappear, are verified, or
//! expire after four days.

use wfmt_types::SortBy;

use crate::store::PageKind;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};

use crate::wfm::Auction;

/// warframe.market's page size for auction searches.
pub const PAGE_CAP: usize = 100;
pub const EXPIRE_AFTER: TimeDelta = TimeDelta::days(4);
/// A listing a feed keeps returning unchanged still gets a row this often, to record its sightings.
pub const FEED_ROW_REFRESH: TimeDelta = TimeDelta::hours(1);
/// A listing the entry endpoint confirmed isn't checked again for this long.
pub const RECHECK_AFTER: TimeDelta = TimeDelta::days(1);

/// Search coverage hints. Price bounds describe observed pages, not proof of removal.
#[derive(Clone, Debug, Default)]
pub struct Coverage {
    /// The search returned every auction, see [`returned_everything`].
    pub complete: bool,
    /// Interrupted or malformed fetches must not infer absence or expiry.
    pub incomplete: bool,
    /// Every auction with a lower buyout is in the cheapest-first page.
    pub up_to: Option<i64>,
    /// Every auction with a higher buyout is in the most-expensive-first page.
    pub from: Option<i64>,
    /// The most-expensive-first page got past the auctions without a buyout, so it has them all.
    pub no_buyout: bool,
}

impl Coverage {
    /// Takes the buyout price of every auction on the page. A short page counts like a full
    /// one here: being short doesn't prove it holds everything.
    pub fn add_page(&mut self, kind: PageKind, buyouts: &[Option<i64>]) {
        let known = buyouts.iter().flatten().copied();
        match kind {
            PageKind::Search(SortBy::PriceAsc) => self.up_to = known.max().max(self.up_to),
            PageKind::Search(SortBy::PriceDesc) => {
                self.from = match (known.min(), self.from) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                self.no_buyout |= self.from.is_some();
            }
            PageKind::Feed(_) => {}
        }
    }

    /// An uninterrupted, fully decoded fetch returned every listing.
    pub fn is_complete(&self) -> bool {
        self.complete && !self.incomplete
    }

    /// Static price-range coverage; never use a previous price as proof of absence. A buyout
    /// exactly at a page's edge isn't certain: the cap can cut between equal prices.
    pub fn covers(&self, buyout: Option<i64>) -> bool {
        if self.is_complete() {
            return true;
        }
        match buyout {
            None => self.no_buyout,
            Some(p) => self.up_to.is_some_and(|m| p < m) || self.from.is_some_and(|m| p > m),
        }
    }
}

/// Whether a weapon's unfiltered search pages, as (sort order, entries, listing ids), returned
/// every auction. A short page alone proves nothing: the site often returns 98 or 99 auctions
/// of a full page, and taking that as complete marked every other listing gone. So both price
/// orders have to come back short with the same listings, which only happens when there are
/// fewer auctions than one page holds.
pub fn returned_everything<'a>(
    pages: impl IntoIterator<Item = (PageKind, usize, &'a HashSet<String>)>,
) -> bool {
    let (mut asc, mut desc) = (false, false);
    let mut listings: Option<&HashSet<String>> = None;
    for (sort_by, entries, ids) in pages {
        if entries >= PAGE_CAP || listings.is_some_and(|l| l != ids) {
            return false;
        }
        listings = Some(ids);
        asc |= sort_by == PageKind::Search(SortBy::PriceAsc);
        desc |= sort_by == PageKind::Search(SortBy::PriceDesc);
    }
    asc && desc
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Seen {
    pub riven_id: String,
    pub price: Option<i64>,
    /// What the search sorts on. Missing in state written before this field existed.
    #[serde(default)]
    pub buyout: Option<i64>,
    /// These three are missing in state written before feeds; that only costs one extra row.
    #[serde(default)]
    pub top_bid: Option<i64>,
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    /// When this auction last got a row in the dataset.
    #[serde(default)]
    pub row_at: Option<DateTime<Utc>>,
    pub direct_sell: bool,
    pub owner_name: String,
    pub created: DateTime<Utc>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// When the entry endpoint last confirmed the listing still exists.
    #[serde(default)]
    pub verified_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoneReason {
    /// Missing from a complete, successfully decoded fetch.
    Missing,
    /// The entry endpoint said it no longer exists.
    Verified,
    /// Not seen for a long time while outside the covered range.
    Expired,
}

impl GoneReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Verified => "verified",
            Self::Expired => "expired",
        }
    }

    /// Parses what [`GoneReason::as_str`] writes.
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Missing, Self::Verified, Self::Expired]
            .into_iter()
            .find(|r| r.as_str() == s)
    }
}

pub struct GoneRow {
    pub detected_at: DateTime<Utc>,
    pub weapon_slug: String,
    pub auction_id: String,
    pub reason: GoneReason,
    pub seen: Seen,
}

/// Last known state of every auction per weapon, persisted as `seen/<weapon>.json`.
#[derive(Default)]
pub struct Tracker {
    dir: Option<PathBuf>,
    weapons: HashMap<String, HashMap<String, Seen>>,
}

impl Tracker {
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::default()
    }

    pub fn open(dir: PathBuf) -> Self {
        Self {
            dir: Some(dir),
            weapons: HashMap::new(),
        }
    }

    fn weapon(&mut self, slug: &str) -> &mut HashMap<String, Seen> {
        if !self.weapons.contains_key(slug) {
            let loaded = self
                .dir
                .as_ref()
                .and_then(|d| fs::read(d.join(format!("{slug}.json"))).ok())
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            self.weapons.insert(slug.to_owned(), loaded);
        }
        self.weapons.get_mut(slug).unwrap()
    }

    pub fn contains(&mut self, weapon: &str, auction_id: &str) -> bool {
        self.weapon(weapon).contains_key(auction_id)
    }

    /// An older API edit may arrive later through another search or a cached feed.
    /// Equal timestamps remain eligible: bids can change without updating the edit time.
    pub fn is_stale(&mut self, weapon: &str, auction: &Auction) -> bool {
        self.weapon(weapon)
            .get(&auction.id)
            .and_then(|s| s.updated)
            .is_some_and(|updated| auction.updated < updated)
    }

    /// Whether a feed sighting is worth a row: the listing is new or edited, has a new bid, or
    /// hasn't had a row for a while. Sightings without a row are lost with their raw archive.
    pub fn needs_row(&mut self, weapon: &str, auction: &Auction, now: DateTime<Utc>) -> bool {
        self.weapon(weapon).get(&auction.id).is_none_or(|s| {
            s.price != auction.price()
                || s.buyout != auction.buyout_price
                || s.top_bid != auction.top_bid()
                || s.updated != Some(auction.updated)
                || s.row_at.is_none_or(|t| now - t >= FEED_ROW_REFRESH)
        })
    }

    /// Absent auctions from incomplete coverage, regardless of their old prices, in the order
    /// they're worth an entry check: most recently seen first. Those most likely just sold, and
    /// would otherwise stay listed for days; the longest unseen expire soon anyway. Listings
    /// confirmed within [`RECHECK_AFTER`] wait, so they don't take every check.
    pub fn uncertain(
        &mut self,
        weapon: &str,
        present: &HashSet<&str>,
        coverage: &Coverage,
        now: DateTime<Utc>,
    ) -> Vec<String> {
        if coverage.is_complete() {
            return Vec::new();
        }
        let mut out: Vec<(&String, &Seen)> = self
            .weapon(weapon)
            .iter()
            .filter(|(id, s)| {
                !present.contains(id.as_str())
                    && s.verified_at.is_none_or(|t| now - t >= RECHECK_AFTER)
            })
            .collect();
        out.sort_by_key(|(id, s)| (std::cmp::Reverse(s.last_seen), *id));
        out.into_iter().map(|(id, _)| id.clone()).collect()
    }

    /// Updates the state with one fetch and returns the auctions that are gone.
    ///
    /// `verified` holds entry-endpoint results: `true` = still exists, `false` = gone.
    pub fn update(
        &mut self,
        weapon: &str,
        now: DateTime<Utc>,
        current: Vec<(String, Seen)>,
        coverage: &Coverage,
        verified: &HashMap<String, bool>,
    ) -> Vec<GoneRow> {
        self.update_present(weapon, now, current, coverage, verified, &HashSet::new())
    }

    pub fn update_present(
        &mut self,
        weapon: &str,
        now: DateTime<Utc>,
        current: Vec<(String, Seen)>,
        coverage: &Coverage,
        verified: &HashMap<String, bool>,
        observed: &HashSet<String>,
    ) -> Vec<GoneRow> {
        let previous = std::mem::take(self.weapon(weapon));
        let present: HashSet<String> = current.iter().map(|(id, _)| id.clone()).collect();
        let mut gone = Vec::new();
        let mut keep = HashMap::new();
        let mut first_seen = HashMap::new();

        for (id, mut s) in previous {
            if present.contains(&id) {
                first_seen.insert(id, s.first_seen);
                continue;
            }
            if observed.contains(&id) {
                s.last_seen = now;
                keep.insert(id, s);
                continue;
            }
            let reason = match verified.get(&id) {
                Some(false) => Some(GoneReason::Verified),
                Some(true) => {
                    s.last_seen = now;
                    s.verified_at = Some(now);
                    None
                }
                None if coverage.is_complete() => Some(GoneReason::Missing),
                None if !coverage.incomplete && now - s.last_seen > EXPIRE_AFTER => {
                    Some(GoneReason::Expired)
                }
                None => None,
            };
            match reason {
                Some(reason) => gone.push(GoneRow {
                    detected_at: now,
                    weapon_slug: weapon.to_owned(),
                    auction_id: id,
                    reason,
                    seen: s,
                }),
                None => {
                    keep.insert(id, s);
                }
            }
        }

        for (id, mut s) in current {
            if let Some(first) = first_seen.get(&id) {
                s.first_seen = *first;
            }
            keep.insert(id, s);
        }
        *self.weapon(weapon) = keep;
        gone
    }

    pub fn snapshot(&self, weapon: &str) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(
            &self.weapons.get(weapon).cloned().unwrap_or_default(),
        )?)
    }

    pub fn save(&self, weapon: &str) -> Result<()> {
        let (Some(dir), Some(seen)) = (&self.dir, self.weapons.get(weapon)) else {
            return Ok(());
        };
        fs::create_dir_all(dir)?;
        let path = dir.join(format!("{weapon}.json"));
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, serde_json::to_vec(seen)?)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    #[cfg(test)]
    fn first_seen(&mut self, weapon: &str, auction_id: &str) -> Option<DateTime<Utc>> {
        self.weapon(weapon).get(auction_id).map(|s| s.first_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASC: PageKind = PageKind::Search(SortBy::PriceAsc);
    const DESC: PageKind = PageKind::Search(SortBy::PriceDesc);

    fn seen(price: i64, at: DateTime<Utc>) -> Seen {
        Seen {
            riven_id: "r".into(),
            price: Some(price),
            buyout: Some(price),
            top_bid: None,
            updated: None,
            row_at: None,
            direct_sell: true,
            owner_name: "x".into(),
            created: at,
            first_seen: at,
            last_seen: at,
            verified_at: None,
        }
    }

    fn capped(sort: PageKind, lo: i64, hi: i64) -> Coverage {
        let prices: Vec<Option<i64>> = (0..PAGE_CAP as i64)
            .map(|i| Some(lo + (hi - lo) * i / (PAGE_CAP as i64 - 1)))
            .collect();
        let mut c = Coverage::default();
        c.add_page(sort, &prices);
        c
    }

    /// A full page: `no_buyout` auctions first, then the given buyouts.
    fn desc_page(no_buyout: usize, high: i64, low: i64) -> Vec<Option<i64>> {
        let priced = PAGE_CAP - no_buyout;
        let mut page = vec![None; no_buyout];
        page.extend(
            (0..priced as i64).map(|i| Some(high - (high - low) * i / (priced as i64 - 1))),
        );
        page
    }

    #[test]
    fn auctions_without_a_buyout_do_not_stretch_the_covered_range() {
        // Real shape: bidding auctions starting at 1p head the most-expensive page.
        let asc: Vec<_> = (0..PAGE_CAP as i64).map(|i| Some(5 + i / 3)).collect();
        let mut c = Coverage::default();
        c.add_page(ASC, &asc);
        c.add_page(DESC, &desc_page(14, 9000, 400));

        assert!(!c.is_complete());
        assert!(c.covers(Some(10)) && c.covers(Some(5000)));
        assert!(!c.covers(Some(100)), "the unseen middle is uncertain");
        assert!(
            c.covers(None),
            "the page got past every auction without a buyout"
        );
    }

    #[test]
    fn a_page_of_only_auctions_without_a_buyout_proves_nothing_about_them() {
        let mut c = Coverage::default();
        c.add_page(DESC, &[None; PAGE_CAP]);
        assert!(!c.covers(None) && !c.covers(Some(500)));
        assert!(!c.is_complete());
    }

    #[test]
    fn buyouts_exactly_at_a_page_edge_are_uncertain() {
        let mut c = capped(ASC, 10, 50);
        assert!(c.covers(Some(49)) && !c.covers(Some(50)));
        c.add_page(DESC, &desc_page(0, 900, 50));
        // Both pages stop at 50, so more 50p listings can sit between them.
        assert!(!c.is_complete() && !c.covers(Some(50)));
        assert!(c.covers(Some(51)));

        let mut overlap = capped(ASC, 10, 60);
        overlap.add_page(DESC, &desc_page(0, 900, 50));
        assert!(
            !overlap.is_complete(),
            "sequential pages are not an atomic snapshot"
        );
    }

    #[test]
    fn state_from_before_the_buyout_field_still_loads() {
        let old = r#"{"riven_id":"r","price":50,"direct_sell":true,"owner_name":"x",
            "created":"2026-11-03T00:00:00Z","first_seen":"2026-11-03T00:00:00Z",
            "last_seen":"2026-11-03T00:00:00Z"}"#;
        let seen: Seen = serde_json::from_str(old).unwrap();
        assert_eq!(seen.price, Some(50));
        assert_eq!(seen.buyout, None);
    }

    #[test]
    fn coverage_ranges() {
        let mut c = capped(ASC, 10, 100);
        assert!(c.covers(Some(50)) && !c.covers(Some(500)));
        c.add_page(DESC, &(0..PAGE_CAP).map(|_| Some(900)).collect::<Vec<_>>());
        assert!(c.covers(Some(950)) && !c.covers(Some(500)));
        let full = Coverage {
            complete: true,
            ..Coverage::default()
        };
        assert!(full.covers(None));
    }

    fn ids(range: std::ops::Range<usize>) -> HashSet<String> {
        range.map(|i| format!("a{i}")).collect()
    }

    #[test]
    fn a_page_just_short_of_the_cap_is_not_everything() {
        // Real shape: the site returns 99 cheapest and 99 most expensive of 1800 listings.
        let (cheap, pricey) = (ids(0..99), ids(1700..1799));
        assert!(!returned_everything([(ASC, 99, &cheap)]));
        assert!(!returned_everything([
            (ASC, 99, &cheap),
            (DESC, 99, &pricey),
        ]));
    }

    #[test]
    fn both_orders_returning_the_same_short_page_is_everything() {
        let all = ids(0..40);
        assert!(returned_everything([(ASC, 40, &all), (DESC, 40, &all),]));
        let empty = HashSet::new();
        assert!(returned_everything([(ASC, 0, &empty), (DESC, 0, &empty),]));
        // A listing added between the two requests: not a consistent snapshot.
        assert!(!returned_everything([
            (ASC, 40, &all),
            (DESC, 41, &ids(0..41)),
        ]));
        // One order alone, however short, proves nothing.
        assert!(!returned_everything([(ASC, 5, &ids(0..5))]));
        assert!(!returned_everything([
            (ASC, 100, &ids(0..100)),
            (DESC, 100, &ids(0..100)),
        ]));
    }

    #[test]
    fn missing_auction_in_the_unseen_middle_is_not_gone() {
        let t0 = Utc::now();
        let mut tracker = Tracker::in_memory();
        let mut coverage = capped(ASC, 10, 50);
        coverage.add_page(DESC, &desc_page(14, 9000, 400));
        let none = HashMap::new();
        let middle = ("middle".to_owned(), seen(100, t0));
        tracker.update("w", t0, vec![middle], &coverage, &none);

        let later = t0 + TimeDelta::hours(4);
        assert!(
            tracker
                .update("w", later, vec![], &coverage, &none)
                .is_empty()
        );
        assert_eq!(
            tracker.uncertain("w", &HashSet::new(), &coverage, later),
            vec!["middle"]
        );
    }

    #[test]
    fn repriced_auctions_remain_uncertain_until_verified_or_a_complete_fetch() {
        let t0 = Utc::now();
        let t1 = t0 + TimeDelta::hours(12);
        let mut tracker = Tracker::in_memory();
        let coverage = capped(ASC, 10, 100);
        let none = HashMap::new();
        tracker.update(
            "w",
            t0,
            vec![
                ("cheap".into(), seen(50, t0)),
                ("pricey".into(), seen(500, t0)),
            ],
            &coverage,
            &none,
        );
        // Either auction may have moved outside the fetched price ranges.
        assert!(tracker.update("w", t1, vec![], &coverage, &none).is_empty());
        let mut uncertain = tracker.uncertain("w", &HashSet::new(), &coverage, t1);
        uncertain.sort();
        assert_eq!(uncertain, vec!["cheap", "pricey"]);
        let verified = HashMap::from([("cheap".to_owned(), false)]);
        let gone = tracker.update("w", t1, vec![], &coverage, &verified);
        assert_eq!(gone[0].reason, GoneReason::Verified);
        let complete = Coverage {
            complete: true,
            ..Coverage::default()
        };
        let gone = tracker.update("w", t1, vec![], &complete, &none);
        assert_eq!(gone[0].auction_id, "pricey");
        assert_eq!(gone[0].reason, GoneReason::Missing);
    }

    #[test]
    fn entry_checks_go_to_the_latest_drop_outs_and_skip_recent_confirmations() {
        let t0 = Utc::now();
        let mut tracker = Tracker::in_memory();
        let coverage = capped(ASC, 10, 100);
        let none = HashMap::new();
        tracker.update(
            "w",
            t0,
            vec![("old".into(), seen(50, t0))],
            &coverage,
            &none,
        );
        let t1 = t0 + TimeDelta::hours(4);
        tracker.update(
            "w",
            t1,
            vec![("new".into(), seen(60, t1))],
            &coverage,
            &none,
        );
        let t2 = t1 + TimeDelta::hours(4);
        tracker.update("w", t2, vec![], &coverage, &none);
        assert_eq!(
            tracker.uncertain("w", &HashSet::new(), &coverage, t2),
            vec!["new", "old"],
            "the listing that dropped out last most likely just sold"
        );

        // Still listed: it stays, and other listings get the next checks.
        let verified = HashMap::from([("new".to_owned(), true)]);
        assert!(
            tracker
                .update("w", t2, vec![], &coverage, &verified)
                .is_empty()
        );
        let t3 = t2 + TimeDelta::hours(4);
        assert_eq!(
            tracker.uncertain("w", &HashSet::new(), &coverage, t3),
            vec!["old"]
        );
        assert_eq!(
            tracker.uncertain("w", &HashSet::new(), &coverage, t2 + RECHECK_AFTER),
            vec!["new", "old"]
        );
        let complete = Coverage {
            complete: true,
            ..Coverage::default()
        };
        assert!(
            tracker
                .uncertain("w", &HashSet::new(), &complete, t3)
                .is_empty()
        );
    }

    #[test]
    fn interrupted_fetches_do_not_expire_or_remove_listings() {
        let t0 = Utc::now();
        let mut tracker = Tracker::in_memory();
        let coverage = Coverage {
            complete: true,
            incomplete: true,
            ..Coverage::default()
        };
        tracker.update(
            "w",
            t0,
            vec![("a".into(), seen(50, t0))],
            &coverage,
            &HashMap::new(),
        );
        assert!(
            tracker
                .update(
                    "w",
                    t0 + TimeDelta::days(5),
                    vec![],
                    &coverage,
                    &HashMap::new()
                )
                .is_empty()
        );
    }

    #[test]
    fn expires_and_keeps_first_seen() {
        let t0 = Utc::now();
        let mut tracker = Tracker::in_memory();
        let coverage = capped(ASC, 10, 100);
        let none = HashMap::new();
        tracker.update(
            "w",
            t0,
            vec![("a".into(), seen(50, t0)), ("b".into(), seen(500, t0))],
            &coverage,
            &none,
        );
        let later = t0 + TimeDelta::hours(1);
        tracker.update(
            "w",
            later,
            vec![("a".into(), seen(50, later))],
            &coverage,
            &none,
        );
        assert_eq!(tracker.first_seen("w", "a"), Some(t0));
        let gone = tracker.update(
            "w",
            t0 + TimeDelta::days(5),
            vec![("a".into(), seen(50, later))],
            &coverage,
            &none,
        );
        assert_eq!(gone.len(), 1);
        assert_eq!(gone[0].reason, GoneReason::Expired);
    }
}
