//! Tests of the data store as a whole: loading, searches and riven pages.

use super::load::price_limit;
use super::query::bucket_hours_for;
use super::*;
use std::io::Read;
use wfmt_api::{DAY_MS, Period};
use wfmt_dataset::identity::Registry;
use wfmt_dataset::maintenance::{MaintenanceArgs, maintain};
use wfmt_dataset::sink::ParquetSink;
use wfmt_scraper::{
    archive::{RawPage, RawResponse},
    normalize::Normalizer,
    pipeline::Pipeline,
    tracking::Tracker,
    wfm::Weapon,
};

/// The fixtures are dated around 1970, outside the default period.
fn all_time() -> Query {
    Query {
        period: Period::All,
        ..Query::default()
    }
}

#[test]
fn price_limit_catches_joke_prices_but_keeps_real_high_ends() {
    // (weapon's prices, limit). Shaped like real listings.
    let mut staircase = vec![100; 140];
    staircase.extend([800; 9]);
    staircase.extend([1800, 2000, 3000, 11111, 22222, 33333, 55555, 77777, 88888]);
    let limit = price_limit(&mut staircase).unwrap();
    assert_eq!(limit, 5000, "cheap weapon: the floor applies");
    assert_eq!(staircase.iter().filter(|p| **p > limit).count(), 6);

    // A weapon with a genuinely expensive high end keeps it.
    let mut expensive = vec![4000; 45];
    expensive.extend([9000, 13000, 15000, 696969]);
    let limit = price_limit(&mut expensive).unwrap();
    assert_eq!(limit, 20000);
    assert_eq!(expensive.iter().filter(|p| **p > limit).count(), 1);

    // A real good roll on a cheap weapon is never flagged.
    assert!(price_limit(&mut [25, 40, 60, 80, 1500]).unwrap() >= 1500);
    // Two jokes among few listings still can't drag the reference up with them.
    let mut small = vec![100; 9];
    small.extend([66666, 88888]);
    assert_eq!(price_limit(&mut small), Some(5000));
    // Too few listings to say anything.
    assert_eq!(price_limit(&mut [10, 20, 99999]), None);
}

fn add_auction(
    inner: &mut Data,
    weapon: WeaponId,
    id: &str,
    price: i64,
    last_seen: i64,
) -> AuctionId {
    let riven = inner.intern_riven(id);
    let index = inner.auctions.push(Auction {
        id: id.into(),
        weapon,
        riven,
        gone: None,
        riven_name: String::new(),
        polarity: String::new(),
        mastery_level: None,
        re_rolls: None,
        rank: None,
        legacy_stats: Some(false),
        fit_ok: true,
        mean_quality: None,
        owner_name: id.into(),
        owner_reputation: None,
        created: 0,
        first_seen: last_seen,
        last_seen,
        attrs: Vec::new(),
        buyout_price: Some(price),
        starting_price: Some(price),
        top_bid: None,
        direct_sell: true,
    });
    inner.auction_index.insert(id.into(), index);
    inner.riven_auctions[riven].push(index);
    inner
        .observations
        .push(Observation::new(last_seen, index, Some(price), true));
    index
}

fn comparison_auction(
    inner: &mut Data,
    weapon: WeaponId,
    id: &str,
    quality: Option<f64>,
    negative: Option<AttrId>,
    time: i64,
) -> AuctionId {
    let index = add_auction(inner, weapon, id, 100, time);
    let critical = inner.intern_attr("critical_chance");
    let damage = inner.intern_attr("critical_damage");
    inner.auctions[index].attrs = [(critical, true), (damage, true)]
        .into_iter()
        .chain(negative.map(|n| (n, false)))
        .map(|(attr, positive)| Attr {
            attr,
            positive,
            value: 100.0,
            value_rank8: Some(100.0),
            quality,
        })
        .collect();
    index
}

#[test]
fn comparisons_rank_stats_then_normalized_rolls_and_ignore_price_and_rank() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    let zoom = inner.intern_attr("zoom");
    let recoil = inner.intern_attr("recoil");
    let source = comparison_auction(&mut inner, weapon, "source", Some(0.5), Some(zoom), 1000);
    comparison_auction(
        &mut inner,
        weapon,
        "different-negative",
        Some(0.5),
        Some(recoil),
        5000,
    );
    comparison_auction(&mut inner, weapon, "far", Some(0.9), Some(zoom), 4000);
    let near = comparison_auction(&mut inner, weapon, "near", Some(0.51), Some(zoom), 2000);
    inner.auctions[near].rank = Some(0);
    inner.auctions[near].attrs[0].value = 10.0;
    inner.auctions[near].buyout_price = Some(999999);
    comparison_auction(&mut inner, weapon, "unknown", None, Some(zoom), 6000);
    comparison_auction(&mut inner, weapon, "no-negative", Some(0.5), None, 7000);
    let incompatible = comparison_auction(
        &mut inner,
        weapon,
        "other-positives",
        Some(0.5),
        Some(zoom),
        1000,
    );
    inner.auctions[incompatible].attrs.remove(0);
    let detail = inner
        .riven_detail("source", None, Comparisons::default())
        .unwrap();
    let ids: Vec<_> = detail
        .similar
        .iter()
        .map(|m| m.listing.auction_id.as_str())
        .collect();
    assert_eq!(
        ids,
        [
            "near",
            "far",
            "unknown",
            "different-negative",
            "no-negative"
        ]
    );
    assert!(detail.similar[0].roll_distance.unwrap() < 0.011);
    assert_eq!(detail.similar[2].roll_distance, None);
    inner.auctions[source].fit_ok = false;
    assert!(
        inner
            .riven_detail("source", None, Comparisons::default())
            .unwrap()
            .similar
            .iter()
            .all(|m| m.roll_distance.is_none())
    );
}

