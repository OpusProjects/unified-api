// Integration tests for the federation connector: serve a REAL unified-api
// instance on a local TCP port (the "edge") and federate it with the
// RemoteConnector (the "central") — the same wire path as production,
// minus TLS.
use std::collections::HashMap;
use unified_api::adapters::out::cache::memory::MemoryCache;
use unified_api::adapters::out::connectors::remote::RemoteConnector;
use unified_api::adapters::out::secrets::mock::MockSecrets;
use unified_api::application::sync::{SyncScope, sync_source};
use unified_api::domain::cache_entry::CacheEntry;
use unified_api::domain::dataset::Dataset;
use unified_api::domain::source::{ConnectorType, Source};
use unified_api::ports::cache::CachePort;
use unified_api::ports::connector::ConnectorPort;

fn edge_dataset() -> Dataset {
    serde_json::from_value(serde_json::json!({
        "hostvars": {
            "web01.mad.example.com": {"ansible_host": "10.1.0.1", "os": "OracleLinux"},
            "web02.mad.example.com": {"os": "OracleLinux"}
        },
        "groups": {"madrid": {"hosts": ["web01.mad.example.com", "web02.mad.example.com"]}}
    }))
    .unwrap()
}

// Boot an edge instance with an api key and a cached source whose entry is
// ALREADY 300s old (that pre-existing age is what federation must not lose).
// Returns its base URL.
async fn spawn_edge(api_key: &str) -> String {
    let (app, state) = unified_api::AppBuilder::new()
        .api_key(Some(api_key.to_string()))
        .build_with_state();

    state.cache.set(
        "src-edge",
        CacheEntry::restore(edge_dataset(), 3600, 300, {
            let mut ages = HashMap::new();
            ages.insert("web01.mad.example.com".to_string(), 300);
            ages.insert("web02.mad.example.com".to_string(), 120);
            ages
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{}", addr)
}

fn remote_config(url: &str) -> HashMap<String, String> {
    let mut config = HashMap::new();
    config.insert("url".to_string(), url.to_string());
    config
}

fn token(key: &str) -> HashMap<String, String> {
    let mut creds = HashMap::new();
    creds.insert("token".to_string(), key.to_string());
    creds
}

#[tokio::test]
async fn fetches_the_remote_dataset_and_its_ages() {
    let url = spawn_edge("edge-key").await;

    let connector = RemoteConnector::new();
    let output = connector
        .execute(
            "src-edge",
            &[],
            Default::default(),
            &remote_config(&url),
            &token("edge-key"),
        )
        .await
        .expect("remote fetch must succeed");

    assert_eq!(output.dataset.hostvars.len(), 2);
    assert_eq!(
        output.dataset.hostvars["web01.mad.example.com"]["ansible_host"],
        "10.1.0.1"
    );
    // the origin's ages came along
    let ages = output.ages.expect("ages must be propagated");
    assert!(ages.dataset_age_seconds >= 300);
    assert!(ages.host_ages["web02.mad.example.com"] >= 120);
    assert!(ages.host_ages["web02.mad.example.com"] < 300);
}

#[tokio::test]
async fn wrong_key_is_a_clear_401_error() {
    let url = spawn_edge("edge-key").await;

    let connector = RemoteConnector::new();
    let err = connector
        .execute(
            "src-edge",
            &[],
            Default::default(),
            &remote_config(&url),
            &token("wrong"),
        )
        .await
        .expect_err("bad key must fail");
    assert!(err.message.contains("401"), "error was: {}", err.message);
}

#[tokio::test]
async fn unknown_remote_source_is_a_clear_404_error() {
    let url = spawn_edge("edge-key").await;

    let connector = RemoteConnector::new();
    let err = connector
        .execute(
            "src-ghost",
            &[],
            Default::default(),
            &remote_config(&url),
            &token("edge-key"),
        )
        .await
        .expect_err("unknown source must fail");
    assert!(err.message.contains("404"), "error was: {}", err.message);
}

// The full chain: central sync_source with a remote source → the central
// cache entry must carry the ORIGIN's age, not age zero.
#[tokio::test]
async fn central_cache_entry_keeps_the_origin_age() {
    let url = spawn_edge("edge-key").await;

    let source = Source {
        name: "DC Madrid".to_string(),
        project_id: "prj-unused".to_string(),
        script_path: "src-edge".to_string(),
        script_args: vec![],
        output_format: Default::default(),
        hosts_from_source: None,
        connector_type: ConnectorType::Remote,
        sync_mode: Default::default(),
        credential_ids: vec![],
        schedule: None,
        sync_interval_seconds: None,
        ttl_seconds: 600,
        timeout_seconds: 60,
        ttl_overrides: Default::default(),
        config: remote_config(&url),
    };

    // MockSecrets resolves nothing; the edge is queried without a key…
    // which would 401. Use an open edge instead for this test.
    let open_url = {
        let (app, state) = unified_api::AppBuilder::new().build_with_state();
        state.cache.set(
            "src-edge",
            CacheEntry::restore(edge_dataset(), 3600, 300, HashMap::new()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{}", addr)
    };
    let source = Source {
        config: remote_config(&open_url),
        ..source
    };

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        "src-madrid",
        &source,
        SyncScope::Full,
    )
    .await;

    assert!(outcome.success(), "sync failed: {:?}", outcome.error);
    assert_eq!(outcome.total_hosts, 2);

    let entry = central_cache.get("src-madrid").unwrap();
    // truthful freshness: the entry is at least as old as it was at the edge
    assert!(
        entry.age_seconds() >= 300,
        "expected origin age >= 300, got {}",
        entry.age_seconds()
    );
    // and with ttl 600 it is still fresh — stale only when the ORIGIN data ages out
    assert!(entry.is_fresh());
}
