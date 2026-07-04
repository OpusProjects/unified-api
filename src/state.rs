use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::source::{ConnectorType, Source};
use crate::ports;

// El estado compartido de la aplicación: los ports (como Arc<dyn Trait>,
// así los handlers dependen de la interfaz y no de la implementación)
// más la configuración estática cargada al arrancar.
//
// Arc = Atomic Reference Counted — un puntero compartido entre threads;
// cada handler de axum recibe un clon barato del mismo AppState.
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
    // Elige el connector adecuado según el tipo declarado en el source
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
