use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::domain::connector::{self, Connector};
use crate::state::RuntimeConfig;

// Connectors: HOW this instance talks to each kind of system, and which
// sources use each one. Admin-only, like projects: a connector names the
// script, its project and its arguments for every source of that system,
// which is deploy tooling rather than consumer data.
//
// The two built-ins (`remote`, `static_inventory`) are listed beside the
// connector items so a source's `connector:` always resolves to a row here.

#[derive(Serialize, ToSchema)]
pub struct ConnectorInfo {
    /// The connector's identity: what a source writes after `connector:`
    pub connector_id: String,
    /// Its description, or the identity when it has none
    pub name: String,
    /// "script", "ssh", "remote" or "static_inventory"
    pub connector_type: String,
    /// One of the connectors every instance has, which no item defines
    pub built_in: bool,
    /// The project whose checkout holds the script. Null for a script on
    /// this instance's disk, for an ssh path on the hosts, and for the
    /// built-ins (a static_inventory source names its own project)
    pub project_id: Option<String>,
    /// ssh only: where `path` is. "host": on each host, a file or a
    /// pattern of files, each run if executable or read if not, as facts;
    /// "instance": a script here, streamed to each host. Null for a
    /// connector that writes its `script` inline, and for every other kind
    pub location: Option<String>,
    /// The file it runs or reads. Null for the built-ins and for a
    /// connector with an inline `script`
    pub path: Option<String>,
    /// ssh only: the script written in the item, streamed to each host and
    /// run there; null when it names a `path`
    pub script: Option<String>,
    pub script_args: Vec<String>,
    pub host_args: Vec<String>,
    /// "native" or "ansible" for a script connector; null otherwise
    pub output_format: Option<String>,
    /// The connector's own bound on a sync; a source may override it. Null for
    /// the built-ins, whose sources fall back to 300
    pub timeout_seconds: Option<u64>,
    /// The sources that name this connector, by id
    pub used_by: Vec<ConnectorUser>,
    /// Those sources' sync health, counted
    pub health: ConnectorHealth,
}

#[derive(Serialize, ToSchema)]
pub struct ConnectorUser {
    pub source_id: String,
    pub name: String,
    /// The timeout this source actually syncs under (its override, or the
    /// connector's)
    pub timeout_seconds: u64,
    /// Whether the last sync of this source failed
    pub failing: bool,
    /// Whether it has synced successfully at least once in this process
    pub synced: bool,
}

/// What a sync-health summary across a connector's sources says at a glance.
#[derive(Serialize, ToSchema, Default)]
pub struct ConnectorHealth {
    pub sources: usize,
    /// Last attempt succeeded
    pub healthy: usize,
    /// Last attempt failed (consecutive_failures > 0)
    pub failing: usize,
    /// No attempt recorded yet in this process
    pub never_synced: usize,
}

fn info(
    state: &AppState,
    config: &RuntimeConfig,
    id: &str,
    connector: Option<&Connector>,
) -> ConnectorInfo {
    let mut used_by: Vec<ConnectorUser> = config
        .sources
        .iter()
        .filter(|(_, s)| s.connector == id)
        .map(|(source_id, s)| {
            let health = state.sync_health.get(source_id);
            ConnectorUser {
                source_id: source_id.clone(),
                name: s.name.clone(),
                timeout_seconds: s.timeout_seconds,
                failing: health.as_ref().is_some_and(|h| h.consecutive_failures > 0),
                synced: health
                    .as_ref()
                    .is_some_and(|h| h.last_success_age_seconds.is_some()),
            }
        })
        .collect();
    used_by.sort_by(|a, b| a.source_id.cmp(&b.source_id));

    let mut health = ConnectorHealth {
        sources: used_by.len(),
        ..ConnectorHealth::default()
    };
    for user in &used_by {
        match state.sync_health.get(&user.source_id) {
            None => health.never_synced += 1,
            Some(h) if h.consecutive_failures > 0 => health.failing += 1,
            Some(_) => health.healthy += 1,
        }
    }

    match connector {
        Some(c) => ConnectorInfo {
            connector_id: id.to_string(),
            name: c.name.clone(),
            connector_type: c.kind.name().to_string(),
            built_in: false,
            project_id: c.project.clone(),
            location: c.ssh_location().map(|l| l.name().to_string()),
            path: (!c.path.is_empty()).then(|| c.path.clone()),
            script: c.script.clone(),
            script_args: c.script_args.clone(),
            host_args: c.host_args.clone(),
            output_format: match c.kind {
                connector::ConnectorKind::Script => {
                    Some(c.output_format.unwrap_or_default().name().to_string())
                }
                connector::ConnectorKind::Ssh => None,
            },
            timeout_seconds: Some(c.timeout_seconds),
            used_by,
            health,
        },
        None => ConnectorInfo {
            connector_id: id.to_string(),
            name: match id {
                connector::REMOTE => "Another unified-api instance".to_string(),
                _ => "Ansible YAML inventory in a project checkout".to_string(),
            },
            connector_type: id.to_string(),
            built_in: true,
            project_id: None,
            location: None,
            path: None,
            script: None,
            script_args: Vec::new(),
            host_args: Vec::new(),
            output_format: None,
            timeout_seconds: None,
            used_by,
            health,
        },
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/connectors",
    tag = "Connectors",
    responses(
        (status = 200, description = "Every connector (the configured ones and the two built-ins), what it runs, the sources that use it and their health", body = Vec<ConnectorInfo>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_connectors(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<ConnectorInfo>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let mut out: Vec<ConnectorInfo> = config
        .connectors
        .iter()
        .map(|(id, c)| info(&state, &config, id, Some(c)))
        .collect();
    out.sort_by(|a, b| a.connector_id.cmp(&b.connector_id));
    // Built-ins last: they are the same on every instance, and what an
    // operator came to look at is usually one of their own.
    for id in connector::BUILT_IN {
        out.push(info(&state, &config, id, None));
    }
    Ok(Json(out))
}

#[utoipa::path(
    get,
    path = "/api/v1/connectors/{id}",
    tag = "Connectors",
    params(("id" = String, Path, description = "Connector identifier, e.g. d42, or a built-in: remote, static_inventory")),
    responses(
        (status = 200, description = "One connector, what it runs, the sources that use it and their health", body = ConnectorInfo),
        (status = 403, description = "API key is not admin", body = ErrorBody),
        (status = 404, description = "No such connector", body = ErrorBody)
    )
)]
pub async fn get_connector(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ConnectorInfo>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    if let Some(c) = config.connectors.get(&id) {
        return Ok(Json(info(&state, &config, &id, Some(c))));
    }
    if connector::BUILT_IN.contains(&id.as_str()) {
        return Ok(Json(info(&state, &config, &id, None)));
    }
    Err(ApiError::not_found(format!(
        "connector '{id}' is not configured"
    )))
}
