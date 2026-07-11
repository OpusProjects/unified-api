use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::collections::HashMap;
use tower::ServiceExt;
use unified_api::domain::endpoint::OutputEndpoint;
use unified_api::domain::enricher::Enricher;
use unified_api::domain::source::{ConnectorType, Source, TtlOverrides};
use unified_api::domain::sync_mode::SyncMode;

async fn request(app: axum::Router, method: &str, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();
    (status, body_str)
}

async fn request_with_json(
    app: axum::Router,
    method: &str,
    path: &str,
    json_body: serde_json::Value,
) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(
            serde_json::to_string(&json_body).unwrap(),
        ))
        .unwrap();

    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();
    (status, body_str)
}

// Creates a source pointing to our fake_inventory.py
fn test_source(scenario: &str) -> Source {
    let mut config = HashMap::new();
    config.insert("scenario".to_string(), scenario.to_string());

    Source {
        name: "Test Source".to_string(),
        project_id: "test".to_string(),
        script_path: "tests/adapters/out/connectors/inventory.py".to_string(),
        script_args: vec![],
        output_format: Default::default(),
        connector_type: ConnectorType::Script,
        sync_mode: SyncMode::Replace,
        credential_ids: vec![],
        schedule: None,
        sync_interval_seconds: None,
        ttl_seconds: 3600,
        timeout_seconds: 300,
        ttl_overrides: TtlOverrides::default(),
        config,
    }
}

// =========================================================================
// Test: full sync — executes script, puts in cache, queries result
// =========================================================================
#[tokio::test]
async fn sync_then_query_full_flow() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // 1. Before sync, the cache is empty
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK);
    let sources_list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources_list.len(), 0);

    // 2. We do sync — executes fake_inventory.py with scenario=default
    let (status, body) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);
    let sync_result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(sync_result["success"], true);
    assert_eq!(sync_result["total_hosts"], 6); // motoko, batou, tachikoma, melchior, balthasar, casper
    assert_eq!(sync_result["total_groups"], 7);
    assert!(sync_result["error"].is_null());

    // 3. Now the cache has data — we query it
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK);
    let sources_list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources_list.len(), 1);
    assert_eq!(sources_list[0]["source_id"], "src-test");
    assert_eq!(sources_list[0]["total_hosts"], 6);

    // 4. We query the full dataset
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(dataset["hostvars"]["motoko.section9.net"].is_object());
    assert!(dataset["groups"]["magi"].is_object());
}