#[test]
fn partial_matches_rank_overlap_before_curse_quality_and_recency() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    let zoom = inner.intern_attr("zoom");
    let recoil = inner.intern_attr("recoil");
    let toxin = inner.intern_attr("toxin_damage");
    let heat = inner.intern_attr("heat_damage");
    let extra = |attr, quality| Attr {
        attr,
        positive: true,
        value: 100.0,
        value_rank8: Some(100.0),
        quality,
    };
    let source = comparison_auction(&mut inner, weapon, "source", Some(0.5), Some(zoom), 1000);
    inner.auctions[source].attrs.push(extra(toxin, Some(0.5)));
    let exact = comparison_auction(&mut inner, weapon, "exact", Some(0.9), Some(zoom), 1000);
    inner.auctions[exact].attrs.push(extra(toxin, Some(0.9)));
    let curse = comparison_auction(
        &mut inner,
        weapon,
        "different-curse",
        Some(0.5),
        Some(recoil),
        1000,
    );
    inner.auctions[curse].attrs.push(extra(toxin, Some(0.5)));
    comparison_auction(&mut inner, weapon, "subset", None, Some(recoil), 1000);
    let swap = comparison_auction(&mut inner, weapon, "swap", Some(0.5), Some(zoom), 5000);
    inner.auctions[swap].attrs.push(extra(heat, Some(0.5)));
    let loose = comparison_auction(&mut inner, weapon, "loose", Some(0.5), Some(zoom), 6000);
    inner.auctions[loose].attrs[0].attr = heat;
    let unrelated =
        comparison_auction(&mut inner, weapon, "unrelated", Some(0.5), Some(zoom), 6000);
    // Negative critical chance is not a matching positive critical chance.
    inner.auctions[unrelated].attrs[0].positive = false;
    inner.auctions[unrelated].attrs[1].attr = heat;
    let normal = inner
        .riven_detail("source", None, Comparisons::default())
        .unwrap();
    let ids = |result: &RivenDetail| {
        result
            .similar
            .iter()
            .map(|m| m.listing.auction_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&normal), ["exact", "different-curse", "subset", "swap"]);
    let subset = &normal.similar[2];
    assert_eq!(subset.shared_positives, 2);
    assert_eq!(subset.missing_positives, ["toxin_damage"]);
    assert!(subset.added_positives.is_empty());
    assert_eq!(subset.roll_distance, None);
    assert_eq!(normal.similar[3].added_positives, ["heat_damage"]);
    assert_eq!(normal.similar[3].missing_positives, ["toxin_damage"]);
    let broad = inner
        .riven_detail(
            "source",
            None,
            Comparisons {
                legacy: false,
                unfit: false,
                loose: true,
            },
        )
        .unwrap();
    assert_eq!(
        ids(&broad),
        ["exact", "different-curse", "subset", "swap", "loose"]
    );
    assert_eq!(broad.similar.last().unwrap().shared_positives, 1);
    // The reverse comparison also admits supersets, even with unavailable quality.
    let reverse = inner
        .riven_detail("subset", None, Comparisons::default())
        .unwrap();
    let superset = reverse
        .similar
        .iter()
        .find(|m| m.listing.auction_id == "source")
        .unwrap();
    assert!(superset.missing_positives.is_empty());
    assert_eq!(superset.added_positives, ["toxin_damage"]);
    assert_eq!(superset.shared_positives, 2);
}

