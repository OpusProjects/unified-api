// Integration tests for views: one id over several sources, routed per host.
//
// The members run the REAL process connector (counting.py), each with its own
// counter file. That is what makes the central assertion possible: not "did the
// data come back fresh" but "which member paid for the gather", which is the
// whole point of routing by declared ownership.
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::collections::HashMap;
use tower::ServiceExt;
use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use unified_api::domain::cache_entry::CacheEntry;
use unified_api::domain::dataset::Dataset;
use unified_api::domain::source::{ConnectorType, Source, TtlOverrides};
use unified_api::domain::view::View;

// =========================================================================
// Helpers
// =========================================================================

struct Response {
    status: StatusCode,
    body: String,
    refreshed: Option<String>,
    refreshed_hosts: Option<String>,
    refresh_error: Option<String>,
}

impl Response {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| {
            panic!("expected JSON, got {:?} ({})", self.body, e);
        })
    }
}

async fn request(app: axum::Router, method: &str, path: &str, key: Option<&str>) -> Response {
    let mut builder = Request::builder().method(method).uri(path);
    if let Some(key) = key {
        builder = builder.header("x-api-key", key);
    }
    let response = app
        .oneshot(builder.body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();

    let status = response.status();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .map(|v| v.to_str().unwrap().to_string())
    };
    let refreshed = header("x-unified-api-refreshed");
    let refreshed_hosts = header("x-unified-api-refreshed-hosts");
    let refresh_error = header("x-unified-api-refresh-error");

    let body = response.into_body().collect().await.unwrap().to_bytes();
    Response {
        status,
        body: String::from_utf8(body.to_vec()).unwrap(),
        refreshed,
        refreshed_hosts,
        refresh_error,
    }
}

async fn get(app: axum::Router, path: &str) -> Response {
    request(app, "GET", path, None).await
}

fn counter_path(name: &str) -> String {
    let path = std::env::temp_dir().join(format!("unified-api-view-{}.log", name));
    let _ = std::fs::remove_file(&path);
    path.to_string_lossy().to_string()
}

