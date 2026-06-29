pub mod adapters;
pub mod api;
pub mod config;
pub mod domain;
pub mod ports;

use axum::{routing::get, routing::post, Router};
use std::collections::HashMap;
use std::sync::Arc;

use adapters::memory_cache::MemoryCache;
use adapters::process_connector::ProcessConnector;
use domain::cache_entry::CacheEntry;
use domain::dataset::Dataset;
use domain::source::Source;

// AppState contiene todo lo que los handlers HTTP necesitan:
// - cache: donde se guardan los datasets
// - connector: cómo ejecutar scripts
// - sources: la configuración de cada source (del YAML)
pub struct AppState {
    pub cache: Arc<dyn ports::cache::CachePort>,
    pub connector: Arc<dyn ports::connector::ConnectorPort>,
    pub sources: HashMap<String, Source>,
}

// Helper para no repetir las rutas en cada build_app
fn create_router(state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route("/healthz", get(api::health::healthz))
        .route("/readyz", get(api::health::readyz))
        .route("/api/v1/sources", get(api::sources::list_cached_sources))
        .route("/api/v1/sources/{id}/dataset", get(api::sources::get_source_dataset))
        .route("/api/v1/sources/{id}/sync", post(api::sources::sync_source))
        .route("/api/v1/sources/{id}/status", get(api::sources::source_status))
        .with_state(state)
}

pub fn build_app() -> Router<()> {
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        sources: HashMap::new(),
    });
    create_router(state)
}

// Versión con sources configurados (para tests que necesitan sync)
pub fn build_app_with_sources(sources: HashMap<String, Source>) -> Router<()> {
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        sources,
    });
    create_router(state)
}

pub fn build_app_with_demo_data() -> Router<()> {
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        sources: HashMap::new(),
    });

    let demo_dataset: Dataset = serde_json::from_str(r#"{
        "hostvars": {
            "motoko.section9.net": {
                "ansible_host": "10.9.1.1",
                "os": "OracleLinux",
                "datacenter": "section9",
                "role": "commander"
            },
            "melchior.seele.net": {
                "ansible_host": "10.6.1.1",
                "os": "OracleLinux",
                "datacenter": "seele",
                "role": "magi-system"
            }
        },
        "groups": {
            "section9": {
                "hosts": ["motoko.section9.net"],
                "vars": {"ntp_server": "ntp.section9.net"}
            },
            "seele": {
                "hosts": ["melchior.seele.net"],
                "vars": {"ntp_server": "ntp.seele.net"}
            }
        }
    }"#).expect("Failed to parse demo dataset");

    state.cache.set("src-demo", CacheEntry::new(demo_dataset, 3600));

    create_router(state)
}