// =========================================================================
// Test: sync with empty inventory
// =========================================================================
#[tokio::test]
async fn sync_empty_inventory() {
    let mut sources = HashMap::new();
    sources.insert("src-empty".to_string(), test_source("empty"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = request(app, "POST", "/api/v1/sources/src-empty/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["total_hosts"], 0);
}

// =========================================================================
// Test: sync with connector error
// =========================================================================
#[tokio::test]
async fn sync_connector_error() {
    let mut sources = HashMap::new();
    sources.insert("src-broken".to_string(), test_source("error"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = request(app, "POST", "/api/v1/sources/src-broken/sync").await;
    assert_eq!(status, StatusCode::OK); // the endpoint worked, the connector failed
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], false);
    assert!(result["error"].as_str().unwrap().contains("failed"));
}

// =========================================================================
// Test: sync of source that does not exist in config
// =========================================================================
#[tokio::test]
async fn sync_unknown_source_returns_404() {
    let app = unified_api::AppBuilder::new().build();

    let (status, _) = request(app, "POST", "/api/v1/sources/inventado/sync").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Test: sync large inventory (50 hosts)
// =========================================================================
#[tokio::test]
async fn sync_large_inventory() {
    let mut sources = HashMap::new();
    sources.insert("src-large".to_string(), test_source("large"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = request(app, "POST", "/api/v1/sources/src-large/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["total_hosts"], 50);
}

// =========================================================================
// Test: sync of a single host — only refreshes that host in cache
// =========================================================================
#[tokio::test]
async fn sync_single_host() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // 1. Full sync first (6 hosts)
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // 2. Sync only motoko
    let (status, body) = request(
        app.clone(),
        "POST",
        "/api/v1/sources/src-test/sync?host=motoko.section9.net",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["scope"], "host:motoko.section9.net");
    assert_eq!(result["total_hosts"], 1); // only motoko

    // 3. The cache still has 6 hosts (the others were not deleted)
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(dataset["hostvars"].as_object().unwrap().len(), 6);
}

// =========================================================================
// Test: sync of a group — only refreshes hosts in the group
// =========================================================================
#[tokio::test]
async fn sync_group() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // 1. Full sync
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // 2. Sync of magi group (3 hosts: melchior, balthasar, casper)
    let (status, body) = request(
        app.clone(),
        "POST",
        "/api/v1/sources/src-test/sync?group=magi",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["scope"], "group:magi");
    assert_eq!(result["total_hosts"], 3);

    // 3. The cache still has 6 hosts
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(dataset["hostvars"].as_object().unwrap().len(), 6);
}

// =========================================================================
// Test: sync of host that does not exist — connector reports error
// =========================================================================
#[tokio::test]
async fn sync_nonexistent_host() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = request(
        app,
        "POST",
        "/api/v1/sources/src-test/sync?host=togusa.section9.net",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], false); // the connector says "host not found"
}

// =========================================================================
// Test: full sync reports scope "full"
// =========================================================================
#[tokio::test]
async fn sync_full_reports_scope() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = request(app, "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["scope"], "full");
}

// =========================================================================
// Test: status shows age_seconds and is_fresh per host
// =========================================================================
#[tokio::test]
async fn status_shows_per_host_info() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // Without sync, status returns 404 (no cache)
    let (status, _) = request(app.clone(), "GET", "/api/v1/sources/src-test/status").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Full sync
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // Now status returns info
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/status").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["source_id"], "src-test");
    assert_eq!(result["total_hosts"], 6);
    assert_eq!(result["dataset_is_fresh"], true);

    // Each host has its info
    let hosts = result["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 6);
    let motoko = hosts
        .iter()
        .find(|h| h["hostname"] == "motoko.section9.net")
        .unwrap();
    assert_eq!(motoko["is_fresh"], true);
    assert!(motoko["age_seconds"].as_u64().unwrap() < 5);
}

// =========================================================================
// Test: status filtered by host
// =========================================================================
#[tokio::test]
async fn status_filter_by_host() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, body) = request(
        app.clone(),
        "GET",
        "/api/v1/sources/src-test/status?host=motoko.section9.net",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["total_hosts"], 1);
    assert_eq!(result["hosts"][0]["hostname"], "motoko.section9.net");
}

// =========================================================================
// Test: status filtered by group
// =========================================================================
#[tokio::test]
async fn status_filter_by_group() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, body) = request(
        app.clone(),
        "GET",
        "/api/v1/sources/src-test/status?group=magi",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["total_hosts"], 3);
}

