use axum::Extension;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

use std::borrow::Cow;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::domain::dataset::{Group, HostVars};

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
    /// Return only these top-level hostvars keys (comma-separated)
    pub fields: Option<String>,
}

impl DatasetParams {
    fn is_plain(&self) -> bool {
        self.host.is_none()
            && self.group.is_none()
            && self.limit.is_none()
            && self.offset.is_none()
            && self.fields.is_none()
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
        (status = 200, description = "Without query params: the raw Dataset (hostvars + groups), with an ETag header. With host/group/limit/offset/fields: a paginated envelope with total_hosts, offset, limit, hostvars and groups. The fields param filters hostvars to only the named top-level keys."),
        (status = 304, description = "If-None-Match matched the current ETag — dataset unchanged, no body (plain queries only)"),
        (status = 403, description = "API key not allowed to read this source", body = ErrorBody),
        (status = 404, description = "Source not in cache, or host/group not found", body = ErrorBody)
    )
)]
pub async fn get_source_dataset(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<DatasetParams>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    let entry = state
        .cache
        .get(&id)
        .ok_or_else(|| ApiError::source_not_cached(&id))?;

    // No params = the raw Dataset, as consumers (AWX inventory scripts, the
    // remote-federation pattern) already parse it. The bytes come from the
    // entry's serialize-once cache: polls of an unchanged dataset share one
    // buffer instead of re-serializing on every request, and the ETag lets a
    // client that sends If-None-Match skip even the transfer.
    if params.is_plain() {
        let cached = entry
            .serialized_json()
            .map_err(|e| ApiError::internal(format!("failed to serialize dataset: {}", e)))?;

        if let Some(if_none_match) = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            && if_none_match.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == cached.etag || candidate == "*"
            })
        {
            return Ok(Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &cached.etag)
                .body(axum::body::Body::empty())
                .unwrap());
        }

        return Ok(Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, &cached.etag)
            // Bytes clone = shared buffer, not a copy
            .body(axum::body::Body::from(cached.bytes.clone()))
            .unwrap());
    }

    // Which hosts survive the filter, sorted so limit/offset pages are stable
    let mut hostnames: Vec<&String> = if let Some(ref host) = params.host {
        host.split(',')
            .filter_map(|h| {
                entry
                    .dataset
                    .hostvars
                    .get_key_value(h.trim())
                    .map(|(k, _)| k)
            })
            .collect()
    } else if let Some(ref group) = params.group {
        match entry.dataset.groups.get(group) {
            Some(g) => g.hosts.iter().collect(),
            None => {
                return Err(ApiError::not_found(format!(
                    "group '{}' is not in source '{}'",
                    group, id
                )));
            }
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

    let field_set: Option<std::collections::HashSet<&str>> = params
        .fields
        .as_ref()
        .map(|f| f.split(',').map(|s| s.trim()).collect());

    let hostvars: HashMap<&String, Cow<HostVars>> = page
        .iter()
        .filter_map(|host| {
            let (k, v) = entry.dataset.hostvars.get_key_value(*host)?;
            let vars = match &field_set {
                Some(fields) => Cow::Owned(
                    v.iter()
                        .filter(|(key, _)| fields.contains(key.as_str()))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect(),
                ),
                None => Cow::Borrowed(v),
            };
            Some((k, vars))
        })
        .collect();

    // With a group filter only that group is returned; otherwise all groups
    // (membership lists are tiny next to hostvars, which carry the facts)
    let groups: HashMap<&String, &Group> = match params.group {
        Some(ref group) => entry
            .dataset
            .groups
            .get_key_value(group)
            .into_iter()
            .collect(),
        None => entry.dataset.groups.iter().collect(),
    };

    // A borrowing struct serialized straight to bytes: no intermediate Value
    // tree (same reasoning as the plain path — a group filter can select most
    // of a large source) and no clone of the selected hostvars.
    #[derive(Serialize)]
    struct DatasetPage<'a> {
        source_id: &'a str,
        total_hosts: usize,
        offset: usize,
        limit: Option<usize>,
        returned: usize,
        hostvars: HashMap<&'a String, Cow<'a, HostVars>>,
        groups: HashMap<&'a String, &'a Group>,
    }

    let body = serde_json::to_vec(&DatasetPage {
        source_id: &id,
        total_hosts,
        offset,
        limit: params.limit,
        returned: hostvars.len(),
        hostvars,
        groups,
    })
    .map_err(|e| ApiError::internal(format!("failed to serialize dataset page: {}", e)))?;

    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .unwrap())
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
        (status = 403, description = "API key not allowed to read this source", body = ErrorBody),
        (status = 404, description = "Source not in cache, or host/group not found", body = ErrorBody)
    )
)]
pub async fn source_status(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<StatusParams>,
) -> Result<Json<SourceStatus>, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    let entry = state
        .cache
        .get(&id)
        .ok_or_else(|| ApiError::source_not_cached(&id))?;
    let source = state.sources.get(&id);

    let hostnames: Vec<String> = if let Some(ref host) = params.host {
        host.split(',')
            .map(|h| h.trim())
            .filter(|h| entry.dataset.hostvars.contains_key(*h))
            .map(|h| h.to_string())
            .collect()
    } else if let Some(ref group) = params.group {
        match entry.dataset.groups.get(group) {
            Some(g) => g.hosts.clone(),
            None => {
                return Err(ApiError::not_found(format!(
                    "group '{}' is not in source '{}'",
                    group, id
                )));
            }
        }
    } else {
        entry.dataset.hostvars.keys().cloned().collect()
    };

    // Resolve group TTL overrides into a per-host map ONCE, instead of
    // scanning every group's member list for every host — that scan made a
    // full status quadratic on large sources. Host-level overrides still win
    // (checked first below). When a host sits in several groups that carry
    // an override the winner is arbitrary, exactly as before, when it
    // depended on group iteration order.
    let group_ttl_by_host: HashMap<&str, u64> = source
        .map(|s| {
            let mut by_host: HashMap<&str, u64> = HashMap::new();
            for (group_name, ttl) in &s.ttl_overrides.groups {
                if let Some(group) = entry.dataset.groups.get(group_name) {
                    for hostname in &group.hosts {
                        by_host.entry(hostname.as_str()).or_insert(*ttl);
                    }
                }
            }
            by_host
        })
        .unwrap_or_default();

    let mut hosts: Vec<HostStatus> = hostnames
        .iter()
        .filter_map(|hostname| {
            let age = entry.host_age_seconds(hostname)?;

            let effective_ttl = source
                .and_then(|s| s.ttl_overrides.hosts.get(hostname).copied())
                .or_else(|| group_ttl_by_host.get(hostname.as_str()).copied())
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
