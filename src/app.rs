//! The running server's parts, set up once in `main` from its `Config`. Server functions and
//! routes reach them through [`get`].

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, anyhow};
use dioxus::prelude::ServerFnError;
use wfmt_store::{Data, Store};

use crate::admin::Admin;
use crate::config::Config;
use crate::hub::Hub;
use crate::images::Images;
use crate::transfer::Transfers;
use crate::worker::Scraper;

pub struct App {
    pub data_dir: PathBuf,
    /// The data, as the pages show it.
    pub store: Store,
    pub admin: Admin,
    pub scraper: Scraper,
    pub hub: Hub,
    pub images: Images,
    pub transfers: Transfers,
    /// The screenshot scanner's files.
    pub ocr_dir: PathBuf,
    /// [`App::stop_on_signal`] is listening.
    listening: AtomicBool,
}

static APP: OnceLock<App> = OnceLock::new();

/// Sets the app up. Only once: everything after reaches it through [`get`].
pub fn init(config: Config) -> Result<&'static App> {
    let app = App {
        store: Store::new(config.data_dir.clone()),
        admin: Admin::new(config.admin_token),
        scraper: Scraper::new(config.scraper),
        hub: Hub::new(config.hub),
        images: Images::new(&config.data_dir)?,
        transfers: Transfers::default(),
        ocr_dir: config.ocr_dir,
        data_dir: config.data_dir,
        listening: AtomicBool::new(false),
    };
    APP.set(app)
        .map_err(|_| anyhow!("the app is already set up"))?;
    Ok(get())
}

pub fn get() -> &'static App {
    APP.get()
        .expect("main sets the app up before anything is served")
}

impl App {
    /// Refreshes the data if needed and runs `f` on it, on a blocking thread.
    pub async fn query<T, F>(&'static self, f: F) -> Result<T, ServerFnError>
    where
        T: Send + 'static,
        F: FnOnce(&Data) -> T + Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            self.store.refresh();
            f(&self.store.read())
        })
        .await
        .map_err(ServerFnError::new)
    }

    /// Runs `f` on the loaded data only if that takes no waiting: no refresh, no lock held by a
    /// reload. For server rendering, which must never hold a page back; the browser fetches
    /// whatever this couldn't answer.
    pub fn try_now<T>(&self, f: impl FnOnce(&Data) -> T) -> Option<T> {
        self.store.try_read().map(|data| f(&data))
    }

    /// When the server is asked to stop (SIGTERM, as from `docker stop`, or Ctrl-C), stops the
    /// scraper so it saves its progress and publishes the rows it holds, then exits. Asked
    /// again, it exits right away. Hub transfers and imports are left to their own recovery:
    /// both commit atomically, so stopping midway costs only the work in progress.
    pub fn stop_on_signal(&'static self) {
        if self.listening.swap(true, Ordering::SeqCst) {
            return;
        }
        tokio::spawn(async move {
            stop_requested().await;
            tracing::info!("stopping the server");
            tokio::select! {
                () = self.scraper.stop() => tracing::info!("scraper stopped, exiting"),
                () = stop_requested() => tracing::warn!("asked again, exiting right away"),
            }
            std::process::exit(0);
        });
    }
}

/// Waits for SIGTERM or Ctrl-C.
async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
                return;
            }
            Err(error) => tracing::warn!("can't listen for SIGTERM: {error}"),
        }
    }
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!("can't listen for Ctrl-C: {error}");
        std::future::pending::<()>().await;
    }
}
