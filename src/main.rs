#[cfg(feature = "server")]
mod admin;
mod api;
mod chart;
#[cfg(feature = "server")]
mod data;
#[cfg(feature = "server")]
mod effects;
#[cfg(feature = "server")]
mod hub;
#[cfg(feature = "server")]
mod images;
mod journey;
#[cfg(feature = "server")]
mod ocr_assets;
mod riven_view;
mod scan;
mod scraper;
#[cfg(feature = "server")]
mod transfer;
mod ui;
mod weapons;
#[cfg(feature = "server")]
mod worker;

#[cfg(not(feature = "server"))]
fn main() {
    dioxus::launch(ui::App);
}

#[cfg(feature = "server")]
fn main() {
    use dioxus::fullstack::axum::routing::{get, post};

    dioxus::serve(|| async move {
        transfer::recover(data::data_dir()).await?;
        worker::start().await?;
        hub::start();
        Ok(dioxus::server::router(ui::App)
            .route("/img/weapon/{slug}", get(images::weapon))
            .route("/ocr/{*path}", get(ocr_assets::serve))
            .route("/api/data/export", post(transfer::export))
            .route("/api/data/import", post(transfer::import))
            // Covers HTML, API responses and WASM/JS when no precompressed asset exists.
            // The layer negotiates Accept-Encoding and skips already encoded responses.
            .layer(tower_http::compression::CompressionLayer::new()))
    });
}
