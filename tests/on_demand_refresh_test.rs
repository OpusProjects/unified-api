// Integration tests for read-through refresh: GET /dataset?host=X&refresh=true.
//
// The connector under these tests is the REAL process connector running
// counting.py, which appends a line per invocation. That is what makes the
// interesting assertions possible: not "is the data fresh" (which a refresh
// makes true either way) but "how many gathers did this cost", which is the
// whole question the feature turns on.
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::collections::HashMap;
use tower::ServiceExt;
use unified_api::domain::cache_entry::CacheEntry;
use unified_api::domain::dataset::Dataset;
use unified_api::domain::source::{ConnectorType, Source, TtlOverrides};

struct Response {
    status: StatusCode,
    body: String,
    refreshed: Option<String>,
    refreshed_hosts: Option<String>,
    refresh_error: Option<String>,
}

async fn get(app: axum::Router, path: &str) -> Response {
    let request = Request::builder()
        .method("GET")
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let refreshed = header("x-unified-api-refreshed");
    let refreshed_hosts = header("x-unified-api-refreshed-hosts");
    let refresh_error = header("x-unified-api-refresh-error");

    let body = response.into_body().collect().await.unwrap().to_bytes();
    Response {
        status,
        body: String::from_utf8(body.to_vec()).unwrap(),
        refreshed,
        refreshed_hosts,
        refresh_error,
    }
}

// A source running counting.py. `extra` config lets a test make it slow or make
// it fail; `allow` is the capability the read-through path requires.
fn counting_source(
    counter_file: &str,
    ttl_seconds: u64,
    allow: bool,
    extra: &[(&str, &str)],
) -> Source {
    let mut config = HashMap::new();
    config.insert("counter_file".to_string(), counter_file.to_string());
    for (key, value) in extra {
        config.insert(key.to_string(), value.to_string());
    }

    Source {
        name: "Counting source".to_string(),
        project_id: "test".to_string(),
        script_path: "tests/adapters/out/connectors/counting.py".to_string(),
        script_args: vec![],
        output_format: Default::default(),
        hosts_from_source: None,
        connector_type: ConnectorType::Script,
        sync_mode: Default::default(),
        credential_ids: vec![],
        schedule: None,
        sync_interval_seconds: None,
        ttl_seconds,
        timeout_seconds: 60,
        ttl_overrides: TtlOverrides::default(),
        allow_on_demand_refresh: allow,
        config,
    }
}

fn sources(source: Source) -> HashMap<String, Source> {
    let mut map = HashMap::new();
    map.insert("src-count".to_string(), source);
    map
}

// A counter file per test, under the target dir so a failed run leaves it behind
// to look at.
fn counter_path(name: &str) -> String {
    let path = std::env::temp_dir().join(format!("unified-api-refresh-{}.log", name));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().to_string()
}

fn gathers(counter_file: &str) -> Vec<String> {
    std::fs::read_to_string(counter_file)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

// Data that is already 300s old, so a TTL of 60 makes it stale without any
// waiting in the test.
fn stale_entry(age_seconds: u64, ttl_seconds: u64) -> CacheEntry {
    let dataset: Dataset = serde_json::from_value(serde_json::json!({
        "hostvars": {
            "h1.example": {"os": "linux", "gathered": false},
            "h2.example": {"os": "linux", "gathered": false}
        },
        "groups": {}
    }))
    .unwrap();

    let mut ages = HashMap::new();
    ages.insert("h1.example".to_string(), age_seconds);
    ages.insert("h2.example".to_string(), age_seconds);
    CacheEntry::restore(dataset, ttl_seconds, age_seconds, ages)
}

// =========================================================================
// The two refusals: both are about the request, so both come before any work
// =========================================================================

#[tokio::test]
async fn refresh_without_naming_hosts_is_rejected() {
    let counter = counter_path("no-hosts");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 60, true, &[])))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(app, "/api/v1/sources/src-count/dataset?refresh=true").await;

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("requires ?host="),
        "{}",
        response.body
    );
    // and above all: it did not gather the whole source to answer a read
    assert!(gathers(&counter).is_empty());
}

#[tokio::test]
async fn refresh_on_a_source_that_does_not_allow_it_is_forbidden() {
    let counter = counter_path("not-allowed");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 60, false, &[])))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(
        app,
        "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(
        response.body.contains("allow_on_demand_refresh"),
        "the error should name the setting that fixes it: {}",
        response.body
    );
    assert!(gathers(&counter).is_empty());
}

// =========================================================================
// The happy path, and the TTL gate that bounds it
// =========================================================================

#[tokio::test]
async fn a_stale_host_is_re_gathered_before_the_read_answers() {
    let counter = counter_path("stale");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 60, true, &[])))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(
        app,
        "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.refreshed.as_deref(), Some("true"));
    assert_eq!(response.refreshed_hosts.as_deref(), Some("h1.example"));
    assert!(response.refresh_error.is_none());

    // exactly one gather, and scoped to the host that was asked for
    assert_eq!(gathers(&counter), vec!["host:h1.example"]);

    // the response carries the newly gathered data, not the seeded fixture
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["hostvars"]["h1.example"]["gathered"], true);
}

