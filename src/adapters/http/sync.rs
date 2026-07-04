use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

// Renombramos el caso de uso al importarlo porque el handler HTTP
// que lo envuelve se llama igual (sync_source)
use crate::application::sync::{sync_source as application_sync_source, SyncScope};
use crate::AppState;

// IntoParams = utoipa genera la documentación de los query params
// Cada campo Option<String> aparece como parámetro opcional en Swagger
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

    // El handler solo traduce HTTP ↔ caso de uso; la lógica del sync
    // vive en application::sync (compartida con el scheduler)
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
