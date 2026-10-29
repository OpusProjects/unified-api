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
use crate::adapters::r#in::http::sources::{RunInfo, SyncHealthInfo};
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
    /// The credential it pulls with, when the repository needs one
    pub credential_id: Option<String>,
    /// Whether a checkout currently exists on disk
    pub checkout_present: bool,
    /// The commit the checkout is at (full id); null without a checkout
    pub revision: Option<String>,
    /// Seconds between periodic re-pulls (absent/0 = only boot and on demand)
    pub sync_interval_seconds: Option<u64>,
    pub sync_on_boot: bool,
    /// Cron expression (UTC) for re-pulls, the alternative to an interval
    pub schedule: Option<String>,
    /// Seconds one clone or pull may take before it is killed
    pub timeout_seconds: u64,
    /// What reads its scripts from this checkout: the ids whose `project_id`
    /// names it. A project nothing uses can go; one with users cannot,
    /// without breaking them.
    pub used_by: ProjectUsers,
    /// Whether anything is still managing to pull this checkout — last
    /// attempt, last success, last error, consecutive failures. Absent until a
    /// sync has run through this process (boot, interval or on demand). This
    /// is where "the checkout exists but is stuck on a stale commit" becomes
    /// visible: `checkout_present` stays true while every pull fails.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
    /// Its last pulls, newest first (up to 50, kept in memory since this
    /// process started): when, why and by whom, and the commit each moved
    /// the checkout from and to.
    pub history: Vec<RunInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct ProjectUsers {
    /// Connectors whose scripts live in this checkout
    pub connectors: Vec<String>,
    /// Sources that run from it: through their connector, or (static
    /// inventory) by naming it themselves
    pub sources: Vec<String>,
    pub endpoints: Vec<String>,
    pub enrichers: Vec<String>,
}

// Sorted ids of everything in `items` whose project is `project`.
fn users_of<'a, T: 'a>(
    items: impl IntoIterator<Item = (&'a String, &'a T)>,
    project: &str,
    project_of: impl Fn(&T) -> Option<&str>,
) -> Vec<String> {
    let mut ids: Vec<String> = items
        .into_iter()
        .filter(|(_, item)| project_of(item) == Some(project))
        .map(|(id, _)| id.clone())
        .collect();
    ids.sort();
    ids
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
    let config = state.config();
    let mut projects: Vec<ProjectInfo> = Vec::with_capacity(config.projects.len());
    for (id, project) in &config.projects {
        let checkout_present = tokio::fs::try_exists(state.projects_dir.join(id).join(".git"))
            .await
            .unwrap_or(false);

        projects.push(ProjectInfo {
            project_id: id.clone(),
            name: project.name.clone(),
            // Shown, so never with a token someone wrote into it; git itself
            // clones from the configured value.
            git_url: crate::domain::topology::without_userinfo(&project.git_url),
            branch: project.branch.clone(),
            credential_id: project.credential_id().map(str::to_string),
            checkout_present,
            revision: if checkout_present {
                state.git.revision(&state.projects_dir.join(id)).await
            } else {
                None
            },
            sync_interval_seconds: project.sync_interval_seconds,
            sync_on_boot: project.sync_on_boot,
            schedule: project.schedule.clone(),
            timeout_seconds: project.timeout_seconds,
            used_by: ProjectUsers {
                connectors: users_of(&config.connectors, id, |c| c.project.as_deref()),
                sources: users_of(&config.sources, id, |s| s.project_id.as_deref()),
                endpoints: users_of(&config.endpoints, id, |e| e.project_id.as_deref()),
                enrichers: users_of(&config.enrichers, id, |e| e.project_id.as_deref()),
            },
            sync_health: state.project_health.get(id).map(Into::into),
            history: state
                .project_health
                .history(id)
                .into_iter()
                .map(|run| RunInfo::from_run(run, true))
                .collect(),
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
    let config = state.config();
    let project = config
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
        crate::application::projects::RunOrigin {
            trigger: crate::domain::sync_health::RunTrigger::Manual,
            by: Some(auth.key_name.as_deref().unwrap_or("open").to_string()),
        },
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
