use std::sync::Arc;

use axum::http::HeaderValue;
use axum::{
    Router, middleware,
    response::Redirect,
    routing::{delete, get, post, put},
};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::request_id::{
    MakeRequestId, PropagateRequestIdLayer, RequestId, SetRequestIdLayer,
};
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tracing::{Level, warn};
use utoipa::OpenApi;
use utoipa_swagger_ui::{Config, SwaggerUi};

use crate::AppState;
use crate::adapters::r#in::http;
use crate::adapters::r#in::http::auth::{ApiKeyRegistry, ApiKeys};
use crate::adapters::r#in::http::openapi::ApiDoc;

// Build the complete router: API routes (protected by API keys if
// configured), public health probes, and Swagger UI.
pub fn create_router(
    state: Arc<AppState>,
    api_keys: Arc<ApiKeyRegistry>,
    cors_allowed_origins: Vec<String>,
    metrics_require_auth: bool,
) -> Router<()> {
    let mut api_routes = Router::new()
        .route("/api/v1/sources", get(http::sources::list_cached_sources))
        .route(
            "/api/v1/sources/{id}/dataset",
            get(http::sources::get_source_dataset),
        )
        .route("/api/v1/sources/{id}", delete(http::cache::evict_source))
        .route(
            "/api/v1/sources/{id}/groups",
            get(http::sources::list_source_groups),
        )
        .route(
            "/api/v1/sources/{id}/hosts",
            get(http::sources::list_source_hosts),
        )
        .route("/api/v1/sources/{id}/sync", post(http::sync::sync_source))
        .route(
            "/api/v1/sources/{id}/status",
            get(http::sources::source_status),
        )
        .route("/api/v1/sources/{id}/scope", get(http::scope::source_scope))
        .route(
            "/api/v1/sources/{id}/hosts/{hostname}",
            put(http::hosts::put_host).delete(http::hosts::delete_host),
        )
        .route("/api/v1/enrichers", get(http::enrichers::list_enrichers))
        .route(
            "/api/v1/enrichers/{id}/run",
            post(http::enrichers::run_enricher),
        )
        .route("/api/v1/endpoints", get(http::endpoints::list_endpoints))
        .route(
            "/api/v1/endpoints/{id}",
            post(http::endpoints::run_endpoint).get(http::endpoints::run_endpoint_get),
        )
        .route("/api/v1/projects", get(http::projects::list_projects))
        .route(
            "/api/v1/projects/{id}/sync",
            post(http::projects::sync_project_now),
        )
        // The configuration directory itself. The two static segments and the
        // {file} capture coexist because a static segment wins the match —
        // there is no configuration file called "reload" or "validate", and
        // even if someone made one, the route would still be the route.
        .route(
            "/api/v1/config",
            get(http::config::get_config).put(http::config::put_config),
        )
        .route(
            "/api/v1/config/validate",
            post(http::config::validate_config),
        )
        .route("/api/v1/config/reload", post(http::config::reload_config))
        .route(
            "/api/v1/config/{file}",
            get(http::config::get_config_file)
                .put(http::config::put_config_file)
                .delete(http::config::delete_config_file),
        );

    // The exposition labels every source id and host count, which describes
    // the inventory topology to anyone who can reach the port. Public is right
    // for a scrape config on a trusted network (Prometheus sends no API key),
    // so it stays the default — but on a shared network the route can move
    // inside the authenticated group instead.
    if metrics_require_auth {
        api_routes = api_routes.route("/metrics", get(http::metrics::metrics));
    }

    let api_routes = api_routes
        .layer(middleware::from_fn(http::auth::require_api_key))
        .layer(axum::Extension(ApiKeys(api_keys)));

    let mut router = Router::new()
        .route("/", get(|| async { Redirect::permanent("/swagger-ui/") }))
        .route("/healthz", get(http::health::healthz))
        .route("/readyz", get(http::health::readyz));

    if !metrics_require_auth {
        router = router.route("/metrics", get(http::metrics::metrics));
    }

    let router = router
        .merge(api_routes)
        .merge(
            SwaggerUi::new("/swagger-ui")
                .url("/api-docs/openapi.json", ApiDoc::openapi())
                .config(swagger_config()),
        )
        .with_state(state);

    // No configured origins = no CORS layer: the browser same-origin policy
    // applies and server-to-server consumers are unaffected. This replaces
    // the old always-on allow-anything layer.
    let router = match cors_layer(&cors_allowed_origins) {
        Some(cors) => router.layer(cors),
        None => router,
    };

    // Request metrics sit inside the compression layer, so the histogram
    // measures handler latency — what the service did — rather than handler
    // plus gzip of the response body.
    let router = router.layer(middleware::from_fn(http::metrics::track_requests));

    // Gzip responses when the client sends Accept-Encoding: gzip (clients
    // that don't are served identity bytes, unchanged). Inventory JSON
    // repeats the same var names for every host, so it compresses ~10x —
    // for WAN consumers (remote federation) transfer time dominates, and
    // this trades a little CPU for most of that.
    let router = router.layer(CompressionLayer::new());

    // The response echoes the request id (inside the trace layer: it copies
    // the id the Set layer below has already assigned).
    let router = router.layer(PropagateRequestIdLayer::x_request_id());

    // One span + response log per request (method, path, status, latency) at
    // INFO, so there are access logs, not just business logs. The span also
    // carries the request id and — filled in by the auth middleware once it
    // knows — the authenticated key's name, so a log line answers WHO did
    // what and an error report quoting an id finds its exact line. Tune
    // verbosity with RUST_LOG (e.g. tower_http=debug for bodies).
    let router = router.layer(
        TraceLayer::new_for_http()
            .make_span_with(|request: &axum::http::Request<axum::body::Body>| {
                let request_id = request
                    .headers()
                    .get("x-request-id")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("-");
                tracing::info_span!(
                    "request",
                    method = %request.method(),
                    uri = %request.uri(),
                    version = ?request.version(),
                    request_id = %request_id,
                    key_name = tracing::field::Empty,
                )
            })
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    );

    // Outermost: assign the request id before anything can log it. A
    // client-provided x-request-id is kept (the layer only fills the header
    // when absent), so a consumer can stitch our lines into its own trace.
    router.layer(SetRequestIdLayer::x_request_id(CounterRequestId::default()))
}

