//! The scraper running inside this server, the only way it runs. Data transfers stop it and
//! start it again. It's off unless `WFMT_ENABLE_SCRAPER=true`; otherwise the server only hosts
//! its data.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{Mutex, watch};
use wfmt_scraper::engine::{self, Engine, EngineArgs, Lock};

struct Running {
    shutdown: watch::Sender<bool>,
    thread: std::thread::JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::const_new(None);

/// The server scrapes in the background. Without it, it only hosts its data: no scraping, and
/// no maintenance either.
pub fn enabled() -> Result<bool> {
    match std::env::var("WFMT_ENABLE_SCRAPER") {
        Ok(value) => parse_switch(&value),
        Err(_) => Ok(false),
    }
}

fn parse_switch(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "no" | "off" => Ok(false),
        "1" | "true" | "yes" | "on" => Ok(true),
        _ => bail!("WFMT_ENABLE_SCRAPER should be true or false, not {value:?}"),
    }
}

/// Taking the lock is retried for about a second before giving up.
const LOCK_ATTEMPTS: u32 = 10;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);

/// Takes the engine lock, or `None` if another server holds it. The status endpoints hold the
/// lock for an instant on every poll, so one failed attempt doesn't mean anything.
pub async fn lock(dir: &Path) -> Result<Option<Lock>> {
    for _ in 0..LOCK_ATTEMPTS {
        if let Some(lock) = engine::try_lock(dir)? {
            return Ok(Some(lock));
        }
        tokio::time::sleep(LOCK_RETRY_DELAY).await;
    }
    Ok(None)
}

/// Loads an engine on `dir`, replaying anything an interrupted run left behind.
pub fn open(dir: PathBuf) -> Result<Engine> {
    let cutoff = std::env::var("WFMT_REWORK_CUTOFF")
        .unwrap_or_else(|_| "2026-10-22T00:00:00Z".into())
        .parse()?;
    Engine::new(dir, cutoff, EngineArgs::from_env())
}

/// Starts the scraper if it's enabled, unless another server already runs one on this data
/// directory.
pub async fn start() -> Result<()> {
    if !enabled()? {
        return Ok(());
    }
    let mut running = RUNNING.lock().await;
    if running.as_ref().is_some_and(|r| !r.thread.is_finished()) {
        return Ok(());
    }
    let dir = crate::data::data_dir();
    let Some(lock) = lock(&dir).await? else {
        return Ok(());
    };
    let engine = tokio::task::spawn_blocking(move || open(dir)).await??;
    let (shutdown, receiver) = watch::channel(false);
    // The engine writes to disk between requests, at times for long, so it gets a thread and
    // runtime of its own rather than stalling one of the web server's.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let thread = std::thread::Builder::new()
        .name("scraper".into())
        .spawn(move || {
            let _lock = lock;
            if let Err(error) = runtime.block_on(engine.run(receiver)) {
                tracing::error!("scraper stopped: {error:#}");
            }
        })?;
    *running = Some(Running { shutdown, thread });
    Ok(())
}

/// Stops the scraper and waits until it has saved its progress and released the engine lock.
/// A running sweep continues when the scraper starts again.
pub async fn stop() {
    if let Some(running) = RUNNING.lock().await.take() {
        let _ = running.shutdown.send(true);
        let _ = tokio::task::spawn_blocking(move || running.thread.join()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_accepts_common_spellings() {
        for off in ["", " ", "0", "false", "No", "OFF"] {
            assert!(!parse_switch(off).unwrap());
        }
        for on in ["1", "true", "TRUE", "yes", " on "] {
            assert!(parse_switch(on).unwrap());
        }
        assert!(parse_switch("maybe").is_err());
    }
}
