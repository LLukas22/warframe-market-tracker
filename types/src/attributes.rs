//! The riven attributes the app knows, by warframe.market slug: how the game names and shows
//! them. The stat model's coefficients live with the model, in the scraper.

use crate::Unit::{self, Multiply, Percent, Raw};

pub struct Attribute {
    /// warframe.market's slug.
    pub slug: &'static str,
    /// As the game names it.
    pub name: &'static str,
    pub unit: Unit,
    /// Shown after a raw value: `m` for meters, `s` for seconds, or nothing.
    pub suffix: &'static str,
    /// Only obtainable through the Riven Splicer, and always positive.
    pub spliced: bool,
}

const fn attr(
    slug: &'static str,
    name: &'static str,
    unit: Unit,
    suffix: &'static str,
    spliced: bool,
) -> Attribute {
    Attribute {
        slug,
        name,
        unit,
        suffix,
        spliced,
    }
}

pub static ATTRIBUTES: &[Attribute] = &[
    attr(
        "chance_to_gain_extra_combo_count",
        "Additional Combo Count Chance",
        Percent,
        "",
        false,
    ),
    attr(
        "chance_to_gain_combo_count",
        "Chance to Gain Combo Count",
        Percent,
        "",
        false,
    ),
    attr("ammo_maximum", "Ammo Maximum", Percent, "", false),
    attr("damage_vs_corpus", "Damage to Corpus", Multiply, "", false),
    attr(
        "damage_vs_grineer",
        "Damage to Grineer",
        Multiply,
        "",
        false,
    ),
    attr(
        "damage_vs_infested",
        "Damage to Infested",
        Multiply,
        "",
        false,
    ),
    attr("cold_damage", "Cold", Percent, "", false),
    attr("combo_duration", "Combo Duration", Raw, "s", false),
    attr("critical_chance", "Critical Chance", Percent, "", false),
    attr(
        "critical_chance_on_slide_attack",
        "Critical Chance on Slide Attack",
        Percent,
        "",
        false,
    ),
    attr("critical_damage", "Critical Damage", Percent, "", false),
    attr("base_damage_/_melee_damage", "Damage", Percent, "", false),
    attr("electric_damage", "Electricity", Percent, "", false),
    attr("heat_damage", "Heat", Percent, "", false),
    attr("finisher_damage", "Finisher Damage", Percent, "", false),
    attr(
        "fire_rate_/_attack_speed",
        "Fire Rate / Attack Speed",
        Percent,
        "",
        false,
    ),
    attr("projectile_speed", "Projectile Speed", Percent, "", false),
    attr("channeling_damage", "Initial Combo", Raw, "", false),
    attr("impact_damage", "Impact", Percent, "", false),
    attr("magazine_capacity", "Magazine Capacity", Percent, "", false),
    attr(
        "channeling_efficiency",
        "Heavy Attack Efficiency",
        Percent,
        "",
        false,
    ),
    attr("multishot", "Multishot", Percent, "", false),
    attr("toxin_damage", "Toxin", Percent, "", false),
    attr("punch_through", "Punch Through", Raw, "m", false),
    attr("puncture_damage", "Puncture", Percent, "", false),
    attr("reload_speed", "Reload Speed", Percent, "", false),
    attr("range", "Range", Raw, "m", false),
    attr("slash_damage", "Slash", Percent, "", false),
    attr("status_chance", "Status Chance", Percent, "", false),
    attr("status_duration", "Status Duration", Percent, "", false),
    attr("recoil", "Weapon Recoil", Percent, "", false),
    attr("zoom", "Zoom", Percent, "", false),
    attr("ammo_efficiency", "Ammo Efficiency", Percent, "", true),
    attr("blast", "Blast", Percent, "", true),
    attr("corrosive", "Corrosive", Percent, "", true),
    attr("damage_to_orokin", "Damage to Orokin", Multiply, "", true),
    attr("damage_to_scaldra", "Damage to Scaldra", Multiply, "", true),
    attr("damage_to_techrot", "Damage to Techrot", Multiply, "", true),
    attr("gas", "Gas", Percent, "", true),
    attr(
        "heavy_attack_wind_up_speed",
        "Heavy Attack Wind Up Speed",
        Percent,
        "",
        true,
    ),
    attr("magnetic", "Magnetic", Percent, "", true),
    attr(
        "melee_damage_on_heavy_attack",
        "Melee Damage on Heavy Attack",
        Percent,
        "",
        true,
    ),
    attr("parry_angle", "Parry Angle", Percent, "", true),
    attr("radiation", "Radiation", Percent, "", true),
    attr(
        "magazine_reloaded_s_when_holstered",
        "Reload While Holstered",
        Percent,
        "",
        true,
    ),
    attr(
        "slam_attack_damage",
        "Slam Attack Damage",
        Percent,
        "",
        true,
    ),
    attr("status_damage", "Status Damage", Percent, "", true),
    attr("viral", "Viral", Percent, "", true),
    attr("weak_point_damage", "Weak Point Damage", Percent, "", true),
    attr(
        "weak_point_critical_chance",
        "Weak Point Critical Chance",
        Percent,
        "",
        true,
    ),
];

/// Fire rate on bows shows doubled.
pub const FIRE_RATE: &str = "fire_rate_/_attack_speed";

pub fn attribute(slug: &str) -> Option<&'static Attribute> {
    ATTRIBUTES.iter().find(|a| a.slug == slug)
}

/// The unit of any slug. One this table doesn't know yet is guessed from its name.
pub fn unit_of(slug: &str) -> Unit {
    match attribute(slug) {
        Some(a) => a.unit,
        None if slug.starts_with("damage_vs_") || slug.starts_with("damage_to_") => Multiply,
        None => Percent,
    }
}

/// A readable name for any slug, also one this table doesn't know yet.
pub fn name_of(slug: &str) -> String {
    attribute(slug).map_or_else(|| slug.replace('_', " "), |a| a.name.to_owned())
}

/// Formats a listed value the way the game shows it.
pub fn format_value(slug: &str, value: f64) -> String {
    match unit_of(slug) {
        Multiply => format!("x{value:.2}"),
        Raw => format!("{value:+.1}{}", attribute(slug).map_or("", |a| a.suffix)),
        Percent => format!("{value:+.1}%"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_show_as_in_game() {
        assert_eq!(format_value("critical_chance", 12.34), "+12.3%");
        assert_eq!(format_value("damage_vs_grineer", 1.071), "x1.07");
        assert_eq!(format_value("damage_vs_someone_new", 1.071), "x1.07");
        assert_eq!(format_value("range", 1.25), "+1.2m");
        assert_eq!(format_value("combo_duration", -2.0), "-2.0s");
        assert_eq!(format_value("channeling_damage", 0.5), "+0.5");
        assert_eq!(name_of("multishot"), "Multishot");
        assert_eq!(name_of("new_stat"), "new stat");
        assert!(attribute(FIRE_RATE).is_some());
    }

    #[test]
    fn slugs_are_unique() {
        for (i, a) in ATTRIBUTES.iter().enumerate() {
            assert!(
                ATTRIBUTES[..i].iter().all(|b| b.slug != a.slug),
                "{}",
                a.slug
            );
        }
    }
}
