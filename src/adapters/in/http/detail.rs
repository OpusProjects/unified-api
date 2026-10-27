use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::adapters::r#in::http::index;
use crate::adapters::r#in::http::sources::{RunInfo, SyncHealthInfo};
use crate::adapters::r#in::http::views;
use crate::domain::source::ConnectorType;
use crate::domain::sync_mode::SyncMode;

// One source, as a resource.
//
// The id had no page of its own: `/api/v1/sources/{id}` answered only DELETE,
// so the obvious click — from the list, or from the breadcrumb trail — landed
// on a 405. Everything a source IS lived either in the list (a summary line)
// or in its sub-resources, and the object in the middle was missing.
//
// It is also the page the action buttons belong on, the way AWX puts them on
// an Inventory Detail rather than on the list.

#[derive(Serialize, ToSchema)]
pub struct SourceDetail {
    pub source_id: String,
    /// This object's own address
    pub url: String,
    pub name: String,
    /// "source" or "view"
    pub kind: &'static str,
    /// Whether there is data behind this id right now
    pub cached: bool,
    /// Whether its data is copied from another unified-api (the remote
    /// connector), for every caller: see the same field on the list
    pub federated: bool,
    pub is_fresh: bool,
    /// Null when nothing is cached — never zero, which would read as "synced
    /// just now"
    pub age_seconds: Option<u64>,
    pub ttl_seconds: u64,
    pub total_hosts: Option<usize>,
    pub total_groups: Option<usize>,
    /// How big the full `/dataset` response is, in bytes — the difference
    /// between a call you can make in a loop and one that wants `?limit=`.
    ///
    /// Reported only when the entry already holds its serialized JSON, which it
    /// does from the first read of `/dataset` until the next sync replaces it.
    /// Null otherwise, including when nothing is cached: building the buffer
    /// just to measure it would make opening a page cost a full serialization.
    pub dataset_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
    /// Its last syncs, newest first (up to 50, kept in memory since this
    /// process started): when, why, what each covered and how it ended.
    /// Empty for a view, which syncs nothing.
    pub history: Vec<RunInfo>,

    /// How this source gathers. Absent for a view, which gathers nothing, and
    /// absent for a non-admin key: it names the git project, the script path
    /// inside it and the credential ids in play, none of which a restricted
    /// key could read before this route existed. The freshness and health half
    /// of the page stays available to everyone who may read the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gathering: Option<Gathering>,

    /// The members a view routes to, and their state. Views only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<views::ViewMemberStatus>>,

    pub related: index::Related,
}

#[derive(Serialize, ToSchema)]
pub struct Gathering {
    /// The connector this source names: a connector item
    /// (`/api/v1/connectors/{id}`) or a built-in (`remote`, `static_inventory`)
    pub connector: String,
    pub connector_type: ConnectorType,
    /// The project whose checkout holds the script (from the connector) or
    /// the inventory (a static_inventory source's own). Null for remote
    /// sources, and for ssh sources whose path is on the hosts or whose
    /// script is inline
    pub project_id: Option<String>,
    /// The file the connector runs or reads. Empty for an ssh connector
    /// whose script is inline (its text is on the admin-only connectors API,
    /// not here: this page is open to restricted keys)
    pub script_path: String,
    /// ssh only: where `script_path` is, "host" or "instance"; null for an
    /// inline script and for every other connector
    pub location: Option<String>,
    /// ssh only: the connector runs a script written in its own item
    pub inline_script: bool,
    pub sync_mode: SyncMode,
    pub timeout_seconds: u64,
    /// Seconds between periodic syncs; null when only a cron schedule or
    /// nothing at all drives this source
    pub sync_interval_seconds: Option<u64>,
    /// Cron expression (UTC), the alternative to an interval
    pub schedule: Option<String>,
    /// Credential ids this connector is given — names, never secrets
    pub credential_ids: Vec<String>,
    pub allow_on_demand_refresh: bool,
    /// For a remote source, the instance it pulls from (its `config.url`).
    /// The one config value shown: it is an address, not a secret, and
    /// `GET /topology` already names it to every caller who may read the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    /// The KEYS of the connector's free-form `config:` map, without their
    /// values. A restricted key may read this page for a source it is granted,
    /// while the configuration API that holds the values is admin-only — so the
    /// page says what is parameterised without widening who can read it.
    pub config_keys: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}",
    tag = "Sources",
    params(("id" = String, Path, description = "Source or view identifier")),
    responses(
        (status = 200, description = "One source (or view) with its freshness, health, gathering configuration and links", body = SourceDetail),
        (status = 403, description = "API key is not allowed this source", body = ErrorBody),
        (status = 404, description = "No such source or view is configured", body = ErrorBody)
    )
)]
pub async fn get_source(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<SourceDetail>, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }

    let config = state.config();

    if let Some(view) = config.views.get(&id) {
        return Ok(Json(views::detail(&state, &id, view)));
    }

    let Some(source) = config.sources.get(&id) else {
        return Err(ApiError::source_not_configured(&id));
    };

    // A configured source that has never synced still has a page: it is
    // configured, it has a schedule, and `sync_health` is where the reason it
    // has no data is written down.
    let entry = state.cache.get(&id);
    // An ssh connector that names no location runs its own inline script,
    // handed over as script_path.
    let inline = source.connector_type == crate::domain::source::ConnectorType::Ssh
        && source.ssh_location.is_none();

    Ok(Json(SourceDetail {
        url: format!("/api/v1/sources/{id}"),
        source_id: id.clone(),
        name: source.name.clone(),
        kind: "source",
        cached: entry.is_some(),
        federated: source.connector_type == crate::domain::source::ConnectorType::Remote,
        is_fresh: entry.as_ref().is_some_and(|e| e.is_fresh()),
        age_seconds: entry.as_ref().map(|e| e.age_seconds()),
        ttl_seconds: source.ttl_seconds,
        total_hosts: entry.as_ref().map(|e| e.dataset.hostvars.len()),
        total_groups: entry.as_ref().map(|e| e.dataset.groups.len()),
        dataset_bytes: entry.as_ref().and_then(|e| e.serialized_len()),
        sync_health: state.sync_health.get(&id).map(Into::into),
        history: state
            .sync_health
            .history(&id)
            .into_iter()
            .map(|run| RunInfo::from_run(run, auth.permissions.is_admin()))
            .collect(),
        gathering: auth.permissions.is_admin().then(|| Gathering {
            connector: source.connector.clone(),
            connector_type: source.connector_type.clone(),
            project_id: source.project_id.clone(),
            script_path: if inline {
                String::new()
            } else {
                source.script_path.clone()
            },
            location: source.ssh_location.map(|l| l.name().to_string()),
            inline_script: inline,
            sync_mode: source.sync_mode.clone(),
            timeout_seconds: source.timeout_seconds,
            sync_interval_seconds: source.sync_interval_seconds,
            schedule: source.schedule.clone(),
            credential_ids: source.credential_ids.clone(),
            allow_on_demand_refresh: source.allow_on_demand_refresh,
            remote_url: crate::domain::topology::remote_url(source),
            config_keys: {
                let mut keys: Vec<String> = source.config.keys().cloned().collect();
                keys.sort();
                keys
            },
        }),
        members: None,
        related: index::source_related(&id),
    }))
}
