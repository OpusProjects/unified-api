// Los archivos en tests/ son integration tests — se compilan como binarios separados
// y ven el proyecto como una librería externa (por eso usamos `unified_api::` en vez de `crate::`)
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

// =========================================================================
// Helpers
// =========================================================================

// Hace una petición GET a la app y devuelve (status_code, body_string)
// Es como hacer curl desde dentro del test, pero sin TCP — todo en memoria.
async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let request = Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();

    // .oneshot() envía una petición al router y devuelve la respuesta
    // como si fuera un servidor HTTP pero sin red — pura lógica
    let response = app.oneshot(request).await.unwrap();

    let status = response.status();
    // Extraemos el body completo y lo convertimos a String
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_str = String::from_utf8(body.to_vec()).unwrap();

    (status, body_str)
}

// =========================================================================
// Tests: health checks
// =========================================================================

// #[tokio::test] es como #[test] pero para funciones async
#[tokio::test]
async fn healthz_returns_ok() {
    let app = unified_api::build_app();

    let (status, body) = get(app, "/healthz").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn readyz_returns_ok() {
    let app = unified_api::build_app();

    let (status, body) = get(app, "/readyz").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "ok");
}

// =========================================================================
// Tests: sources API sin datos — cache vacío
// =========================================================================

#[tokio::test]
async fn list_sources_empty_cache() {
    let app = unified_api::build_app();

    let (status, body) = get(app, "/api/v1/sources").await;

    assert_eq!(status, StatusCode::OK);
    // Cache vacío = array JSON vacío
    let sources: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    assert_eq!(sources.len(), 0);
}

#[tokio::test]
async fn get_dataset_not_found() {
    let app = unified_api::build_app();

    let (status, _body) = get(app, "/api/v1/sources/no-existe/dataset").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// Tests: sources API con datos demo — flujo completo
// =========================================================================

#[tokio::test]
async fn list_sources_with_demo_data() {
    let app = unified_api::build_app_with_demo_data();

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
    let app = unified_api::build_app_with_demo_data();

    let (status, body) = get(app, "/api/v1/sources/src-demo/dataset").await;

    assert_eq!(status, StatusCode::OK);

    let dataset: serde_json::Value = serde_json::from_str(&body).unwrap();

    // Verificamos hosts de Section 9 y SEELE
    let hostvars = &dataset["hostvars"];
    assert!(hostvars["motoko.section9.net"].is_object());
    assert_eq!(hostvars["motoko.section9.net"]["ansible_host"], "10.9.1.1");
    assert_eq!(hostvars["motoko.section9.net"]["role"], "commander");

    assert!(hostvars["melchior.seele.net"].is_object());
    assert_eq!(hostvars["melchior.seele.net"]["role"], "magi-system");

    // Verificamos los grupos
    let groups = &dataset["groups"];
    assert!(groups["section9"].is_object());
    assert!(groups["seele"].is_object());

    // Section 9 tiene a Motoko
    let s9_hosts = groups["section9"]["hosts"].as_array().unwrap();
    assert!(s9_hosts.contains(&serde_json::json!("motoko.section9.net")));
}

#[tokio::test]
async fn nonexistent_route_returns_404() {
    let app = unified_api::build_app();

    let (status, _body) = get(app, "/api/v1/ruta-inventada").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}
