use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use serde::Serialize;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::error::ApiError;
use crate::adapters::r#in::http::sources::{
    CachedSourceInfo, DatasetParams, GroupInfo, HostList, HostStatus, SourceStatus, StatusParams,
    SyncHealthInfo,
};
use crate::application::refresh::{RefreshOutcome, refresh_hosts};
use crate::application::views::{MergedGroup, UnclaimedHosts, snapshot};
use crate::domain::dataset::HostVars;
use crate::domain::view::View;

// Serving a view on the source read routes.
//
// There is no `#[utoipa::path]` here and no route of its own: a view answers at
// `/api/v1/sources/{id}/...`, in the same shapes, which is the entire point.
// A consumer migrating from a member to the view changes one id and its parsing
// is untouched. sources.rs dispatches here when the id names a view.
//
// The permission check happens in sources.rs before the dispatch, against the
// VIEW's id — a key granted the view needs no grant on the members, because the
// members are internal topology and the view is the contract.

// What the members look like right now. Reported by /status because the two
// ways a view can answer "nothing" need different fixes: a member that has
// never synced (no data to serve) and an ownership source that has never synced
// (no routing table, so nothing is claimed at all).
#[derive(Serialize, ToSchema)]
pub struct ViewMemberStatus {
    pub source_id: String,
    /// Whether this member's data is in the cache
    pub cached: bool,
    /// Whether the source this member's ownership resolves against is cached.
    /// false = its group patterns cannot be expanded, so it claims nothing
    /// beyond the hosts named literally in the config.
    pub ownership_cached: bool,
    /// Absent while the member has never synced
    pub age_seconds: Option<u64>,
    pub is_fresh: bool,
    /// The TTL that governs this member's hosts under this view — the view's
    /// declared `ttl_seconds` if it has one, otherwise the member's own
    pub ttl_seconds: u64,
    pub total_hosts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
}

// GET /api/v1/sources/{id}/dataset for a view.
pub async fn dataset(
    state: &Arc<AppState>,
    id: &str,
    view: &View,
    params: DatasetParams,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    // The refresh runs first and against a snapshot of its own: it writes into
    // a member's cache, so the snapshot that serves the response has to be
    // taken afterwards or it would answer with the data the refresh replaced.
    let refresh = if params.refresh.unwrap_or(false) {
        Some(refresh_before_reading(state, id, view, params.host.as_deref()).await?)
    } else {
        None
    };

    let snap = snapshot(&*state.cache, &state.sources, id, view);

    // One validator for both the plain and the filtered shape, unlike a source,
    // whose plain path can derive a strong ETag from bytes it has already
    // serialized. A view serializes on the fly, so the validator is the cache
    // generation (bumped by every write anywhere) plus the query. Pessimistic —
    // an unrelated source's sync invalidates it — but never stale, and it costs
    // one integer read instead of hashing the merged dataset.
    let etag = {
        let mut hasher = DefaultHasher::new();
        id.hash(&mut hasher);
        params.host.hash(&mut hasher);
        params.group.hash(&mut hasher);
        params.limit.hash(&mut hasher);
        params.offset.hash(&mut hasher);
        params.fields.hash(&mut hasher);
        params.refresh.hash(&mut hasher);
        format!("\"{:x}-{}\"", hasher.finish(), state.cache.generation())
    };

    if if_none_match(&headers, &etag) {
        return Ok(with_refresh_headers(
            Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &etag),
            &refresh,
        )
        .body(axum::body::Body::empty())
        .unwrap());
    }

    let selection = snap
        .select(params.host.as_deref(), params.group.as_deref())
        .map_err(|unclaimed| unclaimed_error(id, view, unclaimed))?;

    // No params = the raw Dataset shape, exactly as a source returns it.
    let body = if params.is_plain() {
        serde_json::to_vec(&snap.dataset(&selection))
    } else {
        let total_hosts = selection.len();
        let offset = params.offset.unwrap_or(0);
        let page: Vec<&str> = selection
            .into_iter()
            .skip(offset)
            .take(params.limit.unwrap_or(usize::MAX))
            .collect();

        let field_set: Option<std::collections::HashSet<&str>> = params
            .fields
            .as_ref()
            .map(|f| f.split(',').map(str::trim).collect());

        let hostvars: HashMap<&str, Cow<HostVars>> = snap
            .hostvars(&page)
            .into_iter()
            .map(|(host, vars)| {
                let vars = match &field_set {
                    Some(fields) => Cow::Owned(
                        vars.iter()
                            .filter(|(key, _)| fields.contains(key.as_str()))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    ),
                    None => Cow::Borrowed(vars),
                };
                (host, vars)
            })
            .collect();

        let mut groups = snap.groups();
        if let Some(ref wanted) = params.group {
            groups.retain(|name, _| *name == wanted.as_str());
        }

        serde_json::to_vec(&ViewDatasetPage {
            source_id: id,
            total_hosts,
            offset,
            limit: params.limit,
            returned: hostvars.len(),
            hostvars,
            groups,
        })
    }
    .map_err(|e| ApiError::internal(format!("failed to serialize view dataset: {}", e)))?;

    Ok(with_refresh_headers(
        Response::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ETAG, &etag),
        &refresh,
    )
    .body(axum::body::Body::from(body))
    .unwrap())
}

