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
    app_with_demo_data_and_state().0
}

// Same fixture, but also handing back the AppState for tests that need to
// mutate the cache mid-test (e.g. to observe an ETag change)
fn app_with_demo_data_and_state() -> (axum::Router, std::sync::Arc<unified_api::AppState>) {
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

    (app, state)
}

// An app whose cached group lists the same host twice — what a connector
// emitting a host under two nested groups produces once they are merged.
fn app_with_duplicate_group_members() -> axum::Router {
    let (app, state) = unified_api::AppBuilder::new().build_with_state();

    let dataset: unified_api::domain::dataset::Dataset = serde_json::from_str(
        r#"{
        "hostvars": {"motoko.section9.net": {"os": "OracleLinux"}, "batou.section9.net": {}},
        "groups": {
            "section9": {
                "hosts": ["motoko.section9.net", "batou.section9.net", "motoko.section9.net"]
            }
        }
    }"#,
    )
    .expect("Failed to parse dataset");

    state.cache.set(
        "src-dupes",
        unified_api::domain::cache_entry::CacheEntry::new(dataset, 3600),
    );

    app
}

// A minimal script source. Every field of Source has to be given a value, and
// the tests that need a *configured* source (rather than just a cache entry)
// only ever vary the script path.
fn script_source(script_path: &str) -> unified_api::domain::source::Source {
    unified_api::domain::source::Source {
        name: "Test Source".to_string(),
        project_id: "test".to_string(),
        script_path: script_path.to_string(),
        script_args: vec![],
        host_args: vec![],
        host_sync_updates_group_vars: false,
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
        allow_on_demand_refresh: false,
        advertise_scope: None,
        config: std::collections::HashMap::new(),
    }
}

// Reads one gauge value out of the Prometheus text exposition format, so tests
// assert on numbers instead of substring-matching rendered floats.
// None = the series is not present at all.
fn gauge_value(body: &str, metric: &str, source: &str) -> Option<f64> {
    let needle = format!("{}{{source=\"{}\"}}", metric, source);
    body.lines()
        .find(|line| line.starts_with(&needle))
        .and_then(|line| line.rsplit(' ').next())
        .and_then(|value| value.parse().ok())
}

// =========================================================================
// Tests: error bodies
// =========================================================================

// Every failure carries {"error": "..."}, and the message distinguishes cases
// that share a status code
#[tokio::test]
async fn error_responses_carry_a_json_body() {
    let app = app_with_demo_data();

    let (status, body) = get(app.clone(), "/api/v1/sources/src-nope/dataset").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let json: serde_json::Value = serde_json::from_str(&body).expect("404 body is not JSON");
    let message = json["error"].as_str().expect("no error field");
    assert!(
        message.contains("src-nope") && message.contains("cache"),
        "unhelpful message: {}",
        message
    );

    // Same status code, different meaning: not configured vs not cached. These
    // were both an empty-bodied 404 before.
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/sources/src-nope/sync")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"].as_str().unwrap().contains("not configured"),
        "sync 404 should say the source is not configured: {}",
        json
    );

    // Deleting a host from a source that IS cached names the host, not the source
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/src-demo/hosts/ghost.example.com")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let message = json["error"].as_str().unwrap();
    assert!(
        message.contains("ghost.example.com"),
        "should name the missing host: {}",
        message
    );
}

// A 403 says which source was refused, not just that something was
#[tokio::test]
async fn forbidden_body_names_the_source() {
    let app = app_with_scoped_keys();

    // A key scoped to src-alpha asking for src-beta
    let (status, body) =
        get_with_key(app, "/api/v1/sources/src-beta/dataset", "alpha-secret").await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let json: serde_json::Value = serde_json::from_str(&body).expect("403 body is not JSON");
    assert!(
        json["error"].as_str().unwrap().contains("src-beta"),
        "should name the refused source: {}",
        json
    );
}

// =========================================================================
// Tests: discovery routes (groups / hosts)
// =========================================================================

#[tokio::test]
async fn list_source_groups_returns_names_and_counts() {
    let (status, body) = get(app_with_demo_data(), "/api/v1/sources/src-demo/groups").await;

    assert_eq!(status, StatusCode::OK);
    let groups: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(groups.len(), 2);
    // Sorted by name
    assert_eq!(groups[0]["name"], "section9");
    assert_eq!(groups[0]["host_count"], 1);
    assert_eq!(groups[0]["has_vars"], true);
    assert_eq!(groups[0]["children"].as_array().unwrap().len(), 0);
    assert_eq!(groups[1]["name"], "seele");
    // The facts themselves are not in this response
    assert!(groups[0].get("hosts").is_none());
}

#[tokio::test]
async fn list_source_hosts_returns_sorted_names_only() {
    let (status, body) = get(app_with_demo_data(), "/api/v1/sources/src-demo/hosts").await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["source_id"], "src-demo");
    assert_eq!(json["total_hosts"], 2);
    let hosts = json["hosts"].as_array().unwrap();
    assert_eq!(hosts[0], "melchior.seele.net");
    assert_eq!(hosts[1], "motoko.section9.net");
}

#[tokio::test]
async fn discovery_routes_404_when_source_not_cached() {
    let app = unified_api::AppBuilder::new().build();
    let (status, _) = get(app.clone(), "/api/v1/sources/src-nope/groups").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = get(app, "/api/v1/sources/src-nope/hosts").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Tests: cache eviction
// =========================================================================

#[tokio::test]
async fn evict_source_drops_the_cache_entry() {
    let app = app_with_demo_data();

    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/src-demo")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["source_id"], "src-demo");
    assert_eq!(json["hosts_dropped"], 2);

    // Gone from every read path, not just the list
    let (status, _) = get(app.clone(), "/api/v1/sources/src-demo/dataset").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, body) = get(app, "/api/v1/sources").await;
    let sources: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources.len(), 0);
}

