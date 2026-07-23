use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;

// Read from the sources cache: list, full dataset, and per-host status.
// Write operations live in sync.rs, enrichers.rs, and hosts.rs.
//
// Authorization pattern used by every handler that takes an id: the auth
// middleware already verified WHO calls (AuthContext in the extensions);
// each handler checks whether that identity may touch THIS id and answers
// 403 Forbidden if not. List endpoints filter instead of failing: a scoped
// key sees its slice of the world, not an error.

// ToSchema = utoipa generates the JSON Schema definition for this struct
// It will appear in the "Schemas" section of the Swagger UI
#[derive(Serialize, ToSchema)]
pub struct CachedSourceInfo {
    pub source_id: String,
    pub is_fresh: bool,
    pub age_seconds: u64,
    pub total_hosts: usize,
}

// #[utoipa::path] describes the endpoint for documentation:
// - get = HTTP method
// - path = the URL
// - responses = what it returns and with which status code
// - tag = grouping in the Swagger UI
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
    Extension(auth): Extension<AuthContext>,
) -> Json<Vec<CachedSourceInfo>> {
    let keys = state.cache.keys();

    let sources: Vec<CachedSourceInfo> = keys
        .iter()
        .filter(|key| auth.permissions.allows_source(key))
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

// Query parameters for the dataset endpoint. All optional — without any of
// them the response is the raw Dataset, exactly as before (consumers depend
// on that shape). With any of them, the response becomes a paginated
// envelope; large inventories (a 1000-host dataset is ~10MB of JSON) hang
// browser UIs like Swagger when rendered whole.
#[derive(Deserialize, IntoParams)]
pub struct DatasetParams {
    /// Return only these hosts (comma-separated)
    pub host: Option<String>,
    /// Return only the hosts of this group
    pub group: Option<String>,
    /// Max hosts to return (hosts are sorted by name for stable pages)
    pub limit: Option<usize>,
    /// How many hosts to skip (use with limit to page)
    pub offset: Option<usize>,
}

impl DatasetParams {
    fn is_plain(&self) -> bool {
        self.host.is_none() && self.group.is_none() && self.limit.is_none() && self.offset.is_none()
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/dataset",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier (e.g. src-section9)"),
        DatasetParams
    ),
    responses(
        (status = 200, description = "Without query params: the raw Dataset (hostvars + groups). With host/group/limit/offset: a paginated envelope with total_hosts, offset, limit, hostvars and groups"),
        (status = 403, description = "API key not allowed to read this source"),
        (status = 404, description = "Source not in cache, or host/group not found")
    )
)]
pub async fn get_source_dataset(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<DatasetParams>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    if !auth.permissions.allows_source(&id) {
        return Err(StatusCode::FORBIDDEN);
    }
    let entry = state.cache.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    // No params = the raw Dataset, byte-compatible with what consumers
    // (AWX inventory scripts, the remote-federation pattern) already parse
    if params.is_plain() {
        let json =
            serde_json::to_value(&entry.dataset).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(Json(json));
    }

    // Which hosts survive the filter, sorted so limit/offset pages are stable
    let mut hostnames: Vec<&String> = if let Some(ref host) = params.host {
        let matched: Vec<&String> = host
            .split(',')
            .filter_map(|h| entry.dataset.hostvars.get_key_value(h.trim()).map(|(k, _)| k))
            .collect();
        if matched.is_empty() {
            return Err(StatusCode::NOT_FOUND);
        }
        matched
    } else if let Some(ref group) = params.group {
        match entry.dataset.groups.get(group) {
            Some(g) => g.hosts.iter().collect(),
            None => return Err(StatusCode::NOT_FOUND),
        }
    } else {
        entry.dataset.hostvars.keys().collect()
    };
    hostnames.sort();
    hostnames.dedup();

    let total_hosts = hostnames.len();
    let offset = params.offset.unwrap_or(0);
    let page: Vec<&String> = hostnames
        .into_iter()
        .skip(offset)
        .take(params.limit.unwrap_or(usize::MAX))
        .collect();

    let hostvars: HashMap<&String, &crate::domain::dataset::HostVars> = page
        .iter()
        .filter_map(|host| entry.dataset.hostvars.get_key_value(*host))
        .collect();

    // With a group filter only that group is returned; otherwise all groups
    // (membership lists are tiny next to hostvars, which carry the facts)
    let groups: HashMap<&String, &crate::domain::dataset::Group> = match params.group {
        Some(ref group) => entry
            .dataset
            .groups
            .get_key_value(group)
            .into_iter()
            .collect(),
        None => entry.dataset.groups.iter().collect(),
    };

    let json = serde_json::json!({
        "source_id": id,
        "total_hosts": total_hosts,
        "offset": offset,
        "limit": params.limit,
        "returned": hostvars.len(),
        "hostvars": hostvars,
        "groups": groups,
    });
    Ok(Json(json))
}

// IntoParams = utoipa generates documentation for query params
// Each Option<String> field appears as an optional parameter in Swagger
#[derive(Deserialize, IntoParams)]
pub struct StatusParams {
    /// Filter by hostname, comma-separated (e.g. motoko.section9.net)
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
        (status = 403, description = "API key not allowed to read this source"),
        (status = 404, description = "Source not in cache, or host/group not found")
    )
)]
pub async fn source_status(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<StatusParams>,
) -> Result<Json<SourceStatus>, StatusCode> {
    if !auth.permissions.allows_source(&id) {
        return Err(StatusCode::FORBIDDEN);
    }
    let entry = state.cache.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let source = state.sources.get(&id);

    let hostnames: Vec<String> = if let Some(ref host) = params.host {
        let matched: Vec<String> = host
            .split(',')
            .map(|h| h.trim())
            .filter(|h| entry.dataset.hostvars.contains_key(*h))
            .map(|h| h.to_string())
            .collect();
        if matched.is_empty() {
            return Err(StatusCode::NOT_FOUND);
        }
        matched
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
                    s.ttl_overrides.hosts.get(hostname).copied().or_else(|| {
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
