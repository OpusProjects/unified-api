use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Serialize;
use utoipa::ToSchema;
use web_rs::config::api as shared;

use crate::AppState;
use crate::application::config as reload_use_case;
use crate::config::AppConfig;
use crate::ports::config_store::ConfigStorePort;

use super::auth::{ApiKeys, AuthContext, Permissions, ResolvedApiKey, resolve_api_keys};
use super::error::{ApiError, ErrorBody};

pub use web_rs::config::{
    ConfigBundle, ConfigFileInfo, ConfigInventory, ConfigRejected, DeltaInfo, ReloadQuery,
    ValidationResult, WriteResult,
};

// The configuration directory, over HTTP.
//
// This exists so a configuration-as-code pipeline can PUSH to an instance
// instead of building an image the instance has to PULL. The unit of work is
// the same one the pipeline already has — whole YAML files — and the contract
// is the one it already relies on: a change is validated exactly as
// `--check-config` validates it, as a DIRECTORY, and is refused whole if it
// does not load. Nothing lands half-applied, and an instance that refuses a
// push keeps serving what it had. Stale beats broken, in both directions.
//
// Admin-only, every route, reads included: config.yaml and credentials.yaml
// describe the estate — which systems exist, which variable holds which
// credential — which is exactly what a restricted consumer key has no
// business enumerating.
//
// The routes' logic is web-rs's (the `web` repository), shared with the
// sibling service: staging, If-Match, ETags, the order validate → commit →
// reload. What is this service's own is below: what its configuration counts,
// what a reload reports and does, and which API keys a configuration leaves
// in force. The handlers carry the OpenAPI annotations and hand each request
// to the shared function for its route.

// ---------------------------------------------------------------- wire types

#[derive(Serialize, ToSchema)]
pub struct ConfigSummary {
    pub sources: usize,
    pub connectors: usize,
    pub hosts: usize,
    pub groups: usize,
    pub views: usize,
    pub credentials: usize,
    pub enrichers: usize,
    pub endpoints: usize,
    pub projects: usize,
    pub api_keys: usize,
}

impl From<&reload_use_case::SectionDelta> for DeltaInfo {
    fn from(delta: &reload_use_case::SectionDelta) -> Self {
        Self {
            added: delta.added.clone(),
            removed: delta.removed.clone(),
            changed: delta.changed.clone(),
        }
    }
}

#[derive(Serialize, ToSchema)]
pub struct ReloadInfo {
    pub generation: u64,
    /// Sections that changed and are now live
    pub applied: Vec<String>,
    /// config.yaml keys this process cannot adopt without a restart. Present
    /// means the write landed and part of it is waiting for one
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub restart_required: Vec<String>,
    pub sources: DeltaInfo,
    pub connectors: DeltaInfo,
    pub views: DeltaInfo,
    pub enrichers: DeltaInfo,
    pub endpoints: DeltaInfo,
    pub projects: DeltaInfo,
    pub credentials: DeltaInfo,
    /// How many API keys are in force after the reload
    pub api_keys: usize,
}

// ------------------------------------------------------------- this service

// Swap the API keys in force, and end the browser sessions of any key that no
// longer exists.
//
// The two belong together. A reload is how a key is revoked, and a session is
// a handle on a key: without this, removing a key from api_keys.yaml and
// reloading left every browser already logged in with it working for the rest
// of its TTL — the console outliving the revocation, which is the one thing a
// revocation is for.
fn swap_keys(state: &AppState, keys: &ApiKeys, resolved: Vec<ResolvedApiKey>) -> usize {
    // By identity: a key's label (its description) need not be unique.
    let live: Vec<String> = resolved.iter().map(|key| key.id.clone()).collect();
    let count = resolved.len();
    keys.0.replace(resolved);
    state.sessions.retain_keys(&live);
    count
}

fn summarize(cfg: &AppConfig) -> ConfigSummary {
    ConfigSummary {
        sources: cfg.sources.len(),
        connectors: cfg.connectors.len(),
        hosts: cfg.hosts.len(),
        groups: cfg.groups.len(),
        views: cfg.views.len(),
        credentials: cfg.credentials.len(),
        enrichers: cfg.enrichers.len(),
        endpoints: cfg.endpoints.len(),
        projects: cfg.projects.len(),
        api_keys: cfg.api_keys.len(),
    }
}

fn reload_info(report: &reload_use_case::ReloadReport, api_keys: usize) -> ReloadInfo {
    ReloadInfo {
        generation: report.generation,
        applied: report.applied.clone(),
        restart_required: report.restart_required.clone(),
        sources: (&report.sources).into(),
        connectors: (&report.connectors).into(),
        views: (&report.views).into(),
        enrichers: (&report.enrichers).into(),
        endpoints: (&report.endpoints).into(),
        projects: (&report.projects).into(),
        credentials: (&report.credentials).into(),
        api_keys,
    }
}

