//! Compaction of a closed UTC day into its final form: one file per table, rows sorted by
//! weapon, listing and time, every file with the table's [`schema`].
//!
//! Sorting a table needs its whole day in memory, plus a copy of the sort key. A day that
//! doesn't fit into the memory budget is sorted in ranges of weapons instead: the sort key starts
//! with the weapon, so sorting each range and writing the ranges in order gives the same rows in
//! the same order. A day with a single weapon too large for the budget is left as it is.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use arrow::array::{
    Array, ArrayRef, BooleanArray, RecordBatch, Scalar, StringArray, new_null_array,
};
use arrow::compute::kernels::cmp::{gt_eq, lt_eq};
use arrow::compute::{
    SortColumn, and, concat, filter_record_batch, interleave_record_batch, lexsort_to_indices,
};
use chrono::NaiveDate;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::layout::{PARQUET_DIR, Table};
use crate::schema::{attributes, auctions, gone, schema};
use crate::tx::{PARQUET_ZSTD_LEVEL, Transaction, unique_id};

/// Footer key that marks a file as the final form of its day, with [`SORTED_LAYOUT`].
pub const LAYOUT_KEY: &str = "wfmt.layout";
/// One file per table and day, sorted by [`sort_key`].
pub const SORTED_LAYOUT: &str = "sorted-1";

const MIB: usize = 1024 * 1024;
const READ_BATCH_ROWS: usize = 8192;
pub(crate) const ROW_GROUP_MAX_ROWS: usize = 64 * 1024;
pub(crate) const ROW_GROUP_MAX_BYTES: usize = 64 * MIB;
/// The smallest memory budget that leaves room to sort anything after the writer's buffer.
pub const MIN_MEMORY_MB: u64 = 2 * (ROW_GROUP_MAX_BYTES / MIB) as u64;

/// The columns a table's rows are sorted by. Each key is unique within a day, so the order is
/// total and doesn't depend on how the rows were stored before.
pub fn sort_key(table: Table) -> &'static [&'static str] {
    match table {
        Table::Auctions => &[
            auctions::WEAPON_SLUG,
            auctions::AUCTION_ID,
            auctions::SCRAPED_AT,
        ],
        Table::Attributes => &[
            attributes::WEAPON_SLUG,
            attributes::AUCTION_ID,
            attributes::SCRAPED_AT,
            attributes::POSITION,
        ],
        Table::Gone => &[gone::WEAPON_SLUG, gone::AUCTION_ID, gone::DETECTED_AT],
    }
}

/// The parquet files of one table on one day, sorted by name.
pub(crate) fn table_files(root: &Path, table: Table, date: NaiveDate) -> Result<Vec<PathBuf>> {
    let dir = root.join(PARQUET_DIR).join(table.day_folder(date));
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_file() && path.extension().is_some_and(|e| e == "parquet") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Whether a file's footer marks it as the final form of its day.
fn is_marked(path: &Path) -> Result<bool> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(reader
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .is_some_and(|kv| {
            kv.iter()
                .any(|kv| kv.key == LAYOUT_KEY && kv.value.as_deref() == Some(SORTED_LAYOUT))
        }))
}

