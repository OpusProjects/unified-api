// The configuration API through the real HTTP stack: what a configuration
// pipeline actually does — read the directory, validate a change, push it,
// and have the running process adopt it.
//
// The properties under test are the ones that make a push safe to automate: a
// rejected change touches nothing, a stale write is refused rather than
// applied, and a reload can neither remove the API's authentication nor lock
// a consumer out with an unresolvable key.
use axum::Router;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;
use unified_api::AppState;
use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey, resolve_api_keys};
use unified_api::adapters::out::config::fs::FsConfigStore;
use unified_api::config::{RestartOnlySettings, load_config};

const KEY: &str = "admin-secret";

// =========================================================================
// Helpers
// =========================================================================

const MINIMAL_SERVER: &str = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n";

// A configuration directory that declares its own admin key, which is what
// makes these fixtures behave like a deployment rather than like a builder
// call: the app is then built by resolving that file, exactly as main does,
// so a reload resolves the same file again and the two agree.
//
// One env var name per test, because set_var is process-wide and the suite
// runs its tests in threads.
fn config_dir(key_env: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    write(dir.path(), "config.yaml", MINIMAL_SERVER);
    write(dir.path(), "api_keys.yaml", &api_keys_yaml(key_env));
    // SAFETY: the name is unique to the calling test (see above), and the
    // value never changes, so no other test can observe a different one.
    unsafe { std::env::set_var(key_env, KEY) };
    dir
}

fn api_keys_yaml(key_env: &str) -> String {
    format!(
        "key-pipeline:\n  name: \"pipeline\"\n  env: \"{}\"\n  role: \"admin\"\n",
        key_env
    )
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).expect("write fixture");
}

fn read(dir: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(dir.join(name)).ok()
}

// The app main would build for this directory, with the configuration API on.
fn app_at(dir: &Path) -> (Router, Arc<AppState>) {
    build(dir, true)
}

fn build(dir: &Path, config_api: bool) -> (Router, Arc<AppState>) {
    let cfg = load_config(dir.to_str().expect("utf-8 path")).expect("fixture must load");
    let live = RestartOnlySettings::from_config(&cfg);
    let keys = resolve_api_keys(&cfg).expect("fixture keys resolve");
    let mut builder = unified_api::AppBuilder::new()
        .from_config(&cfg)
        .api_keys(keys);
    if config_api {
        builder = builder.config_api(Arc::new(FsConfigStore::new(dir)), live);
    }
    builder.build_with_state()
}

async fn send(app: &Router, request: Request<axum::body::Body>) -> (StatusCode, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (status, String::from_utf8(body.to_vec()).expect("utf-8"))
}

async fn send_with_headers(
    app: &Router,
    request: Request<axum::body::Body>,
) -> (StatusCode, axum::http::HeaderMap, String) {
    let response = app.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    (
        status,
        headers,
        String::from_utf8(body.to_vec()).expect("utf-8"),
    )
}

fn get(uri: &str) -> Request<axum::body::Body> {
    Request::builder()
        .uri(uri)
        .header("x-api-key", KEY)
        .body(axum::body::Body::empty())
        .expect("request")
}

fn put_yaml(uri: &str, body: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("x-api-key", KEY)
        .header("content-type", "application/yaml")
        .body(axum::body::Body::from(body.to_string()))
        .expect("request")
}

fn put_json(uri: &str, body: serde_json::Value) -> Request<axum::body::Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("x-api-key", KEY)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("request")
}

fn post(uri: &str) -> Request<axum::body::Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-api-key", KEY)
        .body(axum::body::Body::empty())
        .expect("request")
}

fn json(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("json body")
}

// A source that references a project declared in the same push.
const PROJECTS: &str =
    "prj-inv:\n  name: \"Inventory\"\n  git_url: \"https://example.invalid/i.git\"\n";
const SOURCES: &str = "src-dc4:\n  name: \"DC4\"\n  project_id: \"prj-inv\"\n  script_path: \"inv.py\"\n  ttl_seconds: 300\n";

// =========================================================================
// It is off unless it is turned on
// =========================================================================

