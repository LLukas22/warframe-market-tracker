//! Warframe's weekly riven trade report: what rivens sold for in in-game trades over the past
//! week, per weapon, published by Digital Extremes for each platform. Unlike warframe.market's
//! asking prices, these are prices that were paid.
//!
//! Each platform's report is kept as it was published, one file per week:
//!
//! ```text
//! weekly_rivens/<platform>/<YYYY-MM-DD>.parquet   the rows, see `Entry`
//! weekly_rivens/raw/<platform>/<YYYY-MM-DD>.js.gz the response as it came
//! ```
//!
//! The date is the UTC day the report was published (its `Last-Modified`), a Monday so far.
//! Files are written once and never changed, and the Hub dataset has the same paths.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use arrow::array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, RecordBatch, StringArray,
    TimestampMillisecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, TimeUnit};
use chrono::{DateTime, NaiveDate, Utc};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use serde::{Deserialize, Serialize};

use crate::tx::{PARQUET_ZSTD_LEVEL, durable_write};

/// The folder, relative to the data directory and in the Hub dataset.
pub const DIR: &str = "weekly_rivens";
/// Inside [`DIR`]: the responses as they came. Kept locally, not uploaded.
pub const RAW_DIR: &str = "raw";

/// The platforms with a report: our name, and the one in its URL.
pub const PLATFORMS: [(&str, &str); 4] =
    [("pc", "PC"), ("ps4", "PS4"), ("xb1", "XB1"), ("swi", "SWI")];

/// Where a platform's current report is published.
pub fn url(platform: &str) -> Option<String> {
    PLATFORMS
        .iter()
        .find(|(name, _)| *name == platform)
        .map(|(_, code)| format!("https://www-static.warframe.com/repos/weeklyRivens{code}.json"))
}

/// One row of a report: a weapon's rivens, rolled or not, or for `compatibility: None`, the
/// unveiled rivens of a type. Prices are in platinum.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    /// `Rifle Riven Mod`, `Melee Riven Mod`, …
    pub item_type: String,
    /// The weapon's English name, `None` for veiled rivens.
    pub compatibility: Option<String>,
    /// Rerolled at least once.
    pub rerolled: bool,
    pub avg: f64,
    pub stddev: f64,
    pub min: f64,
    pub max: f64,
    /// How often it was traded compared to the rest, from 1 to 100.
    pub pop: f64,
    pub median: f64,
}

/// A row as stored: the [`Entry`] and the report it's from.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub date: NaiveDate,
    pub platform: String,
    pub published_at: Option<DateTime<Utc>>,
    pub entry: Entry,
}

/// Reads a report. It looks like JSON but is a JavaScript literal: unquoted keys and single
/// quoted strings. Strict JSON is accepted too, in case that changes.
pub fn parse(text: &str) -> Result<Vec<Entry>> {
    let entries: Vec<Entry> = match serde_json::from_str(text) {
        Ok(entries) => entries,
        Err(_) => json5::from_str(text).context("the weekly riven report isn't readable")?,
    };
    if entries.is_empty() {
        bail!("the weekly riven report is empty");
    }
    Ok(entries)
}

/// The file of a platform's week, relative to the data directory.
pub fn path(platform: &str, date: NaiveDate) -> PathBuf {
    Path::new(DIR)
        .join(platform)
        .join(format!("{date}.parquet"))
}

fn raw_path(platform: &str, date: NaiveDate) -> PathBuf {
    Path::new(DIR)
        .join(RAW_DIR)
        .join(platform)
        .join(format!("{date}.js.gz"))
}

/// The platform and date of a report file at `path` (relative, `/` between parts), as in
/// [`path`]; `None` for anything else.
pub fn parse_path(path: &str) -> Option<(&'static str, NaiveDate)> {
    let mut parts = path.split('/');
    let (dir, platform, name) = (parts.next()?, parts.next()?, parts.next()?);
    if dir != DIR || parts.next().is_some() {
        return None;
    }
    let platform = PLATFORMS.iter().find(|(p, _)| *p == platform)?.0;
    let date = name.strip_suffix(".parquet")?.parse().ok()?;
    Some((platform, date))
}

