//! Turns raw auctions into flat rows: one per auction, one per riven attribute.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::identity::{IdAttr, IdInput, Registry};
use crate::riven::{self, AttrInput, RivenInput, WeaponClass};
use crate::wfm::{Auction, Weapon};

/// Dispositions as listed on warframe.market right after the rework (still pre-rework values).
const LEGACY_DISPOSITIONS: &str = include_str!("../assets/legacy_dispositions.json");

pub struct AuctionRow {
    pub scraped_at: DateTime<Utc>,
    pub platform: String,
    pub auction_id: String,
    /// Stable identity of the riven itself, shared by relists and resales (see `identity`).
    pub riven_id: String,
    /// Which sort order(s) of the search returned this auction, e.g. `price_asc`.
    pub source: String,
    pub weapon_slug: String,
    pub weapon_name: String,
    pub weapon_class: Option<&'static str>,
    pub riven_type: String,
    pub is_bow: bool,
    pub riven_name: String,
    pub polarity: String,
    pub mastery_level: Option<i64>,
    pub re_rolls: Option<i64>,
    pub n_positive: i32,
    pub n_negative: i32,
    pub has_spliced: bool,
    pub listed_rank: i64,
    pub rank: Option<i32>,
    pub disposition: Option<f64>,
    pub legacy_stats: Option<bool>,
    pub fit_ok: bool,
    pub fit_ambiguous: Option<bool>,
    pub fit_error: Option<String>,
    pub mean_positive_quality: Option<f64>,
    pub buyout_price: Option<i64>,
    pub starting_price: Option<i64>,
    pub top_bid: Option<i64>,
    pub is_direct_sell: bool,
    pub minimal_reputation: Option<i64>,
    pub visible: bool,
    pub closed: bool,
    pub private: bool,
    pub crossplay: bool,
    pub created: DateTime<Utc>,
    pub updated: DateTime<Utc>,
    pub owner_id: String,
    pub owner_name: String,
    pub owner_reputation: Option<i64>,
    pub owner_status: String,
    pub owner_last_seen: Option<DateTime<Utc>>,
    pub note: String,
}

impl AuctionRow {
    /// Same as `Auction::price`.
    pub fn price(&self) -> Option<i64> {
        self.buyout_price.or(self.top_bid).or(self.starting_price)
    }
}

pub struct AttributeRow {
    pub scraped_at: DateTime<Utc>,
    pub auction_id: String,
    pub weapon_slug: String,
    pub position: i32,
    pub attribute: String,
    pub positive: bool,
    pub spliced: Option<bool>,
    pub value: f64,
    pub value_rank8: Option<f64>,
    pub roll: Option<f64>,
    pub quality: Option<f64>,
    pub coefficient: Option<f64>,
    pub layout_multiplier: Option<f64>,
}

pub struct Normalizer {
    legacy: HashMap<String, f64>,
    rework_cutoff: DateTime<Utc>,
}

impl Normalizer {
    pub fn new(rework_cutoff: DateTime<Utc>) -> Self {
        let legacy = serde_json::from_str(LEGACY_DISPOSITIONS).expect("bundled dispositions");
        Self {
            legacy,
            rework_cutoff,
        }
    }

