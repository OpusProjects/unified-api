use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

// Rename the use case on import because the HTTP handler wrapping
// it has the same name (run_enricher)
use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::enrich::run_enricher as application_run_enricher;

#[derive(Serialize, ToSchema)]
pub struct EnrichResult {
    pub target_id: String,
    pub enricher_id: String,
    pub success: bool,
    pub hosts_updated: usize,
    pub hosts_removed: usize,
    pub duration_ms: u128,
    pub error: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/enrichers/{id}/run",
    tag = "Enrichers",
    params(
        ("id" = String, Path, description = "Enricher identifier (e.g. enrich-resolve-ssh)")
    ),
    responses(
        (status = 200, description = "Enrichment result", body = EnrichResult),
        (status = 403, description = "API key not allowed to write this enricher's target", body = ErrorBody),
        (status = 404, description = "Enricher not configured or target not in cache", body = ErrorBody)
    )
)]
pub async fn run_enricher(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<EnrichResult>, ApiError> {
    let enricher_def = state
        .enrichers
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("enricher '{}' is not configured", id)))?;

    // An enricher writes into its target's cache entry, so the permission
    // that matters is the TARGET one — no separate enricher grant to manage.
    if !auth.permissions.allows_source(&enricher_def.target_id) {
        return Err(ApiError::forbidden(format!(
            "this API key is not allowed to write source '{}', the target of enricher '{}'",
            enricher_def.target_id, id
        )));
    }

    // None = target not in cache. Same status as an unknown enricher id before
    // this change, which sent a caller looking for a config typo that wasn't
    // there.
    let outcome = application_run_enricher(&*state.cache, &*state.enricher, enricher_def)
        .await
        .ok_or_else(|| {
            ApiError::not_found(format!(
                "target '{}' of enricher '{}' is not in the cache — sync it first",
                enricher_def.target_id, id
            ))
        })?;

    Ok(Json(EnrichResult {
        target_id: enricher_def.target_id.clone(),
        enricher_id: id,
        success: outcome.success(),
        hosts_updated: outcome.hosts_updated,
        hosts_removed: outcome.hosts_removed,
        duration_ms: outcome.duration_ms,
        error: outcome.error,
    }))
}
