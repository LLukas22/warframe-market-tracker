//! What a riven card can say: weapon names, attribute names and the syllables riven names are
//! built from, taken from warframe.market's `/v2/riven/weapons` and `/v2/riven/attributes`.
//!
//! Requesting those with a `Language` header (e.g. `zh-hans`) adds that language's names next to
//! English, and every language present is matched.

use std::collections::HashMap;

use serde::Deserialize;

use crate::text::{self, key};

/// In-game wording that differs from warframe.market's attribute names.
const ALIASES: &[(&str, &str)] = &[
    // Seen on Chinese cards.
    ("critical_chance", "暴击几率"),
    ("base_damage_/_melee_damage", "近战伤害"),
    ("fire_rate_/_attack_speed", "射速"),
    ("fire_rate_/_attack_speed", "射速（弓类武器效果加倍）"),
    ("fire_rate_/_attack_speed", "攻击速度"),
    // English. Not checked against real cards yet.
    ("base_damage_/_melee_damage", "Melee Damage"),
    ("fire_rate_/_attack_speed", "Fire Rate"),
    ("fire_rate_/_attack_speed", "Fire Rate (x2 for Bows)"),
    ("fire_rate_/_attack_speed", "Attack Speed"),
];

#[derive(Deserialize)]
struct Envelope<T> {
    data: Vec<T>,
}

#[derive(Deserialize)]
struct Names {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WfmWeapon {
    slug: String,
    #[serde(default)]
    group: String,
    #[serde(default)]
    riven_type: String,
    #[serde(default)]
    i18n: HashMap<String, Names>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WfmAttribute {
    slug: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    suffix: String,
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    positive_is_negative: Option<bool>,
    #[serde(default)]
    exclusive_to: Option<Vec<String>>,
    #[serde(default)]
    i18n: HashMap<String, Names>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Percent,
    /// Faction damage, shown as `x1.07`.
    Multiply,
    /// Seconds, meters, combo counts.
    Raw,
}

#[derive(Clone, Debug)]
pub struct Weapon {
    pub slug: String,
    /// warframe.market's `rivenType`: rifle, shotgun, pistol, kitgun, melee or zaw.
    pub riven_type: String,
    pub group: String,
    pub names: Vec<String>,
    keys: Vec<Vec<char>>,
}

impl Weapon {
    /// The riven type `exclusiveTo` lists use; archguns report `rifle` but have their own.
    pub fn kind(&self) -> &str {
        if self.group == "archgun" {
            "archgun"
        } else {
            &self.riven_type
        }
    }
}

#[derive(Clone, Debug)]
pub struct Attribute {
    pub slug: String,
    pub unit: Unit,
    /// A lower value is the bonus (recoil).
    pub positive_is_negative: bool,
    /// Riven types the attribute can roll on, `None` for all.
    pub exclusive_to: Option<Vec<String>>,
    pub names: Vec<String>,
    keys: Vec<Vec<char>>,
}

impl Attribute {
    pub fn rolls_on(&self, weapon: &Weapon) -> bool {
        self.exclusive_to
            .as_ref()
            .is_none_or(|kinds| kinds.iter().any(|k| k == weapon.kind()))
    }
}

/// A riven name split into its syllables, e.g. "Hexa-ignitox" = hexa + igni + tox.
#[derive(Clone, Debug, PartialEq)]
pub struct RivenName {
    pub name: String,
    /// For each syllable, the attributes it can stand for (a few attributes share one).
    pub syllables: Vec<Vec<String>>,
    /// 1 for an exact match, lower the more letters had to change.
    pub score: f32,
}

#[derive(Clone, Debug)]
pub struct Vocabulary {
    pub weapons: Vec<Weapon>,
    pub attributes: Vec<Attribute>,
    /// Lowercase syllable and the attributes using it.
    prefixes: Vec<(Vec<char>, Vec<String>)>,
    suffixes: Vec<(Vec<char>, Vec<String>)>,
}

#[derive(Debug)]
pub struct VocabularyError(String);

impl std::fmt::Display for VocabularyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VocabularyError {}

fn names_and_keys(i18n: HashMap<String, Names>, extra: &[&str]) -> (Vec<String>, Vec<Vec<char>>) {
    let mut names: Vec<String> = i18n.into_values().map(|n| n.name).collect();
    names.extend(extra.iter().map(|s| s.to_string()));
    names.sort();
    names.dedup();
    names.retain(|n| !key(n).is_empty());
    let keys = names.iter().map(|n| key(n)).collect();
    (names, keys)
}

fn syllables(
    attributes: &[WfmAttribute],
    part: impl Fn(&WfmAttribute) -> &str,
) -> Vec<(Vec<char>, Vec<String>)> {
    let mut map: Vec<(Vec<char>, Vec<String>)> = Vec::new();
    for a in attributes {
        let syllable = key(part(a));
        if syllable.is_empty() {
            continue;
        }
        match map.iter_mut().find(|(s, _)| *s == syllable) {
            Some((_, slugs)) => slugs.push(a.slug.clone()),
            None => map.push((syllable, vec![a.slug.clone()])),
        }
    }
    map
}

impl Vocabulary {
    /// Builds the vocabulary from warframe.market's `/v2/riven/weapons` and
    /// `/v2/riven/attributes` response bodies.
    pub fn from_wfm(weapons_json: &str, attributes_json: &str) -> Result<Self, VocabularyError> {
        let weapons: Envelope<WfmWeapon> = serde_json::from_str(weapons_json)
            .map_err(|e| VocabularyError(format!("riven weapons: {e}")))?;
        let attributes: Envelope<WfmAttribute> = serde_json::from_str(attributes_json)
            .map_err(|e| VocabularyError(format!("riven attributes: {e}")))?;

        let prefixes = syllables(&attributes.data, |a| &a.prefix);
        let suffixes = syllables(&attributes.data, |a| &a.suffix);

        let weapons = weapons
            .data
            .into_iter()
            .map(|w| {
                let (names, keys) = names_and_keys(w.i18n, &[]);
                Weapon {
                    slug: w.slug,
                    riven_type: w.riven_type,
                    group: w.group,
                    names,
                    keys,
                }
            })
            .collect();
        let attributes = attributes
            .data
            .into_iter()
            .map(|a| {
                let aliases: Vec<&str> = ALIASES
                    .iter()
                    .filter(|(slug, _)| *slug == a.slug)
                    .map(|(_, alias)| *alias)
                    .collect();
                let (names, keys) = names_and_keys(a.i18n, &aliases);
                Attribute {
                    unit: match a.unit.as_deref() {
                        Some("percent") => Unit::Percent,
                        Some("multiply") => Unit::Multiply,
                        _ => Unit::Raw,
                    },
                    positive_is_negative: a.positive_is_negative.unwrap_or(false),
                    exclusive_to: a.exclusive_to,
                    slug: a.slug,
                    names,
                    keys,
                }
            })
            .collect();
        Ok(Self {
            weapons,
            attributes,
            prefixes,
            suffixes,
        })
    }

