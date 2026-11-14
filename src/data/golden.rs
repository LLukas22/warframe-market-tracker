//! Dumps the answers to many queries on the real dataset as JSON lines, to compare two
//! versions of the code: run it on each and diff the files. Server times are blanked.
//!
//! ```sh
//! WFMT_GOLDEN=/tmp/out.jsonl cargo test --release --no-default-features --features server \
//!     --bin warframe-market-tracker data::golden -- --ignored
//! ```

use std::io::Write;

use super::*;
use crate::api::Period;

#[test]
#[ignore = "writes answers on the real dataset for comparison"]
fn dump_answers() {
    let Some(out) = std::env::var_os("WFMT_GOLDEN") else {
        eprintln!("set WFMT_GOLDEN to the output file");
        return;
    };
    // A fixed clock: periods slide with time, and the two runs being compared happen apart.
    FROZEN_NOW.store(1_794_600_000_000, Ordering::Relaxed);
    let store = Store::new(data_dir());
    store.refresh();
    let inner = store.inner.read().unwrap();
    let mut file = std::io::BufWriter::new(File::create(out).unwrap());
    let mut write = |name: &str, mut value: serde_json::Value| {
        if let Some(object) = value.as_object_mut() {
            object.remove("server_time");
        }
        writeln!(file, "{name}\t{value}").unwrap();
    };

    let overview = inner.overview();
    write("overview", serde_json::to_value(&overview).unwrap());
    let mut weapons: Vec<_> = overview.weapons.iter().filter(|w| w.listings > 0).collect();
    weapons.sort_by_key(|w| std::cmp::Reverse(w.listings));
    let picked: Vec<Option<String>> = std::iter::once(None)
        .chain(
            weapons
                .iter()
                .step_by(weapons.len() / 6 + 1)
                .map(|w| Some(w.slug.clone())),
        )
        .collect();

    let mut queries = Vec::new();
    for weapon in &picked {
        for period in [Period::Today, Period::Week, Period::Month, Period::All] {
            for sort in [
                Sort::PriceAsc,
                Sort::PriceDesc,
                Sort::QualityDesc,
                Sort::Newest,
            ] {
                queries.push(Query {
                    weapon: weapon.clone(),
                    period,
                    sort,
                    ..Query::default()
                });
            }
        }
        queries.push(Query {
            weapon: weapon.clone(),
            include_legacy: true,
            include_unfit: true,
            include_outliers: true,
            direct_only: false,
            min_price: None,
            period: Period::All,
            ..Query::default()
        });
        queries.push(Query {
            weapon: weapon.clone(),
            positives: vec!["critical_chance".into()],
            curse: CurseFilter::Required,
            min_quality: 0.4,
            max_price: Some(500),
            ..Query::default()
        });
        queries.push(Query {
            weapon: weapon.clone(),
            price_changed: true,
            ..Query::default()
        });
    }
    for (n, q) in queries.iter().enumerate() {
        write(
            &format!("search {n}"),
            serde_json::to_value(inner.search(q)).unwrap(),
        );
        for offset in [50, 100, 1000] {
            for gone in [false, true] {
                write(
                    &format!("page {n} {offset} {gone}"),
                    serde_json::to_value(inner.search_page(q, offset, Some(gone))).unwrap(),
                );
            }
        }
    }

    for w in &weapons {
        write(
            &format!("effects {}", w.slug),
            serde_json::to_value(inner.stat_effects(&w.slug)).unwrap(),
        );
    }

    let listings = inner.search(&Query::default()).listings;
    for l in listings.iter().take(20) {
        for options in [false, true] {
            write(
                &format!("detail {} {options}", l.auction_id),
                serde_json::to_value(inner.riven_detail(
                    &l.riven_id,
                    Some(&l.auction_id),
                    Comparisons {
                        legacy: options,
                        unfit: options,
                        loose: options,
                    },
                ))
                .unwrap(),
            );
        }
        write(
            &format!("history {}", l.riven_id),
            serde_json::to_value(inner.riven_history(&l.riven_id)).unwrap(),
        );
        let q = Query {
            riven_id: Some(l.riven_id[..6].to_owned()),
            ..Query::default()
        };
        write(
            &format!("id {}", l.riven_id),
            serde_json::to_value(inner.search(&q)).unwrap(),
        );
    }
}
