//! Read-only matches against the shared dataset; never polls upstream per visitor.
use dioxus::fullstack::axum::{Json, http::StatusCode};
use wfmt_api::{AlertFilter, AlertSnapshot};

pub async fn matches(
    Json(filters): Json<Vec<AlertFilter>>,
) -> Result<Json<AlertSnapshot>, (StatusCode, &'static str)> {
    if filters.len() > 50
        || filters.iter().any(|f| {
            f.weapon.len() > 100 || f.weapon.is_empty() || f.max_price <= 0 || f.min_reputation < 0
        })
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Invalid alert filters (maximum 50).",
        ));
    }
    let app = crate::app::get();
    let now = crate::app::now_ms();
    let (listings, latest_observation) = app
        .query(move |data| data.alert_matches(&filters, now))
        .await
        .map_err(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not load listings.",
            )
        })?;
    Ok(Json(AlertSnapshot {
        listings,
        latest_observation,
        live_scraper: app.scraper.is_running(),
        server_time: now,
    }))
}