// =========================================================================
// Test: status of host that does not exist → 404
// =========================================================================
#[tokio::test]
async fn status_unknown_host_returns_404() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, _) = request(
        app,
        "GET",
        "/api/v1/sources/src-test/status?host=togusa.section9.net",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Test: PUT host — immediate addition
// =========================================================================
#[tokio::test]
async fn put_host_adds_to_cache() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // Sync to have data
    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    // Immediate registration of a new host
    let (status, _) = request_with_json(
        app.clone(),
        "PUT",
        "/api/v1/sources/src-test/hosts/togusa.section9.net",
        serde_json::json!({"role": "detective", "ansible_host": "10.9.1.99"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify the host appears in the dataset
    let (_, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        dataset["hostvars"]["togusa.section9.net"]["role"],
        "detective"
    );
    assert_eq!(dataset["hostvars"].as_object().unwrap().len(), 7); // 6 + 1
}

// =========================================================================
// Test: DELETE host — immediate removal
// =========================================================================
#[tokio::test]
async fn delete_host_removes_from_cache() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    // Immediate removal
    let (status, _) = request(
        app.clone(),
        "DELETE",
        "/api/v1/sources/src-test/hosts/motoko.section9.net",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Verify it is no longer there
    let (_, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        !dataset["hostvars"]
            .as_object()
            .unwrap()
            .contains_key("motoko.section9.net")
    );
    assert_eq!(dataset["hostvars"].as_object().unwrap().len(), 5); // 6 - 1
}

// =========================================================================
// Test: DELETE host that does not exist → 404
// =========================================================================
#[tokio::test]
async fn delete_nonexistent_host_returns_404() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, _) = request(app, "DELETE", "/api/v1/sources/src-test/hosts/fantasma.net").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Test: enricher — enriches hosts in cache
// =========================================================================
#[tokio::test]
async fn enricher_updates_hosts_in_cache() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));

    let mut enrichers = HashMap::new();
    enrichers.insert(
        "enrich-test".to_string(),
        Enricher {
            name: "Test Enricher".to_string(),
            source_id: "src-test".to_string(),
            script_path: "tests/adapters/out/enrichers/enricher.py".to_string(),
            script_args: vec![],
            project_id: None,
            sync_interval_seconds: None,
            config: HashMap::new(),
            timeout_seconds: 300,
        },
    );

    let (app, _state) = unified_api::AppBuilder::new()
        .sources(sources)
        .enrichers(enrichers)
        .build_with_state();

    // Sync first to have data
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // Run enricher
    let (status, body) = request(app.clone(), "POST", "/api/v1/enrichers/enrich-test/run").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["hosts_updated"], 6);
    assert_eq!(result["hosts_removed"], 0);

    // Verify the hosts were enriched
    let (_, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(dataset["hostvars"]["motoko.section9.net"]["enriched"], true);
}

// =========================================================================
// Test: enricher with remove_hosts
// =========================================================================
#[tokio::test]
async fn enricher_removes_hosts() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));

    let mut enricher_config = HashMap::new();
    enricher_config.insert("remove_hosts".to_string(), "batou.section9.net".to_string());

    let mut enrichers = HashMap::new();
    enrichers.insert(
        "enrich-cleanup".to_string(),
        Enricher {
            name: "Cleanup Enricher".to_string(),
            source_id: "src-test".to_string(),
            script_path: "tests/adapters/out/enrichers/enricher.py".to_string(),
            script_args: vec![],
            project_id: None,
            sync_interval_seconds: None,
            config: enricher_config,
            timeout_seconds: 300,
        },
    );

    let (app, _state) = unified_api::AppBuilder::new()
        .sources(sources)
        .enrichers(enrichers)
        .build_with_state();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, body) = request(app.clone(), "POST", "/api/v1/enrichers/enrich-cleanup/run").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["hosts_removed"], 1);

    // Verify batou was deleted
    let (_, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(
        !dataset["hostvars"]
            .as_object()
            .unwrap()
            .contains_key("batou.section9.net")
    );
}

