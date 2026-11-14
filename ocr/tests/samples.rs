//! Scans real riven screenshots and compares them with hand-read values.
//!
//! ```text
//! ocr/scripts/fetch-models.sh && ocr/scripts/fetch-samples.sh
//! cargo test --release -p wfmt-ocr --features engine --test samples -- --ignored --nocapture
//! ```
//!
//! `WFMT_OCR_TIER=small` tests the larger model. A reroll count the OCR didn't see is reported
//! but allowed, since the form asks for it anyway; a wrong value fails.

mod support;

use serde_json::Value;

#[test]
#[ignore = "needs the OCR models, run ocr/scripts/fetch-models.sh"]
fn torid_user_screenshot() {
    let tier = std::env::var("WFMT_OCR_TIER").unwrap_or_else(|_| "tiny".into());
    let scanner = support::scanner(&tier);
    let scan = scanner
        .scan(include_bytes!("fixtures/torid-cari-saticron.png"))
        .unwrap();
    // The ordinary parser unit test verifies these recorded lines against hand-read values.
    // This test also catches changes in detection, recognition and image preprocessing.
    let lines: Vec<wfmt_ocr::Line> =
        serde_json::from_str(include_str!("fixtures/torid-cari-saticron-lines.json")).unwrap();
    let expected = wfmt_ocr::parse(scanner.vocabulary(), &lines);
    let actual = scan.riven;
    assert_eq!(actual.weapon, expected.weapon);
    assert_eq!(actual.name, expected.name);
    assert_eq!(actual.mastery_rank, expected.mastery_rank);
    assert_eq!(actual.rerolls, expected.rerolls);
    assert_eq!(actual.name_matches_attributes, Some(true));
    let attrs = |riven: &wfmt_ocr::ScannedRiven| {
        riven
            .attributes
            .iter()
            .map(|a| (a.slug.clone(), a.value, a.positive))
            .collect::<Vec<_>>()
    };
    assert_eq!(attrs(&actual), attrs(&expected));
}

#[test]
#[ignore = "needs the OCR models and sample images, see the module docs"]
fn real_screenshots() {
    let tier = std::env::var("WFMT_OCR_TIER").unwrap_or_else(|_| "tiny".into());
    let dir = support::crate_dir().join("tests/samples");
    let expected: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("expected.json")).unwrap()).unwrap();
    let scanner = support::scanner(&tier);

    let mut failures = Vec::new();
    let mut missed = Vec::new();
    for sample in expected["samples"].as_array().unwrap() {
        let file = sample["file"].as_str().unwrap();
        let path = dir.join("images").join(file);
        assert!(
            path.exists(),
            "{} missing, run ocr/scripts/fetch-samples.sh",
            path.display()
        );
        let scan = scanner.scan(&support::read_image(&path)).unwrap();
        let riven = &scan.riven;
        let mut wrong = |what: &str, got: String, want: String| {
            failures.push(format!("{file}: {what} is {got}, expected {want}"));
        };

        if sample["not_a_riven"].as_bool() == Some(true) {
            if riven.weapon.is_some() {
                wrong("weapon", format!("{:?}", riven.weapon), "none".into());
            }
            println!("{file}: not a riven, as expected ({:?})", riven.warnings);
            continue;
        }

        let want_weapon = sample["weapon"].as_str().map(String::from);
        if riven.weapon != want_weapon {
            wrong(
                "weapon",
                format!("{:?}", riven.weapon),
                format!("{want_weapon:?}"),
            );
        }
        let want_name = sample["name"].as_str().map(String::from);
        if riven.name != want_name {
            wrong(
                "name",
                format!("{:?}", riven.name),
                format!("{want_name:?}"),
            );
        }
        let want_rank = sample["mastery_rank"].as_u64().map(|r| r as u8);
        if riven.mastery_rank != want_rank {
            wrong(
                "mastery rank",
                format!("{:?}", riven.mastery_rank),
                format!("{want_rank:?}"),
            );
        }
        let want_rerolls = sample["rerolls"].as_u64().map(|r| r as u32);
        match (riven.rerolls, want_rerolls) {
            (got, want) if got == want => {}
            (None, Some(want)) => missed.push(format!("{file}: reroll count {want} not read")),
            (got, want) => wrong("rerolls", format!("{got:?}"), format!("{want:?}")),
        }
        let got: Vec<(String, f64, bool)> = riven
            .attributes
            .iter()
            .map(|a| (a.slug.clone(), a.value, a.positive))
            .collect();
        let want: Vec<(String, f64, bool)> = sample["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a[0].as_str().unwrap().to_string(),
                    a[1].as_f64().unwrap(),
                    a[2].as_bool().unwrap(),
                )
            })
            .collect();
        let same = got.len() == want.len()
            && got
                .iter()
                .zip(&want)
                .all(|(g, w)| g.0 == w.0 && (g.1 - w.1).abs() < 1e-9 && g.2 == w.2);
        if !same {
            wrong("attributes", format!("{got:?}"), format!("{want:?}"));
        }
        println!(
            "{file}: {} {} | name agrees: {:?} | {} | warnings: {:?}",
            riven.weapon.as_deref().unwrap_or("?"),
            riven.name.as_deref().unwrap_or("?"),
            riven.name_matches_attributes,
            support::check_fit(riven),
            riven.warnings
        );
    }
    for m in &missed {
        println!("missed: {m}");
    }
    assert!(
        failures.is_empty(),
        "PP-OCRv6 {tier}:\n{}",
        failures.join("\n")
    );
}
