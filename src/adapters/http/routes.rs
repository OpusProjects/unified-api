use std::sync::Arc;

use axum::{
    Router, middleware,
    response::Redirect,
    routing::{get, post, put},
};
use tower_http::cors::{Any, CorsLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::AppState;
use crate::adapters::http;
use crate::adapters::http::auth::ApiKey;
use crate::adapters::http::openapi::ApiDoc;

// Build the complete router: API routes (protected by API key if
// configured), public health probes, and Swagger UI.
pub fn create_router(state: Arc<AppState>, api_key: Option<String>) -> Router<()> {
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

    Router::new()
        .route("/", get(|| async { Redirect::permanent("/swagger-ui/") }))
        .route("/healthz", get(http::health::healthz))
        .route("/readyz", get(http::health::readyz))
        .route("/metrics", get(http::metrics::metrics))
        .merge(api_routes)
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
}
