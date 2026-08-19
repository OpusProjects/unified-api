use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};

// Scope advertising: an instance states, over the API, what a source (or
// view) claims to own — the fact a federating central's view had to duplicate
// into its own config until now. Config-derived, never cache-derived, so it
// answers identically whether or not anything has synced: scope is a
// declaration about responsibility, not a report about data.

#[derive(Serialize, ToSchema)]
pub struct ScopeInfo {
    pub source_id: String,
    /// "source" or "view" — a view advertises the union of its members'
    /// declared ownership patterns
    pub kind: &'static str,
    /// False when this source makes no ownership claim at all (an ordinary
    /// script source without `advertise_scope`); groups/hosts are then empty
    /// and mean nothing
    pub declared: bool,
    /// Group names the consumer resolves against an inventory of ITS choosing
    pub groups: Vec<String>,
    /// Hostnames claimed literally
    pub hosts: Vec<String>,
    /// True when the claim is "everything my inventory dependency knows"
    /// (an SSH source with an empty match_pattern) — stated as a flag so a
    /// consumer can tell "claims everything" from "claims nothing"
    pub catch_all: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/sources/{id}/scope",
    tag = "Sources",
    params(("id" = String, Path, description = "Source or view identifier")),
    responses(
        (status = 200, description = "The ownership scope this source (or view) advertises, derived from its configuration: an explicit advertise_scope block, an SSH source's hosts_from_source pattern, or a view's member ownership union. declared=false means no claim is made", body = ScopeInfo),
        (status = 403, description = "API key not allowed to read this source", body = ErrorBody),
        (status = 404, description = "Source not configured (scope is config, so an uncached-but-configured source still answers)", body = ErrorBody)
    )
)]
pub async fn source_scope(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ScopeInfo>, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }

    // Views share the source routes and id space, as everywhere else
    let config = state.config();
    if let Some(view) = config.views.get(&id) {
        let claim = view.advertised_scope();
        return Ok(Json(ScopeInfo {
            source_id: id,
            kind: "view",
            declared: true,
            groups: claim.groups,
            hosts: claim.hosts,
            catch_all: claim.catch_all,
        }));
    }

    // Config, not cache: this is the not-configured 404, like POST /sync
    let source = config
        .sources
        .get(&id)
        .ok_or_else(|| ApiError::source_not_configured(&id))?;

    let info = match source.advertised_scope() {
        Some(claim) => ScopeInfo {
            source_id: id,
            kind: "source",
            declared: true,
            groups: claim.groups,
            hosts: claim.hosts,
            catch_all: claim.catch_all,
        },
        None => ScopeInfo {
            source_id: id,
            kind: "source",
            declared: false,
            groups: Vec::new(),
            hosts: Vec::new(),
            catch_all: false,
        },
    };

    Ok(Json(info))
}