#[tokio::test]
async fn every_route_refuses_when_the_configuration_api_is_disabled() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_DISABLED");
    let (app, _) = build(dir.path(), false);

    for request in [
        get("/api/v1/config"),
        get("/api/v1/config/sources.yaml"),
        post("/api/v1/config/reload"),
        post("/api/v1/config/validate"),
    ] {
        let (status, body) = send(&app, request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            body.contains("config_api.enabled"),
            "a 403 has to say how to turn it on: {}",
            body
        );
    }
}

#[tokio::test]
async fn a_restricted_key_cannot_read_the_configuration() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_RESTRICTED");
    let cfg = load_config(dir.path().to_str().expect("utf-8 path")).expect("fixture");
    let live = RestartOnlySettings::from_config(&cfg);
    let (app, _) = unified_api::AppBuilder::new()
        .from_config(&cfg)
        .api_keys(vec![ResolvedApiKey {
            name: "consumer".to_string(),
            secret: KEY.to_string(),
            permissions: Permissions::Scoped {
                sources: ["src-dc4".to_string()].into_iter().collect(),
                endpoints: Default::default(),
            },
        }])
        .config_api(Arc::new(FsConfigStore::new(dir.path())), live)
        .build_with_state();

    // The files describe the estate — which systems exist, which variable
    // holds which credential — so a consumer key has no business reading them.
    let (status, _) = send(&app, get("/api/v1/config")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&app, get("/api/v1/config/config.yaml")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// =========================================================================
// Reading
// =========================================================================

#[tokio::test]
async fn the_inventory_names_what_is_there_and_what_is_not() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_INVENTORY");
    write(dir.path(), "projects.yaml", PROJECTS);
    let (app, _) = app_at(dir.path());

    let (status, body) = send(&app, get("/api/v1/config")).await;
    assert_eq!(status, StatusCode::OK);
    let inventory = json(&body);

    let names: Vec<&str> = inventory["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|f| f["name"].as_str().expect("name"))
        .collect();
    assert_eq!(
        names,
        vec!["config.yaml", "projects.yaml", "api_keys.yaml"],
        "listed in the order the loader reads them"
    );
    assert!(
        inventory["missing"]
            .as_array()
            .expect("missing")
            .iter()
            .any(|n| n == "sources.yaml")
    );
    assert_eq!(inventory["valid"], true);
    assert_eq!(inventory["reload_pending"], false);
    assert_eq!(inventory["generation"], 0);
    assert!(!inventory["etag"].as_str().expect("etag").is_empty());
    assert_eq!(
        inventory["files"][0]["sha256"].as_str().expect("sha").len(),
        64
    );
}

#[tokio::test]
async fn a_file_is_served_verbatim_and_revalidates_with_its_etag() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_ETAG");
    let (app, _) = app_at(dir.path());

    let (status, headers, body) = send_with_headers(&app, get("/api/v1/config/config.yaml")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, MINIMAL_SERVER, "the bytes must come back unchanged");
    let etag = headers
        .get("etag")
        .expect("an ETag")
        .to_str()
        .expect("ascii")
        .to_string();

    let request = Request::builder()
        .uri("/api/v1/config/config.yaml")
        .header("x-api-key", KEY)
        .header("if-none-match", &etag)
        .body(axum::body::Body::empty())
        .expect("request");
    let (status, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a_name_the_loader_does_not_read_is_a_404_naming_the_ones_it_does() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_UNKNOWN");
    let (app, _) = app_at(dir.path());

    let (status, body) = send(&app, get("/api/v1/config/secrets.yaml")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("credentials.yaml"), "body: {}", body);
}

#[tokio::test]
async fn the_static_routes_are_not_shadowed_by_the_file_route() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_ROUTES");
    let (app, _) = app_at(dir.path());

    // "reload" and "validate" sit in the same position as a file name; if the
    // capture won the match they would be 404s for an unknown file.
    let (status, _) = send(&app, post("/api/v1/config/reload")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(&app, post("/api/v1/config/validate")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["valid"], true);
}

// =========================================================================
// Validating
// =========================================================================

#[tokio::test]
async fn validation_reports_every_problem_at_once_and_writes_nothing() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_VALIDATE");
    let (app, _) = app_at(dir.path());

    let body = serde_json::json!({
        "files": {
            "sources.yaml": "src-a:\n  name: \"A\"\n  project_id: \"prj-ghost\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
            "enrichers.yaml": "enr-a:\n  name: \"A\"\n  target_id: \"src-nowhere\"\n  script_path: \"e.py\"\n"
        }
    });
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/config/validate")
        .header("x-api-key", KEY)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_string()))
        .expect("request");

    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "a dry run is not an error");
    let result = json(&body);
    assert_eq!(result["valid"], false);
    let errors = result["errors"].as_array().expect("errors");
    assert!(
        errors.len() >= 2,
        "every problem at once, not the first: {:?}",
        errors
    );
    assert!(!dir.path().join("sources.yaml").exists());
}