// GET /api/v1/sources/{id}/status for a view.
pub fn status(
    state: &Arc<AppState>,
    id: &str,
    view: &View,
    params: StatusParams,
) -> Result<SourceStatus, ApiError> {
    let snap = snapshot(&*state.cache, &state.sources, id, view);

    let selection = snap
        .select(params.host.as_deref(), params.group.as_deref())
        .map_err(|unclaimed| unclaimed_error(id, view, unclaimed))?;

    // Group overrides resolved once per member, not once per host — the same
    // reason the source path does it (a full status was quadratic otherwise).
    let group_ttls: Vec<HashMap<&str, u64>> = snap
        .members
        .iter()
        .map(|member| member.group_ttls())
        .collect();

    let hosts: Vec<HostStatus> = selection
        .iter()
        .filter_map(|hostname| {
            let index = snap
                .members
                .iter()
                .position(|member| member.claims(hostname))?;
            let owner = &snap.members[index];
            let entry = owner.entry.as_ref()?;
            let age = entry.host_age_seconds(hostname)?;
            let ttl = owner.effective_ttl(view, hostname, &group_ttls[index]);

            Some(HostStatus {
                hostname: hostname.to_string(),
                age_seconds: age,
                is_fresh: entry.is_host_fresh(hostname, Some(ttl)),
                ttl_seconds: ttl,
            })
        })
        .collect();

    let members: Vec<ViewMemberStatus> = snap
        .members
        .iter()
        .map(|member| ViewMemberStatus {
            source_id: member.source_id.to_string(),
            cached: member.entry.is_some(),
            ownership_cached: member.ownership_cached(),
            age_seconds: member.entry.as_ref().map(|entry| entry.age_seconds()),
            is_fresh: member.entry.as_ref().is_some_and(|entry| entry.is_fresh()),
            ttl_seconds: member.default_ttl(view),
            total_hosts: member
                .entry
                .as_ref()
                .map(|entry| entry.dataset.hostvars.len())
                .unwrap_or(0),
            sync_health: state.sync_health.get(member.source_id).map(Into::into),
        })
        .collect();

    Ok(SourceStatus {
        source_id: id.to_string(),
        dataset_age_seconds: snap.age_seconds(),
        dataset_is_fresh: snap.is_fresh(),
        ttl_seconds: snap.ttl_seconds(),
        total_hosts: snap.hosts().len(),
        returned: hosts.len(),
        // A view never syncs, so it has no sync health of its own. Each
        // member's is in `members` below, which is where the answer to "why is
        // this stale" actually lives.
        sync_health: None,
        hosts,
        members: Some(members),
    })
}

// GET /api/v1/sources/{id}/groups for a view.
pub fn groups(state: &Arc<AppState>, id: &str, view: &View) -> Vec<GroupInfo> {
    let snap = snapshot(&*state.cache, &state.sources, id, view);

    // Already sorted: the merged namespace is a BTreeMap
    snap.groups()
        .into_iter()
        .map(|(name, group)| GroupInfo {
            name: name.to_string(),
            // The merge already deduplicated across members
            host_count: group.hosts.len(),
            children: group.children.iter().map(|c| c.to_string()).collect(),
            has_vars: group.vars.is_some(),
        })
        .collect()
}

// GET /api/v1/sources/{id}/hosts for a view.
pub fn hosts(state: &Arc<AppState>, id: &str, view: &View) -> HostList {
    let snap = snapshot(&*state.cache, &state.sources, id, view);
    let hosts: Vec<String> = snap.hosts().into_iter().map(str::to_string).collect();

    HostList {
        source_id: id.to_string(),
        total_hosts: hosts.len(),
        hosts,
    }
}

// One line of GET /api/v1/sources.
//
// A configured view is listed whether or not any member has synced, unlike a
// source, which appears once it is in the cache. A view is a contract rather
// than an entry: it is listed so a consumer can discover it, and `is_fresh` /
// `age_seconds` say what state it is in.
pub fn info(state: &Arc<AppState>, id: &str, view: &View) -> CachedSourceInfo {
    let snap = snapshot(&*state.cache, &state.sources, id, view);

    CachedSourceInfo {
        source_id: id.to_string(),
        kind: "view",
        is_fresh: snap.is_fresh(),
        age_seconds: snap.age_seconds(),
        total_hosts: snap.hosts().len(),
        // Nothing syncs a view. The members' health is on the view's /status.
        sync_health: None,
    }
}

