//! String matching for noisy OCR text.

/// Lowercased letters and digits only. Drops spaces, punctuation and the junk characters OCR
/// produces for the element icons, so "+21.1%入冲击伤害" and "Damage to Corpus" compare by content.
pub fn key(text: &str) -> Vec<char> {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Levenshtein distance.
pub fn distance(a: &[char], b: &[char]) -> usize {
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = diagonal + usize::from(ca != cb);
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

/// Smallest edit distance between `pattern` and any substring of `text`.
pub fn distance_within(pattern: &[char], text: &[char]) -> usize {
    // Same as `distance`, except that skipping text before and after the match is free.
    let mut row = vec![0; text.len() + 1];
    for (i, cp) in pattern.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, ct) in text.iter().enumerate() {
            let substitute = diagonal + usize::from(cp != ct);
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row.into_iter().min().unwrap_or(pattern.len())
}

/// How well `name` explains `text`, in 0..=1. Finding the whole name in the text counts most;
/// covering the rest of the text breaks ties, so "Critical Damage" beats "Damage" for
/// "+57.6% Critical Damage", and stray icon characters only cost a little.
pub fn phrase_score(name: &[char], text: &[char]) -> f32 {
    if name.is_empty() || text.is_empty() {
        return 0.0;
    }
    let found = 1.0 - distance_within(name, text) as f32 / name.len() as f32;
    let coverage = (name.len() as f32 / text.len() as f32).min(1.0);
    found.max(0.0) * (0.8 + 0.2 * coverage)
}

/// How well `name` matches the start of `text`, and how many characters of `text` it takes.
pub fn prefix_score(name: &[char], text: &[char]) -> (f32, usize) {
    let mut best = (0.0, 0);
    let lo = name.len().saturating_sub(1).max(1);
    let hi = (name.len() + 1).min(text.len());
    for len in lo..=hi {
        let score = 1.0 - distance(name, &text[..len]) as f32 / name.len() as f32;
        if score > best.0 {
            best = (score, len);
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Vec<char> {
        key(s)
    }

    #[test]
    fn keys_drop_icons_and_spaces() {
        assert_eq!(k("x1.07 对 Corpus 的伤害"), k("x107对corpus的伤害"));
        assert_eq!(k("+11.4%@毒素伤害"), k("114毒素伤害"));
    }

    #[test]
    fn distances() {
        assert_eq!(distance(&k("kitten"), &k("sitting")), 3);
        assert_eq!(distance_within(&k("冲击伤害"), &k("21.1%入冲击伤害")), 0);
        assert_eq!(distance_within(&k("toxin"), &k("+55%toxln")), 1);
    }

    #[test]
    fn longer_names_win_ties() {
        let text = k("+57.6% Critical Damage");
        assert!(phrase_score(&k("Critical Damage"), &text) > phrase_score(&k("Damage"), &text));
        let text = k("+21.1%入冲击伤害");
        assert!(phrase_score(&k("冲击伤害"), &text) > 0.9);
    }

    #[test]
    fn prefixes() {
        assert_eq!(
            prefix_score(&k("啐沫者"), &k("醉沫者Magnadex")),
            (1.0 - 1.0 / 3.0, 3)
        );
        assert_eq!(prefix_score(&k("Soma"), &k("Soma Toxitron")), (1.0, 4));
    }
}