// =========================================================================
// Writing
// =========================================================================

#[tokio::test]
async fn a_write_that_would_not_load_is_refused_and_the_directory_is_untouched() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_REJECT");
    let (app, _) = app_at(dir.path());

    let (status, body) = send(
        &app,
        put_yaml(
            "/api/v1/config/sources.yaml",
            "src-a:\n  name: \"A\"\n  project_id: \"prj-ghost\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
        ),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let rejected = json(&body);
    assert!(
        rejected["error"].as_str().is_some(),
        "the ordinary error field has to be there for a generic client"
    );
    assert!(
        rejected["errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e.as_str().expect("str").contains("prj-ghost"))
    );
    assert!(
        read(dir.path(), "sources.yaml").is_none(),
        "a rejected write must not reach the disk"
    );
}

#[tokio::test]
async fn a_valid_write_lands_but_waits_for_a_reload_to_take_effect() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_PENDING");
    let (app, state) = app_at(dir.path());

    let bundle = serde_json::json!({"files": {"projects.yaml": PROJECTS, "sources.yaml": SOURCES}});
    let (status, body) = send(&app, put_json("/api/v1/config", bundle)).await;
    assert_eq!(status, StatusCode::OK, "body: {}", body);

    let result = json(&body);
    assert_eq!(result["summary"]["sources"], 1);
    assert!(result["reloaded"].is_null(), "no reload was asked for");
    assert_eq!(
        result["reload_pending"], true,
        "the files are on disk and the process is still serving the old configuration"
    );
    assert_eq!(read(dir.path(), "sources.yaml").as_deref(), Some(SOURCES));

    // Still not live.
    assert!(state.config().sources.is_empty());

    let (status, body) = send(&app, post("/api/v1/config/reload")).await;
    assert_eq!(status, StatusCode::OK);
    let reloaded = json(&body);
    assert_eq!(reloaded["generation"], 1);
    assert_eq!(reloaded["sources"]["added"][0], "src-dc4");
    assert!(
        reloaded["applied"]
            .as_array()
            .expect("applied")
            .iter()
            .any(|s| s == "sources")
    );
    assert!(state.config().sources.contains_key("src-dc4"));
}

#[tokio::test]
async fn a_write_can_apply_itself_in_the_same_request() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_INLINE");
    let (app, state) = app_at(dir.path());

    let bundle = serde_json::json!({"files": {"projects.yaml": PROJECTS, "sources.yaml": SOURCES}});
    let (status, body) = send(&app, put_json("/api/v1/config?reload=true", bundle)).await;

    assert_eq!(status, StatusCode::OK);
    let result = json(&body);
    assert_eq!(result["reload_pending"], false);
    assert_eq!(result["reloaded"]["generation"], 1);
    assert!(state.config().sources.contains_key("src-dc4"));
    assert_eq!(state.reload.generation(), 1);
}

