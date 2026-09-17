use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::HashMap;
use tower::ServiceExt;

// The browsable API, end to end through the real router.
//
// The thing worth protecting here is not that the pages are pretty: it is that

fn app_with_ui(enabled: bool) -> axum::Router {
    let ui = unified_api::config::UiConfig {
        enabled,
        ..Default::default()
    };
    unified_api::AppBuilder::new()
        .sources(HashMap::new())
        .ui(ui)
        .build()
}

async fn get(app: axum::Router, uri: &str, accept: &str) -> (StatusCode, String, String) {
    let resp = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("accept", accept)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        content_type,
        String::from_utf8_lossy(&bytes).to_string(),
    )
}

#[tokio::test]
async fn with_the_ui_off_a_browser_still_gets_json() {
    let app = app_with_ui(false);
    let (status, content_type, body) = get(app, "/api/v1/sources", "text/html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        content_type.starts_with("application/json"),
        "content-type was {content_type}"
    );
    assert!(body.starts_with('['));
}

#[tokio::test]
async fn with_the_ui_off_there_is_no_login_form() {
    let app = app_with_ui(false);
    let (status, _, _) = get(app.clone(), "/api/v1/login", "text/html").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a deployment that did not ask for the UI must not grow a login page"
    );
    let (status, _, _) = get(app, "/api/v1/logout", "text/html").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn with_the_ui_on_a_browser_gets_a_page() {
    let app = app_with_ui(true);
    let (status, content_type, body) = get(app, "/api/v1/sources", "text/html").await;
    assert_eq!(status, StatusCode::OK);
    assert!(content_type.starts_with("text/html"));
    assert!(body.contains("<!DOCTYPE html>"));
}

// The same route, the same data, decided by one header.
#[tokio::test]
async fn a_json_client_is_unaffected_either_way() {
    for enabled in [false, true] {
        let app = app_with_ui(enabled);
        let (status, content_type, body) = get(app, "/api/v1/sources", "application/json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            content_type.starts_with("application/json"),
            "ui.enabled={enabled} gave {content_type}"
        );
        assert!(body.starts_with('['));
    }
}

#[tokio::test]
async fn a_rendered_page_is_never_cached_and_never_framed() {
    let app = app_with_ui(true);
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/sources")
                .header("accept", "text/html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };

    assert!(
        header("cache-control").contains("no-store"),
        "a page carrying a CSRF token must not be storable by a shared cache"
    );
    assert!(header("content-security-policy").contains("frame-ancestors 'none'"));
}

#[tokio::test]
async fn the_index_lists_the_collections() {
    let app = app_with_ui(false);
    let (status, _, body) = get(app, "/api/v1/", "application/json").await;
    assert_eq!(status, StatusCode::OK);
    let index: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(index["sources"], "/api/v1/sources");
    assert_eq!(index["enrichers"], "/api/v1/enrichers");
    assert_eq!(index["version"], env!("CARGO_PKG_VERSION"));

    // Without a trailing slash too: nobody should have to guess ours
    let app = app_with_ui(false);
    let (status, _, _) = get(app, "/api/v1", "application/json").await;
    assert_eq!(status, StatusCode::OK);
}
