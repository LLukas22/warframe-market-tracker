//! Notification matches use observations, never historical bids or inferred prices.
use super::*;
use wfmt_api::{AlertFilter, AlertListing};

impl Data {
    pub fn alert_matches(
        &self,
        filters: &[AlertFilter],
        now: i64,
    ) -> (Vec<AlertListing>, Option<i64>) {
        let latest = self.auctions.iter().map(|a| a.last_seen).max();
        let cutoff = now.saturating_sub(15 * 60 * 1000);
        let listings = self
            .auctions
            .iter()
            .filter_map(|a| {
                let price = a.buyout_price.filter(|p| *p > 0)?;
                if !a.direct_sell || !self.is_current(a) || a.last_seen < cutoff {
                    return None;
                }
                let weapon = &self.weapons[a.weapon];
                if !filters.iter().any(|f| {
                    f.weapon == weapon.slug
                        && price <= f.max_price
                        && f.status.matches(&a.owner_status)
                        && (f.min_reputation == 0
                            || a.owner_reputation.is_some_and(|r| r >= f.min_reputation))
                }) {
                    return None;
                }
                Some(AlertListing {
                    auction_id: a.id.clone(),
                    weapon: weapon.slug.clone(),
                    weapon_name: weapon.name.clone(),
                    price,
                    seller: a.owner_name.clone(),
                    status: a.owner_status.clone(),
                    reputation: a.owner_reputation,
                    platform: a.platform.clone(),
                    observed_at: a.last_seen,
                })
            })
            .collect();
        (listings, latest)
    }
}
