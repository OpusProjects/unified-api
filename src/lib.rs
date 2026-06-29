pub mod adapters;
pub mod api;
pub mod config;
pub mod domain;
pub mod ports;

use axum::{routing::get, routing::post, Router};
use std::collections::HashMap;
use std::sync::Arc;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use adapters::memory_cache::MemoryCache;
use adapters::process_connector::ProcessConnector;
use domain::cache_entry::CacheEntry;
use domain::dataset::Dataset;
use domain::source::Source;

pub struct AppState {
    pub cache: Arc<dyn ports::cache::CachePort>,
    pub connector: Arc<dyn ports::connector::ConnectorPort>,
    pub sources: HashMap<String, Source>,
}

// #[derive(OpenApi)] genera la spec OpenAPI completa
// - paths() = los handlers anotados con #[utoipa::path]
// - components(schemas()) = los structs anotados con #[derive(ToSchema)]
// - tags = agrupaciones que aparecen en el Swagger UI
// - info = título, descripción, versión de la API
#[derive(OpenApi)]
#[openapi(
    paths(
        api::health::healthz,
        api::health::readyz,
        api::sources::list_cached_sources,
        api::sources::get_source_dataset,
        api::sources::source_status,
        api::sources::sync_source,
    ),
    components(schemas(
        api::sources::CachedSourceInfo,
        api::sources::HostStatus,
        api::sources::SourceStatus,
        api::sources::SyncResult,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Sources", description = "Inventory source management, sync, and cache status")
    ),
    info(
        title = "Unified API",
        version = "0.1.0",
        description = "Infrastructure inventory aggregation and caching middleware"
    )
)]
struct ApiDoc;

fn create_router(state: Arc<AppState>) -> Router<()> {
    Router::new()
        .route("/healthz", get(api::health::healthz))
        .route("/readyz", get(api::health::readyz))
        .route("/api/v1/sources", get(api::sources::list_cached_sources))
        .route("/api/v1/sources/{id}/dataset", get(api::sources::get_source_dataset))
        .route("/api/v1/sources/{id}/sync", post(api::sources::sync_source))
        .route("/api/v1/sources/{id}/status", get(api::sources::source_status))
        // SwaggerUi sirve la interfaz web en /swagger-ui/
        // y la spec JSON en /api-docs/openapi.json
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
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
