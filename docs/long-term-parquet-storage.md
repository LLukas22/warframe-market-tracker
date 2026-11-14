# Long-term Parquet storage design

Status: proposed; no storage implementation or migration has been performed.

## Objective

Keep Parquet as the permanent historical record, reduce repeated listing and attribute data, and preserve the existing daily publication workflow. The same archive must be readable by the application and by Hugging Face's `datasets` library without a custom decoding script.

The design preserves every observation already admitted to the dataset. It does not change scrape frequency, admission rules, feed suppression, or raw-response retention. It does not replace historical sightings with aggregates or price-change-only events.

Compatibility means standard Parquet loading and documented joins. It does not mean that existing consumers of the current `auctions` schema can continue without changes.

## Current pipeline and constraints

| Stage | Current behavior | Design implication |
| --- | --- | --- |
| `scraper/src/pipeline.rs` | Deduplicates overlapping pages within a fetch, rejects stale edits, and records repeat weapon sightings; feeds suppress some repeats. | Preserve the observations produced by these rules. |
| `scraper/src/normalize.rs` | Writes a full auction row and one row per attribute for each sighting, including fitted model outputs. | Repeated payloads are the main structural opportunity. |
| `dataset/src/sink.rs` | Writes daily auction/attribute pairs and gone events with Zstd level 3; commits receipts and tracking checkpoints transactionally. | Keep crash recovery and append publication intact. |
| `dataset/src/maintenance.rs` | Merges closed-day parts by compatible schema, without sorting or deduplication. Uses a 128 MiB combined input target and row-group limits of 64K rows or 64 MiB. | Add a versioned archival transformation, including single-file days. |
| `store/src/load.rs` | Discovers paired auction/attribute files and constructs latest listing state plus historical observations. | Add explicit bundle discovery and loading. |
| `store/src/lib.rs` | Detects removed loaded paths and rebuilds the in-memory store. | Track archive generations and avoid loading both representations. |
| `src/hub.rs`, `src/hub/pull.rs` | Publishes and replaces finished days; discovers three known tables and compares filenames and sizes. | Publish complete generations with immutable filenames and integrity metadata. |
| `src/transfer.rs` | Transfers the dataset with a versioned export manifest. | Include archive metadata and validate the new relationships. |
| `scraper/src/reprocess.rs` | Rebuilds from retained raw responses. | Historical format migration needs a separate Parquet-to-Parquet path. |

Repeated sightings affect chart bucket membership, first/last seen times, and observation counts. Removing them would change application behavior even when their prices are unchanged.

## Core decisions

1. Keep daily UTC partitions as the publication, replacement, and recovery unit.
2. Store distinct listing payloads and attribute sets once per bundle; keep one small observation row per recorded sighting.
3. Make every bundle self-contained. No reference points into another day or bundle.
4. Represent all joins with ordinary columns. Filenames, row positions, and manifests are not foreign keys.
5. Keep every currently stored value, including model outputs and fields not used by today's UI.
6. Use standard Parquet compression and encodings, with no external compression dictionary or opaque packed payload.
7. Give each published day generation immutable files. Replace generations atomically rather than updating files in place.
8. Use daily compaction first. Cross-day or monthly deduplication is deferred because it complicates incremental downloads and independent recovery.

Daily independence deliberately permits some repetition across days. Compression savings must be measured after accounting for that repetition and the new reference columns.

## Logical schema: format v2

All tables have an explicit schema, including nullable columns in files where every value is null. Preserve current numeric precision and millisecond UTC timestamps. Do not infer types independently for each file.

Common columns:

- `archive_date`: non-null date32, the UTC observation/event partition date.
- `bundle_id`: non-null string, globally unique for an immutable bundle.

Local IDs are non-null int64 values. They identify records within a bundle, not row positions. An ID has no meaning without its `bundle_id`.

### `listing_states`

One row per distinct listing payload within the bundle.

Key: `(bundle_id, listing_state_id)`.

Columns copied from the current auction schema:

- Identity: `platform`, `auction_id`, `riven_id`.
- Weapon: `weapon_slug`, `weapon_name`, `weapon_class`, `riven_type`, `is_bow`.
- Item: `riven_name`, `polarity`, `mastery_level`, `re_rolls`, `n_positive`, `n_negative`, `has_spliced`, `listed_rank`.
- Model results: `rank`, `disposition`, `legacy_stats`, `fit_ok`, `fit_ambiguous`, `fit_error`, `mean_positive_quality`.
- Listing details: `is_direct_sell`, `minimal_reputation`, `visible`, `closed`, `private`, `crossplay`, `created`, `note`.
- Owner identity: `owner_id`, `owner_name`.

Additional columns:

