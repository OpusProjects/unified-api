// Files in tests/ are integration tests — they compile as separate binaries
// and see the project as an external library (that's why we use `unified_api::` instead of `crate::`)
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

// =========================================================================
// Helpers
// =========================================================================

// Makes a GET request to the app and returns (status_code, body_string)
// It's like running curl from within the test, but without TCP — everything in memory.
async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();

    // .oneshot() sends a request to the router and returns the response
    // as if it were an HTTP server but without network — pure logic
    let response = app.oneshot(request).await.unwrap();

    let status = response.status();
    // Extract the full body and convert it to String
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    (status, body_str)
}

// App with a demo inventory preloaded in the cache. It used to live in lib.rs
// (build_app_with_demo_data), but test data has no place in the library —
// it's a fixture of these tests.
fn app_with_demo_data() -> axum::Router {
    let (app, state) = unified_api::AppBuilder::new().build_with_state();

    let demo_dataset: unified_api::domain::dataset::Dataset = serde_json::from_str(
        r#"{
        "hostvars": {
            "motoko.section9.net": {
                "ansible_host": "10.9.1.1",
                "os": "OracleLinux",
                "datacenter": "section9",
                "role": "commander"
            },
            "melchior.seele.net": {
                "ansible_host": "10.6.1.1",
                "os": "OracleLinux",
                "datacenter": "seele",
                "role": "magi-system"
            }
        },
        "groups": {
            "section9": {
                "hosts": ["motoko.section9.net"],
                "vars": {"ntp_server": "ntp.section9.net"}
            },
            "seele": {
                "hosts": ["melchior.seele.net"],
                "vars": {"ntp_server": "ntp.seele.net"}
            }
        }
    }"#,
    )
    .expect("Failed to parse demo dataset");

    state.cache.set(
        "src-demo",
        unified_api::domain::cache_entry::CacheEntry::new(demo_dataset, 3600),
    );

    app
}

// =========================================================================
// Tests: health checks
// =========================================================================

// #[tokio::test] is like #[test] but for async functions
#[tokio::test]
async fn healthz_returns_ok() {
    let app = unified_api::AppBuilder::new().build();

    let (status, body) = get(app, "/healthz").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn readyz_returns_ok_without_sources() {
    let app = unified_api::AppBuilder::new().build();

    let (status, body) = get(app, "/readyz").await;

    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["ready"], true);
    assert_eq!(result["sources_total"], 0);
}

#[tokio::test]
async fn readyz_returns_503_before_sync() {
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "src-test".to_string(),
        unified_api::domain::source::Source {
            name: "Test".to_string(),
            project_id: "test".to_string(),
            script_path: "tests/adapters/out/connectors/inventory.py".to_string(),
            script_args: vec![],
            output_format: Default::default(),
            hosts_from_source: None,
            connector_type: unified_api::domain::source::ConnectorType::Script,
            sync_mode: unified_api::domain::sync_mode::SyncMode::Replace,
            credential_ids: vec![],
            schedule: None,
            sync_interval_seconds: None,
            ttl_seconds: 3600,
            timeout_seconds: 300,
            ttl_overrides: Default::default(),
            config: std::collections::HashMap::new(),
        },
    );
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = get(app, "/readyz").await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["ready"], false);
    assert_eq!(result["sources_pending"][0], "src-test");
}

// =========================================================================
// Tests: sources API without data — empty cache
// =========================================================================

#[tokio::test]
async fn list_sources_empty_cache() {
    let app = unified_api::AppBuilder::new().build();

    let (status, body) = get(app, "/api/v1/sources").await;

    assert_eq!(status, StatusCode::OK);
    // Empty cache = empty JSON array
    let sources: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources.len(), 0);
}

#[tokio::test]
async fn get_dataset_not_found() {
    let app = unified_api::AppBuilder::new().build();

    let (status, _body) = get(app, "/api/v1/sources/no-existe/dataset").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Tests: sources API with demo data — full flow
// =========================================================================

#[tokio::test]
async fn list_sources_with_demo_data() {
    let app = app_with_demo_data();

    let (status, body) = get(app, "/api/v1/sources").await;

    assert_eq!(status, StatusCode::OK);
    let sources: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0]["source_id"], "src-demo");
    assert_eq!(sources[0]["is_fresh"], true);
    assert_eq!(sources[0]["total_hosts"], 2);
}

