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
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::adapters::r#in::http::views;
use crate::application::refresh::{RefreshOutcome, refresh_hosts};
use crate::domain::cache_entry::CacheEntry;
use crate::domain::dataset::{Group, HostVars};
use crate::domain::sync_health::SyncHealth;

// Read from the sources cache: list, full dataset, and per-host status.
// Write operations live in sync.rs, enrichers.rs, and hosts.rs.
//
// Authorization pattern used by every handler that takes an id: the auth
// middleware already verified WHO calls (AuthContext in the extensions);
// each handler checks whether that identity may touch THIS id and answers
// 403 Forbidden if not. List endpoints filter instead of failing: a scoped
// key sees its slice of the world, not an error.

// Resolve the ?host= / ?group= filter into the hostnames it selects, sorted
// and deduplicated.
//
// Shared by /dataset and /status because the two must answer the same filter
// the same way, and twice they had not: only the dataset path deduplicated its
// selection, and until recently both refused an unmatched group with 404 while
// an unmatched host returned an empty result. One implementation removes the
// room for a third divergence.
//
// Neither input is unique: a group's member list can name a host twice (a
// connector emitting it under two nested groups that get merged), and
// ?host=a,a is a query a caller can build. Sorting also gives the callers
// their ordering for free — /status needs no separate sort, and /dataset's
// limit/offset pages stay stable.
//
// A filter matching nothing yields an empty selection, never an error: 404 on
// these routes means the SOURCE is not in the cache, which both callers check
// before getting here.
fn resolve_hostnames<'a>(
    entry: &'a CacheEntry,
    host: Option<&str>,
    group: Option<&str>,
) -> Vec<&'a String> {
    let mut hostnames: Vec<&String> = if let Some(host) = host {
        host.split(',')
            .filter_map(|h| {
                entry
                    .dataset
                    .hostvars
                    .get_key_value(h.trim())
                    .map(|(name, _)| name)
            })
            .collect()
    } else if let Some(group) = group {
        match entry.dataset.groups.get(group) {
            Some(g) => g.hosts.iter().collect(),
            None => Vec::new(),
        }
    } else {
        entry.dataset.hostvars.keys().collect()
    };

    hostnames.sort();
    hostnames.dedup();
    hostnames
}

