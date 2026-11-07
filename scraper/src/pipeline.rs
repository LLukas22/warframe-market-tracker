//! Processing of one weapon fetch, shared by the engine and `reprocess` so both produce
//! identical output from the same raw responses.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::Result;

use crate::identity::Registry;
use crate::normalize::Normalizer;
use crate::store::{ParquetSink, RawPage, RawResponse};
use crate::tracking::{Coverage, Seen, Tracker};
use crate::wfm::{self, Auction, Weapon};

pub struct Summary {
    pub auctions: usize,
    pub fitted: usize,
    pub skipped: usize,
    pub gone: usize,
    pub complete: bool,
}

pub struct Pages {
    /// Unique auctions, each tagged with the sort orders that returned it.
    pub auctions: Vec<(Auction, String)>,
    pub coverage: Coverage,
    pub skipped: usize,
    pub present: HashSet<String>,
}

/// Merges search pages into unique auctions and works out which price range they cover.
pub fn parse_pages(pages: &[RawPage]) -> Result<Pages> {
    let mut coverage = Coverage::default();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<(Auction, String)> = Vec::new();
    let mut skipped = 0;
    let mut present = HashSet::new();
    for page in pages {
        let (auctions, bad, ids) = match wfm::parse_auction_page(&page.body) {
            Ok(page) => page,
            Err(e) => {
                tracing::warn!(error = %e, "skipping unparseable archived page; absence inference disabled");
                coverage.incomplete = true;
                skipped += 1;
                continue;
            }
        };
        present.extend(ids);
        coverage.incomplete |= bad > 0;
        skipped += bad;
        let mut buyouts: Vec<Option<i64>> = auctions.iter().map(|a| a.buyout_price).collect();
        // Malformed entries still count towards the page cap.
        buyouts.extend(std::iter::repeat_n(None, bad));
        // A filtered page says nothing about auctions outside its search scope, even if short.
        if page.buyout_policy.is_none() {
            coverage.add_page(&page.sort_by, &buyouts);
        }
        for auction in auctions {
            match index.get(&auction.id) {
                Some(&i) => {
                    out[i].1.push(',');
                    out[i].1.push_str(&page.sort_by);
                }
                None => {
                    index.insert(auction.id.clone(), out.len());
                    out.push((auction, page.sort_by.clone()));
                }
            }
        }
    }
    Ok(Pages {
        auctions: out,
        coverage,
        skipped,
        present,
    })
}

pub struct Pipeline {
    pub normalizer: Normalizer,
    pub registry: Registry,
    pub tracker: Tracker,
}

impl Pipeline {
    /// Auctions that dropped out of the pages but can't be declared gone from the pages alone.
    pub fn uncertain(&mut self, weapon: &str, pages: &[RawPage]) -> Result<Vec<String>> {
        let pages = parse_pages(pages)?;
        let present: HashSet<&str> = pages.present.iter().map(String::as_str).collect();
        Ok(self.tracker.uncertain(weapon, &present, &pages.coverage))
    }

    pub fn process(
        &mut self,
        weapon: &Weapon,
        raw: &RawResponse,
        sink: &mut ParquetSink,
    ) -> Result<Summary> {
        let Pages {
            mut auctions,
            mut coverage,
            skipped,
            present,
        } = parse_pages(&raw.pages)?;
        coverage.incomplete |= !raw.complete;
        // Count the full response for coverage and presence first: discarding unpriced rows
        // must not turn a capped page into proof that missing auctions have disappeared.
        // Deduplication happens in parse_pages, so listings returned by several searches
        // contribute only once. Starting-only listings never influence their own benchmark.
        let (sum, count) = auctions
            .iter()
            .filter_map(|(a, _)| a.market_price())
            .fold((0i128, 0i128), |(sum, count), p| {
                (sum + i128::from(p), count + 1)
            });
        auctions.retain(|(auction, _)| {
            auction.market_price().is_some()
                || (count > 0
                    && auction
                        .starting_price
                        .is_some_and(|p| p > 1 && i128::from(p) * count > sum))
        });
        let verified: HashMap<String, bool> = raw
            .verified
            .iter()
            .map(|v| (v.auction_id.clone(), v.exists))
            .collect();
        let (fitted, gone) =
            self.commit(weapon, raw, &auctions, &coverage, &verified, &present, sink)?;
        Ok(Summary {
            auctions: auctions.len(),
            fitted,
            skipped,
            gone,
            complete: coverage.is_complete(),
        })
    }