#[tokio::test]
async fn a_stale_write_is_refused_rather_than_applied_on_top() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_STALE");
    let (app, _) = app_at(dir.path());

    let (_, body) = send(&app, get("/api/v1/config")).await;
    let etag = json(&body)["etag"].as_str().expect("etag").to_string();

    // Somebody else writes first.
    let (status, _) = send(
        &app,
        put_json(
            "/api/v1/config",
            serde_json::json!({"files": {"projects.yaml": PROJECTS}}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let request = Request::builder()
        .method("PUT")
        .uri("/api/v1/config")
        .header("x-api-key", KEY)
        .header("content-type", "application/json")
        .header("if-match", format!("\"{}\"", etag))
        .body(axum::body::Body::from(
            serde_json::json!({"files": {"enrichers.yaml": ""}}).to_string(),
        ))
        .expect("request");
    let (status, body) = send(&app, request).await;

    assert_eq!(status, StatusCode::PRECONDITION_FAILED);
    assert!(body.contains("If-Match"), "body: {}", body);
    assert!(
        read(dir.path(), "enrichers.yaml").is_none(),
        "the refused write must not have landed"
    );
}

#[tokio::test]
async fn a_matching_if_match_goes_through() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_IFMATCH");
    let (app, _) = app_at(dir.path());

    let (_, headers, _) = send_with_headers(&app, get("/api/v1/config/config.yaml")).await;
    let etag = headers.get("etag").expect("etag").to_str().expect("ascii");

    let request = Request::builder()
        .method("PUT")
        .uri("/api/v1/config/config.yaml")
        .header("x-api-key", KEY)
        .header("if-match", etag)
        .body(axum::body::Body::from(
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  readyz_require_all_sources: true\n",
        ))
        .expect("request");

    let (status, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn pushing_the_whole_directory_removes_what_it_left_out() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_PRUNE");
    write(dir.path(), "projects.yaml", PROJECTS);
    write(dir.path(), "sources.yaml", SOURCES);
    let (app, state) = app_at(dir.path());

    let bundle = serde_json::json!({
        "prune": true,
        "files": {
            "config.yaml": MINIMAL_SERVER,
            "api_keys.yaml": api_keys_yaml("UNIFIED_API_TEST_KEY_PRUNE"),
        }
    });
    let (status, body) = send(&app, put_json("/api/v1/config?reload=true", bundle)).await;

    assert_eq!(status, StatusCode::OK, "body: {}", body);
    assert!(read(dir.path(), "sources.yaml").is_none());
    assert!(read(dir.path(), "projects.yaml").is_none());
    assert!(state.config().sources.is_empty(), "and it is live");
    assert_eq!(json(&body)["reloaded"]["sources"]["removed"][0], "src-dc4");
}

#[tokio::test]
async fn deleting_a_file_is_allowed_but_never_config_yaml() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_DELETE");
    write(dir.path(), "enrichers.yaml", "");
    let (app, _) = app_at(dir.path());

    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/config/enrichers.yaml")
        .header("x-api-key", KEY)
        .body(axum::body::Body::empty())
        .expect("request");
    let (status, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert!(read(dir.path(), "enrichers.yaml").is_none());

    let request = Request::builder()
        .method("DELETE")
        .uri("/api/v1/config/config.yaml")
        .header("x-api-key", KEY)
        .body(axum::body::Body::empty())
        .expect("request");
    let (status, body) = send(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("cannot be deleted"), "body: {}", body);
    assert!(read(dir.path(), "config.yaml").is_some());
}

// =========================================================================
// What a reload refuses
// =========================================================================

#[tokio::test]
async fn a_reload_will_not_leave_the_api_without_authentication() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_NOAUTH");
    let (app, state) = app_at(dir.path());

    // An empty api_keys.yaml is a legitimate configuration — it is how a fresh
    // instance starts — but arriving at it under a running process would turn
    // authentication off for everyone who can reach the port.
    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/api_keys.yaml?reload=true", ""),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.contains("without authentication"), "body: {}", body);
    assert_eq!(
        read(dir.path(), "api_keys.yaml").as_deref(),
        Some(api_keys_yaml("UNIFIED_API_TEST_KEY_NOAUTH").as_str()),
        "the refusal must happen before the commit"
    );
    assert_eq!(state.reload.generation(), 0);

    // The key that made the request still works.
    let (status, _) = send(&app, get("/api/v1/config")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn a_key_whose_env_var_is_missing_fails_the_whole_operation() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_ABSENT");
    let (app, _) = app_at(dir.path());

    let keys =
        "key-new:\n  name: \"new\"\n  env: \"UNIFIED_API_TEST_ABSENT_VAR\"\n  role: \"admin\"\n";
    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/api_keys.yaml?reload=true", keys),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        body.contains("UNIFIED_API_TEST_ABSENT_VAR"),
        "body: {}",
        body
    );
    assert_eq!(
        read(dir.path(), "api_keys.yaml").as_deref(),
        Some(api_keys_yaml("UNIFIED_API_TEST_KEY_ABSENT").as_str()),
        "nothing may land that cannot then be applied"
    );
}

#[tokio::test]
async fn a_reloaded_key_file_is_in_force_on_the_next_request() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_ROTATE");
    let (app, _) = app_at(dir.path());

    // SAFETY: a name unique to this test, so no other test observes it.
    unsafe { std::env::set_var("UNIFIED_API_TEST_ROTATED_KEY", "rotated-secret") };
    let keys = "key-rotated:\n  name: \"rotated\"\n  env: \"UNIFIED_API_TEST_ROTATED_KEY\"\n  role: \"admin\"\n";

    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/api_keys.yaml?reload=true", keys),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {}", body);
    assert_eq!(json(&body)["reloaded"]["api_keys"], 1);

    let request = Request::builder()
        .uri("/api/v1/config")
        .header("x-api-key", "rotated-secret")
        .body(axum::body::Body::empty())
        .expect("request");
    let (status, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::OK, "the new key authenticates");

    // And the one it replaced does not.
    let (status, _) = send(&app, get("/api/v1/config")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_setting_a_running_process_cannot_adopt_is_reported_not_dropped() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_RESTART");
    let (app, _) = app_at(dir.path());

    let moved_port = "server:\n  host: \"127.0.0.1\"\n  port: 9999\n";
    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", moved_port),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let restart_required = json(&body)["reloaded"]["restart_required"].clone();
    assert_eq!(restart_required[0], "server.port");

    // And it keeps being reported until a restart actually adopts it, so the
    // state is visible to anything that looks — not only to whoever wrote it.
    let (_, body) = send(&app, get("/api/v1/config")).await;
    assert_eq!(json(&body)["restart_required"][0], "server.port");
}