- `listing_state_id`.
- `attribute_set_id`, referencing this bundle's attribute set.
- `attribute_count`, so an empty attribute set can be distinguished from missing/corrupt attribute rows.
- `source_schema_id`, identifying the source field-presence contract for legacy migration.

Any change to these values creates or selects another state. States are immutable and may be reused if an identical state reappears later.

### `attribute_sets`

One row per attribute position in a distinct ordered attribute set.

Key: `(bundle_id, attribute_set_id, position)`.

Preserve `position`, `attribute`, `positive`, `spliced`, `value`, `value_rank8`, `roll`, `quality`, `coefficient`, and `layout_multiplier`.

The old `scraped_at`, `auction_id`, and `weapon_slug` columns are reconstructed by joining observations and listing states. They are not repeated on every attribute row.

Deduplicate by the entire ordered set, including nulls and stored model outputs. Never deduplicate solely by `riven_id`: rank, disposition, input values, and model results can differ for the same riven identity.

Empty sets have zero rows and are identified through `attribute_count = 0` in the referencing state. Non-empty sets must have exactly the declared number of positions.

### `observations`

One row per existing recorded auction sighting, including unchanged sightings.

Columns:

- Common columns plus `observation_id` and `listing_state_id`.
- `scraped_at`, `source`.
- `buyout_price`, `starting_price`, `top_bid`.
- `updated`, `owner_reputation`, `owner_status`, `owner_last_seen`.
- `observation_order`: a persisted day-scoped ordering value used to preserve tie behavior when timestamps are equal.

Key: `(bundle_id, observation_id)`.

Frequently changing prices and owner activity stay here so they do not cause repeated copies of the full listing state. Unchanged values can still benefit from ordinary Parquet encoding after sorting.

Do not turn missing prices into zero or carry a prior price forward. Do not collapse multiple observations at the same timestamp. Migration preserves the existing traversal order for ties; future writes persist an equivalent deterministic order. Recompaction preserves this order rather than deriving it from new filenames.

### `gone`

Preserve the existing event payload: `detected_at`, `weapon_slug`, `auction_id`, `riven_id`, `reason`, `last_price`, `direct_sell`, `owner_name`, `created`, `first_seen`, and `last_seen`.

Add the common columns and a bundle-local `event_id`. Retain these snapshots even if a related listing observation is absent from the selected day. Do not require a listing-state reference to interpret a disappearance event.

### Fidelity and schema evolution

Deduplication uses exact stored-value equality, not approximate float matching. Hashing may accelerate lookup, but a hash collision must never merge unequal payloads. Define canonical null and float handling, including signed zero and non-finite values, before implementation.

Validate that each legacy attribute row maps unambiguously to its original auction observation. Preserve original pairing and multiplicity; do not silently discard unmatched rows or multiply rows through an ambiguous join. Unsupported or inconsistent inputs leave the day unconverted with a diagnostic.

Legacy schemas can omit columns. A source-schema descriptor must record which fields existed so migration does not claim that a missing field was an explicitly stored null. Keep descriptors in a small standard Parquet `source_schemas` table, keyed by `source_schema_id`, with a documented canonical schema description string. Publish it as its own Hugging Face configuration. Include all required descriptors with each independently distributable archive generation; duplicate identical descriptors may be deduplicated by key by readers.

Future incompatible schema versions use distinct publication paths/configurations or a new dataset version. Do not mix incompatible files under one Hugging Face configuration wildcard.

## Physical layout and metadata

Local layout, with the same table directories published at the repository root:

```text
data/
  parquet/
    observations/date=YYYY-MM-DD/part-<generation>-<bundle>.parquet
    listing_states/date=YYYY-MM-DD/part-<generation>-<bundle>.parquet
    attribute_sets/date=YYYY-MM-DD/part-<generation>-<bundle>.parquet
    gone/date=YYYY-MM-DD/part-<generation>-<bundle>.parquet
    source_schemas/date=YYYY-MM-DD/part-<generation>.parquet
  archive/
    format.json
    days/YYYY-MM-DD.json
```

`archive/format.json` declares the archive format version and reader requirements. Each day manifest declares its active generation, input fingerprint, bundle membership, table schemas, row counts, file byte sizes, and content checksums. It also records the compactor/encoding version and validation result.

The manifest coordinates the application's synchronization and validation. Standard Parquet readers can interpret all table data and joins without it. Day manifests must not contain the only copy of any field required to reconstruct observations.

Write typed empty tables where needed to make a bundle's expected membership unambiguous. The descriptor table can be shared within its day generation; it is small and is included whenever that generation is transferred.

## Compression strategy

Before choosing settings, measure existing compressed bytes by column and table, distinct payload/set counts, decode time, full application load time, and peak memory.

Benchmark two approaches on the same representative days:

