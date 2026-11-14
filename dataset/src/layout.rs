//! Where the dataset keeps its files, relative to the data directory:
//!
//! ```text
//! parquet/<table>/date=<YYYY-MM-DD>/<part>.parquet   the tables, see `schema`
//! raw/<YYYY-MM-DD>/<source>-<ms>.json.gz             raw responses, for reprocessing
//! raw/<YYYY-MM-DD>/.committed/*.json                 receipts, published with the parquet
//! rivens.jsonl                                       the riven registry, see `identity`
//! storage/                                           transactions and maintenance state
//! ```

use std::fmt::{self, Display};
use std::path::PathBuf;
use std::str::FromStr;

use chrono::NaiveDate;

pub const PARQUET_DIR: &str = "parquet";
/// The riven registry, an append-only journal: see `identity`.
pub const REGISTRY: &str = "rivens.jsonl";
/// Starts the name of a table's folder for one UTC day.
pub const DATE_PREFIX: &str = "date=";
pub const RAW_DIR: &str = "raw";
/// Inside a day of raw archives: the receipts of the archives whose rows were published.
pub const RECEIPTS_DIR: &str = ".committed";

/// The receipts of one day of raw archives, relative to the data directory.
pub fn receipt_dir(date: NaiveDate) -> PathBuf {
    PathBuf::from(RAW_DIR)
        .join(date.to_string())
        .join(RECEIPTS_DIR)
}

/// One of the dataset's tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Table {
    /// Every sighting of a listing.
    Auctions,
    /// The stats of those sightings, one row each.
    Attributes,
    /// Listings that disappeared.
    Gone,
}

impl Table {
    pub const ALL: &'static [Self] = &[Self::Auctions, Self::Attributes, Self::Gone];

    /// Tables written together, one part each per write: a listing's sighting and its stats
    /// share a part name.
    pub const GROUPS: [&'static [Self]; 2] = [&[Self::Auctions, Self::Attributes], &[Self::Gone]];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auctions => "auctions",
            Self::Attributes => "attributes",
            Self::Gone => "gone",
        }
    }

    /// The folder of one day, relative to [`PARQUET_DIR`], with `/` between its parts.
    pub fn day_folder(self, date: impl Display) -> String {
        format!("{self}/{DATE_PREFIX}{date}")
    }
}

impl Display for Table {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Table {
    type Err = UnknownTable;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .iter()
            .copied()
            .find(|t| t.as_str() == s)
            .ok_or(UnknownTable)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnknownTable;

impl Display for UnknownTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not one of the dataset's tables")
    }
}

impl std::error::Error for UnknownTable {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_name_table_and_date() {
        assert_eq!(Table::Gone.day_folder("2026-11-08"), "gone/date=2026-11-08");
        assert_eq!("attributes".parse::<Table>(), Ok(Table::Attributes));
        let date = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
        assert_eq!(
            receipt_dir(date),
            PathBuf::from("raw/2026-11-08/.committed")
        );
    }
}
