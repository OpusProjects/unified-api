use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use std::sync::Arc;
use utoipa::IntoParams;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::ApiError;
use crate::adapters::r#in::http::error::ErrorBody;
use crate::application::topology::{Inputs, MAX_DEPTH, describe};
use crate::domain::topology::Topology;

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
        },
        |id| auth.permissions.allows_source(id),
        query.depth.unwrap_or(2).min(MAX_DEPTH),
    )
    .await;
    Ok(Json(if auth.permissions.is_admin() {
        topology
    } else {
        for_restricted(topology)
    }))
}

// What a restricted caller may see of the peers behind the sources it reads.
// The peers were asked with THIS instance's key, which may be an admin key
// over there: their own peers, URLs and counts are that key's view, not the
// caller's, so only each peer's version is kept. And why a peer was not
// asked stays generic: the resolver's message names env vars and Vault
// paths, which /credentials keeps for admins.
fn for_restricted(mut topology: Topology) -> Topology {
    for peer in &mut topology.peers {
        if let Some(instance) = peer.instance.as_mut() {
            instance.sources = None;
            instance.peers = Vec::new();
        }
        if peer
            .error
            .as_deref()
            .is_some_and(|e| e.starts_with("not asked:") && !e.contains("depth limit"))
        {
            peer.error = Some("not asked: this instance cannot authenticate to it".to_string());
        }
    }
    topology
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::topology::Peer;

    fn peer(instance: Option<Topology>, error: Option<&str>) -> Peer {
        Peer {
            url: "https://edge.example".to_string(),
            sources: Vec::new(),
            instance: instance.map(Box::new),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn a_restricted_caller_sees_a_peers_version_and_no_secret_locations() {
        let behind = Topology {
            version: "0.33.0".to_string(),
            sources: Some(12),
            peers: vec![peer(None, None)],
        };
        let topology = Topology {
            version: "0.34.0".to_string(),
            sources: Some(1),
            peers: vec![
                peer(Some(behind), None),
                peer(None, Some("not asked: Env var 'EDGE_TOKEN' not set")),
                peer(None, Some("not asked: depth limit reached")),
            ],
        };
        let seen = for_restricted(topology);
        let instance = seen.peers[0].instance.as_ref().unwrap();
        assert_eq!(instance.version, "0.33.0");
        assert!(instance.peers.is_empty() && instance.sources.is_none());
        assert!(
            !seen.peers[1]
                .error
                .as_deref()
                .unwrap()
                .contains("EDGE_TOKEN")
        );
        assert_eq!(
            seen.peers[2].error.as_deref(),
            Some("not asked: depth limit reached")
        );
    }
}
