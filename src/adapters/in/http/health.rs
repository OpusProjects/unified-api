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
    let config = state.config();
    let sources_total = config.sources.len();

    let sources_pending: Vec<String> = config
        .sources
        .keys()
        .filter(|id| state.cache.get(id).is_none())
        .cloned()
        .collect();

    let sources_synced = sources_total - sources_pending.len();

    // Default: ready if no sources are configured, or at least one is synced —
    // a pod serving part of the inventory beats one serving nothing while it
    // waits on the slowest source. With readyz_require_all_sources, every
    // configured source must be in cache first, for deployments where a
    // partial inventory is worse than none (a job template that would run
    // against half a datacenter).
    let ready = if config.readyz_require_all_sources {
        sources_pending.is_empty()
    } else {
        sources_total == 0 || sources_synced > 0
    };

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
