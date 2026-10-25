// The built unified-ui, served at "/" when `server.ui.assets_dir` names one:
// the UI and the API on one address, so the UI's calls are same-origin and the
// login's session cookie covers both, with no proxy in front.
//
// Three kinds of path reach this fallback (everything the router does not
// match):
//   - a file of the build (/assets/index-3f2a.js, /favicon.svg): served as is;
//   - a page of the app (/sources, /topology/...): no such file, so the app's
//     index.html, whose own router draws the page;
//   - an API path nothing matched (/api/v1/typo): the API's own JSON 404, never
//     the app, or a client would read a 200 HTML page as an answer.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

use crate::adapters::r#in::http::error::ApiError;

#[derive(Clone)]
pub struct UiAssets {
    dir: PathBuf,
}

impl UiAssets {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        // Not fatal: the API works without its UI. Said at startup, where a
        // wrong path in a deployment is noticed, rather than as a 404 later.
        if !dir.join("index.html").is_file() {
            tracing::warn!(
                dir = %dir.display(),
                "server.ui.assets_dir has no index.html: the UI will not load"
            );
        }
        Self { dir }
    }

    fn index(&self) -> PathBuf {
        self.dir.join("index.html")
    }
}

// Paths that belong to the API: a miss there is the API's 404, not the app.
fn is_api_path(path: &str) -> bool {
    ["/api", "/swagger-ui", "/api-docs"]
        .iter()
        .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")))
}

pub async fn serve(State(assets): State<UiAssets>, request: Request) -> Response {
    let path = request.uri().path().to_string();
    if is_api_path(&path) {
        return ApiError::not_found(format!("no route for {path}")).into_response();
    }
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return ApiError::new(
            axum::http::StatusCode::METHOD_NOT_ALLOWED,
            "the UI's files are read-only",
        )
        .into_response();
    }

    let service = ServeDir::new(&assets.dir).fallback(ServeFile::new(assets.index()));
    let response = match service.oneshot(request).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    };
    with_cache_policy(&path, response)
}

// Vite names every built file after its content (index-3f2a.js), so a file
// under /assets never changes and can be kept for good; everything else (the
// index.html that names those files) must be asked again on every load, or a
// browser would keep running the previous release's app.
fn with_cache_policy(path: &str, mut response: Response) -> Response {
    let policy = if path.starts_with("/assets/") && response.status().is_success() {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(policy));
    response
}

// /metrics is two things on one path: Prometheus's endpoint and the UI's
// Metrics page. A browser navigating there asks for HTML and gets the app;
// a scraper, or the page's own fetch, never does and gets the metrics.
pub async fn page_for_browsers(
    State(assets): State<UiAssets>,
    request: Request,
    next: Next,
) -> Response {
    let wants_html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));
    if !wants_html {
        return next.run(request).await;
    }
    let response = match ServeFile::new(assets.index()).oneshot(request).await {
        Ok(response) => response.map(Body::new),
        Err(never) => match never {},
    };
    with_cache_policy("/metrics", response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths_are_never_the_app() {
        for path in [
            "/api",
            "/api/v1/nope",
            "/swagger-ui/",
            "/api-docs/openapi.json",
        ] {
            assert!(is_api_path(path), "{path}");
        }
        for path in ["/", "/sources", "/apis", "/assets/index.js", "/metrics"] {
            assert!(!is_api_path(path), "{path}");
        }
    }

    #[test]
    fn only_built_files_are_cached_for_good() {
        let ok = || Response::new(Body::empty());
        let cache = |r: Response| {
            r.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .to_string()
        };
        assert!(cache(with_cache_policy("/assets/index-3f2a.js", ok())).contains("immutable"));
        assert_eq!(cache(with_cache_policy("/sources", ok())), "no-cache");
        assert_eq!(cache(with_cache_policy("/", ok())), "no-cache");
    }
}