    fn legacy_disposition(&self, weapon: &Weapon) -> Option<f64> {
        self.legacy
            .get(&weapon.slug)
            .copied()
            .or(weapon.disposition)
            .filter(|d| (d - riven::DISPOSITION).abs() > 1e-9)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn normalize(
        &self,
        weapon: &Weapon,
        platform: &str,
        scraped_at: DateTime<Utc>,
        auctions: &[(Auction, String)],
        registry: &mut Registry,
        auction_rows: &mut Vec<AuctionRow>,
        attribute_rows: &mut Vec<AttributeRow>,
    ) {
        let class = WeaponClass::from_wfm(&weapon.riven_type, &weapon.group);
        let legacy = self.legacy_disposition(weapon);

        for (auction, source) in auctions {
            let item = &auction.item;
            let inputs: Vec<AttrInput> = item
                .attributes
                .iter()
                .map(|a| AttrInput {
                    slug: &a.url_name,
                    value: a.value,
                    positive: a.positive,
                })
                .collect();
            let fit = match class {
                Some(class) => riven::fit(&RivenInput {
                    class,
                    is_bow: weapon.is_bow(),
                    listed_rank: item.mod_rank,
                    attributes: &inputs,
                    legacy_disposition: legacy,
                    prefer_legacy: auction.created < self.rework_cutoff,
                })
                .map_err(|e| e.to_string()),
                None => Err(format!("unknown riven type {}", weapon.riven_type)),
            };
            let fit = fit.as_ref();

            let n_positive = inputs.iter().filter(|a| a.positive).count();
            let positive_qualities: Vec<f64> = fit
                .map(|f| {
                    f.attributes
                        .iter()
                        .zip(&inputs)
                        .filter(|(_, i)| i.positive)
                        .map(|(a, _)| a.quality)
                        .collect()
                })
                .unwrap_or_default();

            for (position, a) in item.attributes.iter().enumerate() {
                let attr_fit = fit.ok().map(|f| &f.attributes[position]);
                attribute_rows.push(AttributeRow {
                    scraped_at,
                    auction_id: auction.id.clone(),
                    weapon_slug: weapon.slug.clone(),
                    position: position as i32,
                    attribute: a.url_name.clone(),
                    positive: a.positive,
                    spliced: riven::attribute(&a.url_name).map(|s| s.spliced),
                    value: a.value,
                    value_rank8: attr_fit.map(|f| f.value_rank8),
                    roll: attr_fit.map(|f| f.roll),
                    quality: attr_fit.map(|f| f.quality),
                    coefficient: attr_fit.map(|f| f.coefficient),
                    layout_multiplier: attr_fit.map(|f| f.layout_multiplier),
                });
            }

            let id_attrs: Vec<IdAttr> = item
                .attributes
                .iter()
                .enumerate()
                .map(|(i, a)| IdAttr {
                    slug: &a.url_name,
                    positive: a.positive,
                    value: a.value,
                    roll: fit
                        .ok()
                        .map(|f| (f.attributes[i].roll, f.attributes[i].roll_tolerance)),
                })
                .collect();
            let riven_id = registry.identify(&IdInput {
                weapon: &weapon.slug,
                polarity: &item.polarity,
                mastery_level: item.mastery_level,
                re_rolls: item.re_rolls,
                listed_rank: item.mod_rank,
                owner: &auction.owner.id,
                attributes: &id_attrs,
            });

            auction_rows.push(AuctionRow {
                riven_id,
                source: source.clone(),
                scraped_at,
                platform: if auction.platform.is_empty() {
                    platform.to_owned()
                } else {
                    auction.platform.clone()
                },
                auction_id: auction.id.clone(),
                weapon_slug: weapon.slug.clone(),
                weapon_name: weapon.name().to_owned(),
                weapon_class: class.map(WeaponClass::as_str),
                riven_type: weapon.riven_type.clone(),
                is_bow: weapon.is_bow(),
                riven_name: item.name.clone(),
                polarity: item.polarity.clone(),
                mastery_level: item.mastery_level,
                re_rolls: item.re_rolls,
                n_positive: n_positive as i32,
                n_negative: (inputs.len() - n_positive) as i32,
                has_spliced: inputs
                    .iter()
                    .any(|a| riven::attribute(a.slug).is_some_and(|s| s.spliced)),
                listed_rank: item.mod_rank,
                rank: fit.ok().map(|f| i32::from(f.rank)),
                disposition: fit.ok().map(|f| f.disposition),
                legacy_stats: fit.ok().map(|f| f.legacy),
                fit_ok: fit.is_ok(),
                fit_ambiguous: fit.ok().map(|f| f.ambiguous),
                fit_error: fit.err().cloned(),
                mean_positive_quality: (!positive_qualities.is_empty()).then(|| {
                    positive_qualities.iter().sum::<f64>() / positive_qualities.len() as f64
                }),
                buyout_price: auction.buyout_price,
                starting_price: auction.starting_price,
                top_bid: auction.top_bid(),
                is_direct_sell: auction.is_direct_sell,
                minimal_reputation: auction.minimal_reputation,
                visible: auction.visible,
                closed: auction.closed,
                private: auction.private,
                crossplay: auction.crossplay,
                created: auction.created,
                updated: auction.updated,
                owner_id: auction.owner.id.clone(),
                owner_name: auction.owner.ingame_name.clone(),
                owner_reputation: auction.owner.reputation,
                owner_status: auction.owner.status.clone(),
                owner_last_seen: auction.owner.last_seen,
                note: auction.note.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wfm::parse_auctions;
    use std::io::Read;

    #[test]
    fn normalizes_fixture() {
        let mut body = String::new();
        flate2::read::GzDecoder::new(
            &include_bytes!("../tests/fixtures/kulstar_auctions.json.gz")[..],
        )
        .read_to_string(&mut body)
        .unwrap();
        let (auctions, skipped) = parse_auctions(&body).unwrap();
        assert_eq!(skipped, 0);
        let auctions: Vec<(Auction, String)> = auctions
            .into_iter()
            .map(|a| (a, "price_asc".to_owned()))
            .collect();

        let weapon = Weapon {
            slug: "kulstar".into(),
            game_ref: "/Lotus/Weapons/Grineer/Pistols/GrnTorpedoPistol/GrnTorpedoPistol".into(),
            group: "secondary".into(),
            riven_type: "pistol".into(),
            disposition: Some(1.3),
            i18n: serde_json::json!({"en": {"name": "Kulstar"}}),
        };
        let cutoff = "2026-10-22T00:00:00Z".parse().unwrap();
        let (mut rows, mut attrs) = (Vec::new(), Vec::new());
        Normalizer::new(cutoff).normalize(
            &weapon,
            "pc",
            Utc::now(),
            &auctions,
            &mut Registry::in_memory(),
            &mut rows,
            &mut attrs,
        );

        assert_eq!(rows.len(), auctions.len());
        let ok = rows.iter().filter(|r| r.fit_ok).count();
        let legacy = rows.iter().filter(|r| r.legacy_stats == Some(true)).count();
        // 95 of 100 fit; the rest are "1.0 / 1.0" style placeholders.
        assert!(ok >= 90, "only {ok} fitted");
        assert!(legacy > 0);
        assert!(
            attrs
                .iter()
                .filter_map(|a| a.quality)
                .all(|q| (0.0..=1.0).contains(&q))
        );
    }
}
