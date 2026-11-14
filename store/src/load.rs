//! Reading the scraper's parquet parts into [`Data`].

use super::*;
use wfmt_dataset::layout;
use wfmt_dataset::schema::{attributes, auctions, gone};

/// Prices above this are invalid. `None` when there are too few listings to tell.
pub(super) fn price_limit(prices: &mut [i64]) -> Option<i64> {
    if prices.len() < OUTLIER_MIN_LISTINGS {
        return None;
    }
    prices.sort_unstable();
    let index = ((prices.len() - 1) as f64 * OUTLIER_PERCENTILE).round() as usize;
    // On small weapons the percentile alone could land on a joke price itself.
    let index = index.min(prices.len() - 1 - OUTLIER_SKIP_TOP);
    Some(
        prices[index]
            .saturating_mul(OUTLIER_FACTOR)
            .max(OUTLIER_FLOOR),
    )
}

pub(super) fn apply_catalog(inner: &mut Data, catalog: Catalog) {
    for w in catalog.data {
        let class = WeaponClass::of(w.riven_type, w.group);
        let index = inner.intern_weapon(&w.slug, &w.i18n.en.name, class);
        let weapon = &mut inner.weapons[index];
        weapon.name = w.i18n.en.name;
        weapon.is_bow = wfmt_types::market::is_bow(w.riven_type, &w.game_ref);
        weapon.group = w.group;
        weapon.thumb = w.i18n.en.thumb;
    }
}

#[derive(serde::Deserialize)]
pub(super) struct Catalog {
    pub(super) data: Vec<CatalogWeapon>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CatalogWeapon {
    pub(super) slug: String,
    #[serde(default)]
    pub(super) group: WeaponGroup,
    #[serde(default)]
    pub(super) riven_type: RivenType,
    #[serde(default)]
    pub(super) game_ref: String,
    pub(super) i18n: CatalogI18n,
}

#[derive(serde::Deserialize)]
pub(super) struct CatalogI18n {
    pub(super) en: CatalogName,
}

#[derive(serde::Deserialize)]
pub(super) struct CatalogName {
    pub(super) name: String,
    pub(super) thumb: Option<String>,
}

/// Pairs of (auctions, attributes) part files that both exist, oldest first.
pub(super) fn part_files(dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    let Ok(days) = fs::read_dir(dir.join(layout::Table::Auctions.as_str())) else {
        return out;
    };
    for day in days.flatten() {
        let Ok(parts) = fs::read_dir(day.path()) else {
            continue;
        };
        for part in parts.flatten() {
            let path = part.path();
            if path.extension().is_none_or(|e| e != "parquet") {
                continue;
            }
            let attrs = dir
                .join(layout::Table::Attributes.as_str())
                .join(day.file_name())
                .join(part.file_name());
            if attrs.exists() {
                out.push((path, attrs));
            }
        }
    }
    out.sort();
    out
}

/// Parquet parts of the `gone` table, oldest first.
pub(super) fn gone_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(days) = fs::read_dir(dir.join(layout::Table::Gone.as_str())) else {
        return out;
    };
    for day in days.flatten() {
        let Ok(parts) = fs::read_dir(day.path()) else {
            continue;
        };
        out.extend(
            parts
                .flatten()
                .map(|p| p.path())
                .filter(|p| p.extension().is_some_and(|e| e == "parquet")),
        );
    }
    out.sort();
    out
}

pub(super) type LoadResult<T> = anyhow::Result<T>;

pub(super) fn read_batches(path: &Path) -> LoadResult<ParquetRecordBatchReader> {
    Ok(ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?
        .with_batch_size(READ_BATCH_SIZE)
        .build()?)
}

/// A column of a batch as its array type, by its name from `wfmt_dataset::schema`.
macro_rules! column {
    ($batch:expr, $name:expr, $ty:ty) => {
        $batch
            .column_by_name($name)
            .and_then(|c| c.as_any().downcast_ref::<$ty>())
            .with_context(|| format!("missing column {}", $name))?
    };
}

// Decode and validate every batch before any live indexes are changed. In particular,
// a corrupt later row group must not leave a prefix that gets replayed on every refresh.
pub(super) fn checked_batches(
    batches: impl IntoIterator<Item = LoadResult<RecordBatch>>,
    validate: fn(&RecordBatch) -> LoadResult<()>,
) -> LoadResult<Vec<RecordBatch>> {
    batches
        .into_iter()
        .map(|batch| {
            let batch = batch?;
            validate(&batch)?;
            Ok(batch)
        })
        .collect()
}

