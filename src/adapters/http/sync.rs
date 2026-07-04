use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

// Rename the use case on import because the HTTP handler wrapping
// it has the same name (sync_source)
use crate::AppState;
use crate::application::sync::{SyncScope, sync_source as application_sync_source};

// IntoParams = utoipa generates documentation for query params
// Each Option<String> field appears as an optional parameter in Swagger
#[derive(Deserialize, IntoParams)]
pub struct SyncParams {
    /// Sync only this host (e.g. motoko.section9.net)
    pub host: Option<String>,
    /// Sync only hosts in this group (e.g. magi)
    pub group: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SyncResult {
    pub source_id: String,
    pub success: bool,
    /// "full", "host:motoko.section9.net", or "group:magi"
    pub scope: String,
    pub total_hosts: usize,
    pub total_groups: usize,
    pub sync_duration_ms: u128,
    pub error: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/sources/{id}/sync",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier"),
        SyncParams
    ),
    responses(
        (status = 200, description = "Sync result with host/group counts", body = SyncResult),
        (status = 404, description = "Source not configured")
    )
)]
pub async fn sync_source(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<SyncParams>,
) -> Result<Json<SyncResult>, StatusCode> {
    let source = state.sources.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    let scope = if let Some(host) = params.host {
        SyncScope::Host(host)
    } else if let Some(group) = params.group {
        SyncScope::Group(group)
    } else {
        SyncScope::Full
    };

    // The handler only translates HTTP ↔ use case; the sync logic
    // lives in application::sync (shared with the scheduler)
    let connector = state.connector_for(&source.connector_type);
    let outcome = application_sync_source(
        &*state.cache,
        &**connector,
        &*state.secrets,
        &id,
        source,
        scope,
    )
    .await;

    Ok(Json(SyncResult {
        source_id: id,
        success: outcome.success(),
        scope: outcome.scope,
        total_hosts: outcome.total_hosts,
        total_groups: outcome.total_groups,
        sync_duration_ms: outcome.duration_ms,
        error: outcome.error,
    }))
}
