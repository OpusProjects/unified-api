use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use std::sync::Arc;

use crate::AppState;
use crate::domain::dataset::HostVars;

// Immediate host add/remove in a source's cache

#[utoipa::path(
    put,
    path = "/api/v1/sources/{id}/hosts/{hostname}",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier"),
        ("hostname" = String, Path, description = "Host to add or update")
    ),
    request_body(content = Object, description = "Host variables as JSON key-value pairs"),
    responses(
        (status = 200, description = "Host added/updated"),
        (status = 404, description = "Source not in cache")
    )
)]
pub async fn put_host(
    State(state): State<Arc<AppState>>,
    Path((id, hostname)): Path<(String, String)>,
    Json(vars): Json<HostVars>,
) -> Result<StatusCode, StatusCode> {
    let mut vars = Some(vars);
    let found = state.cache.update(&id, &mut |entry| {
        if let Some(v) = vars.take() {
            entry.update_host(hostname.clone(), v);
        }
    });
    if !found {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::OK)
}

#[utoipa::path(
    delete,
    path = "/api/v1/sources/{id}/hosts/{hostname}",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier"),
        ("hostname" = String, Path, description = "Host to remove")
    ),
    responses(
        (status = 200, description = "Host removed"),
        (status = 404, description = "Source or host not in cache")
    )
)]
pub async fn delete_host(
    State(state): State<Arc<AppState>>,
    Path((id, hostname)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    // The "does host exist?" check and deletion happen in the same
    // update() — checking outside with get() would be another race window.
    let mut removed = false;
    let found = state.cache.update(&id, &mut |entry| {
        if entry.dataset.hostvars.contains_key(&hostname) {
            entry.remove_host(&hostname);
            removed = true;
        }
    });
    if !found || !removed {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(StatusCode::OK)
}
