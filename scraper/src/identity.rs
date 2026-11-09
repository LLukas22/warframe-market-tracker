//! Stable riven identities that survive relisting and reselling.
//!
//! A riven is identified by everything that only changes when the riven itself changes:
//! weapon, polarity, mastery requirement, reroll count, the stat types with their signs, and
//! the random roll `W` of every stat. Rolls are rank and disposition independent, so the same
//! riven listed at rank 0 by one seller and at rank 8 by another maps to the same id.
//!
//! Listed values are rounded, which makes rolls fuzzy (a rank 0 listing is about 9x less precise
//! than a rank 8 one). Hashing alone would split those, so ids live in a registry: a listing
//! reuses an existing id when all its rolls agree with a known riven within their combined
//! rounding tolerance, and mints a new id otherwise. Processing is deterministic, so
//! `reprocess` rebuilds the same ids from the raw archive.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// One stat of a listing, as needed for identification.
pub struct IdAttr<'a> {
    pub slug: &'a str,
    pub positive: bool,
    /// Listed value, used when the stats could not be fitted.
    pub value: f64,
    /// `(roll, tolerance)` when the stats were fitted.
    pub roll: Option<(f64, f64)>,
}

pub struct IdInput<'a> {
    pub weapon: &'a str,
    pub polarity: &'a str,
    pub mastery_level: Option<i64>,
    pub re_rolls: Option<i64>,
    pub listed_rank: i64,
    /// Seller; only used for listings whose stats could not be verified.
    pub owner: &'a str,
    pub attributes: &'a [IdAttr<'a>],
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    key: String,
    id: String,
    rolls: Vec<f64>,
    tolerances: Vec<f64>,
}

