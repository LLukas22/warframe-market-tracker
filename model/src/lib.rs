//! Riven stat model after the disposition removal (hotfix 44.1.3, 2026-10-22).
//!
//! Source: https://wiki.warframe.com/w/Riven_Mods ("Attribute Value Formula").
//!
//! ```text
//! v = b × 10 × (r + 1) × D × L × W        W = 0.9 + 0.2u,  0 ≤ u ≤ 1
//! ```
//!
//! * `b` base coefficient of the attribute for the weapon class
//! * `r` riven rank (0..=8)
//! * `D` disposition, now the constant 1.2 for every weapon
//! * `L` bonus/malus multiplier from the positive/negative layout
//! * `W` random roll, `u` is the normalized roll quality
//!
//! Display conversion: percentages show `100v`, faction damage shows `×(1 + v)`,
//! a few attributes (punch through, range, combo duration, initial combo) show `v` as-is.

use wfmt_types::riven::{ROLL_MAX, ROLL_MIN};

pub const DISPOSITION: f64 = 1.2;
pub const MAX_RANK: u8 = 8;

use std::sync::LazyLock;

use wfmt_types::attributes;
pub use wfmt_types::{Unit, WeaponClass};

/// Half a unit of the precision listings are entered with, in display units.
fn rounding_slack(unit: Unit) -> f64 {
    match unit {
        Unit::Percent | Unit::Raw => 0.05,
        Unit::Multiply => 0.005,
    }
}

/// An attribute the stat model can fit: the shared description plus its coefficients.
pub struct AttributeSpec {
    /// warframe.market attribute slug.
    pub slug: &'static str,
    pub unit: Unit,
    /// Only obtainable through the Riven Splicer.
    pub spliced: bool,
    /// |b| per class: rifle, shotgun, pistol/kitgun, archgun, melee/zaw.
    pub coefficients: Coefficients,
}

type Coefficients = [Option<f64>; 5];

impl AttributeSpec {
    pub fn coefficient(&self, class: WeaponClass) -> Option<f64> {
        self.coefficients[class as usize]
    }
}

const fn spec(slug: &'static str, coefficients: Coefficients) -> (&'static str, Coefficients) {
    (slug, coefficients)
}

const N: Option<f64> = None;
const fn c(v: f64) -> Option<f64> {
    Some(v)
}
const fn all(v: f64) -> [Option<f64>; 5] {
    [c(v), c(v), c(v), c(v), c(v)]
}
const fn guns(v: f64) -> [Option<f64>; 5] {
    [c(v), c(v), c(v), c(v), N]
}
const fn melee(v: f64) -> [Option<f64>; 5] {
    [N, N, N, N, c(v)]
}

