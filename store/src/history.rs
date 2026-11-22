//! Historical sightings live in Parquet. Keep only the summaries needed by listing filters
//! and the chart clock between requests. DuckDB projects the price columns on demand.
use super::*;
use anyhow::{Result, ensure};
use duckdb::{Connection, types::Value};
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct History {
    pub files: Vec<Part>,
    count: usize,
    times: BTreeSet<i64>,
    pub prices: HashMap<AuctionId, (i64, i64)>,
    // Auctions whose observed chart state ever differed from their latest state.
    pub variable: HashSet<AuctionId>,
    pub changed_weapon: HashSet<AuctionId>,
}

pub(super) struct Part {
    path: PathBuf,
    modified: SystemTime,
    bytes: u64,
    pub bounds: Option<(i64, i64)>,
}

impl Part {
    pub fn new(path: &Path) -> Result<Self> {
        let meta = path.metadata()?;
        Ok(Self {
            path: path.to_owned(),
            modified: meta.modified()?,
            bytes: meta.len(),
            bounds: None,
        })
    }

    // Parts are immutable. Compaction/import can replace them while this version of the
    // listing index is still being served. Reject a mixed snapshot and allow a fresh request.
    pub(super) fn verify(&self) -> Result<()> {
        let meta = self.path.metadata()?;
        ensure!(
            self.bytes == meta.len() && self.modified == meta.modified()?,
            "market data changed during the history query"
        );
        Ok(())
    }
}

impl History {
    pub fn record(&mut self, t: i64, auction: AuctionId, price: Option<i64>) {
        self.count += 1;
        self.times.insert(t);
        if let Some(p) = price {
            let range = self.prices.entry(auction).or_insert((p, p));
            range.0 = range.0.min(p);
            range.1 = range.1.max(p);
        }
    }
}

fn connection() -> Result<Connection> {
    static DB: Mutex<Option<Connection>> = Mutex::new(None);
    let mut engine = DB.lock().unwrap_or_else(PoisonError::into_inner);
    let db = match engine.as_ref() {
        Some(db) => db,
        None => engine.insert(Connection::open_in_memory()?),
    };
    Ok(db.try_clone()?)
}

impl Data {
    pub(super) fn observation_count(&self) -> usize {
        #[cfg(test)]
        if !self.observations.is_empty() {
            return self.observations.len();
        }
        self.history.count
    }

    pub(super) fn observation_bounds(&self, start: Option<i64>) -> (Option<i64>, Option<i64>) {
        #[cfg(test)]
        if !self.observations.is_empty() {
            let i = start.map_or(0, |s| self.observations.partition_point(|o| o.t < s));
            return (
                self.observations.get(i).map(|o| o.t),
                self.observations.last().map(|o| o.t),
            );
        }
        (
            self.history
                .times
                .range(start.unwrap_or(i64::MIN)..)
                .next()
                .copied(),
            self.history.times.last().copied(),
        )
    }

    /// All sightings for a journey, or the last sighting of each auction per chart bucket.
    /// Older sightings feed the first bucket so its opening prices carry forward correctly.
    pub(super) fn read_history(
        &self,
        selected: &[AuctionId],
        chart: Option<(i64, i64, i64)>,
    ) -> Result<Vec<Observation>> {
        if selected.is_empty() {
            return Ok(vec![]);
        }
        #[cfg(test)]
        if self.history.files.is_empty() {
            let mut events: Vec<_> = self
                .observations
                .iter()
                .filter(|o| selected.contains(&o.auction))
                .copied()
                .collect();
            events.sort_by_key(|o| o.t);
            return Ok(events);
        }
        if let Some((first, _, width)) = chart {
            // A constant asking price is known from the first sighting onwards. Listings
            // last seen in or before the opening bucket also have a known opening price.
            // Only the remaining auctions need a historical price scan.
            let mut known = Vec::new();
            let mut unknown = Vec::new();
            for &i in selected {
                let a = &self.auctions[i];
                if !self.history.variable.contains(&i) || a.last_seen < first.saturating_add(width)
                {
                    known.push(Observation::new(
                        a.first_seen,
                        i,
                        auction_price(a),
                        a.direct_sell,
                    ));
                } else {
                    unknown.push(i);
                }
            }
            if !unknown.is_empty() {
                known.extend(self.scan_history(&unknown, chart)?);
            }
            known.sort_unstable_by_key(|o| o.t);
            return Ok(known);
        }
        self.scan_history(selected, chart)
    }

