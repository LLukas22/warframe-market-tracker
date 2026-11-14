//! The scraper running inside this server, the only way it runs. Data transfers and shutdown
//! stop it; transfers start it again. It's off unless `WFMT_ENABLE_SCRAPER=true`; otherwise the
//! server only hosts its data.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use anyhow::{Result, bail};
use wfmt_scraper::engine::{self, Handle, Lock};
use wfmt_types::{Command, ScraperStatus};

use crate::config::ScraperSettings;

/// Takes the engine lock, or `None` if another server holds it.
pub fn lock(dir: &Path) -> Result<Option<Lock>> {
    engine::try_lock(dir)
}

/// Starts and stops the scraper, and passes the scraper page's commands to it.
pub struct Scraper {
    settings: ScraperSettings,
    /// The thread of the running engine. Also makes starting and stopping take turns.
    thread: tokio::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The latest engine's handle, kept after it stops so its last status stays visible.
    handle: Mutex<Option<Handle>>,
}

impl Scraper {
    pub fn new(settings: ScraperSettings) -> Self {
        Self {
            settings,
            thread: tokio::sync::Mutex::new(None),
            handle: Mutex::new(None),
        }
    }

    /// The server scrapes in the background.
    pub fn enabled(&self) -> bool {
        self.settings.enabled
    }

    pub fn settings(&self) -> &ScraperSettings {
        &self.settings
    }

    fn handle(&self) -> std::sync::MutexGuard<'_, Option<Handle>> {
        self.handle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The engine runs right now.
    pub fn is_running(&self) -> bool {
        self.handle().as_ref().is_some_and(Handle::is_running)
    }

    /// What the engine last reported, also while it's stopped for a transfer.
    pub fn status(&self) -> Option<ScraperStatus> {
        self.handle().as_ref().and_then(Handle::status)
    }

    /// Passes a command from the scraper page to the engine.
    pub fn send(&self, command: Command) -> Result<()> {
        match self.handle().as_ref() {
            Some(handle) if handle.is_running() => handle.send(command),
            _ => bail!("The scraper isn't running right now."),
        }
    }

    /// Starts the scraper on `dir` if it's enabled, unless another server already runs one
    /// there.
    pub async fn start(&self, dir: &Path) -> Result<()> {
        if !self.enabled() {
            return Ok(());
        }
        let mut thread = self.thread.lock().await;
        if thread.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(());
        }
        let Some(lock) = lock(dir)? else {
            tracing::warn!("another server runs the scraper on this data directory");
            return Ok(());
        };
        let (settings, dir) = (self.settings.clone(), dir.to_owned());
        let engine = tokio::task::spawn_blocking(move || settings.open(dir)).await??;
        let (handle, steering) = engine::handle();
        // The engine writes to disk between requests, at times for long, so it gets a thread
        // and runtime of its own rather than stalling one of the web server's.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        *thread = Some(
            std::thread::Builder::new()
                .name("scraper".into())
                .spawn(move || {
                    let _lock = lock;
                    if let Err(error) = runtime.block_on(engine.run(steering)) {
                        tracing::error!("scraper stopped: {error:#}");
                    }
                })?,
        );
        *self.handle() = Some(handle);
        Ok(())
    }

    /// Stops the scraper and waits until it has saved its progress and released the engine
    /// lock. A running sweep continues when the scraper starts again.
    pub async fn stop(&self) {
        let mut thread = self.thread.lock().await;
        if let Some(handle) = self.handle().as_ref() {
            handle.stop();
        }
        if let Some(thread) = thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
