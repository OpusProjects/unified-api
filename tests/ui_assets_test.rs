use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use std::collections::HashMap;
use tower::ServiceExt;

// The built UI served by the API itself (server.ui.assets_dir), end to end
// through the real router, against a stand-in build in a temp directory.

const INDEX: &str = "<!doctype html><title>unified-api</title><div id=root></div>";

struct Served {
    app: axum::Router,
    // Held so the directory outlives the requests.
    _dir: tempfile::TempDir,
}

fn app(enabled: bool) -> Served {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), INDEX).unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/index-3f2a.js"), "console.log(1)").unwrap();
    let ui = unified_api::config::UiConfig {
        enabled,
        assets_dir: Some(dir.path().to_str().unwrap().to_string()),
        ..Default::default()
    };
    Served {
        app: unified_api::AppBuilder::new()
            .sources(HashMap::new())
            .ui(ui)
            .build(),
        _dir: dir,
    }
}

async fn get(app: &axum::Router, uri: &str, accept: &str) -> (StatusCode, String, String, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header(header::ACCEPT, accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let header_of = |name| {
        resp.headers()
            .get(name)
            .and_then(|v: &header::HeaderValue| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (content_type, cache) = (
        header_of(header::CONTENT_TYPE),
        header_of(header::CACHE_CONTROL),
    );
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        content_type,
        cache,
        String::from_utf8_lossy(&bytes).to_string(),
    )
}

const BROWSER: &str = "text/html,application/xhtml+xml,*/*;q=0.8";

#[tokio::test]
async fn the_app_loads_at_the_root_and_on_every_page_of_it() {
    let served = app(true);
    for path in ["/", "/sources", "/sources/src-a?host=web-01", "/topology"] {
        let (status, content_type, cache, body) = get(&served.app, path, BROWSER).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            content_type.starts_with("text/html"),
            "{path}: {content_type}"
        );
        assert_eq!(body, INDEX, "{path}");
        // The page names this release's files: never kept across a release.
        assert_eq!(cache, "no-cache", "{path}");
    }
}

#[tokio::test]
async fn built_files_are_served_and_kept_for_good() {
    let served = app(true);
    let (status, content_type, cache, body) =
        get(&served.app, "/assets/index-3f2a.js", "*/*").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.contains("javascript"), "{content_type}");
    assert!(cache.contains("immutable"), "{cache}");
    assert_eq!(body, "console.log(1)");
}

// An API path nothing matched must stay the API's 404: a client reading a
// 200 HTML page as an answer is worse than an error.
#[tokio::test]
async fn an_unknown_api_path_is_still_a_json_404() {
    let served = app(true);
    for path in ["/api/v1/no-such-route", "/api", "/api-docs/nope"] {
        let (status, content_type, _, body) = get(&served.app, path, "application/json").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(content_type.starts_with("application/json"), "{path}");
        assert!(body.contains("\"error\""), "{path}: {body}");
    }
}

#[tokio::test]
async fn the_api_answers_as_before() {
    let served = app(true);
    let (status, content_type, _, body) =
        get(&served.app, "/api/v1/sources", "application/json").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("application/json"));
    assert!(body.starts_with('['));
    let (status, _, _, _) = get(&served.app, "/healthz", "*/*").await;
    assert_eq!(status, StatusCode::OK);
}

// /metrics: the Metrics page for a browser, the exposition for Prometheus.
#[tokio::test]
async fn metrics_is_a_page_for_a_browser_and_text_for_a_scraper() {
    let served = app(true);
    let (_, content_type, _, body) = get(&served.app, "/metrics", BROWSER).await;
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert_eq!(body, INDEX);
    let (status, content_type, _, body) = get(&served.app, "/metrics", "text/plain").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/plain"), "{content_type}");
    assert!(!body.contains("<!doctype"));
}

// The UI signs in through the login page, which exists only with the UI on:
// assets alone serve nothing (and config validation refuses the pair).
#[tokio::test]
async fn without_the_browsable_ui_nothing_is_served() {
    let served = app(false);
    let (status, _, _, body) = get(&served.app, "/sources", BROWSER).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body, INDEX);
}

// A write to a page of the app is not a thing: refused, not the index.
#[tokio::test]
async fn the_ui_files_are_read_only() {
    let served = app(true);
    let resp = served
        .app
        .clone()
        .oneshot(Request::post("/sources").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

// With keys configured, a browser that is not signed in is sent to the login
// form by the server, with the page it asked for as `next`: the app never
// loads only to find out it is signed out (a flash of the overview, then the
// login page). Its files still load: the login page needs some, and they are
// not secrets.
#[tokio::test]
async fn a_signed_out_browser_goes_to_the_login_form_before_the_app_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("index.html"), INDEX).unwrap();
    std::fs::create_dir(dir.path().join("assets")).unwrap();
    std::fs::write(dir.path().join("assets/index-3f2a.js"), "console.log(1)").unwrap();
    let ui = unified_api::config::UiConfig {
        enabled: true,
        assets_dir: Some(dir.path().to_str().unwrap().to_string()),
        ..Default::default()
    };
    let app = unified_api::AppBuilder::new()
        .ui(ui)
        .api_key(Some("secret".into()))
        .build();

    let resp = app
        .clone()
        .oneshot(
            Request::get("/sources/src-a?x=1")
                .header(header::ACCEPT, BROWSER)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_redirection(), "{}", resp.status());
    assert_eq!(
        resp.headers()[header::LOCATION],
        "/api/v1/login?next=/sources/src-a%3Fx%3D1"
    );

    // The app's files are not gated.
    let (status, _, _, body) = get(&app, "/assets/index-3f2a.js", "*/*").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "console.log(1)");

    // A session that does not exist (expired, or made up) is not signed in.
    let resp = app
        .clone()
        .oneshot(
            Request::get("/")
                .header(header::ACCEPT, BROWSER)
                .header(header::COOKIE, "uapi_session=made-up")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.headers()[header::LOCATION], "/api/v1/login?next=/");
}