// Everything a reload has to be sure of BEFORE anything is committed.
//
// The API keys are resolved here, against the configuration being proposed,
// for two reasons: a key whose env var is missing must fail the whole
// operation rather than lock a consumer out after the files have landed, and
// a reload must not be able to turn authentication OFF on a process that had
// it. A directory with no keys is a legitimate configuration — it is how a
// fresh instance starts — but arriving at it under a running process, over an
// authenticated request, is indistinguishable from an accident and would
// leave the API open to anyone who can reach the port.
fn keys_for(
    cfg: &AppConfig,
    current: &ApiKeys,
) -> Result<Vec<super::auth::ResolvedApiKey>, ApiError> {
    let keys = resolve_api_keys(cfg).map_err(|e| {
        ApiError::new(
            StatusCode::CONFLICT,
            format!("the configuration cannot be applied: {}", e),
        )
    })?;

    super::auth::ldap_requires_keys(cfg, &keys)
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, format!("refusing to reload: {e}")))?;

    if keys.is_empty() && !current.0.is_empty() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "refusing to reload: this configuration declares no API keys, which \
             would leave the running API without authentication. Write the file \
             and restart the instance if that is really the intent",
        ));
    }

    Ok(keys)
}

// What the shared config routes need from this service, for one request: the
// state, and the API keys in force (an Extension, so a reload can swap them).
struct Host<'a> {
    state: &'a Arc<AppState>,
    keys: &'a ApiKeys,
}

impl web_rs::config::ConfigHost<Permissions> for Host<'_> {
    type Config = AppConfig;
    type Summary = ConfigSummary;
    type Prepared = Vec<ResolvedApiKey>;
    type Report = ReloadInfo;

    fn store(&self) -> Option<&Arc<ConfigStorePort>> {
        self.state.config_store.as_ref()
    }

    fn is_admin(&self, permissions: &Permissions) -> bool {
        permissions.is_admin()
    }

    fn generation(&self) -> u64 {
        self.state.reload.generation()
    }

    fn summarize(&self, config: &AppConfig) -> ConfigSummary {
        summarize(config)
    }

    fn pending(&self, config: &AppConfig) -> web_rs::config::Pending {
        let pending = reload_use_case::pending(self.state, config);
        web_rs::config::Pending {
            changed: pending.changed_anything(),
            restart_required: pending.restart_required,
        }
    }

    fn prepare(&self, config: &AppConfig) -> Result<Vec<ResolvedApiKey>, ApiError> {
        keys_for(config, self.keys)
    }

    fn apply(&self, config: &AppConfig, keys: Vec<ResolvedApiKey>) -> ReloadInfo {
        let report = reload_use_case::apply(self.state, config);
        let key_count = swap_keys(self.state, self.keys, keys);
        reload_info(&report, key_count)
    }

    fn count(&self, metric: web_rs::config::ConfigMetric, outcome: &'static str) {
        match metric {
            web_rs::config::ConfigMetric::Write => {
                metrics::counter!("unified_api_config_writes_total", "outcome" => outcome)
                    .increment(1)
            }
            web_rs::config::ConfigMetric::Reload => {
                metrics::counter!("unified_api_config_reloads_total", "outcome" => outcome)
                    .increment(1)
            }
        }
    }
}

// ------------------------------------------------------------------ handlers

#[utoipa::path(
    get,
    path = "/api/v1/config",
    tag = "Configuration",
    responses(
        (status = 200, description = "The configuration directory: every file with its hash, whether it still loads, and whether the process is running it", body = ConfigInventory),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody)
    )
)]
pub async fn get_config(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
) -> Result<Json<ConfigInventory>, ApiError> {
    shared::get_config(
        &Host {
            state: &state,
            keys: &keys,
        },
        &auth,
    )
}

#[utoipa::path(
    get,
    path = "/api/v1/config/{file}",
    tag = "Configuration",
    params(("file" = String, Path, description = "Path inside the configuration directory, e.g. sources/acme/src-d42-acme.yaml (slashes allowed)")),
    responses(
        (status = 200, description = "The file, verbatim. The ETag is the sha256 of the bytes — send it back as If-Match to write safely", body = String, content_type = "application/yaml"),
        (status = 304, description = "If-None-Match matched: unchanged"),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 404, description = "Not a configuration file, or not present", body = ErrorBody)
    )
)]
pub async fn get_config_file(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    shared::get_config_file(
        &Host {
            state: &state,
            keys: &keys,
        },
        &auth,
        &file,
        &headers,
    )
}

