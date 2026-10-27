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
        http::index::api_index,
        http::health::healthz,
        http::health::readyz,
        http::metrics::metrics,
        http::sources::list_cached_sources,
        http::detail::get_source,
        http::sources::get_source_dataset,
        http::sources::list_source_groups,
        http::sources::get_source_group,
        http::sources::list_source_hosts,
        http::sources::source_status,
        http::scope::source_scope,
        http::cache::evict_source,
        http::sync::sync_source,
        http::enrichers::list_enrichers,
        http::enrichers::run_enricher,
        http::hosts::put_host,
        http::hosts::delete_host,
        http::endpoints::run_endpoint,
        http::endpoints::run_endpoint_get,
        http::endpoints::endpoint_counts,
        http::credentials::list_credentials,
        http::settings::get_settings,
        http::host_search::search_hosts,
        http::endpoints::list_endpoints,
        http::projects::list_projects,
        http::connectors::list_connectors,
        http::connectors::get_connector,
        http::users::get_auth,
        http::topology::get_topology,
        http::topology::get_federation,
        http::topology::put_federation,
        http::projects::sync_project_now,
        http::config::get_config,
        http::config::get_config_file,
        http::config::put_config,
        http::config::put_config_file,
        http::config::delete_config_file,
        http::config::validate_config,
        http::config::reload_config,
    ),
    components(schemas(
        http::error::ErrorBody,
        http::index::ApiIndex,
        http::sources::CachedSourceInfo,
        http::detail::SourceDetail,
        http::detail::Gathering,
        http::sources::GroupInfo,
        http::sources::GroupDetail,
        http::sources::HostList,
        http::sources::GroupsMode,
        http::sources::HostStatus,
        http::sources::SourceStatus,
        http::sources::SyncHealthInfo,
        http::sources::RunInfo,
        http::scope::ScopeInfo,
        http::views::ViewMemberStatus,
        http::cache::EvictResult,
        http::sync::SyncResult,
        http::enrichers::EnrichResult,
        http::endpoints::EndpointInfo,
        http::endpoints::EndpointTransform,
        http::endpoints::EndpointCounts,
        http::endpoints::EndpointUnavailableBody,
        http::enrichers::EnricherInfo,
        http::health::ReadyStatus,
        http::projects::ProjectInfo,
        http::projects::ProjectUsers,
        http::connectors::ConnectorInfo,
        http::connectors::ConnectorUser,
        http::connectors::ConnectorHealth,
        http::credentials::CredentialInfo,
        http::credentials::FileKeyInfo,
        http::credentials::VaultConnectionInfo,
        http::credentials::CredentialUsersInfo,
        http::settings::Settings,
        http::host_search::HostSearchResult,
        http::host_search::FoundHost,
        http::host_search::FoundIn,
        http::settings::ServerSettings,
        http::settings::UiSettings,
        http::settings::CacheSettings,
        http::settings::ProjectsSettings,
        http::settings::SecretsSettings,
        http::settings::VaultSettings,
        http::settings::ConfigApiSettings,
        http::users::AuthInfo,
        crate::domain::topology::Topology,
        crate::domain::topology::Peer,
        crate::domain::topology::PeerSource,
        crate::domain::topology::FederationPush,
        crate::domain::topology::Federation,
        crate::domain::topology::InstanceSummary,
        http::users::Me,
        http::users::Methods,
        http::users::LdapInfo,
        http::users::AccessRuleInfo,
        http::users::ApiKeyInfo,
        http::projects::ProjectSyncResult,
        http::config::ConfigBundle,
        http::config::ConfigFileInfo,
        http::config::ConfigInventory,
        http::config::ConfigRejected,
        http::config::ConfigSummary,
        http::config::DeltaInfo,
        http::config::ReloadInfo,
        http::config::ValidationResult,
        http::config::WriteResult,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Sources", description = "Inventory source management, sync, and cache status. Views — read-only composites over several sources — answer on the same routes, in the same shapes: a per-host read is served by whichever member owns that host. The write routes (sync, eviction, host PUT/DELETE) refuse a view id"),
        (name = "Enrichers", description = "Post-processing enrichment of cached data"),
        (name = "Endpoints", description = "Output endpoints for consumers (AWX, AnsibleForms)"),
        (name = "Projects", description = "Git project checkouts (admin-only operational routes)"),
        (name = "Connectors", description = "How this instance talks to each kind of system — the script, its project and arguments — and which sources use each connector (admin-only)"),
        (name = "Credentials", description = "Configured credentials: where each one's secrets come from, whether they resolve now, and what uses it (admin-only; never a secret value)"),
        (name = "Topology", description = "The federation seen from this instance: the unified-api instances it pulls from, and theirs"),
        (name = "Auth", description = "Who the caller is, and the ways in: LDAP login and API keys (read-only; declared in git)"),
        (name = "Configuration", description = "The configuration directory over HTTP (admin-only): read it, validate a proposed change exactly as --check-config would, write it atomically, and reload it into the running process. Off unless config_api.enabled is set")
    ),
    // No explicit version: utoipa takes it from Cargo.toml (CARGO_PKG_VERSION),
    // so the spec can never disagree with the crate version after a bump
    info(
        title = "Unified API",
        description = "Infrastructure inventory aggregation and caching middleware"
    )
)]
pub struct ApiDoc;
