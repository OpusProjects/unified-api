use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Health",
    responses(
        (status = 200, description = "Service is alive", body = String)
    )
)]
pub async fn healthz(State(_state): State<Arc<AppState>>) -> &'static str {
    "ok"
}

#[derive(Serialize, ToSchema)]
pub struct ReadyStatus {
    pub ready: bool,
    pub sources_total: usize,
    pub sources_synced: usize,
    pub sources_pending: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Health",
    responses(
        (status = 200, description = "Service is ready", body = ReadyStatus),
        (status = 503, description = "Service is not ready — sources pending sync", body = ReadyStatus)
    )
)]
pub async fn readyz(State(state): State<Arc<AppState>>) -> (StatusCode, Json<ReadyStatus>) {
    let sources_total = state.sources.len();

    let sources_pending: Vec<String> = state
        .sources
        .keys()
        .filter(|id| state.cache.get(id).is_none())
        .cloned()
        .collect();

    let sources_synced = sources_total - sources_pending.len();

    // Ready if no sources configured, or if at least one is synced
    let ready = sources_total == 0 || sources_synced > 0;

    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(ReadyStatus {
            ready,
            sources_total,
            sources_synced,
            sources_pending,
        }),
    )
}
