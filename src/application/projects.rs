use std::path::Path;

use crate::domain::project::GitProject;
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;

// The use case "bring a project checkout up to date": resolve its credential
// (if any) and let the GitPort clone or update the directory. Shared by the
// boot sequence in main and the periodic scheduler task, like sync/enrich.
pub async fn sync_project(
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
    git.ensure(&dir, project, &credentials)
        .await
        .map_err(|e| e.message)
}
