//! The running server's parts, set up once in `main` from its `Config`. Server functions and
//! routes reach them through [`get`].

use std::path::PathBuf;
use std::sync::OnceLock;

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
}
