//! `/img/weapon/{slug}`: weapon thumbnails, fetched from warframe.market's static CDN once and
//! then served from `<data>/cache/images` forever.

use std::collections::HashSet;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use dioxus::fullstack::axum::extract::Path;
use dioxus::fullstack::axum::http::{StatusCode, header};
use dioxus::fullstack::axum::response::{IntoResponse, Response};
use tokio::sync::Mutex as AsyncMutex;

const ASSETS: &str = "https://warframe.market/static/assets/";
/// Minimum gap between two downloads, to stay polite even when a page asks for 100 images.
const DOWNLOAD_GAP: Duration = Duration::from_millis(250);

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent(wfmt_types::USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()
        .expect("http client")
});
/// Serializes downloads (and remembers when the last one happened).
static DOWNLOADS: LazyLock<AsyncMutex<Option<Instant>>> = LazyLock::new(|| AsyncMutex::new(None));
/// Slugs whose download failed; not retried until restart.
static FAILED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);

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
    let dir = crate::data::data_dir().join("cache/images");
    let path = dir.join(format!("{slug}.png"));
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return png(bytes);
    }
    if FAILED.lock().unwrap().contains(slug) {
        return StatusCode::NOT_FOUND.into_response();
    }

    let mut last = DOWNLOADS.lock().await;
    // Another request may have downloaded it while we waited for the lock.
    if let Ok(bytes) = tokio::fs::read(&path).await {
        return png(bytes);
    }
    let Some(thumb) = crate::data::weapon_thumb(slug.to_owned()).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if let Some(t) = *last {
        tokio::time::sleep_until((t + DOWNLOAD_GAP).into()).await;
    }
    *last = Some(Instant::now());

    let result = async {
        let response = CLIENT
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
                tokio::fs::create_dir_all(&dir).await?;
                tokio::fs::write(&tmp, &bytes).await?;
                tokio::fs::rename(&tmp, &path).await
            }
            .await;
            if let Err(e) = saved {
                eprintln!("failed to cache image {slug}: {e}");
            }
            png(bytes)
        }
        Err(e) => {
            eprintln!("failed to download image {slug}: {e}");
            FAILED.lock().unwrap().insert(slug.to_owned());
            StatusCode::NOT_FOUND.into_response()
        }
    }
}