- Existing flat schema, sorted and recompressed.
- V2 normalized schema, sorted and recompressed, including all metadata and reference overhead.

Test Zstd levels such as 3, 9, and 15, plus several bounded row-group sizes and suitable dictionary/numeric encodings. Final settings are a benchmark decision, not a promised saving.

Suggested sorting:

- Listing states grouped by platform, auction ID, and local state ID.
- Observations grouped by listing state and timestamp, preserving `observation_order` for ties.
- Attributes grouped by attribute set and position.
- Gone events grouped by auction ID and detection time.

Use an external sort or bounded partitioning so a large day does not need to fit in RAM. Prefer splitting bundles along listing boundaries. Define a bounded fallback for an unusually large listing group; duplicate its needed state across bundles rather than introduce cross-bundle references.

Avoid optimizing only for the smallest file. Row groups must also permit efficient streaming and Hub previews. Sorting may change physical row order, so application chronology must always use explicit ordering columns.

## Daily write and publication lifecycle

### 1. Open UTC day

Initially retain the existing append-only ingestion format and transaction behavior. The application reads those live parts through its legacy reader. Preserve current durability, receipts, tracking checkpoints, and recovery semantics.

Do not publish a partial v2 day as a completed archive. Live legacy parts and closed v2 archives may coexist locally for different dates.

### 2. Close and snapshot the day

Flush pending writes and identify the complete input set for a date strictly before the current UTC day. Capture a stable generation/fingerprint under the existing dataset coordination rules.

Build output in staging. A single input bundle is eligible: this is a format conversion, not merely a small-file merge.

For the initial implementation, reuse existing engine/dataset locking for correctness. A later optimization may build from an immutable snapshot outside the exclusive lock, but publication must recheck the input fingerprint and retry if it changed. Do not place long-lived build output where the current lock acquisition cleanup would delete it.

### 3. Validate and activate locally

Check checksums, schemas, key uniqueness, complete references, attribute counts, row counts, and semantic equivalence. Publish all new tables, the day manifest, and removal of the old representation using the existing recoverable transaction mechanism.

A date has exactly one authoritative representation. Never discover both legacy rows and their v2 replacements as separate observations.

Mark the day ready for upload only after successful activation. A day-specific generation is the authority; a date watermark alone cannot establish whether a corrected day is ready.

### 4. Publish to Hugging Face

For each new or corrected date, make one repository commit containing all generation files and its manifest, and deleting superseded files for that date. Use immutable generation filenames so same-size content changes cannot be missed by existing size-based checks.

Ordinary daily updates add only the new day's files and manifest. They do not rewrite older days. Schema/configuration metadata changes only when required.

Pin consumers to one repository revision when loading related tables. This avoids mixing states from one generation with observations from another while an update is being published.

### 5. Download and apply

Read manifests and fetch files from a single pinned repository revision. Compare generation/checksum information, download into staging, validate all table relationships, and transactionally replace the local date.

Incomplete downloads leave the previous day generation usable. A remote day that is absent remains untouched, matching current behavior. Removing a day intentionally would require an explicit deletion protocol; absence alone is not a deletion instruction.

### 6. Correct or reprocess a historical day

Invalidate only that day's archive fingerprint. Rebuild from its authoritative Parquet records plus explicitly admitted new/replacement inputs. Preserve observation identity/order and prevent duplicates when replaying data.

Activate and upload a new complete generation for that day. Downloaders replace it atomically. Repeating maintenance with unchanged inputs produces no new generation and no upload.

Format migration and re-normalization are different operations: migration preserves stored model outputs and IDs. It must not rerun fitting or identity assignment as a side effect.

## Hugging Face Datasets contract

Publish each logical table as a separate configuration, each with a `train` split. Here `train` is simply the conventional name for the complete table, not a claim that records are a training sample.

Example dataset-card front matter:

```yaml
configs:
  - config_name: observations
    default: true
    data_files:
      - split: train
        path: observations/date=*/*.parquet
  - config_name: listing_states
    data_files:
      - split: train
        path: listing_states/date=*/*.parquet
  - config_name: attribute_sets
    data_files:
      - split: train
        path: attribute_sets/date=*/*.parquet
  - config_name: gone
    data_files:
      - split: train
        path: gone/date=*/*.parquet
  - config_name: source_schemas
    data_files:
      - split: train
        path: source_schemas/date=*/*.parquet
```

Example consumer API after v2 is published:

```python
from datasets import load_dataset

repo = "OWNER/DATASET"
revision = "PINNED_COMMIT_SHA"

observations = load_dataset(
    repo, "observations", split="train", revision=revision
)
states = load_dataset(
    repo, "listing_states", split="train", revision=revision
)
streamed = load_dataset(
    repo, "observations", split="train", revision=revision, streaming=True
)
```