    pub fn weapon(&self, slug: &str) -> Option<&Weapon> {
        self.weapons.iter().find(|w| w.slug == slug)
    }

    pub fn attribute(&self, slug: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|a| a.slug == slug)
    }

    /// Weapons whose name starts `text`, best first: (weapon, matched name, score, characters of
    /// `text` the name took).
    pub(crate) fn weapons_at_start(&self, text: &[char]) -> Vec<(&Weapon, &str, f32, usize)> {
        let mut found: Vec<(&Weapon, &str, f32, usize)> = Vec::new();
        for weapon in &self.weapons {
            let best = weapon
                .keys
                .iter()
                .zip(&weapon.names)
                .map(|(k, name)| {
                    let (score, len) = text::prefix_score(k, text);
                    (name.as_str(), score, len, k.len())
                })
                // Longer names win ties: "Braton Prime" over "Braton".
                .max_by(|a, b| a.1.total_cmp(&b.1).then(a.3.cmp(&b.3)));
            if let Some((name, score, len, _)) = best {
                found.push((weapon, name, score, len));
            }
        }
        found.sort_by(|a, b| b.2.total_cmp(&a.2).then(b.3.cmp(&a.3)));
        found
    }

    /// The attribute `text` (a stat line without its value) names, with a score in 0..=1.
    pub(crate) fn attribute_for(
        &self,
        text: &str,
        weapon: Option<&Weapon>,
    ) -> Option<(&Attribute, f32)> {
        let text = key(text);
        self.attributes
            .iter()
            .filter(|a| weapon.is_none_or(|w| a.rolls_on(w)))
            .map(|a| {
                let score = a
                    .keys
                    .iter()
                    .map(|k| text::phrase_score(k, &text))
                    .fold(0.0, f32::max);
                (a, score)
            })
            .max_by(|a, b| a.1.total_cmp(&b.1))
    }