// Validate what the caller asked for, then hand off to the use case.
//
// The two refusals are checked here because they are about the REQUEST, not
// about the refresh: naming no hosts, and a source that has not been given the
// capability. Everything after that is best effort — a refresh that fails
// reports itself in the headers and the read still answers from cache.
async fn refresh_before_reading(
    state: &Arc<AppState>,
    id: &str,
    host: Option<&str>,
) -> Result<RefreshOutcome, ApiError> {
    // source_for_sync rather than sources.get: the refresh executes the
    // connector, so the script path resolves into its checkout per execution
    let source = state
        .source_for_sync(id)
        .ok_or_else(|| ApiError::source_not_configured(id))?;

    if !source.allow_on_demand_refresh {
        return Err(ApiError::refresh_not_allowed(id));
    }

    let hosts: Vec<String> = host
        .map(|h| {
            h.split(',')
                .map(|name| name.trim())
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    if hosts.is_empty() {
        return Err(ApiError::refresh_needs_hosts());
    }

    let connector = state.connector_for(&source.connector_type);
    let enrichment = state.enrichment();
    Ok(refresh_hosts(
        &*state.cache,
        &**connector,
        &*state.secrets,
        &state.sync_health,
        &state.refresh,
        &state.syncs,
        id,
        &source,
        &hosts,
        // No view in play: the source's own TTL is the gate
        None,
        Some(&enrichment),
    )
    .await)
}

// Stamp the outcome onto whatever response the read produced, including a 304:
// a caller that asked for a refresh is told whether it happened even when the
// answer is "nothing changed".
fn with_refresh_headers(
    mut builder: axum::http::response::Builder,
    refresh: &Option<RefreshOutcome>,
) -> axum::http::response::Builder {
    if let Some(outcome) = refresh {
        builder = builder.header(HEADER_REFRESHED, (outcome.error.is_none()).to_string());
        if !outcome.refreshed.is_empty() {
            builder = header_if_valid(builder, HEADER_REFRESHED_HOSTS, outcome.refreshed.join(","));
        }
        if let Some(error) = &outcome.error {
            builder = header_if_valid(builder, HEADER_REFRESH_ERROR, sanitize_header_value(error));
        }
    }
    builder
}

// Header values cannot carry control characters, and both of these carry text
// from outside: a connector's stderr, and the hostnames the CALLER named in
// `?host=` — which come back in `refreshed` for any host a successful sync did
// not return, so a percent-encoded control byte in the query string reaches
// here verbatim.
//
// `Builder::header` defers an invalid value to `body()`, where every caller
// unwraps, so this used to panic the handler: a request for a host with a funny
// name dropped the connection. Skipping the header instead is the proportionate
// answer — it is metadata about the response, and failing to describe a response
// is no reason to refuse to send it.
fn header_if_valid(
    builder: axum::http::response::Builder,
    name: &'static str,
    value: String,
) -> axum::http::response::Builder {
    match axum::http::HeaderValue::try_from(value) {
        Ok(value) => builder.header(name, value),
        Err(_) => builder,
    }
}

// Replaces what a header cannot carry, so the common case — a script's stderr
// with newlines in it — still reaches the caller instead of being dropped.
pub(crate) fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ToSchema = utoipa generates the JSON Schema definition for this struct
// It will appear in the "Schemas" section of the Swagger UI
#[derive(Serialize, ToSchema)]
pub struct CachedSourceInfo {
    pub source_id: String,
    /// "source" for a cached source, "view" for a read-only composite over
    /// several of them. A view answers on the same routes in the same shapes,
    /// so this is the only place the difference shows.
    pub kind: &'static str,
    pub is_fresh: bool,
    pub age_seconds: u64,
    pub total_hosts: usize,
    /// Absent until the source has been synced at least once through this process
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
}

// The freshness fields above say how old the data is. These say whether
// anything is still managing to refresh it — the difference between "syncs
// hourly, last one 20 minutes ago" and "connector has been failing since
// Tuesday", which used to be visible only in the logs.
#[derive(Serialize, ToSchema)]
pub struct SyncHealthInfo {
    pub last_attempt_age_seconds: Option<u64>,
    pub last_success_age_seconds: Option<u64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl From<SyncHealth> for SyncHealthInfo {
    fn from(health: SyncHealth) -> Self {
        Self {
            last_attempt_age_seconds: health.last_attempt_age_seconds,
            last_success_age_seconds: health.last_success_age_seconds,
            last_error: health.last_error,
            consecutive_failures: health.consecutive_failures,
        }
    }
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
        (status = 200, description = "Cached sources with freshness info, then every configured view (kind: \"view\"). A view is listed whether or not its members have synced — it is a contract to discover, not a cache entry", body = Vec<CachedSourceInfo>)
    )
)]
pub async fn list_cached_sources(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
) -> Json<Vec<CachedSourceInfo>> {
    let keys = state.cache.keys();

    let mut sources: Vec<CachedSourceInfo> = keys
        .iter()
        .filter(|key| auth.permissions.allows_source(key))
        .filter_map(|key| {
            let entry = state.cache.get(key)?;
            Some(CachedSourceInfo {
                source_id: key.clone(),
                kind: "source",
                is_fresh: entry.is_fresh(),
                age_seconds: entry.age_seconds(),
                total_hosts: entry.dataset.hostvars.len(),
                sync_health: state.sync_health.get(key).map(Into::into),
            })
        })
        .collect();

    // Views are appended sorted by id, and are listed whether or not their
    // members have synced: a view is a contract to discover, not a cache entry
    // that appears once there is data.
    let mut view_ids: Vec<&String> = state
        .views
        .keys()
        .filter(|id| auth.permissions.allows_source(id))
        .collect();
    view_ids.sort();
    sources.extend(
        view_ids
            .into_iter()
            .map(|id| views::info(&state, id, &state.views[id])),
    );

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
    /// Max hosts to return (hosts are sorted by name for stable pages).
    /// Omitting it returns every host — Swagger prefills 50 from the example
    /// below so that clicking Execute doesn't hand the browser a whole
    /// datacenter; clear the field there to get the raw, unpaginated Dataset.
    // The example is not a server-side default (that would be a `default`, and
    // the server has none): it is what Swagger UI puts in the input box, which
    // is the only place the accidental 10MB request comes from.
    #[param(example = 50)]
    pub limit: Option<usize>,
    /// How many hosts to skip (use with limit to page)
    pub offset: Option<usize>,
    /// Return only these top-level hostvars keys (comma-separated)
    pub fields: Option<String>,
    /// Bring the requested hosts up to date before answering, if the source's
    /// TTL says they are stale. Requires `host` and a source configured with
    /// `allow_on_demand_refresh`. A refresh that fails does not fail the read:
    /// the cached data is served and the `x-unified-api-refreshed` header says
    /// what happened.
    pub refresh: Option<bool>,
}

impl DatasetParams {
    // `refresh` is deliberately not part of this: it changes how current the
    // data is, not the shape of the response. A consumer that adds
    // `&refresh=true` to an existing call keeps getting the shape it parses.
    pub fn is_plain(&self) -> bool {
        self.host.is_none()
            && self.group.is_none()
            && self.limit.is_none()
            && self.offset.is_none()
            && self.fields.is_none()
    }
}

// What the refresh did, as response headers rather than body fields — it is
// metadata about the response, not part of the resource, and putting it in
// headers is what lets both the raw Dataset and the paginated envelope carry it
// without either shape changing.
const HEADER_REFRESHED: &str = "x-unified-api-refreshed";
const HEADER_REFRESH_ERROR: &str = "x-unified-api-refresh-error";
const HEADER_REFRESHED_HOSTS: &str = "x-unified-api-refreshed-hosts";

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/dataset",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier (e.g. src-section9)"),
        DatasetParams
    ),
    responses(
        (status = 200, description = "Without query params: the raw Dataset (hostvars + groups), with an ETag header. With host/group/limit/offset/fields: a paginated envelope with total_hosts, offset, limit, hostvars and groups. The fields param filters hostvars to only the named top-level keys. With refresh=true the response also carries x-unified-api-refreshed (whether the refresh succeeded), x-unified-api-refreshed-hosts (which hosts were re-gathered) and, on failure, x-unified-api-refresh-error — the data is served either way."),
        (status = 304, description = "If-None-Match matched the current ETag — nothing changed, no body"),
        (status = 400, description = "refresh=true without ?host=: the hosts to refresh must be named", body = ErrorBody),
        (status = 403, description = "API key not allowed to read this source, or refresh=true on a source without allow_on_demand_refresh (on a view, on the member that owns the named hosts)", body = ErrorBody),
        (status = 404, description = "Source not in cache. An unmatched host/group filter is an empty result, not a 404 — except on a view, where a NAMED host that no member claims is a 404, because the request cannot be routed at all", body = ErrorBody)
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

    // A view answers here, in the same shapes: the id space is shared and
    // config validation rejects a collision, so this dispatch is unambiguous.
    if let Some(view) = state.views.get(&id) {
        return views::dataset(&state, &id, view, params, headers).await;
    }

    // The refresh runs BEFORE the entry is read and before any ETag is
    // computed: doing it after would answer a 304 for data the caller just
    // asked to have replaced.
    let refresh = if params.refresh.unwrap_or(false) {
        Some(refresh_before_reading(&state, &id, params.host.as_deref()).await?)
    } else {
        None
    };

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
            return Ok(with_refresh_headers(
                Response::builder()
                    .status(StatusCode::NOT_MODIFIED)
                    .header(header::ETAG, &cached.etag),
                &refresh,
            )
            .body(axum::body::Body::empty())
            .unwrap());
        }

        return Ok(with_refresh_headers(
            Response::builder()
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ETAG, &cached.etag),
            &refresh,
        )
        // Bytes clone = shared buffer, not a copy
        .body(axum::body::Body::from(cached.bytes.clone()))
        .unwrap());
    }

    // A filtered response depends on two things: the data and the query. The
    // validator combines both — the cache generation, bumped by every write,
    // and a hash of the parameters.
    //
    // The generation is global, so an unrelated source's sync also invalidates
    // this ETag. That is pessimistic (a needless re-transfer) but never stale,
    // and it costs one integer read instead of hashing the selected hostvars.
    //
    // Unlike the plain path's content-derived ETag, this one does not survive a
    // restart: the generation counter starts from zero again, so a client's
    // stored validator stops matching and it re-fetches once. Correct, just not
    // as good as the plain path.
    let etag = {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        params.host.hash(&mut hasher);
        params.group.hash(&mut hasher);
        params.limit.hash(&mut hasher);
        params.offset.hash(&mut hasher);
        params.fields.hash(&mut hasher);
        // A refresh request and a plain one get distinct validators: a client
        // polling with refresh=true must not be handed a 304 that was minted
        // for a request that never went to the origin.
        params.refresh.hash(&mut hasher);
        format!("\"{:x}-{}\"", hasher.finish(), state.cache.generation())
    };

    if let Some(if_none_match) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && if_none_match.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate == etag || candidate == "*"
        })
    {
        return Ok(with_refresh_headers(
            Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &etag),
            &refresh,
        )
        .body(axum::body::Body::empty())
        .unwrap());
    }

    let hostnames = resolve_hostnames(&entry, params.host.as_deref(), params.group.as_deref());

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

    Ok(with_refresh_headers(
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, &etag),
        &refresh,
    )
    .body(axum::body::Body::from(body))
    .unwrap())
}

