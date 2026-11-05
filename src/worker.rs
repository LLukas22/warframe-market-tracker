//! The scraper running inside this server, the only way it runs. Data transfers stop it and
//! start it again.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use wfmt_scraper::engine::{self, Engine, EngineArgs, Lock};

struct Running {
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::const_new(None);

/// Takes the engine lock, or `None` if another server holds it. The status endpoints hold the
/// lock for an instant on every poll, so one failed attempt doesn't mean anything.
pub async fn lock(dir: &Path) -> Result<Option<Lock>> {
    for _ in 0..10 {
        if let Some(lock) = engine::try_lock(dir)? {
            return Ok(Some(lock));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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

/// Starts the scraper unless another server already runs one on this data directory.
pub async fn start() -> Result<()> {
    let mut running = RUNNING.lock().await;
    if running.as_ref().is_some_and(|r| !r.task.is_finished()) {
        return Ok(());
    }
    let dir = crate::data::data_dir();
    let Some(lock) = lock(&dir).await? else {
        return Ok(());
    };
    let engine = tokio::task::spawn_blocking(move || open(dir)).await??;
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(async move {
        let _lock = lock;
        if let Err(error) = engine.run(receiver).await {
            eprintln!("scraper stopped: {error:#}");
        }
    });
    *running = Some(Running { shutdown, task });
    Ok(())
}

/// Stops the scraper and waits until it has saved its progress and released the engine lock.
/// A running sweep continues when the scraper starts again.
pub async fn stop() {
    if let Some(running) = RUNNING.lock().await.take() {
        let _ = running.shutdown.send(true);
        let _ = running.task.await;
    }
}
