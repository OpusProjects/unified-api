use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};

// Cache administration. Filling a source's entry is sync.rs; this is its
// inverse, and the only operation that drops a whole entry rather than
// individual hosts (hosts.rs).

#[derive(Serialize, ToSchema)]
pub struct EvictResult {
    pub source_id: String,
    /// Hosts that were in the entry that got dropped
    pub hosts_dropped: usize,
}

#[utoipa::path(
    delete,
    path = "/api/v1/sources/{id}",
    tag = "Sources",
    params(("id" = String, Path, description = "Source identifier")),
    responses(
        (status = 200, description = "Cache entry dropped (the source's configuration is untouched)", body = EvictResult),
        (status = 400, description = "The id names a view — a view holds no cache entry of its own", body = ErrorBody),
        (status = 403, description = "API key not allowed to touch this source", body = ErrorBody),
        (status = 404, description = "Source not in cache", body = ErrorBody)
    )
)]
pub async fn evict_source(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<EvictResult>, ApiError> {
    // Same permission as the other writes to a source's entry (host
    // PUT/DELETE): a key that may replace the hosts one by one may drop them
    // all at once.
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    if let Some(view) = state.config().views.get(&id) {
        return Err(crate::adapters::r#in::http::views::write_refused(
            &id,
            view,
            "be evicted",
        ));
    }

    // Read the size before dropping, so the response says what was discarded.
    // A concurrent sync landing between these two calls can only change the
    // reported count, never whether the entry ends up gone.
    let entry = state
        .cache
        .get(&id)
        .ok_or_else(|| ApiError::source_not_cached(&id))?;
    let hosts_dropped = entry.dataset.hostvars.len();

    state.cache.remove(&id);
    crate::adapters::r#in::http::audit::record(&auth, &headers, "evict", &id, "success");

    Ok(Json(EvictResult {
        source_id: id,
        hosts_dropped,
    }))
}
