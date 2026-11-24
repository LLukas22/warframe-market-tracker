//! Searches, the overview and price charts.

use super::*;
use std::cmp::Reverse;

/// Each riven's newest current auction, see [`Data::newest_current`].
pub(super) type Newest = Table<RivenId, Option<AuctionId>>;

/// One bit per weapon class, and one for weapons without a class.
fn class_bit(class: Option<WeaponClass>) -> u8 {
    1 << class.map_or(WeaponClass::ALL.len(), |c| c as usize)
}

/// Classes in the order the browser lists them: by name, the unknown one where "unknown" goes.
fn class_order(class: &Option<WeaponClass>) -> &'static str {
    class.map_or("unknown", WeaponClass::as_str)
}

pub(super) fn median(mut v: Vec<i64>) -> Option<i64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

/// Chart bucket widths, in hours, from an hour to a week.
pub(super) const BUCKET_HOURS: [u32; 7] = [1, 3, 6, 12, 24, 72, 168];
/// The bucket width of empty charts.
pub(super) const DAY_BUCKET_HOURS: u32 = 24;
pub(super) const MAX_CHART_POINTS: i64 = 100;
/// A chart point's median and quartiles are of the asking prices set (listed or changed) in
/// this long before the bucket's end...
const RECENT: i64 = 2 * wfmt_types::DAY_MS;
/// ...and of at least this many, the newest, where fewer were.
const RECENT_MIN: usize = 20;

/// Chart bucket width for a time span: the finest that keeps the chart at `MAX_CHART_POINTS`
/// points or fewer.
pub(super) fn bucket_hours_for(span_ms: i64) -> u32 {
    let widest = BUCKET_HOURS[BUCKET_HOURS.len() - 1];
    BUCKET_HOURS
        .into_iter()
        .find(|h| span_ms <= MAX_CHART_POINTS * i64::from(*h) * HOUR_MS)
        .unwrap_or(widest)
}

impl Data {
    /// `matches` on the row; only stat filters look at the auction itself.
    pub(super) fn row_matches(
        &self,
        r: &Row,
        a: &Auction,
        q: &Query,
        weapon: Option<WeaponId>,
    ) -> bool {
        if q.riven_id.is_some() {
            return self.matches(a, q, weapon);
        }
        if weapon.is_some_and(|w| w != r.weapon) {
            return false;
        }
        if q.price_changed || q.owner_changed {
            let changes = self.riven_changes[r.riven];
            if (q.price_changed && !changes.price) || (q.owner_changed && !changes.owner) {
                return false;
            }
        }
        if (!q.include_unfit && !r.has(RowFlags::FIT_OK))
            || (!q.include_legacy && r.has(RowFlags::LEGACY))
            || (q.min_quality > 0.0 && (r.mean_quality.is_nan() || r.mean_quality < q.min_quality))
        {
            return false;
        }
        match &q.curse {
            CurseFilter::None if r.has(RowFlags::CURSED) => return false,
            CurseFilter::Required if !r.has(RowFlags::CURSED) => return false,
            _ => {}
        }
        if q.spliced && !r.has(RowFlags::SPLICED) {
            return false;
        }
        if !q.positives.is_empty() || matches!(q.curse, CurseFilter::Attr(_)) {
            return self.matches(a, q, weapon);
        }
        true
    }

    pub(super) fn price_outlier(&self, weapon: WeaponId, price: Option<i64>) -> bool {
        match (self.weapons[weapon].price_limit, price) {
            (Some(limit), Some(price)) => price > limit,
            _ => false,
        }
    }

    /// A gone event still holds if the auction wasn't seen again afterwards.
    pub(super) fn gone_valid(&self, g: &GoneEvent) -> bool {
        self.auctions[g.auction].seen() <= g.t
    }

    pub(super) fn is_current(&self, a: &Auction) -> bool {
        // Absence from capped pages is unresolved, not evidence of removal.
        a.current_gone().is_none()
    }

    pub(super) fn newer(&self, a: AuctionId, b: AuctionId) -> bool {
        recency(&self.auctions[a]) > recency(&self.auctions[b])
    }

