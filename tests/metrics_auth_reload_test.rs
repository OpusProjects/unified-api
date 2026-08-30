// server.metrics_require_auth through a live reload: the flag used to decide
// where /metrics was REGISTERED, which no reload could change. Now the
// handler checks it per scrape, so a configuration push flips it with no
// restart — in both directions.
//
// Its own test binary on purpose: the metrics recorder is a process-wide
// global, and config_api_test already has a test asserting unlabeled gauge
// values that a concurrent scrape from another app would race.
use axum::Router;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;
use unified_api::adapters::out::config::fs::FsConfigStore;
use unified_api::config::{RestartOnlySettings, load_config};

const KEY: &str = "admin-secret";
const OPEN: &str = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n";
const LOCKED: &str = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  metrics_require_auth: true\n";

fn app_at(dir: &Path) -> Router {
    let cfg = load_config(dir.to_str().expect("utf-8 path")).expect("fixture must load");
    let live = RestartOnlySettings::from_config(&cfg);
    let keys = unified_api::adapters::r#in::http::auth::resolve_api_keys(&cfg)
        .expect("fixture keys resolve");
    let (app, _) = unified_api::AppBuilder::new()
        .from_config(&cfg)
        .api_keys(keys)
        .config_api(Arc::new(FsConfigStore::new(dir)), live)
        .build_with_state();
    app
}

async fn scrape(app: &Router, key: Option<&str>) -> StatusCode {
    let mut request = Request::builder().uri("/metrics");
    if let Some(key) = key {
        request = request.header("x-api-key", key);
    }
    let response = app
        .clone()
        .oneshot(request.body(axum::body::Body::empty()).expect("request"))
        .await
        .expect("response");
    response.status()
}

async fn reload_config_yaml(app: &Router, contents: &str) {
    let request = Request::builder()
        .method("PUT")
        .uri("/api/v1/config/config.yaml?reload=true")
        .header("x-api-key", KEY)
        .header("content-type", "application/yaml")
        .body(axum::body::Body::from(contents.to_string()))
        .expect("request");
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "reload failed: {}",
        String::from_utf8_lossy(&body)
    );
}

#[tokio::test]
async fn metrics_auth_flips_on_a_reload_in_both_directions() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(dir.path().join("config.yaml"), OPEN).expect("fixture");
    std::fs::write(
        dir.path().join("api_keys.yaml"),
        "key-admin:\n  name: \"admin\"\n  env: \"UNIFIED_API_TEST_KEY_METRICS_FLIP\"\n  role: \"admin\"\n",
    )
    .expect("fixture");
    // SAFETY: the name is unique to this test binary and the value never
    // changes, so nothing else can observe a different one.
    unsafe { std::env::set_var("UNIFIED_API_TEST_KEY_METRICS_FLIP", KEY) };
    let app = app_at(dir.path());

    // The default: public, like a Prometheus scrape config expects.
    assert_eq!(scrape(&app, None).await, StatusCode::OK);

    // Lock it over the API — no restart anywhere in this test.
    reload_config_yaml(&app, LOCKED).await;
    assert_eq!(
        scrape(&app, None).await,
        StatusCode::UNAUTHORIZED,
        "the very next scrape must require the key"
    );
    assert_eq!(scrape(&app, Some("wrong")).await, StatusCode::UNAUTHORIZED);
    assert_eq!(scrape(&app, Some(KEY)).await, StatusCode::OK);

    // And back: unlocking must also take effect live, or an operator who
    // locked the wrong fleet would need a rolling restart to recover.
    reload_config_yaml(&app, OPEN).await;
    assert_eq!(scrape(&app, None).await, StatusCode::OK);
}