/// 64-bit FNV-1a, stable across Rust versions and platforms (unlike `DefaultHasher`).
fn fnv1a(data: &str) -> u64 {
    data.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[derive(Default)]
pub struct Registry {
    entries: HashMap<String, Vec<Entry>>,
    ids: HashSet<String>,
    /// Append-only journal; `None` keeps everything in memory.
    path: Option<PathBuf>,
    pending: Vec<Entry>,
}

impl Registry {
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Loads (or starts) the journal at `path`. Later lines for the same id replace earlier ones.
    pub fn open(path: PathBuf) -> Result<Self> {
        let mut registry = Self {
            path: Some(path.clone()),
            ..Self::default()
        };
        if let Ok(file) = File::open(&path) {
            for line in BufReader::new(file).lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Entry>(&line) {
                    Ok(entry) => registry.insert(entry),
                    // A torn last line after a crash is harmless.
                    Err(e) => tracing::warn!(error = %e, "skipping bad riven registry line"),
                }
            }
        }
        tracing::info!(rivens = registry.ids.len(), "riven registry loaded");
        Ok(registry)
    }

    fn insert(&mut self, entry: Entry) {
        let list = self.entries.entry(entry.key.clone()).or_default();
        match list.iter_mut().find(|e| e.id == entry.id) {
            Some(existing) => *existing = entry,
            None => {
                self.ids.insert(entry.id.clone());
                list.push(entry);
            }
        }
    }

    /// Writes new and updated entries to the journal.
    pub fn flush(&mut self) -> Result<()> {
        let Some(path) = &self.path else {
            self.pending.clear();
            return Ok(());
        };
        if self.pending.is_empty() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        let mut buf = String::new();
        for entry in &self.pending {
            buf.push_str(&serde_json::to_string(&entry)?);
            buf.push('\n');
        }
        // Separate a possible torn tail from the next complete JSON record.
        file.write_all(b"\n")?;
        file.write_all(buf.as_bytes())?;
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            crate::dataset::sync_dir(parent)?;
        }
        self.pending.clear();
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    /// Returns the riven id for a listing, registering a new riven if needed.
    pub fn identify(&mut self, input: &IdInput) -> String {
        let mut order: Vec<usize> = (0..input.attributes.len()).collect();
        order.sort_by_key(|&i| (input.attributes[i].slug, !input.attributes[i].positive));
        let attrs: Vec<&IdAttr> = order.iter().map(|&i| &input.attributes[i]).collect();

        let stats = attrs
            .iter()
            .map(|a| format!("{}{}", if a.positive { '+' } else { '-' }, a.slug))
            .collect::<Vec<_>>()
            .join(",");
        let base = format!(
            "{}|{}|mr{}|rr{}|{}",
            input.weapon,
            input.polarity,
            input.mastery_level.unwrap_or(-1),
            input.re_rolls.unwrap_or(-1),
            stats
        );

        let fitted: Option<Vec<(f64, f64)>> = attrs.iter().map(|a| a.roll).collect();
        let Some(fitted) = fitted else {
            // Unverifiable stats (placeholders, typos) can't be trusted to tell rivens apart,
            // so they only match an exact repeat by the same seller.
            let values = attrs
                .iter()
                .map(|a| format!("{:.2}", a.value))
                .collect::<Vec<_>>()
                .join(",");
            let key = format!("{base}|raw|{}|r{}|{values}", input.owner, input.listed_rank);
            return self.mint(&key, &key, Vec::new(), Vec::new(), true);
        };

        let (rolls, tolerances): (Vec<f64>, Vec<f64>) = fitted.into_iter().unzip();
        let best = self.entries.get(&base).and_then(|list| {
            list.iter()
                .enumerate()
                .filter_map(|(i, e)| {
                    let mut worst = 0.0f64;
                    for k in 0..rolls.len() {
                        let diff = (rolls[k] - e.rolls[k]).abs();
                        if diff > tolerances[k] + e.tolerances[k] + 1e-6 {
                            return None;
                        }
                        worst = worst.max(diff);
                    }
                    Some((i, worst))
                })
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        });

        match best {
            Some(i) => {
                let entry = &mut self.entries.get_mut(&base).unwrap()[i];
                // Keep the most precise measurement of each roll.
                let mut changed = false;
                for k in 0..rolls.len() {
                    if tolerances[k] < entry.tolerances[k] * 0.5 {
                        entry.rolls[k] = rolls[k];
                        entry.tolerances[k] = tolerances[k];
                        changed = true;
                    }
                }
                let id = entry.id.clone();
                if changed {
                    self.pending.push(entry.clone());
                }
                id
            }
            None => {
                let hashed = format!(
                    "{base}|{}",
                    rolls
                        .iter()
                        .map(|r| format!("{r:.3}"))
                        .collect::<Vec<_>>()
                        .join(",")
                );
                self.mint(&base, &hashed, rolls, tolerances, false)
            }
        }
    }

    /// Finds a known riven without registering anything, for rivens read from a screenshot.
    /// Screenshots don't show polarity reliably, so any polarity matches; everything else
    /// follows [`identify`](Self::identify). Only fitted stats can match.
    ///
    /// Streams the journal at `path` and only parses this weapon's lines.
    pub fn find_in_journal(path: &Path, input: &IdInput) -> Result<Option<String>> {
        let mut order: Vec<usize> = (0..input.attributes.len()).collect();
        order.sort_by_key(|&i| (input.attributes[i].slug, !input.attributes[i].positive));
        let attrs: Vec<&IdAttr> = order.iter().map(|&i| &input.attributes[i]).collect();
        let Some(fitted) = attrs.iter().map(|a| a.roll).collect::<Option<Vec<_>>>() else {
            return Ok(None);
        };
        let stats = attrs
            .iter()
            .map(|a| format!("{}{}", if a.positive { '+' } else { '-' }, a.slug))
            .collect::<Vec<_>>()
            .join(",");
        // `weapon|polarity|rest`: everything but the polarity has to be equal.
        let rest = format!(
            "mr{}|rr{}|{stats}",
            input.mastery_level.unwrap_or(-1),
            input.re_rolls.unwrap_or(-1),
        );
        let prefix = format!("{{\"key\":\"{}|", input.weapon);

        let file = match File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // Later lines for the same id replace earlier ones.
        let mut candidates: HashMap<String, Entry> = HashMap::new();
        for line in BufReader::new(file).lines() {
            let line = line?;
            if !line.starts_with(&prefix) {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<Entry>(&line) else {
                continue;
            };
            let mut parts = entry.key.splitn(3, '|');
            let (Some(_weapon), Some(_polarity), Some(key_rest)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if key_rest == rest && entry.rolls.len() == fitted.len() {
                candidates.insert(entry.id.clone(), entry);
            }
        }
        Ok(candidates
            .into_values()
            .filter_map(|e| {
                let mut worst = 0.0f64;
                for (k, (roll, tolerance)) in fitted.iter().enumerate() {
                    let diff = (roll - e.rolls[k]).abs();
                    if diff > tolerance + e.tolerances[k] + 1e-6 {
                        return None;
                    }
                    worst = worst.max(diff);
                }
                Some((e.id, worst))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
            .map(|(id, _)| id))
    }

    fn mint(
        &mut self,
        key: &str,
        hashed: &str,
        rolls: Vec<f64>,
        tolerances: Vec<f64>,
        exact: bool,
    ) -> String {
        if exact && let Some(e) = self.entries.get(key).and_then(|l| l.first()) {
            return e.id.clone();
        }
        // Salt on the (astronomically unlikely) hash collision.
        let id = (0u32..)
            .map(|salt| {
                let input = if salt == 0 {
                    hashed.to_owned()
                } else {
                    format!("{hashed}#{salt}")
                };
                format!("{:016x}", fnv1a(&input))
            })
            .find(|id| !self.ids.contains(id))
            .unwrap();
        let entry = Entry {
            key: key.to_owned(),
            id: id.clone(),
            rolls,
            tolerances,
        };
        self.pending.push(entry.clone());
        self.insert(entry);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(attrs: &'a [IdAttr<'a>], re_rolls: i64) -> IdInput<'a> {
        IdInput {
            weapon: "rubico",
            polarity: "madurai",
            mastery_level: Some(12),
            re_rolls: Some(re_rolls),
            listed_rank: 8,
            owner: "seller",
            attributes: attrs,
        }
    }

    fn fitted(slug: &str, positive: bool, roll: f64, tol: f64) -> IdAttr<'_> {
        IdAttr {
            slug,
            positive,
            value: 0.0,
            roll: Some((roll, tol)),
        }
    }

    #[test]
    fn same_riven_at_different_precision_matches() {
        let mut reg = Registry::in_memory();
        // Rank 8 listing: precise rolls.
        let a = [
            fitted("critical_chance", true, 1.0412, 0.0004),
            fitted("multishot", true, 0.9533, 0.0005),
        ];
        // Same riven listed at rank 0, in another order: rolls known to ±0.004.
        let b = [
            fitted("multishot", true, 0.9561, 0.0045),
            fitted("critical_chance", true, 1.0390, 0.0040),
        ];
        let id_a = reg.identify(&input(&a, 3));
        let id_b = reg.identify(&input(&b, 3));
        assert_eq!(id_a, id_b);
    }

    #[test]
    fn different_rolls_rerolls_or_signs_differ() {
        let mut reg = Registry::in_memory();
        let a = [
            fitted("critical_chance", true, 1.04, 0.0004),
            fitted("multishot", true, 0.95, 0.0005),
        ];
        let other_roll = [
            fitted("critical_chance", true, 1.06, 0.0004),
            fitted("multishot", true, 0.95, 0.0005),
        ];
        let id = reg.identify(&input(&a, 3));
        assert_ne!(id, reg.identify(&input(&other_roll, 3)));
        assert_ne!(id, reg.identify(&input(&a, 4)));
        let curse = [
            fitted("critical_chance", true, 1.04, 0.0004),
            fitted("multishot", false, 0.95, 0.0005),
        ];
        assert_ne!(id, reg.identify(&input(&curse, 3)));
        // Still stable for the original.
        assert_eq!(id, reg.identify(&input(&a, 3)));
    }

    #[test]
    fn ids_are_deterministic_across_registries() {
        let a = [
            fitted("critical_chance", true, 1.04, 0.0004),
            fitted("multishot", true, 0.95, 0.0005),
        ];
        let id1 = Registry::in_memory().identify(&input(&a, 1));
        let id2 = Registry::in_memory().identify(&input(&a, 1));
        assert_eq!(id1, id2);
    }

    #[test]
    fn journal_lookup_ignores_polarity_and_never_registers() {
        let dir = std::env::temp_dir().join(format!("wfmt-lookup-{}", std::process::id()));
        let path = dir.join("rivens.jsonl");
        let _ = fs::remove_file(&path);
        let a = [
            fitted("critical_chance", true, 1.04, 0.0004),
            fitted("multishot", true, 0.95, 0.0005),
        ];
        let mut reg = Registry::open(path.clone()).unwrap();
        let id = reg.identify(&input(&a, 1));
        reg.flush().unwrap();

        // Read from a screenshot: no polarity, rolls known less precisely, other order.
        let scanned = [
            fitted("multishot", true, 0.953, 0.004),
            fitted("critical_chance", true, 1.042, 0.004),
        ];
        let mut lookup = input(&scanned, 1);
        lookup.polarity = "";
        assert_eq!(Registry::find_in_journal(&path, &lookup).unwrap(), Some(id));
        let other = [
            fitted("multishot", true, 1.05, 0.004),
            fitted("critical_chance", true, 1.042, 0.004),
        ];
        assert_eq!(
            Registry::find_in_journal(&path, &input(&other, 1)).unwrap(),
            None
        );
        assert_eq!(
            Registry::find_in_journal(&path, &input(&scanned, 2)).unwrap(),
            None
        );
        assert_eq!(Registry::open(path.clone()).unwrap().len(), 1);
        assert_eq!(
            Registry::find_in_journal(&dir.join("missing.jsonl"), &lookup).unwrap(),
            None
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn journal_roundtrip() {
        let dir = std::env::temp_dir().join(format!("wfmt-registry-{}", std::process::id()));
        let path = dir.join("rivens.jsonl");
        let _ = fs::remove_file(&path);
        let a = [
            fitted("critical_chance", true, 1.04, 0.0004),
            fitted("multishot", true, 0.95, 0.0005),
        ];
        let mut reg = Registry::open(path.clone()).unwrap();
        let id = reg.identify(&input(&a, 1));
        reg.flush().unwrap();
        let mut again = Registry::open(path.clone()).unwrap();
        assert_eq!(again.len(), 1);
        let b = [
            fitted("multishot", true, 0.951, 0.004),
            fitted("critical_chance", true, 1.041, 0.004),
        ];
        assert_eq!(id, again.identify(&input(&b, 1)));
        let _ = fs::remove_dir_all(&dir);
    }
}
