use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use std::sync::Arc;
use std::time::Instant;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::adapters::r#in::http::sources::SyncHealthInfo;
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
    /// Whether anything is still managing to pull this checkout — last
    /// attempt, last success, last error, consecutive failures. Absent until a
    /// sync has run through this process (boot, interval or on demand). This
    /// is where "the checkout exists but is stuck on a stale commit" becomes
    /// visible: `checkout_present` stays true while every pull fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
}

#[utoipa::path(
    get,
    path = "/api/v1/projects",
    tag = "Projects",
    responses(
        (status = 200, description = "Configured git projects and their checkout state", body = Vec<ProjectInfo>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<ProjectInfo>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }

    // A loop rather than a map, because `checkout_present` is a filesystem
    // question and it is asked with tokio::fs: `Path::exists` is a blocking
    // syscall, and this handler runs on the runtime. One stat per configured
    // project is small, but it is small on local disk — a checkout on a network
    // or overlay volume is the case that would park a worker thread with
    // unrelated requests queued behind it.
    //
    // An IO error (a permissions problem on the projects directory) reads as
    // "no checkout", which is what an operator would conclude from it anyway.
    let mut projects: Vec<ProjectInfo> = Vec::with_capacity(state.projects.len());
    for (id, project) in &state.projects {
        let checkout_present = tokio::fs::try_exists(state.projects_dir.join(id).join(".git"))
            .await
            .unwrap_or(false);

        projects.push(ProjectInfo {
            project_id: id.clone(),
            name: project.name.clone(),
            git_url: project.git_url.clone(),
            branch: project.branch.clone(),
            checkout_present,
            sync_interval_seconds: project.sync_interval_seconds,
            sync_on_boot: project.sync_on_boot,
            sync_health: state.project_health.get(id).map(Into::into),
        });
    }

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
        (status = 403, description = "API key is not admin", body = ErrorBody),
        (status = 404, description = "Project not configured", body = ErrorBody),
        (status = 502, description = "git clone/fetch failed", body = ProjectSyncResult)
    )
)]
pub async fn sync_project_now(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<(StatusCode, Json<ProjectSyncResult>), ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let project = state
        .projects
        .get(&id)
        .ok_or_else(|| ApiError::not_found(format!("project '{}' is not configured", id)))?;

    let start = Instant::now();
    let result = sync_project(
        &*state.git,
        &*state.secrets,
        &*state.venv,
        &state.project_health,
        &id,
        project,
        &state.projects_dir,
    )
    .await;
    let duration_ms = start.elapsed().as_millis();

    // Scripts are read from disk on every execution, and their paths resolve
    // into the checkout per run (application::scripts) — so both an updated
    // script and one that first APPEARS with this sync take effect on the
    // next sync/enrich/endpoint run. No restart for either case.
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        "project_sync",
        &id,
        if result.is_ok() { "success" } else { "error" },
    );

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