#[tokio::test]
async fn get_dataset_returns_inventory() {
    let app = app_with_demo_data();

    let (status, body) = get(app, "/api/v1/sources/src-demo/dataset").await;

    assert_eq!(status, StatusCode::OK);

    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();

    // Verify hosts from Section 9 and SEELE
    let hostvars = &dataset["hostvars"];
    assert!(hostvars["motoko.section9.net"].is_object());
    assert_eq!(hostvars["motoko.section9.net"]["ansible_host"], "10.9.1.1");
    assert_eq!(hostvars["motoko.section9.net"]["role"], "commander");

    assert!(hostvars["melchior.seele.net"].is_object());
    assert_eq!(hostvars["melchior.seele.net"]["role"], "magi-system");

    // Verify the groups
    let groups = &dataset["groups"];
    assert!(groups["section9"].is_object());
    assert!(groups["seele"].is_object());

    // Section 9 contains Motoko
    let s9_hosts = groups["section9"]["hosts"].as_array().unwrap();
    assert!(s9_hosts.contains(&serde_json::json!("motoko.section9.net")));
}

#[tokio::test]
async fn nonexistent_route_returns_404() {
    let app = unified_api::AppBuilder::new().build();

    let (status, _body) = get(app, "/api/v1/ruta-inventada").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Test: the OpenAPI spec version tracks Cargo.toml, never a hardcoded string
// =========================================================================
#[tokio::test]
async fn openapi_version_matches_crate_version() {
    let app = unified_api::AppBuilder::new().build();

    let (status, body) = get(app, "/api-docs/openapi.json").await;

    assert_eq!(status, StatusCode::OK);
    let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(spec["info"]["version"], env!("CARGO_PKG_VERSION"));
}

// =========================================================================
// Test: /metrics exposes sync counters after a sync runs
// =========================================================================
#[tokio::test]
async fn metrics_exposes_sync_counters() {
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "src-metrics".to_string(),
        unified_api::domain::source::Source {
            name: "Metrics Test".to_string(),
            project_id: "test".to_string(),
            script_path: "tests/adapters/out/connectors/inventory.py".to_string(),
            script_args: vec![],
            output_format: Default::default(),
            hosts_from_source: None,
            connector_type: unified_api::domain::source::ConnectorType::Script,
            sync_mode: unified_api::domain::sync_mode::SyncMode::Replace,
            credential_ids: vec![],
            schedule: None,
            sync_interval_seconds: None,
            ttl_seconds: 3600,
            timeout_seconds: 300,
            ttl_overrides: Default::default(),
            config: std::collections::HashMap::new(),
        },
    );
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // Run a sync so there is something to measure
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/sources/src-metrics/sync")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (status, body) = get(app, "/metrics").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("unified_api_sync_total"),
        "missing counter in: {}",
        body
    );
    assert!(body.contains("unified_api_sync_duration_seconds"));
    assert!(body.contains("src-metrics"));
}

// =========================================================================
// Tests: CORS is off by default, opt-in via allowed origins
// =========================================================================
#[tokio::test]
async fn cors_disabled_by_default() {
    let app = unified_api::AppBuilder::new().build();

    let request = Request::builder()
        .uri("/healthz")
        .header("origin", "https://evil.example")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("access-control-allow-origin")
            .is_none()
    );
}

#[tokio::test]
async fn cors_allows_configured_origin() {
    let app = unified_api::AppBuilder::new()
        .cors_allowed_origins(vec!["https://forms.example".to_string()])
        .build();

    let request = Request::builder()
        .uri("/healthz")
        .header("origin", "https://forms.example")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://forms.example")
    );
}

#[tokio::test]
async fn cors_wildcard_allows_any_origin() {
    let app = unified_api::AppBuilder::new()
        .cors_allowed_origins(vec!["*".to_string()])
        .build();

    let request = Request::builder()
        .uri("/healthz")
        .header("origin", "https://anywhere.example")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*")
    );
}

#[tokio::test]
async fn cors_skips_invalid_origin_keeps_valid() {
    // A bad origin is warned-and-dropped, not fatal; the valid one still works
    let app = unified_api::AppBuilder::new()
        .cors_allowed_origins(vec![
            "not a valid origin".to_string(),
            "https://ok.example".to_string(),
        ])
        .build();

    let request = Request::builder()
        .uri("/healthz")
        .header("origin", "https://ok.example")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://ok.example")
    );
}

// =========================================================================
// Tests: scoped API keys
// =========================================================================

use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};