    /// Each riven's newest current auction. A relist replaces the auction before it, so only
    /// this copy stands for the riven, whatever the others ask.
    pub(super) fn newest_current(&self, rows: &Table<AuctionId, Row>) -> Newest {
        let mut newest = self.rivens.same_ids(None);
        for (index, r) in rows.enumerate() {
            if !r.has(RowFlags::CURRENT) {
                continue;
            }
            let slot = &mut newest[r.riven];
            if slot.is_none_or(|n| self.newer(index, n)) {
                *slot = Some(index);
            }
        }
        newest
    }

    /// The auction disappeared but its riven is listed again in a newer one: relisted, not sold.
    pub(super) fn superseded(&self, newest: &Newest, auction: AuctionId) -> bool {
        newest[self.auctions[auction].riven].is_some_and(|n| n != auction && self.newer(n, auction))
    }

    /// A weapon's thumbnail, relative to warframe.market's static assets.
    pub fn thumb(&self, slug: &str) -> Option<String> {
        let index = *self.weapon_index.get(slug)?;
        self.weapons[index].thumb.clone()
    }

    pub fn live(&self) -> (u32, u64) {
        (self.auctions.len() as u32, self.version)
    }

    pub fn overview(&self, now: i64) -> Overview {
        let mut listings = self.weapons.same_ids(0u32);
        // Sane current prices per weapon, for the median/cheapest shown in the weapon list.
        let mut prices = self.weapons.same_ids(Vec::new());
        // Which weapon classes each attribute was seen on, as a bitmask: this visits every
        // attribute of every auction.
        let weapon_class: Table<WeaponId, u8> =
            self.weapons.iter().map(|w| class_bit(w.class)).collect();
        let mut classes = self.attrs.same_ids(0u8);
        let newest = self.newest_current(&self.rows());
        for (index, a) in self.auctions.enumerate() {
            let class = weapon_class[a.weapon];
            for attr in &a.attrs {
                classes[attr.attr] |= class;
            }
            // One listing per riven: its newest current copy.
            if newest[a.riven] != Some(index) {
                continue;
            }
            listings[a.weapon] += 1;
            let price = price_of(a.buyout_price, a.top_bid, a.starting_price);
            if let Some(p) = price.filter(|p| *p >= MIN_PRICE)
                && !self.price_outlier(a.weapon, price)
                && a.direct_sell
                && a.fit_ok
                && a.legacy_stats != Some(true)
            {
                prices[a.weapon].push(p);
            }
        }
        let week_ago = now - WEEK_MS;
        let mut gone_prices = self.weapons.same_ids(Vec::new());
        for g in self.gone.iter().filter(|g| {
            g.t >= week_ago && self.gone_valid(g) && !self.superseded(&newest, g.auction)
        }) {
            let a = &self.auctions[g.auction];
            if let Some(p) = g.price.filter(|p| *p >= MIN_PRICE)
                && !self.price_outlier(a.weapon, g.price)
                && g.direct_sell
                && a.fit_ok
                && a.legacy_stats != Some(true)
            {
                gone_prices[a.weapon].push(p);
            }
        }
        let mut weapons: Vec<WeaponInfo> = self
            .weapons
            .iter()
            .zip(listings)
            .zip(prices)
            .zip(gone_prices)
            .map(|(((w, listings), mut prices), gone)| {
                prices.sort_unstable();
                WeaponInfo {
                    gone_7d: gone.len() as u32,
                    gone_median_7d: median(gone),
                    slug: w.slug.clone(),
                    name: w.name.clone(),
                    class: w.class,
                    group: w.group,
                    has_image: w.thumb.is_some(),
                    listings,
                    median_price: prices.get(prices.len() / 2).copied(),
                    min_price: prices.first().copied(),
                    last_scrape: w.last_scrape,
                    // The server adds it, see `Weekly::popularity`.
                    trade_popularity: None,
                }
            })
            .collect();
        weapons.sort_by(|a, b| a.name.cmp(&b.name));
        let mut attributes: Vec<AttrInfo> = self
            .attrs
            .iter()
            .zip(classes)
            .map(|(slug, mask)| {
                let mut classes: Vec<Option<WeaponClass>> = std::iter::once(None)
                    .chain(WeaponClass::ALL.iter().copied().map(Some))
                    .filter(|c| mask & class_bit(*c) != 0)
                    .collect();
                classes.sort_by_key(class_order);
                AttrInfo {
                    slug: slug.clone(),
                    name: wfmt_api::attribute_name(slug),
                    classes,
                }
            })
            .collect();
        attributes.sort_by(|a, b| a.name.cmp(&b.name));
        Overview {
            weapons,
            attributes,
            auctions: self.auctions.len() as u32,
            observations: self.observation_count() as u32,
            last_scrape: self.weapons.iter().map(|w| w.last_scrape).max(),
        }
    }

