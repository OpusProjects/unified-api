use std::sync::Arc;

use axum::http::HeaderValue;
use axum::{
    Router, middleware,
    response::Redirect,
    routing::{get, post, put},
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};
use tracing::{Level, warn};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::AppState;
use crate::adapters::http;
use crate::adapters::http::auth::ApiKey;
use crate::adapters::http::openapi::ApiDoc;

// Build the complete router: API routes (protected by API key if
// configured), public health probes, and Swagger UI.
pub fn create_router(
    state: Arc<AppState>,
    api_key: Option<String>,
    cors_allowed_origins: Vec<String>,
) -> Router<()> {
    let api_routes = Router::new()
        .route("/api/v1/sources", get(http::sources::list_cached_sources))
        .route(
            "/api/v1/sources/{id}/dataset",
            get(http::sources::get_source_dataset),
        )
        .route("/api/v1/sources/{id}/sync", post(http::sync::sync_source))
        .route(
            "/api/v1/sources/{id}/status",
            get(http::sources::source_status),
        )
        .route(
            "/api/v1/sources/{id}/hosts/{hostname}",
            put(http::hosts::put_host).delete(http::hosts::delete_host),
        )
        .route(
            "/api/v1/enrichers/{id}/run",
            post(http::enrichers::run_enricher),
        )
        .route("/api/v1/endpoints", get(http::endpoints::list_endpoints))
        .route(
            "/api/v1/endpoints/{id}",
            post(http::endpoints::run_endpoint),
        )
        .layer(middleware::from_fn(http::auth::require_api_key))
        .layer(axum::Extension(ApiKey(api_key)));

    let router = Router::new()
        .route("/", get(|| async { Redirect::permanent("/swagger-ui/") }))
        .route("/healthz", get(http::health::healthz))
        .route("/readyz", get(http::health::readyz))
        .route("/metrics", get(http::metrics::metrics))
        .merge(api_routes)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .with_state(state);

    // No configured origins = no CORS layer: the browser same-origin policy
    // applies and server-to-server consumers are unaffected. This replaces
    // the old always-on allow-anything layer.
    let router = match cors_layer(&cors_allowed_origins) {
        Some(cors) => router.layer(cors),
        None => router,
    };

    // Outermost layer: one span + response log per request (method, path,
    // status, latency) at INFO, so there are access logs, not just business
    // logs. Tune verbosity with RUST_LOG (e.g. tower_http=debug for bodies).
    router.layer(
        TraceLayer::new_for_http()
            .make_span_with(DefaultMakeSpan::new().level(Level::INFO))
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    )
}

fn cors_layer(origins: &[String]) -> Option<CorsLayer> {
    if origins.is_empty() {
        return None;
    }

    let layer = if origins.iter().any(|o| o == "*") {
        CorsLayer::new().allow_origin(Any)
    } else {
        // Parse each origin, warning (not silently dropping) on a bad one so a
        // typo in config.yaml doesn't fail closed with no explanation.
        let list: Vec<HeaderValue> = origins
            .iter()
            .filter_map(|o| match o.parse() {
                Ok(value) => Some(value),
                Err(_) => {
                    warn!(origin = %o, "ignoring invalid CORS origin");
                    None
                }
            })
            .collect();
        CorsLayer::new().allow_origin(list)
    };

    Some(layer.allow_methods(Any).allow_headers(Any))
}