#[test]
fn comparisons_include_gone_deduplicate_relists_and_validate_selected_auction() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    let other_weapon = inner.intern_weapon("other", "Other", Some(WeaponClass::Rifle));
    comparison_auction(&mut inner, weapon, "source", Some(0.5), None, 1000);
    let old = comparison_auction(&mut inner, weapon, "old", Some(0.5), None, 1000);
    let new = comparison_auction(&mut inner, weapon, "new", Some(0.6), None, 2000);
    let riven = inner.auctions[old].riven;
    let old_new_riven = inner.auctions[new].riven;
    inner.riven_auctions[old_new_riven].clear();
    inner.auctions[new].riven = riven;
    inner.riven_auctions[riven].push(new);
    inner.auctions[new].gone = Some(Gone {
        at: 3000,
        reason: GoneReason::Missing,
    });
    comparison_auction(
        &mut inner,
        other_weapon,
        "wrong-weapon",
        Some(0.5),
        None,
        1000,
    );
    let legacy = comparison_auction(&mut inner, weapon, "legacy", Some(0.5), None, 1000);
    inner.auctions[legacy].legacy_stats = Some(true);
    let unfit = comparison_auction(&mut inner, weapon, "unfit", Some(0.5), None, 1000);
    inner.auctions[unfit].fit_ok = false;
    let result = inner
        .riven_detail("source", None, Comparisons::default())
        .unwrap();
    assert_eq!(result.similar_total, 1);
    assert_eq!(result.comparisons_hidden, 2);
    assert_eq!(result.similar[0].listing.auction_id, "new");
    assert_eq!(result.similar[0].listing.gone_at, Some(3000));
    assert_eq!(result.similar[0].listing.copies, 2);
    assert_eq!(
        inner
            .riven_detail(
                "source",
                None,
                Comparisons {
                    legacy: true,
                    unfit: true,
                    loose: false,
                }
            )
            .unwrap()
            .similar_total,
        3
    );
    assert!(
        inner
            .riven_detail("missing", None, Comparisons::default())
            .is_none()
    );
    assert!(
        inner
            .riven_detail("source", Some("old"), Comparisons::default())
            .is_none()
    );
    assert!(
        inner
            .riven_detail("source", Some("missing"), Comparisons::default())
            .is_none()
    );
    assert_eq!(
        inner
            .riven_detail("old", None, Comparisons::default())
            .unwrap()
            .listing
            .auction_id,
        "new"
    );
    assert_eq!(
        inner
            .riven_detail("old", Some("old"), Comparisons::default())
            .unwrap()
            .listing
            .auction_id,
        "old"
    );
    assert!(
        inner
            .riven_detail(
                "old",
                None,
                Comparisons {
                    legacy: true,
                    unfit: true,
                    loose: false,
                }
            )
            .unwrap()
            .similar
            .iter()
            .all(|m| m.listing.riven_id != "old")
    );
}

#[test]
fn riven_history_orders_observations_and_clears_stale_removal_status() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    let index = add_auction(&mut inner, weapon, "riven", 200, 3000);
    inner
        .observations
        .push(Observation::new(1000, index, Some(100), true));
    inner.auctions[index].gone = Some(Gone {
        at: 2000,
        reason: GoneReason::Missing,
    });
    let history = inner.riven_history("riven");
    assert_eq!(
        history[0].points.iter().map(|p| p.t).collect::<Vec<_>>(),
        [1000, 3000]
    );
    assert_eq!(history[0].gone_at, None);
    assert_eq!(history[0].gone_reason, None);
}

#[test]
fn comparisons_are_bounded_with_deterministic_recency_ties() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    comparison_auction(&mut inner, weapon, "source", Some(0.5), None, 1000);
    assert!(
        inner
            .riven_detail("source", None, Comparisons::default())
            .unwrap()
            .similar
            .is_empty()
    );
    for i in 0..25 {
        comparison_auction(
            &mut inner,
            weapon,
            &format!("match-{i:02}"),
            Some(0.5),
            None,
            2000 + i,
        );
    }
    let result = inner
        .riven_detail("source", None, Comparisons::default())
        .unwrap();
    assert_eq!(result.similar_total, 25);
    assert_eq!(result.similar.len(), 10);
    let broad = inner
        .riven_detail(
            "source",
            None,
            Comparisons {
                legacy: false,
                unfit: false,
                loose: true,
            },
        )
        .unwrap();
    assert_eq!(broad.similar.len(), 10);
    assert_eq!(broad.similar_total, 25);
    assert_eq!(result.similar[0].listing.auction_id, "match-24");
    inner.auctions.iter_mut().last().unwrap().last_seen = 2023;
    let result = inner
        .riven_detail("source", None, Comparisons::default())
        .unwrap();
    assert_eq!(result.similar[0].listing.auction_id, "match-23");
}

/// A weapon with normal prices at 100p plus a joke-priced riven, and a joke riven that's gone.
fn store_with_outlier() -> Data {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    inner.weapons[weapon].last_scrape = 2000;
    for i in 0..9 {
        add_auction(&mut inner, weapon, &format!("normal{i}"), 100, 2000);
    }
    add_auction(&mut inner, weapon, "joke", 66666, 2000);
    let gone = add_auction(&mut inner, weapon, "gonejoke", 88888, 1000);
    inner.auctions[gone].gone = Some(Gone {
        at: 1500,
        reason: GoneReason::Missing,
    });
    inner.gone.push(GoneEvent {
        t: 1500,
        auction: gone,
        price: Some(88888),
        direct_sell: true,
    });
    inner.finish_load();
    inner
}

/// Rivens where +crit doubles the price and -zoom raises it by half, plus a neutral stat
/// and optional joke prices.
fn store_with_stat_effects(count: usize, strength: f64) -> Data {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    inner.weapons[weapon].last_scrape = 2000;
    let crit = inner.intern_attr("critical_chance");
    let zoom = inner.intern_attr("zoom");
    let toxin = inner.intern_attr("toxin_damage");
    let attr = |attr, positive| Attr {
        attr,
        positive,
        value: 1.0,
        value_rank8: None,
        quality: None,
    };
    // Prime steps spread the stats independently of each other.
    for i in 0..count {
        let (has_crit, has_zoom, has_toxin) = (i % 2 == 0, i % 3 == 0, i % 5 < 2);
        let wobble = 1.0 + ((i * 7) % 11) as f64 / 50.0;
        let mut price = 100.0 * wobble;
        if has_crit {
            price *= 1.0 + strength;
        }
        if has_zoom {
            price *= 1.0 + strength / 2.0;
        }
        let index = add_auction(&mut inner, weapon, &format!("r{i}"), price as i64, 2000);
        let attrs = &mut inner.auctions[index].attrs;
        attrs.extend(has_crit.then(|| attr(crit, true)));
        attrs.extend(has_zoom.then(|| attr(zoom, false)));
        attrs.extend(has_toxin.then(|| attr(toxin, true)));
    }
    inner.finish_load();
    inner
}

