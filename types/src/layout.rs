//! Where the dataset keeps its tables, relative to the data directory:
//! `parquet/<table>/date=<YYYY-MM-DD>/<part>.parquet`.

use std::fmt::Display;

pub const PARQUET_DIR: &str = "parquet";
/// Starts the name of a table's folder for one UTC day.
pub const DATE_PREFIX: &str = "date=";

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

names!(Table {
    Auctions => "auctions",
    Attributes => "attributes",
    Gone => "gone",
});

impl Table {
    /// Tables written together, one part each per write: a listing's sighting and its stats
    /// share a part name.
    pub const GROUPS: [&'static [Self]; 2] = [&[Self::Auctions, Self::Attributes], &[Self::Gone]];

    /// The folder of one day, relative to [`PARQUET_DIR`], with `/` between its parts.
    pub fn day_folder(self, date: impl Display) -> String {
        format!("{self}/{DATE_PREFIX}{date}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_folders_name_table_and_date() {
        assert_eq!(Table::Gone.day_folder("2026-11-08"), "gone/date=2026-11-08");
        assert_eq!("attributes".parse::<Table>(), Ok(Table::Attributes));
    }
}
