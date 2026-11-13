use super::*;
use arrow::array::ArrayRef;
use std::sync::Arc;

fn attributes() -> RecordBatch {
    RecordBatch::try_from_iter([
        (
            "scraped_at",
            Arc::new(TimestampMillisecondArray::from(vec![1])) as ArrayRef,
        ),
        (
            "auction_id",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        ("position", Arc::new(Int32Array::from(vec![1])) as ArrayRef),
        (
            "attribute",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "positive",
            Arc::new(BooleanArray::from(vec![true])) as ArrayRef,
        ),
        ("value", Arc::new(Float64Array::from(vec![1.0])) as ArrayRef),
        (
            "value_rank8",
            Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
        ),
        (
            "quality",
            Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
        ),
    ])
    .unwrap()
}

fn auctions() -> RecordBatch {
    RecordBatch::try_from_iter([
        (
            "scraped_at",
            Arc::new(TimestampMillisecondArray::from(vec![1])) as ArrayRef,
        ),
        (
            "auction_id",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "weapon_slug",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "weapon_name",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "weapon_class",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "riven_name",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "polarity",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "mastery_level",
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
        ),
        ("re_rolls", Arc::new(Int64Array::from(vec![1])) as ArrayRef),
        ("rank", Arc::new(Int32Array::from(vec![1])) as ArrayRef),
        (
            "legacy_stats",
            Arc::new(BooleanArray::from(vec![true])) as ArrayRef,
        ),
        (
            "fit_ok",
            Arc::new(BooleanArray::from(vec![true])) as ArrayRef,
        ),
        (
            "mean_positive_quality",
            Arc::new(Float64Array::from(vec![1.0])) as ArrayRef,
        ),
        (
            "buyout_price",
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
        ),
        (
            "starting_price",
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
        ),
        ("top_bid", Arc::new(Int64Array::from(vec![1])) as ArrayRef),
        (
            "is_direct_sell",
            Arc::new(BooleanArray::from(vec![true])) as ArrayRef,
        ),
        (
            "visible",
            Arc::new(BooleanArray::from(vec![true])) as ArrayRef,
        ),
        (
            "closed",
            Arc::new(BooleanArray::from(vec![false])) as ArrayRef,
        ),
        (
            "created",
            Arc::new(TimestampMillisecondArray::from(vec![1])) as ArrayRef,
        ),
        (
            "owner_name",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "owner_reputation",
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
        ),
    ])
    .unwrap()
}

fn gone() -> RecordBatch {
    RecordBatch::try_from_iter([
        (
            "detected_at",
            Arc::new(TimestampMillisecondArray::from(vec![1])) as ArrayRef,
        ),
        (
            "auction_id",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "reason",
            Arc::new(StringArray::from(vec!["test"])) as ArrayRef,
        ),
        (
            "last_price",
            Arc::new(Int64Array::from(vec![1])) as ArrayRef,
        ),
        (
            "direct_sell",
            Arc::new(BooleanArray::from(vec![true])) as ArrayRef,
        ),
    ])
    .unwrap()
}

#[test]
fn late_auction_failure_does_not_apply_a_prefix_or_intern_attributes() {
    let mut inner = Inner::default();
    for _ in 0..2 {
        assert!(
            inner
                .load_parts(
                    [Ok(auctions()), Err("damaged later row group".into())],
                    [Ok(attributes())],
                )
                .is_err()
        );
        assert!(inner.auctions.is_empty());
        assert!(inner.observations.is_empty());
        assert!(inner.attrs.is_empty());
        assert!(inner.weapons.is_empty());
    }
    inner
        .load_parts([Ok(auctions())], [Ok(attributes())])
        .unwrap();
    assert_eq!(inner.auctions.len(), 1);
    assert_eq!(inner.observations.len(), 1);
}

#[test]
fn late_schema_failure_leaves_existing_data_unchanged() {
    let mut inner = Inner::default();
    inner
        .load_parts([Ok(auctions())], [Ok(attributes())])
        .unwrap();
    assert!(
        inner
            .load_parts([Ok(auctions()), Ok(gone())], [Ok(attributes())])
            .is_err()
    );
    assert_eq!(inner.observations.len(), 1);
    assert_eq!(inner.auctions.len(), 1);
}

#[test]
fn late_gone_failure_does_not_mark_auctions_or_duplicate_events() {
    let mut inner = Inner::default();
    inner
        .load_parts([Ok(auctions())], [Ok(attributes())])
        .unwrap();
    for _ in 0..2 {
        assert!(
            inner
                .load_gone_batches([Ok(gone()), Err("damaged later row group".into())])
                .is_err()
        );
        assert!(inner.gone.is_empty());
        assert!(inner.auctions[0].gone.is_none());
    }
    inner.load_gone_batches([Ok(gone())]).unwrap();
    assert_eq!(inner.gone.len(), 1);
    assert!(inner.auctions[0].gone.is_some());
}
