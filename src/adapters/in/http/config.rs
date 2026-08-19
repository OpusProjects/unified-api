use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::out::config::fs::directory_etag;
use crate::application::config as reload_use_case;
use crate::config::{AppConfig, CONFIG_FILES, ConfigErrors, REQUIRED_CONFIG_FILE, is_config_file};
use crate::ports::config_store::{ConfigChange, ConfigFileStat, ConfigStorePort};

use super::audit;
use super::auth::{ApiKeys, AuthContext, resolve_api_keys};
use super::error::{ApiError, ErrorBody};

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

// ---------------------------------------------------------------- wire types

#[derive(Serialize, ToSchema)]
pub struct ConfigFileInfo {
    pub name: String,
    pub size: u64,
    /// Hex sha256 of the contents — the value served as this file's ETag
    pub sha256: String,
    /// RFC 3339, absent if the filesystem does not report one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ConfigSummary {
    pub sources: usize,
    pub views: usize,
    pub credentials: usize,
    pub enrichers: usize,
    pub endpoints: usize,
    pub projects: usize,
    pub api_keys: usize,
}

#[derive(Serialize, ToSchema)]
pub struct ConfigInventory {
    /// Where the files live on the instance
    pub directory: String,
    /// One hash over the whole directory — send it back as If-Match on a
    /// whole-directory push and a write that would clobber someone else's
    /// change is refused instead of silently winning
    pub etag: String,
    pub files: Vec<ConfigFileInfo>,
    /// Known files that are not present. Every one of them is optional except
    /// config.yaml
    pub missing: Vec<String>,
    /// How many times this process has reloaded. 0 = still running exactly
    /// what it booted with
    pub generation: u64,
    /// Whether what is on disk right now would load
    pub valid: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    /// True when the directory differs from what the process is serving —
    /// somebody wrote files and nobody reloaded
    pub reload_pending: bool,
    /// config.yaml keys that differ from the ones this process was built with
    /// and that no reload can adopt
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub restart_required: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct DeltaInfo {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
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
    pub views: DeltaInfo,
    pub enrichers: DeltaInfo,
    pub endpoints: DeltaInfo,
    pub projects: DeltaInfo,
    pub credentials: DeltaInfo,
    /// How many API keys are in force after the reload
    pub api_keys: usize,
}

#[derive(Serialize, ToSchema)]
pub struct WriteResult {
    pub written: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deleted: Vec<String>,
    /// The directory ETag AFTER the write — feed it to the next If-Match
    pub etag: String,
    pub summary: ConfigSummary,
    /// Present when the write asked for ?reload=true
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reloaded: Option<ReloadInfo>,
    /// True when the files are on disk but the process is still serving the
    /// previous configuration (no ?reload=true, and something differs)
    pub reload_pending: bool,
}

#[derive(Serialize, ToSchema)]
pub struct ValidationResult {
    pub valid: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<ConfigSummary>,
}

// A rejected configuration, as a SUPERSET of the ordinary error body: `error`
// is the one-line summary every other route answers with, `errors` is the
// whole list. A pipeline renders the list; a generic client that only knows
// `error` keeps working.
#[derive(Serialize, ToSchema)]
pub struct ConfigRejected {
    pub error: String,
    pub errors: Vec<String>,
}

// The whole directory in one request — the verb a pipeline actually wants.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigBundle {
    /// File name -> YAML contents. Unnamed files keep what is on disk unless
    /// `prune` says otherwise
    pub files: BTreeMap<String, String>,
    /// Delete every known file this payload does not name, so the directory
    /// ends up being exactly the push. The same semantics as the
    /// configuration image it replaces: what is not in the image is not in
    /// /config
    #[serde(default)]
    pub prune: bool,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReloadQuery {
    /// Apply the new configuration to the running process as part of the
    /// write, so the push is one request instead of two
    #[serde(default)]
    pub reload: bool,
}

// ------------------------------------------------------------------- helpers

fn store(state: &AppState) -> Result<&Arc<dyn ConfigStorePort>, ApiError> {
    state.config_store.as_ref().ok_or_else(|| {
        ApiError::forbidden(
            "the configuration API is disabled on this instance — set \
             config_api.enabled: true in config.yaml to allow writing the \
             configuration directory over HTTP",
        )
    })
}

fn admin(auth: &AuthContext) -> Result<(), ApiError> {
    if auth.permissions.is_admin() {
        Ok(())
    } else {
        Err(ApiError::admin_only())
    }
}

fn known_file(name: &str) -> Result<(), ApiError> {
    if is_config_file(name) {
        Ok(())
    } else {
        Err(ApiError::not_found(format!(
            "'{}' is not a configuration file — the loader reads exactly: {}",
            name,
            CONFIG_FILES.join(", ")
        )))
    }
}

fn rfc3339(time: std::time::SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339()
}

fn file_info(stat: &ConfigFileStat) -> ConfigFileInfo {
    ConfigFileInfo {
        name: stat.name.clone(),
        size: stat.size,
        sha256: stat.sha256.clone(),
        modified: stat.modified.map(rfc3339),
    }
}

fn summarize(cfg: &AppConfig) -> ConfigSummary {
    ConfigSummary {
        sources: cfg.sources.len(),
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
        views: (&report.views).into(),
        enrichers: (&report.enrichers).into(),
        endpoints: (&report.endpoints).into(),
        projects: (&report.projects).into(),
        credentials: (&report.credentials).into(),
        api_keys,
    }
}

// A rejected configuration is a 400 with the whole list, not a 400 with the
// first line of it: a pipeline that pushed eight files wants every problem in
// one round trip, which is the same reason --check-config prints them all.
fn rejected(errors: ConfigErrors) -> Response {
    metrics::counter!("unified_api_config_writes_total", "outcome" => "rejected").increment(1);
    let error = match errors.errors.as_slice() {
        [one] => one.clone(),
        many => format!("{} configuration errors", many.len()),
    };
    (
        StatusCode::BAD_REQUEST,
        Json(ConfigRejected {
            error,
            errors: errors.errors,
        }),
    )
        .into_response()
}

// If-Match, honored the way a config pipeline needs it: the client sends the
// ETag it read, and a write against a directory (or file) that has moved since
// is refused rather than applied on top of someone else's change. `*` means
// "must exist". No header at all means "I am the only writer", which is the
// common case and stays unceremonious.
fn precondition(headers: &HeaderMap, current: Option<&str>) -> Result<(), ApiError> {
    let Some(expected) = headers.get(header::IF_MATCH).and_then(|v| v.to_str().ok()) else {
        return Ok(());
    };

    let matches = expected
        .split(',')
        .map(|candidate| candidate.trim().trim_matches('"'))
        .any(|candidate| match current {
            Some(etag) => candidate == "*" || candidate == etag,
            None => false,
        });

    if matches {
        return Ok(());
    }

    Err(ApiError::new(
        StatusCode::PRECONDITION_FAILED,
        match current {
            Some(etag) => format!(
                "If-Match did not match: the current ETag is '{}'. Re-read the \
                 configuration and rebase the change onto it",
                etag
            ),
            None => "If-Match was sent but the file does not exist".to_string(),
        },
    ))
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
) -> Result<Json<ConfigInventory>, ApiError> {
    admin(&auth)?;
    let store = store(&state)?;

    let stats = store.stat_all().map_err(ApiError::internal)?;
    let present: Vec<&str> = stats.iter().map(|s| s.name.as_str()).collect();
    let missing = CONFIG_FILES
        .iter()
        .filter(|name| !present.contains(name))
        .map(|name| name.to_string())
        .collect();

    // What is on disk may differ from what is running (somebody wrote without
    // reloading) and may not even load (somebody edited the file by hand).
    // Both are worth an answer here rather than at the next restart.
    let (valid, errors, reload_pending, restart_required) =
        match store.load(&ConfigChange::default()) {
            Ok(cfg) => {
                let pending = reload_use_case::pending(&state, &cfg);
                (
                    true,
                    Vec::new(),
                    pending.changed_anything(),
                    pending.restart_required,
                )
            }
            Err(e) => (false, e.errors, false, Vec::new()),
        };

    Ok(Json(ConfigInventory {
        directory: store.location(),
        etag: directory_etag(&stats),
        files: stats.iter().map(file_info).collect(),
        missing,
        generation: state.reload.generation(),
        valid,
        errors,
        reload_pending,
        restart_required,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/config/{file}",
    tag = "Configuration",
    params(("file" = String, Path, description = "File name, e.g. sources.yaml")),
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
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    admin(&auth)?;
    let store = store(&state)?;
    known_file(&file)?;

    let Some((stat, contents)) = store.read(&file).map_err(ApiError::internal)? else {
        return Err(ApiError::not_found(format!(
            "'{}' is not present in the configuration directory",
            file
        )));
    };

    let etag = format!("\"{}\"", stat.sha256);
    if let Some(sent) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        && sent
            .split(',')
            .any(|candidate| candidate.trim() == etag || candidate.trim() == "*")
    {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/yaml".to_string()),
            (header::ETAG, etag),
        ],
        contents,
    )
        .into_response())
}

#[utoipa::path(
    put,
    path = "/api/v1/config/{file}",
    tag = "Configuration",
    params(
        ("file" = String, Path, description = "File name, e.g. sources.yaml"),
        ("reload" = Option<bool>, Query, description = "Apply to the running process as part of the write")
    ),
    request_body(content = String, description = "The file's new contents, verbatim YAML", content_type = "application/yaml"),
    responses(
        (status = 200, description = "Written — and applied, if reload was asked for", body = WriteResult),
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
    admin(&auth)?;
    let store = store(&state)?;
    known_file(&file)?;

    let current = store.read(&file).map_err(ApiError::internal)?;
    precondition(
        &headers,
        current.as_ref().map(|(stat, _)| stat.sha256.as_str()),
    )?;

    let change = ConfigChange::writing([(file.clone(), body)].into_iter().collect());
    write(&state, store, &keys, &auth, &headers, change, query.reload).await
}

#[utoipa::path(
    delete,
    path = "/api/v1/config/{file}",
    tag = "Configuration",
    params(
        ("file" = String, Path, description = "File name, e.g. enrichers.yaml"),
        ("reload" = Option<bool>, Query, description = "Apply to the running process as part of the delete")
    ),
    responses(
        (status = 200, description = "Removed — and applied, if reload was asked for", body = WriteResult),
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
    admin(&auth)?;
    let store = store(&state)?;
    known_file(&file)?;

    if file == REQUIRED_CONFIG_FILE {
        return Err(ApiError::bad_request(format!(
            "'{}' cannot be deleted — it is the one file a configuration \
             cannot start without",
            REQUIRED_CONFIG_FILE
        )));
    }

    let Some((stat, _)) = store.read(&file).map_err(ApiError::internal)? else {
        return Err(ApiError::not_found(format!(
            "'{}' is not present in the configuration directory",
            file
        )));
    };
    precondition(&headers, Some(&stat.sha256))?;

    write(
        &state,
        store,
        &keys,
        &auth,
        &headers,
        ConfigChange::deleting(&file),
        query.reload,
    )
    .await
}

#[utoipa::path(
    put,
    path = "/api/v1/config",
    tag = "Configuration",
    params(("reload" = Option<bool>, Query, description = "Apply to the running process as part of the write")),
    request_body = ConfigBundle,
    responses(
        (status = 200, description = "The whole directory was replaced — and applied, if reload was asked for", body = WriteResult),
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
    admin(&auth)?;
    let store = store(&state)?;

    for name in bundle.files.keys() {
        known_file(name)?;
    }

    let stats = store.stat_all().map_err(ApiError::internal)?;
    precondition(&headers, Some(&directory_etag(&stats)))?;

    let change = ConfigChange {
        write: bundle.files,
        prune: bundle.prune,
        ..ConfigChange::default()
    };
    write(&state, store, &keys, &auth, &headers, change, query.reload).await
}

#[utoipa::path(
    post,
    path = "/api/v1/config/validate",
    tag = "Configuration",
    request_body(content = ConfigBundle, description = "The directory to validate. Omit the body to validate what is on disk"),
    responses(
        (status = 200, description = "The verdict — valid or not, with every problem at once. Nothing is ever written", body = ValidationResult),
        (status = 403, description = "Not an admin key, or the configuration API is disabled", body = ErrorBody),
        (status = 404, description = "The payload names a file the loader does not read", body = ErrorBody)
    )
)]
pub async fn validate_config(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    body: Option<Json<ConfigBundle>>,
) -> Result<Json<ValidationResult>, ApiError> {
    admin(&auth)?;
    let store = store(&state)?;

    let change = match body {
        Some(Json(bundle)) => {
            for name in bundle.files.keys() {
                known_file(name)?;
            }
            ConfigChange {
                write: bundle.files,
                prune: bundle.prune,
                ..ConfigChange::default()
            }
        }
        None => ConfigChange::default(),
    };

    Ok(Json(match store.load(&change) {
        Ok(cfg) => ValidationResult {
            valid: true,
            errors: Vec::new(),
            summary: Some(summarize(&cfg)),
        },
        Err(e) => ValidationResult {
            valid: false,
            errors: e.errors,
            summary: None,
        },
    }))
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
    admin(&auth)?;
    let store = store(&state)?;

    let cfg = match store.load(&ConfigChange::default()) {
        Ok(cfg) => cfg,
        Err(e) => {
            metrics::counter!("unified_api_config_reloads_total", "outcome" => "invalid")
                .increment(1);
            audit::record(&auth, &headers, "config_reload", "config", "invalid");
            return Ok(rejected(e));
        }
    };

    let resolved = keys_for(&cfg, &keys)?;
    let report = reload_use_case::apply(&state, &cfg);
    let key_count = resolved.len();
    keys.0.replace(resolved);

    audit::record(&auth, &headers, "config_reload", "config", "success");

    Ok(Json(reload_info(&report, key_count)).into_response())
}