#[tokio::test]
async fn evict_source_not_in_cache_returns_404() {
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/sources/src-nope")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app_with_demo_data().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
            host_args: vec![],
            host_sync_updates_group_vars: false,
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
            allow_on_demand_refresh: false,
            advertise_scope: None,
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

// With readyz_require_all_sources, one synced source out of two is not enough
#[tokio::test]
async fn readyz_require_all_sources_waits_for_every_source() {
    let mut sources = std::collections::HashMap::new();
    for id in ["src-a", "src-b"] {
        sources.insert(
            id.to_string(),
            unified_api::domain::source::Source {
                name: "Test".to_string(),
                project_id: "test".to_string(),
                script_path: "tests/adapters/out/connectors/inventory.py".to_string(),
                script_args: vec![],
                host_args: vec![],
                host_sync_updates_group_vars: false,
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
                allow_on_demand_refresh: false,
                advertise_scope: None,
                config: std::collections::HashMap::new(),
            },
        );
    }
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .readyz_require_all_sources(true)
        .build_with_state();

    let dataset: unified_api::domain::dataset::Dataset =
        serde_json::from_str(r#"{"hostvars": {"a": {}}}"#).unwrap();
    state.cache.set(
        "src-a",
        unified_api::domain::cache_entry::CacheEntry::new(dataset.clone(), 3600),
    );

    // One of two synced: the default would call this ready, this must not
    let (status, body) = get(app.clone(), "/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["ready"], false);
    assert_eq!(result["sources_synced"], 1);
    assert_eq!(result["sources_pending"][0], "src-b");

    state.cache.set(
        "src-b",
        unified_api::domain::cache_entry::CacheEntry::new(dataset, 3600),
    );

    let (status, body) = get(app, "/readyz").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["ready"], true);
    assert_eq!(result["sources_synced"], 2);
}

// =========================================================================
// Tests: metrics_require_auth
// =========================================================================

#[tokio::test]
async fn metrics_is_public_by_default() {
    let app = unified_api::AppBuilder::new()
        .api_key(Some("secret123".to_string()))
        .build();

    // No key, still served — Prometheus scrape configs carry none
    let (status, _) = get(app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn metrics_require_auth_rejects_unauthenticated_scrapes() {
    let app = unified_api::AppBuilder::new()
        .api_key(Some("secret123".to_string()))
        .metrics_require_auth(true)
        .build();

    let (status, _) = get(app.clone(), "/metrics").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Only the status matters here: whether the exposition has any series yet
    // depends on which other tests in this binary have recorded metrics
    let (status, _) = get_with_key(app.clone(), "/metrics", "secret123").await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = get_with_key(app.clone(), "/metrics", "wrong").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The health probes stay public either way — they carry no inventory data
    let (status, _) = get(app, "/healthz").await;
    assert_eq!(status, StatusCode::OK);
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
        script_source("tests/adapters/out/connectors/inventory.py"),
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
    // Bucketed, not a summary: aggregatable across instances
    assert!(body.contains("unified_api_sync_duration_seconds_bucket"));
    assert!(body.contains("src-metrics"));
}

// Freshness gauges are computed at scrape time from the cache, so they answer
// "how stale is this source right now" without waiting for a sync to push them
#[tokio::test]
async fn metrics_exposes_freshness_gauges() {
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "src-gauges".to_string(),
        script_source("tests/adapters/out/connectors/inventory.py"),
    );
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .build_with_state();

    // Configured but never synced: cached=0 is reported (an absent series
    // could not be told apart from a removed source), and the dataset gauges
    // do not exist yet because there is no entry to describe.
    let (status, body) = get(app.clone(), "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        gauge_value(&body, "unified_api_source_cached", "src-gauges"),
        Some(0.0),
        "missing cached gauge in: {}",
        body
    );
    assert_eq!(
        gauge_value(&body, "unified_api_source_hosts", "src-gauges"),
        None
    );

    let dataset: unified_api::domain::dataset::Dataset = serde_json::from_str(
        r#"{"hostvars": {"a": {}, "b": {}}, "groups": {"g": {"hosts": ["a"]}}}"#,
    )
    .unwrap();
    state.cache.set(
        "src-gauges",
        unified_api::domain::cache_entry::CacheEntry::new(dataset, 3600),
    );

    let (_, body) = get(app.clone(), "/metrics").await;
    assert_eq!(
        gauge_value(&body, "unified_api_source_cached", "src-gauges"),
        Some(1.0)
    );
    assert_eq!(
        gauge_value(&body, "unified_api_source_hosts", "src-gauges"),
        Some(2.0)
    );
    assert_eq!(
        gauge_value(&body, "unified_api_source_groups", "src-gauges"),
        Some(1.0)
    );
    assert_eq!(
        gauge_value(&body, "unified_api_source_fresh", "src-gauges"),
        Some(1.0)
    );
    assert_eq!(
        gauge_value(&body, "unified_api_source_ttl_seconds", "src-gauges"),
        Some(3600.0)
    );
    assert!(gauge_value(&body, "unified_api_source_age_seconds", "src-gauges").is_some());

    // ttl 0 = expired the moment it was stored: this is the gauge an alert
    // fires on, and it flips without any sync having to run
    let dataset: unified_api::domain::dataset::Dataset =
        serde_json::from_str(r#"{"hostvars": {"a": {}}}"#).unwrap();
    state.cache.set(
        "src-gauges",
        unified_api::domain::cache_entry::CacheEntry::new(dataset, 0),
    );

    let (_, body) = get(app, "/metrics").await;
    assert_eq!(
        gauge_value(&body, "unified_api_source_fresh", "src-gauges"),
        Some(0.0)
    );
}

// Sync health was tracked (consecutive failures, last error) but only /status
// could see it: alerting had to infer a failing connector from the lossy
// `unified_api_source_fresh == 0` proxy, which only fires once the whole TTL
// has run out. These gauges export the registry itself, at scrape time.
#[tokio::test]
async fn metrics_exposes_sync_health_gauges() {
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "src-health-ok".to_string(),
        script_source("tests/adapters/out/connectors/inventory.py"),
    );
    sources.insert(
        "src-health-bad".to_string(),
        script_source("tests/adapters/out/connectors/does_not_exist.py"),
    );
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // Before any sync there is no record, so there is no series: absent means
    // "never attempted", exactly like the sync_health block on /sources
    let (_, body) = get(app.clone(), "/metrics").await;
    assert_eq!(
        gauge_value(
            &body,
            "unified_api_source_sync_consecutive_failures",
            "src-health-ok"
        ),
        None
    );

    for id in ["src-health-ok", "src-health-bad"] {
        let request = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/sources/{}/sync", id))
            .body(axum::body::Body::empty())
            .unwrap();
        // Always 200: a failed connector reports success=false in the body
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    let (_, body) = get(app.clone(), "/metrics").await;

    // The healthy source: zero failures, both ages present
    assert_eq!(
        gauge_value(
            &body,
            "unified_api_source_sync_consecutive_failures",
            "src-health-ok"
        ),
        Some(0.0)
    );
    assert!(
        gauge_value(
            &body,
            "unified_api_source_sync_last_attempt_age_seconds",
            "src-health-ok"
        )
        .is_some()
    );
    assert!(
        gauge_value(
            &body,
            "unified_api_source_sync_last_success_age_seconds",
            "src-health-ok"
        )
        .is_some()
    );

    // The broken source: a failure streak to alert on, and no success age at
    // all — it has never synced successfully, and an absent series says so
    assert_eq!(
        gauge_value(
            &body,
            "unified_api_source_sync_consecutive_failures",
            "src-health-bad"
        ),
        Some(1.0)
    );
    assert_eq!(
        gauge_value(
            &body,
            "unified_api_source_sync_last_success_age_seconds",
            "src-health-bad"
        ),
        None
    );
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
async fn cors_preflight_is_answered_for_an_allowed_origin() {
    // Pins the per-request layer wiring: a preflight OPTIONS must be
    // intercepted and answered by the CORS machinery, not routed (an OPTIONS
    // on /healthz has no handler and would 405).
    let app = unified_api::AppBuilder::new()
        .cors_allowed_origins(vec!["https://forms.example".to_string()])
        .build();

    let request = Request::builder()
        .method("OPTIONS")
        .uri("/healthz")
        .header("origin", "https://forms.example")
        .header("access-control-request-method", "GET")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("https://forms.example")
    );
    assert!(
        response
            .headers()
            .get("access-control-allow-methods")
            .is_some(),
        "a preflight answer names the allowed methods"
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

    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?host=ghost.example.com",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 0);
    assert_eq!(json["returned"], 0);

    // An unknown group is an empty selection too, not a 404 — with auto-groups
    // the group set depends on the data, so a name can be valid one sync and
    // absent the next
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?group=ghosts",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 0);
    assert_eq!(json["returned"], 0);
    assert_eq!(json["hostvars"].as_object().unwrap().len(), 0);
    // Only the (absent) filtered group is described, so groups comes back empty
    assert_eq!(json["groups"].as_object().unwrap().len(), 0);
}

// Same for /status: an unmatched group filter is an empty host list
#[tokio::test]
async fn status_unknown_group_returns_empty_result() {
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/status?group=ghosts",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["source_id"], "src-demo");
    assert_eq!(json["hosts"].as_array().unwrap().len(), 0);
}

// The source itself still 404s — that is a missing resource, not a filter
#[tokio::test]
async fn status_unknown_source_still_returns_404() {
    let (status, _) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-nope/status?group=ghosts",
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// A host listed twice in a group must appear once in /status, and count once
#[tokio::test]
async fn status_lists_duplicate_group_members_once() {
    let (status, body) = get(
        app_with_duplicate_group_members(),
        "/api/v1/sources/src-dupes/status?group=section9",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let hosts = json["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2, "duplicate member returned twice: {}", body);
    assert_eq!(json["total_hosts"], 2);
    // Sorted by hostname, which now comes from the deduplication step
    assert_eq!(hosts[0]["hostname"], "batou.section9.net");
    assert_eq!(hosts[1]["hostname"], "motoko.section9.net");
}

// Same for a repeated ?host= value, which is a valid query a caller can build
#[tokio::test]
async fn status_lists_repeated_host_parameter_once() {
    let (status, body) = get(
        app_with_duplicate_group_members(),
        "/api/v1/sources/src-dupes/status?host=motoko.section9.net,motoko.section9.net",
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["hosts"].as_array().unwrap().len(), 1);
    // The duplicate collapses into one returned entry; total_hosts describes
    // the source (2 hosts), not the filter
    assert_eq!(json["returned"], 1);
    assert_eq!(json["total_hosts"], 2);
}

// Filtered/paginated responses carry a validator too, so a consumer polling
// the same slice on a schedule gets 304 while nothing changes
#[tokio::test]
async fn filtered_dataset_supports_if_none_match() {
    let (app, state) = app_with_demo_data_and_state();
    let path = "/api/v1/sources/src-demo/dataset?group=section9&limit=10";

    let request = Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get("etag")
        .expect("filtered response carries no ETag")
        .to_str()
        .unwrap()
        .to_string();

    // Same query, validator sent back: nothing changed
    let request = Request::builder()
        .uri(path)
        .header("if-none-match", &etag)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);

    // A different slice must not match the stored validator
    let request = Request::builder()
        .uri("/api/v1/sources/src-demo/dataset?group=seele&limit=10")
        .header("if-none-match", &etag)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // And a write invalidates it: the cache generation moves
    state.cache.update("src-demo", &mut |entry| {
        entry.update_host("batou.section9.net".to_string(), Default::default());
    });

    let request = Request::builder()
        .uri(path)
        .header("if-none-match", &etag)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
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

#[tokio::test]
async fn dataset_fields_filter_limits_hostvars_keys() {
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?fields=role",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 2);
    assert_eq!(json["returned"], 2);

    let motoko = &json["hostvars"]["motoko.section9.net"];
    assert_eq!(motoko["role"], "commander");
    assert!(motoko.get("ansible_host").is_none());
    assert!(motoko.get("os").is_none());

    let melchior = &json["hostvars"]["melchior.seele.net"];
    assert_eq!(melchior["role"], "magi-system");
    assert!(melchior.get("ansible_host").is_none());
}

#[tokio::test]
async fn dataset_fields_with_group_filter() {
    let (status, body) = get(
        app_with_demo_data(),
        "/api/v1/sources/src-demo/dataset?group=section9&fields=role,os",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["total_hosts"], 1);

    let motoko = &json["hostvars"]["motoko.section9.net"];
    assert_eq!(motoko["role"], "commander");
    assert_eq!(motoko["os"], "OracleLinux");
    assert!(motoko.get("ansible_host").is_none());
    assert!(motoko.get("datacenter").is_none());
}

// =========================================================================
// Tests: ETag / If-None-Match and compression on the dataset endpoint
// =========================================================================

#[tokio::test]
async fn dataset_etag_flow() {
    let (app, _state) = app_with_demo_data_and_state();

    // First request: 200 with an ETag header
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/sources/src-demo/dataset")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let etag = response
        .headers()
        .get("etag")
        .expect("plain dataset response must carry an ETag")
        .to_str()
        .unwrap()
        .to_string();

    // Same ETag in If-None-Match: 304 with an empty body
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/sources/src-demo/dataset")
                .header("if-none-match", &etag)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(body.is_empty());

    // A stale ETag: full 200 again
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/sources/src-demo/dataset")
                .header("if-none-match", "\"deadbeef-0\"")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn dataset_etag_changes_when_data_changes() {
    let (app, state) = app_with_demo_data_and_state();

    let etag_of = |app: axum::Router| async move {
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/sources/src-demo/dataset")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response
            .headers()
            .get("etag")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    };

    let before = etag_of(app.clone()).await;

    state.cache.update("src-demo", &mut |entry| {
        entry.update_host(
            "togusa.section9.net".to_string(),
            std::collections::HashMap::new(),
        );
    });

    let after = etag_of(app).await;
    assert_ne!(before, after, "mutating the dataset must change the ETag");
}

#[tokio::test]
async fn dataset_compresses_when_client_accepts_gzip() {
    let (app, _state) = app_with_demo_data_and_state();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/sources/src-demo/dataset")
                .header("accept-encoding", "gzip")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("gzip")
    );
}

// =========================================================================
// Swagger UI on enterprise-sized responses
// =========================================================================

// highlight.js builds one DOM node per token, so a 2000-host dataset (~10MB
// of JSON) freezes the browser tab while the server has already answered.
// The switch lives in the initializer the UI loads, so that is what we assert.
#[tokio::test]
async fn swagger_ui_serves_syntax_highlighting_disabled() {
    let app = app_with_demo_data();

    let (status, body) = get(app, "/swagger-ui/swagger-initializer.js").await;

    assert_eq!(status, StatusCode::OK);
    // The config is pretty-printed into the file, so compare without spacing.
    let compact: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(
        compact.contains(r#""syntaxHighlight":{"activated":false}"#),
        "swagger-initializer.js must turn syntax highlighting off, got: {body}"
    );
}

// Swagger prefills a parameter input from its example, so this is what stops
// a plain Execute from pulling the whole inventory into the browser.
#[tokio::test]
async fn dataset_limit_parameter_carries_a_swagger_example() {
    let app = app_with_demo_data();

    let (status, body) = get(app, "/api-docs/openapi.json").await;
    assert_eq!(status, StatusCode::OK);

    let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
    let params = spec["paths"]["/api/v1/sources/{id}/dataset"]["get"]["parameters"]
        .as_array()
        .expect("dataset GET must declare parameters");
    let limit = params
        .iter()
        .find(|p| p["name"] == "limit")
        .expect("limit parameter must be documented");

    // The example may hang off the parameter or its schema depending on how
    // utoipa renders it; either position is what Swagger UI reads.
    let example = limit
        .get("example")
        .or_else(|| limit["schema"].get("example"))
        .expect("limit must carry an example so Swagger prefills a small page");
    assert_eq!(example, &serde_json::json!(50));

    // An example is a suggestion, not a server-side default: a caller that
    // omits limit still gets every host.
    assert!(
        limit.get("default").is_none() && limit["schema"].get("default").is_none(),
        "limit must not declare a default — the server applies none"
    );
}

// =========================================================================
// Tests: every error carries a body, endpoints included
// =========================================================================

// The endpoint route answered its authorization and not-found cases with a bare
// StatusCode, which axum renders with an EMPTY body — while every other route
// explains itself. That is the one convention the project states outright, and
// endpoints are the consumer-facing route: AWX and AnsibleForms call these.
#[tokio::test]
async fn endpoint_errors_carry_a_body_like_every_other_route() {
    use std::collections::HashMap;
    use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
    use unified_api::domain::endpoint::OutputEndpoint;

    let endpoints = HashMap::from([(
        "ep-granted".to_string(),
        OutputEndpoint {
            name: "Granted".to_string(),
            source_ids: vec![],
            output: None,
            script_path: Some("tests/adapters/out/output/ansible_inventory.py".to_string()),
            script_args: vec![],
            project_id: None,
            config: HashMap::new(),
            limit: None,
            timeout_seconds: Some(30),
        },
    )]);

    let app = unified_api::AppBuilder::new()
        .endpoints(endpoints)
        .api_keys(vec![ResolvedApiKey {
            name: "scoped".to_string(),
            secret: "scoped-secret".to_string(),
            permissions: Permissions::Scoped {
                sources: Default::default(),
                endpoints: Default::default(),
            },
        }])
        .build();

    // Not granted: 403 naming the endpoint
    let (status, body) =
        get_with_key(app.clone(), "/api/v1/endpoints/ep-granted", "scoped-secret").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|_| panic!("403 should carry an error body, got {:?}", body));
    assert!(
        parsed["error"].as_str().unwrap().contains("ep-granted"),
        "the error should name the endpoint: {}",
        body
    );
}

#[tokio::test]
async fn an_unknown_endpoint_id_says_so() {
    use std::collections::HashMap;
    use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};

    let app = unified_api::AppBuilder::new()
        .endpoints(HashMap::new())
        .api_keys(vec![ResolvedApiKey {
            name: "admin".to_string(),
            secret: "admin-secret".to_string(),
            permissions: Permissions::Admin,
        }])
        .build();

    let (status, body) = get_with_key(app, "/api/v1/endpoints/ep-ghost", "admin-secret").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let parsed: serde_json::Value = serde_json::from_str(&body)
        .unwrap_or_else(|_| panic!("404 should carry an error body, got {:?}", body));
    assert!(
        parsed["error"].as_str().unwrap().contains("ep-ghost"),
        "the error should name the id: {}",
        body
    );
}

// =========================================================================
// Test: script paths resolve into project checkouts at execution time
// =========================================================================
// The checkout is created AFTER the app is built — the situation every boot
// is in now that serving starts before the clones. Boot-time resolution
// would keep the unresolved path until a restart; per-execution resolution
// picks the script up on the very next sync.
#[tokio::test]
async fn a_script_appearing_in_a_checkout_after_boot_is_used_by_the_next_sync() {
    use std::os::unix::fs::PermissionsExt;

    let projects_dir = tempfile::tempdir().unwrap();

    let mut source = script_source("fetch.py");
    source.project_id = "prj-test".to_string();
    let mut sources = std::collections::HashMap::new();
    sources.insert("src-checkout".to_string(), source);

    let app = unified_api::AppBuilder::new()
        .sources(sources)
        .projects(
            std::collections::HashMap::new(),
            projects_dir.path().to_path_buf(),
        )
        .build();

    let sync = || {
        Request::builder()
            .method("POST")
            .uri("/api/v1/sources/src-checkout/sync")
            .body(axum::body::Body::empty())
            .unwrap()
    };

    // No checkout yet: the relative path resolves nowhere and the sync fails
    let response = app.clone().oneshot(sync()).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["success"], false, "no script yet: {}", result);

    // The checkout appears — what a finished clone produces
    std::fs::create_dir_all(projects_dir.path().join("prj-test")).unwrap();
    let script = projects_dir.path().join("prj-test/fetch.py");
    std::fs::write(
        &script,
        "#!/usr/bin/env python3\nprint('{\"hostvars\": {\"checkout.example\": {}}, \"groups\": {}}')\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    // ...and the NEXT sync runs it. No restart, no rebuild.
    let response = app.clone().oneshot(sync()).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["success"], true, "sync failed: {}", result);

    let (status, body) = get(app, "/api/v1/sources/src-checkout/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("checkout.example"), "body was: {}", body);
}

// =========================================================================
// Test: HTTP request metrics
// =========================================================================
#[tokio::test]
async fn metrics_exposes_http_request_series_by_matched_route() {
    let app = app_with_demo_data();

    // A request whose raw path contains a source id: the label must carry the
    // route PATTERN, never the raw URL — one series per route, not per host
    let (status, _) = get(app.clone(), "/api/v1/sources/src-demo/dataset").await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = get(app.clone(), "/metrics").await;
    assert!(
        body.contains(r#"path="/api/v1/sources/{id}/dataset""#),
        "counter should be labeled by the matched route pattern: {}",
        &body[..body.len().min(2000)]
    );
    assert!(
        !body.contains(r#"path="/api/v1/sources/src-demo/dataset""#),
        "a raw request path leaked into the metric labels"
    );
    assert!(body.contains("unified_api_http_requests_total"));

    // Real histogram buckets (le=...), not client-side summary quantiles:
    // the _bucket series is what makes latencies aggregatable across instances
    assert!(
        body.contains("unified_api_http_request_duration_seconds_bucket"),
        "durations should render as histogram buckets"
    );
}

// =========================================================================
// Test: request ids
// =========================================================================
#[tokio::test]
async fn responses_carry_a_request_id() {
    let app = app_with_demo_data();

    let id = |response: &axum::http::Response<axum::body::Body>| {
        response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };

    let first = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let second = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let (first, second) = (id(&first).unwrap(), id(&second).unwrap());
    assert!(!first.is_empty());
    assert_ne!(first, second, "each request gets its own id");

    // A client-provided id is KEPT and echoed, so a consumer can stitch our
    // log lines into its own trace
    let stitched = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .header("x-request-id", "consumer-trace-42")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(id(&stitched).as_deref(), Some("consumer-trace-42"));
}

// =========================================================================
// Tests: enricher routes
// =========================================================================
fn app_with_declarative_enricher() -> (axum::Router, std::sync::Arc<unified_api::AppState>) {
    let enricher: unified_api::domain::enricher::Enricher = serde_yaml_ng::from_str(
        "name: Copy infinibox\ntarget_id: src-demo\nsource_id: src-storage\nfields: [\"infinibox\"]\n",
    )
    .expect("enricher fixture");
    let mut enrichers = std::collections::HashMap::new();
    enrichers.insert("en-storage".to_string(), enricher);

    let (app, state) = unified_api::AppBuilder::new()
        .enrichers(enrichers)
        .build_with_state();
    (app, state)
}

#[tokio::test]
async fn enrichers_are_listed_with_target_readiness_and_health() {
    let (app, state) = app_with_declarative_enricher();

    // Before anything is cached: listed, target not ready, no health yet
    let (status, body) = get(app.clone(), "/api/v1/enrichers").await;
    assert_eq!(status, StatusCode::OK);
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list[0]["enricher_id"], "en-storage");
    assert_eq!(list[0]["target_ready"], false);
    assert!(list[0].get("sync_health").is_none());

    // Running it now is a 404 that says the TARGET is missing, not the enricher
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/enrichers/en-storage/run")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"].as_str().unwrap().contains("src-demo"),
        "the 404 must name the uncached target: {}",
        json
    );

    // Cache target and source, run again: success, and the listing now
    // carries health and readiness
    let dataset = |vars: serde_json::Value| -> unified_api::domain::dataset::Dataset {
        serde_json::from_value(
            serde_json::json!({"hostvars": {"motoko.section9.net": vars}, "groups": {}}),
        )
        .unwrap()
    };
    state.cache.set(
        "src-demo",
        unified_api::domain::cache_entry::CacheEntry::new(dataset(serde_json::json!({})), 3600),
    );
    state.cache.set(
        "src-storage",
        unified_api::domain::cache_entry::CacheEntry::new(
            dataset(serde_json::json!({"infinibox": "vol-a"})),
            3600,
        ),
    );

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/enrichers/en-storage/run")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let result: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(result["success"], true, "run failed: {}", result);
    assert_eq!(result["hosts_updated"], 1);

    let (_, body) = get(app.clone(), "/api/v1/enrichers").await;
    let list: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(list[0]["target_ready"], true);
    assert_eq!(list[0]["sync_health"]["consecutive_failures"], 0);
}

#[tokio::test]
async fn running_an_unknown_enricher_is_a_404_naming_it() {
    let (app, _) = app_with_declarative_enricher();
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/enrichers/en-ghost/run")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["error"].as_str().unwrap().contains("en-ghost"));
}

// =========================================================================
// Test: task-health gauges on /metrics
// =========================================================================
#[tokio::test]
async fn metrics_exposes_task_health_gauges() {
    let enricher: unified_api::domain::enricher::Enricher =
        serde_yaml_ng::from_str("name: E\ntarget_id: src-demo\nscript_path: e.py\n").unwrap();
    let mut enrichers = std::collections::HashMap::new();
    enrichers.insert("en-gauges".to_string(), enricher);
    let project: unified_api::domain::project::GitProject =
        serde_yaml_ng::from_str("name: P\ngit_url: \"https://example.com/r.git\"\n").unwrap();
    let mut projects = std::collections::HashMap::new();
    projects.insert("prj-gauges".to_string(), project);

    let (app, state) = unified_api::AppBuilder::new()
        .enrichers(enrichers)
        .projects(projects, std::path::PathBuf::from("unused"))
        .build_with_state();

    state
        .enrich_health
        .record_failure("en-gauges", "script exploded");
    state.project_health.record_success("prj-gauges");
    state.snapshot_health.record_failure(
        unified_api::adapters::out::cache::persistence::SNAPSHOT_HEALTH_KEY,
        "disk full",
    );

    let (status, body) = get(app, "/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains(r#"unified_api_enricher_consecutive_failures{enricher="en-gauges"} 1"#),
        "missing enricher gauge"
    );
    assert!(
        body.contains(r#"unified_api_project_sync_consecutive_failures{project="prj-gauges"} 0"#),
        "missing project gauge"
    );
    assert!(
        body.contains("unified_api_project_sync_last_success_age_seconds"),
        "missing project success age"
    );
    assert!(
        body.contains("unified_api_snapshot_consecutive_failures 1"),
        "missing snapshot gauge"
    );
}

// =========================================================================
// Test: the triggering request id reaches the connector script
// =========================================================================
#[tokio::test]
async fn a_syncs_trigger_reaches_the_script_inside_source_config() {
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "src-probe".to_string(),
        script_source("tests/adapters/out/connectors/env_probe.py"),
    );
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/sources/src-probe/sync")
        .header("x-request-id", "consumer-trace-77")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // env_probe.py stores its whole environment as the probe host's vars;
    // SOURCE_CONFIG in there must carry the trigger with the request's id
    let (status, body) = get(app, "/api/v1/sources/src-probe/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("consumer-trace-77"),
        "the trigger never reached SOURCE_CONFIG"
    );
    assert!(body.contains("trigger"));
}

// The same trace-joining for enricher runs: the request id behind a manual
// run — and the trigger of the sync being re-applied — reaches the script
// inside SOURCE_CONFIG.
#[tokio::test]
async fn an_enrichers_trigger_reaches_the_script_inside_source_config() {
    let mut sources = std::collections::HashMap::new();
    sources.insert(
        "src-probe".to_string(),
        script_source("tests/adapters/out/connectors/inventory.py"),
    );
    let enricher: unified_api::domain::enricher::Enricher = serde_yaml_ng::from_str(concat!(
        "name: Probe\n",
        "target_id: src-probe\n",
        "script_path: \"tests/adapters/out/enrichers/env_probe.py\"\n",
    ))
    .expect("enricher fixture");
    let mut enrichers = std::collections::HashMap::new();
    enrichers.insert("en-probe".to_string(), enricher);
    let app = unified_api::AppBuilder::new()
        .sources(sources)
        .enrichers(enrichers)
        .build();

    // A sync whose re-applied enrichment must inherit the sync's own trigger
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/sources/src-probe/sync")
        .header("x-request-id", "sync-trace-11")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (status, body) = get(app.clone(), "/api/v1/sources/src-probe/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("sync-trace-11"),
        "the sync's trigger never reached the re-applied enricher: {}",
        body
    );

    // A manual run passes its own request id
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/enrichers/en-probe/run")
        .header("x-request-id", "enrich-trace-22")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (status, body) = get(app, "/api/v1/sources/src-probe/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("enrich-trace-22"),
        "the run's request id never reached SOURCE_CONFIG: {}",
        body
    );
}

// =========================================================================
// Test: scope advertising
// =========================================================================
#[tokio::test]
async fn scope_advertises_config_derived_ownership() {
    let ssh_source: unified_api::domain::source::Source = serde_yaml_ng::from_str(concat!(
        "name: Edge\n",
        "project_id: p\n",
        "script_path: gather_facts\n",
        "connector_type: \"ssh\"\n",
        "ttl_seconds: 60\n",
        "hosts_from_source:\n",
        "  source: \"src-inv\"\n",
        "  match_pattern:\n",
        "    groups: [\"datacenter_dc2\"]\n",
        "    hosts: [\"appliance.dc2.example\"]\n",
    ))
    .unwrap();
    let mut sources = std::collections::HashMap::new();
    sources.insert("src-edge".to_string(), ssh_source);
    sources.insert(
        "src-plain".to_string(),
        script_source("tests/adapters/out/connectors/inventory.py"),
    );

    let view: unified_api::domain::view::View = serde_yaml_ng::from_str(concat!(
        "name: All\nmembers:\n",
        "  - source: src-edge\n    owns:\n      source: \"src-inv\"\n      groups: [\"datacenter_dc2\"]\n",
        "  - source: src-plain\n    owns:\n      source: \"src-inv\"\n      groups: [\"datacenter_dc1\"]\n",
    ))
    .unwrap();
    let mut views = std::collections::HashMap::new();
    views.insert("vw-all".to_string(), view);

    let app = unified_api::AppBuilder::new()
        .sources(sources)
        .views(views)
        .build();

    // An SSH source advertises the pattern it gathers by — config, not cache,
    // so it answers before anything has synced
    let (status, body) = get(app.clone(), "/api/v1/sources/src-edge/scope").await;
    assert_eq!(status, StatusCode::OK);
    let scope: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(scope["declared"], true);
    assert_eq!(scope["groups"][0], "datacenter_dc2");
    assert_eq!(scope["hosts"][0], "appliance.dc2.example");
    assert_eq!(scope["catch_all"], false);

    // An ordinary source makes no claim — and says so, rather than answering
    // with empty lists that could mean anything
    let (_, body) = get(app.clone(), "/api/v1/sources/src-plain/scope").await;
    let scope: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(scope["declared"], false);

    // A view advertises its members' ownership union
    let (_, body) = get(app.clone(), "/api/v1/sources/vw-all/scope").await;
    let scope: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(scope["kind"], "view");
    assert_eq!(
        scope["groups"],
        serde_json::json!(["datacenter_dc2", "datacenter_dc1"])
    );

    // Unknown id: the not-configured 404, since scope is about config
    let (status, body) = get(app, "/api/v1/sources/src-ghost/scope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("not configured"), "body was: {}", body);
}

// =========================================================================
// A declarative enricher's group vars have to survive all the way out of an
// endpoint, and the endpoint prunes a group with neither hosts nor children.
// Asserting on the cache alone would pass while the rendered inventory --
// the only thing a consumer ever sees -- carried nothing at all.
// =========================================================================

fn app_for_group_var_enrichment() -> (axum::Router, std::sync::Arc<unified_api::AppState>) {
    use std::collections::HashMap;
    use unified_api::domain::dataset::Dataset;
    use unified_api::domain::endpoint::{OutputEndpoint, OutputFormat};
    use unified_api::domain::enricher::Enricher;

    let enrichers = HashMap::from([(
        "en-vars".to_string(),
        serde_yaml_ng::from_str::<Enricher>(
            "name: vars\ntarget_id: src-target\nsource_id: src-vars\nfields: [\"cmdb_role\"]\n",
        )
        .expect("enricher fixture"),
    )]);

    let endpoints = HashMap::from([(
        "ep-target".to_string(),
        OutputEndpoint {
            name: "Target".to_string(),
            source_ids: vec!["src-target".to_string()],
            output: Some(OutputFormat::Ansible),
            script_path: None,
            script_args: vec![],
            project_id: None,
            config: HashMap::new(),
            limit: None,
            timeout_seconds: Some(30),
        },
    )]);

    let (app, state) = unified_api::AppBuilder::new()
        .enrichers(enrichers)
        .endpoints(endpoints)
        .build_with_state();

    // The target: hosts and their own groups, no `all`, like a Device42 source.
    let target: Dataset = serde_json::from_str(
        r#"{"hostvars": {"motoko.section9.net": {"os": "OracleLinux"}},
            "groups": {"section9": {"hosts": ["motoko.section9.net"]}}}"#,
    )
    .expect("target fixture");

    // The source: says what `all` means, and what a group the target has never
    // heard of means. It holds no hosts of its own for either.
    let vars: Dataset = serde_json::from_str(
        r#"{"hostvars": {},
            "groups": {
              "all": {"vars": {"cmdb_role": "device42", "secret": "no"}},
              "oraclelinux_9": {"vars": {"cmdb_role": "device42-ol9"}}
            }}"#,
    )
    .expect("vars fixture");

    state.cache.set(
        "src-target",
        unified_api::domain::cache_entry::CacheEntry::new(target, 3600),
    );
    state.cache.set(
        "src-vars",
        unified_api::domain::cache_entry::CacheEntry::new(vars, 3600),
    );

    (app, state)
}

async fn post(app: axum::Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn enriched_group_vars_reach_the_rendered_inventory() {
    let (app, _state) = app_for_group_var_enrichment();

    let (status, _) = post(app.clone(), "/api/v1/enrichers/en-vars/run").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(app, "/api/v1/endpoints/ep-target").await;
    assert_eq!(status, StatusCode::OK);
    let inventory: serde_json::Value = serde_json::from_str(&body).expect("ansible json");

    // The whole point: `all` is rendered, carrying the value, with the hosts
    // that keep it from being pruned on the way out.
    assert_eq!(
        inventory["all"]["vars"]["cmdb_role"], "device42",
        "the endpoint must carry the group var; body was: {}",
        body
    );
    assert_eq!(
        inventory["all"]["hosts"],
        serde_json::json!(["motoko.section9.net"])
    );
    // The allow-list still holds on the way through.
    assert!(inventory["all"]["vars"].get("secret").is_none());
    // And nothing was copied onto the host.
    assert!(
        inventory["_meta"]["hostvars"]["motoko.section9.net"]
            .get("cmdb_role")
            .is_none(),
        "a group var must not be duplicated onto every member"
    );
}

// A group can arrive with vars and no hosts of its own: an enricher publishing
// what a group MEANS, for members a consumer works out later -- `group_by` puts
// a host into an existing group of the same name at play time and picks up the
// vars it finds there. The endpoint used to drop such a group for being empty,
// between the enricher writing it and the render, with nothing said.
#[tokio::test]
async fn a_group_with_vars_and_no_hosts_still_reaches_the_rendered_inventory() {
    let (app, _state) = app_for_group_var_enrichment();

    let (status, _) = post(app.clone(), "/api/v1/enrichers/en-vars/run").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = get(app, "/api/v1/endpoints/ep-target").await;
    assert_eq!(status, StatusCode::OK);
    let inventory: serde_json::Value = serde_json::from_str(&body).expect("ansible json");

    assert_eq!(
        inventory["oraclelinux_9"]["vars"]["cmdb_role"], "device42-ol9",
        "a group the target never had must still be rendered; body was: {}",
        body
    );
    assert!(
        inventory["oraclelinux_9"].get("hosts").is_none(),
        "it carries no hosts of its own -- membership is decided elsewhere"
    );
}

// The dataset is served from a cached serialization with an ETag. Enrichment
// that forgets to invalidate it answers the next request with the old body
// under the old validator -- stale, and cacheable.
#[tokio::test]
async fn enriching_group_vars_invalidates_the_cached_serialization() {
    let (app, _state) = app_for_group_var_enrichment();

    let before = Request::builder()
        .uri("/api/v1/sources/src-target/dataset")
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app.clone().oneshot(before).await.unwrap();
    let etag_before = response
        .headers()
        .get("etag")
        .map(|v| v.to_str().unwrap().to_string());
    let body_before = response.into_body().collect().await.unwrap().to_bytes();
    assert!(!String::from_utf8_lossy(&body_before).contains("cmdb_role"));

    let (status, _) = post(app.clone(), "/api/v1/enrichers/en-vars/run").await;
    assert_eq!(status, StatusCode::OK);

    let (status, after) = get(app, "/api/v1/sources/src-target/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        after.contains("cmdb_role"),
        "the dataset must reflect the enrichment, not a cached copy of it: {}",
        after
    );
    assert!(
        etag_before.is_some(),
        "the route is expected to send an ETag"
    );
}
