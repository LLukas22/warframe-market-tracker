#[cfg(feature = "server")]
mod admin;
mod api;
#[cfg(feature = "server")]
mod app;
mod chart;
#[cfg(feature = "server")]
mod config;
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

    // Settings are checked before anything starts. Logging only starts with the server, so a
    // bad setting is reported on stderr.
    let app = match config::Config::from_env().and_then(app::init) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("Can't start: {error:#}");
            std::process::exit(1);
        }
    };
    // Runs again when the router is hot-reloaded in development, so everything here only
    // starts what isn't running yet.
    dioxus::serve(move || async move {
        app.stop_on_signal();
        transfer::recover(app.data_dir.clone()).await?;
        app.scraper.start(&app.data_dir).await?;
        app.hub.start(app.data_dir.clone());
        // Load the data in the background while the server starts listening, so the first
        // visitor doesn't pay for it. Pages rendered before it's done fetch from the browser.
        tokio::task::spawn_blocking(|| app.store.refresh());
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