    pub(super) fn matches(&self, a: &Auction, q: &Query, weapon: Option<WeaponId>) -> bool {
        if let Some(prefix) = &q.riven_id {
            return self.rivens[a.riven].starts_with(prefix.as_str());
        }
        if weapon.is_some_and(|w| w != a.weapon) {
            return false;
        }
        let changes = self.riven_changes[a.riven];
        if (q.price_changed && !changes.price) || (q.owner_changed && !changes.owner) {
            return false;
        }
        if !q.include_unfit && !a.fit_ok {
            return false;
        }
        if !q.include_legacy && a.legacy_stats == Some(true) {
            return false;
        }
        if q.min_quality > 0.0 && a.mean_quality.is_none_or(|m| m < q.min_quality) {
            return false;
        }
        let has = |slug: &str, positive: bool| {
            self.attr_index.get(slug).is_some_and(|&id| {
                a.attrs
                    .iter()
                    .any(|x| x.attr == id && x.positive == positive)
            })
        };
        if !q.positives.iter().all(|p| has(p, true)) {
            return false;
        }
        if q.spliced
            && !a
                .attrs
                .iter()
                .any(|x| wfmt_api::is_spliced(&self.attrs[x.attr]))
        {
            return false;
        }
        let has_curse = a.attrs.iter().any(|x| !x.positive);
        match &q.curse {
            CurseFilter::Any => true,
            CurseFilter::None => !has_curse,
            CurseFilter::Required => has_curse,
            CurseFilter::Attr(slug) => has(slug, false),
        }
    }

    pub(super) fn price_ok(q: &Query, price: Option<i64>, direct_sell: bool) -> bool {
        let Some(price) = price else {
            return false;
        };
        (!q.direct_only || direct_sell)
            && q.min_price.is_none_or(|m| price >= m)
            && q.max_price.is_none_or(|m| price <= m)
    }

    pub(super) fn listing(&self, a: &Auction) -> Listing {
        let w = &self.weapons[a.weapon];
        let copies = &self.riven_auctions[a.riven];
        let sellers: HashSet<&str> = copies
            .iter()
            .map(|&i| self.auctions[i].owner_name.as_str())
            .collect();
        Listing {
            auction_id: a.id.clone(),
            riven_id: self.rivens[a.riven].clone(),
            price_outlier: self.price_outlier(a.weapon, auction_price(a)),
            price_limit: w.price_limit,
            copies: copies.len() as u32,
            sellers: sellers.len() as u32,
            gone_at: a.current_gone().map(|g| g.at),
            gone_reason: a.current_gone().map(|g| g.reason.as_str().to_owned()),
            last_seen: a.seen(),
            unconfirmed: a.seen() < w.last_scrape,
            weapon_slug: w.slug.clone(),
            weapon_name: w.name.clone(),
            riven_name: a.riven_name.clone(),
            price: price_of(a.buyout_price, a.top_bid, a.starting_price),
            buyout_price: a.buyout_price,
            starting_price: a.starting_price,
            top_bid: a.top_bid,
            direct_sell: a.direct_sell,
            rank: a.rank,
            re_rolls: a.re_rolls,
            mastery_level: a.mastery_level,
            polarity: a.polarity.clone(),
            legacy_stats: a.legacy_stats,
            fit_ok: a.fit_ok,
            mean_quality: a.mean_quality,
            owner_name: a.owner_name.clone(),
            owner_reputation: a.owner_reputation,
            created: a.created,
            first_seen: a.first_seen,
            attributes: a
                .attrs
                .iter()
                .map(|x| AttrView {
                    slug: self.attrs[x.attr].clone(),
                    positive: x.positive,
                    value: x.value,
                    value_rank8: x.value_rank8,
                    quality: x.quality,
                })
                .collect(),
        }
    }

