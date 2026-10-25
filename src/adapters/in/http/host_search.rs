use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::host_search::search;

// Declared as query parameters here: the handler takes them wrapped in a
// Result (to answer a malformed one as JSON), which hides the Query<T> the
// OpenAPI derive would otherwise infer them from.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct HostSearchQuery {
    /// Part of a host name, any case; at least two characters
    pub q: String,
    /// Most hosts to return (default 20, at most 100)
    pub limit: Option<usize>,
}

#[derive(Serialize, ToSchema)]
pub struct HostSearchResult {
    /// The hosts found, best match first: the whole name, then its start,
    /// then anywhere in it
    pub hosts: Vec<FoundHost>,
    /// How many hosts matched in all, before the limit
    pub total: usize,
}

#[derive(Serialize, ToSchema)]
pub struct FoundHost {
    pub host: String,
    /// The sources holding it, with the age of each one's data
    pub sources: Vec<FoundIn>,
    /// The endpoints whose default output includes it: their sources, limit
    /// and own settings decide, and an endpoint missing data from a source
    /// (which answers 503) is never listed. A caller's own parameters can
    /// still leave the host out of a particular call.
    pub endpoints: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct FoundIn {
    pub source_id: String,
    pub age_seconds: u64,
}

#[utoipa::path(
    get,
    path = "/api/v1/hosts/search",
    tag = "Sources",
    params(HostSearchQuery),
    responses(
        (status = 200, description = "Hosts whose name contains the query, across every source the key may read, with where each is held and which endpoints it reaches. From the cache: nothing is gathered", body = HostSearchResult),
        (status = 400, description = "`q` is missing or a parameter is malformed", body = ErrorBody),
        (status = 401, description = "Not authenticated", body = ErrorBody)
    )
)]
pub async fn search_hosts(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    query: Result<Query<HostSearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<HostSearchResult>, ApiError> {
    // A missing `q` answers like every other failure: {"error": ...}.
    let Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let config = state.config();
    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    let result = search(
        &query.q,
        limit,
        crate::application::host_search::Catalog {
            sources: &config.sources,
            views: &config.views,
            endpoints: &config.endpoints,
            cache: state.cache.as_ref(),
            scopes: state.advertised_scopes.as_ref(),
        },
        |id| auth.permissions.allows_source(id),
        |id| auth.permissions.allows_endpoint(id),
    );
    Ok(Json(HostSearchResult {
        hosts: result
            .matches
            .into_iter()
            .map(|m| FoundHost {
                host: m.host,
                sources: m
                    .sources
                    .into_iter()
                    .map(|s| FoundIn {
                        source_id: s.source_id,
                        age_seconds: s.age_seconds,
                    })
                    .collect(),
                endpoints: m.endpoints,
            })
            .collect(),
        total: result.total,
    }))
}
