//! `/img/weapon/{slug}`: weapon thumbnails, fetched from warframe.market's static CDN once and
//! then served from `<data>/cache/images` forever.

use std::collections::HashSet;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use dioxus::fullstack::axum::extract::Path;
use dioxus::fullstack::axum::http::{StatusCode, header};
use dioxus::fullstack::axum::response::{IntoResponse, Response};
use tokio::sync::Mutex as AsyncMutex;

const ASSETS: &str = "https://warframe.market/static/assets/";
/// Minimum gap between two downloads, to stay polite even when a page asks for 100 images.
const DOWNLOAD_GAP: Duration = Duration::from_millis(250);

/// The thumbnail cache and its downloads.
pub struct Images {
    dir: PathBuf,
    client: reqwest::Client,
    /// Serializes downloads (and remembers when the last one happened).
    downloads: AsyncMutex<Option<Instant>>,
    /// Slugs whose download failed; not retried until restart.
    failed: Mutex<HashSet<String>>,
}

impl Images {
    pub fn new(data_dir: &FsPath) -> anyhow::Result<Self> {
        Ok(Self {
            dir: data_dir.join("cache/images"),
            client: reqwest::Client::builder()
                .user_agent(wfmt_types::USER_AGENT)
                .timeout(Duration::from_secs(20))
                .build()?,
            downloads: AsyncMutex::new(None),
            failed: Mutex::new(HashSet::new()),
        })
    }

    fn failed(&self) -> std::sync::MutexGuard<'_, HashSet<String>> {
        self.failed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn png(bytes: Vec<u8>) -> Response {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=604800, immutable"),
        ],
        bytes,
    )
        .into_response()
}

pub async fn weapon(Path(slug): Path<String>) -> Response {
    let slug = slug.trim_end_matches(".png");
    if slug.is_empty() || !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let app = crate::app::get();
    let images = &app.images;
    let dir = &images.dir;
    let path = dir.join(format!("{slug}.png"));
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return png(bytes);
    }
    if images.failed().contains(slug) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut last = images.downloads.lock().await;
    // Another request may have downloaded it while we waited for the lock.
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return png(bytes);
    }
    let wanted = slug.to_owned();
    let Some(thumb) = app
        .query(move |data| data.thumb(&wanted))
        .await
        .ok()
        .flatten()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(t) = *last {
        tokio::time::sleep_until((t + DOWNLOAD_GAP).into()).await;
    }
    *last = Some(Instant::now());

    let result = async {
        let response = images
            .client
            .get(format!("{ASSETS}{thumb}"))
            .send()
            .await?
            .error_for_status()?;
        response.bytes().await
    }
    .await;
    match result {
        Ok(bytes) => {
            let bytes = bytes.to_vec();
            let tmp = path.with_extension("tmp");
            let saved = async {
                tokio::fs::create_dir_all(dir).await?;
                tokio::fs::write(&tmp, &bytes).await?;
                tokio::fs::rename(&tmp, &path).await
            }
            .await;
            if let Err(e) = saved {
                tracing::warn!("failed to cache image {slug}: {e}");
            }
            png(bytes)
        }
        Err(e) => {
            tracing::warn!("failed to download image {slug}: {e}");
            images.failed().insert(slug.to_owned());
            StatusCode::NOT_FOUND.into_response()
        }
    }
}
