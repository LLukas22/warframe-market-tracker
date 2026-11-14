//! Turns OCR lines from a riven card into a riven.
//!
//! The card is found through its stat lines ("+75.2% Toxin", "x1.07 Damage to Corpus"), which
//! are centered on the card. Everything far off that center is a neighbouring card or the UI. Above
//! the stats are the weapon and riven name, which may wrap onto a second line. Below them is the
//! mastery rank, with the reroll count to its right.

use serde::{Deserialize, Serialize};
use wfmt_types::riven as rules;

use crate::text::key;
use crate::vocab::{Unit, Vocabulary, Weapon};

/// One line of text the OCR found, in image pixels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub text: String,
    pub confidence: f32,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Line {
    fn center_x(&self) -> f32 {
        self.x + self.width / 2.0
    }

    fn center_y(&self) -> f32 {
        self.y + self.height / 2.0
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ScannedAttribute {
    /// warframe.market attribute slug.
    pub slug: String,
    /// As warframe.market lists it: signed percent or raw value, `1.07` for faction damage.
    pub value: f64,
    /// A bonus rather than the curse (`-` recoil is a bonus).
    pub positive: bool,
    /// The card's text for this stat.
    pub text: String,
    /// How well the text matched the attribute name, 0..=1.
    pub score: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ScannedRiven {
    /// warframe.market weapon slug.
    pub weapon: Option<String>,
    /// The weapon name it matched, in the card's language.
    pub weapon_name: Option<String>,
    pub weapon_score: f32,
    /// e.g. "Hexa-ignitox".
    pub name: Option<String>,
    pub name_score: f32,
    pub mastery_rank: Option<u8>,
    /// `None` when no count was read. Unrolled rivens show none at all.
    pub rerolls: Option<u32>,
    pub attributes: Vec<ScannedAttribute>,
    /// The name's syllables agree with the positive stats. A strong sign that both were read right.
    pub name_matches_attributes: Option<bool>,
    /// Things to double-check before trusting the scan.
    pub warnings: Vec<String>,
}

/// Lines further than this many line heights from the card's center belong to something else.
const CARD_HALF_WIDTH: f32 = 3.5;
/// Gap, in line heights, at which the card's text block ends.
const MAX_ROW_GAP: f32 = 1.8;
const MIN_CONFIDENCE: f32 = 0.5;
const MIN_ATTRIBUTE_SCORE: f32 = 0.6;
const MIN_WEAPON_SCORE: f32 = 0.6;
const MIN_NAME_SCORE: f32 = 0.7;
/// Lines whose centers are closer than this share of the shorter line's height are on one row.
const SAME_ROW: f32 = 0.5;
/// Weapon names tried at the start of the name text, best first; weaker ones aren't tried.
const WEAPON_CANDIDATES: usize = 8;
const MIN_WEAPON_CANDIDATE_SCORE: f32 = 0.5;
/// How much the weapon and the riven name each count when choosing between readings.
const WEAPON_WEIGHT: f32 = 0.6;
const NAME_WEIGHT: f32 = 0.4;
// Where an unread reroll count can be, in line heights: from just right of the MR label to the
// mirror of the label's left edge, padded. Narrower areas can't hold one.
const REROLL_GAP: f32 = 0.3;
const REROLL_PADDING: f32 = 0.5;
const MIN_REROLL_WIDTH: f32 = 1.5;
/// Rivens can require mastery rank 8 to 16.
const MASTERY_RANKS: std::ops::RangeInclusive<u32> = 8..=16;
const MASTERY_LABELS: &[&str] = &["段位", "mastery", "mr"];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Sign {
    Plus,
    Minus,
    Times,
    Missing,
}

#[derive(Debug, PartialEq)]
struct Value<'a> {
    sign: Sign,
    number: f64,
    percent: bool,
    rest: &'a str,
}

/// Splits "+75.2% Toxin" into its value and the rest. `None` if the text doesn't start with a
/// number or has no words after it.
fn split_value(text: &str) -> Option<Value<'_>> {
    let text = text.trim_start();
    let mut chars = text.char_indices().peekable();
    let (_, first) = *chars.peek()?;
    let sign = match first {
        '+' | '＋' => Sign::Plus,
        '-' | '−' | '–' | '—' => Sign::Minus,
        'x' | 'X' | '×' => Sign::Times,
        c if c.is_ascii_digit() => Sign::Missing,
        _ => return None,
    };
    if sign != Sign::Missing {
        chars.next();
    }
    while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
    let start = chars.peek()?.0;
    let mut end = start;
    let mut seen_point = false;
    while let Some(&(i, c)) = chars.peek() {
        if c.is_ascii_digit() || (!seen_point && (c == '.' || c == ',') && i > start) {
            seen_point |= !c.is_ascii_digit();
            end = i + c.len_utf8();
            chars.next();
        } else {
            break;
        }
    }
    let number: f64 = text[start..end]
        .trim_end_matches(['.', ','])
        .replace(',', ".")
        .parse()
        .ok()?;
    while chars.next_if(|(_, c)| c.is_whitespace()).is_some() {}
    let percent = chars.next_if(|(_, c)| matches!(c, '%' | '％')).is_some();
    let rest = chars.peek().map_or("", |&(i, _)| &text[i..]);
    rest.chars().any(char::is_alphabetic).then_some(Value {
        sign,
        number,
        percent,
        rest,
    })
}

struct Row {
    lines: Vec<Line>,
}

impl Row {
    fn text(&self) -> String {
        let parts: Vec<&str> = self.lines.iter().map(|l| l.text.trim()).collect();
        parts.join(" ")
    }

    fn top(&self) -> f32 {
        self.lines.iter().map(|l| l.y).fold(f32::MAX, f32::min)
    }

    fn bottom(&self) -> f32 {
        self.lines
            .iter()
            .map(|l| l.y + l.height)
            .fold(f32::MIN, f32::max)
    }
}

fn median(mut values: Vec<f32>) -> f32 {
    values.sort_by(f32::total_cmp);
    values[values.len() / 2]
}

/// Groups lines whose vertical centers are close into rows, top to bottom.
fn rows(mut lines: Vec<Line>) -> Vec<Row> {
    lines.sort_by(|a, b| a.center_y().total_cmp(&b.center_y()));
    let mut rows: Vec<Row> = Vec::new();
    for line in lines {
        match rows.last_mut() {
            Some(row)
                if {
                    let last = row.lines.last().unwrap();
                    (line.center_y() - last.center_y()).abs()
                        < SAME_ROW * line.height.min(last.height)
                } =>
            {
                row.lines.push(line)
            }
            _ => rows.push(Row { lines: vec![line] }),
        }
    }
    for row in &mut rows {
        row.lines.sort_by(|a, b| a.x.total_cmp(&b.x));
    }
    rows
}

fn is_stat(vocab: &Vocabulary, text: &str) -> bool {
    split_value(text).is_some_and(|v| {
        vocab
            .attribute_for(v.rest, None)
            .is_some_and(|(_, score)| score >= MIN_ATTRIBUTE_SCORE)
    })
}

/// Keeps the lines of the card whose stat lines are most numerous, dropping text from the UI and
/// neighbouring cards. Returns the kept lines and the card's typical line height.
fn card_lines(vocab: &Vocabulary, lines: &[Line]) -> Option<(Vec<Line>, f32, f32)> {
    // Stray marks around the card (pips, icons) come out as single characters with low confidence.
    let lines: Vec<&Line> = lines
        .iter()
        .filter(|l| l.confidence >= MIN_CONFIDENCE)
        .collect();
    let stats: Vec<&Line> = lines
        .iter()
        .copied()
        .filter(|l| is_stat(vocab, &l.text))
        .collect();
    let near =
        |a: &Line, b: &Line| (a.center_x() - b.center_x()).abs() <= CARD_HALF_WIDTH * a.height;
    let anchor = stats
        .iter()
        .max_by_key(|a| stats.iter().filter(|b| near(a, b)).count())?;
    let cluster: Vec<&&Line> = stats.iter().filter(|b| near(anchor, b)).collect();
    let center = median(cluster.iter().map(|l| l.center_x()).collect());
    let height = median(cluster.iter().map(|l| l.height).collect());
    // Wide English stat lines establish the card's width more reliably than line height.
    // The MR label is near its left edge, rather than centered like the stats.
    let half_width = cluster
        .iter()
        .map(|l| l.width / 2.0)
        .fold(CARD_HALF_WIDTH * height, f32::max);
    let kept = lines
        .iter()
        .filter(|l| (l.center_x() - center).abs() <= half_width)
        .map(|l| (*l).clone())
        .collect();
    Some((kept, center, height))
}

/// A reroll count as read: the cycle icon in front of it comes out as a letter, a "0" or a "9".
/// Strips those, and says so when that changed a number.
fn rerolls_from(run: &str) -> (Option<u32>, Option<String>) {
    let trimmed = run.trim_start_matches('0');
    if trimmed.is_empty() {
        return (Some(0), None);
    }
    if trimmed.len() >= 3 && trimmed.starts_with('9') {
        let count = trimmed[1..].trim_start_matches('0').parse().ok();
        let warning = format!("Read the reroll count as {run}, assumed the icon became a 9.");
        return (count, Some(warning));
    }
    (trimmed.parse().ok(), None)
}

/// Mastery rank and reroll count from the row under the stats, e.g. "段位θ 16" + "O41". The lock
/// icon before the rank and the cycle icon before the count often come out as an extra digit.
fn mastery_and_rerolls(text: &str) -> Option<(Option<u8>, Option<String>)> {
    let lower = text.to_lowercase();
    let at = MASTERY_LABELS
        .iter()
        .filter_map(|label| lower.find(label).map(|i| i + label.len()))
        .min()?;
    let mut runs: Vec<&str> = lower[at..]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .collect();
    // An icon read as a separate "0" before the rank.
    if runs.len() > 1 && !MASTERY_RANKS.contains(&runs[0].parse().unwrap_or(0)) {
        let next: u32 = runs[1].parse().unwrap_or(0);
        if MASTERY_RANKS.contains(&next) {
            runs.remove(0);
        }
    }
    let rank = runs.first().and_then(|run| {
        let value: u32 = run.parse().ok()?;
        if MASTERY_RANKS.contains(&value) || run.len() < 2 {
            return u8::try_from(value).ok();
        }
        // An icon read as a leading digit: "016".
        let trimmed: u32 = run[1..].parse().ok()?;
        u8::try_from(if MASTERY_RANKS.contains(&trimmed) {
            trimmed
        } else {
            value
        })
        .ok()
    });
    Some((rank, runs.get(1).map(|run| run.to_string())))
}

/// How the weapon and riven name were read from the text above the stats.
struct NameRead<'v> {
    weapon: Option<(&'v Weapon, String, f32)>,
    name: Option<(String, f32, Vec<Vec<String>>)>,
    observed_name: String,
}

fn read_name<'v>(vocab: &'v Vocabulary, text: &str) -> NameRead<'v> {
    let text = key(text);
    let mut best: Option<(f32, NameRead<'v>)> = None;
    for (weapon, weapon_name, weapon_score, len) in vocab
        .weapons_at_start(&text)
        .into_iter()
        .take(WEAPON_CANDIDATES)
    {
        if weapon_score < MIN_WEAPON_CANDIDATE_SCORE {
            break;
        }
        let rest: String = text[len..].iter().collect();
        let name = vocab.riven_name(&rest);
        let name_score = name.as_ref().map_or(0.0, |n| n.score);
        let total = WEAPON_WEIGHT * weapon_score + NAME_WEIGHT * name_score;
        if best.as_ref().is_none_or(|(b, _)| total > *b) {
            best = Some((
                total,
                NameRead {
                    weapon: Some((weapon, weapon_name.to_string(), weapon_score)),
                    name: name.map(|n| (n.name, n.score, n.syllables)),
                    observed_name: rest,
                },
            ));
        }
    }
    match best {
        Some((_, read)) => read,
        None => NameRead {
            weapon: None,
            name: vocab
                .riven_name(&text.iter().collect::<String>())
                .map(|n| (n.name, n.score, n.syllables)),
            observed_name: text.iter().collect(),
        },
    }
}

