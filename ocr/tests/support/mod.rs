//! Shared by the `scan` example and the `samples` test.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use wfmt_model::{AttrInput, RivenInput, WeaponClass, fit};
use wfmt_ocr::{ScannedRiven, Scanner, Vocabulary};
use wfmt_types::{RivenType, WeaponGroup};

pub const WEAPONS: &str = include_str!("../fixtures/riven_weapons.json");
pub const ATTRIBUTES: &str = include_str!("../fixtures/riven_attributes.json");

pub fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Loads `models/<tier>_det` and `models/<tier>_rec`, see `scripts/fetch-models.sh`.
pub fn scanner(tier: &str) -> Scanner {
    let dir = crate_dir().join("models");
    let read = |part: &str, file: &str| {
        let path = dir.join(format!("{tier}_{part}")).join(file);
        std::fs::read(&path).unwrap_or_else(|e| {
            panic!(
                "{}: {e}. Run ocr/scripts/fetch-models.sh {tier}",
                path.display()
            )
        })
    };
    let text = |part: &str| String::from_utf8(read(part, "inference.yml")).unwrap();
    Scanner::new(
        read("det", "inference.onnx"),
        text("det"),
        read("rec", "inference.onnx"),
        text("rec"),
        Vocabulary::from_wfm(WEAPONS, ATTRIBUTES).unwrap(),
    )
    .unwrap()
}

pub fn read_image(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Checks the scanned stats against the riven stat model: a misread value rarely fits any rank.
pub fn check_fit(riven: &ScannedRiven) -> String {
    /// The parts of warframe.market's weapon list the stat model needs.
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Weapon {
        slug: String,
        #[serde(default)]
        group: WeaponGroup,
        riven_type: RivenType,
        #[serde(default)]
        game_ref: String,
        disposition: Option<f64>,
    }
    #[derive(serde::Deserialize)]
    struct List {
        data: Vec<Weapon>,
    }
    let weapons = serde_json::from_str::<List>(WEAPONS).unwrap().data;
    let Some(weapon) = riven
        .weapon
        .as_ref()
        .and_then(|s| weapons.iter().find(|w| &w.slug == s))
    else {
        return "no weapon".into();
    };
    let Some(class) = WeaponClass::of(weapon.riven_type, weapon.group) else {
        return format!("unknown riven type {}", weapon.riven_type);
    };
    let inputs: Vec<AttrInput> = riven
        .attributes
        .iter()
        .map(|a| AttrInput {
            slug: &a.slug,
            value: a.value,
            positive: a.positive,
        })
        .collect();
    let input = RivenInput {
        class,
        is_bow: wfmt_types::market::is_bow(weapon.riven_type, &weapon.game_ref),
        listed_rank: 8,
        attributes: &inputs,
        legacy_disposition: weapon.disposition,
        prefer_legacy: false,
    };
    match fit(&input) {
        Ok(f) => format!(
            "fits rank {}{}{}",
            f.rank,
            if f.legacy {
                " (pre-rework disposition)"
            } else {
                ""
            },
            if f.ambiguous { ", ambiguous" } else { "" }
        ),
        Err(e) => format!("does not fit: {e}"),
    }
}