// Request ids from a process-wide counter, not a UUID: the id only needs to
// be unique within one process's log stream (grep it, read the request's
// whole story), a counter does that with no new dependency, and ordered ids
// sort by arrival — useful in themselves. Restarts reuse ids; logs carry
// timestamps, so collisions across boots do not confuse a search bounded to
// an incident window.
#[derive(Clone, Default)]
struct CounterRequestId {
    counter: Arc<std::sync::atomic::AtomicU64>,
}

impl MakeRequestId for CounterRequestId {
    fn make_request_id<B>(&mut self, _request: &axum::http::Request<B>) -> Option<RequestId> {
        let id = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        HeaderValue::from_str(&format!("req-{}", id))
            .ok()
            .map(RequestId::new)
    }
}

// Swagger UI colours every response with highlight.js, which turns the body
// into one DOM node per token. An enterprise dataset (2000 hosts is ~10MB of
// JSON) becomes millions of nodes and the tab stops responding — the server
// answered in milliseconds and the browser never finishes painting it. With
// highlighting off the body is a single <pre>: no colours, but it renders.
//
// This is a property of the UI, not of the response, so it has to be fixed
// here: pagination (?limit=) only helps the caller who remembers to ask for
// it, and routes whose body is script-defined (output endpoints) can't
// paginate at all.
fn swagger_config() -> Config<'static> {
    // Urls are left empty on purpose: the axum adapter fills them in from
    // SwaggerUi::url(), so the spec URL stays declared in one place above.
    Config::default().with_syntax_highlight(false)
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