// =========================================================================
// Test: infra source — filesystems, cpu, memory, sysctl, users
// =========================================================================
#[tokio::test]
async fn sync_infra_source() {
    let mut config = HashMap::new();
    config.insert("scenario".to_string(), "default".to_string());

    let mut sources = HashMap::new();
    sources.insert(
        "src-infra".to_string(),
        Source {
            name: "Infrastructure Data".to_string(),
            project_id: "test".to_string(),
            script_path: "tests/adapters/out/connectors/infra.py".to_string(),
            script_args: vec![],
            output_format: Default::default(),
            connector_type: ConnectorType::Script,
            sync_mode: SyncMode::Replace,
            credential_ids: vec![],
            schedule: None,
            sync_interval_seconds: None,
            ttl_seconds: 3600,
            timeout_seconds: 300,
            ttl_overrides: TtlOverrides::default(),
            config,
        },
    );
    let app = unified_api::AppBuilder::new().sources(sources).build();

    // Sync
    let (status, body) = request(app.clone(), "POST", "/api/v1/sources/src-infra/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["total_hosts"], 6);

    // Verify infra data
    let (_, body) = request(app.clone(), "GET", "/api/v1/sources/src-infra/dataset").await;
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();

    let motoko = &dataset["hostvars"]["motoko.section9.net"];
    assert_eq!(motoko["cpu"]["cores"], 8);
    assert_eq!(motoko["memory"]["total_mb"], 32768);
    assert_eq!(motoko["filesystems"].as_array().unwrap().len(), 4);
    assert_eq!(motoko["sysctl"]["vm.swappiness"], "10");
    assert_eq!(motoko["users"].as_array().unwrap().len(), 4);

    // Melchior — Oracle DB with many sysctl tunings
    let melchior = &dataset["hostvars"]["melchior.seele.net"];
    assert_eq!(melchior["cpu"]["cores"], 16);
    assert_eq!(melchior["memory"]["total_mb"], 65536);
    assert!(melchior["sysctl"]["kernel.shmmax"].as_str().is_some());
    assert_eq!(melchior["filesystems"].as_array().unwrap().len(), 3);

    // Groups
    assert!(dataset["groups"]["oracle_db"].is_object());
    assert!(dataset["groups"]["high_memory"].is_object());
}

// =========================================================================
// Test: enricher for source without cache → 404
// =========================================================================
#[tokio::test]
async fn enricher_without_cached_source_returns_404() {
    let mut enrichers = HashMap::new();
    enrichers.insert(
        "enrich-orphan".to_string(),
        Enricher {
            name: "Orphan Enricher".to_string(),
            source_id: "src-nonexistent".to_string(),
            script_path: "tests/adapters/out/enrichers/enricher.py".to_string(),
            script_args: vec![],
            project_id: None,
            sync_interval_seconds: None,
            config: HashMap::new(),
            timeout_seconds: 300,
        },
    );

    let (app, _state) = unified_api::AppBuilder::new()
        .enrichers(enrichers)
        .build_with_state();

    let (status, _) = request(app, "POST", "/api/v1/enrichers/enrich-orphan/run").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Test: endpoint combines two sources in Ansible format
// =========================================================================
#[tokio::test]
async fn endpoint_combines_sources() {
    let mut sources = HashMap::new();
    sources.insert("src-inventory".to_string(), test_source("default"));

    let mut infra_config = HashMap::new();
    infra_config.insert("scenario".to_string(), "default".to_string());
    sources.insert(
        "src-infra".to_string(),
        Source {
            name: "Infra Data".to_string(),
            project_id: "test".to_string(),
            script_path: "tests/adapters/out/connectors/infra.py".to_string(),
            script_args: vec![],
            output_format: Default::default(),
            connector_type: ConnectorType::Script,
            sync_mode: SyncMode::Replace,
            credential_ids: vec![],
            schedule: None,
            sync_interval_seconds: None,
            ttl_seconds: 3600,
            timeout_seconds: 300,
            ttl_overrides: TtlOverrides::default(),
            config: infra_config,
        },
    );

    let mut endpoints = HashMap::new();
    endpoints.insert(
        "ep-full".to_string(),
        OutputEndpoint {
            name: "Full Inventory".to_string(),
            source_ids: vec!["src-inventory".to_string(), "src-infra".to_string()],
            script_path: "tests/adapters/out/output/ansible_inventory.py".to_string(),
            script_args: vec![],
            project_id: None,
            config: HashMap::new(),
            timeout_seconds: 300,
        },
    );

    let (app, _) = unified_api::AppBuilder::new()
        .sources(sources)
        .endpoints(endpoints)
        .build_with_state();

    // Sync both sources
    let (s1, _) = request(app.clone(), "POST", "/api/v1/sources/src-inventory/sync").await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, _) = request(app.clone(), "POST", "/api/v1/sources/src-infra/sync").await;
    assert_eq!(s2, StatusCode::OK);

    // Get endpoint
    let (status, body) = request(app.clone(), "POST", "/api/v1/endpoints/ep-full").await;
    assert_eq!(status, StatusCode::OK);
    let inventory: serde_json::Value = serde_json::from_str(&body).unwrap();

    // Ansible format: _meta.hostvars
    assert!(inventory["_meta"]["hostvars"]["motoko.section9.net"].is_object());
    // Infra data merged in — cpu field from src-infra
    assert!(inventory["_meta"]["hostvars"]["motoko.section9.net"]["cpu"].is_object());
    // Groups present
    assert!(inventory["section9"].is_object());
}

// =========================================================================
// Test: endpoint with filter
// =========================================================================
#[tokio::test]
async fn endpoint_filters_by_datacenter() {
    let mut sources = HashMap::new();
    sources.insert("src-inventory".to_string(), test_source("default"));

    let mut ep_config = HashMap::new();
    ep_config.insert("filter_datacenter".to_string(), "section9".to_string());

    let mut endpoints = HashMap::new();
    endpoints.insert(
        "ep-section9".to_string(),
        OutputEndpoint {
            name: "Section 9 Only".to_string(),
            source_ids: vec!["src-inventory".to_string()],
            script_path: "tests/adapters/out/output/ansible_inventory.py".to_string(),
            script_args: vec![],
            project_id: None,
            config: ep_config,
            timeout_seconds: 300,
        },
    );

    let (app, _) = unified_api::AppBuilder::new()
        .sources(sources)
        .endpoints(endpoints)
        .build_with_state();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-inventory/sync").await;

    let (status, body) = request(app.clone(), "POST", "/api/v1/endpoints/ep-section9").await;
    assert_eq!(status, StatusCode::OK);
    let inventory: serde_json::Value = serde_json::from_str(&body).unwrap();

    let hostvars = inventory["_meta"]["hostvars"].as_object().unwrap();
    // Only section9 — 3 hosts
    assert_eq!(hostvars.len(), 3);
    assert!(hostvars.contains_key("motoko.section9.net"));
    assert!(!hostvars.contains_key("melchior.seele.net"));
}

// =========================================================================
// Test: endpoint without synced sources → 503
// =========================================================================
#[tokio::test]
async fn endpoint_without_synced_sources_returns_503() {
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "ep-missing".to_string(),
        OutputEndpoint {
            name: "Missing Sources".to_string(),
            source_ids: vec!["src-nonexistent".to_string()],
            script_path: "tests/adapters/out/output/ansible_inventory.py".to_string(),
            script_args: vec![],
            project_id: None,
            config: HashMap::new(),
            timeout_seconds: 300,
        },
    );

    let (app, _) = unified_api::AppBuilder::new()
        .endpoints(endpoints)
        .build_with_state();

    let (status, body) = request(app, "POST", "/api/v1/endpoints/ep-missing").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["missing_sources"][0], "src-nonexistent");
}

