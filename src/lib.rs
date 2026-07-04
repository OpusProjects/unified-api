pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;
pub mod ports;
mod state;

// Re-export: el resto del código (y los tests) siguen usando crate::AppState
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

// AppBuilder es el composition root: el ÚNICO sitio donde se eligen los
// adapters concretos que llenan los ports del AppState. Producción cambia
// los secrets (EnvSecrets en vez de MockSecrets) y añade la API key; los
// tests usan los defaults. Patrón builder: cada método consume self y lo
// devuelve, así se encadenan llamadas y build() cierra la construcción.
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
            // MockSecrets por defecto: en tests no hay almacén de secrets
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

    // Devuelve también el AppState: lo necesitan main (para arrancar el
    // scheduler sobre el mismo estado) y los tests que preparan el cache
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
