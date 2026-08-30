// The audit trail through the real HTTP stack: a mutating request by a named
// key emits one structured event under the `audit` tracing target, carrying
// who, what, which resource, the request id and the outcome.
use axum::http::{Request, StatusCode};
use std::collections::HashMap;
use tower::ServiceExt;
use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use unified_api::domain::cache_entry::CacheEntry;
use unified_api::domain::dataset::Dataset;

// A Write target the subscriber clones per event; the test reads it back.
#[derive(Clone, Default)]
struct CapturedLog(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn admin_key() -> Vec<ResolvedApiKey> {
    vec![ResolvedApiKey {
        name: "key-ops".to_string(),
        secret: "sekrit".to_string(),
        permissions: Permissions::Admin,
    }]
}

#[tokio::test]
async fn a_host_write_lands_in_the_audit_log_with_actor_and_request_id() {
    let (app, state) = unified_api::AppBuilder::new()
        .api_keys(admin_key())
        .build_with_state();
    state.cache.set(
        "src-audited",
        CacheEntry::new(
            Dataset {
                hostvars: HashMap::new(),
                groups: HashMap::new(),
                remove_hosts: Vec::new(),
            },
            3600,
        ),
    );

    let log = CapturedLog::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_env_filter("audit=info")
        .finish();

    // with_default scopes the subscriber to this future's thread — the
    // handler runs inline under oneshot, so the event cannot escape it
    let response = tracing::subscriber::with_default(subscriber, || {
        futures::executor::block_on(async {
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri("/api/v1/sources/src-audited/hosts/web01.example.com")
                        .header("x-api-key", "sekrit")
                        .header("x-request-id", "audit-trace-9")
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(r#"{"os": "OracleLinux"}"#))
                        .unwrap(),
                )
                .await
                .unwrap()
        })
    });
    assert_eq!(response.status(), StatusCode::OK);

    let captured = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(captured.contains("audit"), "no audit event: {}", captured);
    assert!(captured.contains("key-ops"), "actor missing: {}", captured);
    assert!(
        captured.contains("host_put"),
        "action missing: {}",
        captured
    );
    assert!(
        captured.contains("src-audited/web01.example.com"),
        "resource missing: {}",
        captured
    );
    assert!(
        captured.contains("audit-trace-9"),
        "request id missing: {}",
        captured
    );
    assert!(
        captured.contains("success"),
        "outcome missing: {}",
        captured
    );
}

#[tokio::test]
async fn a_denied_write_emits_no_audit_event() {
    let (app, state) = unified_api::AppBuilder::new()
        .api_keys(admin_key())
        .build_with_state();
    state.cache.set(
        "src-audited",
        CacheEntry::new(
            Dataset {
                hostvars: HashMap::new(),
                groups: HashMap::new(),
                remove_hosts: Vec::new(),
            },
            3600,
        ),
    );

    let log = CapturedLog::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_env_filter("audit=info")
        .finish();

    // Wrong key: rejected by the middleware before the handler runs — the
    // attempt belongs to the access log, not here (see audit.rs)
    let response = tracing::subscriber::with_default(subscriber, || {
        futures::executor::block_on(async {
            app.clone()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri("/api/v1/sources/src-audited")
                        .header("x-api-key", "wrong")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
        })
    });
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let captured = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        !captured.contains("evict"),
        "a denied request must not audit: {}",
        captured
    );
}

// =========================================================================
// The configuration API's audit events — the highest-privilege actions the
// service records, and the ones a compliance review asks about first. These
// pin the action strings (config_write / config_write_reload / config_reload)
// and outcomes so a log pipeline built on them cannot break silently.
// =========================================================================

