# Long-term Parquet storage design

Status: stage 1a, 1b, 1d and 1e are implemented and tested, not yet deployed. 1c and stage 2 are
still proposals. See "Implementation notes" at the end of stage 1.

## Objective

Keep Parquet as the permanent historical record and make it cheap to keep for years, without
changing what is recorded. Scrape frequency, admission rules, feed suppression and raw retention
stay as they are, and every recorded sighting is kept: repeat sightings feed chart buckets,
first/last seen times and observation counts.

Everything the scraper writes, sweeps and feed polls alike, must end up in one well-formed file
per table per closed day.

The archive must stay readable by the application and by Hugging Face `datasets` or DuckDB with no
custom loader.

## What the data looks like

Measured on the published dataset, 2026-11-07 to 2026-11-13, which includes the `feed_recent` and
`feed_popular` polls. Sizes are `auctions` plus `attributes` of 2026-11-12, rewritten with DuckDB
at Zstd 3 and 64K-row groups; the Rust writer will differ slightly. The appendix has the commit,
versions and queries.

| Layout | Size | Change |
| --- | --- | --- |
| Published files | 59.9 MB | |
| Same flat schema, sorted | 23.3 MB | -61% |
| Normalized (stage 2 below) | 14.7 MB | -76% |

Zstd 19 only reaches 21.1 MB and 13.2 MB, so the level is not worth changing.

Facts the design relies on:

- A day is about 480k sightings of 98k auctions. The median auction has 6 sightings, from the
  four-hourly sweeps; the busiest had 19 (40 on 2026-11-08).
- Feeds add little volume: 12,073 rows for 11,239 auctions, 2.5% of the day. They arrive in about
  590 polls a day that each write their own small part.
- Payloads barely change: 98,585 distinct listing payloads for 98,459 auctions, and 96,706
  distinct attribute sets for 481,414 sightings.
- `(auction_id, scraped_at)` is unique in `auctions` on every day, feeds included, and
  `(auction_id, scraped_at, position)` in `attributes`. Every sighting has attribute rows.
- 83% of one day's auctions were already listed the day before. Over two days, 190k attribute
  sets stored per day collapse to 110k distinct ones.
- One early file lacks `riven_id` and `source`. Both are always written non-null since, so a null
  can only mean "not recorded".
- `gone` is 0.1 to 0.7 MB a day. `(auction_id, detected_at)` is unique in it.
- Equal timestamps across listings are common. On 2026-11-12, 2,726 riven-and-time pairs had
  sightings of several auctions, 1,668 of them at different prices. Over 2026-11-10 to 2026-11-13,
  381 riven-and-time pairs had several auctions going at once.
- One day decodes from 57 MB (`auctions`) and 40 MB (`attributes`) of encoded pages. In Arrow
  memory that is a few times larger, not yet measured.

## Problem found: compaction leaves fragments

Closed days are not being merged into one file. 2026-11-12 was published as seven `auctions`
files: one with 466,918 rows, one with 13,529, and five with 1 to 716 rows. Days since 2026-11-07
have five to nine files per table.

The cause is in `dataset/src/schema.rs`. The batches are built with `RecordBatch::try_from_iter`,
which marks a column nullable only if that batch happens to contain a null. A sweep flush has
nulls in most optional columns; a feed poll with a handful of rows often has none in `top_bid`,
`buyout_price` or the fit columns. `maintain` groups parts by the debug string of their schema,
nullability included, so each null pattern becomes its own group and is compacted separately. A
pattern that occurs once stays an uncompacted `part-` file, because groups of one are skipped. A
one-row part costs 12 KB.

The same applies to `attributes` (two patterns on 2026-11-12) and to `gone`.

This is what stage 1 fixes first. It also makes the published schema consistent, which it
currently is not from file to file.

## Decisions

1. Fix the schema and sort closed days in the existing compaction. The file format does not
   change for readers; the store only gets deterministic tie-breaking.
2. Normalize only if that is not enough, and decide it with numbers (see the gate below).
3. One file per table per closed day. No bundles, generations or sidecar manifests.
4. Files describe themselves through Parquet footer metadata. Joins use ordinary columns.
5. `gone` keeps its columns in every stage.
6. Standard Parquet only: Zstd 3, default encodings, no external dictionaries.
7. The UTC day stays the unit of publication, replacement and recovery.
8. A day is published only when it is complete, checked per day from its files.
9. Query results never depend on the physical order of rows.

## Stage 1: one sorted file per closed day

### 1a. Explicit schemas

