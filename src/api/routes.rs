use std::sync::Arc;

use axum::{middleware, response::Redirect, routing::{get, post, put}, Router};
use tower_http::cors::{Any, CorsLayer};
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::api;
use crate::api::auth::ApiKey;
use crate::api::openapi::ApiDoc;
use crate::AppState;

// Monta el router completo: rutas de la API (protegidas por API key si
// hay una configurada), health probes públicos y el Swagger UI.
pub fn create_router(state: Arc<AppState>, api_key: Option<String>) -> Router<()> {
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
