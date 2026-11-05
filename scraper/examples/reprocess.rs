//! Rebuilds a stopped server's dataset from its raw archive, without touching the API:
//! `cargo run -p wfmt-scraper --release --example reprocess -- --replace`

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "WFMT_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,
    #[arg(
        long,
        env = "WFMT_REWORK_CUTOFF",
        default_value = "2026-10-22T00:00:00Z"
    )]
    rework_cutoff: DateTime<Utc>,
    /// Output directory, must not exist yet (defaults to <data-dir>/reprocessed).
    #[arg(long)]
    out: Option<PathBuf>,
    /// Swap the result into the data directory (old files move to <data-dir>/backup-*).
    #[arg(long)]
    replace: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();
    wfmt_scraper::reprocess::reprocess(&args.data_dir, args.rework_cutoff, args.out, args.replace)
}
