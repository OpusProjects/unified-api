use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tower::ServiceExt;

use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use unified_api::adapters::out::secrets::env::EnvSecrets;
use unified_api::domain::credential::Credential;
use unified_api::domain::source::Source;

// The credentials route, through the real router and the real env resolver:
// it says where each secret lives, whether it resolves, and what uses it —
// and never, anywhere in its body, a secret's value.

const SECRET: &str = "s3cr3t-value-that-must-never-appear";

fn credential(yaml: &str) -> Credential {
    serde_saphyr::from_str(yaml).unwrap()
}

fn app() -> axum::Router {
    // A name no other test uses, so setting it cannot race another test.
    // SAFETY: set before the app is built, read only by this test's resolver.
    unsafe { std::env::set_var("CREDPAGE_OK_TOKEN", SECRET) };
    let credentials: HashMap<String, Credential> = [
        (
            "cred-ok".to_string(),
            credential("name: Works\ntype: token\nenv_prefix: CREDPAGE_OK\nsecret_keys:\n  token: TOKEN\n"),
        ),
        (
            "cred-missing".to_string(),
            credential(
                "name: Missing\ntype: username_password\nenv_prefix: CREDPAGE_MISSING\nsecret_keys:\n  username: USER\n  password: PASS\n",
            ),
        ),
    ]
    .into_iter()
    .collect();
    let source: Source = serde_saphyr::from_str(
        "name: x\nproject_id: p\nscript_path: s.py\nttl_seconds: 60\ncredential_ids: [cred-ok]\n",
    )
    .unwrap();

    unified_api::AppBuilder::new()
        .sources([("src-a".to_string(), source)].into_iter().collect())
        .secrets(Arc::new(EnvSecrets::new(credentials.clone())))
        .credentials(credentials, Default::default())
        .api_keys(vec![
            ResolvedApiKey {
                name: "admin".to_string(),
                secret: "adm".to_string(),
                permissions: Permissions::Admin,
            },
            ResolvedApiKey {
                name: "awx".to_string(),
                secret: "awx".to_string(),
                permissions: Permissions::Scoped {
                    sources: HashSet::new(),
                    endpoints: HashSet::new(),
                },
            },
        ])
        .build()
}

async fn get(app: axum::Router, key: &str) -> (StatusCode, String) {
    let resp = app
        .oneshot(
            Request::get("/api/v1/credentials")
                .header("x-api-key", key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn credentials_say_where_and_whether_but_never_what() {
    let (status, body) = get(app(), "adm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains(SECRET), "a secret value leaked: {body}");

    let list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let by_id = |id: &str| list.iter().find(|c| c["credential_id"] == id).unwrap();

    let ok = by_id("cred-ok");
    assert_eq!(ok["resolves"], true);
    assert_eq!(ok["backend"], "env");
    assert_eq!(ok["env_vars"], serde_json::json!(["CREDPAGE_OK_TOKEN"]));
    assert_eq!(ok["used_by"]["sources"], serde_json::json!(["src-a"]));

    // Missing: says so, and names what is missing rather than failing blank.
    let missing = by_id("cred-missing");
    assert_eq!(missing["resolves"], false);
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("CREDPAGE_MISSING_"),
        "{missing}"
    );
    assert_eq!(missing["used_by"]["sources"], serde_json::json!([]));
}

#[tokio::test]
async fn credentials_are_admin_only() {
    let (status, _) = get(app(), "awx").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// The configuration in force: defaults included (what config.yaml leaves
// out is still answered), admin-only like the configuration API.
#[tokio::test]
async fn settings_show_values_in_force_to_admins_only() {
    let request = |key: &'static str| {
        app().oneshot(
            Request::get("/api/v1/settings")
                .header("x-api-key", key)
                .body(Body::empty())
                .unwrap(),
        )
    };
    let resp = request("adm").await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let settings: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Set nowhere in this app, so these are the defaults.
    assert_eq!(settings["server"]["metrics_require_auth"], false);
    assert_eq!(settings["server"]["ui"]["session_ttl_seconds"], 3600);
    assert_eq!(settings["secrets"]["vault"], serde_json::Value::Null);
    assert_eq!(settings["restart_pending"], serde_json::json!([]));

    assert_eq!(
        request("awx").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
}
