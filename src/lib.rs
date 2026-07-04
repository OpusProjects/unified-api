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

use adapters::memory_cache::MemoryCache;
use adapters::mock_secrets::MockSecrets;
use adapters::process_connector::ProcessConnector;
use adapters::process_enricher::ProcessEnricher;
use adapters::process_output::ProcessOutput;
use adapters::ssh_connector::SshConnector;
use domain::endpoint::OutputEndpoint;
use domain::enricher::Enricher;
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
    secrets: Arc<dyn SecretsPort>,
    api_key: Option<String>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            sources: HashMap::new(),
            enrichers: HashMap::new(),
            endpoints: HashMap::new(),
            // MockSecrets by default: in tests there is no secrets store
            secrets: Arc::new(MockSecrets::new()),
            api_key: None,
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

    pub fn secrets(mut self, secrets: Arc<dyn SecretsPort>) -> Self {
        self.secrets = secrets;
        self
    }

    pub fn api_key(mut self, api_key: Option<String>) -> Self {
        self.api_key = api_key;
        self
    }

    pub fn build(self) -> Router<()> {
        let (router, _state) = self.build_with_state();
        router
    }

    // Also returns the AppState: needed by main (to start the scheduler on the
    // same state) and tests that prepare the cache
    pub fn build_with_state(self) -> (Router<()>, Arc<AppState>) {
        let state = Arc::new(AppState {
            cache: Arc::new(MemoryCache::new()),
            connector: Arc::new(ProcessConnector::new()),
            ssh_connector: Arc::new(SshConnector::new()),
            enricher: Arc::new(ProcessEnricher::new()),
            output: Arc::new(ProcessOutput::new()),
            secrets: self.secrets,
            sources: self.sources,
            enrichers: self.enrichers,
            endpoints: self.endpoints,
        });
        let router = adapters::http::routes::create_router(Arc::clone(&state), self.api_key);
        (router, state)
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}