// An app whose configuration API is on, built from a real directory the way
// main builds it — a reload re-resolves api_keys.yaml from that directory, so
// the key must be declared there (env-resolved), not only handed to the
// builder. One env var name per test: set_var is process-wide.
fn config_app(key_env: &str) -> (axum::Router, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("config.yaml"),
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
    )
    .expect("fixture");
    std::fs::write(
        dir.path().join("api_keys.yaml"),
        format!(
            "key-audit:\n  name: \"auditor\"\n  env: \"{}\"\n  role: \"admin\"\n",
            key_env
        ),
    )
    .expect("fixture");
    // SAFETY: the name is unique to the calling test and the value never
    // changes, so no other test can observe a different one.
    unsafe { std::env::set_var(key_env, "sekrit") };

    let cfg =
        unified_api::config::load_config(dir.path().to_str().expect("utf-8 path")).expect("load");
    let live = unified_api::config::RestartOnlySettings::from_config(&cfg);
    let keys =
        unified_api::adapters::r#in::http::auth::resolve_api_keys(&cfg).expect("keys resolve");
    let (app, _) = unified_api::AppBuilder::new()
        .from_config(&cfg)
        .api_keys(keys)
        .config_api(
            std::sync::Arc::new(unified_api::adapters::out::config::fs::FsConfigStore::new(
                dir.path(),
            )),
            live,
        )
        .build_with_state();
    (app, dir)
}

// One request under a capturing audit subscriber; returns status + log text.
fn send_captured(app: &axum::Router, request: Request<axum::body::Body>) -> (StatusCode, String) {
    let log = CapturedLog::default();
    let writer = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_env_filter("audit=info")
        .finish();
    let response = tracing::subscriber::with_default(subscriber, || {
        futures::executor::block_on(async { app.clone().oneshot(request).await.unwrap() })
    });
    let status = response.status();
    let captured = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    (status, captured)
}

fn put_config_yaml(uri: &str, body: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("x-api-key", "sekrit")
        .header("content-type", "application/yaml")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap()
}

const TUNED_SERVER: &str =
    "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  readyz_require_all_sources: true\n";

#[tokio::test]
async fn a_config_write_without_reload_audits_config_write() {
    let (app, _dir) = config_app("UNIFIED_API_TEST_KEY_AUDIT_WRITE");

    let (status, captured) = send_captured(
        &app,
        put_config_yaml("/api/v1/config/config.yaml", TUNED_SERVER),
    );

    assert_eq!(status, StatusCode::OK);
    assert!(captured.contains("auditor"), "actor missing: {}", captured);
    assert!(
        captured.contains("config_write") && !captured.contains("config_write_reload"),
        "a plain write audits config_write, not the reload variant: {}",
        captured
    );
    assert!(
        captured.contains("config.yaml"),
        "resource missing: {}",
        captured
    );
    assert!(
        captured.contains("success"),
        "outcome missing: {}",
        captured
    );
}

#[tokio::test]
async fn a_config_write_with_reload_audits_config_write_reload() {
    let (app, _dir) = config_app("UNIFIED_API_TEST_KEY_AUDIT_WRITE_RELOAD");

    let (status, captured) = send_captured(
        &app,
        put_config_yaml("/api/v1/config/config.yaml?reload=true", TUNED_SERVER),
    );

    assert_eq!(status, StatusCode::OK);
    assert!(
        captured.contains("config_write_reload"),
        "action missing: {}",
        captured
    );
    assert!(
        captured.contains("success"),
        "outcome missing: {}",
        captured
    );
}

#[tokio::test]
async fn a_standalone_reload_audits_config_reload() {
    let (app, _dir) = config_app("UNIFIED_API_TEST_KEY_AUDIT_RELOAD");

    let (status, captured) = send_captured(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/config/reload")
            .header("x-api-key", "sekrit")
            .body(axum::body::Body::empty())
            .unwrap(),
    );

    assert_eq!(status, StatusCode::OK);
    assert!(captured.contains("auditor"), "actor missing: {}", captured);
    assert!(
        captured.contains("config_reload") && !captured.contains("config_write"),
        "action missing or wrong: {}",
        captured
    );
    assert!(
        captured.contains("success"),
        "outcome missing: {}",
        captured
    );
}

#[tokio::test]
async fn a_rejected_config_write_audits_the_rejection() {
    let (app, _dir) = config_app("UNIFIED_API_TEST_KEY_AUDIT_REJECT");

    // An unknown key fails validation on the staged copy; nothing lands, and
    // the audit trail still records that a write was ATTEMPTED and refused.
    let (status, captured) = send_captured(
        &app,
        put_config_yaml(
            "/api/v1/config/config.yaml",
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  bogus_key: true\n",
        ),
    );

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        captured.contains("config_write"),
        "action missing: {}",
        captured
    );
    assert!(
        captured.contains("rejected"),
        "outcome missing: {}",
        captured
    );
}