    /// Splits OCR text into riven name syllables: prefix + suffix for two positives,
    /// prefix-prefix + suffix for three.
    pub fn riven_name(&self, text: &str) -> Option<RivenName> {
        self.riven_name_matching(text, |_| true)
    }

    /// Best fuzzy name whose syllables satisfy an independent check of the card's stats.
    pub(crate) fn riven_name_matching(
        &self,
        text: &str,
        accepts: impl Fn(&[Vec<String>]) -> bool,
    ) -> Option<RivenName> {
        let observed: Vec<char> = text
            .chars()
            .filter(char::is_ascii_alphabetic)
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if observed.len() < 4 {
            return None;
        }

        // (distance, syllable indices: prefixes then the suffix)
        let mut best: Option<(usize, Vec<usize>, usize)> = None;
        let mut consider = |candidate: &[char], parts: Vec<usize>, suffix: usize| {
            let d = text::distance(candidate, &observed);
            if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                let mut syllables: Vec<Vec<String>> =
                    parts.iter().map(|&i| self.prefixes[i].1.clone()).collect();
                syllables.push(self.suffixes[suffix].1.clone());
                if !accepts(&syllables) {
                    return;
                }
                best = Some((d, parts, suffix));
            }
        };
        for (i, (p1, _)) in self.prefixes.iter().enumerate() {
            for (s, (suffix, _)) in self.suffixes.iter().enumerate() {
                let candidate: Vec<char> = p1.iter().chain(suffix).copied().collect();
                consider(&candidate, vec![i], s);
            }
            // Three positives: only try prefixes that already fit the start.
            let start = &observed[..p1.len().min(observed.len())];
            if text::distance(p1, start) > 1 {
                continue;
            }
            for (j, (p2, _)) in self.prefixes.iter().enumerate() {
                for (s, (suffix, _)) in self.suffixes.iter().enumerate() {
                    let candidate: Vec<char> = p1.iter().chain(p2).chain(suffix).copied().collect();
                    consider(&candidate, vec![i, j], s);
                }
            }
        }

        let (d, parts, suffix) = best?;
        let score = 1.0 - d as f32 / observed.len() as f32;
        let prefix = |i: usize| self.prefixes[i].0.iter().collect::<String>();
        let mut name = capitalize(&prefix(parts[0]));
        if let Some(&second) = parts.get(1) {
            name.push('-');
            name.push_str(&prefix(second));
        }
        name.extend(self.suffixes[suffix].0.iter());
        let mut syllables: Vec<Vec<String>> =
            parts.iter().map(|&i| self.prefixes[i].1.clone()).collect();
        syllables.push(self.suffixes[suffix].1.clone());
        Some(RivenName {
            name,
            syllables,
            score,
        })
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn fixture() -> Vocabulary {
        Vocabulary::from_wfm(
            include_str!("../tests/fixtures/riven_weapons.json"),
            include_str!("../tests/fixtures/riven_attributes.json"),
        )
        .unwrap()
    }

    #[test]
    fn loads_both_languages() {
        let v = fixture();
        assert!(v.weapons.len() > 400);
        let soma = v.weapon("soma").unwrap();
        assert!(soma.names.contains(&"Soma".to_string()));
        assert!(soma.names.contains(&"月神".to_string()));
        assert_eq!(
            v.attribute("damage_vs_corpus").unwrap().unit,
            Unit::Multiply
        );
        assert!(v.attribute("recoil").unwrap().positive_is_negative);
    }

    #[test]
    fn riven_names() {
        let v = fixture();
        let name = v.riven_name("Hexa-ignitox").unwrap();
        assert_eq!(name.name, "Hexa-ignitox");
        assert_eq!(name.score, 1.0);
        assert_eq!(name.syllables[0], ["status_chance"]);
        // Wrapped and hyphen lost, as OCR returns it.
        assert_eq!(v.riven_name("Manti acritox").unwrap().name, "Manti-acritox");
        assert_eq!(v.riven_name("Insitron").unwrap().name, "Insitron");
        assert_eq!(
            v.riven_name("tOxlpha Visi").map(|n| n.score < 1.0),
            Some(true)
        );
    }

    #[test]
    fn weapon_prefix() {
        let v = fixture();
        let text = key("醉沫者Magnadex");
        let (weapon, name, score, len) = v.weapons_at_start(&text)[0];
        assert_eq!((weapon.slug.as_str(), name, len), ("tysis", "啐沫者", 3));
        assert!(score > 0.6);
    }
}