    pub fn search(&self, q: &Query, now: i64) -> anyhow::Result<SearchResult> {
        self.search_page(q, 0, None, now)
    }

    /// `overview`, reused while the data and the memo are fresh (see `Memo`).
    pub fn overview_memo(&self, now: i64) -> Overview {
        // A memo is only ever replaced whole, so one left by a panic is still consistent.
        let mut memo = self
            .memo
            .overview
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((version, at, overview)) = &*memo
            && *version == self.version
            && at.elapsed() < MEMO_TTL
        {
            return overview.clone();
        }
        let overview = self.overview(now);
        *memo = Some((self.version, Instant::now(), overview.clone()));
        overview
    }

    pub fn cached_search(&self, q: &Query) -> Option<SearchResult> {
        let entries = self.memo.search.try_lock().ok()?;
        let entry = entries
            .iter()
            .find(|e| e.fresh(self.version) && e.query == *q)?;
        entry.result.try_lock().ok()?.clone()
    }

    /// Concurrent callers share a scan of the same query. Each entry has its own lock,
    /// so a broad chart does not stall callers asking about a different weapon.
    pub fn search_memo(&self, q: &Query, now: i64) -> anyhow::Result<SearchResult> {
        let slot = {
            let mut entries = self
                .memo
                .search
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            entries.retain(|e| e.fresh(self.version));
            if let Some(entry) = entries.iter().find(|e| e.query == *q) {
                entry.result.clone()
            } else {
                let result = std::sync::Arc::new(Mutex::new(None));
                entries.insert(
                    0,
                    CachedSearch {
                        version: self.version,
                        at: Instant::now(),
                        query: q.clone(),
                        result: result.clone(),
                    },
                );
                entries.truncate(SEARCH_MEMO_SIZE);
                result
            }
        };
        let mut cached = slot.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(result) = cached.as_ref() {
            return Ok(result.clone());
        }
        let result = self.search(q, now)?;
        *cached = Some(result.clone());
        Ok(result)
    }

