use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use std::sync::Arc;
use utoipa::IntoParams;

use crate::AppState;
use crate::adapters::r#in::http::auth::{AuthContext, AuthMethod};
use crate::adapters::r#in::http::error::ApiError;
use crate::adapters::r#in::http::error::ErrorBody;
use crate::application::topology::{Inputs, MAX_DEPTH, describe};
use crate::domain::topology::{Federation, FederationPush, Topology};

// Declared as query parameters here: the handler takes them wrapped in a
// Result (to answer a malformed one as JSON), which hides the Query<T> the
// OpenAPI derive would otherwise infer them from.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct TopologyQuery {
    /// How many levels of peers to ask (default 2: this instance's peers and
    /// theirs). Capped at 3; 0 maps the peers without asking them.
    pub depth: Option<u8>,
}

// The federation seen from here, for the UI's Topology page.
//
// Any key may ask, and each sees the peers behind the sources it may read:
// an edge answers its central's restricted key with what that key is for,
// and nothing about sources it cannot see.
#[utoipa::path(
    get,
    path = "/api/v1/topology",
    tag = "Topology",
    params(TopologyQuery),
    responses(
        (status = 200, description = "This instance, the instances it pulls from through remote sources, and theirs, as far as `depth` reaches. A peer that could not be asked carries `error` instead of `instance`. A restricted key sees each peer's version only, not the peer's own peers", body = Topology),
        (status = 400, description = "`depth` is not a number from 0 to 255", body = ErrorBody),
        (status = 401, description = "Not authenticated", body = ErrorBody)
    )
)]
pub async fn get_topology(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    query: Result<Query<TopologyQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Topology>, ApiError> {
    // A malformed depth answers like every other failure: {"error": ...}.
    let Query(query) = query.map_err(|e| ApiError::bad_request(e.body_text()))?;
    let config = state.config();
    let topology = describe(
        Inputs {
            sources: &config.sources,
            cache: state.cache.as_ref(),
            health: state.sync_health.as_ref(),
            secrets: state.secrets.as_ref(),
            peers: Arc::clone(&state.peers),
            name: config.name.as_deref(),
        },
        |id| auth.permissions.allows_source(id),
        query.depth.unwrap_or(2).min(MAX_DEPTH),
    )
    .await;
    Ok(Json(if auth.permissions.is_admin() {
        topology
    } else {
        topology.redacted()
    }))
}

// An instance that reads from this one says what the federation looks like
// from there (application::topology::push_federation, every minute).
//
// Any API key may push: the instance above holds the restricted key its
// remote sources read with, and requiring admin would mean handing it more
// than it reads. What a push can do is bounded by that: it replaces the
// sender's own picture (stored under its key's identity, its `name`), shown
// to admins only, and changes nothing else. A browser session cannot push,
// whether it signed in through the directory or with a key: a session has no
// key identity to store the map under (its label is not unique, and could
// overwrite another key's entry), and no instance pushes through one.
#[utoipa::path(
    put,
    path = "/api/v1/topology/federation",
    tag = "Topology",
    request_body = FederationPush,
    responses(
        (status = 204, description = "Stored under the sending key's name, replacing that key's previous push"),
        (status = 400, description = "The body is not a federation push", body = ErrorBody),
        (status = 401, description = "Not authenticated", body = ErrorBody),
        (status = 403, description = "A browser session (signed in through the UI, with a key or a directory account) rather than an API key sent with the request", body = ErrorBody)
    )
)]
pub async fn put_federation(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    body: Result<Json<FederationPush>, axum::extract::rejection::JsonRejection>,
) -> Result<axum::http::StatusCode, ApiError> {
    // A key sent with the request is the only way to have a `key_id`; an
    // open instance (no keys at all) has nobody to tell apart.
    let sender = match (auth.method, auth.key_id) {
        (AuthMethod::ApiKey, Some(id)) => id,
        (AuthMethod::Open, _) => "open".to_string(),
        _ => {
            return Err(ApiError::forbidden(
                "a federation map is pushed by another instance, with an API key",
            ));
        }
    };
    let Json(push) = body.map_err(|e| ApiError::bad_request(e.body_text()))?;
    state.federation.record(sender, push);
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// The maps pushed here, newest first: on an edge, the federation it belongs
// to. Admin-only: it is the sender's whole map, other instances included.
#[utoipa::path(
    get,
    path = "/api/v1/topology/federation",
    tag = "Topology",
    responses(
        (status = 200, description = "One entry per instance that reads from this one and pushed its map in the last ten minutes. Empty on an instance nothing reads from", body = [Federation]),
        (status = 401, description = "Not authenticated", body = ErrorBody),
        (status = 403, description = "Not an admin key", body = ErrorBody)
    )
)]
pub async fn get_federation(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<Federation>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    Ok(Json(state.federation.list()))
}
