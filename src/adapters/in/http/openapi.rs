use utoipa::openapi::security::{ApiKey as OpenApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::adapters::r#in::http;

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
        http::sources::list_source_groups,
        http::sources::list_source_hosts,
        http::sources::source_status,
        http::cache::evict_source,
        http::sync::sync_source,
        http::enrichers::list_enrichers,
        http::enrichers::run_enricher,
        http::hosts::put_host,
        http::hosts::delete_host,
        http::endpoints::run_endpoint,
        http::endpoints::run_endpoint_get,
        http::endpoints::list_endpoints,
        http::projects::list_projects,
        http::projects::sync_project_now,
    ),
    components(schemas(
        http::error::ErrorBody,
        http::sources::CachedSourceInfo,
        http::sources::GroupInfo,
        http::sources::HostList,
        http::sources::HostStatus,
        http::sources::SourceStatus,
        http::sources::SyncHealthInfo,
        http::views::ViewMemberStatus,
        http::cache::EvictResult,
        http::sync::SyncResult,
        http::enrichers::EnrichResult,
        http::endpoints::EndpointInfo,
        http::enrichers::EnricherInfo,
        http::health::ReadyStatus,
        http::projects::ProjectInfo,
        http::projects::ProjectSyncResult,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Sources", description = "Inventory source management, sync, and cache status. Views — read-only composites over several sources — answer on the same routes, in the same shapes: a per-host read is served by whichever member owns that host. The write routes (sync, eviction, host PUT/DELETE) refuse a view id"),
        (name = "Enrichers", description = "Post-processing enrichment of cached data"),
        (name = "Endpoints", description = "Output endpoints for consumers (AWX, AnsibleForms)"),
        (name = "Projects", description = "Git project checkouts (admin-only operational routes)")
    ),
    // No explicit version: utoipa takes it from Cargo.toml (CARGO_PKG_VERSION),
    // so the spec can never disagree with the crate version after a bump
    info(
        title = "Unified API",
        description = "Infrastructure inventory aggregation and caching middleware"
    )
)]
pub struct ApiDoc;