    /// `None` includes both first pages and history; subsequent pages fetch only one tab.
    pub fn search_page(
        &self,
        q: &Query,
        offset: usize,
        gone_tab: Option<bool>,
        now: i64,
    ) -> anyhow::Result<SearchResult> {
        // An id search shows everything about that riven, whatever the other filters say.
        let id_query;
        let q = match &q.riven_id {
            Some(prefix) => {
                id_query = Query {
                    riven_id: Some(prefix.trim().to_ascii_lowercase()),
                    weapon: None,
                    direct_only: false,
                    min_price: None,
                    max_price: None,
                    ..q.clone()
                };
                &id_query
            }
            None => q,
        };
        let weapon = match &q.weapon {
            Some(slug) => match self.weapon_index.get(slug) {
                Some(&w) => Some(w),
                None => {
                    return Ok(SearchResult {
                        listings: Vec::new(),
                        total: 0,
                        gone: Vec::new(),
                        gone_total: 0,
                        outliers_hidden: 0,
                        price_limit: None,
                        history: Vec::new(),
                        bucket_hours: DAY_BUCKET_HOURS,
                        server_time: now,
                    });
                }
            },
            None => None,
        };
        let rows = self.rows();
        let matching: Table<AuctionId, bool> = match &q.riven_id {
            // Ignores all other filters: find the rivens, then mark their auctions.
            Some(prefix) => {
                let mut matching = self.auctions.same_ids(false);
                for (riven, id) in self.rivens.enumerate() {
                    if id.starts_with(prefix.as_str()) {
                        for &a in &self.riven_auctions[riven] {
                            matching[a] = true;
                        }
                    }
                }
                matching
            }
            None => rows
                .iter()
                .zip(&self.auctions)
                .map(|(r, a)| self.row_matches(r, a, q, weapon))
                .collect(),
        };

        // Current listings, one per riven: its newest copy, which must match by itself; an
        // older copy it replaced never stands in, even when that one is cheaper or matches.
        // Indexed by riven rather than hashed: this visits every auction, ~100k.
        let price = |i: AuctionId| rows[i].price().unwrap_or(0);
        let newest = self.newest_current(&rows);
        // Invalid prices are hidden unless asked for. An id search shows everything, and the
        // charts below only count them for that one riven: they'd swamp any analysis.
        let id_search = q.riven_id.is_some();
        let show_outliers = q.include_outliers || id_search;
        let (mut current, hidden): (Vec<AuctionId>, Vec<AuctionId>) = newest
            .iter()
            .copied()
            .flatten()
            .filter(|&i| {
                matching[i] && {
                    let r = &rows[i];
                    Self::price_ok(q, r.price(), r.has(RowFlags::DIRECT))
                }
            })
            .partition(|&i| show_outliers || !rows[i].outlier());
        let outliers_hidden = hidden.len();
        let total = current.len();
        let page = |gone: bool| {
            if gone_tab == Some(!gone) {
                0
            } else if id_search && gone_tab.is_none() {
                usize::MAX
            } else {
                LISTING_PAGE_SIZE
            }
        };
        // A total order (ids are unique), so selecting the page and sorting only it gives
        // the same rows as sorting everything.
        // Compares rows first and reads the ids only for ties.
        let id = |i: AuctionId| self.auctions[i].id.as_str();
        let quality = |i: AuctionId| {
            let q = rows[i].mean_quality;
            if q.is_nan() { -1.0 } else { q }
        };
        let order = |&a: &AuctionId, &b: &AuctionId| {
            let (ra, rb) = (&rows[a], &rows[b]);
            match q.sort {
                Sort::PriceAsc => price(a).cmp(&price(b)),
                Sort::PriceDesc => price(b).cmp(&price(a)),
                Sort::QualityDesc => quality(b).total_cmp(&quality(a)),
                Sort::Newest => rb.created.cmp(&ra.created),
            }
            .then_with(|| id(a).cmp(id(b)))
        };
        let end = offset.saturating_add(page(false));
        if end < current.len() {
            current.select_nth_unstable_by(end, order);
            current.truncate(end);
        }
        current.sort_unstable_by(order);
        let listings = current
            .into_iter()
            .skip(offset)
            .map(|i| self.listing(&self.auctions[i]))
            .collect();

        // The period limits the chart and the disappeared listings; current listings are current.
        // An id search shows the riven's whole past.
        let (start, end) = if id_search {
            (None, None)
        } else {
            q.period.bounds(now)
        };
        let in_period = |t: i64| start.is_none_or(|s| t >= s) && end.is_none_or(|e| t < e);

        // Disappeared listings (≈ sold), newest first, one per riven. A relisted riven
        // wasn't sold: its old auction stays out.
        let gone_events: Vec<&GoneEvent> = self
            .gone
            .iter()
            .filter(|g| {
                in_period(g.t)
                    && matching[g.auction]
                    && self.gone_valid(g)
                    && !self.superseded(&newest, g.auction)
                    && Self::price_ok(q, g.price, g.direct_sell)
            })
            .collect();
        let gone_outlier = |g: &GoneEvent| rows[g.auction].outlier_price(g.price);
        let mut seen_rivens = HashSet::default();
        let recent_gone: Vec<&GoneEvent> = gone_events
            .iter()
            .rev()
            .filter(|g| show_outliers || !gone_outlier(g))
            .filter(|g| seen_rivens.insert(self.auctions[g.auction].riven))
            .copied()
            .collect();
        let gone_total = recent_gone.len();
        let gone = recent_gone
            .into_iter()
            .skip(offset)
            .take(page(true))
            .map(|g| self.listing(&self.auctions[g.auction]))
            .collect();

        if gone_tab.is_some() {
            return Ok(SearchResult {
                listings,
                total,
                gone,
                gone_total,
                outliers_hidden,
                price_limit: weapon.and_then(|w| self.weapons[w].price_limit),
                history: Vec::new(),
                bucket_hours: DAY_BUCKET_HOURS,
                server_time: now,
            });
        }

        // Bucket width follows the time the data covers: hours for a day, days for months.
        let (first_in_period, last_seen) = self.observation_bounds(start);
        let span_start = first_in_period.or(start).unwrap_or(now);
        // Up to the last scrape: past it, nothing is known about the market.
        let last_scrape = last_seen.unwrap_or(now);
        let span_end = end.unwrap_or(now).min(now).min(last_scrape);
        let bucket_hours = bucket_hours_for(span_end - span_start);
        let bucket = i64::from(bucket_hours) * HOUR_MS;
        let bucket_of = |t: i64| t - t.rem_euclid(bucket);

        // The last prices of rivens that disappeared, per bucket.
        let mut gone_by_bucket: HashMap<i64, HashMap<RivenId, i64>> = HashMap::default();
        for g in gone_events.iter().filter(|g| id_search || !gone_outlier(g)) {
            if let Some(p) = g.price {
                gone_by_bucket
                    .entry(bucket_of(g.t))
                    .or_default()
                    .insert(self.auctions[g.auction].riven, p);
            }
        }

        // Price history: per bucket, the market as it stood. Every matching riven listed at
        // some point in the bucket counts once, at the asking price last observed for it by
        // the bucket's end. A listing counts from its first sighting until it's gone, whether
        // or not a scrape happened to see it in that bucket: sweeps and the newest-listings
        // feed see very different parts of the market, and counting only what each bucket
        // saw made the chart jump between them. A relisted riven counts with its newest copy.
        // The cheapest is of all of them; the median and quartiles only of recent asking
        // prices (see `RECENT`), as listings stay up for weeks and their median barely moves.
        let history = if first_in_period.is_none() && start.is_some() {
            Vec::new()
        } else {
            self.market_history(
                &rows,
                &matching,
                q,
                id_search,
                (bucket_of(span_start), span_end, bucket),
                &mut gone_by_bucket,
            )?
        };

        Ok(SearchResult {
            listings,
            total,
            gone,
            gone_total,
            outliers_hidden,
            price_limit: weapon.and_then(|w| self.weapons[w].price_limit),
            history,
            bucket_hours,
            server_time: now,
        })
    }