#[tokio::test]
async fn refresh_limits_and_shutdown_grace_reload_without_a_restart() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_RELOADABLE");
    let (app, state) = app_at(dir.path());

    let tuned = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  \
                 refresh_timeout_seconds: 30\n  refresh_max_concurrent: 2\n  \
                 shutdown_grace_seconds: 5\n";
    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", tuned),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {}", body);

    // Applied, and NOT reported as needing a restart — these keys used to be
    // on the restart-only list.
    let reloaded = json(&body)["reloaded"].clone();
    let applied = reloaded["applied"].to_string();
    for key in [
        "server.refresh_timeout_seconds",
        "server.refresh_max_concurrent",
        "server.shutdown_grace_seconds",
    ] {
        assert!(applied.contains(key), "missing {} in {}", key, applied);
    }
    assert!(
        reloaded["restart_required"]
            .as_array()
            .is_none_or(|keys| keys.is_empty()),
        "restart_required: {}",
        reloaded["restart_required"]
    );

    // The running process adopted the values, not just the report.
    assert_eq!(state.config().refresh_timeout_seconds, 30);
    assert_eq!(state.config().refresh_max_concurrent, 2);
    assert_eq!(state.config().shutdown_grace_seconds, 5);
}

#[tokio::test]
async fn the_body_limit_applies_on_a_reload() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_BODY_LIMIT");
    let (app, _) = app_at(dir.path());

    // Shrink the limit to something a padded config file will exceed.
    let tiny = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  max_body_bytes: 300\n";
    let (status, _) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", tiny),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A padded (but valid) push is now refused, naming the live limit.
    let padded = format!(
        "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  max_body_bytes: 300\n# {}\n",
        "x".repeat(400)
    );
    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", &padded),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        body.contains("server.max_body_bytes") && body.contains("300"),
        "body: {}",
        body
    );

    // The way out is another push: restoring the default limit is itself a
    // small body, so the shrunken limit cannot lock the config API shut.
    let (status, _) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", MINIMAL_SERVER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", &padded),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the default limit admits it again");
}

