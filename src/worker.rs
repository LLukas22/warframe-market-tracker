//! The scraper running inside this server, the only way it runs. Data transfers stop it and
//! start it again. It's off unless `WFMT_ENABLE_SCRAPER=true`; otherwise the server only hosts
//! its data.

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, watch};
use wfmt_scraper::engine::{self, Lock};

use crate::config::ScraperSettings;

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

struct Running {
    shutdown: watch::Sender<bool>,
    thread: std::thread::JoinHandle<()>,
}

/// Starts and stops the scraper.
pub struct Scraper {
    settings: ScraperSettings,
    running: Mutex<Option<Running>>,
}

impl Scraper {
    pub fn new(settings: ScraperSettings) -> Self {
        Self {
            settings,
            running: Mutex::new(None),
        }
    }

    /// The server scrapes in the background.
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    pub fn settings(&self) -> &ScraperSettings {
        &self.settings
    }

    /// Starts the scraper on `dir` if it's enabled, unless another server already runs one
    /// there.
    pub async fn start(&self, dir: &Path) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let mut running = self.running.lock().await;
        if running.as_ref().is_some_and(|r| !r.thread.is_finished()) {
            return Ok(());
        }
        let Some(lock) = lock(dir).await? else {
            return Ok(());
        };
        let (settings, dir) = (self.settings.clone(), dir.to_owned());
        let engine = tokio::task::spawn_blocking(move || settings.open(dir)).await??;
        let (shutdown, receiver) = watch::channel(false);
        // The engine writes to disk between requests, at times for long, so it gets a thread
        // and runtime of its own rather than stalling one of the web server's.
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

    /// Stops the scraper and waits until it has saved its progress and released the engine
    /// lock. A running sweep continues when the scraper starts again.
    pub async fn stop(&self) {
        if let Some(running) = self.running.lock().await.take() {
            let _ = running.shutdown.send(true);
            let _ = tokio::task::spawn_blocking(move || running.thread.join()).await;
        }
    }
}
