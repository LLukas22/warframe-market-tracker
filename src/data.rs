//! The app's one data store (see `wfmt_store`), on the data directory.

use std::path::PathBuf;
use std::sync::LazyLock;

use dioxus::prelude::ServerFnError;
use wfmt_store::{Data, Store};

static STORE: LazyLock<Store> = LazyLock::new(|| Store::new(data_dir()));

/// The data directory (`WFMT_DATA_DIR`, else `./data`).
pub fn data_dir() -> PathBuf {
    std::env::var_os("WFMT_DATA_DIR").map_or_else(|| PathBuf::from("data"), PathBuf::from)
}

/// Loads the data from scratch on the next request, after an import replaced it.
pub fn reload() {
    STORE.reload();
}

/// Thumbnail path (relative to warframe.market's static assets) for a weapon.
pub async fn weapon_thumb(slug: String) -> Option<String> {
    run(move |data| data.thumb(&slug)).await.ok().flatten()
}

/// Loads the data before the first request arrives, so the first visitor doesn't wait for it.
pub fn warm() {
    STORE.refresh();
}

/// Runs `f` on the loaded data only if that takes no waiting: no refresh, no lock held by a
/// reload. For server rendering, which must never hold a page back; the browser fetches
/// whatever this couldn't answer.
pub fn try_now<T>(f: impl FnOnce(&Data) -> T) -> Option<T> {
    STORE.try_read().map(|data| f(&data))
}

/// Refreshes the store if needed and runs `f` on a blocking thread.
pub async fn run<T, F>(f: F) -> Result<T, ServerFnError>
where
    T: Send + 'static,
    F: FnOnce(&Data) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        STORE.refresh();
        f(&STORE.read())
    })
    .await
    .map_err(ServerFnError::new)
}