// The load bound, stated as a test: a consumer asking for a fresh host cannot
// cause a gather however often it asks. This is what makes refresh=true safe to
// hand to a form that a hundred people might open.
#[tokio::test]
async fn a_fresh_host_costs_nothing_however_many_times_it_is_asked_for() {
    let counter = counter_path("fresh");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 3600, true, &[])))
        .build_with_state();
    // 5 seconds old against a TTL of an hour: comfortably fresh
    state.cache.set("src-count", stale_entry(5, 3600));

    for _ in 0..10 {
        let response = get(
            app.clone(),
            "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
        )
        .await;
        assert_eq!(response.status, StatusCode::OK);
        // "true" = nothing went wrong, which for a fresh host means nothing was
        // needed; the absence of the hosts header is what says none were gathered
        assert_eq!(response.refreshed.as_deref(), Some("true"));
        assert!(response.refreshed_hosts.is_none());
    }

    assert!(
        gathers(&counter).is_empty(),
        "a fresh host must not be gathered: {:?}",
        gathers(&counter)
    );
}

#[tokio::test]
async fn several_named_hosts_are_refreshed_in_one_gather() {
    let counter = counter_path("batch");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 60, true, &[])))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(
        app,
        "/api/v1/sources/src-count/dataset?host=h1.example,h2.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.refreshed.as_deref(), Some("true"));
    // one invocation carrying both hosts, not one per host
    assert_eq!(gathers(&counter), vec!["host:h1.example,h2.example"]);
}

// Concurrent readers of the same stale host: the first gathers, the rest wait on
// it and then find the data already fresh. Without the per-host lock and the
// re-check behind it, this is five gathers.
#[tokio::test]
async fn concurrent_reads_of_the_same_stale_host_cause_one_gather() {
    let counter = counter_path("coalesce");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(
            &counter,
            60,
            true,
            // slow enough that the requests genuinely overlap
            &[("delay_seconds", "0.4")],
        )))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let requests = (0..5).map(|_| {
        get(
            app.clone(),
            "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
        )
    });
    let responses = futures::future::join_all(requests).await;

    for response in &responses {
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(response.refreshed.as_deref(), Some("true"));
    }
    assert_eq!(
        gathers(&counter).len(),
        1,
        "five concurrent readers gathered {:?}",
        gathers(&counter)
    );
}

// =========================================================================
// Degrading: a refresh that cannot happen must not take the read down with it
// =========================================================================

#[tokio::test]
async fn a_failed_refresh_still_serves_the_cached_data() {
    let counter = counter_path("failing");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(
            &counter,
            60,
            true,
            &[("fail", "true")],
        )))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(
        app,
        "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
    )
    .await;

    // the read succeeds…
    assert_eq!(response.status, StatusCode::OK);
    // …and says plainly that what it served is not current
    assert_eq!(response.refreshed.as_deref(), Some("false"));
    assert!(
        response.refresh_error.is_some(),
        "a failed refresh must say why"
    );
    assert!(response.refreshed_hosts.is_none());

    // the cached (stale) data is what came back
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["hostvars"]["h1.example"]["gathered"], false);
}

#[tokio::test]
async fn a_refresh_that_outlasts_its_budget_serves_the_cached_data() {
    let counter = counter_path("timeout");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(
            &counter,
            60,
            true,
            &[("delay_seconds", "3")],
        )))
        // one second is all a read may wait
        .on_demand_refresh(1, 8)
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(
        app,
        "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.refreshed.as_deref(), Some("false"));
    assert!(
        response
            .refresh_error
            .as_deref()
            .unwrap()
            .contains("did not finish within 1s"),
        "error was {:?}",
        response.refresh_error
    );
    let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(body["hostvars"]["h1.example"]["gathered"], false);
}

// =========================================================================
// Shape and caching: adding refresh= must not change what a consumer parses
// =========================================================================

#[tokio::test]
async fn refresh_does_not_change_the_response_shape() {
    let counter = counter_path("shape");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 60, true, &[])))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    // With ?host= the envelope, exactly as without refresh
    let filtered = get(
        app.clone(),
        "/api/v1/sources/src-count/dataset?host=h1.example&refresh=true",
    )
    .await;
    let body: serde_json::Value = serde_json::from_str(&filtered.body).unwrap();
    assert_eq!(body["source_id"], "src-count");
    assert_eq!(body["returned"], 1);
    assert!(body["hostvars"]["h1.example"].is_object());
}

// A read that does not ask for a refresh carries none of the headers: consumers
// that never opted in see the responses they always saw.
#[tokio::test]
async fn a_plain_read_is_untouched() {
    let counter = counter_path("plain");
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources(counting_source(&counter, 60, true, &[])))
        .build_with_state();
    state.cache.set("src-count", stale_entry(300, 60));

    let response = get(app, "/api/v1/sources/src-count/dataset?host=h1.example").await;

    assert_eq!(response.status, StatusCode::OK);
    assert!(response.refreshed.is_none());
    assert!(response.refreshed_hosts.is_none());
    assert!(response.refresh_error.is_none());
    assert!(gathers(&counter).is_empty());
}
