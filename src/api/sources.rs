use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

// Renombramos los casos de uso al importarlos porque los handlers HTTP
// que los envuelven se llaman igual (sync_source, run_enricher)
use crate::application::enrich::run_enricher as application_run_enricher;
use crate::application::sync::{sync_source as application_sync_source, SyncScope};
use crate::domain::dataset::HostVars;
use crate::AppState;

// ToSchema = utoipa genera la definición JSON Schema de este struct
// Aparecerá en la sección "Schemas" del Swagger UI
#[derive(Serialize, ToSchema)]
pub struct CachedSourceInfo {
    pub source_id: String,
    pub is_fresh: bool,
    pub age_seconds: u64,
    pub total_hosts: usize,
}

// #[utoipa::path] describe el endpoint para la documentación:
// - get = método HTTP
// - path = la URL
// - responses = qué devuelve y con qué status code
// - tag = agrupación en el Swagger UI
#[utoipa::path(
    get,
    path = "/api/v1/sources",
    tag = "Sources",
    responses(
        (status = 200, description = "List of cached sources with freshness info", body = Vec<CachedSourceInfo>)
    )
)]
pub async fn list_cached_sources(
    State(state): State<Arc<AppState>>,
) -> Json<Vec<CachedSourceInfo>> {
    let keys = state.cache.keys();

    let sources: Vec<CachedSourceInfo> = keys
        .iter()
        .filter_map(|key| {
            let entry = state.cache.get(key)?;
            Some(CachedSourceInfo {
                source_id: key.clone(),
                is_fresh: entry.is_fresh(),
                age_seconds: entry.age_seconds(),
                total_hosts: entry.dataset.hostvars.len(),
            })
        })
        .collect();

    Json(sources)
}

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/dataset",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier (e.g. src-section9)")
    ),
    responses(
        (status = 200, description = "Full cached dataset with hostvars and groups"),
        (status = 404, description = "Source not found in cache")
    )
)]
pub async fn get_source_dataset(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match state.cache.get(&id) {
        Some(entry) => {
            let json = serde_json::to_value(&entry.dataset)
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
            Ok(Json(json))
        }
        None => Err(StatusCode::NOT_FOUND),
    }
}

// IntoParams = utoipa genera la documentación de los query params
// Cada campo Option<String> aparece como parámetro opcional en Swagger
#[derive(Deserialize, IntoParams)]
pub struct StatusParams {
    /// Filter by hostname (e.g. motoko.section9.net)
    pub host: Option<String>,
    /// Filter by group name (e.g. magi)
    pub group: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct HostStatus {
    pub hostname: String,
    pub age_seconds: u64,
    pub is_fresh: bool,
    pub ttl_seconds: u64,
}

#[derive(Serialize, ToSchema)]
pub struct SourceStatus {
    pub source_id: String,
    pub dataset_age_seconds: u64,
    pub dataset_is_fresh: bool,
    pub ttl_seconds: u64,
    pub total_hosts: usize,
    pub hosts: Vec<HostStatus>,
}

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/status",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier"),
        StatusParams
    ),
    responses(
        (status = 200, description = "Cache status per host with TTL info", body = SourceStatus),
        (status = 404, description = "Source not in cache, or host/group not found")
    )
)]
pub async fn source_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(params): Query<StatusParams>,
) -> Result<Json<SourceStatus>, StatusCode> {
    let entry = state.cache.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let source = state.sources.get(&id);

    let hostnames: Vec<String> = if let Some(ref host) = params.host {
        if entry.dataset.hostvars.contains_key(host) {
            vec![host.clone()]
        } else {
            return Err(StatusCode::NOT_FOUND);
        }
    } else if let Some(ref group) = params.group {
        match entry.dataset.groups.get(group) {
            Some(g) => g.hosts.clone(),
            None => return Err(StatusCode::NOT_FOUND),
        }
    } else {
        entry.dataset.hostvars.keys().cloned().collect()
    };

    let mut hosts: Vec<HostStatus> = hostnames
        .iter()
        .filter_map(|hostname| {
            let age = entry.host_age_seconds(hostname)?;

            let effective_ttl = source
                .and_then(|s| {
                    s.ttl_overrides.hosts.get(hostname).copied()
                        .or_else(|| {
                            entry.dataset.groups.iter().find_map(|(group_name, group)| {
                                if group.hosts.contains(hostname) {
                                    s.ttl_overrides.groups.get(group_name).copied()
                                } else {
                                    None
                                }
                            })
                        })
                })
                .unwrap_or(entry.ttl.as_secs());

            Some(HostStatus {
                hostname: hostname.clone(),
                age_seconds: age,
                is_fresh: entry.is_host_fresh(hostname, Some(effective_ttl)),
                ttl_seconds: effective_ttl,
            })
        })
        .collect();

    hosts.sort_by(|a, b| a.hostname.cmp(&b.hostname));

    Ok(Json(SourceStatus {
        source_id: id,
        dataset_age_seconds: entry.age_seconds(),
        dataset_is_fresh: entry.is_fresh(),
        ttl_seconds: entry.ttl.as_secs(),
        total_hosts: hosts.len(),
        hosts,
    }))
}

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

// --- Enricher endpoint ---

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
        (status = 404, description = "Enricher not configured or source not in cache")
    )
)]
pub async fn run_enricher(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<EnrichResult>, StatusCode> {
    let enricher_def = state.enrichers.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    // None = el source no está en cache → 404
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

// --- Alta y baja inmediata de hosts ---

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
    // La comprobación "¿existe el host?" y el borrado van dentro del mismo
    // update() — comprobar fuera con get() sería otra ventana de carrera.
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