`load_dataset` loads tables, not foreign-key relationships. Consumers explicitly join observations to states on `(bundle_id, listing_state_id)` and states to attributes on `(bundle_id, attribute_set_id)`. Attribute joins are one-to-many and must preserve empty attribute sets. Descriptor rows are resolved by `source_schema_id`.

Provide a documented reconstruction example. Do not require a custom dataset script just to load the data. Streaming a table is supported; streaming reconstructed full listings requires a consumer to resolve the referenced bundle data.

Always store `archive_date` and `bundle_id` in the files themselves. Do not depend on directory partition inference or preservation of source filenames by the library.

Keep active generation files only under each configuration's wildcard. Retaining old and new generations there would make a generic Hugging Face load duplicate history, regardless of our application manifest.

Prefer a versioned v2 dataset repository for the initial rollout while old clients use v1. Do not silently replace the old public `auctions` schema. Freeze or retire v1 through an explicit migration policy rather than permanently publishing a second expanded copy that cancels the storage benefit.

## Application reader and transfer changes

- Discover complete authoritative day generations, while supporting legacy live/unmigrated dates.
- Validate a bundle before changing live indexes.
- Load each state and attribute set once; build existing in-memory observations directly without materializing a wide reconstructed row for every sighting.
- Preserve source-sensitive freshness, first/last seen, prices, model results, relists, and disappearance/reappearance behavior.
- Detect generation replacement and rebuild/swap the affected read model safely; a full rebuild is acceptable initially.
- Extend import/export manifests, allowed paths, backup handling, and validation to include v2 metadata and all tables.
- Keep archive readability separate from resuming a scraper: tracker and registry checkpoints remain operational state, not prerequisites for reading historical Parquet.

## Migration and validation

Add a resumable Parquet-to-Parquet migration that operates one closed day at a time and requires no raw archives. Leave source files intact until the converted generation passes validation and is committed.

Required validation:

1. Reconstruct legacy records and compare their full logical contents and multiplicity, including field absence, nulls, floats, timestamps, and source strings.
2. Exercise repeated sightings, bids, withdrawn prices, changing owner activity, state changes, model changes, relists, removals, reappearances, and equal-timestamp records.
3. Compare application searches, history buckets/counts, detail timelines, and freshness behavior before and after conversion.
4. Verify empty sets/tables, multiple bundles, multiple days, duplicate local IDs in different bundles, and malformed or missing references.
5. Exercise crashes during local publication, partial downloads, failed validation, late-day correction, and idempotent maintenance/retry.
6. Load actual Rust-written v2 samples using Hugging Face `datasets` in regular and streaming modes on the minimum supported and current library versions. Verify types, values, joins, multiple-day loads, and selected-day loads.
7. Check a small published fixture with the Hub viewer before production migration; publication is a separate implementation-stage action.
8. Report total archive bytes including descriptors/manifests, compression CPU time, peak memory, decoding/load time, and downloaded bytes for a new day and a corrected day.

No compression percentage is an acceptance promise yet. The normalized format must demonstrate a meaningful net improvement over a sorted/recompressed flat baseline while meeting the correctness and compatibility gates.

## Implementation order

1. Add storage profiling and benchmark candidate physical encodings on copies of representative data.
2. Finalize exact v2 Arrow schemas, field mapping, source-schema descriptors, tie ordering, manifest contract, and bundle validator.
3. Implement the converter and mixed-version reader; establish record and application equivalence.
4. Add version-aware daily maintenance, generation publication, download validation, and transfer support.
5. Validate Hugging Face loading and viewer behavior; publish migration documentation and reconstruction examples.
6. Migrate historical days incrementally and enable automatic conversion for newly closed days.
7. Only if measurements justify it, write normalized bundles directly during ingestion; retain closed-day consolidation.

Decisions still requiring measurements: archive Zstd level, encodings, row-group limits, bundle target size, and whether direct v2 ingestion earns enough benefit to justify changing the live writer.

## Documentation references

The compatibility assessment is based on official documentation; generated-file compatibility remains an implementation acceptance test.

- [Hugging Face Parquet loading, projection, filtering, and streaming](https://huggingface.co/docs/datasets/en/package_reference/loading_methods)
- [Hugging Face dataset configurations](https://huggingface.co/docs/hub/datasets-manual-configuration)
- [Hugging Face feature types and consistent schemas](https://huggingface.co/docs/datasets/en/about_dataset_features)
- [Hub data-file configuration and viewer row-group guidance](https://huggingface.co/docs/hub/datasets-data-files-configuration)
- [PyArrow Parquet compression and writer options](https://arrow.apache.org/docs/python/generated/pyarrow.parquet.ParquetWriter.html)