/// Whether each name syllable stands for a different positive stat, covering all of them.
fn name_agrees(syllables: &[Vec<String>], attributes: &[ScannedAttribute]) -> bool {
    let positives: Vec<&str> = attributes
        .iter()
        .filter(|a| a.positive)
        .map(|a| a.slug.as_str())
        .collect();
    if syllables.len() != positives.len() {
        return false;
    }
    let mut used = vec![false; positives.len()];
    syllables.iter().all(|options| {
        let free = positives
            .iter()
            .enumerate()
            .find(|(i, slug)| !used[*i] && options.iter().any(|o| o == *slug));
        free.map(|(i, _)| used[i] = true).is_some()
    })
}

/// Where the reroll count should be, for a second look when it wasn't read: right of the
/// mastery rank, mirrored around the card's center.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RerollArea {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Reads a riven from the OCR lines of a screenshot showing one riven card.
pub fn parse(vocab: &Vocabulary, lines: &[Line]) -> ScannedRiven {
    parse_with_layout(vocab, lines).0
}

/// [`parse`], plus where to look for a reroll count that wasn't read.
pub(crate) fn parse_with_layout(
    vocab: &Vocabulary,
    lines: &[Line],
) -> (ScannedRiven, Option<RerollArea>) {
    let mut riven = ScannedRiven::default();
    let Some((lines, center, height)) = card_lines(vocab, lines) else {
        riven
            .warnings
            .push("No stat lines found. Is this a riven card?".into());
        return (riven, None);
    };
    let rows = rows(lines);
    let Some(first_stat) = rows.iter().position(|r| is_stat(vocab, &r.text())) else {
        riven
            .warnings
            .push("No stat lines found. Is this a riven card?".into());
        return (riven, None);
    };

    // Name: the rows right above the stats.
    let mut start = first_stat;
    while start > 0
        && !is_stat(vocab, &rows[start - 1].text())
        && rows[start].top() - rows[start - 1].bottom() <= MAX_ROW_GAP * height
    {
        start -= 1;
    }
    let name_text: String = rows[start..first_stat].iter().map(Row::text).collect();
    // Weapon names start with a letter; anything before is a stray mark.
    let name_text = name_text
        .trim_start_matches(|c: char| !c.is_alphabetic())
        .to_string();
    let read = read_name(vocab, &name_text);
    let weapon = match read.weapon {
        Some((weapon, name, score)) if score >= MIN_WEAPON_SCORE => {
            riven.weapon = Some(weapon.slug.clone());
            riven.weapon_name = Some(name);
            riven.weapon_score = score;
            Some(weapon)
        }
        _ => {
            riven
                .warnings
                .push(format!("No weapon found in \"{name_text}\"."));
            None
        }
    };
    let mut syllables = None;
    match read.name {
        Some((name, score, parts)) if score >= MIN_NAME_SCORE => {
            riven.name = Some(name);
            riven.name_score = score;
            syllables = Some(parts);
        }
        _ => riven
            .warnings
            .push(format!("No riven name found in \"{name_text}\".")),
    }

    // Stats, then the mastery rank row.
    let mut stat_texts: Vec<String> = Vec::new();
    let mut mastery_found = false;
    let mut reroll_area = None;
    for (i, row) in rows.iter().enumerate().skip(first_stat) {
        if i > first_stat && row.top() - rows[i - 1].bottom() > MAX_ROW_GAP * height {
            break;
        }
        let text = row.text();
        // A percent value starts a new stat even when its name wraps and the first half
        // cannot be matched yet (e.g. "-42.5% Magazine" followed by "Capacity").
        if is_stat(vocab, &text) || split_value(&text).is_some_and(|v| v.percent) {
            stat_texts.push(text);
        } else if let Some((rank, rerolls)) = mastery_and_rerolls(&text) {
            riven.mastery_rank = rank;
            if let Some(run) = rerolls {
                let (count, warning) = rerolls_from(&run);
                riven.rerolls = count;
                riven.warnings.extend(warning);
            } else {
                // Without a count the label is centered; with one it moves left to make room.
                let label = &row.lines[0];
                let x = label.x + label.width + REROLL_GAP * height;
                let right = 2.0 * center - label.x + REROLL_PADDING * height;
                let shifted = center - label.center_x() > height;
                let padding = REROLL_PADDING * height;
                reroll_area =
                    (shifted && right - x >= MIN_REROLL_WIDTH * height).then_some(RerollArea {
                        x,
                        y: label.y - padding,
                        width: right - x,
                        height: label.height + 2.0 * padding,
                    });
            }
            mastery_found = true;
            break;
        } else if let Some(last) = stat_texts.last_mut() {
            // A stat wrapped onto a second line: "+13.5% 射速（弓类武器" + "效果加倍)".
            last.push(' ');
            last.push_str(&text);
        }
    }
    if !mastery_found {
        riven.warnings.push("No mastery rank found.".into());
    } else if riven
        .mastery_rank
        .is_none_or(|r| !MASTERY_RANKS.contains(&u32::from(r)))
    {
        riven.warnings.push("Mastery rank outside 8..=16.".into());
    }

    for text in stat_texts {
        let value = split_value(&text).expect("checked by is_stat");
        let Some((attribute, score)) = vocab
            .attribute_for(value.rest, weapon)
            .filter(|(_, s)| *s >= MIN_ATTRIBUTE_SCORE)
        else {
            riven
                .warnings
                .push(format!("Stat \"{text}\" can't roll on this weapon."));
            continue;
        };
        if value.sign == Sign::Missing {
            riven
                .warnings
                .push(format!("No sign read in \"{text}\", assumed +."));
        }
        let (value, positive) = match (attribute.unit, value.sign) {
            (Unit::Multiply, Sign::Times) => (value.number, value.number > 1.0),
            (Unit::Multiply, sign) => {
                let delta = value.number / 100.0;
                if sign == Sign::Minus {
                    (1.0 - delta, false)
                } else {
                    (1.0 + delta, true)
                }
            }
            (_, sign) => {
                let signed = if sign == Sign::Minus {
                    -value.number
                } else {
                    value.number
                };
                (signed, (signed > 0.0) != attribute.positive_is_negative)
            }
        };
        riven.attributes.push(ScannedAttribute {
            slug: attribute.slug.clone(),
            value,
            positive,
            text,
            score,
        });
    }

    let positives = riven.attributes.iter().filter(|a| a.positive).count();
    let negatives = riven.attributes.len() - positives;
    if !rules::valid_layout(positives, negatives) {
        riven.warnings.push(format!(
            "{positives} positive and {negatives} negative stats; rivens have {}-{} and 0-{}.",
            rules::POSITIVES.start(),
            rules::POSITIVES.end(),
            rules::MAX_NEGATIVES,
        ));
    }
    if let Some(syllables) = syllables {
        let mut agrees = name_agrees(&syllables, &riven.attributes);
        // An obscured letter can favor a different valid syllable. Let the independently
        // read stats resolve close matches. The warning retains the original OCR spelling,
        // including when that spelling happened to be another valid name.
        if !agrees
            && let Some(name) = vocab.riven_name_matching(&read.observed_name, |parts| {
                name_agrees(parts, &riven.attributes)
            })
        {
            let one_edit = 1.0 / key(&read.observed_name).len().max(1) as f32;
            if name.score >= MIN_NAME_SCORE && name.score + one_edit >= riven.name_score {
                riven.warnings.push(format!(
                    "Recovered riven name {} from \"{}\" using the positive stats.",
                    name.name, read.observed_name
                ));
                riven.name = Some(name.name);
                riven.name_score = name.score;
                agrees = true;
            }
        }
        riven.name_matches_attributes = Some(agrees);
        if !agrees {
            riven
                .warnings
                .push("The riven name doesn't fit the positive stats.".into());
        }
    }
    (riven, reroll_area)
}

