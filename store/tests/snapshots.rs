//! The answers to the web UI's queries on a small dataset, made the way the scraper makes it,
//! compared with the snapshots in `tests/snapshots`. A change in what any query answers shows
//! up here. After an intended change, review the new answers and accept them with
//! `cargo insta review` (or rerun with `INSTA_UPDATE=always`).

use std::io::Read;
use std::path::Path;

use anyhow::Result;
use wfmt_api::{Comparisons, Period, Query, RivenScan, ScanAttr, Sort};
use wfmt_dataset::identity::Registry;
use wfmt_dataset::layout::REGISTRY;
use wfmt_dataset::sink::ParquetSink;
use wfmt_scraper::archive::{RawPage, RawResponse, Verified};
use wfmt_scraper::normalize::Normalizer;
use wfmt_scraper::pipeline::Pipeline;
use wfmt_scraper::tracking::Tracker;
use wfmt_scraper::wfm::Weapon;
use wfmt_store::Store;
use wfmt_types::{RivenType, SortBy, WeaponGroup};

/// When the queries run: a day after the last scrape.
const NOW: i64 = 1_793_664_000_000; // 2026-11-03T00:00:00Z

/// Three scrapes of one weapon over two days, so there is price history to chart, and a
/// fourth in which one listing is gone, confirmed by the listing's own page.
fn dataset(root: &Path) -> Result<()> {
    let mut body = String::new();
    flate2::read::GzDecoder::new(
        &include_bytes!("../../scraper/tests/fixtures/kulstar_auctions.json.gz")[..],
    )
    .read_to_string(&mut body)?;
    let weapon = Weapon {
        slug: "kulstar".into(),
        game_ref: String::new(),
        group: WeaponGroup::Secondary,
        riven_type: RivenType::Pistol,
        disposition: Some(1.3),
        i18n: serde_json::json!({"en":{"name":"Kulstar"}}),
    };
    let mut pipeline = Pipeline {
        normalizer: Normalizer::new("2026-10-22T00:00:00Z".parse()?),
        registry: Registry::open(root.join(REGISTRY))?,
        tracker: Tracker::open(root.join("seen")),
    };
    let mut sink = ParquetSink::new(root.join("parquet"));
    for (at, order) in [
        ("2026-11-01T00:00:00Z", SortBy::PriceAsc),
        ("2026-11-01T12:00:00Z", SortBy::PriceDesc),
        ("2026-11-02T00:00:00Z", SortBy::PriceAsc),
    ] {
        let raw = RawResponse::new(
            at.parse()?,
            weapon.slug.clone(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: order.into(),
                body: body.clone(),
            }],
            Vec::new(),
        );
        pipeline.process(&weapon, &raw, &mut sink)?;
        sink.flush()?;
    }
    let mut page: serde_json::Value = serde_json::from_str(&body)?;
    let auctions = page["payload"]["auctions"]
        .as_array_mut()
        .expect("a page of auctions");
    // A direct sale of at least 10p, so the searches that list unfitted stats show it.
    let index = auctions
        .iter()
        .rposition(|a| a["is_direct_sell"] == true && a["buyout_price"].as_i64() >= Some(10))
        .expect("a direct sale");
    let sold = auctions.remove(index)["id"]
        .as_str()
        .expect("an auction id")
        .to_owned();
    let raw = RawResponse::new(
        "2026-11-02T12:00:00Z".parse()?,
        weapon.slug.clone(),
        "pc".into(),
        vec![RawPage {
            buyout_policy: None,
            sort_by: SortBy::PriceAsc.into(),
            body: page.to_string(),
        }],
        vec![Verified {
            auction_id: sold,
            exists: false,
        }],
    );
    pipeline.process(&weapon, &raw, &mut sink)?;
    sink.flush()?;
    pipeline.registry.flush()?;
    Ok(())
}

#[test]
fn query_answers_stay_the_same() -> Result<()> {
    let temp = tempfile::tempdir()?;
    dataset(temp.path())?;
    let store = Store::new(temp.path().to_path_buf());
    store.refresh();
    let data = store.read();
    let everything = Query {
        period: Period::All,
        direct_only: false,
        min_price: None,
        include_legacy: true,
        include_unfit: true,
        include_outliers: true,
        ..Query::default()
    };
    assert_eq!(
        data.search(&everything, NOW).gone_total,
        1,
        "one listing is gone"
    );

    insta::assert_json_snapshot!("overview", data.overview(NOW));

    let all_time = Query {
        period: Period::All,
        ..Query::default()
    };
    insta::assert_json_snapshot!("search_all_weapons", data.search(&all_time, NOW));
    for sort in Sort::ALL {
        let query = Query {
            weapon: Some("kulstar".into()),
            sort,
            include_legacy: true,
            include_unfit: true,
            ..all_time.clone()
        };
        insta::assert_json_snapshot!(
            format!("search_kulstar_{}", sort.key()),
            data.search(&query, NOW)
        );
    }
    let first_day = Query {
        weapon: Some("kulstar".into()),
        period: Period::custom(Some(1_793_491_200_000), Some(1_793_491_200_000)),
        ..Query::default()
    };
    insta::assert_json_snapshot!("search_kulstar_first_day", data.search(&first_day, NOW));

    insta::assert_json_snapshot!("stat_effects", data.stat_effects("kulstar"));

    let listing = data.search(&all_time, NOW).listings.remove(0);
    for (name, comparisons) in [
        ("riven_detail", Comparisons::default()),
        (
            "riven_detail_all_comparisons",
            Comparisons {
                legacy: true,
                unfit: true,
                loose: true,
            },
        ),
    ] {
        insta::assert_json_snapshot!(
            name,
            data.riven_detail(&listing.riven_id, None, comparisons, NOW)
        );
    }
    insta::assert_json_snapshot!("riven_history", data.riven_history(&listing.riven_id));

    // The same riven read from a screenshot: found again, and graded like a listing.
    let scan = RivenScan {
        weapon: Some("kulstar".into()),
        name: Some(listing.riven_name.clone()),
        mastery_rank: listing.mastery_level,
        rerolls: listing.re_rolls,
        attributes: listing
            .attributes
            .iter()
            .map(|a| ScanAttr {
                slug: a.slug.clone(),
                value: a.value_rank8.unwrap_or(a.value),
                positive: a.positive,
            })
            .collect(),
        warnings: Vec::new(),
    };
    let registry = temp.path().join(REGISTRY);
    assert_eq!(
        data.scan_match(&scan, &registry, NOW),
        Some(listing.riven_id.clone())
    );
    insta::assert_json_snapshot!(
        "scanned_detail",
        data.scanned_detail(&scan, Comparisons::default(), NOW)
    );
    Ok(())
}