/// Base coefficients (magnitudes) from the wiki tables, for every attribute in
/// `wfmt_types::attributes`. Signs are irrelevant because warframe.market reports the sign
/// through `positive` and we fit on magnitudes.
static COEFFICIENTS: &[(&str, Coefficients)] = &[
    spec("chance_to_gain_extra_combo_count", melee(0.00653)),
    spec("chance_to_gain_combo_count", melee(0.01165)),
    spec("ammo_maximum", [c(0.00555), c(0.01), c(0.01), c(0.0111), N]),
    spec("damage_vs_corpus", all(0.005)),
    spec("damage_vs_grineer", all(0.005)),
    spec("damage_vs_infested", all(0.005)),
    spec(
        "cold_damage",
        [c(0.01), c(0.01), c(0.01), c(0.0133), c(0.01)],
    ),
    spec("combo_duration", melee(0.09)),
    spec(
        "critical_chance",
        [c(0.016666), c(0.01), c(0.016666), c(0.0111), c(0.02)],
    ),
    spec("critical_chance_on_slide_attack", melee(0.013334)),
    spec(
        "critical_damage",
        [c(0.013333), c(0.01), c(0.01), c(0.0089), c(0.01)],
    ),
    spec(
        "base_damage_/_melee_damage",
        [c(0.0183333), c(0.0183), c(0.0244), c(0.0111), c(0.0183)],
    ),
    spec(
        "electric_damage",
        [c(0.01), c(0.01), c(0.01), c(0.0133), c(0.01)],
    ),
    spec(
        "heat_damage",
        [c(0.01), c(0.01), c(0.01), c(0.0133), c(0.01)],
    ),
    spec("finisher_damage", melee(0.0133)),
    spec(
        "fire_rate_/_attack_speed",
        [c(0.00667), c(0.01), c(0.0083), c(0.00667), c(0.0061)],
    ),
    spec("projectile_speed", [c(0.01), c(0.01), c(0.01), N, N]),
    spec("channeling_damage", melee(0.27224)),
    spec(
        "impact_damage",
        [c(0.013333), c(0.013333), c(0.013333), c(0.01), c(0.0133)],
    ),
    spec(
        "magazine_capacity",
        [c(0.005555), c(0.005555), c(0.005555), c(0.0067), N],
    ),
    spec("channeling_efficiency", melee(0.00816)),
    spec("multishot", [c(0.01), c(0.0133), c(0.0133), c(0.0067), N]),
    spec(
        "toxin_damage",
        [c(0.01), c(0.01), c(0.01), c(0.0133), c(0.01)],
    ),
    spec("punch_through", guns(0.03)),
    spec(
        "puncture_damage",
        [c(0.01333), c(0.01333), c(0.01333), c(0.01), c(0.0133)],
    ),
    spec(
        "reload_speed",
        [c(0.005555), c(0.005555), c(0.005555), c(0.0111), N],
    ),
    spec("range", melee(0.02158)),
    spec(
        "slash_damage",
        [c(0.013333), c(0.013333), c(0.013333), c(0.01), c(0.0133)],
    ),
    spec(
        "status_chance",
        [c(0.01), c(0.01), c(0.01), c(0.0067), c(0.01)],
    ),
    spec("status_duration", all(0.01111)),
    spec("recoil", guns(0.01)),
    spec(
        "zoom",
        [c(0.006666), c(0.004666), c(0.0089), c(0.006666), N],
    ),
    // Spliced attributes (Riven Splicer), always positive.
    spec("ammo_efficiency", guns(0.005)),
    spec("blast", all(0.01)),
    spec("corrosive", all(0.01)),
    spec("damage_to_orokin", all(0.005)),
    spec("damage_to_scaldra", all(0.005)),
    spec("damage_to_techrot", all(0.005)),
    spec("gas", all(0.01)),
    spec("heavy_attack_wind_up_speed", melee(0.0133)),
    spec("magnetic", all(0.01)),
    spec("melee_damage_on_heavy_attack", melee(0.0133)),
    // Shown without a percent sign in game, but it is a percentage.
    spec("parry_angle", melee(0.009)),
    spec("radiation", all(0.01)),
    spec(
        "magazine_reloaded_s_when_holstered",
        [c(0.01), c(0.01), c(0.01), N, N],
    ),
    spec("slam_attack_damage", melee(0.0133)),
    spec("status_damage", all(0.01)),
    spec("viral", all(0.01)),
    spec("weak_point_damage", guns(0.025)),
    spec("weak_point_critical_chance", guns(0.0275)),
];

pub static ATTRIBUTES: LazyLock<Vec<AttributeSpec>> = LazyLock::new(|| {
    COEFFICIENTS
        .iter()
        .map(|&(slug, coefficients)| {
            let info = attributes::attribute(slug)
                .unwrap_or_else(|| panic!("{slug} is missing from wfmt_types::attributes"));
            AttributeSpec {
                slug,
                unit: info.unit,
                spliced: info.spliced,
                coefficients,
            }
        })
        .collect()
});

pub fn attribute(slug: &str) -> Option<&'static AttributeSpec> {
    ATTRIBUTES.iter().find(|a| a.slug == slug)
}

/// Bonus and malus multiplier (`L`) for a positive/negative layout.
pub fn layout_multipliers(positives: usize, negatives: usize) -> Option<(f64, f64)> {
    match (positives, negatives) {
        (2, 0) => Some((0.99, 0.0)),
        (2, 1) => Some((1.2375, 0.495)),
        (3, 0) => Some((0.75, 0.0)),
        (3, 1) => Some((0.9375, 0.75)),
        _ => None,
    }
}