pub(super) fn validate_attributes(b: &RecordBatch) -> LoadResult<()> {
    let _ = column!(b, attributes::SCRAPED_AT, TimestampMillisecondArray);
    let _ = column!(b, attributes::AUCTION_ID, StringArray);
    let _ = column!(b, attributes::POSITION, Int32Array);
    let _ = column!(b, attributes::ATTRIBUTE, StringArray);
    let _ = column!(b, attributes::POSITIVE, BooleanArray);
    let _ = column!(b, attributes::VALUE, Float64Array);
    let _ = column!(b, attributes::VALUE_RANK8, Float64Array);
    let _ = column!(b, attributes::QUALITY, Float64Array);
    Ok(())
}

pub(super) fn validate_auctions(b: &RecordBatch) -> LoadResult<()> {
    let _ = column!(b, auctions::SCRAPED_AT, TimestampMillisecondArray);
    let _ = column!(b, auctions::AUCTION_ID, StringArray);
    let _ = column!(b, auctions::WEAPON_SLUG, StringArray);
    let _ = column!(b, auctions::WEAPON_NAME, StringArray);
    let _ = column!(b, auctions::WEAPON_CLASS, StringArray);
    let _ = column!(b, auctions::RIVEN_NAME, StringArray);
    let _ = column!(b, auctions::POLARITY, StringArray);
    let _ = column!(b, auctions::MASTERY_LEVEL, Int64Array);
    let _ = column!(b, auctions::RE_ROLLS, Int64Array);
    let _ = column!(b, auctions::RANK, Int32Array);
    let _ = column!(b, auctions::LEGACY_STATS, BooleanArray);
    let _ = column!(b, auctions::FIT_OK, BooleanArray);
    let _ = column!(b, auctions::MEAN_POSITIVE_QUALITY, Float64Array);
    let _ = column!(b, auctions::BUYOUT_PRICE, Int64Array);
    let _ = column!(b, auctions::STARTING_PRICE, Int64Array);
    let _ = column!(b, auctions::TOP_BID, Int64Array);
    let _ = column!(b, auctions::IS_DIRECT_SELL, BooleanArray);
    let _ = column!(b, auctions::VISIBLE, BooleanArray);
    let _ = column!(b, auctions::CLOSED, BooleanArray);
    let _ = column!(b, auctions::CREATED, TimestampMillisecondArray);
    let _ = column!(b, auctions::OWNER_NAME, StringArray);
    let _ = column!(b, auctions::OWNER_REPUTATION, Int64Array);
    Ok(())
}

pub(super) fn validate_gone(b: &RecordBatch) -> LoadResult<()> {
    let _ = column!(b, gone::DETECTED_AT, TimestampMillisecondArray);
    let _ = column!(b, gone::AUCTION_ID, StringArray);
    let reason = column!(b, gone::REASON, StringArray);
    if let Some(unknown) = reason
        .iter()
        .flatten()
        .find(|r| GoneReason::parse(r).is_none())
    {
        anyhow::bail!("unknown gone reason {unknown:?}");
    }
    let _ = column!(b, gone::LAST_PRICE, Int64Array);
    let _ = column!(b, gone::DIRECT_SELL, BooleanArray);
    Ok(())
}

/// Overwrites `s` with `value`, reusing its buffer.
pub(super) fn set_str(s: &mut String, value: &str) {
    if s != value {
        s.clear();
        s.push_str(value);
    }
}

pub(super) fn opt<A: Array, T>(a: &A, i: usize, get: impl Fn(&A, usize) -> T) -> Option<T> {
    a.is_valid(i).then(|| get(a, i))
}

impl Data {
    pub(super) fn intern_attr(&mut self, slug: &str) -> AttrId {
        if let Some(i) = self.attr_index.get(slug) {
            return *i;
        }
        let i = self.attrs.push(slug.to_owned());
        self.attr_index.insert(slug.to_owned(), i);
        i
    }

    pub(super) fn intern_riven(&mut self, id: &str) -> RivenId {
        if let Some(i) = self.riven_index.get(id) {
            return *i;
        }
        let i = self.rivens.push(id.to_owned());
        self.riven_index.insert(id.to_owned(), i);
        self.riven_auctions.push(Vec::new());
        self.riven_changes.push(RivenChanges::default());
        i
    }

    pub(super) fn intern_weapon(
        &mut self,
        slug: &str,
        name: &str,
        class: Option<WeaponClass>,
    ) -> WeaponId {
        if let Some(i) = self.weapon_index.get(slug) {
            return *i;
        }
        let i = self.weapons.push(Weapon {
            slug: slug.to_owned(),
            name: name.to_owned(),
            class,
            group: WeaponGroup::Unknown,
            is_bow: false,
            thumb: None,
            last_scrape: 0,
            price_limit: None,
        });
        self.weapon_index.insert(slug.to_owned(), i);
        i
    }