#[utoipa::path(
    put,
    path = "/api/v1/config/{file}",
    tag = "Configuration",
    params(
        ("file" = String, Path, description = "Path inside the configuration directory, e.g. sources/acme/src-d42-acme.yaml (slashes allowed)"),
        ("reload" = Option<bool>, Query, description = "Apply to the running process as part of the write")
    ),
    request_body(content = String, description = "The file's new contents, verbatim YAML", content_type = "application/yaml"),
    responses(
        (status = 200, description = "Written — and applied, if reload was asked for", body = WriteResult<ConfigSummary, ReloadInfo>),
        (status = 400, description = "The directory would not load with this file in it. Nothing was written", body = ConfigRejected),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 404, description = "Not a configuration file", body = ErrorBody),
        (status = 409, description = "Applying it would break the running process (a key's env var is missing, or authentication would be turned off)", body = ErrorBody),
        (status = 412, description = "If-Match did not match the file's current ETag", body = ErrorBody)
    )
)]
pub async fn put_config_file(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    Path(file): Path<String>,
    Query(query): Query<ReloadQuery>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let host = Host {
        state: &state,
        keys: &keys,
    };
    shared::put_config_file(&host, &auth, &file, query.reload, &headers, body)
}

#[utoipa::path(
    delete,
    path = "/api/v1/config/{file}",
    tag = "Configuration",
    params(
        ("file" = String, Path, description = "Path inside the configuration directory, e.g. sources/acme/src-d42-acme.yaml (slashes allowed)"),
        ("reload" = Option<bool>, Query, description = "Apply to the running process as part of the delete")
    ),
    responses(
        (status = 200, description = "Removed — and applied, if reload was asked for", body = WriteResult<ConfigSummary, ReloadInfo>),
        (status = 400, description = "The directory would not load without it, or it is config.yaml. Nothing was removed", body = ConfigRejected),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 404, description = "Not a configuration file, or not present", body = ErrorBody),
        (status = 409, description = "Applying it would break the running process", body = ErrorBody),
        (status = 412, description = "If-Match did not match the file's current ETag", body = ErrorBody)
    )
)]
pub async fn delete_config_file(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    Path(file): Path<String>,
    Query(query): Query<ReloadQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let host = Host {
        state: &state,
        keys: &keys,
    };
    shared::delete_config_file(&host, &auth, &file, query.reload, &headers)
}

#[utoipa::path(
    put,
    path = "/api/v1/config",
    tag = "Configuration",
    params(("reload" = Option<bool>, Query, description = "Apply to the running process as part of the write")),
    request_body = ConfigBundle,
    responses(
        (status = 200, description = "The whole directory was replaced — and applied, if reload was asked for", body = WriteResult<ConfigSummary, ReloadInfo>),
        (status = 400, description = "The pushed directory would not load. Nothing was written", body = ConfigRejected),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 409, description = "Applying it would break the running process", body = ErrorBody),
        (status = 412, description = "If-Match did not match the directory ETag — someone else wrote first", body = ErrorBody)
    )
)]
pub async fn put_config(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    Query(query): Query<ReloadQuery>,
    headers: HeaderMap,
    Json(bundle): Json<ConfigBundle>,
) -> Result<Response, ApiError> {
    let host = Host {
        state: &state,
        keys: &keys,
    };
    shared::put_config(&host, &auth, query.reload, &headers, bundle)
}

#[utoipa::path(
    post,
    path = "/api/v1/config/validate",
    tag = "Configuration",
    request_body(content = ConfigBundle, description = "The directory to validate. Omit the body to validate what is on disk"),
    responses(
        (status = 200, description = "The verdict — valid or not, with every problem at once. Nothing is ever written", body = ValidationResult<ConfigSummary>),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 404, description = "The payload names a file the loader does not read", body = ErrorBody)
    )
)]
pub async fn validate_config(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    body: Option<Json<ConfigBundle>>,
) -> Result<Json<ValidationResult<ConfigSummary>>, ApiError> {
    let host = Host {
        state: &state,
        keys: &keys,
    };
    shared::validate_config(&host, &auth, body.map(|Json(bundle)| bundle))
}

#[utoipa::path(
    post,
    path = "/api/v1/config/reload",
    tag = "Configuration",
    responses(
        (status = 200, description = "The directory on disk is now the running configuration", body = ReloadInfo),
        (status = 400, description = "What is on disk does not load — the process keeps running what it had", body = ConfigRejected),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 409, description = "A key's env var is missing, or the reload would turn authentication off", body = ErrorBody)
    )
)]
pub async fn reload_config(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    shared::reload_config(
        &Host {
            state: &state,
            keys: &keys,
        },
        &auth,
        &headers,
    )
}