#[tokio::test]
async fn cors_origins_apply_on_a_reload_in_both_directions() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_CORS");
    let (app, _) = app_at(dir.path());

    let with_origin = |app: &Router| {
        let request = Request::builder()
            .uri("/healthz")
            .header("origin", "https://dash.example")
            .body(axum::body::Body::empty())
            .expect("request");
        let app = app.clone();
        async move {
            let response = app.oneshot(request).await.expect("response");
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        }
    };

    // The default: no origins configured, no CORS headers at all.
    assert_eq!(with_origin(&app).await, None);

    // Somebody stands up a dashboard: its origin arrives by push, and the
    // very next browser request is welcomed — no restart anywhere.
    let dashboard = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  \
                     cors_allowed_origins: [\"https://dash.example\"]\n";
    let (status, body) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", dashboard),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {}", body);
    assert_eq!(
        with_origin(&app).await.as_deref(),
        Some("https://dash.example")
    );

    // And revoking it works the same way — back to no headers.
    let (status, _) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", MINIMAL_SERVER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(with_origin(&app).await, None);
}

// The only test in this binary that scrapes /metrics: the recorder is a
// process-wide global, so a second scraping test would race this one's
// unlabeled gauges.
#[tokio::test]
async fn restart_required_and_generation_are_exported_as_gauges() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_GAUGES");
    let (app, _) = app_at(dir.path());

    let scrape = || {
        Request::builder()
            .uri("/metrics")
            .body(axum::body::Body::empty())
            .expect("request")
    };

    // At boot nothing is pending and no reload has been applied.
    let (status, body) = send(&app, scrape()).await;
    assert_eq!(status, StatusCode::OK, "metrics are public");
    assert!(
        body.contains("unified_api_config_restart_required 0"),
        "body: {}",
        body
    );
    assert!(body.contains("unified_api_config_generation 0"), "{}", body);
    assert!(
        body.contains(&format!(
            "unified_api_build_info{{version=\"{}\"}} 1",
            env!("CARGO_PKG_VERSION")
        )),
        "body: {}",
        body
    );

    // A reload that moves the port cannot adopt it — the gauge must say so.
    let moved_port = "server:\n  host: \"127.0.0.1\"\n  port: 9999\n";
    let (status, _) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", moved_port),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = send(&app, scrape()).await;
    assert!(
        body.contains("unified_api_config_restart_required 1"),
        "one restart-only key is pending, and Prometheus can see it: {}",
        body
    );
    assert!(body.contains("unified_api_config_generation 1"), "{}", body);

    // A follow-up reload that reverts the change clears the pending state.
    let (status, _) = send(
        &app,
        put_yaml("/api/v1/config/config.yaml?reload=true", MINIMAL_SERVER),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = send(&app, scrape()).await;
    assert!(
        body.contains("unified_api_config_restart_required 0"),
        "body: {}",
        body
    );
    assert!(body.contains("unified_api_config_generation 2"), "{}", body);
}

#[tokio::test]
async fn a_hand_edited_directory_that_no_longer_loads_is_visible_and_refuses_to_reload() {
    let dir = config_dir("UNIFIED_API_TEST_KEY_HANDEDIT");
    let (app, state) = app_at(dir.path());

    // Somebody edited the file on the box, bypassing the API entirely.
    write(dir.path(), "config.yaml", "server:\n  porT: 9090\n");

    let (status, body) = send(&app, get("/api/v1/config")).await;
    assert_eq!(status, StatusCode::OK);
    let inventory = json(&body);
    assert_eq!(inventory["valid"], false);
    assert!(
        inventory["errors"][0]
            .as_str()
            .expect("error")
            .contains("porT"),
        "errors: {}",
        inventory["errors"]
    );

    let (status, body) = send(&app, post("/api/v1/config/reload")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("porT"), "body: {}", body);
    assert_eq!(
        state.reload.generation(),
        0,
        "the process keeps running what it had"
    );
}
