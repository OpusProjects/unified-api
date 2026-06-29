use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use utoipa::{IntoParams, ToSchema};

use crate::domain::cache_entry::CacheEntry;
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

    let mut config = source.config.clone();
    let scope_label;

    if let Some(ref host) = params.host {
        config.insert("scope".to_string(), "host".to_string());
        config.insert("target".to_string(), host.clone());
        scope_label = format!("host:{}", host);
    } else if let Some(ref group) = params.group {
        config.insert("scope".to_string(), "group".to_string());
        config.insert("target".to_string(), group.clone());
        scope_label = format!("group:{}", group);
    } else {
        scope_label = "full".to_string();
    }

    let start = Instant::now();
    let credentials: HashMap<String, String> = HashMap::new();

    let result = state
        .connector
        .execute(&source.script_path, &config, &credentials)
        .await;

    let duration_ms = start.elapsed().as_millis();

    match result {
        Ok(dataset) => {
            let total_hosts = dataset.hostvars.len();
            let total_groups = dataset.groups.len();

            if let Some(ref host) = params.host {
                if let Some(vars) = dataset.hostvars.get(host) {
                    if let Some(mut entry) = state.cache.get(&id) {
                        entry.update_host(host.clone(), vars.clone());
                        state.cache.set(&id, entry);
                    } else {
                        state.cache.set(&id, CacheEntry::new(dataset, source.ttl_seconds));
                    }
                }
            } else if let Some(ref group) = params.group {
                if let Some(mut entry) = state.cache.get(&id) {
                    entry.update_group(group, dataset);
                    state.cache.set(&id, entry);
                } else {
                    state.cache.set(&id, CacheEntry::new(dataset, source.ttl_seconds));
                }
            } else {
                state.cache.set(&id, CacheEntry::new(dataset, source.ttl_seconds));
            }

            Ok(Json(SyncResult {
                source_id: id,
                success: true,
                scope: scope_label,
                total_hosts,
                total_groups,
                sync_duration_ms: duration_ms,
                error: None,
            }))
        }
        Err(connector_error) => Ok(Json(SyncResult {
            source_id: id,
            success: false,
            scope: scope_label,
            total_hosts: 0,
            total_groups: 0,
            sync_duration_ms: duration_ms,
            error: Some(connector_error.message),
        })),
    }
}
