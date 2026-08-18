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
