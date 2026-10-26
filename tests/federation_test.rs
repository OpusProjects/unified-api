// The federation map pushed down to an edge: the real HTTP adapter sending
// it, the edge's route storing it, and the edge serving it back.

use unified_api::adapters::out::peers::http::HttpPeers;
use unified_api::domain::topology::{FederationPush, Topology};
use unified_api::ports::peer::{PeerPort, PeerRequest};

async fn spawn_edge() -> String {
    let app = unified_api::AppBuilder::new().build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

fn request(url: &str) -> PeerRequest {
    PeerRequest {
        url: url.to_string(),
        api_key: None,
        insecure_tls: false,
        timeout_seconds: 5,
        depth: 0,
    }
}

#[tokio::test]
async fn a_pushed_map_is_served_back_by_the_edge() {
    let edge = spawn_edge().await;
    let central = Topology {
        name: None,
        version: "0.35.0".to_string(),
        sources: Some(30),
        peers: Vec::new(),
    };
    HttpPeers::new()
        .push(
            request(&edge),
            FederationPush {
                url: edge.clone(),
                sender_url: Some("https://central.example".to_string()),
                sender: Default::default(),
                topology: central.clone(),
            },
        )
        .await
        .expect("the edge accepts the push");

    let served: serde_json::Value = reqwest::get(format!("{edge}/api/v1/topology/federation"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list = served.as_array().unwrap();
    assert_eq!(list.len(), 1);
    // No keys configured: the sender is recorded as the open API.
    assert_eq!(list[0]["pushed_by"], "open");
    assert_eq!(list[0]["url"], edge);
    assert_eq!(list[0]["topology"]["sources"], 30);
    assert_eq!(list[0]["sender_url"], "https://central.example");
}

#[tokio::test]
async fn a_malformed_push_is_refused_with_a_body() {
    let edge = spawn_edge().await;
    let response = reqwest::Client::new()
        .put(format!("{edge}/api/v1/topology/federation"))
        .json(&serde_json::json!({"url": "x"}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_client_error());
    let body: serde_json::Value = response.json().await.unwrap();
    assert!(body["error"].is_string());
}

// The key a central holds on an edge is restricted to the sources it reads.
// It may push (so no edge has to grant it more), and its picture is stored
// under its name; reading the maps back is for the edge's admins.
#[tokio::test]
async fn a_restricted_key_may_push_but_only_an_admin_reads() {
    use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
    let app = unified_api::AppBuilder::new()
        .api_keys(vec![
            ResolvedApiKey {
                id: "key-central".to_string(),
                // The label, as a description sets it: not what is recorded.
                name: "Central aggregator".to_string(),
                secret: "central-secret".to_string(),
                permissions: Permissions::Scoped {
                    sources: Default::default(),
                    endpoints: Default::default(),
                },
            },
            ResolvedApiKey {
                id: "key-admin".to_string(),
                name: "key-admin".to_string(),
                secret: "admin-secret".to_string(),
                permissions: Permissions::Admin,
            },
        ])
        .build();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let edge = format!("http://{addr}");

    let mut push = request(&edge);
    push.api_key = Some("central-secret".to_string());
    HttpPeers::new()
        .push(
            push,
            FederationPush {
                url: edge.clone(),
                sender_url: Some("https://central.example".to_string()),
                sender: Default::default(),
                topology: Topology {
                    name: None,
                    version: "0.35.0".to_string(),
                    sources: Some(1),
                    peers: Vec::new(),
                },
            },
        )
        .await
        .expect("a restricted key may push");

    let client = reqwest::Client::new();
    let url = format!("{edge}/api/v1/topology/federation");
    let restricted = client
        .get(&url)
        .header("x-api-key", "central-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(restricted.status(), 403);
    let admin: serde_json::Value = client
        .get(&url)
        .header("x-api-key", "admin-secret")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(admin[0]["pushed_by"], "key-central");
}

// server.url is in force from start-up, not only after a reload: it is what
// a central tells its edges it is called.
#[test]
fn server_url_is_read_at_start_up() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("config.yaml"),
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  url: \"https://central.example/\"\n",
    )
    .unwrap();
    let cfg = unified_api::config::load_config(dir.path().to_str().unwrap()).unwrap();
    let (_, state) = unified_api::AppBuilder::new()
        .from_config(&cfg)
        .build_with_state();
    assert_eq!(
        state.config().url.as_deref(),
        Some("https://central.example")
    );
}