#[derive(Serialize, ToSchema)]
pub struct GroupInfo {
    pub name: String,
    pub host_count: usize,
    pub children: Vec<String>,
    /// Whether the group carries group vars (the values are in the dataset)
    pub has_vars: bool,
}

// Discovery: what is in this source, without pulling the facts.
//
// Auto-groups derive their names from fact keys, so the group set is data
// dependent and cannot be read off the config. Before this route the only way
// to learn it was fetching the whole dataset (~11MB on an SSH source) or
// knowing that `?limit=0` returns all groups with no hosts.
#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/groups",
    tag = "Sources",
    params(("id" = String, Path, description = "Source identifier")),
    responses(
        (status = 200, description = "Groups in the cached dataset, sorted by name", body = Vec<GroupInfo>),
        (status = 403, description = "API key not allowed to read this source", body = ErrorBody),
        (status = 404, description = "Source not in cache", body = ErrorBody)
    )
)]
pub async fn list_source_groups(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<Vec<GroupInfo>>, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    if let Some(view) = state.views.get(&id) {
        return Ok(Json(views::groups(&state, &id, view)));
    }
    let entry = state
        .cache
        .get(&id)
        .ok_or_else(|| ApiError::source_not_cached(&id))?;

    let mut groups: Vec<GroupInfo> = entry
        .dataset
        .groups
        .iter()
        .map(|(name, group)| GroupInfo {
            name: name.clone(),
            // Unique members: a member list can repeat a host (a connector
            // emitting it under two nested groups), and a count that included
            // the duplicate would not match what ?group= returns.
            host_count: group
                .hosts
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            children: group.children.clone(),
            has_vars: group.vars.is_some(),
        })
        .collect();

    groups.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(groups))
}