    /// The chart's points for buckets of width `bucket` from `first` (a bucket start) to
    /// `last` (a time), see `search_page`. Each point also takes its bucket's disappeared
    /// listings out of `gone_by_bucket`.
    fn market_history(
        &self,
        rows: &Table<AuctionId, Row>,
        matching: &Table<AuctionId, bool>,
        q: &Query,
        id_search: bool,
        (first, last, bucket): (i64, i64, i64),
        gone_by_bucket: &mut HashMap<i64, HashMap<RivenId, i64>>,
    ) -> anyhow::Result<Vec<HistoryPoint>> {
        /// Not observed yet.
        const UNSEEN: i64 = i64::MIN;
        /// Observed, but its price doesn't count: filtered out, invalid or unknown.
        const NO_PRICE: i64 = i64::MIN + 1;
        /// Gone before the bucket at hand.
        const GONE: i64 = i64::MIN + 2;
        const NOBODY: u32 = u32::MAX;
        // Per auction: its price as of the bucket's end, or one of the markers above.
        let mut price = self.auctions.same_ids(UNSEEN);
        // Per auction: when its price was last set, by listing it or repricing it.
        let mut priced_at = self.auctions.same_ids(0i64);
        // Per riven: its newest listed copy, kept up as listings appear and go, so a bucket
        // reads one price per riven rather than comparing every copy again.
        let mut newest = self.rivens.same_ids(NOBODY);
        let mut in_listed = self.rivens.same_ids(false);
        let mut listed: Vec<RivenId> = Vec::new();
        let mut removals: Vec<(i64, AuctionId)> = self
            .auctions
            .enumerate()
            .filter(|&(i, _)| matching[i])
            .filter_map(|(i, a)| a.current_gone().map(|g| (g.at, i)))
            .collect();
        removals.sort_unstable();
        let mut removals = removals.into_iter().peekable();
        let selected: Vec<_> = matching
            .enumerate()
            .filter_map(|(id, &yes)| yes.then_some(id))
            .collect();
        let events = self.read_history(&selected, Some((first, last, bucket)))?;
        let mut observations = events.iter().peekable();
        let mut history = Vec::new();
        let mut b = first;
        while b <= last {
            let end = b + bucket;
            // Prices as of the bucket's end, carried over from earlier buckets.
            while let Some(o) = observations.next_if(|o| o.t < end) {
                if !matching[o.auction] {
                    continue;
                }
                let a = o.auction;
                if price[a] == UNSEEN {
                    let riven = rows[a].riven;
                    let slot = newest[riven];
                    if slot == NOBODY || self.newer(a, AuctionId::from_index(slot as usize)) {
                        newest[riven] = a.index() as u32;
                    }
                    if !in_listed[riven] {
                        in_listed[riven] = true;
                        listed.push(riven);
                    }
                }
                let p = o.price();
                let counts =
                    Self::price_ok(q, p, o.direct_sell) && (id_search || !rows[a].outlier_price(p));
                let new = p.filter(|_| counts).unwrap_or(NO_PRICE);
                if price[a] == UNSEEN {
                    let auction = &self.auctions[a];
                    priced_at[a] = if auction.created > 0 {
                        auction.created
                    } else {
                        auction.first_seen
                    };
                } else if price[a] != new {
                    priced_at[a] = o.t;
                }
                price[a] = new;
            }
            // Gone before this bucket: gone for every later one too. A listing that goes in
            // this bucket was still listed in it.
            while let Some((_, a)) = removals.next_if(|&(t, _)| t < b) {
                if price[a] == UNSEEN {
                    continue;
                }
                price[a] = GONE;
                let riven = rows[a].riven;
                if newest[riven] as usize == a.index() {
                    // Rare: an older copy may still be listed.
                    newest[riven] = self.riven_auctions[riven]
                        .iter()
                        .copied()
                        .filter(|&c| !matches!(price[c], UNSEEN | GONE))
                        .reduce(|x, y| if self.newer(y, x) { y } else { x })
                        .map_or(NOBODY, |c| c.index() as u32);
                }
            }
            listed.retain(|&r| {
                let keep = newest[r] != NOBODY;
                in_listed[r] = keep;
                keep
            });
            let mut sample: Vec<(i64, i64)> = listed
                .iter()
                .map(|&r| AuctionId::from_index(newest[r] as usize))
                .map(|a| (priced_at[a], price[a]))
                .filter(|&(_, p)| p != NO_PRICE)
                .collect();
            let count = sample.len();
            let min = sample.iter().map(|&(_, p)| p).min();
            // The recent asking prices, and at least the newest few.
            let since = end - RECENT;
            if sample.iter().filter(|&&(at, _)| at >= since).count() >= RECENT_MIN {
                sample.retain(|&(at, _)| at >= since);
            } else if sample.len() > RECENT_MIN {
                let older = sample.len() - RECENT_MIN;
                sample.select_nth_unstable_by_key(older, |&(at, _)| at);
                sample.drain(..older);
            }
            let mut v: Vec<i64> = sample.into_iter().map(|(_, p)| p).collect();
            let gone: Vec<i64> = gone_by_bucket
                .remove(&b)
                .map(|g| g.into_values().collect())
                .unwrap_or_default();
            b = end;
            let Some(min) = min else {
                continue;
            };
            // Three ranks are needed, not a sorted list: select them, which is linear.
            let top = (v.len() - 1) as f64;
            let rank = |p: f64| (top * p).round() as usize;
            let (r25, r50, r75) = (rank(0.25), rank(0.5), rank(0.75));
            let (below, &mut median_value, above) = v.select_nth_unstable(r50);
            let p75 = if r75 > r50 {
                *above.select_nth_unstable(r75 - r50 - 1).1
            } else {
                median_value
            };
            let p25 = if r25 < r50 {
                *below.select_nth_unstable(r25).1
            } else {
                median_value
            };
            history.push(HistoryPoint {
                t: end - bucket,
                count: count as u32,
                recent: v.len() as u32,
                min,
                p25,
                median: median_value,
                p75,
                gone_count: gone.len() as u32,
                gone_median: median(gone),
            });
        }
        Ok(history)
    }