In `dataset/src/schema.rs`, define one Arrow schema per table and build batches against it with
`RecordBatch::try_new`. Nullability follows the row structs: `Option` fields are nullable, the
rest are not. `riven_id` and `source` are nullable too, so the early file fits the same schema.

From then on every part the sink writes has the same schema, whatever its rows contain.

### 1b. Compaction

In `dataset/src/maintenance.rs`, for each closed day and table group:

- Read every part and cast it to the canonical schema: same columns and types, nullability
  ignored, missing `riven_id` and `source` filled with nulls. A part with an unknown column or a
  different type fails the day with a diagnostic and leaves it untouched.
- Sort, and write one file per table:
  - `auctions`: by `weapon_slug`, `auction_id`, `scraped_at`.
  - `attributes`: by `weapon_slug`, `auction_id`, `scraped_at`, `position`.
  - `gone`: by `weapon_slug`, `auction_id`, `detected_at`.
- Mark outputs with footer key-value metadata, `wfmt.layout = sorted-1`.
- Keep the same transaction: stage outputs, check row counts, delete inputs, commit.
- A day that fails is logged and skipped; the pass continues with the other days.

A day is complete when:

- `auctions` and `attributes` each have exactly one file, with the same name, both marked; and
- `gone` has no file, or exactly one marked file. A day without disappearances has none.

This replaces the "fewer than two parts" check. A complete day is not rewritten.

The 128 MiB input grouping goes away for closed days, since one sorted file needs the whole day.
Keep the 64K-row and 64 MiB row-group limits. Sorting is bounded by a memory budget, a new
maintenance option:

- The budget covers what a sort really holds: the decoded Arrow batches, the sort indices, the
  reordered copy, and the writer's row-group buffer. Sorting a table needs roughly twice its
  decoded size, plus one row group.
- Count the decoded bytes while reading, from the batches' own buffer sizes. Footer sizes are
  only a first guess. Sort in memory, one table at a time, as long as the count stays within the
  share of the budget that leaves room for the rest.
- When reading would pass that share, switch to splitting: divide the weapons into ranges that
  fit, then for each range read the parts again keeping only its rows, sort them, and append them
  in order. The sort key leads with `weapon_slug`, so the rows come out in the same order.
- If a single range still does not fit, fail the day and leave it as it is.

Before choosing the default, measure the peak resident memory of sorting the largest current day,
and set the budget from that with headroom.

Outputs still get a new unique name, so the Hub's name-and-size comparison in `src/hub.rs` picks
up every rewritten day. Leading with `weapon_slug` gives row-group statistics that let a reader
fetch one weapon's history without scanning the day.

Rows that arrive for a day after it was compacted (a late flush, a recovered raw archive) are
ordinary parts without the marker. The next pass sees more than one file and merges again.

### 1c. The open day

Today's parts are left alone until the day closes, so the open day holds one part per flush: about
590 from feed polls alone, per table, plus the sweep flushes. Count the parts and their bytes on
the production server before deciding anything here.

If it is a problem, extend the maintenance pass, which already runs every six hours, to merge
today's parts into one unsorted file per table, using the existing streaming merge. Do not batch
feed flushes in the sink instead: that would delay new listings in the app, which is what the
feeds are for.

A merge replaces loaded files, so the store rebuilds from scratch (`store/src/lib.rs`). That
already happens when closed days are compacted, but it is the reason not to merge more often than
the maintenance interval.

### 1d. Publish only complete days

`src/hub.rs` uploads every day before `compacted_before`, whatever files it holds. With failed
days skipped and late parts possible, that watermark no longer says a day is ready.

In `prepare`, under the dataset lock it already takes, upload a day only if it is complete by the
rule above. A day that is not keeps its previously published version until a later pass completes
it. `compacted_before` stays as the cut-off between closed days and the open one.

### 1e. Make the store independent of row order

Sorting changes the order of rows with equal timestamps, and the store's results depend on that
order today:

- `finish_load` sorts observations and gone events by time alone, so equal times keep file order.
- The history's `gone_by_bucket` keeps one price per riven and bucket; the last event wins. When
  two copies of a riven go at the same time, file order picks the price.

Two orders are already total and stay as they are, covered by the new test: `newer` compares
`recency`, which ends in the auction ID, and the listing order falls back to the auction ID.

Rather than preserve file order, remove the dependence. Gone events at the same time are ordered
by their auction's `recency`, so when copies of a riven go at once the newest copy speaks for it,
in `gone_by_bucket` and on the gone tab. Observations need no tie-break: an auction has at most one
per time, and copies of a riven are told apart through `recency`. Results then follow from the
rows alone, and no stored order column is needed in any stage.