#[derive(Clone, Debug)]
pub struct AttrInput<'a> {
    pub slug: &'a str,
    /// Value as listed on warframe.market (percent, raw, or multiplier for faction damage).
    pub value: f64,
    pub positive: bool,
}

#[derive(Clone, Debug)]
pub struct RivenInput<'a> {
    pub class: WeaponClass,
    /// Bows show fire rate doubled.
    pub is_bow: bool,
    pub listed_rank: i64,
    pub attributes: &'a [AttrInput<'a>],
    /// Pre-rework disposition, used to recognise listings whose stats were never updated.
    pub legacy_disposition: Option<f64>,
    /// Try the legacy disposition first (listing created before the rework).
    pub prefer_legacy: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttrFit {
    pub spliced: bool,
    pub coefficient: f64,
    pub layout_multiplier: f64,
    /// Random roll `W` in [0.9, 1.1] (slightly outside because of listing rounding).
    pub roll: f64,
    /// How far `roll` can be off because the listed value is rounded.
    pub roll_tolerance: f64,
    /// Normalized roll quality `u` in [0, 1]. For negatives a higher value means a worse curse.
    pub quality: f64,
    /// Listed value rescaled to rank 8 with disposition 1.2, in the same display units.
    pub value_rank8: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Fit {
    pub rank: u8,
    pub disposition: f64,
    /// Stats still reflect the pre-rework disposition.
    pub legacy: bool,
    /// Another rank/disposition also fits and gives different roll qualities.
    pub ambiguous: bool,
    pub attributes: Vec<AttrFit>,
}

/// Why listed values don't fit the stat model.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum FitError {
    #[error("invalid layout {positives}+/{negatives}-")]
    Layout { positives: usize, negatives: usize },
    #[error("unknown attribute {0}")]
    UnknownAttribute(String),
    #[error("attribute {0} cannot roll on this weapon class")]
    NotForClass(String),
    #[error("values do not match any rank/disposition")]
    NoFit,
}

/// Magnitude of the modifier `v` in formula units, plus the rounding slack in the same units.
fn magnitude(unit: Unit, value: f64) -> (f64, f64) {
    let slack = rounding_slack(unit);
    match unit {
        Unit::Percent => (value.abs() / 100.0, slack / 100.0),
        Unit::Raw => (value.abs(), slack),
        // Negative faction damage is listed as e.g. 0.95; tolerate a raw delta too.
        Unit::Multiply if value > 0.0 => ((value - 1.0).abs(), slack),
        Unit::Multiply => (value.abs(), slack),
    }
}

fn rescale(unit: Unit, value: f64, factor: f64) -> f64 {
    match unit {
        Unit::Multiply if value > 0.0 => 1.0 + (value - 1.0) * factor,
        _ => value * factor,
    }
}

struct Resolved<'a> {
    input: &'a AttrInput<'a>,
    spec: &'static AttributeSpec,
    coefficient: f64,
    layout_multiplier: f64,
    magnitude: f64,
    slack: f64,
    bow_factors: &'static [f64],
}

fn try_fit(attrs: &[Resolved], rank: u8, disposition: f64) -> Option<Vec<AttrFit>> {
    let rank_scale = 10.0 * f64::from(rank + 1) * disposition;
    let to_rank8 = (10.0 * f64::from(MAX_RANK + 1) * DISPOSITION) / rank_scale;
    attrs
        .iter()
        .map(|a| {
            a.bow_factors.iter().find_map(|&factor| {
                let expected = a.coefficient * rank_scale * a.layout_multiplier * factor;
                let lo = (a.magnitude - a.slack) / expected;
                let hi = (a.magnitude + a.slack) / expected;
                if hi < ROLL_MIN - 1e-9 || lo > ROLL_MAX + 1e-9 {
                    return None;
                }
                let roll = a.magnitude / expected;
                Some(AttrFit {
                    spliced: a.spec.spliced,
                    coefficient: a.coefficient,
                    layout_multiplier: a.layout_multiplier,
                    roll,
                    roll_tolerance: a.slack / expected,
                    quality: ((roll - ROLL_MIN) / (ROLL_MAX - ROLL_MIN)).clamp(0.0, 1.0),
                    value_rank8: rescale(a.spec.unit, a.input.value, to_rank8),
                })
            })
        })
        .collect()
}

