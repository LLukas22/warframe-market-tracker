//! One riven's page: its listings, similar rivens, and scanned rivens.

use super::*;

/// Stat overlap dominates roll closeness; prices and raw percentages never affect rank.
#[derive(Debug)]
pub(super) struct Similarity {
    pub(super) shared: usize,
    pub(super) missing: Vec<AttrId>,
    pub(super) added: Vec<AttrId>,
    pub(super) same_negative: bool,
    pub(super) roll_distance: Option<f64>,
}

pub(super) fn similarity(a: &Auction, b: &Auction) -> Option<Similarity> {
    let stats = |a: &Auction, positive| {
        a.attrs
            .iter()
            .filter(|x| x.positive == positive)
            .map(|x| x.attr)
            .collect::<HashSet<_>>()
    };
    let positives = stats(a, true);
    let other_positives = stats(b, true);
    let shared = positives.intersection(&other_positives).count();
    if shared == 0 {
        return None;
    }
    let mut missing: Vec<_> = positives.difference(&other_positives).copied().collect();
    let mut added: Vec<_> = other_positives.difference(&positives).copied().collect();
    missing.sort_unstable();
    added.sort_unstable();
    let same_negative = stats(a, false) == stats(b, false);
    let roll_distance = if a.fit_ok && b.fit_ok {
        a.attrs
            .iter()
            .filter_map(|x| {
                b.attrs
                    .iter()
                    .find(|y| y.attr == x.attr && y.positive == x.positive)
                    .map(|y| {
                        x.quality
                            .zip(y.quality)
                            .filter(|(x, y)| x.is_finite() && y.is_finite())
                            .map(|(x, y)| (x - y).abs())
                    })
            })
            .collect::<Option<Vec<_>>>()
            .filter(|values| !values.is_empty())
            .map(|values| values.iter().sum::<f64>() / values.len() as f64)
    } else {
        None
    };
    Some(Similarity {
        shared,
        missing,
        added,
        same_negative,
        roll_distance,
    })
}

impl Inner {
    pub fn riven_detail(
        &self,
        id: &str,
        auction_id: Option<&str>,
        comparisons: Comparisons,
    ) -> Option<RivenDetail> {
        let riven = *self.riven_index.get(id)?;
        let copies = &self.riven_auctions[riven];
        let selected = if let Some(id) = auction_id {
            let index = *self.auction_index.get(id)?;
            let auction = &self.auctions[index];
            if auction.riven != riven {
                return None;
            }
            auction
        } else {
            copies
                .iter()
                .map(|&i| &self.auctions[i])
                .max_by(|a, b| recency(a).cmp(&recency(b)))?
        };
        let (similar, similar_total, comparisons_hidden) = self.similar_to(selected, comparisons);
        Some(RivenDetail {
            listing: self.listing(selected),
            similar,
            similar_total,
            comparisons_hidden,
            server_time: now_ms(),
        })
    }

