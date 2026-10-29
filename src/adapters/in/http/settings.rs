use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};

// The configuration in force, read-only, for the UI's Configuration page.
//
// Not the file: the VALUES, defaults included. config.yaml is short because
// most settings are left at their default, and the question an operator
// brings ("is /metrics public here?", "how long does a session last?") is
// usually about one of those. The file itself, when it is wanted, is
// GET /api/v1/config/config.yaml (with the configuration API on).
//
// Grouped as config.yaml is, and named by the file's keys. Admin-only, like
// the configuration API: the listen address, the Vault and the paths are a
// map of the deployment. Never a secret: Vault is described by its address,
// mount and how it authenticates, not by a token.

#[derive(Serialize, ToSchema)]
pub struct Settings {
    /// The unified-api version running
    pub version: &'static str,
    /// How long this process has been running
    pub uptime_seconds: u64,
    pub server: ServerSettings,
    pub cache: CacheSettings,
    pub projects: ProjectsSettings,
    pub secrets: SecretsSettings,
    pub config_api: ConfigApiSettings,
    /// Keys a reload has changed in the files that this process cannot adopt
    /// without a restart; empty when none
    pub restart_pending: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ServerSettings {
    /// Restart-only; null when the process was not built from a config file
    pub host: Option<String>,
    /// Restart-only, like host
    pub port: Option<u16>,
    pub cors_allowed_origins: Vec<String>,
    pub readyz_require_all_sources: bool,
    pub metrics_require_auth: bool,
    pub refresh_timeout_seconds: u64,
    pub refresh_max_concurrent: usize,
    pub shutdown_grace_seconds: u64,
    pub max_body_bytes: usize,
    /// This instance's address, as `server.url` declares it; null when unset
    pub url: Option<String>,
    /// What this instance is called, as `server.name` declares it
    pub name: Option<String>,
    pub ui: UiSettings,
}

#[derive(Serialize, ToSchema)]
pub struct UiSettings {
    pub enabled: bool,
    pub session_ttl_seconds: u64,
    /// Whether directory (LDAP) login is configured; its details are on
    /// GET /api/v1/auth
    pub ldap: bool,
}

#[derive(Serialize, ToSchema)]
pub struct CacheSettings {
    /// Where the cache snapshot is written; null = no snapshot (restart-only)
    pub persistence_path: Option<String>,
    pub persistence_interval_seconds: Option<u64>,
    /// When a snapshot was last written; null before the first, or with
    /// persistence off. An idle cache skips its snapshots on purpose, so
    /// this ages while nothing changes.
    pub last_saved_age_seconds: Option<u64>,
    /// Snapshot writes that have failed since the last one that worked
    pub consecutive_failures: u32,
}

#[derive(Serialize, ToSchema)]
pub struct ProjectsSettings {
    /// Where project checkouts live (restart-only)
    pub dir: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct SecretsSettings {
    /// The deprecated config.yaml cache over every credential, when still
    /// set (a Vault credential now has its own `cache_ttl_seconds`)
    pub cache_ttl_seconds: Option<u64>,
    pub vault: Option<VaultSettings>,
    /// Whether config.yaml still has the deprecated `secrets:` block
    pub deprecated: bool,
}

#[derive(Serialize, ToSchema)]
pub struct VaultSettings {
    pub address: String,
    pub mount: String,
    /// `kubernetes` (with its role) or `token` (with the env var holding it)
    pub auth: &'static str,
    pub kubernetes_role: Option<String>,
    /// The environment variable the token is read from, for token auth
    pub token_env: Option<String>,
    pub timeout_seconds: u64,
}

#[derive(Serialize, ToSchema)]
pub struct ConfigApiSettings {
    /// Whether the configuration directory is readable and writable over
    /// /api/v1/config (restart-only)
    pub enabled: Option<bool>,
}

#[utoipa::path(
    get,
    path = "/api/v1/settings",
    tag = "Configuration",
    responses(
        (status = 200, description = "The configuration in force, defaults included, grouped and named as config.yaml is. Never a secret", body = Settings),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn get_settings(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Settings>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let boot = state.live_settings();

    let snapshot = state
        .snapshot_health
        .get(crate::adapters::out::cache::persistence::SNAPSHOT_HEALTH_KEY);

    Ok(Json(Settings {
        version: env!("CARGO_PKG_VERSION"),
        uptime_seconds: state.started.elapsed().as_secs(),
        server: ServerSettings {
            host: boot.as_ref().map(|b| b.host.clone()),
            port: boot.as_ref().map(|b| b.port),
            cors_allowed_origins: config.cors_allowed_origins.clone(),
            readyz_require_all_sources: config.readyz_require_all_sources,
            metrics_require_auth: config.metrics_require_auth,
            refresh_timeout_seconds: config.refresh_timeout_seconds,
            refresh_max_concurrent: config.refresh_max_concurrent,
            shutdown_grace_seconds: config.shutdown_grace_seconds,
            max_body_bytes: config.max_body_bytes,
            url: config.url.clone(),
            name: config.name.clone(),
            ui: UiSettings {
                enabled: state.ui.enabled,
                session_ttl_seconds: state.ui.session_ttl_seconds,
                ldap: config.ldap.is_some(),
            },
        },
        cache: CacheSettings {
            persistence_path: boot.as_ref().and_then(|b| b.persistence_path.clone()),
            persistence_interval_seconds: boot
                .as_ref()
                .and_then(|b| b.persistence_interval_seconds),
            last_saved_age_seconds: snapshot.as_ref().and_then(|h| h.last_success_age_seconds),
            consecutive_failures: snapshot.as_ref().map_or(0, |h| h.consecutive_failures),
        },
        projects: ProjectsSettings {
            dir: boot.as_ref().map(|b| b.projects_dir.clone()),
        },
        secrets: SecretsSettings {
            cache_ttl_seconds: config.secrets.cache_ttl_seconds,
            deprecated: config.secrets.cache_ttl_seconds.is_some()
                || config.secrets.vault.is_some(),
            vault: config.secrets.vault.as_ref().map(|v| VaultSettings {
                address: v.address.clone(),
                mount: v.mount.clone(),
                auth: if v.kubernetes_role.is_some() {
                    "kubernetes"
                } else {
                    "token"
                },
                kubernetes_role: v.kubernetes_role.clone(),
                token_env: v.kubernetes_role.is_none().then(|| v.token_env.clone()),
                timeout_seconds: v.timeout_seconds,
            }),
        },
        config_api: ConfigApiSettings {
            enabled: boot.as_ref().map(|b| b.config_api_enabled),
        },
        restart_pending: state.restart_pending(),
    }))
}