    fn scan_history(
        &self,
        selected: &[AuctionId],
        chart: Option<(i64, i64, i64)>,
    ) -> Result<Vec<Observation>> {
        let earliest = selected
            .iter()
            .map(|&i| self.auctions[i].first_seen)
            .min()
            .unwrap();
        let latest = selected
            .iter()
            .map(|&i| self.auctions[i].last_seen)
            .max()
            .unwrap();
        let chart_end = chart.map_or(latest, |(_, last, width)| {
            (last - last.rem_euclid(width))
                .saturating_add(width)
                .saturating_sub(1)
        });
        let latest = latest.min(chart_end);
        let parts: Vec<_> = self
            .history
            .files
            .iter()
            .filter(|p| {
                p.bounds
                    .is_some_and(|(lo, hi)| lo <= latest && hi >= earliest)
            })
            .collect();
        if parts.is_empty() {
            return Ok(vec![]);
        }
        let mut paths = Vec::with_capacity(parts.len());
        for part in &parts {
            part.verify()?;
            paths.push(Value::Text(
                part.path
                    .to_str()
                    .context("Parquet path is not UTF-8")?
                    .to_owned(),
            ));
        }

        let mut args = vec![Value::List(paths), earliest.into(), latest.into()];
        let mut scan = String::from(
            "SELECT auction_id, epoch_ms(scraped_at) AS t,
            coalesce(buyout_price, top_bid, starting_price) AS price, is_direct_sell AS direct
            FROM read_parquet(?, hive_partitioning=false)
            WHERE visible AND NOT closed AND scraped_at BETWEEN epoch_ms(?) AND epoch_ms(?)",
        );
        let weapon = self.auctions[selected[0]].weapon;
        if selected.iter().all(|&i| {
            self.auctions[i].weapon == weapon && !self.history.changed_weapon.contains(&i)
        }) {
            scan.push_str(" AND weapon_slug = ?");
            args.push(self.weapons[weapon].slug.clone().into());
        }
        let db = connection()?;
        if selected.len() != self.auctions.len() {
            // A short IN list lets the reader prune row groups by auction id. Use a
            // hash semi-join for a large selection instead of thousands of placeholders.
            if selected.len() <= 32 {
                let slots = vec!["?"; selected.len()].join(",");
                scan.push_str(&format!(" AND auction_id IN ({slots})"));
                args.extend(
                    selected
                        .iter()
                        .map(|&i| Value::Text(self.auctions[i].id.clone())),
                );
            } else {
                db.execute_batch("CREATE TEMP TABLE selected_auctions(id VARCHAR)")?;
                let mut appender =
                    db.appender_to_catalog_and_db("selected_auctions", "temp", "main")?;
                for &i in selected {
                    appender.append_row([self.auctions[i].id.as_str()])?;
                }
                appender.flush()?;
                scan.push_str(" AND auction_id IN (SELECT id FROM selected_auctions)");
            }
        }
        let sql = if let Some((first, _, width)) = chart {
            args.push(Value::BigInt(first));
            // Keep null-price withdrawals inside the struct. A bare arg_max(price,t)
            // would silently resurrect the previous price when the newest one is NULL.
            format!("WITH sightings AS ({scan}), buckets AS (
                SELECT auction_id, max(t) AS t, arg_max(struct_pack(price := price, direct := direct), t) AS state
                FROM sightings GROUP BY auction_id, greatest(t - ((t % {width} + {width}) % {width}), ?)
            ) SELECT auction_id,t,state.price,state.direct FROM buckets
              QUALIFY state IS DISTINCT FROM lag(state) OVER (PARTITION BY auction_id ORDER BY t)
              ORDER BY t")
        } else {
            format!("{scan} ORDER BY t")
        };
        let mut statement = db.prepare(&sql)?;
        let mut rows = statement.query(duckdb::params_from_iter(args))?;
        let mut events = Vec::new();
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let auction = *self
                .auction_index
                .get(&id)
                .context("auction changed during history query")?;
            events.push(Observation::new(
                row.get(1)?,
                auction,
                row.get(2)?,
                row.get(3)?,
            ));
        }
        for part in parts {
            part.verify()?;
        }
        Ok(events)
    }
}
