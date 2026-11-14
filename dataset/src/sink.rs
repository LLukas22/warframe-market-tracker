//! Writes rows to the dataset, one parquet part per table per flush, published atomically
//! together with the receipts of the raw archives they came from.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use arrow::array::RecordBatch;
use chrono::{DateTime, NaiveDate, Utc};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;

use crate::layout::{PARQUET_DIR, Table, receipt_dir};
use crate::schema::{
    AttributeRow, AuctionRow, GoneRow, attributes_batch, auctions_batch, gone_batch,
};
use crate::tx::{DatasetLock, PARQUET_ZSTD_LEVEL, Transaction, unique_id};

/// Buffers rows and writes them as one parquet file per table per flush,
/// partitioned by scrape date.
pub struct ParquetSink {
    dir: PathBuf,
    date: Option<NaiveDate>,
    pub auctions: Vec<AuctionRow>,
    pub attributes: Vec<AttributeRow>,
    pub gone: Vec<GoneRow>,
    raw_files: Vec<String>,
    pending_id: Option<String>,
    checkpoints: HashMap<PathBuf, Vec<u8>>,
}

impl ParquetSink {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            date: None,
            auctions: Vec::new(),
            attributes: Vec::new(),
            gone: Vec::new(),
            raw_files: Vec::new(),
            pending_id: None,
            checkpoints: HashMap::new(),
        }
    }

    /// Must be called before pushing rows for `scraped_at`, so files never straddle a date.
    pub fn prepare(&mut self, scraped_at: DateTime<Utc>) -> Result<()> {
        // Resolve a failed publication before allowing any more rows into its buffer.
        if self.pending_id.is_some() {
            self.flush()?;
        }
        let date = scraped_at.date_naive();
        if self.date.is_some_and(|d| d != date) {
            self.flush()?;
        }
        self.date = Some(date);
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        let Some(date) = self.date else {
            return Ok(());
        };
        if self.auctions.is_empty()
            && self.attributes.is_empty()
            && self.gone.is_empty()
            && self.raw_files.is_empty()
            && self.checkpoints.is_empty()
        {
            return Ok(());
        }
        let root = self
            .dir
            .parent()
            .context("parquet directory needs a parent")?;
        let _lock = DatasetLock::write(root)?;
        let stamp = self.pending_id.get_or_insert_with(unique_id).clone();
        let mut tables = Vec::new();
        // Auctions and attributes are always written as a pair (the web UI joins them).
        if !self.auctions.is_empty() || !self.attributes.is_empty() {
            tables.push((Table::Auctions, auctions_batch(&self.auctions)?));
            tables.push((Table::Attributes, attributes_batch(&self.attributes)?));
        }
        if !self.gone.is_empty() {
            tables.push((Table::Gone, gone_batch(&self.gone)?));
        }
        let receipt = receipt_dir(date).join(format!("{stamp}.json"));
        let destinations: Vec<_> = tables
            .iter()
            .map(|(table, _)| {
                Path::new(PARQUET_DIR)
                    .join(table.day_folder(date))
                    .join(format!("part-{stamp}.parquet"))
            })
            .collect();
        // A prior attempt may have failed after journaling. The lock recovered it, so don't
        // append the same buffered observations a second time.
        let published = root.join(&receipt).exists();
        if !published {
            let mut tx = Transaction::new(root)?;
            for ((_, batch), destination) in tables.into_iter().zip(destinations) {
                write_parquet(&tx.stage(destination), &batch)?;
            }
            for (path, bytes) in &self.checkpoints {
                fs::write(tx.stage(path.clone()), bytes)?;
            }
            // Also serves as an idempotent commit marker for --no-raw and empty fetches.
            fs::write(tx.stage(receipt), serde_json::to_vec(&self.raw_files)?)?;
            tx.commit()?;
        }
        tracing::info!(
            auctions = self.auctions.len(),
            attributes = self.attributes.len(),
            gone = self.gone.len(),
            %date,
            "wrote parquet"
        );
        self.auctions.clear();
        self.attributes.clear();
        self.gone.clear();
        self.raw_files.clear();
        self.checkpoints.clear();
        self.pending_id = None;
        Ok(())
    }

    pub fn checkpoint(&mut self, path: PathBuf, bytes: Vec<u8>) {
        self.checkpoints.insert(path, bytes);
    }

    /// Call only after the raw archive was saved and processing succeeded. The receipt is
    /// published in the same transaction as all three normalized tables, including empty fetches.
    pub fn acknowledge_raw(&mut self, file_name: String) {
        self.raw_files.push(file_name);
    }
}

fn write_parquet(path: &Path, batch: &RecordBatch) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    let props = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(PARQUET_ZSTD_LEVEL)?))
        .build();
    let mut writer = ArrowWriter::try_new(File::create(&tmp)?, batch.schema(), Some(props))?;
    writer.write(batch)?;
    writer.close()?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}