#[test]
fn stat_effects_split_positive_and_negative_and_find_the_right_stats() {
    let inner = store_with_stat_effects(150, 1.0);
    let result = inner.stat_effects("w");
    assert_eq!(result.rivens, 150);
    assert!(result.explained.unwrap() > 0.5);

    let names: Vec<&str> = result.positive.iter().map(|s| s.slug.as_str()).collect();
    assert_eq!(names, ["critical_chance", "toxin_damage"]);
    let crit = &result.positive[0];
    // True effects are +100% and +50%. The penalty shrinks them, keeping their order.
    assert!(
        crit.certain && (0.5..1.0).contains(&crit.effect),
        "{crit:?}"
    );
    assert!(crit.low < crit.effect && crit.effect < crit.high);
    assert!(result.positive[1].effect.abs() < 0.1);

    assert_eq!(result.negative.len(), 1);
    let zoom = &result.negative[0];
    assert_eq!(zoom.slug, "zoom");
    assert!(
        zoom.certain && (0.25..0.5).contains(&zoom.effect),
        "{zoom:?}"
    );
    assert!(crit.effect > zoom.effect);
}

#[test]
fn stat_effects_ignore_invalid_prices_and_hold_back_weak_signals() {
    // A joke price mustn't change anything about the estimate.
    let clean = store_with_stat_effects(150, 1.0).stat_effects("w");
    let mut joked = store_with_stat_effects(150, 1.0);
    let weapon = joked.weapon_index["w"];
    let index = add_auction(&mut joked, weapon, "joke", 888888, 2000);
    let attrs = vec![Attr {
        attr: joked.attr_index["toxin_damage"],
        positive: true,
        value: 1.0,
        value_rank8: None,
        quality: None,
    }];
    joked.auctions[index].attrs = attrs;
    joked.finish_load();
    assert_eq!(joked.stat_effects("w").positive, clean.positive);

    // Too few rivens, prices unrelated to stats, and unknown weapons show no effects.
    let few = store_with_stat_effects(20, 1.0).stat_effects("w");
    assert_eq!(few.rivens, 20);
    assert!(few.positive.is_empty() && few.negative.is_empty());
    let flat = store_with_stat_effects(150, 0.0).stat_effects("w");
    assert!(flat.positive.is_empty() && flat.negative.is_empty() && flat.rivens == 150);
    assert_eq!(flat.explained.map(|e| e < 0.15), Some(true));
    assert_eq!(clean.rare_stats, 0);
    assert_eq!(Data::default().stat_effects("nope").rivens, 0);
}

#[test]
fn history_filters_combine_and_paginate_active_and_gone_rivens() {
    fn pair(
        inner: &mut Data,
        weapon: WeaponId,
        id: &str,
        old_price: i64,
        new_price: i64,
        seller: &str,
    ) {
        let old = add_auction(inner, weapon, &format!("{id}-old"), old_price, 1000);
        let new = add_auction(inner, weapon, &format!("{id}-new"), new_price, 3000);
        let old_riven = inner.auctions[old].riven;
        let new_riven = inner.auctions[new].riven;
        inner.riven_auctions[new_riven].clear();
        inner.auctions[new].riven = old_riven;
        inner.riven_auctions[old_riven].push(new);
        inner.auctions[old].owner_name = "Alice".into();
        inner.auctions[new].owner_name = seller.into();
        inner.auctions[old].gone = Some(Gone {
            at: 2000,
            reason: GoneReason::Missing,
        });
        inner.gone.push(GoneEvent {
            t: 2000,
            auction: old,
            price: Some(old_price),
            direct_sell: true,
        });
    }
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    pair(&mut inner, weapon, "price", 100, 200, "Alice");
    pair(&mut inner, weapon, "owner", 100, 100, "Bob");
    pair(&mut inner, weapon, "same", 100, 100, " alice ");
    pair(&mut inner, weapon, "unknown", 100, 100, "");
    for i in 0..57 {
        pair(&mut inner, weapon, &format!("both{i}"), 100, 200, "Bob");
    }
    let unchanged = inner.auction_index["same-new"];
    // Repeated observations and an unknown price don't count as price changes.
    inner
        .observations
        .push(Observation::new(4000, unchanged, None, true));
    inner
        .observations
        .push(Observation::new(5000, unchanged, Some(100), true));
    inner.finish_load();
    // First every riven is listed again (its old auction was relisted, not sold), then
    // every relist disappears too.
    for gone in [false, true] {
        if gone {
            let relists: Vec<AuctionId> = inner
                .auctions
                .ids()
                .filter(|&a| inner.auctions[a].id.ends_with("-new"))
                .collect();
            for a in relists {
                inner.auctions[a].gone = Some(Gone {
                    at: 4000,
                    reason: GoneReason::Missing,
                });
                let price = auction_price(&inner.auctions[a]);
                inner.gone.push(GoneEvent {
                    t: 4000,
                    auction: a,
                    price,
                    direct_sell: true,
                });
            }
            inner.finish_load();
        }
        for (price_changed, owner_changed, expected) in [
            (false, false, 61),
            (true, false, 58),
            (false, true, 58),
            (true, true, 57),
        ] {
            let query = Query {
                price_changed,
                owner_changed,
                ..all_time()
            };
            let result = inner.search(&query);
            let totals = if gone { (0, expected) } else { (expected, 0) };
            assert_eq!((result.total, result.gone_total), totals);
            let first = inner.search_page(&query, 0, Some(gone));
            let second = inner.search_page(&query, 50, Some(gone));
            let rows: Vec<_> = if gone {
                first.gone.into_iter().chain(second.gone).collect()
            } else {
                first.listings.into_iter().chain(second.listings).collect()
            };
            assert_eq!(rows.len(), expected);
            assert!(rows.iter().all(|l| l.auction_id.ends_with("-new")));
            assert_eq!(
                rows.iter()
                    .map(|l| &l.riven_id)
                    .collect::<HashSet<_>>()
                    .len(),
                expected
            );
            if price_changed && owner_changed {
                assert!(rows.iter().all(|l| l.riven_id.starts_with("both")));
            }
        }
        let id = inner.search(&Query {
            riven_id: Some("same".into()),
            price_changed: true,
            owner_changed: true,
            ..all_time()
        });
        assert_eq!(id.total + id.gone_total, 1, "ID searches ignore filters");
    }

    // A change within one auction is sufficient, and refresh rebuilds the cached flags.
    inner
        .observations
        .push(Observation::new(6000, unchanged, Some(125), true));
    inner.finish_load();
    assert_eq!(
        inner
            .search(&Query {
                price_changed: true,
                ..all_time()
            })
            .gone_total,
        59
    );
    inner.observations.pop();
    inner.finish_load();
    assert_eq!(
        inner
            .search(&Query {
                price_changed: true,
                ..all_time()
            })
            .gone_total,
        58
    );
}

