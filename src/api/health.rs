use axum::extract::State;
use std::sync::Arc;

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

#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Health",
    responses(
        (status = 200, description = "Service is ready to serve requests", body = String)
    )
)]
pub async fn readyz(State(state): State<Arc<AppState>>) -> &'static str {
    let _keys = state.cache.keys();
    "ok"
}