// Same as get() but authenticating with an API key header
async fn get_with_key(app: axum::Router, path: &str, key: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .header("x-api-key", key)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

// Two cached sources, one admin key and one key scoped to src-alpha only
fn app_with_scoped_keys() -> axum::Router {
    let keys = vec![
        ResolvedApiKey {
            name: "admin".to_string(),
            secret: "admin-secret".to_string(),
            permissions: Permissions::Admin,
        },
        ResolvedApiKey {
            name: "alpha-only".to_string(),
            secret: "alpha-secret".to_string(),
            permissions: Permissions::Scoped {
                sources: ["src-alpha".to_string()].into_iter().collect(),
                endpoints: std::collections::HashSet::new(),
            },
        },
    ];

    let (app, state) = unified_api::AppBuilder::new()
        .api_keys(keys)
        .build_with_state();

    let empty: unified_api::domain::dataset::Dataset =
        serde_json::from_str(r#"{"hostvars": {}, "groups": {}}"#).unwrap();
    state.cache.set(
        "src-alpha",
        unified_api::domain::cache_entry::CacheEntry::new(empty.clone(), 3600),
    );
    state.cache.set(
        "src-beta",
        unified_api::domain::cache_entry::CacheEntry::new(empty, 3600),
    );

    app
}

#[tokio::test]
async fn admin_key_sees_all_sources() {
    let (status, body) =
        get_with_key(app_with_scoped_keys(), "/api/v1/sources", "admin-secret").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("src-alpha"));
    assert!(body.contains("src-beta"));
}

#[tokio::test]
async fn scoped_key_list_is_filtered() {
    let (status, body) =
        get_with_key(app_with_scoped_keys(), "/api/v1/sources", "alpha-secret").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("src-alpha"));
    // The other source is invisible, not an error
    assert!(!body.contains("src-beta"));
}

#[tokio::test]
async fn scoped_key_reads_allowed_source() {
    let (status, _) = get_with_key(
        app_with_scoped_keys(),
        "/api/v1/sources/src-alpha/dataset",
        "alpha-secret",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn scoped_key_gets_403_on_other_source() {
    let (status, _) = get_with_key(
        app_with_scoped_keys(),
        "/api/v1/sources/src-beta/dataset",
        "alpha-secret",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn scoped_key_gets_403_on_other_source_status() {
    let (status, _) = get_with_key(
        app_with_scoped_keys(),
        "/api/v1/sources/src-beta/status",
        "alpha-secret",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn wrong_key_still_401() {
    let (status, _) = get_with_key(app_with_scoped_keys(), "/api/v1/sources", "nope").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn scoped_key_cannot_run_unlisted_endpoint() {
    // Endpoints require configuration; an empty scoped key must get 403
    // before the 404 lookup (the id is not theirs to probe).
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/endpoints/ep-anything")
        .header("x-api-key", "alpha-secret")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app_with_scoped_keys().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn legacy_single_api_key_is_admin() {
    // The old .api_key(Some(...)) path must keep behaving as full access
    let (app, state) = unified_api::AppBuilder::new()
        .api_key(Some("legacy-secret".to_string()))
        .build_with_state();

    let empty: unified_api::domain::dataset::Dataset =
        serde_json::from_str(r#"{"hostvars": {}, "groups": {}}"#).unwrap();
    state.cache.set(
        "src-any",
        unified_api::domain::cache_entry::CacheEntry::new(empty, 3600),
    );

    let (status, body) = get_with_key(app, "/api/v1/sources", "legacy-secret").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("src-any"));
}

// =========================================================================
// Tests: dataset pagination and filtering
// =========================================================================

#[tokio::test]
async fn dataset_without_params_is_the_raw_shape() {
    let (status, body) = get(app_with_demo_data(), "/api/v1/sources/src-demo/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    // raw Dataset: hostvars/groups at the top, no pagination envelope
    assert!(json.get("hostvars").is_some());
    assert!(json.get("total_hosts").is_none());
    assert_eq!(json["hostvars"].as_object().unwrap().len(), 2);
}

#[tokio::test]
async fn dataset_with_limit_pages_sorted_hosts() {
    // page 1: hosts sorted by name → melchior.seele.net comes first
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?limit=1",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 2);
    assert_eq!(json["returned"], 1);
    assert_eq!(json["offset"], 0);
    assert!(json["hostvars"].get("melchior.seele.net").is_some());

    // page 2
    let (_, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?limit=1&offset=1",
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["hostvars"].get("motoko.section9.net").is_some());
    assert_eq!(json["returned"], 1);
}

#[tokio::test]
async fn dataset_group_filter_returns_only_that_group() {
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?group=section9",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 1);
    assert!(json["hostvars"].get("motoko.section9.net").is_some());
    assert!(json["hostvars"].get("melchior.seele.net").is_none());
    // only the filtered group comes back
    assert_eq!(json["groups"].as_object().unwrap().len(), 1);
    assert!(json["groups"].get("section9").is_some());
}

#[tokio::test]
async fn dataset_host_filter_and_not_found_cases() {
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?host=motoko.section9.net",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 1);

    let (status, _) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?host=ghost.example.com",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?group=ghosts",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn dataset_offset_beyond_total_returns_empty_page() {
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?limit=10&offset=99",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 2);
    assert_eq!(json["returned"], 0);
}