#[test]
fn period_limits_chart_and_gone_listings_but_not_current_ones() {
    let now = now_ms();
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    // Seen long ago and gone long ago, seen recently and gone recently, and still listed.
    let old = add_auction(&mut inner, weapon, "old", 100, now - 60 * DAY_MS);
    let recent = add_auction(&mut inner, weapon, "recent", 200, now - 2 * DAY_MS);
    add_auction(&mut inner, weapon, "current", 300, now - 3_600_000);
    for (auction, t) in [(old, now - 59 * DAY_MS), (recent, now - DAY_MS)] {
        inner.auctions[auction].gone = Some(Gone {
            at: t,
            reason: GoneReason::Missing,
        });
        inner.gone.push(GoneEvent {
            t,
            auction,
            price: Some(100),
            direct_sell: true,
        });
    }
    inner.finish_load();
    let search = |period| {
        inner.search(&Query {
            period,
            ..all_time()
        })
    };

    let all = search(Period::All);
    assert_eq!((all.total, all.gone_total), (1, 2));
    assert_eq!(all.bucket_hours, 24);
    assert_eq!(all.history.iter().map(|p| p.count).sum::<u32>(), 3);

    let week = search(Period::Week);
    assert_eq!((week.total, week.gone_total), (1, 1));
    // Only two days of the week have data, so the buckets fit those.
    assert_eq!(week.bucket_hours, 1);
    assert_eq!(week.history.iter().map(|p| p.count).sum::<u32>(), 2);
    assert!(
        week.history
            .iter()
            .all(|p| p.t >= now - 7 * DAY_MS - 3_600_000)
    );

    let hours = |h: i64| bucket_hours_for(h * 3_600_000);
    assert_eq!(
        [
            hours(24),
            hours(168),
            hours(720),
            hours(2400),
            hours(9000),
            hours(20000)
        ],
        [1, 3, 12, 24, 168, 168]
    );

    let today = search(Period::Today);
    assert_eq!(today.bucket_hours, 1);
    assert_eq!(today.gone_total, 0);

    // Whole days, both inclusive.
    let day = now - 60 * DAY_MS;
    let day = day - day.rem_euclid(DAY_MS);
    let custom = search(Period::Custom {
        from: Some(day),
        to: Some(day + DAY_MS),
    });
    assert_eq!(custom.gone_total, 1);
    assert_eq!(custom.history.iter().map(|p| p.count).sum::<u32>(), 1);
    assert_eq!(custom.bucket_hours, 1);

    // An id search shows the riven's whole past whatever the period.
    let id = inner.auctions[old].riven;
    let found = inner.search(&Query {
        riven_id: Some(inner.rivens[id].clone()),
        period: Period::Today,
        ..all_time()
    });
    assert_eq!(found.gone_total, 1);
}

#[test]
fn old_search_requests_default_history_filters_to_off() {
    let mut value = serde_json::to_value(Query::default()).unwrap();
    value.as_object_mut().unwrap().remove("price_changed");
    value.as_object_mut().unwrap().remove("owner_changed");
    assert_eq!(
        serde_json::from_value::<Query>(value).unwrap(),
        Query::default()
    );
}

