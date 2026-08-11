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
use crate::adapters::r#in::http::sources::SyncHealthInfo;
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

#[derive(Serialize, ToSchema)]
pub struct EnricherInfo {
    pub enricher_id: String,
    pub name: String,
    pub target_id: String,
    /// Present for a declarative merge, absent for a script enricher
    pub source_id: Option<String>,
    /// The top-level hostvars keys a declarative merge copies
    pub fields: Option<Vec<String>>,
    pub script_path: Option<String>,
    pub sync_interval_seconds: Option<u64>,
    /// Whether the target is in the cache. An enricher whose target has never
    /// synced cannot run, and said so only when you tried it.
    pub target_ready: bool,
    /// Whether anything is still managing to run this enricher — last attempt,
    /// last success, last error, consecutive failures. Absent until it has run
    /// at least once through this process. Same shape as a source's
    /// `sync_health`, for the same reason: a permanently failing enricher used
    /// to be visible only in the logs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
}

#[utoipa::path(
    get,
    path = "/api/v1/enrichers",
    tag = "Enrichers",
    responses(
        (status = 200, description = "List configured enrichers", body = Vec<EnricherInfo>)
    )
)]
pub async fn list_enrichers(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<Vec<EnricherInfo>> {
    // Same rule as running one: an enricher writes into its target, so the
    // target's permission is the one that governs.
    let mut enrichers: Vec<EnricherInfo> = state
        .enrichers
        .iter()
        .filter(|(_, enricher)| auth.permissions.allows_source(&enricher.target_id))
        .map(|(id, enricher)| EnricherInfo {
            enricher_id: id.clone(),
            name: enricher.name.clone(),
            target_id: enricher.target_id.clone(),
            source_id: enricher.source_id.clone(),
            fields: enricher.fields.clone(),
            script_path: enricher.script_path.clone(),
            sync_interval_seconds: enricher.sync_interval_seconds,
            target_ready: state.cache.get(&enricher.target_id).is_some(),
            sync_health: state.enrich_health.get(id).map(Into::into),
        })
        .collect();

    enrichers.sort_by(|a, b| a.enricher_id.cmp(&b.enricher_id));
    Json(enrichers)
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
    let outcome = application_run_enricher(
        &*state.cache,
        &*state.enricher,
        &state.enrich_health,
        &id,
        enricher_def,
    )
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
