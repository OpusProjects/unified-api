use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use std::sync::Arc;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
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
        (status = 400, description = "The id names a view — a view is read-only and holds no cache entry", body = ErrorBody),
        (status = 404, description = "Source not in cache", body = ErrorBody)
    )
)]
pub async fn put_host(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path((id, hostname)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(vars): Json<HostVars>,
) -> Result<StatusCode, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    if let Some(view) = state.views.get(&id) {
        return Err(crate::adapters::r#in::http::views::write_refused(
            &id,
            view,
            "take a host write",
        ));
    }
    let mut vars = Some(vars);
    let found = state.cache.update(&id, &mut |entry| {
        if let Some(v) = vars.take() {
            entry.update_host(hostname.clone(), v);
        }
    });
    if !found {
        return Err(ApiError::source_not_cached(&id));
    }
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        "host_put",
        &format!("{}/{}", id, hostname),
        "success",
    );
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
        (status = 400, description = "The id names a view — a view is read-only and holds no cache entry", body = ErrorBody),
        (status = 404, description = "Source or host not in cache", body = ErrorBody)
    )
)]
pub async fn delete_host(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path((id, hostname)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<StatusCode, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    if let Some(view) = state.views.get(&id) {
        return Err(crate::adapters::r#in::http::views::write_refused(
            &id,
            view,
            "take a host write",
        ));
    }
    // The "does host exist?" check and deletion happen in the same
    // update() — checking outside with get() would be another race window.
    let mut removed = false;
    let found = state.cache.update(&id, &mut |entry| {
        if entry.dataset.hostvars.contains_key(&hostname) {
            entry.remove_host(&hostname);
            removed = true;
        }
    });
    // Both cases were one indistinguishable 404 before: "no such source" and
    // "source is there, host isn't" need different fixes
    if !found {
        return Err(ApiError::source_not_cached(&id));
    }
    if !removed {
        return Err(ApiError::not_found(format!(
            "host '{}' is not in source '{}'",
            hostname, id
        )));
    }
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        "host_delete",
        &format!("{}/{}", id, hostname),
        "success",
    );
    Ok(StatusCode::OK)
}