// =========================================================================
// Test: unknown endpoint → 404
// =========================================================================
#[tokio::test]
async fn endpoint_not_configured_returns_404() {
    let app = unified_api::AppBuilder::new().build();

    let (status, _) = request(app, "POST", "/api/v1/endpoints/inventado").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Test: list endpoints
// =========================================================================
#[tokio::test]
async fn list_endpoints_shows_readiness() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));

    let mut endpoints = HashMap::new();
    endpoints.insert(
        "ep-test".to_string(),
        OutputEndpoint {
            name: "Test Endpoint".to_string(),
            source_ids: vec!["src-test".to_string(), "src-missing".to_string()],
            script_path: "tests/adapters/out/output/ansible_inventory.py".to_string(),
            script_args: vec![],
            project_id: None,
            config: HashMap::new(),
            timeout_seconds: 300,
        },
    );

    let (app, _) = unified_api::AppBuilder::new()
        .sources(sources)
        .endpoints(endpoints)
        .build_with_state();

    // Before sync — both sources missing from cache
    let (status, body) = request(app.clone(), "GET", "/api/v1/endpoints").await;
    assert_eq!(status, StatusCode::OK);
    let eps: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(eps[0]["sources_ready"], 0);
    assert_eq!(eps[0]["sources_missing"].as_array().unwrap().len(), 2);

    // Sync src-test
    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    // After sync — one ready, one still missing
    let (_, body) = request(app.clone(), "GET", "/api/v1/endpoints").await;
    let eps: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(eps[0]["sources_ready"], 1);
    assert_eq!(eps[0]["sources_missing"].as_array().unwrap().len(), 1);
    assert_eq!(eps[0]["sources_missing"][0], "src-missing");
}

