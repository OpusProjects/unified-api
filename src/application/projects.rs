use std::path::Path;

use crate::domain::project::GitProject;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;

// The use case "bring a project checkout up to date": resolve its credential
// (if any) and let the GitPort clone or update the directory. Shared by the
// boot sequence in main and the periodic scheduler task, like sync/enrich.
//
// Health is recorded here, at the one place every project sync passes through,
// for the same reason sync_source records sync health: a checkout stuck on a
// stale commit because every pull fails used to be a log line per interval and
// nothing an operator could query or alert on.
pub async fn sync_project(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    health: &SyncHealthRegistry,
    project_id: &str,
    project: &GitProject,
    projects_dir: &Path,
) -> Result<(), String> {
    let result = run(git, secrets, project_id, project, projects_dir).await;

    match &result {
        Ok(()) => health.record_success(project_id),
        Err(error) => health.record_failure(project_id, error),
    }

    result
}

async fn run(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    project_id: &str,
    project: &GitProject,
    projects_dir: &Path,
) -> Result<(), String> {
    let credentials = match &project.credential_id {
        Some(credential_id) => secrets
            .resolve(credential_id)
            .await
            .map_err(|e| format!("credential '{}': {}", credential_id, e.message))?,
        None => Default::default(),
    };

    let dir = projects_dir.join(project_id);
    // Bounded like every connector/enricher/output run: a git remote that
    // never answers used to hang this future — and with it whatever awaited
    // the sync. Dropping the future on timeout kills the git child
    // (kill_on_drop in the adapter), so nothing keeps running behind our back.
    match tokio::time::timeout(
        std::time::Duration::from_secs(project.timeout_seconds),
        git.ensure(&dir, project, &credentials),
    )
    .await
    {
        Ok(result) => result.map_err(|e| e.message),
        Err(_elapsed) => Err(format!(
            "git operation timed out after {}s",
            project.timeout_seconds
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::secrets::mock::MockSecrets;
    use crate::ports::git::{GitError, GitFuture};
    use std::collections::HashMap;

    // A GitPort that answers what it is told to, so the test controls the
    // outcome without a git binary or a network.
    struct StubGit {
        fail_with: Option<String>,
    }

    impl GitPort for StubGit {
        fn ensure(
            &self,
            _dir: &Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            let fail_with = self.fail_with.clone();
            Box::pin(async move {
                match fail_with {
                    Some(message) => Err(GitError { message }),
                    None => Ok(()),
                }
            })
        }
    }

    // A GitPort that never answers — the unreachable remote.
    struct HungGit;

    impl GitPort for HungGit {
        fn ensure(
            &self,
            _dir: &Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    fn project() -> GitProject {
        serde_yaml_ng::from_str("name: Test\ngit_url: \"https://example.com/repo.git\"\n")
            .expect("project fixture")
    }

    #[tokio::test]
    async fn a_failed_pull_is_recorded_and_a_later_success_clears_it() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let dir = std::path::PathBuf::from("unused");

        let failing = StubGit {
            fail_with: Some("remote unreachable".to_string()),
        };
        sync_project(&failing, &secrets, &health, "prj-a", &project(), &dir)
            .await
            .expect_err("the stub fails");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert_eq!(recorded.last_error.as_deref(), Some("remote unreachable"));
        assert_eq!(recorded.consecutive_failures, 1);

        let working = StubGit { fail_with: None };
        sync_project(&working, &secrets, &health, "prj-a", &project(), &dir)
            .await
            .expect("the stub succeeds");

        let recorded = health.get("prj-a").unwrap();
        assert_eq!(recorded.consecutive_failures, 0);
        assert_eq!(recorded.last_error, None);
    }

    // The unreachable remote used to hang this future forever — and boot, the
    // scheduler task or the HTTP request along with it.
    #[tokio::test(start_paused = true)]
    async fn a_hung_git_remote_times_out_and_records_the_failure() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();

        let project: GitProject = serde_yaml_ng::from_str(
            "name: Test\ngit_url: \"https://example.com/repo.git\"\ntimeout_seconds: 30\n",
        )
        .expect("project fixture");

        sync_project(
            &HungGit,
            &secrets,
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
        )
        .await
        .expect_err("the hung remote must time out");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("timed out after 30s")),
            "error was: {:?}",
            recorded.last_error
        );
    }

    #[tokio::test]
    async fn an_unresolvable_credential_is_a_recorded_failure_too() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let git = StubGit { fail_with: None };

        let project: GitProject = serde_yaml_ng::from_str(
            "name: Test\ngit_url: \"https://example.com/repo.git\"\ncredential_id: \"cred-ghost\"\n",
        )
        .expect("project fixture");

        sync_project(
            &git,
            &secrets,
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
        )
        .await
        .expect_err("the credential does not resolve");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("cred-ghost"))
        );
    }
}