#[test]
fn listing_pages_reach_every_match_beyond_the_old_caps() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    for i in 0..237 {
        add_auction(&mut inner, weapon, &format!("active{i:03}"), 100, 1000);
    }
    for i in 0..113 {
        let index = add_auction(&mut inner, weapon, &format!("gone{i:03}"), 100, 1000);
        inner.auctions[index].gone = Some(Gone {
            at: 2000 + i,
            reason: GoneReason::Missing,
        });
        inner.gone.push(GoneEvent {
            t: 2000 + i,
            auction: index,
            price: Some(100),
            direct_sell: true,
        });
    }
    inner.finish_load();
    for sort in [
        Sort::PriceAsc,
        Sort::PriceDesc,
        Sort::QualityDesc,
        Sort::Newest,
    ] {
        let query = Query {
            weapon: Some("w".into()),
            sort,
            ..all_time()
        };
        let first = inner.search(&query);
        assert_eq!((first.total, first.gone_total), (237, 113));
        assert_eq!((first.listings.len(), first.gone.len()), (50, 50));
        for (gone, expected) in [(false, 237), (true, 113)] {
            let mut ids = HashSet::default();
            for offset in (0..expected).step_by(LISTING_PAGE_SIZE) {
                let page = inner.search_page(&query, offset, Some(gone));
                assert!(page.history.is_empty());
                let rows = if gone {
                    assert!(page.listings.is_empty());
                    page.gone
                } else {
                    assert!(page.gone.is_empty());
                    page.listings
                };
                assert_eq!(rows.len(), (expected - offset).min(LISTING_PAGE_SIZE));
                for row in rows {
                    assert_eq!(row.gone_at.is_some(), gone);
                    assert!(ids.insert(row.auction_id), "duplicate across pages");
                }
            }
            assert_eq!(ids.len(), expected);
            let end = inner.search_page(&query, expected, Some(gone));
            assert!(end.listings.is_empty() && end.gone.is_empty());
        }
        let filtered = inner.search_page(
            &Query {
                min_price: Some(101),
                ..query
            },
            0,
            Some(false),
        );
        assert_eq!(filtered.total, 0);
    }
}

#[test]
fn capped_fetch_keeps_unconfirmed_listings_and_respects_gone_events() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    inner.weapons[weapon].last_scrape = 2000;
    let a = add_auction(&mut inner, weapon, "old", 100, 1000);
    inner.finish_load();
    let query = Query {
        weapon: Some("w".into()),
        ..all_time()
    };
    let result = inner.search(&query);
    assert_eq!(result.total, 1);
    assert!(result.listings[0].unconfirmed);
    assert_eq!(result.listings[0].last_seen, 1000);
    inner.auctions[a].gone = Some(Gone {
        at: 2000,
        reason: GoneReason::Expired,
    });
    // As after `load_gone`: every load ends with `finish_load`.
    inner.finish_load();
    assert_eq!(inner.search(&query).total, 0);
}

#[test]
fn relists_replace_the_older_auction_in_listings_overview_and_charts() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    inner.weapons[weapon].last_scrape = 3000;
    // Still current, though no longer seen (capped pages), and cheaper.
    let old = add_auction(&mut inner, weapon, "old", 100, 1000);
    let new = add_auction(&mut inner, weapon, "new", 300, 3000);
    let riven = inner.auctions[old].riven;
    let new_riven = inner.auctions[new].riven;
    inner.riven_auctions[new_riven].clear();
    inner.auctions[new].riven = riven;
    inner.riven_auctions[riven].push(new);
    inner.auctions[old].created = 500;
    inner.auctions[new].created = 2500;
    inner.finish_load();
    let query = Query {
        weapon: Some("w".into()),
        ..all_time()
    };
    let result = inner.search(&query);
    assert_eq!(result.total, 1);
    assert_eq!(result.listings[0].auction_id, "new");
    assert_eq!(result.listings[0].copies, 2);
    assert_eq!(result.history.last().unwrap().min, 300);
    // The old copy doesn't stand in when the new one misses a filter.
    let cheap = Query {
        max_price: Some(200),
        ..query.clone()
    };
    assert_eq!(inner.search(&cheap).total, 0);
    let w = &inner.overview().weapons[0];
    assert_eq!((w.listings, w.min_price), (1, Some(300)));

    // Once the old auction is gone, it was relisted rather than sold.
    inner.auctions[old].gone = Some(Gone {
        at: 2000,
        reason: GoneReason::Missing,
    });
    inner.gone.push(GoneEvent {
        t: 2000,
        auction: old,
        price: Some(100),
        direct_sell: true,
    });
    inner.finish_load();
    let result = inner.search(&query);
    assert_eq!((result.total, result.gone_total), (1, 0));
    assert!(result.history.iter().all(|h| h.gone_count == 0));
    // When the relist disappears too, it is the one shown as gone.
    inner.auctions[new].gone = Some(Gone {
        at: 4000,
        reason: GoneReason::Missing,
    });
    inner.gone.push(GoneEvent {
        t: 4000,
        auction: new,
        price: Some(300),
        direct_sell: true,
    });
    inner.finish_load();
    let result = inner.search(&query);
    assert_eq!((result.total, result.gone_total), (0, 1));
    assert_eq!(result.gone[0].auction_id, "new");
}