Three more dependences turned up when comparing answers before and after compacting real days.
All went through ids the store assigns in load order:

- The stats a similar riven lacks or adds were sorted by attribute id. They are now sorted by
  name.
- Stat effects ordered the regression's columns by attribute id, which changed its rounding. The
  columns are now ordered by name.
- Stat effects took the cheapest copy of a riven; between copies at the same price, load order
  decided. The newest copy now wins.

This can change the gone median of a bucket in which two copies of a riven went at once. Write
the expected result for that case by hand in the new test. If an existing snapshot changes, check
that each difference is such a bucket before accepting it.

### Acceptance

- Per day, the multiset of rows is identical before and after, for all three tables.
- A day built from parts with every null pattern seen in production ends as one file per table.
- Order independence, as a test: build a dataset with equal-time sightings of several copies of a
  riven at different prices, equal-time gone events for a riven within one bucket, and equally
  recent copies. Load it in several row and part orders, sorted included; every query result must
  be identical. Run the same check over a real day.
- The over-budget path gives the same schema and the same rows in the same order as the in-memory
  path on the same input, and the same query results. The files need not match byte for byte:
  row-group boundaries may differ.
- A day with late parts, or one that failed, is not uploaded; its previous version stays.
- A second maintenance pass rewrites nothing.

### Implementation notes

What was built, where it differs from the text above, and what it measured.

- Code: `dataset/src/compact.rs` (canonical schema reading, sorting, completeness),
  `dataset/src/schema.rs` (fixed schemas), `dataset/src/maintenance.rs` (the pass),
  `src/hub.rs` (holding back incomplete days), `store/src/load.rs`, `query.rs` and `detail.rs`
  (tie-breaks). Tests: `dataset/src/compact.rs`, `store/tests/row_order.rs`.
- The memory option is `WFMT_COMPACT_MEMORY_MB` (`--compact-memory-mb`), default 1024, minimum
  128. It replaces `WFMT_COMPACT_TARGET_MB`.
- The sort copies only the key columns and gathers the output one row group at a time, so it
  holds the rows once rather than twice. A whole-day sort with `concat` and `take` peaked at
  2.8 times the decoded size; this one at 1.7 times.
- The over-budget path reads the parts once per weapon range instead of spilling to temporary
  files. It costs decoding the day a few times, with nothing to clean up after a crash.
- A day that fails keeps its files and its raw archives, the pass compacts the other days, and
  then ends with an error naming the failed days.
- One snapshot changed: two stat lists on a riven page are now alphabetical.

Measured on the published days 2026-11-07 to 2026-11-13 (14.9 million rows):

| | Result |
| --- | --- |
| Files | 101 to 21, one per table per day |
| Size | 403 MB to 154 MB |
| Largest table-day, decoded | 195 MiB (`attributes`) |
| Default budget (1024 MiB), all in memory | 7 s, peak 329 MiB |
| 384 / 256 / 160 MiB budgets | peak 285 / 184 / 120 MiB, 12 to 14 of 21 tables split |
| Answers before and after compacting | identical for all 1,432 queries of `golden.rs` |
| Split path against in-memory path | the same rows at the same file positions |

Against the committed store code on the same uncompacted data, the tie-breaks change 649 of
those answers, all ties that load order used to settle:

- 239 gone lists: only the order of listings that disappeared at the same time. Times and totals
  are the same.
- 43 chart points: the gone median of a bucket in which two copies of a riven went at once.
- 48 stat lists on riven pages, now alphabetical.
- Stat effects: float rounding, around 1e-15.

### Rollout

Every published day is rewritten once and uploaded again. The old blobs stay in the repository
history; squash it afterwards if that matters.

## Gate for stage 2

Stage 1 lands at roughly 23 MB per day, about 8.5 GB per year. Stage 2 saves about 9 MB per day
more, around 3.2 GB per year, and costs a converter, a second reader path, a new public schema and
a new dataset repository.

Go ahead only if, after a month of stage 1 data, at least one holds:

- The Hub repository or the server's disk is on course to exceed what is acceptable to keep.
- Download time for a fresh `WFMT_HF_PULL_DATASET` server is a problem.
- The store work below wants per-listing data stored once anyway.

Otherwise stop at stage 1.

## Stage 2: normalized closed days

