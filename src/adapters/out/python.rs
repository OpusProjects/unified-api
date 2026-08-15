use std::path::{Path, PathBuf};

use tokio::process::Command;
use tracing::{debug, info};

use crate::ports::venv::{VENVS_DIR, VenvError, VenvFuture, VenvOutcome, VenvPort};

// VenvPort implementation shelling out to `python3 -m venv` and the venv's
// own pip — the same philosophy as the git and process adapters: the tools an
// operator would debug with are the tools the app uses.
//
// Idempotence comes from a marker file: the installed requirements.txt is
// copied into the venv after a successful install, and a later run whose
// requirements match the marker does nothing. That is what makes it safe to
// call on EVERY project sync — an unchanged project costs two file reads, not
// a pip run per pull.
pub struct PyVenv;

impl Default for PyVenv {
    fn default() -> Self {
        Self::new()
    }
}

impl PyVenv {
    pub fn new() -> Self {
        Self
    }
}

fn marker_path(venv: &Path) -> PathBuf {
    venv.join(".requirements.installed")
}

async fn run(mut cmd: Command, what: &str) -> Result<(), VenvError> {
    // Killed if this future is dropped — which is what the caller's timeout
    // does. A wedged pip must not keep installing behind our back.
    cmd.kill_on_drop(true);
    let output = cmd.output().await.map_err(|e| VenvError {
        message: format!("failed to run {}: {}", what, e),
    })?;
    if !output.status.success() {
        return Err(VenvError {
            message: format!(
                "{} failed ({}): {}",
                what,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }
    Ok(())
}

impl VenvPort for PyVenv {
    fn ensure(&self, projects_dir: &Path, project_id: &str) -> VenvFuture<'_> {
        let checkout = projects_dir.join(project_id);
        let venv = projects_dir.join(VENVS_DIR).join(project_id);
        let project_id = project_id.to_string();

        Box::pin(async move {
            let requirements = checkout.join("requirements.txt");
            let wanted = match tokio::fs::read_to_string(&requirements).await {
                Ok(contents) => contents,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    debug!(project = %project_id, "No requirements.txt, no venv to build");
                    return Ok(VenvOutcome::NoRequirements);
                }
                Err(e) => {
                    return Err(VenvError {
                        message: format!("read '{}': {}", requirements.display(), e),
                    });
                }
            };

            // The marker holds the requirements that are actually installed;
            // matching content means pip already did this exact job
            if let Ok(installed) = tokio::fs::read_to_string(marker_path(&venv)).await
                && installed == wanted
            {
                debug!(project = %project_id, "requirements.txt unchanged, venv kept");
                return Ok(VenvOutcome::Unchanged);
            }

            let python = venv.join("bin").join("python3");
            if !tokio::fs::try_exists(&python).await.unwrap_or(false) {
                info!(project = %project_id, venv = %venv.display(), "Creating virtualenv");
                let mut cmd = Command::new("python3");
                cmd.arg("-m").arg("venv").arg(&venv);
                run(cmd, "python3 -m venv").await?;
            }

            info!(project = %project_id, "Installing requirements into the virtualenv");
            let mut cmd = Command::new(venv.join("bin").join("pip"));
            cmd.arg("install")
                .arg("--disable-pip-version-check")
                .arg("-r")
                .arg(&requirements);
            run(cmd, "pip install -r requirements.txt").await?;

            // Written only after a SUCCESSFUL install: a failed pip leaves no
            // marker, so the next sync tries again instead of believing it
            tokio::fs::write(marker_path(&venv), &wanted)
                .await
                .map_err(|e| VenvError {
                    message: format!("write venv marker: {}", e),
                })?;

            Ok(VenvOutcome::Installed)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An empty requirements.txt makes pip a no-op, so these tests run a REAL
    // `python3 -m venv` + pip without touching the network.
    async fn checkout_with_requirements(projects_dir: &Path, project_id: &str, requirements: &str) {
        let checkout = projects_dir.join(project_id);
        tokio::fs::create_dir_all(&checkout).await.unwrap();
        tokio::fs::write(checkout.join("requirements.txt"), requirements)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_checkout_without_requirements_builds_nothing() {
        let projects_dir = tempfile::tempdir().unwrap();
        tokio::fs::create_dir_all(projects_dir.path().join("prj-a"))
            .await
            .unwrap();

        let outcome = PyVenv::new()
            .ensure(projects_dir.path(), "prj-a")
            .await
            .expect("no requirements is not an error");

        assert_eq!(outcome, VenvOutcome::NoRequirements);
        assert!(!projects_dir.path().join(VENVS_DIR).exists());
    }

    #[tokio::test]
    async fn requirements_build_a_venv_and_a_second_sync_reuses_it() {
        let projects_dir = tempfile::tempdir().unwrap();
        checkout_with_requirements(projects_dir.path(), "prj-a", "# nothing yet\n").await;

        let venv = PyVenv::new();
        let outcome = venv
            .ensure(projects_dir.path(), "prj-a")
            .await
            .expect("venv builds");
        assert_eq!(outcome, VenvOutcome::Installed);
        assert!(
            projects_dir
                .path()
                .join(VENVS_DIR)
                .join("prj-a/bin/python3")
                .exists(),
            "the venv interpreter must exist where execution will look for it"
        );

        // Unchanged requirements: the every-sync call must cost file reads,
        // not a pip run
        let outcome = venv
            .ensure(projects_dir.path(), "prj-a")
            .await
            .expect("second call succeeds");
        assert_eq!(outcome, VenvOutcome::Unchanged);
    }

    #[tokio::test]
    async fn changed_requirements_reinstall() {
        let projects_dir = tempfile::tempdir().unwrap();
        checkout_with_requirements(projects_dir.path(), "prj-a", "# v1\n").await;

        let venv = PyVenv::new();
        venv.ensure(projects_dir.path(), "prj-a")
            .await
            .expect("first install");

        checkout_with_requirements(projects_dir.path(), "prj-a", "# v2\n").await;
        let outcome = venv
            .ensure(projects_dir.path(), "prj-a")
            .await
            .expect("reinstall");
        assert_eq!(
            outcome,
            VenvOutcome::Installed,
            "a changed requirements.txt must reinstall, not be believed"
        );
    }
}
