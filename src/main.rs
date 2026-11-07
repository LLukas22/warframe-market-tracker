#[cfg(feature = "server")]
mod admin;
mod api;
mod chart;
#[cfg(feature = "server")]
mod data;
#[cfg(feature = "server")]
mod effects;
#[cfg(feature = "server")]
mod images;
mod journey;
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
        Ok(dioxus::server::router(ui::App)
            .route("/img/weapon/{slug}", get(images::weapon))
            .route("/api/data/export", post(transfer::export))
            .route("/api/data/import", post(transfer::import)))
    });
}
