use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::source::{ConnectorType, Source};
use crate::ports;

// The shared application state: the ports (as Arc<dyn Trait>, so handlers
// depend on the interface, not the implementation) plus the static
// configuration loaded at startup.
//
// Arc = Atomic Reference Counted — a reference-counted pointer shared across
// threads; each axum handler receives a cheap clone of the same AppState.
pub struct AppState {
    pub cache: Arc<dyn ports::cache::CachePort>,
    pub connector: Arc<dyn ports::connector::ConnectorPort>,
    pub ssh_connector: Arc<dyn ports::connector::ConnectorPort>,
    pub enricher: Arc<dyn ports::enricher::EnricherPort>,
    pub output: Arc<dyn ports::output::OutputPort>,
    pub secrets: Arc<dyn ports::secrets::SecretsPort>,
    pub sources: HashMap<String, Source>,
    pub enrichers: HashMap<String, Enricher>,
    pub endpoints: HashMap<String, OutputEndpoint>,
}

impl AppState {
    // Chooses the appropriate connector based on the type declared in the source
    pub fn connector_for(
        &self,
        connector_type: &ConnectorType,
    ) -> &Arc<dyn ports::connector::ConnectorPort> {
        match connector_type {
            ConnectorType::Script => &self.connector,
            ConnectorType::Ssh => &self.ssh_connector,
        }
    }
}