/// Whether a group of tables (see [`Table::GROUPS`]) is in its final form on a closed day: no
/// files at all, or exactly one marked file per table, all with the same name.
fn group_complete(root: &Path, date: NaiveDate, tables: &[Table]) -> Result<bool> {
    let mut name = None;
    let mut empty = 0;
    for &table in tables {
        let files = table_files(root, table, date)?;
        match files.as_slice() {
            [] => empty += 1,
            [file] => {
                if !is_marked(file)? {
                    return Ok(false);
                }
                let file_name = file.file_name().map(ToOwned::to_owned);
                if name.get_or_insert_with(|| file_name.clone()) != &file_name {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
    }
    Ok(empty == 0 || empty == tables.len())
}

/// Whether a closed day is in its final form and can be published: compacted, with nothing
/// written to it since. A day without disappearances has no `gone` file.
pub fn day_complete(root: &Path, date: NaiveDate) -> Result<bool> {
    for tables in Table::GROUPS {
        if !group_complete(root, date, tables)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A batch read from any version of a table's files, in the table's current schema. Columns
/// the earliest files lack become nulls if the schema allows them; anything else that doesn't
/// fit, an unknown column or a different type, is an error.
fn canonical(table: Table, batch: &RecordBatch) -> Result<RecordBatch> {
    let schema = schema(table);
    for field in batch.schema().fields() {
        ensure!(
            schema.field_with_name(field.name()).is_ok(),
            "unknown {table} column {}",
            field.name()
        );
    }
    let columns = schema
        .fields()
        .iter()
        .map(|field| match batch.column_by_name(field.name()) {
            Some(column) => {
                ensure!(
                    column.data_type() == field.data_type(),
                    "{table} column {} is {}, expected {}",
                    field.name(),
                    column.data_type(),
                    field.data_type()
                );
                Ok(column.clone())
            }
            None if field.is_nullable() => Ok(new_null_array(field.data_type(), batch.num_rows())),
            None => bail!("{table} column {} is missing", field.name()),
        })
        .collect::<Result<Vec<ArrayRef>>>()?;
    RecordBatch::try_new(schema, columns).with_context(|| format!("{table} rows don't fit"))
}

/// Reads the files' batches in the canonical schema, passing each to `f`, until it breaks.
fn read_until(
    table: Table,
    files: &[PathBuf],
    mut f: impl FnMut(RecordBatch) -> Result<ControlFlow<()>>,
) -> Result<()> {
    for path in files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)
            .with_context(|| format!("reading {}", path.display()))?
            .with_batch_size(READ_BATCH_ROWS)
            .build()?;
        for batch in reader {
            let batch =
                canonical(table, &batch?).with_context(|| format!("in {}", path.display()))?;
            if f(batch)?.is_break() {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Reads all of the files' batches in the canonical schema, passing each to `f`.
fn read_each(
    table: Table,
    files: &[PathBuf],
    mut f: impl FnMut(RecordBatch) -> Result<()>,
) -> Result<()> {
    read_until(table, files, |batch| {
        f(batch).map(|()| ControlFlow::Continue(()))
    })
}

/// The bytes a batch holds in memory, the same measure for every path.
fn bytes(batch: &RecordBatch) -> usize {
    batch.get_array_memory_size()
}

/// All rows of the files, or `None` as soon as they would hold more than `limit` bytes.
fn read_within(table: Table, files: &[PathBuf], limit: usize) -> Result<Option<Vec<RecordBatch>>> {
    let mut batches = Vec::new();
    let mut held = 0;
    let mut over = false;
    read_until(table, files, |batch| {
        held += bytes(&batch);
        over = held > limit;
        if over {
            batches.clear();
            return Ok(ControlFlow::Break(()));
        }
        batches.push(batch);
        Ok(ControlFlow::Continue(()))
    })?;
    Ok((!over).then_some(batches))
}

/// Writes the rows of `batches` in `table`'s sort order. Only the sort key is copied to sort
/// it; the rows are then gathered from the batches one row group at a time, so this holds little
/// more than the batches themselves. Returns the number of rows.
fn write_sorted(
    table: Table,
    batches: &[RecordBatch],
    writer: &mut ArrowWriter<File>,
) -> Result<u64> {
    if batches.is_empty() {
        return Ok(0);
    }
    let columns: Vec<SortColumn> = sort_key(table)
        .iter()
        .map(|name| {
            let parts: Vec<&dyn Array> = batches
                .iter()
                .map(|b| b.column_by_name(name).expect("a sort column").as_ref())
                .collect();
            Ok(SortColumn {
                values: concat(&parts)?,
                options: None,
            })
        })
        .collect::<Result<_>>()?;
    let order = lexsort_to_indices(&columns, None)?;
    drop(columns);
    // Where each batch starts in the concatenated order.
    let starts: Vec<usize> = batches
        .iter()
        .scan(0, |start, b| {
            let first = *start;
            *start += b.num_rows();
            Some(first)
        })
        .collect();
    let batches: Vec<&RecordBatch> = batches.iter().collect();
    for chunk in order.values().chunks(ROW_GROUP_MAX_ROWS) {
        let rows: Vec<(usize, usize)> = chunk
            .iter()
            .map(|&row| {
                let row = row as usize;
                let batch = starts.partition_point(|&start| start <= row) - 1;
                (batch, row - starts[batch])
            })
            .collect();
        writer.write(&interleave_record_batch(&batches, &rows)?)?;
    }
    Ok(order.len() as u64)
}

/// The weapon column of a batch of `table`.
fn weapons(table: Table, batch: &RecordBatch) -> Result<&StringArray> {
    batch
        .column_by_name(sort_key(table)[0])
        .and_then(|c| c.as_any().downcast_ref::<StringArray>())
        .context("missing weapon column")
}

/// The bytes each weapon's rows hold, sharing a batch's bytes among its rows evenly.
fn weapon_sizes(table: Table, files: &[PathBuf]) -> Result<BTreeMap<String, usize>> {
    let mut sizes: BTreeMap<String, usize> = BTreeMap::new();
    read_each(table, files, |batch| {
        let per_row = bytes(&batch).div_ceil(batch.num_rows().max(1));
        let weapons = weapons(table, &batch)?;
        for i in 0..weapons.len() {
            *sizes.entry(weapons.value(i).to_owned()).or_default() += per_row;
        }
        Ok(())
    })?;
    Ok(sizes)
}

/// Splits the weapons of the files into ranges `(first, last)` whose rows hold at most `limit`
/// bytes.
fn weapon_ranges(table: Table, files: &[PathBuf], limit: usize) -> Result<Vec<(String, String)>> {
    let sizes = weapon_sizes(table, files)?;
    let mut ranges: Vec<(String, String)> = Vec::new();
    let mut held = 0;
    for (weapon, size) in sizes {
        ensure!(
            size <= limit,
            "{table} rows of {weapon} need about {} MiB, more than the compaction memory budget allows",
            size / MIB
        );
        match ranges.last_mut() {
            Some((_, last)) if held + size <= limit => {
                *last = weapon;
                held += size;
            }
            _ => {
                ranges.push((weapon.clone(), weapon));
                held = size;
            }
        }
    }
    Ok(ranges)
}

/// The rows of the files whose weapon is within `first..=last`.
fn read_range(
    table: Table,
    files: &[PathBuf],
    first: &str,
    last: &str,
) -> Result<Vec<RecordBatch>> {
    let (first, last) = (
        Scalar::new(StringArray::from(vec![first])),
        Scalar::new(StringArray::from(vec![last])),
    );
    let mut out = Vec::new();
    read_each(table, files, |batch| {
        let weapons = weapons(table, &batch)?;
        let keep: BooleanArray = and(&gt_eq(weapons, &first)?, &lt_eq(weapons, &last)?)?;
        let batch = filter_record_batch(&batch, &keep)?;
        if batch.num_rows() > 0 {
            out.push(batch);
        }
        Ok(())
    })?;
    Ok(out)
}

fn writer(path: &Path, table: Table) -> Result<ArrowWriter<File>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(PARQUET_ZSTD_LEVEL)?))
        .set_max_row_group_row_count(Some(ROW_GROUP_MAX_ROWS))
        .set_max_row_group_bytes(Some(ROW_GROUP_MAX_BYTES))
        .set_key_value_metadata(Some(vec![KeyValue::new(
            LAYOUT_KEY.to_owned(),
            SORTED_LAYOUT.to_owned(),
        )]))
        .build();
    Ok(ArrowWriter::try_new(
        File::create(path)?,
        schema(table),
        Some(props),
    )?)
}

/// Writes the files' rows to `out`, sorted. Returns the number of rows.
fn sort_into(table: Table, files: &[PathBuf], out: &Path, memory: usize) -> Result<u64> {
    // Sorting holds the rows, a copy of their sort key and one row group in its new order, next
    // to the writer's buffer of one row group. Measured on real days, the process peaks at about
    // 1.7 times the rows' size; half of what's left after the writer leaves room for that.
    let limit = memory.saturating_sub(ROW_GROUP_MAX_BYTES) / 2;
    let mut writer = writer(out, table)?;
    let mut rows = 0;
    if let Some(batches) = read_within(table, files, limit)? {
        let held: usize = batches.iter().map(bytes).sum();
        tracing::info!(%table, mib = held / MIB, "sorting a day in memory");
        rows += write_sorted(table, &batches, &mut writer)?;
    } else {
        let ranges = weapon_ranges(table, files, limit)?;
        tracing::info!(%table, ranges = ranges.len(), "sorting a large day in ranges of weapons");
        for (first, last) in ranges {
            rows += write_sorted(
                table,
                &read_range(table, files, &first, &last)?,
                &mut writer,
            )?;
        }
    }
    writer.close()?;
    File::open(out)?.sync_all()?;
    Ok(rows)
}

/// Rows in the footers of the files.
fn footer_rows(files: &[PathBuf]) -> Result<u64> {
    let mut rows = 0;
    for path in files {
        let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
        rows += u64::try_from(reader.metadata().file_metadata().num_rows())?;
    }
    Ok(rows)
}

/// What compacting one group of a day did.
pub(crate) struct Compacted {
    pub inputs: usize,
    pub outputs: usize,
}

/// Rewrites one group of tables of a closed day into its final form, in one transaction, unless
/// it already is. `memory` is the budget in bytes. Caller holds the exclusive dataset lock.
pub(crate) fn compact_group(
    root: &Path,
    date: NaiveDate,
    tables: &[Table],
    memory: usize,
) -> Result<Option<Compacted>> {
    if group_complete(root, date, tables)? {
        return Ok(None);
    }
    let files: Vec<Vec<PathBuf>> = tables
        .iter()
        .map(|&table| table_files(root, table, date))
        .collect::<Result<_>>()?;
    // Tables written together must have a part each, under the same name.
    let names = |files: &[PathBuf]| -> BTreeSet<_> {
        files
            .iter()
            .map(|p| p.file_name().map(ToOwned::to_owned))
            .collect()
    };
    for (table, table_files) in tables.iter().zip(&files).skip(1) {
        ensure!(
            names(table_files) == names(&files[0]),
            "{table} parts of {date} don't pair with the {} parts",
            tables[0]
        );
    }
    let id = unique_id();
    let mut tx = Transaction::new(root)?;
    let mut compacted = Compacted {
        inputs: 0,
        outputs: 0,
    };
    for (&table, files) in tables.iter().zip(&files) {
        let destination = Path::new(PARQUET_DIR)
            .join(table.day_folder(date))
            .join(format!("compact-{id}.parquet"));
        let stage = tx.stage(destination);
        let rows = sort_into(table, files, &stage, memory)
            .with_context(|| format!("compacting {table} of {date}"))?;
        ensure!(
            rows == footer_rows(files)? && rows == footer_rows(std::slice::from_ref(&stage))?,
            "compaction row count mismatch in {table} of {date}"
        );
        for path in files {
            tx.delete(path.strip_prefix(root)?.to_path_buf());
        }
        compacted.inputs += files.len();
        compacted.outputs += 1;
    }
    tx.commit()?;
    Ok(Some(compacted))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::schema::{AttributeRow, AuctionRow, GoneReason, GoneRow};
    use crate::sink::ParquetSink;
    use arrow::compute::concat_batches;
    use arrow::util::display::array_value_to_string;
    use chrono::{DateTime, Utc};
    use wfmt_types::{RivenType, WeaponClass};

    pub(crate) fn time(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    /// A sighting. `price` `None` leaves every optional column empty, which a batch of only such
    /// rows used to give a different schema than one with values.
    pub(crate) fn auction(weapon: &str, id: &str, at: &str, price: Option<i64>) -> AuctionRow {
        let at = time(at);
        AuctionRow {
            scraped_at: at,
            platform: "pc".into(),
            auction_id: id.into(),
            riven_id: format!("riven-{id}"),
            source: "price_asc".into(),
            weapon_slug: weapon.into(),
            weapon_name: weapon.into(),
            weapon_class: price.map(|_| WeaponClass::Rifle),
            riven_type: RivenType::Rifle,
            is_bow: false,
            riven_name: "name".into(),
            polarity: "madurai".into(),
            mastery_level: price.map(|_| 10),
            re_rolls: price.map(|_| 1),
            n_positive: 2,
            n_negative: 0,
            has_spliced: false,
            listed_rank: 8,
            rank: price.map(|_| 8),
            disposition: price.map(|_| 1.0),
            legacy_stats: price.map(|_| false),
            fit_ok: price.is_some(),
            fit_ambiguous: price.map(|_| false),
            fit_error: price.is_none().then(|| "no fit".into()),
            mean_positive_quality: price.map(|_| 0.5),
            buyout_price: price,
            starting_price: price,
            top_bid: price,
            is_direct_sell: true,
            minimal_reputation: price.map(|_| 0),
            visible: true,
            closed: false,
            private: false,
            crossplay: true,
            created: at,
            updated: at,
            owner_id: "owner".into(),
            owner_name: "owner".into(),
            owner_reputation: price.map(|_| 1),
            owner_status: "ingame".into(),
            owner_last_seen: price.map(|_| at),
            note: String::new(),
        }
    }

    pub(crate) fn attribute(a: &AuctionRow, position: i32) -> AttributeRow {
        let fitted = a.fit_ok.then_some(0.5);
        AttributeRow {
            scraped_at: a.scraped_at,
            auction_id: a.auction_id.clone(),
            weapon_slug: a.weapon_slug.clone(),
            position,
            attribute: "critical_chance".into(),
            positive: true,
            spliced: fitted.map(|_| false),
            value: 100.0,
            value_rank8: fitted,
            roll: fitted,
            quality: fitted,
            coefficient: fitted,
            layout_multiplier: fitted,
        }
    }

    pub(crate) fn gone_row(weapon: &str, id: &str, at: &str, price: Option<i64>) -> GoneRow {
        GoneRow {
            detected_at: time(at),
            weapon_slug: weapon.into(),
            auction_id: id.into(),
            reason: GoneReason::Missing,
            riven_id: format!("riven-{id}"),
            last_price: price,
            direct_sell: true,
            owner_name: "owner".into(),
            created: time(at),
            first_seen: time(at),
            last_seen: time(at),
        }
    }

    /// Writes one part per entry, the way the scraper does: a sighting with its stats, and the
    /// disappearances found in the same fetch.
    pub(crate) fn write_parts(root: &Path, parts: Vec<(Vec<AuctionRow>, Vec<GoneRow>)>) {
        let mut sink = ParquetSink::new(root.join(PARQUET_DIR));
        for (rows, gone) in parts {
            let at = rows
                .first()
                .map(|r| r.scraped_at)
                .or(gone.first().map(|g| g.detected_at))
                .unwrap();
            sink.prepare(at).unwrap();
            for row in rows {
                sink.attributes
                    .extend((0..2).map(|position| attribute(&row, position)));
                sink.auctions.push(row);
            }
            sink.gone.extend(gone);
            sink.flush().unwrap();
        }
    }

    /// Every row of the files, in file order, in the canonical schema.
    pub(crate) fn read_all(table: Table, files: &[PathBuf]) -> RecordBatch {
        let mut batches = Vec::new();
        read_each(table, files, |b| {
            batches.push(b);
            Ok(())
        })
        .unwrap();
        concat_batches(&schema(table), &batches).unwrap()
    }

    fn cell(batch: &RecordBatch, column: &str, row: usize) -> String {
        let column = batch.column_by_name(column).unwrap();
        // Shown as milliseconds: naming the time zone needs a feature arrow is built without.
        let column = match column.data_type() {
            arrow::datatypes::DataType::Timestamp(..) => {
                arrow::compute::cast(column, &arrow::datatypes::DataType::Int64).unwrap()
            }
            _ => column.clone(),
        };
        array_value_to_string(&column, row).unwrap()
    }

    /// Every row as text, in order.
    pub(crate) fn rows_text(batch: &RecordBatch) -> Vec<String> {
        let schema = batch.schema();
        (0..batch.num_rows())
            .map(|i| {
                schema
                    .fields()
                    .iter()
                    .map(|f| format!("{}={}", f.name(), cell(batch, f.name(), i)))
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    fn date(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Parts with every null pattern the scraper writes, out of order and on several weapons.
    fn day(root: &Path) {
        write_parts(
            root,
            vec![
                (
                    vec![
                        auction("soma", "s1", "2026-11-01T04:00:00Z", Some(100)),
                        auction("kulstar", "k1", "2026-11-01T04:00:00Z", Some(50)),
                    ],
                    vec![gone_row("soma", "s0", "2026-11-01T04:00:00Z", Some(80))],
                ),
                (
                    vec![auction("kulstar", "k2", "2026-11-01T01:00:00Z", None)],
                    vec![gone_row("kulstar", "k0", "2026-11-01T01:00:00Z", None)],
                ),
                (
                    vec![
                        auction("soma", "s1", "2026-11-01T00:00:00Z", Some(120)),
                        auction("acrid", "a1", "2026-11-01T00:00:00Z", None),
                    ],
                    Vec::new(),
                ),
                (
                    vec![auction("kulstar", "k1", "2026-11-01T00:00:00Z", Some(55))],
                    Vec::new(),
                ),
            ],
        );
    }

    #[test]
    fn parts_share_one_schema_whatever_their_nulls() {
        let temp = tempfile::tempdir().unwrap();
        day(temp.path());
        for table in Table::ALL {
            for path in table_files(temp.path(), *table, date("2026-11-01")).unwrap() {
                let reader =
                    ParquetRecordBatchReaderBuilder::try_new(File::open(path).unwrap()).unwrap();
                let mut written = reader.schema().as_ref().clone();
                written.metadata.clear();
                assert_eq!(&written, schema(*table).as_ref());
            }
        }
    }

    #[test]
    fn a_day_becomes_one_sorted_file_per_table() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let d = date("2026-11-01");
        day(root);
        assert!(!day_complete(root, d).unwrap());
        let before: Vec<Vec<String>> = Table::ALL
            .iter()
            .map(|&t| {
                let mut rows = rows_text(&read_all(t, &table_files(root, t, d).unwrap()));
                rows.sort();
                rows
            })
            .collect();

        for tables in Table::GROUPS {
            compact_group(root, d, tables, 1024 * MIB).unwrap().unwrap();
        }
        assert!(day_complete(root, d).unwrap());
        for (&table, before) in Table::ALL.iter().zip(before) {
            let files = table_files(root, table, d).unwrap();
            assert_eq!(files.len(), 1, "{table}");
            assert!(is_marked(&files[0]).unwrap());
            let batch = read_all(table, &files);
            let key = |i: usize| -> Vec<String> {
                sort_key(table).iter().map(|c| cell(&batch, c, i)).collect()
            };
            for i in 1..batch.num_rows() {
                assert!(key(i - 1) < key(i), "{table} row {i} is out of order");
            }
            let mut after = rows_text(&batch);
            after.sort();
            assert_eq!(after, before, "{table} kept every row");
        }
        let auctions = table_files(root, Table::Auctions, d).unwrap();
        let attributes = table_files(root, Table::Attributes, d).unwrap();
        assert_eq!(auctions[0].file_name(), attributes[0].file_name());

        // Already final: nothing to do.
        for tables in Table::GROUPS {
            assert!(
                compact_group(root, d, tables, 1024 * MIB)
                    .unwrap()
                    .is_none()
            );
        }

        // A late part reopens the day, and the next compaction takes it in.
        write_parts(
            root,
            vec![(
                vec![auction("acrid", "a2", "2026-11-01T23:00:00Z", Some(10))],
                Vec::new(),
            )],
        );
        assert!(!day_complete(root, d).unwrap());
        compact_group(root, d, Table::GROUPS[0], 1024 * MIB)
            .unwrap()
            .unwrap();
        assert!(day_complete(root, d).unwrap());
        let auctions = read_all(
            Table::Auctions,
            &table_files(root, Table::Auctions, d).unwrap(),
        );
        assert!(
            rows_text(&auctions)
                .iter()
                .any(|r| r.contains("auction_id=a2 "))
        );
    }

    #[test]
    fn ranges_of_weapons_give_the_same_rows_as_one_sort() {
        let temp = tempfile::tempdir().unwrap();
        day(temp.path());
        let d = date("2026-11-01");
        for &table in Table::ALL {
            let files = table_files(temp.path(), table, d).unwrap();
            let whole = temp.path().join(format!("whole-{table}.parquet"));
            let ranged = temp.path().join(format!("ranged-{table}.parquet"));
            let rows = sort_into(table, &files, &whole, 1024 * MIB).unwrap();
            // Room for the largest weapon, not for the whole day.
            let largest = *weapon_sizes(table, &files).unwrap().values().max().unwrap();
            assert!(read_within(table, &files, largest).unwrap().is_none());
            assert!(weapon_ranges(table, &files, largest).unwrap().len() > 1);
            let memory = ROW_GROUP_MAX_BYTES + 2 * largest;
            assert_eq!(sort_into(table, &files, &ranged, memory).unwrap(), rows);
            assert_eq!(
                read_all(table, &[whole]),
                read_all(table, &[ranged]),
                "{table}: same schema, rows and order"
            );
        }
    }

    #[test]
    fn a_weapon_too_large_for_the_budget_fails_the_day() {
        let temp = tempfile::tempdir().unwrap();
        day(temp.path());
        let d = date("2026-11-01");
        let files = table_files(temp.path(), Table::Auctions, d).unwrap();
        let out = temp.path().join("out.parquet");
        let error = sort_into(Table::Auctions, &files, &out, ROW_GROUP_MAX_BYTES + 2).unwrap_err();
        assert!(error.to_string().contains("budget"), "{error:#}");
    }

    #[test]
    fn unknown_columns_fail_instead_of_being_dropped() {
        let batch = RecordBatch::try_from_iter([(
            "surprise",
            std::sync::Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
        )])
        .unwrap();
        assert!(canonical(Table::Gone, &batch).is_err());
    }

    #[test]
    fn the_earliest_files_get_nulls_for_columns_they_lack() {
        let temp = tempfile::tempdir().unwrap();
        let row = auction("soma", "s1", "2026-11-01T00:00:00Z", Some(1));
        let full = crate::schema::auctions_batch(&[row]).unwrap();
        let legacy = full
            .project(
                &(0..full.num_columns())
                    .filter(|&i| {
                        let name = full.schema().field(i).name().clone();
                        name != auctions::RIVEN_ID && name != auctions::SOURCE
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        let path = temp.path().join("legacy.parquet");
        let mut writer =
            ArrowWriter::try_new(File::create(&path).unwrap(), legacy.schema(), None).unwrap();
        writer.write(&legacy).unwrap();
        writer.close().unwrap();
        let mut batches = Vec::new();
        read_each(Table::Auctions, &[path], |b| {
            batches.push(b);
            Ok(())
        })
        .unwrap();
        assert_eq!(batches[0].schema(), schema(Table::Auctions));
        assert_eq!(
            batches[0]
                .column_by_name(auctions::RIVEN_ID)
                .unwrap()
                .null_count(),
            1
        );
    }
}