fn gathers(counter_file: &str) -> Vec<String> {
    std::fs::read_to_string(counter_file)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

fn member_source(counter_file: &str, ttl_seconds: u64, allow: bool) -> Source {
    let mut config = HashMap::new();
    config.insert("counter_file".to_string(), counter_file.to_string());

    Source {
        name: "Member".to_string(),
        project_id: "test".to_string(),
        script_path: "tests/adapters/out/connectors/counting.py".to_string(),
        script_args: vec![],
        output_format: Default::default(),
        hosts_from_source: None,
        connector_type: ConnectorType::Script,
        sync_mode: Default::default(),
        credential_ids: vec![],
        schedule: None,
        sync_interval_seconds: None,
        ttl_seconds,
        timeout_seconds: 60,
        ttl_overrides: TtlOverrides::default(),
        allow_on_demand_refresh: allow,
        advertise_scope: None,
        config,
    }
}

// The inventory both members resolve ownership against: it says which site each
// host belongs to. h3 is in neither — a host the view only knows because a
// member claims it literally.
fn inventory() -> CacheEntry {
    let dataset: Dataset = serde_json::from_value(serde_json::json!({
        "hostvars": {"h1.example": {}, "h2.example": {}},
        "groups": {
            "site_a": {"hosts": ["h1.example"]},
            "site_b": {"hosts": ["h2.example"]}
        }
    }))
    .unwrap();
    CacheEntry::new(dataset, 7200)
}

// One member's cached facts for one host, already `age_seconds` old.
fn facts(hostname: &str, os: &str, age_seconds: u64, ttl_seconds: u64) -> CacheEntry {
    let dataset: Dataset = serde_json::from_value(serde_json::json!({
        "hostvars": {hostname: {"os": os, "gathered": false}},
        "groups": {
            "linux": {"hosts": [hostname]},
            format!("only_{}", hostname): {"hosts": [hostname]}
        }
    }))
    .unwrap();
    let ages = HashMap::from([(hostname.to_string(), age_seconds)]);
    CacheEntry::restore(dataset, ttl_seconds, age_seconds, ages)
}

const VIEW_YAML: &str = "\
name: All sites
members:
  - source: src-a
    owns:
      source: src-inventory
      groups: [\"site_a\"]
  - source: src-b
    owns:
      source: src-inventory
      groups: [\"site_b\"]
      hosts: [\"h3.example\"]
";

struct Fixture {
    app: axum::Router,
    counter_a: String,
    counter_b: String,
}

// Two members, one inventory, one view over them. `member_ttl` is what each
// member's cache entry carries; `view_ttl` is the view's own policy, if any.
// The facts are seeded 300s old, so a 60s member TTL makes them stale without
// the test having to wait for anything.
fn fixture(name: &str, member_ttl: u64, view_ttl: Option<u64>, allow: bool) -> Fixture {
    let counter_a = counter_path(&format!("{}-a", name));
    let counter_b = counter_path(&format!("{}-b", name));

    let sources = HashMap::from([
        (
            "src-inventory".to_string(),
            member_source("/dev/null", 7200, false),
        ),
        (
            "src-a".to_string(),
            member_source(&counter_a, member_ttl, allow),
        ),
        (
            "src-b".to_string(),
            member_source(&counter_b, member_ttl, allow),
        ),
    ]);

    let mut view: View = serde_yaml_ng::from_str(VIEW_YAML).unwrap();
    view.ttl_seconds = view_ttl;

    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .views(HashMap::from([("vw-all".to_string(), view)]))
        .build_with_state();

    state.cache.set("src-inventory", inventory());
    state
        .cache
        .set("src-a", facts("h1.example", "OracleLinux", 300, member_ttl));
    state
        .cache
        .set("src-b", facts("h2.example", "RHEL", 300, member_ttl));

    Fixture {
        app,
        counter_a,
        counter_b,
    }
}

// =========================================================================
// Reading: one id, routed per host
// =========================================================================

#[tokio::test]
async fn each_host_is_served_from_the_member_that_owns_it() {
    let f = fixture("route", 60, None, false);

    let a = get(
        f.app.clone(),
        "/api/v1/sources/vw-all/dataset?host=h1.example",
    )
    .await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(a.json()["hostvars"]["h1.example"]["os"], "OracleLinux");

    let b = get(f.app, "/api/v1/sources/vw-all/dataset?host=h2.example").await;
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(b.json()["hostvars"]["h2.example"]["os"], "RHEL");
}

// The migration promise: a consumer changes one id and its parsing is untouched.
#[tokio::test]
async fn a_filtered_read_has_the_same_shape_as_the_member_it_replaces() {
    let f = fixture("shape-filtered", 60, None, false);

    let from_member = get(
        f.app.clone(),
        "/api/v1/sources/src-a/dataset?host=h1.example",
    )
    .await;
    let from_view = get(f.app, "/api/v1/sources/vw-all/dataset?host=h1.example").await;

    let keys = |value: &serde_json::Value| -> Vec<String> {
        let mut names: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        names.sort();
        names
    };
    assert_eq!(keys(&from_member.json()), keys(&from_view.json()));
    assert_eq!(
        from_member.json()["hostvars"]["h1.example"],
        from_view.json()["hostvars"]["h1.example"]
    );
    assert_eq!(from_view.json()["source_id"], "vw-all");
}

#[tokio::test]
async fn the_plain_dataset_is_the_union_in_the_raw_source_shape() {
    let f = fixture("union", 60, None, false);

    let from_member = get(f.app.clone(), "/api/v1/sources/src-a/dataset").await;
    let from_view = get(f.app, "/api/v1/sources/vw-all/dataset").await;

    let keys = |value: &serde_json::Value| -> Vec<String> {
        let mut names: Vec<String> = value.as_object().unwrap().keys().cloned().collect();
        names.sort();
        names
    };
    assert_eq!(keys(&from_member.json()), keys(&from_view.json()));

    let body = from_view.json();
    assert_eq!(body["hostvars"]["h1.example"]["os"], "OracleLinux");
    assert_eq!(body["hostvars"]["h2.example"]["os"], "RHEL");
    // h3 is claimed but nobody has ever gathered it, so it is not in the data
    assert!(body["hostvars"]["h3.example"].is_null());
}

#[tokio::test]
async fn same_named_groups_merge_into_one_namespace() {
    let f = fixture("groups", 60, None, false);

    let body = get(f.app.clone(), "/api/v1/sources/vw-all/dataset")
        .await
        .json();
    let mut linux: Vec<&str> = body["groups"]["linux"]["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    linux.sort();
    assert_eq!(linux, vec!["h1.example", "h2.example"]);

    // and the discovery route agrees with the dataset
    let groups = get(f.app, "/api/v1/sources/vw-all/groups").await.json();
    let named: Vec<&str> = groups
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap())
        .collect();
    assert!(named.contains(&"linux"));
    let linux_info = groups
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["name"] == "linux")
        .unwrap();
    assert_eq!(linux_info["host_count"], 2);
}

#[tokio::test]
async fn the_host_list_is_the_union_of_what_the_members_actually_have() {
    let f = fixture("hosts", 60, None, false);
    let body = get(f.app, "/api/v1/sources/vw-all/hosts").await.json();
    assert_eq!(
        body["hosts"],
        serde_json::json!(["h1.example", "h2.example"])
    );
    assert_eq!(body["total_hosts"], 2);
}

// =========================================================================
// The refusal that must not degrade into empty data
// =========================================================================

#[tokio::test]
async fn a_host_no_member_claims_is_a_404_that_says_so() {
    let f = fixture("unclaimed", 60, None, false);

    let response = get(f.app, "/api/v1/sources/vw-all/dataset?host=nobody.example").await;

    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert!(
        response.body.contains("nobody.example") && response.body.contains("src-a"),
        "the error should name the host and the members: {}",
        response.body
    );
}

// A conditional request must not be able to talk the server out of the refusal.
// The ETag used to be minted, and a 304 answered, before the hosts were routed
// at all — so `If-None-Match: *` turned an unroutable request into "nothing
// changed", which is the same silent nothing the 404 exists to prevent.
#[tokio::test]
async fn a_conditional_request_for_an_unclaimed_host_is_still_a_404() {
    let f = fixture("unclaimed-cond", 60, None, false);

    let response = f
        .app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/sources/vw-all/dataset?host=nobody.example")
                .header("if-none-match", "*")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unmatched_group_filter_is_still_an_empty_result_not_a_404() {
    let f = fixture("ghost-group", 60, None, false);
    let response = get(f.app, "/api/v1/sources/vw-all/dataset?group=ghosts").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["returned"], 0);
}

// =========================================================================
// Refresh delegation
// =========================================================================

#[tokio::test]
async fn a_refresh_is_delegated_to_the_member_that_owns_the_host() {
    let f = fixture("delegate", 60, None, true);

    let response = get(
        f.app,
        "/api/v1/sources/vw-all/dataset?host=h2.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.refreshed.as_deref(), Some("true"));
    assert_eq!(response.refreshed_hosts.as_deref(), Some("h2.example"));

    // The whole claim of the feature: the OWNER gathered, and nobody else did
    assert_eq!(gathers(&f.counter_b), vec!["host:h2.example"]);
    assert!(
        gathers(&f.counter_a).is_empty(),
        "the member that does not own the host must not be asked to gather it"
    );

    // and the read answers with what was just gathered
    assert_eq!(response.json()["hostvars"]["h2.example"]["gathered"], true);
}

// Acceptance: a host that no member has ever gathered must ROUTE, not 404. The
// appliance case — permanently the edge's responsibility, permanently absent
// from its data — and the freshly built VM case are the same case.
#[tokio::test]
async fn a_host_that_was_never_gathered_routes_instead_of_404ing() {
    let f = fixture("never-gathered", 60, None, true);

    let response = get(
        f.app,
        "/api/v1/sources/vw-all/dataset?host=h3.example&refresh=true",
    )
    .await;

    // It reached the owner (counting.py refuses a host it does not know, which
    // is what an unreachable appliance looks like from here)
    assert_eq!(gathers(&f.counter_b), vec!["host:h3.example"]);
    assert!(gathers(&f.counter_a).is_empty());

    // and the read still answers, saying the refresh did not work
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.refreshed.as_deref(), Some("false"));
    assert!(response.refresh_error.is_some());
}

#[tokio::test]
async fn hosts_across_two_members_each_go_to_their_own_owner() {
    let f = fixture("split", 60, None, true);

    let response = get(
        f.app,
        "/api/v1/sources/vw-all/dataset?host=h1.example,h2.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(gathers(&f.counter_a), vec!["host:h1.example"]);
    assert_eq!(gathers(&f.counter_b), vec!["host:h2.example"]);
    let refreshed = response.refreshed_hosts.unwrap();
    assert!(refreshed.contains("h1.example") && refreshed.contains("h2.example"));
}

// The TTL is the refresh GATE, not a freshness label: a view that declares one
// is what decides whether a read pays for a gather.
#[tokio::test]
async fn a_declared_view_ttl_governs_whether_a_read_gathers() {
    // Facts are 300s old. Under the member's own 60s TTL they are stale...
    let stale = fixture("gate-stale", 60, None, true);
    let response = get(
        stale.app,
        "/api/v1/sources/vw-all/dataset?host=h1.example&refresh=true",
    )
    .await;
    assert_eq!(response.refreshed_hosts.as_deref(), Some("h1.example"));
    assert_eq!(gathers(&stale.counter_a), vec!["host:h1.example"]);

    // ...and under a view TTL of 600s the same data is fresh, so nothing is
    // asked of anyone. The view's policy reached the gate.
    let fresh = fixture("gate-fresh", 60, Some(600), true);
    let response = get(
        fresh.app,
        "/api/v1/sources/vw-all/dataset?host=h1.example&refresh=true",
    )
    .await;
    assert_eq!(response.refreshed.as_deref(), Some("true"));
    assert!(response.refreshed_hosts.is_none());
    assert!(
        gathers(&fresh.counter_a).is_empty(),
        "a view TTL that says the data is fresh must not gather"
    );
}

#[tokio::test]
async fn a_member_that_does_not_allow_on_demand_refresh_refuses_and_names_itself() {
    let f = fixture("not-allowed", 60, None, false);

    let response = get(
        f.app,
        "/api/v1/sources/vw-all/dataset?host=h1.example&refresh=true",
    )
    .await;

    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(
        response.body.contains("src-a") && response.body.contains("allow_on_demand_refresh"),
        "the error should name the member and the setting that fixes it: {}",
        response.body
    );
    assert!(gathers(&f.counter_a).is_empty());
}

// =========================================================================
// Status and listing
// =========================================================================

#[tokio::test]
async fn status_reports_each_host_against_its_owners_clock_and_lists_the_members() {
    let f = fixture("status", 60, None, false);

    let body = get(f.app, "/api/v1/sources/vw-all/status").await.json();

    assert_eq!(body["source_id"], "vw-all");
    assert_eq!(body["total_hosts"], 2);
    // seeded 300s old against a 60s TTL
    assert_eq!(body["dataset_age_seconds"], 300);
    assert_eq!(body["dataset_is_fresh"], false);

    let hosts = body["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 2);
    for host in hosts {
        assert_eq!(host["age_seconds"], 300);
        assert_eq!(host["ttl_seconds"], 60);
        assert_eq!(host["is_fresh"], false);
    }

    let members = body["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["source_id"], "src-a");
    assert!(members[0]["cached"].as_bool().unwrap());
    assert!(members[0]["ownership_cached"].as_bool().unwrap());
    // A view never syncs, so it has no health of its own
    assert!(body["sync_health"].is_null());
}

#[tokio::test]
async fn a_declared_view_ttl_is_what_status_reports() {
    let f = fixture("status-ttl", 60, Some(600), false);
    let body = get(f.app, "/api/v1/sources/vw-all/status").await.json();

    assert_eq!(body["ttl_seconds"], 600);
    for host in body["hosts"].as_array().unwrap() {
        assert_eq!(host["ttl_seconds"], 600);
        // 300s old under a 600s policy
        assert_eq!(host["is_fresh"], true);
    }
}

#[tokio::test]
async fn the_view_is_listed_alongside_the_sources_and_says_it_is_one() {
    let f = fixture("list", 60, None, false);
    let body = get(f.app, "/api/v1/sources").await.json();

    let entries = body.as_array().unwrap();
    let view = entries
        .iter()
        .find(|e| e["source_id"] == "vw-all")
        .expect("the view should be listed");
    assert_eq!(view["kind"], "view");
    assert_eq!(view["total_hosts"], 2);

    let source = entries.iter().find(|e| e["source_id"] == "src-a").unwrap();
    assert_eq!(source["kind"], "source");
}

// =========================================================================
// A view is read-only
// =========================================================================

#[tokio::test]
async fn sync_on_a_view_is_refused_and_says_why() {
    let f = fixture("no-sync", 60, None, true);

    let response = request(f.app, "POST", "/api/v1/sources/vw-all/sync", None).await;

    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("is a view")
            && response.body.contains("src-a")
            && response.body.contains("src-b"),
        "the refusal should explain itself and name the members: {}",
        response.body
    );
    // and above all, it did not quietly re-gather two datacenters
    assert!(gathers(&f.counter_a).is_empty());
    assert!(gathers(&f.counter_b).is_empty());
}

#[tokio::test]
async fn evicting_a_view_is_refused() {
    let f = fixture("no-evict", 60, None, false);
    let response = request(f.app.clone(), "DELETE", "/api/v1/sources/vw-all", None).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    // the members' entries are untouched
    let hosts = get(f.app, "/api/v1/sources/vw-all/hosts").await.json();
    assert_eq!(hosts["total_hosts"], 2);
}

#[tokio::test]
async fn deleting_a_host_through_a_view_is_refused() {
    let f = fixture("no-host-write", 60, None, false);
    let response = request(
        f.app,
        "DELETE",
        "/api/v1/sources/vw-all/hosts/h1.example",
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("is a view"), "{}", response.body);
}

// =========================================================================
// Auth: the view is the contract, the members are internal topology
// =========================================================================

#[tokio::test]
async fn a_key_granted_only_the_view_can_read_it_and_not_its_members() {
    let counter_a = counter_path("auth-a");
    let counter_b = counter_path("auth-b");
    let sources = HashMap::from([
        (
            "src-inventory".to_string(),
            member_source("/dev/null", 7200, false),
        ),
        ("src-a".to_string(), member_source(&counter_a, 60, false)),
        ("src-b".to_string(), member_source(&counter_b, 60, false)),
    ]);
    let view: View = serde_yaml_ng::from_str(VIEW_YAML).unwrap();

    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .views(HashMap::from([("vw-all".to_string(), view)]))
        .api_keys(vec![ResolvedApiKey {
            name: "forms".to_string(),
            secret: "view-only".to_string(),
            permissions: Permissions::Scoped {
                sources: ["vw-all".to_string()].into_iter().collect(),
                endpoints: Default::default(),
            },
        }])
        .build_with_state();

    state.cache.set("src-inventory", inventory());
    state
        .cache
        .set("src-a", facts("h1.example", "OracleLinux", 300, 60));
    state
        .cache
        .set("src-b", facts("h2.example", "RHEL", 300, 60));

    let allowed = request(
        app.clone(),
        "GET",
        "/api/v1/sources/vw-all/dataset?host=h1.example",
        Some("view-only"),
    )
    .await;
    assert_eq!(allowed.status, StatusCode::OK);
    assert_eq!(
        allowed.json()["hostvars"]["h1.example"]["os"],
        "OracleLinux"
    );

    let refused = request(
        app.clone(),
        "GET",
        "/api/v1/sources/src-a/dataset",
        Some("view-only"),
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);

    // and the listing shows the view alone, not the members behind it
    let listed = request(app, "GET", "/api/v1/sources", Some("view-only")).await;
    let listed = listed.json();
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["source_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["vw-all"]);
}

// =========================================================================
// Observability: a view is the address consumers are given, so it has to be
// the address an operator can alert on
// =========================================================================

// The Prometheus recorder is a process global shared by every test in this
// binary, so a gauge keyed by view id is only assertable if the id is unique to
// the test. `vw-all` is used by every other fixture here; these build their own.
fn metrics_app(view_id: &str, name: &str, with_inventory: bool) -> axum::Router {
    let sources = HashMap::from([
        (
            "src-inventory".to_string(),
            member_source("/dev/null", 7200, false),
        ),
        (
            "src-a".to_string(),
            member_source(&counter_path(&format!("{}-a", name)), 60, false),
        ),
        (
            "src-b".to_string(),
            member_source(&counter_path(&format!("{}-b", name)), 60, false),
        ),
    ]);

    let view: View = serde_yaml_ng::from_str(VIEW_YAML).unwrap();
    let (app, state) = unified_api::AppBuilder::new()
        .sources(sources)
        .views(HashMap::from([(view_id.to_string(), view)]))
        .build_with_state();

    if with_inventory {
        state.cache.set("src-inventory", inventory());
    }
    state
        .cache
        .set("src-a", facts("h1.example", "OracleLinux", 300, 60));
    state
        .cache
        .set("src-b", facts("h2.example", "RHEL", 300, 60));

    app
}

// A view holds no cache entry, so it appears in neither `cache.keys()` nor
// `sources` — and had no metric series at all. Every member could be healthy
// while the view served nothing, and no gauge would say so.
#[tokio::test]
async fn a_view_reports_its_own_gauges() {
    let app = metrics_app("vw-metrics-healthy", "metrics-healthy", true);
    let response = get(app, "/metrics").await;
    assert_eq!(response.status, StatusCode::OK);

    for series in [
        "unified_api_view_fresh",
        "unified_api_view_age_seconds",
        "unified_api_view_ttl_seconds",
        "unified_api_view_hosts",
        "unified_api_view_members_total",
        "unified_api_view_members_cached",
        "unified_api_view_members_routable",
    ] {
        assert!(
            response
                .body
                .contains(&format!("{}{{view=\"vw-metrics-healthy\"}}", series)),
            "missing series {}; body was:\n{}",
            series,
            response.body
        );
    }

    for expected in [
        "unified_api_view_members_total{view=\"vw-metrics-healthy\"} 2",
        "unified_api_view_members_cached{view=\"vw-metrics-healthy\"} 2",
        "unified_api_view_members_routable{view=\"vw-metrics-healthy\"} 2",
        "unified_api_view_hosts{view=\"vw-metrics-healthy\"} 2",
    ] {
        assert!(
            response.body.contains(expected),
            "expected {}; body was:\n{}",
            expected,
            response.body
        );
    }
}

// The state no other gauge can show: both members hold data and look healthy,
// but the inventory their ownership resolves against has never synced — so
// nothing is claimed and the view serves an empty inventory.
#[tokio::test]
async fn a_view_that_cannot_route_is_visible_in_the_gauges() {
    let app = metrics_app("vw-metrics-unroutable", "metrics-unroutable", false);
    let response = get(app, "/metrics").await;

    for expected in [
        // both members have their own data...
        "unified_api_view_members_cached{view=\"vw-metrics-unroutable\"} 2",
        // ...but neither can expand its ownership patterns...
        "unified_api_view_members_routable{view=\"vw-metrics-unroutable\"} 0",
        // ...so the view serves nothing
        "unified_api_view_hosts{view=\"vw-metrics-unroutable\"} 0",
    ] {
        assert!(
            response.body.contains(expected),
            "expected {}; body was:\n{}",
            expected,
            response.body
        );
    }
}