/// Reroll count from OCR lines of the [`RerollArea`] crop. `None` if there was no number.
pub(crate) fn rerolls_in(lines: &[Line]) -> Option<(u32, Option<String>)> {
    let text: String = lines
        .iter()
        .filter(|l| l.confidence >= MIN_CONFIDENCE)
        .map(|l| l.text.as_str())
        .collect();
    let run = text
        .split(|c: char| !c.is_ascii_digit())
        .find(|s| !s.is_empty())?;
    let (count, warning) = rerolls_from(run);
    Some((count?, warning))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab::tests::fixture;

    /// Lines stacked like a card, centered on x = 200, each 24 px tall.
    fn card(texts: &[&str]) -> Vec<Line> {
        texts
            .iter()
            .enumerate()
            .map(|(i, text)| Line {
                text: text.to_string(),
                confidence: 1.0,
                x: 200.0 - 5.0 * text.chars().count() as f32,
                y: 100.0 + 26.0 * i as f32,
                width: 10.0 * text.chars().count() as f32,
                height: 24.0,
            })
            .collect()
    }

    fn attrs(riven: &ScannedRiven) -> Vec<(&str, f64, bool)> {
        riven
            .attributes
            .iter()
            .map(|a| (a.slug.as_str(), a.value, a.positive))
            .collect()
    }

    #[test]
    fn torid_screenshot_with_wrapped_penalty_and_obscured_name() {
        // PP-OCRv6 tiny output from the user screenshot, with pixel boxes rounded.
        let lines: Vec<Line> = serde_json::from_str(include_str!(
            "../tests/fixtures/torid-cari-saticron-lines.json"
        ))
        .unwrap();
        let riven = parse(&fixture(), &lines);
        assert_eq!(riven.weapon.as_deref(), Some("torid"));
        assert_eq!(
            attrs(&riven),
            [
                ("critical_chance", 156.7, true),
                ("viral", 108.8, true),
                ("multishot", 95.6, true),
                ("magazine_capacity", -42.5, false),
            ]
        );
        assert_eq!((riven.mastery_rank, riven.rerolls), (Some(8), Some(385)));
        assert_eq!(riven.name.as_deref(), Some("Cari-saticron"));
        assert_eq!(riven.name_matches_attributes, Some(true));
    }

    #[test]
    fn conflicting_name_is_not_forced_to_match_stats() {
        for (name, expected) in [("Soma Toxitron", "Toxitron"), ("Soma Toxitrox", "Toxitox")] {
            let riven = parse(
                &fixture(),
                &card(&[
                    name,
                    "+156.7% Critical Chance",
                    "+108.8% Viral",
                    "+95.6% Multishot",
                    "-42.5% Magazine Capacity",
                    "MR 8 O385",
                ]),
            );
            assert_eq!(riven.name.as_deref(), Some(expected));
            assert_eq!(riven.name_matches_attributes, Some(false));
            assert!(riven.warnings.iter().any(|w| w.contains("doesn't fit")));
        }
    }

    #[test]
    fn values() {
        let v = split_value("+75.2%毒素伤害").unwrap();
        assert_eq!(
            (v.sign, v.number, v.percent, v.rest),
            (Sign::Plus, 75.2, true, "毒素伤害")
        );
        let v = split_value("x1.07对 Corpus 的伤害").unwrap();
        assert_eq!((v.sign, v.number, v.percent), (Sign::Times, 1.07, false));
        assert_eq!(
            split_value("-8.7% Status Chance").unwrap().sign,
            Sign::Minus
        );
        assert_eq!(split_value("+2.9 初始连击").unwrap().number, 2.9);
        assert_eq!(split_value("Xoris Hexaum"), None);
        assert_eq!(split_value("段位13"), None);
        assert_eq!(split_value("10"), None);
    }

    #[test]
    fn mastery_rows() {
        let read = |text| {
            mastery_and_rerolls(text).map(|(rank, run)| (rank, run.map(|r| rerolls_from(&r).0)))
        };
        assert_eq!(read("段位13"), Some((Some(13), None)));
        assert_eq!(read("段位θ 16"), Some((Some(16), None)));
        assert_eq!(read("段位016"), Some((Some(16), None)));
        assert_eq!(read("段位8 024"), Some((Some(8), Some(Some(24)))));
        assert_eq!(read("段位14 41"), Some((Some(14), Some(Some(41)))));
        assert_eq!(read("段位14 941"), Some((Some(14), Some(Some(41)))));
        assert_eq!(read("段位 0 16 6"), Some((Some(16), Some(Some(6)))));
        assert_eq!(read("MR 12 O7"), Some((Some(12), Some(Some(7)))));
        assert_eq!(read("+14% 触发几率"), None);
        assert!(rerolls_from("941").1.is_some());
        assert_eq!(rerolls_from("96"), (Some(96), None));
    }

    #[test]
    fn chinese_card_with_wrapped_name() {
        let riven = parse(
            &fixture(),
            &card(&[
                "弧电离子枪Manti",
                "acritox",
                "x1.31对 Corpus 的伤害",
                "+57.6%暴击伤害",
                "+55.2%毒素伤害",
                "-47.2%弹药最大值",
                "段位θ 16",
            ]),
        );
        assert_eq!(riven.weapon.as_deref(), Some("arca_plasmor"));
        assert_eq!(riven.name.as_deref(), Some("Manti-acritox"));
        assert_eq!(riven.mastery_rank, Some(16));
        assert_eq!(
            attrs(&riven),
            [
                ("damage_vs_corpus", 1.31, true),
                ("critical_damage", 57.6, true),
                ("toxin_damage", 55.2, true),
                ("ammo_maximum", -47.2, false),
            ]
        );
        assert_eq!(riven.name_matches_attributes, Some(true));
        assert!(riven.warnings.is_empty(), "{:?}", riven.warnings);
    }

    #[test]
    fn recoil_and_wrapped_stat() {
        let v = fixture();
        let riven = parse(
            &v,
            &card(&[
                "哈帕克 Insitron",
                "+19.6%穿刺伤害",
                "x1.07对 Corpus 的伤害",
                "+6.5%武器后坐力",
                "段位10",
            ]),
        );
        assert_eq!(attrs(&riven)[2], ("recoil", 6.5, false));
        assert_eq!(riven.name_matches_attributes, Some(true));

        let riven = parse(
            &v,
            &card(&[
                "守望者Manti-",
                "critadra",
                "x1.12对 Corpus的伤害",
                "+34.5%暴击几率",
                "+13.5%射速（弓类武器",
                "效果加倍)",
                "-11.7%变焦",
                "段位8",
            ]),
        );
        assert_eq!(riven.weapon.as_deref(), Some("vectis"));
        assert_eq!(attrs(&riven)[2], ("fire_rate_/_attack_speed", 13.5, true));
        assert_eq!(attrs(&riven)[3], ("zoom", -11.7, false));
        assert_eq!(riven.name_matches_attributes, Some(true));
    }

    #[test]
    fn english_card() {
        let riven = parse(
            &fixture(),
            &card(&[
                "Soma Toxitron",
                "x1.07 Damage to Corpus",
                "+14.6% Toxin",
                "-5.5% Status Chance",
                "MR 12  O23",
            ]),
        );
        assert_eq!(riven.weapon.as_deref(), Some("soma"));
        assert_eq!(riven.name.as_deref(), Some("Toxitron"));
        assert_eq!((riven.mastery_rank, riven.rerolls), (Some(12), Some(23)));
        assert_eq!(
            attrs(&riven),
            [
                ("damage_vs_corpus", 1.07, true),
                ("toxin_damage", 14.6, true),
                ("status_chance", -5.5, false)
            ]
        );
        assert!(riven.warnings.is_empty(), "{:?}", riven.warnings);
    }

    #[test]
    fn ignores_neighbouring_cards() {
        let mut lines = card(&[
            "月神Toxitron",
            "x1.07对 Corpus 的伤害",
            "+14.6%毒素伤害",
            "-5.5%触发几率",
            "段位12",
        ]);
        for (text, x, y) in [
            ("张Mod", 3.0, 20.0),
            ("18", 0.0, 140.0),
            ("+5% 暴击伤害", 420.0, 150.0),
            ("ex", 0.0, 160.0),
        ] {
            lines.push(Line {
                text: text.into(),
                confidence: 1.0,
                x,
                y,
                width: 40.0,
                height: 22.0,
            });
        }
        let riven = parse(&fixture(), &lines);
        assert_eq!(riven.weapon.as_deref(), Some("soma"));
        assert_eq!(riven.attributes.len(), 3);
        assert!(riven.warnings.is_empty(), "{:?}", riven.warnings);
    }

    #[test]
    fn regular_mods_are_not_rivens() {
        let riven = parse(
            &fixture(),
            &card(&["手枪精通 Prime", "+153%暴击几率", "手枪"]),
        );
        assert_eq!(riven.weapon, None);
        assert!(!riven.warnings.is_empty());
    }
}