#[derive(Serialize, ToSchema)]
pub struct HostList {
    pub source_id: String,
    pub total_hosts: usize,
    pub hosts: Vec<String>,
}

// The hostnames only — the cheap answer to "what is in this source", for a UI
// picker or an operator. Without it, the alternatives were the full dataset or
// passing a deliberately non-existent ?fields= value to empty out the vars.
#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/hosts",
    tag = "Sources",
    params(("id" = String, Path, description = "Source identifier")),
    responses(
        (status = 200, description = "Hostnames in the cached dataset, sorted", body = HostList),
        (status = 403, description = "API key not allowed to read this source", body = ErrorBody),
        (status = 404, description = "Source not in cache", body = ErrorBody)
    )
)]
pub async fn list_source_hosts(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<HostList>, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    if let Some(view) = state.views.get(&id) {
        return Ok(Json(views::hosts(&state, &id, view)));
    }
    let entry = state
        .cache
        .get(&id)
        .ok_or_else(|| ApiError::source_not_cached(&id))?;

    let mut hosts: Vec<String> = entry.dataset.hostvars.keys().cloned().collect();
    hosts.sort();

    Ok(Json(HostList {
        source_id: id,
        total_hosts: hosts.len(),
        hosts,
    }))
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
    /// Hosts in the source, whatever the filter selected
    pub total_hosts: usize,
    /// Hosts described in this response
    pub returned: usize,
    /// Absent until the source has been synced at least once through this
    /// process, and always absent for a view — a view never syncs, so its
    /// members carry the health instead
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
    pub hosts: Vec<HostStatus>,
    /// Views only: the state of each member, in declared order. This is where
    /// "why is this stale" is answerable for a view
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<crate::adapters::r#in::http::views::ViewMemberStatus>>,
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
        (status = 200, description = "Cache status per host with TTL info. On a view, each host's age and TTL come from the member that owns it, and `members` reports the state of every member", body = SourceStatus),
        (status = 403, description = "API key not allowed to read this source", body = ErrorBody),
        (status = 404, description = "Source not in cache. An unmatched host/group filter is an empty result, not a 404", body = ErrorBody)
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
    if let Some(view) = state.views.get(&id) {
        return Ok(Json(views::status(&state, &id, view, params)?));
    }
    let entry = state
        .cache
        .get(&id)
        .ok_or_else(|| ApiError::source_not_cached(&id))?;
    let source = state.sources.get(&id);

    let hostnames = resolve_hostnames(&entry, params.host.as_deref(), params.group.as_deref());

    // Group overrides resolved into a per-host map ONCE, rather than scanning
    // every group's member list for every host, which made a full status
    // quadratic on large sources. Shared with the on-demand refresh, which has
    // to reach the same verdict about the same host.
    let group_ttl_by_host: HashMap<&str, u64> = source
        .map(|s| s.group_ttl_by_host(&entry.dataset))
        .unwrap_or_default();

    let hosts: Vec<HostStatus> = hostnames
        .iter()
        .filter_map(|hostname| {
            // The helper borrows from the entry, so this is a &&String
            let hostname = hostname.as_str();
            let age = entry.host_age_seconds(hostname)?;

            let effective_ttl = source
                .map(|s| s.effective_ttl(hostname, &group_ttl_by_host, entry.ttl.as_secs()))
                .unwrap_or(entry.ttl.as_secs());

            Some(HostStatus {
                hostname: hostname.to_string(),
                age_seconds: age,
                is_fresh: entry.is_host_fresh(hostname, Some(effective_ttl)),
                ttl_seconds: effective_ttl,
            })
        })
        .collect();

    // Read before `id` is moved into the response. No sort here: the shared
    // filter helper already returns the hostnames in order.
    let sync_health = state.sync_health.get(&id).map(Into::into);

    Ok(Json(SourceStatus {
        source_id: id,
        dataset_age_seconds: entry.age_seconds(),
        dataset_is_fresh: entry.is_fresh(),
        ttl_seconds: entry.ttl.as_secs(),
        // The source's host count, not the filtered one: `?host=motoko` used
        // to answer total_hosts: 1, which reads as "this source has one host".
        // How many are in THIS response is `returned`, mirroring the dataset
        // envelope's total_hosts/returned pair.
        total_hosts: entry.dataset.hostvars.len(),
        returned: hosts.len(),
        sync_health,
        hosts,
        // Sources have no members; only a view fills this in
        members: None,
    }))
}
