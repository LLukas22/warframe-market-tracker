//! Everything the server is told through environment variables, read and checked once at
//! start-up: a bad value stops the server with a message instead of showing up later.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use wfmt_scraper::engine::{Engine, EngineArgs};

use crate::hub;

pub struct Config {
    /// `WFMT_DATA_DIR`, else `./data`.
    pub data_dir: PathBuf,
    /// `WFMT_ADMIN_TOKEN`. Without it, nothing that changes what the server does or holds is
    /// allowed: no scrapes started from the page, no exports, imports or uploads.
    pub admin_token: Option<String>,
    pub scraper: ScraperSettings,
    /// `WFMT_HF_TOKEN`, `WFMT_HF_DATASET` and `WFMT_HF_PULL_DATASET`.
    pub hub: hub::Mode,
    /// Where the screenshot scanner's files are, see `ocr_assets`.
    pub ocr_dir: PathBuf,
}

/// How the scraper runs. Its engine settings are read even when it's off: imports are checked by
/// opening them the way the scraper would.
#[derive(Clone)]
pub struct ScraperSettings {
    /// `WFMT_ENABLE_SCRAPER=true`. Otherwise the server only hosts its data: no scraping, and
    /// no maintenance either.
    pub enabled: bool,
    /// `WFMT_REWORK_CUTOFF`: listings created before it may still show pre-rework stats.
    pub rework_cutoff: DateTime<Utc>,
    /// The `WFMT_*` settings of `EngineArgs`.
    pub engine: EngineArgs,
}

const DEFAULT_REWORK_CUTOFF: &str = "2026-10-22T00:00:00Z";

impl ScraperSettings {
    /// The defaults, with the scraper off.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            enabled: false,
            rework_cutoff: DEFAULT_REWORK_CUTOFF.parse().unwrap(),
            engine: EngineArgs::from_env().unwrap(),
        }
    }

    /// Loads an engine on `dir`, replaying anything an interrupted run left behind.
    pub fn open(&self, dir: PathBuf) -> Result<Engine> {
        Engine::new(dir, self.rework_cutoff, self.engine.clone())
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Reads the settings through `var`, which gives a variable's value if it's set.
    fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Result<Self> {
        // The token is taken as it is; blank counts as unset, like a commented-out line.
        let admin_token = var("WFMT_ADMIN_TOKEN").filter(|t| !t.trim().is_empty());
        // Everything else is trimmed, and blank counts as unset too.
        let var = |name: &str| {
            var(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let enabled = match var("WFMT_ENABLE_SCRAPER") {
            Some(value) => parse_switch(&value).context("WFMT_ENABLE_SCRAPER")?,
            None => false,
        };
        let rework_cutoff = var("WFMT_REWORK_CUTOFF")
            .as_deref()
            .unwrap_or(DEFAULT_REWORK_CUTOFF)
            .parse()
            .context("WFMT_REWORK_CUTOFF should be a time like 2026-10-22T00:00:00Z")?;
        let hub = hub::Mode::from_settings(
            var("WFMT_HF_TOKEN"),
            var("WFMT_HF_DATASET"),
            var("WFMT_HF_PULL_DATASET"),
            enabled,
        )?;
        Ok(Self {
            data_dir: var("WFMT_DATA_DIR").map_or_else(|| PathBuf::from("data"), PathBuf::from),
            admin_token,
            scraper: ScraperSettings {
                enabled,
                rework_cutoff,
                engine: EngineArgs::from_env()?,
            },
            hub,
            ocr_dir: var("WFMT_OCR_DIR").map_or_else(default_ocr_dir, PathBuf::from),
        })
    }
}

/// `ocr/` next to the server binary (the Docker image), else `ocr/dist` in the working directory
/// (development).
fn default_ocr_dir() -> PathBuf {
    let beside_binary = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ocr")));
    match beside_binary {
        Some(dir) if dir.is_dir() => dir,
        _ => PathBuf::from("ocr/dist"),
    }
}

fn parse_switch(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => bail!("should be true or false, not {value:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|name| vars.get(name).cloned())
    }

    #[test]
    fn switch_accepts_common_spellings() {
        for off in ["0", "false", "No", "OFF"] {
            assert!(!parse_switch(off).unwrap());
        }
        for on in ["1", "true", "TRUE", "yes", "on"] {
            assert!(parse_switch(on).unwrap());
        }
        assert!(parse_switch("maybe").is_err());
    }

    #[test]
    fn defaults_need_no_settings() {
        let config = config(&[]).unwrap();
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert!(config.admin_token.is_none());
        assert!(!config.scraper.enabled);
        assert!(matches!(config.hub, hub::Mode::Off));
    }

    #[test]
    fn blank_values_count_as_unset_and_bad_ones_stop_the_start() {
        let config = config(&[("WFMT_ADMIN_TOKEN", "  "), ("WFMT_ENABLE_SCRAPER", " ")]).unwrap();
        assert!(config.admin_token.is_none());
        assert!(!config.scraper.enabled);
        for bad in [
            ("WFMT_ENABLE_SCRAPER", "maybe"),
            ("WFMT_REWORK_CUTOFF", "yesterday"),
            ("WFMT_HF_DATASET", "me/rivens"),
        ] {
            let error = super::Config::from_lookup(|name| (name == bad.0).then(|| bad.1.into()));
            assert!(error.is_err(), "{bad:?} was accepted");
        }
    }
}
