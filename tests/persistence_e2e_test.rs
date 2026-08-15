// The persistence feature end to end: a populated cache is snapshotted, a
// brand-new app ("the restarted pod") reloads it before serving, and the
// pre-restart data is served over HTTP with its freshness intact. Every prior
// persistence test exercised save/load in isolation; none proved the round
// trip through a real app.
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use unified_api::adapters::out::cache::persistence;
use unified_api::domain::cache_entry::CacheEntry;

async fn get(app: axum::Router, path: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

fn source() -> unified_api::domain::source::Source {
    serde_yaml_ng::from_str(
        "name: Persisted\nproject_id: p\nscript_path: unused.py\nttl_seconds: 3600\n",
    )
    .expect("source fixture")
}

#[tokio::test]
async fn a_snapshot_survives_a_restart_and_serves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cache.json");

    let dataset: unified_api::domain::dataset::Dataset = serde_json::from_str(
        r#"{
            "hostvars": {"motoko.section9.net": {"role": "commander"}},
            "groups": {"section9": {"hosts": ["motoko.section9.net"]}}
        }"#,
    )
    .unwrap();

    // "First boot": an app with data in its cache, snapshotted to disk
    {
        let mut sources = std::collections::HashMap::new();
        sources.insert("src-a".to_string(), source());
        let (_, state) = unified_api::AppBuilder::new()
            .sources(sources)
            .build_with_state();
        state.cache.set("src-a", CacheEntry::new(dataset, 3600));

        let saved = persistence::save(&*state.cache, &path)
            .await
            .expect("snapshot");
        assert_eq!(saved, 1);
    }

    // "Restart": a brand-new process — new app, new empty cache — reloads the
    // snapshot before serving, exactly as main does
    let mut sources = std::collections::HashMap::new();
    sources.insert("src-a".to_string(), source());
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .build_with_state();

    let (status, body) = get(app.clone(), "/readyz").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "empty before the load: {}",
        body
    );

    persistence::load_or_warn(&*state.cache, &path).await;

    // /readyz is green from the reload alone — the point of persistence
    let (status, body) = get(app.clone(), "/readyz").await;
    assert_eq!(status, StatusCode::OK, "body was: {}", body);

    // The pre-restart data is served...
    let (status, body) = get(app.clone(), "/api/v1/sources/src-a/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("motoko.section9.net"), "body was: {}", body);
    assert!(body.contains("commander"));

    // ...with truthful freshness (restored, not stamped "just gathered")
    let (status, body) = get(app, "/api/v1/sources/src-a/status").await;
    assert_eq!(status, StatusCode::OK);
    let status_body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status_body["dataset_is_fresh"], true);
    assert_eq!(status_body["total_hosts"], 1);
}
