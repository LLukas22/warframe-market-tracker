//! The server functions behind the browser's requests, and the answers embedded in the first
//! page. The types they exchange are in `wfmt_api`.

use dioxus::prelude::*;

pub use wfmt_api::*;

#[get("/api/overview")]
pub async fn overview() -> Result<Overview, ServerFnError> {
    crate::data::run(|store| store.overview_memo()).await
}

#[post("/api/search")]
pub async fn search(query: Query) -> Result<SearchResult, ServerFnError> {
    query.period.validate().map_err(ServerFnError::new)?;
    crate::data::run(move |store| store.search_memo(&query)).await
}

/// Fetch another bounded page without recomputing the price chart.
#[post("/api/listing-page")]
pub async fn listing_page(
    query: Query,
    offset: usize,
    gone: bool,
) -> Result<SearchResult, ServerFnError> {
    query.period.validate().map_err(ServerFnError::new)?;
    crate::data::run(move |store| store.search_page(&query, offset, Some(gone))).await
}

#[post("/api/riven-detail")]
pub async fn riven_detail(
    id: String,
    auction_id: Option<String>,
    comparisons: Comparisons,
) -> Result<Option<RivenDetail>, ServerFnError> {
    crate::data::run(move |store| store.riven_detail(&id, auction_id.as_deref(), comparisons)).await
}

/// The riven a scan was matched to, if it has been listed before.
#[post("/api/scan/match")]
pub async fn scan_match(scan: RivenScan) -> Result<Option<String>, ServerFnError> {
    let registry = crate::data::data_dir().join(wfmt_dataset::layout::REGISTRY);
    crate::data::run(move |store| store.scan_match(&scan, &registry)).await
}

/// The riven page for a scanned riven that isn't in the data: the scan graded by the stat
/// model, plus similar rivens. `None` for an unknown weapon.
#[post("/api/scan/detail")]
pub async fn scanned_detail(
    scan: RivenScan,
    comparisons: Comparisons,
) -> Result<Option<RivenDetail>, ServerFnError> {
    crate::data::run(move |store| store.scanned_detail(&scan, comparisons)).await
}

#[post("/api/stat-effects")]
pub async fn stat_effects(weapon: String) -> Result<StatEffects, ServerFnError> {
    crate::data::run(move |store| store.stat_effects(&weapon)).await
}

#[get("/api/riven/{id}")]
pub async fn riven_history(id: String) -> Result<Vec<RivenAuction>, ServerFnError> {
    crate::data::run(move |store| store.riven_history(&id)).await
}

#[get("/api/live")]
pub async fn live_stats() -> Result<LiveStats, ServerFnError> {
    let (auctions, version) = crate::data::run(|store| store.live()).await?;
    let dir = crate::data::data_dir();
    let running = wfmt_scraper::engine::try_lock(&dir)
        .map_err(ServerFnError::new)?
        .is_none();
    let scraping =
        running && wfmt_scraper::engine::read_status(&dir).is_some_and(|s| s.sweep.is_some());
    Ok(LiveStats {
        auctions,
        version,
        scraping,
    })
}

#[get("/api/scraper")]
pub async fn scraper_status() -> Result<ScraperSnapshot, ServerFnError> {
    let dir = crate::data::data_dir();
    let running = wfmt_scraper::engine::try_lock(&dir)
        .map_err(ServerFnError::new)?
        .is_none();
    Ok(ScraperSnapshot {
        status: wfmt_scraper::engine::read_status(&dir),
        running,
        admin: crate::admin::enabled(),
        disabled: !crate::worker::enabled().unwrap_or(false),
        hub: crate::hub::status(),
        server_time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64,
    })
}

#[post("/api/scraper/control")]
pub async fn scraper_control(cancel: bool, token: String) -> Result<(), ServerFnError> {
    crate::admin::check(&token).await?;
    if !crate::worker::enabled().unwrap_or(false) {
        return Err(ServerFnError::new(
            "Scraping is turned off on this server. Set WFMT_ENABLE_SCRAPER=true to turn it on.",
        ));
    }
    let dir = crate::data::data_dir();
    if wfmt_scraper::engine::try_lock(&dir)
        .map_err(ServerFnError::new)?
        .is_some()
    {
        return Err(ServerFnError::new("The scraper isn't running right now."));
    }
    let command = if cancel {
        wfmt_types::Command::Cancel
    } else {
        wfmt_types::Command::ScrapeAll
    };
    wfmt_scraper::engine::send_command(&dir, &command).map_err(ServerFnError::new)
}

/// Uploads every finished day that isn't on the Hub yet, or when pulling, downloads the new
/// days, without waiting for the hourly check.
#[post("/api/hub/upload")]
pub async fn hub_upload(token: String) -> Result<(), ServerFnError> {
    crate::admin::check(&token).await?;
    crate::hub::request_upload().map_err(ServerFnError::new)
}

/// Answers for the server's first render of a page, embedded in its HTML so the browser shows
/// real content before the WASM has loaded and doesn't repeat the request (see `ui::use_seed`).
/// `None` when the data can't answer without waiting, and always in the browser, which then
/// fetches as usual.
pub mod seed {
    use super::{Overview, Query, RivenDetail, SearchResult, StatEffects};

    macro_rules! seeds {
        ($(fn $name:ident($($arg:ident: $ty:ty),*) -> $out:ty = |$store:ident| $body:expr;)*) => {$(
            #[cfg(feature = "server")]
            pub fn $name($($arg: $ty),*) -> Option<$out> {
                crate::data::try_now(|$store| $body)
            }
            #[cfg(not(feature = "server"))]
            pub fn $name($(_: $ty),*) -> Option<$out> {
                None
            }
        )*};
    }

    seeds! {
        fn overview() -> Overview = |store| store.overview_memo();
        fn search(query: &Query) -> SearchResult = |store| store.search_memo(query);
        fn stat_effects(weapon: &str) -> StatEffects = |store| store.stat_effects(weapon);
        fn riven_detail(id: &str, auction: Option<&str>) -> Option<RivenDetail> =
            |store| store.riven_detail(id, auction, super::Comparisons::default());
    }
}