#[test]
fn history_uses_latest_price_of_the_newest_copy() {
    let mut inner = Data::default();
    let weapon = inner.intern_weapon("w", "W", Some(WeaponClass::Rifle));
    let a = add_auction(&mut inner, weapon, "a", 100, 1000);
    let b = add_auction(&mut inner, weapon, "b", 150, 2000);
    inner.auctions[b].riven = inner.auctions[a].riven;
    inner
        .observations
        .push(Observation::new(3000, a, Some(200), true));
    inner.finish_load();
    let query = Query {
        weapon: Some("w".into()),
        ..all_time()
    };
    let history = inner.search(&query).history;
    assert_eq!(history.len(), 1);
    assert_eq!((history[0].count, history[0].min), (1, 150));
    // An earlier matching price must not survive a later out-of-range price.
    let filtered = Query {
        max_price: Some(175),
        ..query
    };
    assert_eq!(inner.search(&filtered).history[0].min, 150);
}

#[test]
fn cooccurring_stats_are_not_reported_as_certain() {
    let mut inner = store_with_stat_effects(150, 1.0);
    let copy = inner.intern_attr("duplicate_crit");
    let crit = inner.attr_index["critical_chance"];
    for a in &mut inner.auctions {
        if a.attrs.iter().any(|attr| attr.attr == crit) {
            a.attrs.push(Attr {
                attr: copy,
                positive: true,
                value: 1.0,
                value_rank8: None,
                quality: None,
            });
        }
    }
    let result = inner.stat_effects("w");
    assert!(result.explained.unwrap() > EFFECT_MIN_EXPLAINED);
    let aliases: Vec<_> = result
        .positive
        .iter()
        .filter(|s| s.slug == "critical_chance" || s.slug == "duplicate_crit")
        .collect();
    assert_eq!(aliases.len(), 2);
    assert!(aliases.iter().all(|s| !s.certain));
}

#[test]
fn invalid_prices_stay_out_of_listings_charts_and_medians() {
    let inner = store_with_outlier();
    let query = Query {
        weapon: Some("w".into()),
        min_price: None,
        ..all_time()
    };

    let result = inner.search(&query);
    assert_eq!(result.total, 9);
    assert_eq!(result.outliers_hidden, 1);
    assert_eq!(result.price_limit, Some(5000));
    assert!(result.gone.is_empty());
    assert!(
        result
            .history
            .iter()
            .all(|p| p.p75 == 100 && p.gone_count == 0)
    );
    let info = &inner.overview().weapons[0];
    assert_eq!((info.median_price, info.gone_7d), (Some(100), 0));

    // Asking for them adds flagged listings, but the analysis stays clean.
    let shown = inner.search(&Query {
        include_outliers: true,
        sort: Sort::PriceDesc,
        ..query.clone()
    });
    assert_eq!((shown.total, shown.outliers_hidden), (10, 0));
    assert!(shown.listings[0].price_outlier && !shown.listings[1].price_outlier);
    assert_eq!(shown.gone.len(), 1);
    assert!(
        shown
            .history
            .iter()
            .all(|p| p.p75 == 100 && p.gone_count == 0)
    );
}

#[test]
fn riven_id_search_shows_an_invalid_price_with_its_chart() {
    let inner = store_with_outlier();
    let id = inner.auctions[inner.auction_index["joke"]].riven;
    let result = inner.search(&Query {
        riven_id: Some(inner.rivens[id].clone()),
        ..all_time()
    });
    assert_eq!(result.total, 1);
    assert!(result.listings[0].price_outlier);
    assert_eq!(result.history.last().map(|p| p.median), Some(66666));
}

#[test]
fn requests_serve_loaded_data_while_another_refresh_runs() -> anyhow::Result<()> {
    use std::sync::Arc;

    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();
    let refresh_in_background = |store: &Arc<Store>| {
        let store = store.clone();
        std::thread::spawn(move || store.refresh())
    };

    // Nothing loaded yet: a request has to wait, or it would serve an empty page.
    let store = Arc::new(Store::new(root.clone()));
    let running = store.last_scan.lock().unwrap();
    let first = refresh_in_background(&store);
    std::thread::sleep(Duration::from_millis(100));
    assert!(!first.is_finished());
    drop(running);
    first.join().unwrap();

    // Once loaded, requests return right away instead of queueing behind a slow refresh.
    let running = store.last_scan.lock().unwrap();
    refresh_in_background(&store).join().unwrap();
    drop(running);
    Ok(())
}

#[test]
fn a_load_that_panicked_is_thrown_away_and_redone() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();
    let store = Store::new(root.clone());
    store.refresh();
    let version = store.read().version;

    // A load that panics half way leaves both locks poisoned.
    let half_done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _last = store.last_scan.lock().unwrap();
        let mut inner = store.write();
        inner.attrs.push("half loaded".into());
        panic!("corrupt part");
    }));
    assert!(half_done.is_err());
    assert!(store.inner.is_poisoned());

    store.refresh();
    assert!(!store.inner.is_poisoned() && !store.last_scan.is_poisoned());
    assert!(store.read().attrs.is_empty());
    assert!(store.read().version > version);
    Ok(())
}

