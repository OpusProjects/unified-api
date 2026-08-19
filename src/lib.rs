pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;
pub mod ports;
mod state;

// Re-export: the rest of the code (and tests) continue using crate::AppState
pub use state::{AppState, ReloadNotifier, RuntimeConfig};

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;

use adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use adapters::out::cache::memory::MemoryCache;
use adapters::out::connectors::process::ProcessConnector;
use adapters::out::connectors::remote::RemoteConnector;
use adapters::out::connectors::ssh::SshConnector;
use adapters::out::connectors::static_inventory::StaticInventoryConnector;
use adapters::out::enrichers::process::ProcessEnricher;
use adapters::out::git::cli::CliGit;
use adapters::out::output::process::ProcessOutput;
use adapters::out::secrets::mock::MockSecrets;
use adapters::out::secrets::reloadable::ReloadableSecrets;
use domain::credential::Credential;
use domain::endpoint::OutputEndpoint;
use domain::enricher::Enricher;
use domain::project::GitProject;
use domain::source::Source;
use domain::view::View;
use ports::secrets::SecretsPort;

// AppBuilder is the composition root: the ONLY place where concrete adapters
// are chosen to fill AppState ports. Production swaps secrets (EnvSecrets
// instead of MockSecrets) and adds the API key; tests use defaults. Builder
// pattern: each method consumes self and returns it, so calls can be chained
// and build() finalizes the construction.
pub struct AppBuilder {
    sources: HashMap<String, Source>,
    views: HashMap<String, View>,
    enrichers: HashMap<String, Enricher>,
    endpoints: HashMap<String, OutputEndpoint>,
    projects: HashMap<String, GitProject>,
    credentials: HashMap<String, Credential>,
    secrets_settings: config::SecretsConfig,
    projects_dir: std::path::PathBuf,
    secrets: Arc<dyn SecretsPort>,
    // Some = the configuration API is enabled: the directory can be read and
    // written over HTTP, and a reload can swap what this builder is about to
    // freeze into AppState.
    config_store: Option<Arc<dyn ports::config_store::ConfigStorePort>>,
    live_settings: Option<config::RestartOnlySettings>,
    api_keys: Vec<ResolvedApiKey>,
    project_health: Arc<domain::sync_health::SyncHealthRegistry>,
    cors_allowed_origins: Vec<String>,
    readyz_require_all_sources: bool,
    metrics_require_auth: bool,
    refresh_timeout_seconds: u64,
    refresh_max_concurrent: usize,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            views: HashMap::new(),
            enrichers: HashMap::new(),
            endpoints: HashMap::new(),
            projects: HashMap::new(),
            credentials: HashMap::new(),
            secrets_settings: config::SecretsConfig::default(),
            projects_dir: std::path::PathBuf::from("projects"),
            // MockSecrets by default: in tests there is no secrets store
            secrets: Arc::new(MockSecrets::new()),
            config_store: None,
            live_settings: None,
            api_keys: Vec::new(),
            project_health: Arc::new(domain::sync_health::SyncHealthRegistry::new()),
            cors_allowed_origins: Vec::new(),
            readyz_require_all_sources: false,
            metrics_require_auth: false,
            // Same defaults as config.rs: a page-length wait, and a cap that
            // lets a handful of forms refresh at once without opening a session
            // per host in the inventory.
            refresh_timeout_seconds: 15,
            refresh_max_concurrent: 8,
        }
    }

    pub fn sources(mut self, sources: HashMap<String, Source>) -> Self {
        self.sources = sources;
        self
    }

    pub fn views(mut self, views: HashMap<String, View>) -> Self {
        self.views = views;
        self
    }

    pub fn enrichers(mut self, enrichers: HashMap<String, Enricher>) -> Self {
        self.enrichers = enrichers;
        self
    }

    pub fn endpoints(mut self, endpoints: HashMap<String, OutputEndpoint>) -> Self {
        self.endpoints = endpoints;
        self
    }

    pub fn projects(
        mut self,
        projects: HashMap<String, GitProject>,
        projects_dir: std::path::PathBuf,
    ) -> Self {
        self.projects = projects;
        self.projects_dir = projects_dir;
        self
    }

    pub fn secrets(mut self, secrets: Arc<dyn SecretsPort>) -> Self {
        self.secrets = secrets;
        self
    }

    // The credential DEFINITIONS (not their values), and the settings the
    // resolver chain is built from. Both only matter to a reload: it rebuilds
    // the chain from the new ones and reports which credential changed.
    pub fn credentials(
        mut self,
        credentials: HashMap<String, Credential>,
        settings: config::SecretsConfig,
    ) -> Self {
        self.credentials = credentials;
        self.secrets_settings = settings;
        self
    }

    // Everything a loaded AppConfig contributes, in one call.
    //
    // Borrows rather than consumes: main still needs the same config for the
    // things that are NOT part of the app (the listener address, the snapshot
    // path, the boot clones), and a reload needs to build the identical set
    // from a config it did not consume either. Cloning a few maps once at
    // boot is not a cost worth a partially-moved struct.
    pub fn from_config(mut self, cfg: &config::AppConfig) -> Self {
        self.sources = cfg.sources.clone();
        self.views = cfg.views.clone();
        self.enrichers = cfg.enrichers.clone();
        self.endpoints = cfg.endpoints.clone();
        self.projects = cfg.projects.clone();
        self.credentials = cfg.credentials.clone();
        self.secrets_settings = cfg.secrets_config.clone();
        self.projects_dir = std::path::PathBuf::from(&cfg.projects_config.dir);
        self.cors_allowed_origins = cfg.server.cors_allowed_origins.clone();
        self.readyz_require_all_sources = cfg.server.readyz_require_all_sources;
        self.metrics_require_auth = cfg.server.metrics_require_auth;
        self.refresh_timeout_seconds = cfg.server.refresh_timeout_seconds;
        self.refresh_max_concurrent = cfg.server.refresh_max_concurrent;
        self
    }

    // Turn on the configuration API. `live_settings` is what THIS process was
    // built from, kept so a later reload can name the keys it cannot adopt.
    pub fn config_api(
        mut self,
        store: Arc<dyn ports::config_store::ConfigStorePort>,
        live_settings: config::RestartOnlySettings,
    ) -> Self {
        self.config_store = Some(store);
        self.live_settings = Some(live_settings);
        self
    }

    // Shorthand kept from the single-key era: one secret = one admin key.
    // Tests and the legacy UNIFIED_API_KEY path use it.
    pub fn api_key(mut self, api_key: Option<String>) -> Self {
        if let Some(secret) = api_key {
            self.api_keys.push(ResolvedApiKey {
                name: "default".to_string(),
                secret,
                permissions: Permissions::Admin,
            });
        }
        self
    }

    pub fn api_keys(mut self, api_keys: Vec<ResolvedApiKey>) -> Self {
        self.api_keys = api_keys;
        self
    }

    // Project pulls start at boot, before the AppState exists (main clones the
    // checkouts so script paths can be resolved into them). Handing the
    // registry in lets those boot syncs and the periodic task write into the
    // same instance the HTTP layer reads. Defaults to a fresh one for tests.
    pub fn project_health(
        mut self,
        registry: Arc<domain::sync_health::SyncHealthRegistry>,
    ) -> Self {
        self.project_health = registry;
        self
    }

    pub fn cors_allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.cors_allowed_origins = origins;
        self
    }

    pub fn readyz_require_all_sources(mut self, require_all: bool) -> Self {
        self.readyz_require_all_sources = require_all;
        self
    }

    pub fn on_demand_refresh(mut self, timeout_seconds: u64, max_concurrent: usize) -> Self {
        self.refresh_timeout_seconds = timeout_seconds;
        self.refresh_max_concurrent = max_concurrent;
        self
    }

    pub fn metrics_require_auth(mut self, require_auth: bool) -> Self {
        self.metrics_require_auth = require_auth;
        self
    }

    pub fn build(self) -> Router<()> {
        let (router, _state) = self.build_with_state();
        router
    }

    // Also returns the AppState: needed by main (to start the scheduler on the
    // same state) and tests that prepare the cache
    pub fn build_with_state(self) -> (Router<()>, Arc<AppState>) {
        // Install the metrics recorder before anything can record
        adapters::r#in::http::metrics::init();

        // The resolver chain always sits behind the reloadable indirection,
        // in tests as in production: one shape to reason about, and a reload
        // never has to ask whether it is allowed to swap.
        let reloadable = Arc::new(ReloadableSecrets::new(self.secrets));

        let state = Arc::new(AppState {
            cache: Arc::new(MemoryCache::new()),
            connector: Arc::new(ProcessConnector::new()),
            ssh_connector: Arc::new(SshConnector::new()),
            static_connector: Arc::new(StaticInventoryConnector::new()),
            remote_connector: Arc::new(RemoteConnector::new()),
            enricher: Arc::new(ProcessEnricher::new()),
            output: Arc::new(ProcessOutput::new()),
            secrets: Arc::clone(&reloadable) as Arc<dyn SecretsPort>,
            git: Arc::new(CliGit::new()),
            venv: Arc::new(adapters::out::python::PyVenv::new()),
            secrets_swap: reloadable,
            config: std::sync::RwLock::new(Arc::new(RuntimeConfig {
                sources: self.sources,
                credentials: self.credentials,
                views: self.views,
                enrichers: self.enrichers,
                endpoints: self.endpoints,
                projects: self.projects,
                secrets: self.secrets_settings,
                readyz_require_all_sources: self.readyz_require_all_sources,
            })),
            config_store: self.config_store,
            live_settings: std::sync::RwLock::new(self.live_settings),
            reload: ReloadNotifier::new(),
            projects_dir: self.projects_dir,
            sync_health: Arc::new(domain::sync_health::SyncHealthRegistry::new()),
            advertised_scopes: Arc::new(domain::source::AdvertisedScopeRegistry::new()),
            enrich_health: Arc::new(domain::sync_health::SyncHealthRegistry::new()),
            project_health: self.project_health,
            snapshot_health: Arc::new(domain::sync_health::SyncHealthRegistry::new()),
            refresh: Arc::new(application::refresh::RefreshCoordinator::new(
                self.refresh_max_concurrent,
                self.refresh_timeout_seconds,
            )),
            syncs: Arc::new(application::sync::SyncCoordinator::new()),
            view_hosts_memo: application::views::ViewHostsMemo::default(),
        });
        let router = adapters::r#in::http::routes::create_router(
            Arc::clone(&state),
            Arc::new(adapters::r#in::http::auth::ApiKeyRegistry::new(
                self.api_keys,
            )),
            self.cors_allowed_origins,
            self.metrics_require_auth,
        );
        (router, state)
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}
