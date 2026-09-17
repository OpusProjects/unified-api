use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;

// The link graph. `Related` is the map a response carries to name the
// resources next to it, so a consumer can start at /api/v1/ and follow rather
// than having read the documentation first. Built here, in one place, because
// a route that moves has to move in exactly one file.
pub type Related = BTreeMap<String, String>;

// A cached source: everything it answers, plus the one thing it does.
//
// `sync` is a POST while the rest are GETs, and it is in here anyway, the way
// AWX lists `launch` on a job template: a caller that wants to know what can be
// done with this object should not have to consult a second document. What
// renders it as a button rather than a link is the consumer's problem, and the
// HTML layer treats the write verbs as exactly that.
pub fn source_related(id: &str) -> Related {
    let mut related = common_related(id);
    related.insert("sync".into(), format!("/api/v1/sources/{id}/sync"));
    // Eviction is a DELETE on the source's own address. Declared here even
    // though it duplicates `url`, because `related` is what the page reads to
    // decide which action buttons an OBJECT accepts — a view lists neither, and
    // so gets neither. Route templates cannot make that distinction: a view and
    // a source share `/api/v1/sources/{id}`.
    related.insert("evict".into(), format!("/api/v1/sources/{id}"));
    related
}

// A view answers the same reads and refuses every write (no sync, no host
// PUT/DELETE), so those links are absent rather than present-and-405. What it
// adds is `members`: /status is where a view reports the state of each member
// it routes to, which is the only place its internal topology is visible.
pub fn view_related(id: &str) -> Related {
    let mut related = common_related(id);
    related.insert("members".into(), format!("/api/v1/sources/{id}/status"));
    related
}

fn common_related(id: &str) -> Related {
    // A source id can contain characters that are not URL-safe. Config
    // validation keeps ids tame, so this is not an encoding routine — it is the
    // reminder that these strings end up in an href.
    BTreeMap::from([
        ("dataset".into(), format!("/api/v1/sources/{id}/dataset")),
        ("groups".into(), format!("/api/v1/sources/{id}/groups")),
        ("hosts".into(), format!("/api/v1/sources/{id}/hosts")),
        ("status".into(), format!("/api/v1/sources/{id}/status")),
        ("scope".into(), format!("/api/v1/sources/{id}/scope")),
    ])
}

// The entry point of the API: every collection, by name, as a path.
//
// Modelled on AWX's /api/v2/, and deliberately just as boring — a flat map of
// name to path. It is what turns the API from a set of addresses you have to
// know into something you can start reading at the root and follow, whether
// "you" is a person in a browser or `jq` in a pipeline.
//
// Paths, never absolute URLs: this process does not reliably know its own
// external origin (an ingress, a port-forward, another instance federating
// through it), and a confidently wrong URL is worse than a relative one the
// caller resolves against whatever address it used to get here.

#[derive(Serialize, ToSchema)]
pub struct ApiIndex {
    /// The running version, same source as the OpenAPI spec's
    pub version: &'static str,
    pub sources: &'static str,
    pub enrichers: &'static str,
    pub endpoints: &'static str,
    /// Admin-only route; absent for a restricted key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<&'static str>,
    /// Admin-only, and only when config_api.enabled is on
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<&'static str>,
    pub health: &'static str,
    pub ready: &'static str,
    pub metrics: &'static str,
    pub docs: &'static str,
}

#[utoipa::path(
    get,
    path = "/api/v1/",
    tag = "Index",
    responses(
        (status = 200, description = "Every collection this key may read, as paths", body = ApiIndex)
    )
)]
pub async fn api_index(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<ApiIndex> {
    // A link the caller cannot follow is worse than no link: it invites a
    // click that can only 403. So the two admin-only collections are present
    // exactly when this key could actually read them — which also makes the
    // index an honest answer to "what am I allowed to see?".
    let admin = auth.permissions.is_admin();

    Json(ApiIndex {
        version: env!("CARGO_PKG_VERSION"),
        sources: "/api/v1/sources",
        enrichers: "/api/v1/enrichers",
        endpoints: "/api/v1/endpoints",
        projects: admin.then_some("/api/v1/projects"),
        // Same condition the config handlers enforce: the routes exist in the
        // router either way, and answer 403 when no store is configured.
        config: (admin && state.config_store.is_some()).then_some("/api/v1/config"),
        health: "/healthz",
        ready: "/readyz",
        metrics: "/metrics",
        docs: "/swagger-ui/",
    })
}