#[test]
fn feed_sightings_do_not_make_other_listings_unconfirmed() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();
    let mut body = String::new();
    flate2::read::GzDecoder::new(
        &include_bytes!("../../scraper/tests/fixtures/kulstar_auctions.json.gz")[..],
    )
    .read_to_string(&mut body)?;
    let page: serde_json::Value = serde_json::from_str(&body)?;
    let mut sighted = page["payload"]["auctions"][0].clone();
    sighted["buyout_price"] = 123_456.into();
    sighted["item"]["weapon_url_name"] = "kulstar".into();
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
        registry: Registry::open(root.join("rivens.jsonl"))?,
        tracker: Tracker::open(root.join("seen")),
    };
    let mut sink = ParquetSink::new(root.join("parquet"));
    let swept_at: chrono::DateTime<chrono::Utc> = "2026-11-01T00:00:00Z".parse()?;
    let raw = |at, id: &str, body: String| {
        RawResponse::new(
            at,
            id.into(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: wfmt_types::Feed::from_id(id)
                    .map_or(wfmt_types::SortBy::PriceAsc.into(), Into::into),
                body,
            }],
            Vec::new(),
        )
    };
    pipeline.process(&weapon, &raw(swept_at, "kulstar", body), &mut sink)?;
    sink.flush()?;
    let feed = serde_json::json!({"payload": {"auctions": [sighted]}}).to_string();
    pipeline.process_feed(
        std::slice::from_ref(&weapon),
        &raw(
            swept_at + chrono::TimeDelta::hours(1),
            wfmt_types::Feed::Recent.as_str(),
            feed,
        ),
        &mut sink,
    )?;
    sink.flush()?;
    let store = Store::new(root.clone());
    store.refresh();
    {
        let inner = store.read();
        assert_eq!(
            inner.weapons[WeaponId::from_index(0)].last_scrape,
            swept_at.timestamp_millis()
        );
        assert!(
            inner
                .auctions
                .iter()
                .any(|a| a.buyout_price == Some(123_456))
        );
    }
    // A later feed withdraws all prices. The reader must replace the old price
    // and retain an unknown-price observation so the history can show a gap.
    sighted["buyout_price"] = serde_json::Value::Null;
    sighted["starting_price"] = serde_json::Value::Null;
    sighted["top_bid"] = serde_json::Value::Null;
    let withdrawn_at = swept_at + chrono::TimeDelta::hours(2);
    pipeline.process_feed(
        std::slice::from_ref(&weapon),
        &raw(
            withdrawn_at,
            wfmt_types::Feed::Recent.as_str(),
            serde_json::json!({"payload": {"auctions": [sighted]}}).to_string(),
        ),
        &mut sink,
    )?;
    sink.flush()?;
    let reloaded = Store::new(root.clone());
    reloaded.refresh();
    {
        let inner = reloaded.read();
        let index = inner.auction_index[sighted["id"].as_str().unwrap()];
        let auction = &inner.auctions[index];
        assert_eq!(auction_price(auction), None);
        assert_eq!(auction.last_seen, withdrawn_at.timestamp_millis());
        assert!(inner.is_current(auction), "withdrawal is not a sale");
        assert!(inner.observations.iter().any(|o| {
            o.auction == index && o.t == withdrawn_at.timestamp_millis() && o.price().is_none()
        }));
    }
    Ok(())
}

#[test]
fn cache_reloads_compacted_history_without_duplicate_observations() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().to_path_buf();
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
        registry: Registry::open(root.join("rivens.jsonl"))?,
        tracker: Tracker::open(root.join("seen")),
    };
    let mut sink = ParquetSink::new(root.join("parquet"));
    for at in ["2026-11-01T00:00:00Z", "2026-11-01T12:00:00Z"] {
        let raw = RawResponse::new(
            at.parse()?,
            "kulstar".into(),
            "pc".into(),
            vec![RawPage {
                buyout_policy: None,
                sort_by: wfmt_types::SortBy::PriceAsc.into(),
                body: body.clone(),
            }],
            Vec::new(),
        );
        pipeline.process(&weapon, &raw, &mut sink)?;
        sink.flush()?;
    }
    let store = Store::new(root.clone());
    store.refresh();
    let observations = store.read().observations.len();
    let auctions = store.read().auctions.len();
    assert!(observations > auctions && auctions > 0);
    let options = MaintenanceArgs {
        raw_retention_days: 14,
        maintenance_interval_hours: 6,
        compact_target_mb: 128,
    };
    let report = maintain(&root, "2026-11-03T00:00:00Z".parse()?, &options)?;
    assert_eq!(report.compacted_inputs, 4);
    *store.last_scan.lock().unwrap() = None;
    store.refresh();
    {
        let inner = store.read();
        assert_eq!(inner.observations.len(), observations);
        assert_eq!(inner.auctions.len(), auctions);
        assert_eq!(inner.loaded.len(), 1);
        assert!(inner.auctions.iter().all(|a| !a.attrs.is_empty()));
    }
    // New parts still load incrementally after a compaction-driven cache rebuild.
    let raw = RawResponse::new(
        "2026-11-02T00:00:00Z".parse()?,
        "kulstar".into(),
        "pc".into(),
        vec![RawPage {
            buyout_policy: None,
            sort_by: wfmt_types::SortBy::PriceAsc.into(),
            body,
        }],
        Vec::new(),
    );
    pipeline.process(&weapon, &raw, &mut sink)?;
    sink.flush()?;
    *store.last_scan.lock().unwrap() = None;
    store.refresh();
    assert_eq!(store.read().observations.len(), observations + auctions);
    Ok(())
}