    /// Records a site-wide feed. A feed only proves that its listings exist, so nothing is ever
    /// inferred from what it leaves out. Raw feed archives are short-lived, so the rows are the
    /// lasting record; only sightings that add nothing are skipped (see `Tracker::needs_row`),
    /// because consecutive polls overlap. Starting-only listings wait for a weapon search, which
    /// has the reference prices they are judged against. `weapons` limits what gets recorded.
    pub fn process_feed(
        &mut self,
        weapons: &[Weapon],
        raw: &RawResponse,
        sink: &mut ParquetSink,
    ) -> Result<Summary> {
        let Pages {
            auctions, skipped, ..
        } = parse_pages(&raw.pages)?;
        // Even a poll with nothing to record needs its receipt filed under its own date.
        sink.prepare(raw.scraped_at)?;
        let mut by_weapon: BTreeMap<String, Vec<(Auction, String)>> = BTreeMap::new();
        for entry in auctions {
            by_weapon
                .entry(entry.0.item.weapon_url_name.clone())
                .or_default()
                .push(entry);
        }
        let coverage = Coverage {
            incomplete: true,
            ..Coverage::default()
        };
        let mut summary = Summary {
            auctions: 0,
            fitted: 0,
            skipped,
            gone: 0,
            complete: false,
        };
        for (slug, listings) in by_weapon {
            let Some(weapon) = weapons.iter().find(|w| w.slug == slug) else {
                continue;
            };
            let present: HashSet<String> = listings.iter().map(|(a, _)| a.id.clone()).collect();
            let fresh: Vec<(Auction, String)> = listings
                .into_iter()
                .filter(|(a, _)| {
                    a.market_price().is_some() && self.tracker.needs_row(&slug, a, raw.scraped_at)
                })
                .collect();
            let (fitted, gone) = self.commit(
                weapon,
                raw,
                &fresh,
                &coverage,
                &HashMap::new(),
                &present,
                sink,
            )?;
            summary.auctions += fresh.len();
            summary.fitted += fitted;
            summary.gone += gone;
        }
        Ok(summary)
    }

    /// Normalizes one weapon's listings and updates its tracking. Returns (fitted, gone).
    #[allow(clippy::too_many_arguments)]
    fn commit(
        &mut self,
        weapon: &Weapon,
        raw: &RawResponse,
        auctions: &[(Auction, String)],
        coverage: &Coverage,
        verified: &HashMap<String, bool>,
        present: &HashSet<String>,
        sink: &mut ParquetSink,
    ) -> Result<(usize, usize)> {
        sink.prepare(raw.scraped_at)?;
        let before = sink.auctions.len();
        self.normalizer.normalize(
            weapon,
            &raw.platform,
            raw.scraped_at,
            auctions,
            &mut self.registry,
            &mut sink.auctions,
            &mut sink.attributes,
        );

        let rows = &sink.auctions[before..];
        let current: Vec<(String, Seen)> = rows
            .iter()
            .map(|r| {
                (
                    r.auction_id.clone(),
                    Seen {
                        riven_id: r.riven_id.clone(),
                        price: r.price(),
                        buyout: r.buyout_price,
                        top_bid: r.top_bid,
                        updated: Some(r.updated),
                        row_at: Some(raw.scraped_at),
                        direct_sell: r.is_direct_sell,
                        owner_name: r.owner_name.clone(),
                        created: r.created,
                        first_seen: raw.scraped_at,
                        last_seen: raw.scraped_at,
                    },
                )
            })
            .collect();
        let fitted = rows.iter().filter(|r| r.fit_ok).count();
        let gone = self.tracker.update_present(
            &weapon.slug,
            raw.scraped_at,
            current,
            coverage,
            verified,
            present,
        );
        let gone_count = gone.len();
        sink.gone.extend(gone);
        self.registry.flush()?;
        sink.checkpoint(
            std::path::PathBuf::from("seen").join(format!("{}.json", weapon.slug)),
            self.tracker.snapshot(&weapon.slug)?,
        );
        Ok((fitted, gone_count))
    }
}
