use utoipa::openapi::security::{ApiKey as OpenApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::adapters::http;

// Add the security scheme (X-API-Key header) to the generated spec
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

// The complete OpenAPI spec: utoipa generates it at compile-time from
// #[utoipa::path] attributes of each handler listed here.
// A new handler doesn't appear in Swagger until registered in paths()
// (and its response structs in components/schemas).
#[derive(OpenApi)]
#[openapi(
    modifiers(&SecurityAddon),
    security(
        ("api_key" = [])
    ),
    paths(
        http::health::healthz,
        http::health::readyz,
        http::sources::list_cached_sources,
        http::sources::get_source_dataset,
        http::sources::source_status,
        http::sync::sync_source,
        http::enrichers::run_enricher,
        http::hosts::put_host,
        http::hosts::delete_host,
        http::endpoints::run_endpoint,
        http::endpoints::list_endpoints,
    ),
    components(schemas(
        http::sources::CachedSourceInfo,
        http::sources::HostStatus,
        http::sources::SourceStatus,
        http::sync::SyncResult,
        http::enrichers::EnrichResult,
        http::endpoints::EndpointInfo,
        http::health::ReadyStatus,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Sources", description = "Inventory source management, sync, and cache status"),
        (name = "Enrichers", description = "Post-processing enrichment of cached data"),
        (name = "Endpoints", description = "Output endpoints for consumers (AWX, AnsibleForms)")
    ),
    // No explicit version: utoipa takes it from Cargo.toml (CARGO_PKG_VERSION),
    // so the spec can never disagree with the crate version after a bump
    info(
        title = "Unified API",
        description = "Infrastructure inventory aggregation and caching middleware"
    )
)]
pub struct ApiDoc;
