// Integration tests for the git CLI adapter. They create real repositories
// in temp directories and talk to them over the file:// protocol — no
// network, but the same code paths as an https remote (minus auth).
use std::collections::HashMap;
use std::path::Path;

use unified_api::adapters::out::git::cli::CliGit;
use unified_api::domain::project::GitProject;
use unified_api::ports::git::GitPort;

async fn run(dir: &Path, args: &[&str]) {
    let status = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // Identity for the test commits — independent of the host's gitconfig
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .await
        .expect("failed to run git");
    assert!(status.success(), "git {:?} failed", args);
}

// Creates an origin repository with one committed script and returns
// (tempdir guard, file:// url)
async fn make_origin() -> (tempfile::TempDir, String) {
    let origin = tempfile::tempdir().unwrap();
    run(origin.path(), &["init", "--initial-branch", "main"]).await;
    tokio::fs::write(origin.path().join("fetch.py"), "print('v1')\n")
        .await
        .unwrap();
    run(origin.path(), &["add", "."]).await;
    run(origin.path(), &["commit", "-m", "v1"]).await;
    let url = format!("file://{}", origin.path().display());
    (origin, url)
}

fn project(url: &str) -> GitProject {
    // Building the domain type from YAML keeps the test honest about what
    // the config file actually looks like.
    serde_yaml_ng::from_str(&format!("name: Test\ngit_url: \"{}\"\n", url)).unwrap()
}

#[tokio::test]
async fn ensure_clones_a_fresh_checkout() {
    let (_origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .expect("clone failed");

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('v1')\n");
}

#[tokio::test]
async fn ensure_updates_an_existing_checkout() {
    let (origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .expect("clone failed");

    // The origin moves forward...
    tokio::fs::write(origin.path().join("fetch.py"), "print('v2')\n")
        .await
        .unwrap();
    run(origin.path(), &["commit", "-am", "v2"]).await;

    // ...and a second ensure() brings the checkout to the new tip
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .expect("update failed");

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('v2')\n");
}

#[tokio::test]
async fn ensure_discards_local_modifications() {
    let (_origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .unwrap();

    // Someone (or something) edits the checkout in place — the next ensure
    // must restore the repository state, not carry the drift forward
    tokio::fs::write(checkout.join("fetch.py"), "tampered\n")
        .await
        .unwrap();

    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .unwrap();

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('v1')\n");
}

#[tokio::test]
async fn ensure_fails_cleanly_on_bad_url() {
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    let err = git
        .ensure(
            &checkout,
            &project("file:///nonexistent/repo.git"),
            &HashMap::new(),
        )
        .await
        .expect_err("expected clone to fail");
    assert!(err.message.contains("git clone"));
}

// =========================================================================
// HTTP routes: on-demand project sync (admin-only)
// =========================================================================

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};

fn keys() -> Vec<ResolvedApiKey> {
    vec![
        ResolvedApiKey {
            name: "admin".to_string(),
            secret: "adm".to_string(),
            permissions: Permissions::Admin,
        },
        ResolvedApiKey {
            name: "limited".to_string(),
            secret: "lim".to_string(),
            permissions: Permissions::Scoped {
                sources: std::collections::HashSet::new(),
                endpoints: std::collections::HashSet::new(),
            },
        },
    ]
}

async fn request(app: axum::Router, method: &str, path: &str, key: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("x-api-key", key)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn post_project_sync_clones_on_demand() {
    let (_origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();

    let projects = [("prj-test".to_string(), project(&url))]
        .into_iter()
        .collect();
    let app = unified_api::AppBuilder::new()
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    // No checkout yet
    let (status, body) = request(app.clone(), "GET", "/api/v1/projects", "adm").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"checkout_present\":false"));

    // On-demand sync clones it
    let (status, body) =
        request(app.clone(), "POST", "/api/v1/projects/prj-test/sync", "adm").await;
    assert_eq!(status, StatusCode::OK, "body: {}", body);
    assert!(body.contains("\"success\":true"));
    assert!(workdir.path().join("prj-test/fetch.py").exists());

    let (_, body) = request(app, "GET", "/api/v1/projects", "adm").await;
    assert!(body.contains("\"checkout_present\":true"));
}

#[tokio::test]
async fn project_routes_are_admin_only() {
    let workdir = tempfile::tempdir().unwrap();
    let projects = [("prj-test".to_string(), project("file:///nowhere/repo.git"))]
        .into_iter()
        .collect();
    let app = unified_api::AppBuilder::new()
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    let (status, _) = request(app.clone(), "GET", "/api/v1/projects", "lim").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(app, "POST", "/api/v1/projects/prj-test/sync", "lim").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn project_sync_unknown_id_is_404_and_bad_repo_is_502() {
    let workdir = tempfile::tempdir().unwrap();
    let projects = [("prj-test".to_string(), project("file:///nowhere/repo.git"))]
        .into_iter()
        .collect();
    let app = unified_api::AppBuilder::new()
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    let (status, _) = request(
        app.clone(),
        "POST",
        "/api/v1/projects/prj-ghost/sync",
        "adm",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Configured but uncloneable: the failure is the upstream's, hence 502
    let (status, body) = request(app, "POST", "/api/v1/projects/prj-test/sync", "adm").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("\"success\":false"));
}
