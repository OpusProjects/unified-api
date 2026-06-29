use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::collections::HashMap;
use tower::ServiceExt;
use unified_api::domain::source::{Source, TtlOverrides};

// Helper para hacer GET/POST
async fn request(
    app: axum::Router,
    method: &str,
    path: &str,
) -> (StatusCode, String) {
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

// Crea un source que apunta a nuestro fake_inventory.py
fn test_source(scenario: &str) -> Source {
    let mut config = HashMap::new();
    config.insert("scenario".to_string(), scenario.to_string());

    Source {
        name: "Test Source".to_string(),
        project_id: "test".to_string(),
        script_path: "test-connectors/fake_inventory.py".to_string(),
        credential_ids: vec![],
        schedule: None,
        sync_interval_seconds: None,
        ttl_seconds: 3600,
        ttl_overrides: TtlOverrides::default(),
        config,
    }
}

// =========================================================================
// Test: sync completo — ejecuta script, mete en cache, consulta resultado
// =========================================================================
#[tokio::test]
async fn sync_then_query_full_flow() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    // 1. Antes del sync, el cache está vacío
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK);
    let sources_list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources_list.len(), 0);

    // 2. Hacemos sync — ejecuta fake_inventory.py con scenario=default
    let (status, body) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);
    let sync_result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(sync_result["success"], true);
    assert_eq!(sync_result["total_hosts"], 6); // motoko, batou, tachikoma, melchior, balthasar, casper
    assert_eq!(sync_result["total_groups"], 7);
    assert!(sync_result["error"].is_null());

    // 3. Ahora el cache tiene datos — consultamos
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK);
    let sources_list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources_list.len(), 1);
    assert_eq!(sources_list[0]["source_id"], "src-test");
    assert_eq!(sources_list[0]["total_hosts"], 6);

    // 4. Consultamos el dataset completo
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(dataset["hostvars"]["motoko.section9.net"].is_object());
    assert!(dataset["groups"]["magi"].is_object());
}

// =========================================================================
// Test: sync con inventario vacío
// =========================================================================
#[tokio::test]
async fn sync_empty_inventory() {
    let mut sources = HashMap::new();
    sources.insert("src-empty".to_string(), test_source("empty"));
    let app = unified_api::build_app_with_sources(sources);

    let (status, body) = request(app, "POST", "/api/v1/sources/src-empty/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["total_hosts"], 0);
}

// =========================================================================
// Test: sync con error del connector
// =========================================================================
#[tokio::test]
async fn sync_connector_error() {
    let mut sources = HashMap::new();
    sources.insert("src-broken".to_string(), test_source("error"));
    let app = unified_api::build_app_with_sources(sources);

    let (status, body) = request(app, "POST", "/api/v1/sources/src-broken/sync").await;
    assert_eq!(status, StatusCode::OK); // el endpoint funcionó, el connector falló
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], false);
    assert!(result["error"].as_str().unwrap().contains("failed"));
}

// =========================================================================
// Test: sync de source que no existe en config
// =========================================================================
#[tokio::test]
async fn sync_unknown_source_returns_404() {
    let app = unified_api::build_app();

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
    let app = unified_api::build_app_with_sources(sources);

    let (status, body) = request(app, "POST", "/api/v1/sources/src-large/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["total_hosts"], 50);
}

// =========================================================================
// Test: sync de un solo host — solo refresca ese host en cache
// =========================================================================
#[tokio::test]
async fn sync_single_host() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    // 1. Sync completo primero (6 hosts)
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // 2. Sync solo de motoko
    let (status, body) = request(
        app.clone(),
        "POST",
        "/api/v1/sources/src-test/sync?host=motoko.section9.net",
    ).await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["scope"], "host:motoko.section9.net");
    assert_eq!(result["total_hosts"], 1); // solo motoko

    // 3. El cache sigue teniendo los 6 hosts (no se borraron los demás)
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(dataset["hostvars"].as_object().unwrap().len(), 6);
}

// =========================================================================
// Test: sync de un grupo — solo refresca los hosts del grupo
// =========================================================================
#[tokio::test]
async fn sync_group() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    // 1. Sync completo
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // 2. Sync del grupo magi (3 hosts: melchior, balthasar, casper)
    let (status, body) = request(
        app.clone(),
        "POST",
        "/api/v1/sources/src-test/sync?group=magi",
    ).await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], true);
    assert_eq!(result["scope"], "group:magi");
    assert_eq!(result["total_hosts"], 3);

    // 3. El cache sigue teniendo los 6 hosts
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/dataset").await;
    assert_eq!(status, StatusCode::OK);
    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(dataset["hostvars"].as_object().unwrap().len(), 6);
}

// =========================================================================
// Test: sync de host que no existe — el connector reporta error
// =========================================================================
#[tokio::test]
async fn sync_nonexistent_host() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    let (status, body) = request(
        app,
        "POST",
        "/api/v1/sources/src-test/sync?host=togusa.section9.net",
    ).await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["success"], false); // el connector dice "host not found"
}

// =========================================================================
// Test: sync full reporta scope "full"
// =========================================================================
#[tokio::test]
async fn sync_full_reports_scope() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    let (status, body) = request(app, "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["scope"], "full");
}

// =========================================================================
// Test: status muestra age_seconds y is_fresh por host
// =========================================================================
#[tokio::test]
async fn status_shows_per_host_info() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    // Sin sync, status devuelve 404 (no hay cache)
    let (status, _) = request(app.clone(), "GET", "/api/v1/sources/src-test/status").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Sync completo
    let (status, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;
    assert_eq!(status, StatusCode::OK);

    // Ahora status devuelve info
    let (status, body) = request(app.clone(), "GET", "/api/v1/sources/src-test/status").await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["source_id"], "src-test");
    assert_eq!(result["total_hosts"], 6);
    assert_eq!(result["dataset_is_fresh"], true);

    // Cada host tiene su info
    let hosts = result["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 6);
    let motoko = hosts.iter().find(|h| h["hostname"] == "motoko.section9.net").unwrap();
    assert_eq!(motoko["is_fresh"], true);
    assert!(motoko["age_seconds"].as_u64().unwrap() < 5);
}

// =========================================================================
// Test: status filtrado por host
// =========================================================================
#[tokio::test]
async fn status_filter_by_host() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, body) = request(
        app.clone(),
        "GET",
        "/api/v1/sources/src-test/status?host=motoko.section9.net",
    ).await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["total_hosts"], 1);
    assert_eq!(result["hosts"][0]["hostname"], "motoko.section9.net");
}

// =========================================================================
// Test: status filtrado por grupo
// =========================================================================
#[tokio::test]
async fn status_filter_by_group() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, body) = request(
        app.clone(),
        "GET",
        "/api/v1/sources/src-test/status?group=magi",
    ).await;
    assert_eq!(status, StatusCode::OK);
    let result: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(result["total_hosts"], 3);
}

// =========================================================================
// Test: status de host que no existe → 404
// =========================================================================
#[tokio::test]
async fn status_unknown_host_returns_404() {
    let mut sources = HashMap::new();
    sources.insert("src-test".to_string(), test_source("default"));
    let app = unified_api::build_app_with_sources(sources);

    let (_, _) = request(app.clone(), "POST", "/api/v1/sources/src-test/sync").await;

    let (status, _) = request(
        app,
        "GET",
        "/api/v1/sources/src-test/status?host=togusa.section9.net",
    ).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
