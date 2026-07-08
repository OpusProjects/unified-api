use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::application::projects::sync_project;

// Operational routes for git project checkouts. Admin-only: this is deploy
// tooling (a pipeline pushes new connector scripts, then calls the sync
// endpoint), not consumer data access.

#[derive(Serialize, ToSchema)]
pub struct ProjectInfo {
    pub project_id: String,
    pub name: String,
    pub git_url: String,
    pub branch: String,
    /// Whether a checkout currently exists on disk
    pub checkout_present: bool,
    /// Seconds between periodic re-pulls (absent/0 = only boot and on demand)
    pub sync_interval_seconds: Option<u64>,
    pub sync_on_boot: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/projects",
    tag = "Projects",
    responses(
        (status = 200, description = "Configured git projects and their checkout state", body = Vec<ProjectInfo>),
        (status = 403, description = "API key is not admin")
    )
)]
pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<ProjectInfo>>, StatusCode> {
    if !auth.permissions.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }

    let mut projects: Vec<ProjectInfo> = state
        .projects
        .iter()
        .map(|(id, project)| ProjectInfo {
            project_id: id.clone(),
            name: project.name.clone(),
            git_url: project.git_url.clone(),
            branch: project.branch.clone(),
            checkout_present: state.projects_dir.join(id).join(".git").exists(),
            sync_interval_seconds: project.sync_interval_seconds,
            sync_on_boot: project.sync_on_boot,
        })
        .collect();

    projects.sort_by(|a, b| a.project_id.cmp(&b.project_id));
    Ok(Json(projects))
}

#[derive(Serialize, ToSchema)]
pub struct ProjectSyncResult {
    pub project_id: String,
    pub success: bool,
    pub duration_ms: u128,
    pub error: Option<String>,
}

#[utoipa::path(
    post,
    path = "/api/v1/projects/{id}/sync",
    tag = "Projects",
    params(
        ("id" = String, Path, description = "Project identifier (e.g. prj-connectors-infra)")
    ),
    responses(
        (status = 200, description = "Checkout updated to the branch tip", body = ProjectSyncResult),
        (status = 403, description = "API key is not admin"),
        (status = 404, description = "Project not configured"),
        (status = 502, description = "git clone/fetch failed", body = ProjectSyncResult)
    )
)]
pub async fn sync_project_now(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<ProjectSyncResult>), StatusCode> {
    if !auth.permissions.is_admin() {
        return Err(StatusCode::FORBIDDEN);
    }
    let project = state.projects.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    let start = Instant::now();
    let result = sync_project(
        &*state.git,
        &*state.secrets,
        &id,
        project,
        &state.projects_dir,
    )
    .await;
    let duration_ms = start.elapsed().as_millis();

    // Scripts are read from disk on every execution, so an updated checkout
    // takes effect on the next sync/enrich/endpoint run — no restart needed.
    // (Only a script that did NOT exist at boot keeps its unresolved path
    // until the next restart, because path resolution runs once at startup.)
    match result {
        Ok(()) => Ok((
            StatusCode::OK,
            Json(ProjectSyncResult {
                project_id: id,
                success: true,
                duration_ms,
                error: None,
            }),
        )),
        Err(e) => Ok((
            StatusCode::BAD_GATEWAY,
            Json(ProjectSyncResult {
                project_id: id,
                success: false,
                duration_ms,
                error: Some(e),
            }),
        )),
    }
}
