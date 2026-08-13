use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

// Rename the use case on import because the HTTP handler wrapping
// it has the same name (sync_source)
use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::sync::{
    DEFAULT_REFRESH_DEPTH, SyncRequest, SyncScope, sync_source as application_sync_source,
};

// IntoParams = utoipa generates documentation for query params
// Each Option<String> field appears as an optional parameter in Swagger
#[derive(Deserialize, IntoParams)]
pub struct SyncParams {
    /// Sync only these hosts, comma-separated (e.g. motoko.section9.net)
    pub host: Option<String>,
    /// Sync only hosts in this group (e.g. magi)
    pub group: Option<String>,
    /// Make a federated source's origin re-gather before answering. Has no
    /// effect on a local source: its sync already gathers fresh data.
    pub refresh_origin: Option<bool>,
    /// How many federation hops the refresh may travel (default 3). Only
    /// meaningful with refresh_origin.
    pub refresh_depth: Option<u8>,
}

#[derive(Serialize, ToSchema)]
pub struct SyncResult {
    pub source_id: String,
    pub success: bool,
    /// "full", "host:motoko.section9.net", or "group:magi"
    pub scope: String,
    pub total_hosts: usize,
    pub total_groups: usize,
    pub sync_duration_ms: u128,
    pub error: Option<String>,
    /// True when no gather ran for this request: a full sync that started
    /// after it began completed while it queued, and these counts report what
    /// that sync left in the cache. N concurrent full syncs cost the origin
    /// one gather, not N.
    pub coalesced: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/sources/{id}/sync",
    tag = "Sources",
    params(
        ("id" = String, Path, description = "Source identifier"),
        SyncParams
    ),
    responses(
        (status = 200, description = "Sync result with host/group counts. Always 200 when the request itself was valid: a connector that failed reports success=false with the reason in `error` — including an origin that refused to re-gather under refresh_origin", body = SyncResult),
        (status = 400, description = "The id names a view — a view gathers nothing, so a sync of it has no meaning; sync the member source instead", body = ErrorBody),
        (status = 403, description = "API key not allowed to sync this source", body = ErrorBody),
        (status = 404, description = "Source not configured", body = ErrorBody)
    )
)]
pub async fn sync_source(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    Query(params): Query<SyncParams>,
) -> Result<Json<SyncResult>, ApiError> {
    if !auth.permissions.allows_source(&id) {
        return Err(ApiError::source_forbidden(&id));
    }
    // A view gathers nothing, so a sync of it has no meaning to invent. The
    // tempting reading — "sync every member" — would let a request aimed at one
    // consumer's view re-gather somebody else's datacenter.
    if let Some(view) = state.views.get(&id) {
        return Err(crate::adapters::r#in::http::views::write_refused(
            &id,
            view,
            "be synced",
        ));
    }
    // Not the same 404 as the read routes: this one means the id is absent
    // from sources.yaml, not merely uncached. source_for_sync also resolves
    // the script path into its project checkout, per execution.
    let source = state
        .source_for_sync(&id)
        .ok_or_else(|| ApiError::source_not_configured(&id))?;

    // `?host=` accepts a comma-separated list, like everywhere else in the API.
    // A value that is all separators selects no hosts, which would be a sync of
    // nothing: fall through to the next scope rather than gather and discard.
    let scope = params
        .host
        .as_deref()
        .and_then(SyncScope::hosts_from_csv)
        .or_else(|| params.group.clone().map(SyncScope::Group))
        .unwrap_or(SyncScope::Full);

    // The refresh intent is separate from the scope: the scope says which hosts,
    // this says whether a federated source's origin should go and re-gather them
    // rather than hand over what it already has.
    let request = if params.refresh_origin.unwrap_or(false) {
        SyncRequest::refreshing_origin(scope, params.refresh_depth.unwrap_or(DEFAULT_REFRESH_DEPTH))
    } else {
        SyncRequest::new(scope)
    };

    // The handler only translates HTTP ↔ use case; the sync logic
    // lives in application::sync (shared with the scheduler)
    let connector = state.connector_for(&source.connector_type);
    let enrichment = state.enrichment();
    let outcome = application_sync_source(
        &*state.cache,
        &**connector,
        &*state.secrets,
        &state.sync_health,
        &state.syncs,
        &id,
        &source,
        request,
        Some(&enrichment),
    )
    .await;

    Ok(Json(SyncResult {
        source_id: id,
        success: outcome.success(),
        scope: outcome.scope,
        total_hosts: outcome.total_hosts,
        total_groups: outcome.total_groups,
        sync_duration_ms: outcome.duration_ms,
        error: outcome.error,
        coalesced: outcome.coalesced,
    }))
}
