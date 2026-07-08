use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

// Rename the use case on import because the HTTP handler wrapping
// it has the same name (run_enricher)
use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::application::enrich::run_enricher as application_run_enricher;

#[derive(Serialize, ToSchema)]
pub struct EnrichResult {
    pub source_id: String,
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
        (status = 403, description = "API key not allowed to write this enricher's source"),
        (status = 404, description = "Enricher not configured or source not in cache")
    )
)]
pub async fn run_enricher(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<EnrichResult>, StatusCode> {
    let enricher_def = state.enrichers.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    // An enricher writes into its source's cache entry, so the permission
    // that matters is the SOURCE one — no separate enricher grant to manage.
    if !auth.permissions.allows_source(&enricher_def.source_id) {
        return Err(StatusCode::FORBIDDEN);
    }

    // None = source not in cache → 404
    let outcome = application_run_enricher(&*state.cache, &*state.enricher, enricher_def)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(EnrichResult {
        source_id: enricher_def.source_id.clone(),
        enricher_id: id,
        success: outcome.success(),
        hosts_updated: outcome.hosts_updated,
        hosts_removed: outcome.hosts_removed,
        duration_ms: outcome.duration_ms,
        error: outcome.error,
    }))
}
