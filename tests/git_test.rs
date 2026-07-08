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