/// Every stored report file with its size, by its path relative to the data directory.
pub fn files(root: &Path) -> Result<BTreeMap<String, u64>> {
    let mut files = BTreeMap::new();
    for (platform, _) in PLATFORMS {
        let dir = root.join(DIR).join(platform);
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let path = format!("{DIR}/{platform}/{name}");
            if parse_path(&path).is_some() && entry.file_type()?.is_file() {
                files.insert(path, entry.metadata()?.len());
            }
        }
    }
    Ok(files)
}

/// The newest stored week of a platform.
pub fn latest(root: &Path, platform: &str) -> Result<Option<NaiveDate>> {
    Ok(files(root)?
        .keys()
        .filter_map(|path| parse_path(path))
        .filter(|(p, _)| *p == platform)
        .map(|(_, date)| date)
        .max())
}

/// Saves a platform's report for the week of `date`, with the response it came from.
pub fn save(
    root: &Path,
    platform: &str,
    date: NaiveDate,
    published_at: Option<DateTime<Utc>>,
    entries: &[Entry],
    raw: &[u8],
) -> Result<()> {
    use std::io::Write;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(raw)?;
    durable_write(&root.join(raw_path(platform, date)), &gz.finish()?)?;
    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| Row {
            date,
            platform: platform.to_owned(),
            published_at,
            entry: entry.clone(),
        })
        .collect();
    durable_write(&root.join(path(platform, date)), &to_parquet(&rows)?)
}

fn schema() -> Arc<Schema> {
    let float = |name| Field::new(name, DataType::Float64, false);
    Arc::new(Schema::new(vec![
        Field::new("date", DataType::Date32, false),
        Field::new("platform", DataType::Utf8, false),
        Field::new(
            "published_at",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            true,
        ),
        Field::new("item_type", DataType::Utf8, false),
        Field::new("compatibility", DataType::Utf8, true),
        Field::new("rerolled", DataType::Boolean, false),
        float("avg"),
        float("stddev"),
        float("min"),
        float("max"),
        float("median"),
        float("pop"),
    ]))
}

fn to_parquet(rows: &[Row]) -> Result<Vec<u8>> {
    let floats = |f: fn(&Entry) -> f64| -> ArrayRef {
        Arc::new(Float64Array::from_iter_values(
            rows.iter().map(|r| f(&r.entry)),
        ))
    };
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Date32Array::from_iter_values(rows.iter().map(|r| {
            i32::try_from((r.date - epoch).num_days()).unwrap_or(i32::MAX)
        }))),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|r| &r.platform),
        )),
        Arc::new(
            TimestampMillisecondArray::from(
                rows.iter()
                    .map(|r| r.published_at.map(|t| t.timestamp_millis()))
                    .collect::<Vec<_>>(),
            )
            .with_timezone("UTC"),
        ),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|r| &r.entry.item_type),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|r| r.entry.compatibility.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(BooleanArray::from(
            rows.iter().map(|r| r.entry.rerolled).collect::<Vec<_>>(),
        )),
        floats(|e| e.avg),
        floats(|e| e.stddev),
        floats(|e| e.min),
        floats(|e| e.max),
        floats(|e| e.median),
        floats(|e| e.pop),
    ];
    let batch = RecordBatch::try_new(schema(), columns)?;
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(PARQUET_ZSTD_LEVEL)?))
        .build();
    let mut bytes = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut bytes, batch.schema(), Some(props))?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(bytes)
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .and_then(|c| c.as_any().downcast_ref::<T>())
        .with_context(|| format!("column {name} is missing or has the wrong type"))
}

/// Reads one stored report file.
pub fn read(file: &Path) -> Result<Vec<Row>> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(file)?)?.build()?;
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let mut rows = Vec::new();
    for batch in reader {
        let batch = batch?;
        let date: &Date32Array = column(&batch, "date")?;
        let platform: &StringArray = column(&batch, "platform")?;
        let published: &TimestampMillisecondArray = column(&batch, "published_at")?;
        let item_type: &StringArray = column(&batch, "item_type")?;
        let compatibility: &StringArray = column(&batch, "compatibility")?;
        let rerolled: &BooleanArray = column(&batch, "rerolled")?;
        let float = |name| column::<Float64Array>(&batch, name);
        let (avg, stddev, min, max, median, pop) = (
            float("avg")?,
            float("stddev")?,
            float("min")?,
            float("max")?,
            float("median")?,
            float("pop")?,
        );
        for i in 0..batch.num_rows() {
            rows.push(Row {
                date: epoch + chrono::Days::new(u64::try_from(date.value(i)).unwrap_or(0)),
                platform: platform.value(i).to_owned(),
                published_at: (!published.is_null(i))
                    .then(|| DateTime::from_timestamp_millis(published.value(i)))
                    .flatten(),
                entry: Entry {
                    item_type: item_type.value(i).to_owned(),
                    compatibility: (!compatibility.is_null(i))
                        .then(|| compatibility.value(i).to_owned()),
                    rerolled: rerolled.value(i),
                    avg: avg.value(i),
                    stddev: stddev.value(i),
                    min: min.value(i),
                    max: max.value(i),
                    median: median.value(i),
                    pop: pop.value(i),
                },
            });
        }
    }
    Ok(rows)
}