// Every write route's answer for a view. A view gathers nothing, holds no cache
// entry and owns no host's data, so sync, eviction and host writes have no
// meaning on it — and inventing one (syncing every member, evicting all their
// entries) would let a request aimed at one consumer's view quietly re-gather
// somebody else's datacenter.
pub fn write_refused(id: &str, view: &View, action: &str) -> ApiError {
    ApiError::bad_request(format!(
        "'{}' is a view, not a source: it composes {} at read time and holds no cache \
         entry, so it cannot {}. Address the member source directly.",
        id,
        view.member_ids().join(", "),
        action
    ))
}

fn unclaimed_error(id: &str, view: &View, unclaimed: UnclaimedHosts) -> ApiError {
    metrics::counter!("unified_api_view_unclaimed_hosts_total", "view" => id.to_string())
        .increment(unclaimed.0.len() as u64);

    ApiError::not_found(format!(
        "no member of view '{}' claims {}. Members are {}; a host is routed by the \
         ownership declared in the view, so either its group is not listed or the \
         inventory source that ownership resolves against has not synced yet.",
        id,
        unclaimed.0.join(", "),
        view.member_ids().join(", ")
    ))
}

// Delegate the refresh to whoever owns each named host.
//
// Validation happens for EVERY routed member before any gather starts: a
// request naming hosts across two members, one of which does not allow
// on-demand refresh, is refused whole rather than half-refreshed.
async fn refresh_before_reading(
    state: &Arc<AppState>,
    view_id: &str,
    view: &View,
    host: Option<&str>,
) -> Result<RefreshOutcome, ApiError> {
    let hosts: Vec<String> = host
        .map(|h| {
            h.split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    if hosts.is_empty() {
        return Err(ApiError::refresh_needs_hosts());
    }

    let snap = snapshot(&*state.cache, &state.sources, view_id, view);
    let routed = snap
        .route(&hosts)
        .map_err(|unclaimed| unclaimed_error(view_id, view, unclaimed))?;

    for (member, _) in &routed {
        let source = member
            .source
            .ok_or_else(|| ApiError::source_not_configured(member.source_id))?;
        if !source.allow_on_demand_refresh {
            return Err(ApiError::forbidden(format!(
                "view '{}' routes {} to source '{}', which does not allow on-demand \
                 refresh — set allow_on_demand_refresh: true on it to let a read \
                 trigger a gather",
                view_id,
                hosts.join(", "),
                member.source_id
            )));
        }
    }

    let mut merged = RefreshOutcome::default();
    let mut errors: Vec<String> = Vec::new();

    for (member, member_hosts) in routed {
        let source = member.source.expect("validated above");
        let connector = state.connector_for(&source.connector_type);

        let outcome = refresh_hosts(
            &*state.cache,
            &**connector,
            &*state.secrets,
            &state.sync_health,
            &state.refresh,
            member.source_id,
            source,
            &member_hosts,
            // The view's TTL is the refresh GATE, not just a freshness label:
            // refresh_hosts only gathers hosts older than it. Passing None
            // leaves the member's own TTL governing, which is what "inherit"
            // has to mean for the answer to stay predictable.
            view.ttl_seconds,
        )
        .await;

        merged.refreshed.extend(outcome.refreshed);
        merged.already_fresh += outcome.already_fresh;
        if let Some(error) = outcome.error {
            errors.push(format!("{}: {}", member.source_id, error));
        }
    }

    if !errors.is_empty() {
        merged.error = Some(errors.join("; "));
    }
    Ok(merged)
}

// Same headers, same meaning as the source path (sources.rs owns the constants;
// these are the same names, kept here so the two files do not have to expose
// each other's internals).
const HEADER_REFRESHED: &str = "x-unified-api-refreshed";
const HEADER_REFRESH_ERROR: &str = "x-unified-api-refresh-error";
const HEADER_REFRESHED_HOSTS: &str = "x-unified-api-refreshed-hosts";

fn with_refresh_headers(
    mut builder: axum::http::response::Builder,
    refresh: &Option<RefreshOutcome>,
) -> axum::http::response::Builder {
    if let Some(outcome) = refresh {
        builder = builder.header(HEADER_REFRESHED, (outcome.error.is_none()).to_string());
        if !outcome.refreshed.is_empty() {
            builder = builder.header(HEADER_REFRESHED_HOSTS, outcome.refreshed.join(","));
        }
        if let Some(error) = &outcome.error {
            let sanitized: String = error
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            builder = builder.header(HEADER_REFRESH_ERROR, sanitized);
        }
    }
    builder
}

fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                candidate == etag || candidate == "*"
            })
        })
}

// The paginated envelope, field for field the one a source returns. Borrowing,
// like its counterpart: a group filter can select most of a large view.
#[derive(Serialize)]
struct ViewDatasetPage<'a> {
    source_id: &'a str,
    total_hosts: usize,
    offset: usize,
    limit: Option<usize>,
    returned: usize,
    hostvars: HashMap<&'a str, Cow<'a, HostVars>>,
    groups: BTreeMap<&'a str, MergedGroup<'a>>,
}
