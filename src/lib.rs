pub mod adapters;
pub mod api;
pub mod config;
pub mod domain;
pub mod ports;
pub mod scheduler;

use axum::{middleware, response::Redirect, routing::{get, post, put}, Router};
use std::collections::HashMap;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use utoipa::openapi::security::{ApiKey as OpenApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};
use utoipa_swagger_ui::SwaggerUi;

use api::auth::ApiKey;

use adapters::env_secrets::{EnvSecrets, MockSecrets};
use adapters::memory_cache::MemoryCache;
use adapters::process_connector::ProcessConnector;
use adapters::process_enricher::ProcessEnricher;
use adapters::process_output::ProcessOutput;
use adapters::ssh_connector::SshConnector;
use domain::cache_entry::CacheEntry;
use domain::credential::Credential;
use domain::dataset::Dataset;
use domain::endpoint::OutputEndpoint;
use domain::enricher::Enricher;
use domain::source::{ConnectorType, Source};

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
    pub fn connector_for(&self, connector_type: &ConnectorType) -> &Arc<dyn ports::connector::ConnectorPort> {
        match connector_type {
            ConnectorType::Script => &self.connector,
            ConnectorType::Ssh => &self.ssh_connector,
        }
    }
}

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().unwrap();
        components.add_security_scheme(
            "api_key",
            SecurityScheme::ApiKey(OpenApiKey::Header(ApiKeyValue::new("X-API-Key"))),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    modifiers(&SecurityAddon),
    security(
        ("api_key" = [])
    ),
    paths(
        api::health::healthz,
        api::health::readyz,
        api::sources::list_cached_sources,
        api::sources::get_source_dataset,
        api::sources::source_status,
        api::sources::sync_source,
        api::sources::run_enricher,
        api::sources::put_host,
        api::sources::delete_host,
        api::endpoints::run_endpoint,
        api::endpoints::list_endpoints,
    ),
    components(schemas(
        api::sources::CachedSourceInfo,
        api::sources::HostStatus,
        api::sources::SourceStatus,
        api::sources::SyncResult,
        api::sources::EnrichResult,
        api::endpoints::EndpointInfo,
        api::health::ReadyStatus,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Sources", description = "Inventory source management, sync, and cache status"),
        (name = "Enrichers", description = "Post-processing enrichment of cached data"),
        (name = "Endpoints", description = "Output endpoints for consumers (AWX, AnsibleForms)")
    ),
    info(
        title = "Unified API",
        version = "0.1.0",
        description = "Infrastructure inventory aggregation and caching middleware"
    )
)]
struct ApiDoc;

fn create_router(state: Arc<AppState>, api_key: Option<String>) -> Router<()> {
    let api_routes = Router::new()
        .route("/api/v1/sources", get(api::sources::list_cached_sources))
        .route("/api/v1/sources/{id}/dataset", get(api::sources::get_source_dataset))
        .route("/api/v1/sources/{id}/sync", post(api::sources::sync_source))
        .route("/api/v1/sources/{id}/status", get(api::sources::source_status))
        .route("/api/v1/sources/{id}/hosts/{hostname}", put(api::sources::put_host).delete(api::sources::delete_host))
        .route("/api/v1/enrichers/{id}/run", post(api::sources::run_enricher))
        .route("/api/v1/endpoints", get(api::endpoints::list_endpoints))
        .route("/api/v1/endpoints/{id}", post(api::endpoints::run_endpoint))
        .layer(middleware::from_fn(api::auth::require_api_key))
        .layer(axum::Extension(ApiKey(api_key)));

    Router::new()
        .route("/", get(|| async { Redirect::permanent("/swagger-ui/") }))
        .route("/healthz", get(api::health::healthz))
        .route("/readyz", get(api::health::readyz))
        .merge(api_routes)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any))
        .with_state(state)
}

pub fn build_app() -> Router<()> {
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        ssh_connector: Arc::new(SshConnector::new()),
        enricher: Arc::new(ProcessEnricher::new()),
        output: Arc::new(ProcessOutput::new()),
        secrets: Arc::new(MockSecrets::new()),
        sources: HashMap::new(),
        enrichers: HashMap::new(),
        endpoints: HashMap::new(),
    });
    create_router(state, None)
}

// Para tests con sources (mock secrets)
pub fn build_app_with_sources(sources: HashMap<String, Source>) -> Router<()> {
    let (router, _state) = build_app_with_sources_and_state(sources);
    router
}

pub fn build_app_with_sources_and_state(
    sources: HashMap<String, Source>,
) -> (Router<()>, Arc<AppState>) {
    build_app_full(sources, HashMap::new(), HashMap::new())
}

pub fn build_app_full(
    sources: HashMap<String, Source>,
    enrichers: HashMap<String, Enricher>,
    endpoints: HashMap<String, OutputEndpoint>,
) -> (Router<()>, Arc<AppState>) {
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        ssh_connector: Arc::new(SshConnector::new()),
        enricher: Arc::new(ProcessEnricher::new()),
        output: Arc::new(ProcessOutput::new()),
        secrets: Arc::new(MockSecrets::new()),
        sources,
        enrichers,
        endpoints,
    });
    let router = create_router(Arc::clone(&state), None);
    (router, state)
}

pub fn build_app_production(
    sources: HashMap<String, Source>,
    credentials: HashMap<String, Credential>,
    enrichers: HashMap<String, Enricher>,
    endpoints: HashMap<String, OutputEndpoint>,
) -> (Router<()>, Arc<AppState>) {
    let api_key = std::env::var("UNIFIED_API_KEY").ok();
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        ssh_connector: Arc::new(SshConnector::new()),
        enricher: Arc::new(ProcessEnricher::new()),
        output: Arc::new(ProcessOutput::new()),
        secrets: Arc::new(EnvSecrets::new(credentials)),
        sources,
        enrichers,
        endpoints,
    });
    let router = create_router(Arc::clone(&state), api_key);
    (router, state)
}

pub fn build_app_with_demo_data() -> Router<()> {
    let state = Arc::new(AppState {
        cache: Arc::new(MemoryCache::new()),
        connector: Arc::new(ProcessConnector::new()),
        ssh_connector: Arc::new(SshConnector::new()),
        enricher: Arc::new(ProcessEnricher::new()),
        output: Arc::new(ProcessOutput::new()),
        secrets: Arc::new(MockSecrets::new()),
        sources: HashMap::new(),
        enrichers: HashMap::new(),
        endpoints: HashMap::new(),
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

    create_router(state, None)
}
