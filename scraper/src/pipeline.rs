//! Processing of one weapon fetch, shared by the engine and `reprocess` so both produce
//! identical output from the same raw responses.

use std::collections::{HashMap, HashSet};

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
        coverage.add_page(&page.sort_by, &buyouts);
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
            auctions,
            mut coverage,
            skipped,
            present,
        } = parse_pages(&raw.pages)?;
        coverage.incomplete |= !raw.complete;
        sink.prepare(raw.scraped_at)?;
        let before = sink.auctions.len();
        self.normalizer.normalize(
            weapon,
            &raw.platform,
            raw.scraped_at,
            &auctions,
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
        let verified: HashMap<String, bool> = raw
            .verified
            .iter()
            .map(|v| (v.auction_id.clone(), v.exists))
            .collect();
        let gone = self.tracker.update_present(
            &weapon.slug,
            raw.scraped_at,
            current,
            &coverage,
            &verified,
            &present,
        );
        let summary = Summary {
            auctions: auctions.len(),
            fitted,
            skipped,
            gone: gone.len(),
            complete: coverage.is_complete(),
        };
        sink.gone.extend(gone);
        self.registry.flush()?;
        sink.checkpoint(
            std::path::PathBuf::from("seen").join(format!("{}.json", weapon.slug)),
            self.tracker.snapshot(&weapon.slug)?,
        );
        Ok(summary)
    }
}
