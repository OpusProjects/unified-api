use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;

use crate::domain::project::GitProject;
use crate::domain::sync_health::{RunRecord, RunTrigger, SyncHealthRegistry};
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;
use crate::ports::venv::{VenvOutcome, VenvPort};

// The use case "bring a project checkout up to date": resolve its credential
// (if any), let the GitPort clone or update the directory, and — for projects
// that opted in — bring the Python virtualenv in step with the checkout's
// requirements.txt. Shared by the boot sequence in main and the periodic
// scheduler task, like sync/enrich.
//
// Health is recorded here, at the one place every project sync passes through,
// for the same reason sync_source records sync health: a checkout stuck on a
// stale commit because every pull fails used to be a log line per interval and
// nothing an operator could query or alert on. A broken requirements.txt is
// the same kind of invisible without this: the scripts would fail at import
// time, one confusing sync error per source, with the actual cause nowhere.
// What started a pull and who asked, for the project's history.
#[derive(Debug, Clone)]
pub struct RunOrigin {
    pub trigger: RunTrigger,
    // The API key's name or the signed-in user, for a manual pull.
    pub by: Option<String>,
}

impl RunOrigin {
    pub fn of(trigger: RunTrigger) -> Self {
        Self { trigger, by: None }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn sync_project(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    venv: &dyn VenvPort,
    health: &SyncHealthRegistry,
    project_id: &str,
    project: &GitProject,
    projects_dir: &Path,
    origin: RunOrigin,
) -> Result<(), String> {
    let dir = projects_dir.join(project_id);
    // One pull of a checkout at a time. A manual Pull can land on top of a
    // scheduled one (or the clone of a reload), and without this both read
    // `before` at the same commit, so each recorded the whole move — the
    // history claimed a change the other pull made. Held across the before
    // read, the pull and the after read, so each row's commits are its own;
    // the second pull simply runs after the first, which is also what the
    // working tree wants (two git processes in one directory fight over
    // index.lock). Taken before the clock starts: the wait is not the pull.
    let checkout = checkout_lock(&dir);
    let _checkout = checkout.lock().await;
    let started_at = crate::domain::unix_now();
    let start = std::time::Instant::now();
    // The commit before and after: what the pull moved the checkout over,
    // which is when desired state or a script changed.
    let before = git.revision(&dir).await;
    let result = run(git, secrets, venv, project_id, project, projects_dir).await;
    let after = git.revision(&dir).await;

    match &result {
        Ok(()) => health.record_success(project_id),
        Err(error) => health.record_failure(project_id, error),
    }
    health.record_run(
        project_id,
        RunRecord {
            started_at,
            duration_ms: start.elapsed().as_millis() as u64,
            trigger: origin.trigger,
            by: origin.by,
            scope: None,
            error: result.as_ref().err().cloned(),
            hosts: None,
            groups: None,
            revision_before: before,
            revision: after,
        },
    );

    result
}

// The lock for one checkout directory, created on first use.
//
// A process-wide map rather than a field on AppState because the pulls that
// must serialise do not all have an AppState: the boot clones in main run
// before it exists. Keyed by the checkout directory — the thing actually
// shared — so two apps a test builds over different directories never wait
// on each other. Entries are never removed: one small mutex per project ever
// configured.
fn checkout_lock(dir: &Path) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<DashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>> = OnceLock::new();
    LOCKS
        .get_or_init(DashMap::new)
        .entry(dir.to_path_buf())
        .or_default()
        .clone()
}

async fn run(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    venv: &dyn VenvPort,
    project_id: &str,
    project: &GitProject,
    projects_dir: &Path,
) -> Result<(), String> {
    let credentials = match project.credential_id() {
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
        git.ensure(&dir, &project.remote(), &credentials),
    )
    .await
    {
        Ok(result) => result.map_err(|e| e.message),
        Err(_elapsed) => Err(format!(
            "git operation timed out after {}s",
            project.timeout_seconds
        )),
    }?;

    if !project.python_venv {
        return Ok(());
    }

    // Its own timeout budget rather than sharing git's: each step is bounded
    // by the same number, and a slow clone must not eat the time a first
    // `pip install` legitimately needs. Dropping on timeout kills the child
    // (kill_on_drop in the adapter), same contract as git.
    match tokio::time::timeout(
        std::time::Duration::from_secs(project.timeout_seconds),
        venv.ensure(projects_dir, project_id),
    )
    .await
    {
        Ok(Ok(outcome)) => {
            if outcome == VenvOutcome::Installed {
                tracing::info!(project = %project_id, "Virtualenv updated");
            }
            Ok(())
        }
        Ok(Err(e)) => Err(format!("venv: {}", e.message)),
        Err(_elapsed) => Err(format!(
            "venv install timed out after {}s",
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

    use crate::ports::venv::{VenvError, VenvFuture};

    // A VenvPort that counts its calls and answers what it is told to.
    #[derive(Default)]
    struct StubVenv {
        calls: std::sync::atomic::AtomicUsize,
        fail_with: Option<String>,
    }

    impl VenvPort for StubVenv {
        fn ensure(&self, _projects_dir: &Path, _project_id: &str) -> VenvFuture<'_> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let fail_with = self.fail_with.clone();
            Box::pin(async move {
                match fail_with {
                    Some(message) => Err(VenvError { message }),
                    None => Ok(VenvOutcome::Installed),
                }
            })
        }
    }

    // A GitPort that answers what it is told to, so the test controls the
    // outcome without a git binary or a network.
    struct StubGit {
        fail_with: Option<String>,
    }

    impl GitPort for StubGit {
        fn ensure(
            &self,
            _dir: &Path,
            _remote: &git_rs::Remote,
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
            _remote: &git_rs::Remote,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    fn project() -> GitProject {
        serde_saphyr::from_str("name: Test\ngit_url: \"https://example.com/repo.git\"\n")
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
        sync_project(
            &failing,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project(),
            &dir,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect_err("the stub fails");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert_eq!(recorded.last_error.as_deref(), Some("remote unreachable"));
        assert_eq!(recorded.consecutive_failures, 1);

        let working = StubGit { fail_with: None };
        sync_project(
            &working,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project(),
            &dir,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect("the stub succeeds");

        let recorded = health.get("prj-a").unwrap();
        assert_eq!(recorded.consecutive_failures, 0);
        assert_eq!(recorded.last_error, None);
    }

    // A checkout whose commit moves on each pull: "aaa" before, "bbb" after.
    struct MovingGit {
        at: std::sync::Mutex<&'static str>,
    }

    impl GitPort for MovingGit {
        fn ensure(
            &self,
            _dir: &Path,
            _remote: &git_rs::Remote,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            *self.at.lock().unwrap() = "bbb";
            Box::pin(async { Ok(()) })
        }

        fn revision(&self, _dir: &Path) -> crate::ports::git::RevisionFuture<'_> {
            let at = self.at.lock().unwrap().to_string();
            Box::pin(async move { Some(at) })
        }
    }

    // Each pull is a row in the project's history: why and by whom, and the
    // commit it moved the checkout from and to.
    #[tokio::test]
    async fn a_pull_is_kept_in_the_history_with_its_commits() {
        let health = SyncHealthRegistry::new();
        let git = MovingGit {
            at: std::sync::Mutex::new("aaa"),
        };
        sync_project(
            &git,
            &MockSecrets::new(),
            &StubVenv::default(),
            &health,
            "prj-a",
            &project(),
            &std::path::PathBuf::from("unused"),
            RunOrigin {
                trigger: RunTrigger::Manual,
                by: Some("key-ops".into()),
            },
        )
        .await
        .expect("the stub succeeds");

        let history = health.history("prj-a");
        assert_eq!(history.len(), 1);
        let run = &history[0];
        assert_eq!(run.trigger, RunTrigger::Manual);
        assert_eq!(run.by.as_deref(), Some("key-ops"));
        assert_eq!(run.revision_before.as_deref(), Some("aaa"));
        assert_eq!(run.revision.as_deref(), Some("bbb"));
        assert_eq!(run.error, None);
        assert!(run.started_at > 0);
    }

    // A slow pull that moves the checkout one commit: c0 → c1 → c2. The
    // commit changes at the END of the pull, after a pause, so two pulls
    // that overlapped would both read c0 as their `before`.
    struct SlowMovingGit {
        at: Arc<std::sync::Mutex<u32>>,
    }

    impl GitPort for SlowMovingGit {
        fn ensure(
            &self,
            _dir: &Path,
            _remote: &git_rs::Remote,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            let at = Arc::clone(&self.at);
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                *at.lock().unwrap() += 1;
                Ok(())
            })
        }

        fn revision(&self, _dir: &Path) -> crate::ports::git::RevisionFuture<'_> {
            let at = format!("c{}", self.at.lock().unwrap());
            Box::pin(async move { Some(at) })
        }
    }

    // Two pulls of one project at once (a manual Pull over a scheduled one):
    // each row records the move IT made, never the other's.
    #[tokio::test(start_paused = true)]
    async fn overlapping_pulls_of_one_project_record_their_own_commits() {
        let health = SyncHealthRegistry::new();
        let git = SlowMovingGit {
            at: Arc::new(std::sync::Mutex::new(0)),
        };
        let dir = std::path::PathBuf::from("unused-overlapping");
        let (secrets, venv, project) = (MockSecrets::new(), StubVenv::default(), project());
        let pull = |trigger| {
            sync_project(
                &git,
                &secrets,
                &venv,
                &health,
                "prj-overlap",
                &project,
                &dir,
                RunOrigin::of(trigger),
            )
        };
        let (first, second) = tokio::join!(pull(RunTrigger::Scheduled), pull(RunTrigger::Manual));
        first.expect("the stub succeeds");
        second.expect("the stub succeeds");

        let history = health.history("prj-overlap");
        let moves: Vec<(Option<&str>, Option<&str>)> = history
            .iter()
            .map(|run| (run.revision_before.as_deref(), run.revision.as_deref()))
            .collect();
        assert_eq!(
            moves,
            [(Some("c1"), Some("c2")), (Some("c0"), Some("c1"))],
            "newest first"
        );
    }

    // The unreachable remote used to hang this future forever — and boot, the
    // scheduler task or the HTTP request along with it.
    #[tokio::test(start_paused = true)]
    async fn a_hung_git_remote_times_out_and_records_the_failure() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();

        let project: GitProject = serde_saphyr::from_str(
            "name: Test\ngit_url: \"https://example.com/repo.git\"\ntimeout_seconds: 30\n",
        )
        .expect("project fixture");

        sync_project(
            &HungGit,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
            RunOrigin::of(RunTrigger::Scheduled),
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

        let project: GitProject = serde_saphyr::from_str(
            "name: Test\ngit_url: \"https://example.com/repo.git\"\ncredential: \"cred-ghost\"\n",
        )
        .expect("project fixture");

        sync_project(
            &git,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
            RunOrigin::of(RunTrigger::Scheduled),
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

    #[tokio::test]
    async fn a_project_without_python_venv_never_builds_one() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let venv = StubVenv::default();

        sync_project(
            &StubGit { fail_with: None },
            &secrets,
            &venv,
            &health,
            "prj-a",
            &project(),
            &std::path::PathBuf::from("unused"),
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect("git succeeds");

        assert_eq!(
            venv.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "python_venv defaults to off: no venv work without the opt-in"
        );
    }

    // A broken requirements.txt fails the PROJECT sync, visibly, instead of
    // every script failing at import time with the cause nowhere.
    #[tokio::test]
    async fn a_failing_venv_install_fails_the_sync_and_records_it() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let venv = StubVenv {
            fail_with: Some("pip could not find torch==999".to_string()),
            ..Default::default()
        };
        let project: GitProject = serde_saphyr::from_str(
            "name: Test\ngit_url: \"https://example.com/repo.git\"\npython_venv: true\n",
        )
        .expect("project fixture");

        sync_project(
            &StubGit { fail_with: None },
            &secrets,
            &venv,
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect_err("the venv step fails");

        assert_eq!(venv.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let recorded = health.get("prj-a").expect("failure recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("venv") && e.contains("torch==999")),
            "error was: {:?}",
            recorded.last_error
        );
    }
}
