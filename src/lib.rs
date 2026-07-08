pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;
pub mod ports;
mod state;

// Re-export: the rest of the code (and tests) continue using crate::AppState
pub use state::AppState;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;

use adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use adapters::out::cache::memory::MemoryCache;
use adapters::out::connectors::process::ProcessConnector;
use adapters::out::connectors::ssh::SshConnector;
use adapters::out::enrichers::process::ProcessEnricher;
use adapters::out::git::cli::CliGit;
use adapters::out::output::process::ProcessOutput;
use adapters::out::secrets::mock::MockSecrets;
use domain::endpoint::OutputEndpoint;
use domain::enricher::Enricher;
use domain::project::GitProject;
use domain::source::Source;
use ports::secrets::SecretsPort;

// AppBuilder is the composition root: the ONLY place where concrete adapters
// are chosen to fill AppState ports. Production swaps secrets (EnvSecrets
// instead of MockSecrets) and adds the API key; tests use defaults. Builder
// pattern: each method consumes self and returns it, so calls can be chained
// and build() finalizes the construction.
pub struct AppBuilder {
    sources: HashMap<String, Source>,
    enrichers: HashMap<String, Enricher>,
    endpoints: HashMap<String, OutputEndpoint>,
    projects: HashMap<String, GitProject>,
    projects_dir: std::path::PathBuf,
    secrets: Arc<dyn SecretsPort>,
    api_keys: Vec<ResolvedApiKey>,
    cors_allowed_origins: Vec<String>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            enrichers: HashMap::new(),
            endpoints: HashMap::new(),
            projects: HashMap::new(),
            projects_dir: std::path::PathBuf::from("projects"),
            // MockSecrets by default: in tests there is no secrets store
            secrets: Arc::new(MockSecrets::new()),
            api_keys: Vec::new(),
            cors_allowed_origins: Vec::new(),
        }
    }

    pub fn sources(mut self, sources: HashMap<String, Source>) -> Self {
        self.sources = sources;
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

    pub fn cors_allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.cors_allowed_origins = origins;
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

        let state = Arc::new(AppState {
            cache: Arc::new(MemoryCache::new()),
            connector: Arc::new(ProcessConnector::new()),
            ssh_connector: Arc::new(SshConnector::new()),
            enricher: Arc::new(ProcessEnricher::new()),
            output: Arc::new(ProcessOutput::new()),
            secrets: self.secrets,
            git: Arc::new(CliGit::new()),
            sources: self.sources,
            enrichers: self.enrichers,
            endpoints: self.endpoints,
            projects: self.projects,
            projects_dir: self.projects_dir,
        });
        let router = adapters::r#in::http::routes::create_router(
            Arc::clone(&state),
            self.api_keys,
            self.cors_allowed_origins,
        );
        (router, state)
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}