// Validate, then commit, then — only if asked — apply. Shared by all three
// write routes so the ORDER cannot differ between them: nothing is written
// until the whole directory loads, and nothing is applied until everything a
// reload needs (every API key's env var, and an API that still has
// authentication) is known to be there.
async fn write(
    state: &Arc<AppState>,
    store: &Arc<dyn ConfigStorePort>,
    keys: &ApiKeys,
    auth: &AuthContext,
    headers: &HeaderMap,
    change: ConfigChange,
    reload: bool,
) -> Result<Response, ApiError> {
    let cfg = match store.load(&change) {
        Ok(cfg) => cfg,
        Err(e) => {
            audit::record(
                auth,
                headers,
                "config_write",
                &changed_names(&change).join(","),
                "rejected",
            );
            return Ok(rejected(e));
        }
    };

    // Before the commit, deliberately: a write that cannot be applied should
    // fail as a whole rather than land on disk and then refuse to take effect.
    let resolved = if reload {
        Some(keys_for(&cfg, keys)?)
    } else {
        None
    };

    store.commit(&change).map_err(|e| {
        metrics::counter!("unified_api_config_writes_total", "outcome" => "error").increment(1);
        ApiError::internal(format!("configuration written to nowhere: {}", e))
    })?;
    metrics::counter!("unified_api_config_writes_total", "outcome" => "success").increment(1);

    let written: Vec<String> = change.write.keys().cloned().collect();
    let deleted = deleted_names(state, store, &change)?;

    let reloaded = match resolved {
        Some(resolved) => {
            let report = reload_use_case::apply(state, &cfg);
            let key_count = resolved.len();
            keys.0.replace(resolved);
            Some(reload_info(&report, key_count))
        }
        None => None,
    };
    // Without a reload the files are on disk and the process is still serving
    // the old configuration. Saying so is the difference between a pipeline
    // that knows it has one more step and one that reports a deploy it did
    // not do.
    let reload_pending =
        reloaded.is_none() && reload_use_case::pending(state, &cfg).changed_anything();

    audit::record(
        auth,
        headers,
        if reload {
            "config_write_reload"
        } else {
            "config_write"
        },
        &changed_names(&change).join(","),
        "success",
    );

    let stats = store.stat_all().map_err(ApiError::internal)?;

    Ok(Json(WriteResult {
        written,
        deleted,
        etag: directory_etag(&stats),
        summary: summarize(&cfg),
        reloaded,
        reload_pending,
    })
    .into_response())
}

fn changed_names(change: &ConfigChange) -> Vec<String> {
    let mut names: Vec<String> = change.write.keys().cloned().collect();
    names.extend(change.delete.iter().cloned());
    if change.prune {
        names.push("(prune)".to_string());
    }
    names
}

// Which files the commit actually removed: the explicit deletions, plus
// whatever a pruning push left out.
fn deleted_names(
    _state: &Arc<AppState>,
    store: &Arc<dyn ConfigStorePort>,
    change: &ConfigChange,
) -> Result<Vec<String>, ApiError> {
    let present = store.stat_all().map_err(ApiError::internal)?;
    let mut deleted: Vec<String> = change.delete.iter().cloned().collect();
    if change.prune {
        for name in CONFIG_FILES {
            if !change.write.contains_key(name) && !present.iter().any(|s| s.name == name) {
                deleted.push(name.to_string());
            }
        }
    }
    deleted.sort();
    deleted.dedup();
    Ok(deleted)
}
