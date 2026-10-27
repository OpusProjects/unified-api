// The connectors routes through the real router, on a configuration loaded
// from item files exactly as main loads it: what each connector runs, which
// sources use it, and how those sources are doing.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use unified_api::config::load_config;

const SCRIPT: &str = "tests/adapters/out/connectors/inventory.py";

fn config_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, body: String| {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    };
    write(
        "config.yaml",
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n".to_string(),
    );
    write(
        "projects/prj-inv.yaml",
        "projects:\n  - name: prj-inv\n    git_url: \"https://example.invalid/inv.git\"\n"
            .to_string(),
    );
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(SCRIPT);
    write(
        "connectors/sample.yaml",
        format!(
            "connectors:\n  - name: sample\n    description: \"Sample inventory\"\n    \
             project: prj-inv\n    path: \"{}\"\n    script_args: [\"--list\"]\n    \
             timeout_seconds: 120\n",
            script.display()
        ),
    );
    write(
        "sources/a/src-ok.yaml",
        "sources:\n  - name: src-ok\n    description: \"Works\"\n    connector: sample\n    \
         ttl_seconds: 60\n    config:\n      scenario: default\n"
            .to_string(),
    );
    write(
        "sources/b/src-broken.yaml",
        "sources:\n  - name: src-broken\n    connector: sample\n    ttl_seconds: 60\n    \
         timeout_seconds: 30\n    config:\n      scenario: error\n"
            .to_string(),
    );
    write(
        "sources/src-edge.yaml",
        "sources:\n  - name: src-edge\n    connector: remote\n    ttl_seconds: 60\n    \
         config:\n      url: \"http://127.0.0.1:9\"\n      source: src-facts\n"
            .to_string(),
    );
    dir
}

fn app(admin: bool) -> axum::Router {
    let dir = config_dir();
    let cfg = load_config(dir.path().to_str().unwrap()).expect("fixture loads");
    let permissions = if admin {
        Permissions::Admin
    } else {
        Permissions::Scoped {
            sources: ["src-ok".to_string()].into_iter().collect(),
            endpoints: Default::default(),
        }
    };
    unified_api::AppBuilder::new()
        .from_config(&cfg)
        .api_keys(vec![ResolvedApiKey {
            id: "test".to_string(),
            name: "test".to_string(),
            secret: "k".to_string(),
            permissions,
        }])
        .build()
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", "k")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn the_list_names_what_each_connector_runs_and_who_uses_it() {
    let app = app(true);
    let (status, body) = call(&app, "GET", "/api/v1/connectors").await;
    assert_eq!(status, StatusCode::OK);
    let list = body.as_array().unwrap();
    let ids: Vec<&str> = list
        .iter()
        .map(|c| c["connector_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["sample", "remote", "static_inventory"],
        "configured first, the built-ins last"
    );

    let sample = &list[0];
    assert_eq!(sample["name"], "Sample inventory");
    assert_eq!(sample["connector_type"], "script");
    assert_eq!(sample["built_in"], false);
    assert_eq!(sample["project_id"], "prj-inv");
    assert!(sample["path"].as_str().unwrap().ends_with("inventory.py"));
    assert_eq!(sample["script_args"], serde_json::json!(["--list"]));
    assert!(sample.get("output_format").is_none(), "removed in 0.36");
    assert_eq!(sample["timeout_seconds"], 120);
    let users: Vec<(&str, u64)> = sample["used_by"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["source_id"].as_str().unwrap(),
                u["timeout_seconds"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        users,
        vec![("src-broken", 30), ("src-ok", 120)],
        "each source with the timeout it actually syncs under"
    );
    assert_eq!(
        sample["health"],
        serde_json::json!({"sources": 2, "healthy": 0, "failing": 0, "never_synced": 2})
    );

    let remote = &list[1];
    assert_eq!(remote["built_in"], true);
    assert_eq!(remote["connector_type"], "remote");
    assert!(remote["path"].is_null());
    assert!(remote.get("script_path").is_none());
    assert_eq!(remote["used_by"][0]["source_id"], "src-edge");
    assert_eq!(list[2]["used_by"], serde_json::json!([]));
}

#[tokio::test]
async fn health_counts_follow_the_syncs_of_the_sources_using_it() {
    let app = app(true);
    call(&app, "POST", "/api/v1/sources/src-ok/sync").await;
    call(&app, "POST", "/api/v1/sources/src-broken/sync").await;

    let (status, body) = call(&app, "GET", "/api/v1/connectors/sample").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["health"],
        serde_json::json!({"sources": 2, "healthy": 1, "failing": 1, "never_synced": 0})
    );
    let broken = body["used_by"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["source_id"] == "src-broken")
        .unwrap();
    assert_eq!(broken["failing"], true);
    assert_eq!(broken["synced"], false);
}

#[tokio::test]
async fn one_connector_by_id_built_ins_included_and_a_404_otherwise() {
    let app = app(true);
    let (status, body) = call(&app, "GET", "/api/v1/connectors/static_inventory").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["built_in"], true);

    let (status, body) = call(&app, "GET", "/api/v1/connectors/ghost").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("ghost"));
}

#[tokio::test]
async fn a_restricted_key_cannot_read_connectors() {
    let app = app(false);
    for uri in ["/api/v1/connectors", "/api/v1/connectors/sample"] {
        let (status, body) = call(&app, "GET", uri).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}");
        assert!(body["error"].is_string(), "an error carries a body");
    }
    let (_, index) = call(&app, "GET", "/api/v1/").await;
    assert!(
        index.get("connectors").is_none(),
        "no link it cannot follow"
    );
}

#[tokio::test]
async fn a_source_and_a_project_name_their_connector() {
    let app = app(true);
    let (_, index) = call(&app, "GET", "/api/v1/").await;
    assert_eq!(index["connectors"], "/api/v1/connectors");

    let (_, source) = call(&app, "GET", "/api/v1/sources/src-ok").await;
    assert_eq!(source["name"], "Works", "the label is the description");
    assert_eq!(source["gathering"]["connector"], "sample");
    assert_eq!(source["gathering"]["connector_type"], "script");
    assert_eq!(source["gathering"]["project_id"], "prj-inv");
    assert_eq!(source["gathering"]["timeout_seconds"], 120);

    let (_, edge) = call(&app, "GET", "/api/v1/sources/src-edge").await;
    assert_eq!(edge["gathering"]["connector"], "remote");
    assert!(edge["gathering"]["project_id"].is_null());

    let (_, projects) = call(&app, "GET", "/api/v1/projects").await;
    assert_eq!(
        projects[0]["used_by"]["connectors"],
        serde_json::json!(["sample"])
    );
    assert_eq!(
        projects[0]["used_by"]["sources"],
        serde_json::json!(["src-broken", "src-ok"])
    );
}