The open day keeps the flat format and writer. Conversion happens when a day closes, in the same
maintenance pass, and replaces the day's `auctions` and `attributes` files.

Layout:

```text
parquet/
  observations/date=YYYY-MM-DD/<id>.parquet
  listing_states/date=YYYY-MM-DD/<id>.parquet
  attribute_sets/date=YYYY-MM-DD/<id>.parquet
  gone/date=YYYY-MM-DD/<id>.parquet            unchanged
```

All three new files of a day share `<id>` and carry `wfmt.layout = normalized-1` in the footer.
Every day is self-contained: IDs are only meaningful together with `archive_date`.

### `listing_states`

One row per distinct listing payload of the day. Key: `(archive_date, listing_state_id)`.

- `archive_date` (date32), `listing_state_id` (int64), `attribute_set_id` (int64),
  `attribute_count` (int32).
- Everything from `auctions` except the observation columns below: identity (`platform`,
  `auction_id`, `riven_id`), weapon and item fields, model results, `is_direct_sell`,
  `minimal_reputation`, `visible`, `closed`, `private`, `crossplay`, `created`, `note`,
  `owner_id`, `owner_name`.

Sorted by `weapon_slug`, `auction_id`. IDs are assigned in that order, so one weapon occupies a
contiguous ID range and the row-group statistics of `observations` still allow fetching a single
weapon.

### `attribute_sets`

One row per stat of a distinct ordered set. Key: `(archive_date, attribute_set_id, position)`.

Columns: `archive_date`, `attribute_set_id`, `position`, `attribute`, `positive`, `spliced`,
`value`, `value_rank8`, `roll`, `quality`, `coefficient`, `layout_multiplier`.

Sets are equal only if every value in every position is equal, model outputs included. Never
deduplicate by `riven_id`. `attribute_count` on the state tells an empty set from missing rows.

### `observations`

One row per sighting, from sweeps and feeds alike. Key:
`(archive_date, listing_state_id, scraped_at)`, unique because `(auction_id, scraped_at)` is.

Columns: `archive_date`, `listing_state_id`, `scraped_at`, `source`, `buyout_price`,
`starting_price`, `top_bid`, `updated`, `owner_reputation`, `owner_status`, `owner_last_seen`.

Sorted by `listing_state_id`, `scraped_at`. No surrogate ID and no stored order column: the key is
a total order.

### Conversion rules

- Compare stored values exactly; floats by bit pattern, null equal only to null.
- Fail the day, leaving it flat, if `(auction_id, scraped_at)` is not unique, if an attribute row
  has no sighting, or if a set's rows do not match its count.
- Before committing, rebuild the flat rows by joining the three tables and compare them with the
  inputs as multisets.
- Commit the three outputs and the deletion of the flat files in one `Transaction`.
- Late rows for a converted day arrive as flat parts. The next pass rebuilds the day from its
  normalized files plus those parts; the local loader reads both in the meantime, since no
  sighting is in both.
- A normalized day is complete when the three tables each have exactly one marked file with the
  same `<id>`, no flat parts remain for the date, and `gone` meets its stage 1 rule. Only complete
  days are uploaded, as in 1d. Uploading a mixed day would hide the flat rows from anyone loading
  `observations`.
- Conversion never refits or reassigns riven IDs. `reprocess --replace` rebuilds the whole dataset
  in the flat format as today, and the next maintenance pass converts it again.

### Code touched

- `dataset/src/layout.rs`: three new `Table` variants and their group.
- `dataset/src/schema.rs`: column names and schemas.
- `dataset/src/maintenance.rs`: the converter.
- `store/src/load.rs`: discover normalized days next to flat ones, and build the same in-memory
  auctions and observations from them. The existing "loaded path disappeared, rebuild" logic in
  `store/src/lib.rs` already covers a day switching representation.
- `src/hub.rs`: the completeness rule for normalized days, and the dataset card.
- `src/hub/pull.rs` and `src/transfer.rs`: before replacing a local day, check what arrived. The
  three files share an `<id>` and carry the marker, every `listing_state_id` in `observations`
  exists in `listing_states`, every `attribute_set_id` exists unless `attribute_count` is zero,
  and each set has as many rows as its count. A day that fails stays as it was. The pull already
  downloads from one commit; keep that.

### Hugging Face

Publish stage 2 to a new dataset repository and freeze the old one, since the `auctions` config
changes shape. One config per table, each with a glob over `date=*`.

IDs are local to a day and are reassigned when a day is rewritten, so tables loaded from different
commits can join wrongly. The card tells consumers to pass the same `revision` to every
`load_dataset` call, and shows it in the example.