/// The entries of a platform's newest stored week, to tell whether a fetched report is new.
pub fn latest_entries(root: &Path, platform: &str) -> Result<Option<(NaiveDate, Vec<Entry>)>> {
    let Some(date) = latest(root, platform)? else {
        return Ok(None);
    };
    let rows = read(&root.join(path(platform, date)))?;
    Ok(Some((date, rows.into_iter().map(|r| r.entry).collect())))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORT: &str = r"[
  {
    itemType: 'Rifle Riven Mod',
    compatibility: null,
    rerolled: false,
    avg: 91.88,
    stddev: 347.54,
    min: 3,
    max: 2300,
    pop: 5,
    median: 10
  },
  {
    itemType: 'Melee Riven Mod',
    compatibility: 'Ack & Brunt',
    rerolled: true,
    avg: 234.18,
    stddev: 377.69,
    min: 15,
    max: 2000,
    pop: 5,
    median: 72.5
  }
]
";

    #[test]
    fn reports_are_javascript_not_json() {
        let entries = parse(REPORT).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].compatibility, None);
        assert_eq!(entries[1].compatibility.as_deref(), Some("Ack & Brunt"));
        assert!(entries[1].rerolled);
        assert!((entries[1].median - 72.5).abs() < 1e-9);
        let json = r#"[{"itemType":"Zaw Riven Mod","compatibility":null,"rerolled":false,
            "avg":1,"stddev":0,"min":1,"max":1,"pop":1,"median":1}]"#;
        assert_eq!(parse(json).unwrap()[0].item_type, "Zaw Riven Mod");
        assert!(parse("[]").is_err());
        assert!(parse("<html>").is_err());
    }

    #[test]
    fn saved_weeks_read_back_and_are_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let entries = parse(REPORT).unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        let published = DateTime::from_timestamp_millis(1_794_877_203_000);
        save(root, "pc", date, published, &entries, REPORT.as_bytes()).unwrap();
        save(
            root,
            "pc",
            date.pred_opt().unwrap(),
            None,
            &entries[..1],
            b"x",
        )
        .unwrap();

        let files = files(root).unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            [
                "weekly_rivens/pc/2026-11-15.parquet",
                "weekly_rivens/pc/2026-11-16.parquet"
            ]
        );
        assert!(root.join("weekly_rivens/raw/pc/2026-11-16.js.gz").is_file());
        let (latest, stored) = latest_entries(root, "pc").unwrap().unwrap();
        assert_eq!((latest, &stored), (date, &entries));
        let rows = read(&root.join(path("pc", date))).unwrap();
        assert_eq!(rows[0].published_at, published);
        assert_eq!(rows[1].platform, "pc");
        assert_eq!(latest_entries(root, "ps4").unwrap(), None);
    }

    #[test]
    fn only_report_paths_are_accepted() {
        let date = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap();
        assert_eq!(
            parse_path("weekly_rivens/xb1/2026-11-16.parquet"),
            Some(("xb1", date))
        );
        for other in [
            "weekly_rivens/raw/pc/2026-11-16.js.gz",
            "weekly_rivens/mac/2026-11-16.parquet",
            "weekly_rivens/pc/soon.parquet",
            "weekly_rivens/pc/2026-11-16.parquet/x",
            "auctions/date=2026-11-16/compact-1.parquet",
            "../weekly_rivens/pc/2026-11-16.parquet",
        ] {
            assert_eq!(parse_path(other), None, "{other}");
        }
    }
}