    pub(super) fn load(&mut self, auctions_path: &Path, attributes_path: &Path) -> LoadResult<()> {
        self.load_parts(
            read_batches(auctions_path)?.map(|b| b.map_err(Into::into)),
            read_batches(attributes_path)?.map(|b| b.map_err(Into::into)),
        )
    }

    pub(super) fn load_parts(
        &mut self,
        auctions: impl IntoIterator<Item = LoadResult<RecordBatch>>,
        attributes: impl IntoIterator<Item = LoadResult<RecordBatch>>,
    ) -> LoadResult<()> {
        let attributes = checked_batches(attributes, validate_attributes)?;
        let auctions = checked_batches(auctions, validate_auctions)?;
        // The attributes of each auction's newest sighting in this part, with its scraped_at.
        // Older sightings never end up as an auction's current version, so their attributes
        // aren't kept. Keyed by the id alone so lookups borrow it instead of allocating.
        let mut attrs: HashMap<String, (i64, Vec<Attr>)> = HashMap::default();
        for b in attributes {
            let scraped = column!(b, attributes::SCRAPED_AT, TimestampMillisecondArray);
            let id = column!(b, attributes::AUCTION_ID, StringArray);
            let position = column!(b, attributes::POSITION, Int32Array);
            let attr = column!(b, attributes::ATTRIBUTE, StringArray);
            let positive = column!(b, attributes::POSITIVE, BooleanArray);
            let value = column!(b, attributes::VALUE, Float64Array);
            let rank8 = column!(b, attributes::VALUE_RANK8, Float64Array);
            let quality = column!(b, attributes::QUALITY, Float64Array);
            let mut rows: Vec<usize> = (0..b.num_rows()).collect();
            rows.sort_by_key(|&i| position.value(i));
            for i in rows {
                let a = Attr {
                    attr: self.intern_attr(attr.value(i)),
                    positive: positive.value(i),
                    value: value.value(i),
                    value_rank8: opt(rank8, i, Float64Array::value),
                    quality: opt(quality, i, Float64Array::value),
                };
                let (auction_id, t) = (id.value(i), scraped.value(i));
                match attrs.get_mut(auction_id) {
                    Some((at, list)) if *at == t => list.push(a),
                    Some((at, list)) if *at < t => {
                        *at = t;
                        list.clear();
                        list.push(a);
                    }
                    Some(_) => {}
                    None => {
                        attrs.insert(auction_id.to_owned(), (t, vec![a]));
                    }
                }
            }
        }

        for b in auctions {
            let scraped = column!(b, auctions::SCRAPED_AT, TimestampMillisecondArray);
            let id = column!(b, auctions::AUCTION_ID, StringArray);
            let weapon_slug = column!(b, auctions::WEAPON_SLUG, StringArray);
            let weapon_name = column!(b, auctions::WEAPON_NAME, StringArray);
            let weapon_class = column!(b, auctions::WEAPON_CLASS, StringArray);
            let riven_name = column!(b, auctions::RIVEN_NAME, StringArray);
            let polarity = column!(b, auctions::POLARITY, StringArray);
            let mastery = column!(b, auctions::MASTERY_LEVEL, Int64Array);
            let re_rolls = column!(b, auctions::RE_ROLLS, Int64Array);
            let rank = column!(b, auctions::RANK, Int32Array);
            let legacy = column!(b, auctions::LEGACY_STATS, BooleanArray);
            let fit_ok = column!(b, auctions::FIT_OK, BooleanArray);
            let mean_q = column!(b, auctions::MEAN_POSITIVE_QUALITY, Float64Array);
            let buyout = column!(b, auctions::BUYOUT_PRICE, Int64Array);
            let starting = column!(b, auctions::STARTING_PRICE, Int64Array);
            let top_bid = column!(b, auctions::TOP_BID, Int64Array);
            let direct = column!(b, auctions::IS_DIRECT_SELL, BooleanArray);
            let visible = column!(b, auctions::VISIBLE, BooleanArray);
            let closed = column!(b, auctions::CLOSED, BooleanArray);
            let created = column!(b, auctions::CREATED, TimestampMillisecondArray);
            let owner_name = column!(b, auctions::OWNER_NAME, StringArray);
            let owner_rep = column!(b, auctions::OWNER_REPUTATION, Int64Array);
            // Older parts predate riven ids; fall back to one riven per auction.
            let riven_ids = b
                .column_by_name(auctions::RIVEN_ID)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());
            let sources = b
                .column_by_name(auctions::SOURCE)
                .and_then(|c| c.as_any().downcast_ref::<StringArray>());

            for i in 0..b.num_rows() {
                if !visible.value(i) || closed.value(i) {
                    continue;
                }
                let t = scraped.value(i);
                let class = weapon_class
                    .is_valid(i)
                    .then(|| weapon_class.value(i).parse().ok())
                    .flatten();
                let weapon = self.intern_weapon(weapon_slug.value(i), weapon_name.value(i), class);
                // A feed sighting says nothing about the weapon's other listings, so it must not
                // make them look unconfirmed.
                if !sources.is_some_and(|c| {
                    c.is_valid(i) && wfmt_types::Feed::from_id(c.value(i)).is_some()
                }) {
                    let w = &mut self.weapons[weapon];
                    w.last_scrape = w.last_scrape.max(t);
                }

                let auction_id = id.value(i);
                let riven_key = riven_ids
                    .filter(|c| c.is_valid(i))
                    .map_or(auction_id, |c| c.value(i));
                let riven = self.intern_riven(riven_key);
                let buyout_price = opt(buyout, i, Int64Array::value);
                let starting_price = opt(starting, i, Int64Array::value);
                let top = opt(top_bid, i, Int64Array::value);
                let existing = self.auction_index.get(auction_id).copied();
                // Most rows are repeat sightings; an older one only moves `first_seen`, so
                // build the full auction (strings, attributes) only for rows that become the
                // latest version.
                // This row's attributes, if it is the newest sighting in the part.
                let mut take_attrs = || {
                    attrs
                        .get_mut(auction_id)
                        .filter(|(at, _)| *at == t)
                        .map(|(_, list)| std::mem::take(list))
                        .unwrap_or_default()
                };
                let index = match existing {
                    // An older sighting only moves `first_seen`.
                    Some(index) if t < self.auctions[index].last_seen => {
                        let a = &mut self.auctions[index];
                        a.first_seen = a.first_seen.min(t);
                        index
                    }
                    // A newer one replaces the auction's details. Parts load oldest first, so
                    // this is the common case: update in place, reusing the string buffers.
                    Some(index) => {
                        let a = &mut self.auctions[index];
                        let old_riven = a.riven;
                        a.weapon = weapon;
                        a.riven = riven;
                        a.gone = None;
                        set_str(&mut a.riven_name, riven_name.value(i));
                        set_str(&mut a.polarity, polarity.value(i));
                        a.mastery_level = opt(mastery, i, Int64Array::value);
                        a.re_rolls = opt(re_rolls, i, Int64Array::value);
                        a.rank = opt(rank, i, Int32Array::value);
                        a.legacy_stats = opt(legacy, i, BooleanArray::value);
                        a.fit_ok = fit_ok.value(i);
                        a.mean_quality = opt(mean_q, i, Float64Array::value);
                        set_str(&mut a.owner_name, owner_name.value(i));
                        a.owner_reputation = opt(owner_rep, i, Int64Array::value);
                        a.created = created.value(i);
                        a.first_seen = a.first_seen.min(t);
                        a.last_seen = t;
                        a.attrs = take_attrs();
                        a.buyout_price = buyout_price;
                        a.starting_price = starting_price;
                        a.top_bid = top;
                        a.direct_sell = direct.value(i);
                        if old_riven != riven {
                            self.riven_auctions[old_riven].retain(|&a| a != index);
                            self.riven_auctions[riven].push(index);
                        }
                        index
                    }
                    None => {
                        let index = self.auctions.push(Auction {
                            id: auction_id.to_owned(),
                            weapon,
                            riven,
                            gone: None,
                            riven_name: riven_name.value(i).to_owned(),
                            polarity: polarity.value(i).to_owned(),
                            mastery_level: opt(mastery, i, Int64Array::value),
                            re_rolls: opt(re_rolls, i, Int64Array::value),
                            rank: opt(rank, i, Int32Array::value),
                            legacy_stats: opt(legacy, i, BooleanArray::value),
                            fit_ok: fit_ok.value(i),
                            mean_quality: opt(mean_q, i, Float64Array::value),
                            owner_name: owner_name.value(i).to_owned(),
                            owner_reputation: opt(owner_rep, i, Int64Array::value),
                            created: created.value(i),
                            first_seen: t,
                            last_seen: t,
                            attrs: take_attrs(),
                            buyout_price,
                            starting_price,
                            top_bid: top,
                            direct_sell: direct.value(i),
                        });
                        self.auction_index.insert(auction_id.to_owned(), index);
                        self.riven_auctions[riven].push(index);
                        index
                    }
                };
                self.observations.push(Observation::new(
                    t,
                    index,
                    price_of(buyout_price, top, starting_price),
                    direct.value(i),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn load_gone(&mut self, path: &Path) -> LoadResult<()> {
        self.load_gone_batches(read_batches(path)?.map(|b| b.map_err(Into::into)))
    }

    pub(super) fn load_gone_batches(
        &mut self,
        batches: impl IntoIterator<Item = LoadResult<RecordBatch>>,
    ) -> LoadResult<()> {
        for b in checked_batches(batches, validate_gone)? {
            let detected = column!(b, gone::DETECTED_AT, TimestampMillisecondArray);
            let id = column!(b, gone::AUCTION_ID, StringArray);
            let reason = column!(b, gone::REASON, StringArray);
            let price = column!(b, gone::LAST_PRICE, Int64Array);
            let direct = column!(b, gone::DIRECT_SELL, BooleanArray);
            for i in 0..b.num_rows() {
                let Some(&auction) = self.auction_index.get(id.value(i)) else {
                    continue;
                };
                let t = detected.value(i);
                let listing = &mut self.auctions[auction];
                if listing.gone.is_none_or(|previous| t >= previous.at) {
                    listing.gone = Some(Gone {
                        at: t,
                        // Checked by `validate_gone`.
                        reason: GoneReason::parse(reason.value(i))
                            .context("unknown gone reason")?,
                    });
                }
                self.gone.push(GoneEvent {
                    t,
                    auction,
                    price: opt(price, i, Int64Array::value),
                    direct_sell: direct.value(i),
                });
            }
        }
        Ok(())
    }

    pub(super) fn finish_load(&mut self) {
        self.observations.sort_by_key(|o| o.t);
        self.gone.sort_by_key(|g| g.t);

        self.riven_changes = self.rivens.same_ids(RivenChanges::default());
        let mut first_prices = self.rivens.same_ids(None);
        for o in &self.observations {
            let riven = self.auctions[o.auction].riven;
            // Missing prices aren't evidence of a change; compare known numeric prices only.
            if let Some(price) = o.price() {
                let first = first_prices[riven].get_or_insert(price);
                self.riven_changes[riven].price |= *first != price;
            }
        }
        for (riven, auctions) in self.riven_auctions.enumerate() {
            let mut sellers = auctions
                .iter()
                .map(|&i| self.auctions[i].owner_name.trim())
                .filter(|name| !name.is_empty());
            if let Some(first) = sellers.next() {
                self.riven_changes[riven].owner =
                    sellers.any(|name| !name.eq_ignore_ascii_case(first));
            }
        }

        let mut prices = self.weapons.same_ids(Vec::new());
        for a in &self.auctions {
            prices[a.weapon].extend(auction_price(a));
        }
        for (weapon, mut prices) in self.weapons.iter_mut().zip(prices) {
            weapon.price_limit = price_limit(&mut prices);
        }
        self.rows = self.build_rows();
    }

    pub(super) fn build_rows(&self) -> Table<AuctionId, Row> {
        self.auctions
            .iter()
            .map(|a| {
                let mut flags = RowFlags::default();
                flags.set(RowFlags::FIT_OK, a.fit_ok);
                flags.set(RowFlags::LEGACY, a.legacy_stats == Some(true));
                flags.set(RowFlags::DIRECT, a.direct_sell);
                flags.set(RowFlags::CURRENT, self.is_current(a));
                flags.set(RowFlags::CURSED, a.attrs.iter().any(|x| !x.positive));
                Row {
                    price: auction_price(a).unwrap_or(Row::NO_PRICE),
                    limit: self.weapons[a.weapon].price_limit.unwrap_or(i64::MAX),
                    created: a.created,
                    mean_quality: a.mean_quality.unwrap_or(f64::NAN),
                    riven: a.riven,
                    weapon: a.weapon,
                    flags,
                }
            })
            .collect()
    }

    /// The rows, rebuilt here if the auctions changed since `finish_load` (only in tests).
    pub(super) fn rows(&self) -> std::borrow::Cow<'_, Table<AuctionId, Row>> {
        if self.rows.len() == self.auctions.len() {
            std::borrow::Cow::Borrowed(&self.rows)
        } else {
            std::borrow::Cow::Owned(self.build_rows())
        }
    }
}