// =========================================================================
// Test: endpoint with dynamic params (POST body)
// =========================================================================
#[tokio::test]
async fn endpoint_with_dynamic_params() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));

    // Endpoint without static filters — the consumer passes them in the body
    let mut endpoints = HashMap::new();
    endpoints.insert(
        "ep-dynamic".to_string(),
        OutputEndpoint {
            name: "Dynamic Endpoint".to_string(),
            source_ids: vec!["src-test".to_string()],
            script_path: "tests/adapters/out/output/ansible_inventory.py".to_string(),
            script_args: vec![],
            project_id: None,
            config: HashMap::new(),
            timeout_seconds: 300,
        },
    );

    let (app, _) = unified_api::AppBuilder::new()
        .sources(sources)
        .endpoints(endpoints)
        .build_with_state();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    // Without params — returns all hosts
    let (status, body) = request(app.clone(), "POST", "/api/v1/endpoints/ep-dynamic").await;
    assert_eq!(status, StatusCode::OK);
    let full: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(full["_meta"]["hostvars"].as_object().unwrap().len(), 6);

    // With params — filters only section9
    let (status, body) = request_with_json(
        app.clone(),
        "POST",
        "/api/v1/endpoints/ep-dynamic",
        serde_json::json!({"filter_datacenter": "section9"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let filtered: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(filtered["_meta"]["hostvars"].as_object().unwrap().len(), 3);
    assert!(filtered["_meta"]["hostvars"]["motoko.section9.net"].is_object());
    assert!(
        !filtered["_meta"]["hostvars"]
            .as_object()
            .unwrap()
            .contains_key("melchior.seele.net")
    );
}

// =========================================================================
// Test: dynamic params override static config
// =========================================================================
#[tokio::test]
async fn endpoint_params_override_config() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));

    // Static config: filters section9
    let mut ep_config = HashMap::new();
    ep_config.insert("filter_datacenter".to_string(), "section9".to_string());

    let mut endpoints = HashMap::new();
    endpoints.insert(
        "ep-override".to_string(),
        OutputEndpoint {
            name: "Override Test".to_string(),
            source_ids: vec!["src-test".to_string()],
            script_path: "tests/adapters/out/output/ansible_inventory.py".to_string(),
            script_args: vec![],
            project_id: None,
            config: ep_config,
            timeout_seconds: 300,
        },
    );

    let (app, _) = unified_api::AppBuilder::new()
        .sources(sources)
        .endpoints(endpoints)
        .build_with_state();

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    // Without params — uses static config (section9 = 3 hosts)
    let (_, body) = request(app.clone(), "POST", "/api/v1/endpoints/ep-override").await;
    let section9: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(section9["_meta"]["hostvars"].as_object().unwrap().len(), 3);

    // With params — overrides to seele
    let (_, body) = request_with_json(
        app.clone(),
        "POST",
        "/api/v1/endpoints/ep-override",
        serde_json::json!({"filter_datacenter": "seele"}),
    )
    .await;
    let seele: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(seele["_meta"]["hostvars"].as_object().unwrap().len(), 3);
    assert!(seele["_meta"]["hostvars"]["melchior.seele.net"].is_object());
}

// =========================================================================
// Test: execution timeout — a hung connector must not hang the sync
// =========================================================================
#[tokio::test]
async fn sync_times_out_on_slow_connector() {
    let mut source = test_source("default");
    source.script_path = "tests/adapters/out/connectors/slow.py".to_string();
    source.timeout_seconds = 1; // the script sleeps 10s

    let mut sources = HashMap::new();
    sources.insert("src-slow".to_string(), source);
    let app = unified_api::AppBuilder::new().sources(sources).build();

    let (status, body) = request(app, "POST", "/api/v1/sources/src-slow/sync").await;

    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], false);
    let error = result["error"].as_str().unwrap();
    assert!(
        error.contains("timed out after 1s"),
        "unexpected error: {}",
        error
    );
    // Nothing gets cached on a timed-out sync
    assert_eq!(result["total_hosts"], 0);
}