    /// Which stats raise or lower the asking price of this weapon's rivens. Positive and negative
    /// versions of a stat count separately. Uses every listing seen rather than the latest scrape,
    /// so that the analysis has as much of the price range as possible.
    pub fn stat_effects(&self, weapon: &str) -> StatEffects {
        let Some(&weapon) = self.weapon_index.get(weapon) else {
            return StatEffects::default();
        };
        // Verified buyouts only, one per riven: the cheapest copy, ordered by the riven's hash so
        // the held-out rivens are an arbitrary but repeatable selection.
        let mut cheapest: HashMap<RivenId, (&Auction, i64)> = HashMap::default();
        // Find the weapon's auctions on the compact rows rather than reading every auction.
        let rows = self.rows();
        let auctions = rows
            .iter()
            .zip(&self.auctions)
            .filter(|(r, _)| r.weapon == weapon)
            .map(|(_, a)| a);
        for a in auctions {
            let Some(price) = a.buyout_price.filter(|p| *p >= MIN_PRICE) else {
                continue;
            };
            if !a.direct_sell
                || !a.fit_ok
                || a.legacy_stats == Some(true)
                || self.price_outlier(weapon, Some(price))
            {
                continue;
            }
            let entry = cheapest.entry(a.riven).or_insert((a, price));
            // Equal prices go to the newest copy, whatever order the auctions were loaded in.
            if (price, Reverse(recency(a))) < (entry.1, Reverse(recency(entry.0))) {
                *entry = (a, price);
            }
        }
        let mut rivens: Vec<(RivenId, &Auction, i64)> =
            cheapest.into_iter().map(|(r, (a, p))| (r, a, p)).collect();
        rivens.sort_by(|a, b| self.rivens[a.0].cmp(&self.rivens[b.0]));

        let mut result = StatEffects {
            rivens: rivens.len() as u32,
            low_price: rivens.iter().map(|r| r.2).min(),
            high_price: rivens.iter().map(|r| r.2).max(),
            ..StatEffects::default()
        };
        if rivens.len() < EFFECT_MIN_RIVENS {
            return result;
        }

        let mut counts: HashMap<(AttrId, bool), u32> = HashMap::default();
        for (_, a, _) in &rivens {
            for attr in &a.attrs {
                *counts.entry((attr.attr, attr.positive)).or_default() += 1;
            }
        }
        result.rare_stats = counts
            .values()
            .filter(|c| **c < EFFECT_MIN_STAT_RIVENS)
            .count() as u32;
        let mut stats: Vec<(AttrId, bool)> = counts
            .iter()
            .filter(|(_, c)| **c >= EFFECT_MIN_STAT_RIVENS)
            .map(|(s, _)| *s)
            .collect();
        // By name: ids follow the order the files were read in, and the column order changes the
        // fit's rounding.
        stats.sort_unstable_by(|a, b| self.attrs[a.0].cmp(&self.attrs[b.0]).then(a.1.cmp(&b.1)));
        if stats.is_empty() {
            return result;
        }

        let x: Vec<Vec<f64>> = rivens
            .iter()
            .map(|(_, a, _)| {
                stats
                    .iter()
                    .map(|(attr, positive)| {
                        let has = a
                            .attrs
                            .iter()
                            .any(|s| s.attr == *attr && s.positive == *positive);
                        f64::from(u8::from(has))
                    })
                    .collect()
            })
            .collect();
        let y: Vec<f64> = rivens.iter().map(|r| (r.2 as f64).ln()).collect();

        result.explained = effects::holdout_r2(&x, &y);
        if result.explained.is_none_or(|r| r < EFFECT_MIN_EXPLAINED) {
            return result;
        }
        let Some(model) = effects::fit(&x, &y) else {
            return result;
        };
        for (i, (attr, positive)) in stats.iter().enumerate() {
            let (b, se) = (model.coefficients[i], model.standard_errors[i]);
            let effect = StatEffect {
                slug: self.attrs[*attr].clone(),
                effect: b.exp_m1(),
                low: (b - INTERVAL * se).exp_m1(),
                high: (b + INTERVAL * se).exp_m1(),
                rivens: counts[&(*attr, *positive)],
                certain: model.identifiable[i] && b.abs() > INTERVAL * se,
            };
            if *positive {
                result.positive.push(effect);
            } else {
                result.negative.push(effect);
            }
        }
        for group in [&mut result.positive, &mut result.negative] {
            group.sort_by(|a, b| b.effect.total_cmp(&a.effect));
        }
        result
    }
}
