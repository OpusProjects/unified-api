use utoipa::openapi::security::{ApiKey as OpenApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::api;

// Añade el esquema de seguridad (header X-API-Key) a la spec generada
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

// La spec OpenAPI completa: utoipa la genera en compile-time a partir de
// los atributos #[utoipa::path] de cada handler listado aquí.
// Un handler nuevo no aparece en Swagger hasta que se registra en paths()
// (y sus structs de respuesta en components/schemas).
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
pub struct ApiDoc;
