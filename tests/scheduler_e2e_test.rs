// The scheduler through a real AppBuilder, end to end: a configured interval
// actually lands a sync in the cache and flips /readyz — the wiring test the
// feature never had (the drain test proves tasks stop; nothing proved a tick
// does its job).
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

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

#[tokio::test]
async fn a_scheduled_source_syncs_into_the_cache_and_readyz_follows() {
    let source: unified_api::domain::source::Source = serde_yaml_ng::from_str(concat!(
        "name: Scheduled\n",
        "project_id: p\n",
        // The sample connector the shipped config uses — real process spawn
        "script_path: \"tests/adapters/out/connectors/inventory.py\"\n",
        "ttl_seconds: 3600\n",
        "sync_interval_seconds: 1\n",
    ))
    .expect("source fixture");

    let mut sources = std::collections::HashMap::new();
    sources.insert("src-sched".to_string(), source);
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .build_with_state();

    let (status, _) = get(app.clone(), "/readyz").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "nothing synced yet"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let handles =
        unified_api::adapters::r#in::scheduler::start_sync_tasks(state.clone(), shutdown_rx);
    assert_eq!(handles.len(), 1);

    // Jitter (< 1s for a 1-second interval) + the first tick + a real python
    // process: give it a generous real-time budget, poll cheaply
    let mut synced = false;
    for _ in 0..300 {
        if state.cache.get("src-sched").is_some() {
            synced = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(synced, "the scheduled sync never landed in the cache");

    // The tick's work is visible everywhere a consumer would look
    let (status, body) = get(app.clone(), "/readyz").await;
    assert_eq!(status, StatusCode::OK, "body was: {}", body);

    let (status, body) = get(app.clone(), "/api/v1/sources/src-sched/dataset").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.is_empty());

    let (_, body) = get(app, "/api/v1/sources/src-sched/status").await;
    let status_body: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        status_body["sync_health"]["consecutive_failures"], 0,
        "the scheduled sync must record health: {}",
        status_body
    );

    // Drain so the task is not left syncing into a dropped runtime
    shutdown_tx.send(true).expect("receiver alive");
    for handle in handles {
        tokio::time::timeout(std::time::Duration::from_secs(10), handle)
            .await
            .expect("task drains")
            .expect("task does not panic");
    }
}
