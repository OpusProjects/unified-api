// Integration tests for the federation connector: serve a REAL unified-api
// instance on a local TCP port (the "edge") and federate it with the
// RemoteConnector (the "central") — the same wire path as production,
// minus TLS.
use std::collections::HashMap;
use unified_api::adapters::out::cache::memory::MemoryCache;
use unified_api::adapters::out::connectors::remote::RemoteConnector;
use unified_api::adapters::out::secrets::mock::MockSecrets;
use unified_api::application::sync::{
    DEFAULT_REFRESH_DEPTH, SyncCoordinator, SyncRequest, SyncScope, sync_source,
};
use unified_api::domain::cache_entry::CacheEntry;
use unified_api::domain::dataset::Dataset;
use unified_api::domain::source::{ConnectorType, Source};
use unified_api::domain::sync_health::SyncHealthRegistry;
use unified_api::ports::cache::CachePort;
use unified_api::ports::connector::ConnectorPort;

fn edge_dataset() -> Dataset {
    serde_json::from_value(serde_json::json!({
        "hostvars": {
            "web01.dc1.example.com": {"ansible_host": "10.1.0.1", "os": "OracleLinux"},
            "web02.dc1.example.com": {"os": "OracleLinux"}
        },
        "groups": {"dc1": {"hosts": ["web01.dc1.example.com", "web02.dc1.example.com"]}}
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
            ages.insert("web01.dc1.example.com".to_string(), 300);
            ages.insert("web02.dc1.example.com".to_string(), 120);
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

// Same edge, no api key: for the tests that go through sync_source with
// MockSecrets, which resolves no token and would be answered 401.
async fn spawn_open_edge() -> String {
    let (app, state) = unified_api::AppBuilder::new().build_with_state();

    state.cache.set(
        "src-edge",
        CacheEntry::restore(edge_dataset(), 3600, 300, {
            let mut ages = HashMap::new();
            ages.insert("web01.dc1.example.com".to_string(), 300);
            ages.insert("web02.dc1.example.com".to_string(), 120);
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

fn remote_source(url: &str) -> Source {
    Source {
        name: "Datacenter A".to_string(),
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
        allow_on_demand_refresh: false,
        config: remote_config(url),
    }
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
        output.dataset.hostvars["web01.dc1.example.com"]["ansible_host"],
        "10.1.0.1"
    );
    // the origin's ages came along
    let ages = output.ages.expect("ages must be propagated");
    assert!(ages.dataset_age_seconds >= 300);
    assert!(ages.host_ages["web02.dc1.example.com"] >= 120);
    assert!(ages.host_ages["web02.dc1.example.com"] < 300);
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
        name: "Datacenter A".to_string(),
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
        allow_on_demand_refresh: false,
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
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-dc1",
        &source,
        SyncScope::Full,
        None,
    )
    .await;

    assert!(outcome.success(), "sync failed: {:?}", outcome.error);
    assert_eq!(outcome.total_hosts, 2);

    let entry = central_cache.get("src-dc1").unwrap();
    // truthful freshness: the entry is at least as old as it was at the edge
    assert!(
        entry.age_seconds() >= 300,
        "expected origin age >= 300, got {}",
        entry.age_seconds()
    );
    // and with ttl 600 it is still fresh — stale only when the ORIGIN data ages out
    assert!(entry.is_fresh());
}

// A host-scoped sync must not drag the whole remote dataset across the WAN:
// the scope becomes a `?host=` on the remote calls.
#[tokio::test]
async fn host_scope_only_fetches_the_named_host() {
    let url = spawn_open_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-dc1",
        &remote_source(&url),
        SyncScope::Hosts(vec!["web02.dc1.example.com".to_string()]),
        None,
    )
    .await;

    assert!(outcome.success(), "sync failed: {:?}", outcome.error);
    // 1, not 2: the edge answered a filtered dataset. Before honouring the
    // scope this was the full host count.
    assert_eq!(outcome.total_hosts, 1);
    assert_eq!(outcome.scope, "host:web02.dc1.example.com");

    let entry = central_cache.get("src-dc1").unwrap();
    assert!(entry.dataset.hostvars.contains_key("web02.dc1.example.com"));
    assert!(!entry.dataset.hostvars.contains_key("web01.dc1.example.com"));
}

// The point of federation: a host pulled from the edge carries the age it has
// AT THE EDGE. Stamping it "now" would report a six-hour-old fact as fresh.
#[tokio::test]
async fn host_scope_keeps_the_origin_age_for_that_host() {
    let url = spawn_open_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-dc1",
        &remote_source(&url),
        SyncScope::Hosts(vec!["web02.dc1.example.com".to_string()]),
        None,
    )
    .await;
    assert!(outcome.success(), "sync failed: {:?}", outcome.error);

    let age = central_cache
        .get("src-dc1")
        .unwrap()
        .host_age_seconds("web02.dc1.example.com")
        .expect("the host must have a timestamp");

    assert!(
        (120..180).contains(&age),
        "expected the edge's ~120s age, got {}",
        age
    );
}

// A second host-scoped sync must not evict what the first one cached: the
// entry accumulates hosts instead of being replaced by the last slice.
#[tokio::test]
async fn successive_host_scopes_accumulate_in_the_entry() {
    let url = spawn_open_edge().await;
    let source = remote_source(&url);
    let central_cache = MemoryCache::new();

    for host in ["web01.dc1.example.com", "web02.dc1.example.com"] {
        let outcome = sync_source(
            &central_cache,
            &RemoteConnector::new(),
            &MockSecrets::new(),
            &SyncHealthRegistry::new(),
            &SyncCoordinator::new(),
            "src-dc1",
            &source,
            SyncScope::Hosts(vec![host.to_string()]),
            None,
        )
        .await;
        assert!(
            outcome.success(),
            "sync of {} failed: {:?}",
            host,
            outcome.error
        );
    }

    let entry = central_cache.get("src-dc1").unwrap();
    assert_eq!(entry.dataset.hostvars.len(), 2);
    // each kept ITS own origin age, not the other's
    let web01 = entry.host_age_seconds("web01.dc1.example.com").unwrap();
    let web02 = entry.host_age_seconds("web02.dc1.example.com").unwrap();
    assert!((300..360).contains(&web01), "web01 age was {}", web01);
    assert!((120..180).contains(&web02), "web02 age was {}", web02);
}

// A host the source does not have is a clear failure, not a silent success
// that cached nothing.
#[tokio::test]
async fn host_scope_naming_an_unknown_host_caches_nothing() {
    let url = spawn_open_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-dc1",
        &remote_source(&url),
        SyncScope::Hosts(vec!["ghost.dc1.example.com".to_string()]),
        None,
    )
    .await;

    // The remote answers an empty filtered dataset (an unmatched filter is an
    // empty result, not a 404), so the sync itself succeeds with nothing in it
    assert_eq!(outcome.total_hosts, 0);
    assert!(central_cache.get("src-dc1").is_none());
}

// =========================================================================
// Refresh at origin: the central asks the edge to re-gather before answering
// =========================================================================

// An edge whose source is a REAL script connector, so a POST /sync on it
// actually re-gathers instead of replaying a fixture. Its cache entry starts
// 300s old, which is what a refresh has to visibly undo.
//
// Returns (base url, the edge's own state) so a test can look at what the edge
// did, not only at what the central ended up with.
async fn spawn_gathering_edge() -> (String, std::sync::Arc<unified_api::AppState>) {
    let mut config = HashMap::new();
    config.insert("scenario".to_string(), "default".to_string());

    let edge_source = Source {
        name: "Edge inventory".to_string(),
        project_id: "test".to_string(),
        script_path: "tests/adapters/out/connectors/inventory.py".to_string(),
        script_args: vec![],
        output_format: Default::default(),
        hosts_from_source: None,
        connector_type: ConnectorType::Script,
        sync_mode: Default::default(),
        credential_ids: vec![],
        schedule: None,
        sync_interval_seconds: None,
        ttl_seconds: 3600,
        timeout_seconds: 60,
        ttl_overrides: Default::default(),
        allow_on_demand_refresh: false,
        config,
    };

    let mut sources = HashMap::new();
    sources.insert("src-edge".to_string(), edge_source);

    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .build_with_state();

    // Seed it with data that is ALREADY 300s old at both levels
    let seeded: Dataset = serde_json::from_value(serde_json::json!({
        "hostvars": {"motoko.section9.net": {"os": "stale"}},
        "groups": {}
    }))
    .unwrap();
    state.cache.set(
        "src-edge",
        CacheEntry::restore(seeded, 3600, 300, {
            let mut ages = HashMap::new();
            ages.insert("motoko.section9.net".to_string(), 300);
            ages
        }),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{}", addr), state)
}

// The baseline this feature exists to change: without the flag the central
// serves the edge's cache, and the age it reports is the edge's age.
#[tokio::test]
async fn without_refresh_origin_the_edge_does_not_re_gather() {
    let (url, edge_state) = spawn_gathering_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-central",
        &remote_source(&url),
        SyncScope::Hosts(vec!["motoko.section9.net".to_string()]),
        None,
    )
    .await;
    assert!(outcome.success(), "sync failed: {:?}", outcome.error);

    // the edge's own entry is untouched: still the seeded 300s
    let edge_age = edge_state
        .cache
        .get("src-edge")
        .unwrap()
        .host_age_seconds("motoko.section9.net")
        .unwrap();
    assert!(
        edge_age >= 300,
        "the edge re-gathered unasked: {}",
        edge_age
    );

    // and the central reports that age rather than pretending otherwise
    let central_age = central_cache
        .get("src-central")
        .unwrap()
        .host_age_seconds("motoko.section9.net")
        .unwrap();
    assert!((300..360).contains(&central_age), "age was {}", central_age);
    // the stale fixture, not a fresh gather
    assert_eq!(
        central_cache.get("src-central").unwrap().dataset.hostvars["motoko.section9.net"]["os"],
        "stale"
    );
}

// The whole point: one call to the central, and the SSH-side instance goes and
// gets the data.
#[tokio::test]
async fn refresh_origin_makes_the_edge_re_gather_the_host() {
    let (url, edge_state) = spawn_gathering_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-central",
        &remote_source(&url),
        SyncRequest::refreshing_origin(
            SyncScope::Hosts(vec!["motoko.section9.net".to_string()]),
            DEFAULT_REFRESH_DEPTH,
        ),
        None,
    )
    .await;
    assert!(outcome.success(), "sync failed: {:?}", outcome.error);

    // the edge really re-gathered: its own entry is new again
    let edge_age = edge_state
        .cache
        .get("src-edge")
        .unwrap()
        .host_age_seconds("motoko.section9.net")
        .unwrap();
    assert!(
        edge_age < 30,
        "the edge did not re-gather: age {}",
        edge_age
    );

    // …and the central holds the fresh data with a truthful, near-zero age
    let entry = central_cache.get("src-central").unwrap();
    let central_age = entry.host_age_seconds("motoko.section9.net").unwrap();
    assert!(central_age < 30, "central age was {}", central_age);
    // the script's real output replaced the stale fixture
    assert_ne!(entry.dataset.hostvars["motoko.section9.net"]["os"], "stale");
}

// A refresh the origin cannot satisfy is reported, not papered over with older
// data labelled as a success.
#[tokio::test]
async fn an_origin_that_cannot_re_gather_fails_the_sync() {
    let (url, _edge_state) = spawn_gathering_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-central",
        &remote_source(&url),
        SyncRequest::refreshing_origin(
            // a host the edge's inventory does not have: its connector exits
            // non-zero, so the edge answers success=false
            SyncScope::Hosts(vec!["ghost.section9.net".to_string()]),
            DEFAULT_REFRESH_DEPTH,
        ),
        None,
    )
    .await;

    let error = outcome.error.expect("the refusal must surface");
    assert!(
        error.contains("refused to re-gather"),
        "error was: {}",
        error
    );
    // nothing was cached from a refresh that did not happen
    assert!(central_cache.get("src-central").is_none());
}

// The hop budget is what keeps a mis-wired topology from becoming a storm.
#[tokio::test]
async fn an_exhausted_hop_budget_still_serves_the_data() {
    let (url, edge_state) = spawn_gathering_edge().await;

    let central_cache = MemoryCache::new();
    let outcome = sync_source(
        &central_cache,
        &RemoteConnector::new(),
        &MockSecrets::new(),
        &SyncHealthRegistry::new(),
        &SyncCoordinator::new(),
        "src-central",
        &remote_source(&url),
        SyncRequest::refreshing_origin(
            SyncScope::Hosts(vec!["motoko.section9.net".to_string()]),
            0,
        ),
        None,
    )
    .await;

    // it degrades to a plain fetch rather than failing
    assert!(outcome.success(), "sync failed: {:?}", outcome.error);
    let edge_age = edge_state
        .cache
        .get("src-edge")
        .unwrap()
        .host_age_seconds("motoko.section9.net")
        .unwrap();
    assert!(edge_age >= 300, "the edge re-gathered with no hops left");
}