/// Recovers rank, disposition and per-attribute roll from listed values.
///
/// Sellers often list the wrong rank (most listings say rank 0 with maxed values), so all
/// ranks are tried, starting with the listed one, then 8 downwards.
pub fn fit(input: &RivenInput) -> Result<Fit, FitError> {
    let positives = input.attributes.iter().filter(|a| a.positive).count();
    let negatives = input.attributes.len() - positives;
    let (bonus, malus) = layout_multipliers(positives, negatives).ok_or(FitError::Layout {
        positives,
        negatives,
    })?;

    let resolved = input
        .attributes
        .iter()
        .map(|a| {
            let spec =
                attribute(a.slug).ok_or_else(|| FitError::UnknownAttribute(a.slug.to_owned()))?;
            let coefficient = spec
                .coefficient(input.class)
                .ok_or_else(|| FitError::NotForClass(a.slug.to_owned()))?;
            let (magnitude, slack) = magnitude(spec.unit, a.value);
            let bow_factors: &[f64] = if input.is_bow && a.slug == attributes::FIRE_RATE {
                &[2.0, 1.0]
            } else {
                &[1.0]
            };
            Ok(Resolved {
                input: a,
                spec,
                coefficient,
                layout_multiplier: if a.positive { bonus } else { malus },
                magnitude,
                slack,
                bow_factors,
            })
        })
        .collect::<Result<Vec<_>, FitError>>()?;

    let legacy = input
        .legacy_disposition
        .filter(|d| (d - DISPOSITION).abs() > 1e-9);
    let dispositions: Vec<(f64, bool)> = match legacy {
        Some(d) if input.prefer_legacy => vec![(d, true), (DISPOSITION, false)],
        Some(d) => vec![(DISPOSITION, false), (d, true)],
        None => vec![(DISPOSITION, false)],
    };
    let listed = u8::try_from(input.listed_rank)
        .ok()
        .filter(|r| *r <= MAX_RANK);
    let ranks: Vec<u8> = listed
        .into_iter()
        .chain((0..=MAX_RANK).rev().filter(|r| Some(*r) != listed))
        .collect();

    let mut candidates = dispositions.iter().flat_map(|&(disposition, legacy)| {
        let resolved = &resolved;
        ranks.iter().filter_map(move |&rank| {
            try_fit(resolved, rank, disposition).map(|attributes| Fit {
                rank,
                disposition,
                legacy,
                ambiguous: false,
                attributes,
            })
        })
    });

    let mut best = candidates.next().ok_or(FitError::NoFit)?;
    best.ambiguous = candidates.any(|other| {
        other
            .attributes
            .iter()
            .zip(&best.attributes)
            .any(|(a, b)| (a.quality - b.quality).abs() > 0.01)
    });
    Ok(best)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_attribute_has_coefficients() {
        for a in attributes::ATTRIBUTES {
            assert!(
                attribute(a.slug).is_some(),
                "{} has no coefficients",
                a.slug
            );
        }
        assert_eq!(ATTRIBUTES.len(), attributes::ATTRIBUTES.len());
    }

    fn input<'a>(attrs: &'a [AttrInput<'a>], class: WeaponClass, rank: i64) -> RivenInput<'a> {
        RivenInput {
            class,
            is_bow: false,
            listed_rank: rank,
            attributes: attrs,
            legacy_disposition: None,
            prefer_legacy: false,
        }
    }

    fn attr(slug: &str, value: f64, positive: bool) -> AttrInput<'_> {
        AttrInput {
            slug,
            value,
            positive,
        }
    }

    #[test]
    fn wiki_rank8_base_values_match_coefficients() {
        // Rank-8 base values on the wiki are b × 90.
        let cc = attribute("critical_chance").unwrap();
        assert!((cc.coefficient(WeaponClass::Rifle).unwrap() * 90.0 * 100.0 - 149.99).abs() < 0.01);
        let ms = attribute("multishot").unwrap();
        assert!(
            (ms.coefficient(WeaponClass::Shotgun).unwrap() * 90.0 * 100.0 - 119.7).abs() < 0.01
        );
    }

    #[test]
    fn fits_maxed_riven_listed_as_rank_zero() {
        // Kulstar (pistol) 3 positives listed at rank 0 with rank 8 values.
        let attrs = [
            attr("impact_damage", 115.8, true),
            attr("status_duration", 93.2, true),
            attr("projectile_speed", 77.4, true),
        ];
        let fit = fit(&input(&attrs, WeaponClass::Pistol, 0)).unwrap();
        assert_eq!(fit.rank, 8);
        assert!(!fit.legacy);
        // impact: 0.013333 × 90 × 1.2 × 0.75 = 1.08 -> 115.8 / 108 = 1.072
        assert!((fit.attributes[0].roll - 1.0722).abs() < 1e-3);
        assert!((fit.attributes[0].value_rank8 - 115.8).abs() < 1e-9);
    }

    #[test]
    fn fits_unranked_riven_and_rescales() {
        // Multishot curse on a rank 0 pistol riven with 2 positives: 0.0133 × 10 × 1.2 × 0.495.
        let attrs = [
            attr("critical_chance", 25.0, true),
            attr("status_chance", 15.0, true),
            attr("multishot", -7.8, false),
        ];
        let fit = fit(&input(&attrs, WeaponClass::Pistol, 0)).unwrap();
        assert_eq!(fit.rank, 0);
        let ms = &fit.attributes[2];
        assert!((ms.value_rank8 - -70.2).abs() < 1e-9);
    }

    #[test]
    fn faction_multiplier_rounding_is_tolerated() {
        // ×1.04 at rank 0 is only accurate to ±12%, must still fit.
        let attrs = [
            attr("ammo_maximum", 9.7, true),
            attr("damage_vs_grineer", 1.04, true),
            attr("toxin_damage", 9.7, true),
        ];
        let fit = fit(&input(&attrs, WeaponClass::Pistol, 0)).unwrap();
        assert_eq!(fit.rank, 0);
    }

    #[test]
    fn detects_legacy_disposition() {
        // 2+1- pistol fire rate curse at rank 8 with the old Kulstar disposition 1.3.
        let attrs = [
            attr("critical_chance", 250.0, true),
            attr("multishot", 200.0, true),
            attr("fire_rate_/_attack_speed", -49.1, false),
        ];
        let mut riven = input(&attrs, WeaponClass::Pistol, 8);
        assert_eq!(fit(&riven), Err(FitError::NoFit));
        riven.legacy_disposition = Some(1.3);
        let fit = fit(&riven).unwrap();
        assert!(fit.legacy);
        assert_eq!(fit.disposition, 1.3);
        assert!((fit.attributes[2].value_rank8 - -49.1 * 1.2 / 1.3).abs() < 1e-9);
    }

    #[test]
    fn rejects_placeholder_values() {
        let attrs = [attr("zoom", 1.0, true), attr("cold_damage", 1.0, true)];
        assert_eq!(
            fit(&input(&attrs, WeaponClass::Pistol, 0)),
            Err(FitError::NoFit)
        );
    }

    #[test]
    fn rejects_bad_layout_and_class() {
        let attrs = [attr("multishot", 50.0, true)];
        assert!(matches!(
            fit(&input(&attrs, WeaponClass::Rifle, 8)),
            Err(FitError::Layout { .. })
        ));
        let attrs = [attr("multishot", 50.0, true), attr("range", 1.0, true)];
        assert!(matches!(
            fit(&input(&attrs, WeaponClass::Melee, 8)),
            Err(FitError::NotForClass(_))
        ));
    }
}