The card gives the two queries that rebuild the old tables, the same ones the converter uses for
its comparison:

```sql
-- auctions: one row per sighting
SELECT s.* EXCLUDE (archive_date, listing_state_id, attribute_set_id, attribute_count),
       o.* EXCLUDE (archive_date, listing_state_id)
FROM observations o
JOIN listing_states s USING (archive_date, listing_state_id);

-- attributes: one row per stat of each sighting; a sighting with no stats has no rows
SELECT o.scraped_at, s.auction_id, s.weapon_slug,
       a.* EXCLUDE (archive_date, attribute_set_id)
FROM observations o
JOIN listing_states s USING (archive_date, listing_state_id)
JOIN attribute_sets a USING (archive_date, attribute_set_id);
```

Before migrating history, upload a few converted days to a test repository and check
`load_dataset` in normal and streaming mode, and the Hub viewer.

## Later, only if measured

Monthly listing tables. Most listings carry over to the next day, so states and attribute sets,
about two thirds of a normalized day, are largely stored again daily. Folding a closed month into
one set of tables would cut stage 2 substantially. It makes the month the replacement unit for old
data and complicates incremental pulls, so it needs its own design and its own gate.

## Separate problem: store memory

No file layout fixes this. `store/src/load.rs` keeps every sighting of every day in memory at 24
bytes each, about 4 GB per year of history at the current rate, and rebuilds everything when a
file is replaced. This will limit the server long before disk does.

Plan it separately. The storage side only has to make it possible: day partitions allow loading a
recent window in full, and the weapon-first sort lets older history be read per weapon on demand.

## Order of work

1. Done: stage 1e, the store's tie-breaking and its order-independence test.
2. Done: stage 1a, 1b and 1d. Remaining: deploy, which rewrites every published day in the first
   maintenance pass and uploads them again.
3. Count open-day parts on the production server and decide on 1c.
4. Run for a month; record bytes per day, files per day, maintenance time and peak memory.
5. Decide on stage 2 with the gate.
6. If yes: schemas and converter, then the reader, then the test repository, then migrate history
   day by day and switch the published dataset.
7. Start the store memory design whenever history passes a few months, regardless of stage 2.

## Appendix: how the numbers were measured

- Dataset `LLukas22/wf-riven-data` at commit `ca36876817b40394b68312a3ceb937379857b5db`, days
  2026-11-07 to 2026-11-13.
- DuckDB 2.0.0 through its Python package, session time zone UTC.
- `union_by_name = true` on every read. Exclude the `date` partition column that DuckDB adds from
  the path before comparing rows across days.

With `A` the day's `auctions` files and `T` its `attributes` files:

```sql
-- observation columns, everything else is the listing payload
-- OBS = scraped_at, source, buyout_price, starting_price, top_bid, updated,
--       owner_reputation, owner_status, owner_last_seen

-- distinct payloads and attribute sets
SELECT count(*) FROM (SELECT DISTINCT * EXCLUDE (OBS) FROM A);
SELECT count(DISTINCT s) FROM (
  SELECT list(struct_pack(position, attribute, positive, spliced, value, value_rank8, roll,
                          quality, coefficient, layout_multiplier) ORDER BY position) AS s
  FROM T GROUP BY auction_id, scraped_at);

-- key uniqueness
SELECT count(*) FROM (SELECT 1 FROM A GROUP BY auction_id, scraped_at HAVING count(*) > 1);

-- null pattern per file, the cause of the fragments
SELECT file_name, list(name ORDER BY name) FROM parquet_schema('auctions/date=2026-11-12/*.parquet')
WHERE repetition_type = 'OPTIONAL' GROUP BY 1;

-- sizes: each written with
--   COPY (...) TO 'x.parquet' (FORMAT parquet, COMPRESSION zstd, COMPRESSION_LEVEL 3,
--                              ROW_GROUP_SIZE 65536)
-- flat sorted:  SELECT * FROM A ORDER BY weapon_slug, auction_id, scraped_at
--               SELECT * FROM T ORDER BY weapon_slug, auction_id, scraped_at, position
-- normalized:   the three stage 2 tables, built with the keys and sort orders given above
```

These are the queries in outline. Commit the full benchmark script with the stage 1
implementation, so the figures can be rerun against later data.

The normalized figure was measured with an extra `bundle_id` string, an `observation_id` and
`attribute_set_id` on `observations`, none of which stage 2 keeps, so it slightly overstates the
size.