    /// Other rivens of the same weapon, closest first: the first 10, how many there are, and
    /// how many the era / verification settings hid.
    pub(super) fn similar_to(
        &self,
        selected: &Auction,
        comparisons: Comparisons,
    ) -> (Vec<SimilarRiven>, usize, usize) {
        let Comparisons {
            legacy: include_legacy,
            unfit: include_unfit,
            loose: include_loose,
        } = comparisons;
        // Pick a representative before scoring, so old relists cannot win twice or
        // make a riven appear closer than its latest eligible observation.
        let mut hidden = HashSet::default();
        let mut representatives: HashMap<RivenId, &Auction> = HashMap::default();
        for a in &self.auctions {
            if a.weapon != selected.weapon || a.riven == selected.riven {
                continue;
            }
            if (!include_legacy && a.legacy_stats != Some(false)) || (!include_unfit && !a.fit_ok) {
                if similarity(selected, a).is_some_and(|score| include_loose || score.shared >= 2) {
                    hidden.insert(a.riven);
                }
                continue;
            }
            representatives
                .entry(a.riven)
                .and_modify(|current| {
                    if (a.last_seen, &a.id) > (current.last_seen, &current.id) {
                        *current = a;
                    }
                })
                .or_insert(a);
        }
        let mut matches: Vec<_> = representatives
            .values()
            .filter_map(|a| {
                let score = similarity(selected, a)?;
                (include_loose || score.shared >= 2).then_some((*a, score))
            })
            .collect();
        matches.sort_by(|(a, score_a), (b, score_b)| {
            score_b
                .shared
                .cmp(&score_a.shared)
                .then_with(|| score_a.added.len().cmp(&score_b.added.len()))
                .then_with(|| score_b.same_negative.cmp(&score_a.same_negative))
                .then_with(|| {
                    a.attrs
                        .len()
                        .abs_diff(selected.attrs.len())
                        .cmp(&b.attrs.len().abs_diff(selected.attrs.len()))
                })
                .then_with(|| {
                    score_a
                        .roll_distance
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&score_b.roll_distance.unwrap_or(f64::INFINITY))
                })
                .then_with(|| b.last_seen.cmp(&a.last_seen))
                .then_with(|| a.id.cmp(&b.id))
        });
        let similar_total = matches.len();
        // Count distinct candidates hidden by quality/era settings, excluding any
        // riven that also has an eligible representative in this result set.
        for (a, _) in &matches {
            hidden.remove(&a.riven);
        }
        let similar = matches
            .into_iter()
            .take(SIMILAR_LIMIT)
            .map(|(a, score)| SimilarRiven {
                listing: self.listing(a),
                same_stats: score.missing.is_empty()
                    && score.added.is_empty()
                    && score.same_negative,
                shared_positives: score.shared,
                missing_positives: score
                    .missing
                    .iter()
                    .map(|&i| self.attrs[i].clone())
                    .collect(),
                added_positives: score.added.iter().map(|&i| self.attrs[i].clone()).collect(),
                same_negative: score.same_negative,
                roll_distance: score.roll_distance,
            })
            .collect();
        (similar, similar_total, hidden.len())
    }

    /// A scan as an auction that isn't stored, with its stats fitted like a listing's. `None`
    /// for an unknown weapon. Attributes the data has never seen are left out of the auction
    /// (they can't be compared), but stay in the fit.
    pub(super) fn scanned_auction(
        &self,
        scan: &RivenScan,
    ) -> Option<(Auction, Option<riven::Fit>)> {
        let weapon = *self.weapon_index.get(scan.weapon.as_deref()?)?;
        let w = &self.weapons[weapon];
        let class = w.class?;
        let inputs: Vec<AttrInput> = scan
            .attributes
            .iter()
            .map(|a| AttrInput {
                slug: &a.slug,
                value: a.value,
                positive: a.positive,
            })
            .collect();
        // Screenshots don't show the rank reliably; the fit tries every rank anyway.
        let fit = riven::fit(&RivenInput {
            class,
            is_bow: w.is_bow,
            listed_rank: i64::from(riven::MAX_RANK),
            attributes: &inputs,
            legacy_disposition: None,
            prefer_legacy: false,
        })
        .ok();
        let attrs: Vec<Attr> = scan
            .attributes
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let fitted = fit.as_ref().map(|f| &f.attributes[i]);
                Some(Attr {
                    attr: *self.attr_index.get(&a.slug)?,
                    positive: a.positive,
                    value: a.value,
                    value_rank8: fitted.map(|f| f.value_rank8),
                    quality: fitted.map(|f| f.quality),
                })
            })
            .collect();
        let positives: Vec<f64> = attrs
            .iter()
            .filter(|a| a.positive)
            .filter_map(|a| a.quality)
            .collect();
        let now = now_ms();
        let auction = Auction {
            id: String::new(),
            weapon,
            riven: RivenId::UNLISTED,
            gone: None,
            riven_name: scan.name.clone().unwrap_or_default().to_lowercase(),
            polarity: String::new(),
            mastery_level: scan.mastery_rank,
            // A card shows no count until the riven is rerolled.
            re_rolls: Some(scan.rerolls.unwrap_or(0)),
            rank: fit.as_ref().map(|f| i32::from(f.rank)),
            legacy_stats: fit.as_ref().map(|f| f.legacy),
            fit_ok: fit.is_some(),
            mean_quality: (!positives.is_empty())
                .then(|| positives.iter().sum::<f64>() / positives.len() as f64),
            owner_name: String::new(),
            owner_reputation: None,
            created: now,
            first_seen: now,
            last_seen: now,
            attrs,
            buyout_price: None,
            starting_price: None,
            top_bid: None,
            direct_sell: false,
        };
        Some((auction, fit))
    }

    /// The id of the stored riven a scan shows, if it has been listed before. Matches the way
    /// listings are identified, except that polarity isn't read from screenshots.
    pub fn scan_match(&self, scan: &RivenScan) -> Option<String> {
        let (auction, fit) = self.scanned_auction(scan)?;
        let fit = fit?;
        let attrs: Vec<IdAttr> = scan
            .attributes
            .iter()
            .zip(&fit.attributes)
            .map(|(a, f)| IdAttr {
                slug: &a.slug,
                positive: a.positive,
                value: a.value,
                roll: Some((f.roll, f.roll_tolerance)),
            })
            .collect();
        let input = IdInput {
            weapon: &self.weapons[auction.weapon].slug,
            polarity: "",
            mastery_level: auction.mastery_level,
            re_rolls: auction.re_rolls,
            listed_rank: i64::from(fit.rank),
            owner: "",
            attributes: &attrs,
        };
        let journal = data_dir().join("rivens.jsonl");
        let id = match Registry::find_in_journal(&journal, &input) {
            Ok(id) => id?,
            Err(e) => {
                tracing::warn!("failed to read {}: {e:#}", journal.display());
                return None;
            }
        };
        self.riven_index.contains_key(&id).then_some(id)
    }

    /// The riven page for a scanned riven that isn't in the data.
    pub fn scanned_detail(
        &self,
        scan: &RivenScan,
        comparisons: Comparisons,
    ) -> Option<RivenDetail> {
        let (auction, fit) = self.scanned_auction(scan)?;
        let (similar, similar_total, comparisons_hidden) = self.similar_to(&auction, comparisons);
        let w = &self.weapons[auction.weapon];
        let listing = Listing {
            auction_id: String::new(),
            riven_id: String::new(),
            price_outlier: false,
            price_limit: w.price_limit,
            copies: 0,
            sellers: 0,
            gone_at: None,
            gone_reason: None,
            last_seen: auction.last_seen,
            unconfirmed: false,
            weapon_slug: w.slug.clone(),
            weapon_name: w.name.clone(),
            riven_name: auction.riven_name.clone(),
            price: None,
            buyout_price: None,
            starting_price: None,
            top_bid: None,
            direct_sell: false,
            rank: auction.rank,
            re_rolls: auction.re_rolls,
            mastery_level: auction.mastery_level,
            polarity: String::new(),
            legacy_stats: auction.legacy_stats,
            fit_ok: auction.fit_ok,
            mean_quality: auction.mean_quality,
            owner_name: String::new(),
            owner_reputation: None,
            created: auction.created,
            first_seen: auction.first_seen,
            attributes: scan
                .attributes
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let fitted = fit.as_ref().map(|f| &f.attributes[i]);
                    AttrView {
                        slug: a.slug.clone(),
                        positive: a.positive,
                        value: a.value,
                        value_rank8: fitted.map(|f| f.value_rank8),
                        quality: fitted.map(|f| f.quality),
                    }
                })
                .collect(),
        };
        Some(RivenDetail {
            listing,
            similar,
            similar_total,
            comparisons_hidden,
            server_time: now_ms(),
        })
    }

    /// Every auction this riven appeared in, oldest first, with its price history.
    pub fn riven_history(&self, riven_id: &str) -> Vec<RivenAuction> {
        let Some(&riven) = self.riven_index.get(riven_id) else {
            return Vec::new();
        };
        let auctions = &self.riven_auctions[riven];
        let mut points: HashMap<AuctionId, Vec<PricePoint>> = HashMap::default();
        for o in self
            .observations
            .iter()
            .filter(|o| auctions.contains(&o.auction))
        {
            points.entry(o.auction).or_default().push(PricePoint {
                t: o.t,
                price: o.price(),
            });
        }
        let mut out: Vec<RivenAuction> = auctions
            .iter()
            .map(|&i| {
                let a = &self.auctions[i];
                RivenAuction {
                    auction_id: a.id.clone(),
                    owner_name: a.owner_name.clone(),
                    created: a.created,
                    first_seen: a.first_seen,
                    last_seen: a.last_seen,
                    gone_at: a.current_gone().map(|g| g.at),
                    gone_reason: a.current_gone().map(|g| g.reason.as_str().to_owned()),
                    points: {
                        let mut observations = points.remove(&i).unwrap_or_default();
                        observations.sort_by_key(|p| p.t);
                        observations
                    },
                }
            })
            .collect();
        out.sort_by(|a, b| {
            a.first_seen
                .cmp(&b.first_seen)
                .then(a.auction_id.cmp(&b.auction_id))
        });
        out
    }
}
