//! The dataset on disk, shared by the scraper, which writes it, and the server's store, which
//! reads it: where its files live, the tables' rows and columns, writing them in transactions,
//! keeping them compact, and the registry that gives every riven a stable id.

pub mod identity;
pub mod layout;
pub mod maintenance;
pub mod schema;
pub mod sink;
pub mod tx;

pub use tx::{DatasetLock, PARQUET_ZSTD_LEVEL, Transaction, durable_write, sync_dir, unique_id};
