use std::sync::OnceLock;
use utoipa::OpenApi;

use crate::adapters::r#in::http::openapi::ApiDoc;

// The OpenAPI document, read from inside the process: what the browsable pages
// show about a route (its description, parameters, responses and actions)
// comes from the same document Swagger publishes, so the two cannot drift.
// The lookups are web-rs's (the `web` repository); this module builds the
// document once and answers through it.
pub use web_rs::spec::{Action, ParamDoc, RouteDoc};

fn spec() -> &'static web_rs::Spec {
    static SPEC: OnceLock<web_rs::Spec> = OnceLock::new();
    SPEC.get_or_init(|| web_rs::Spec::new(ApiDoc::openapi()))
}

// What the spec says about this path, for the method the page was served with.
pub fn lookup(path: &str, method: &str) -> Option<RouteDoc> {
    spec().lookup(path, method)
}

// Whether a path is a GET route at all, so a breadcrumb can link it.
pub fn is_gettable(path: &str) -> bool {
    spec().is_gettable(path)
}

// Whether a GET route takes `limit`, so a page can offer paging.
pub fn accepts_limit(path: &str) -> bool {
    spec().accepts_limit(path)
}

// What a page about `template` can offer to do.
pub fn actions(template: &str) -> Vec<Action> {
    spec().actions(template)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A wildcard must not swallow an empty segment: //  is not an id.

    #[test]
    fn the_static_route_wins_over_the_capture() {
        let doc = lookup("/api/v1/config/validate", "POST").expect("a route");
        assert_eq!(doc.template, "/api/v1/config/validate");
    }

    // The point of the module: the documentation written in the handler
    // attribute is readable from here.
    #[test]
    fn a_route_carries_its_parameters_from_the_spec() {
        let doc = lookup("/api/v1/sources/src-d42/sync", "POST").expect("a route");
        assert_eq!(doc.template, "/api/v1/sources/{id}/sync");
        assert!(doc.methods.contains(&"POST"));

        let host = doc
            .parameters
            .iter()
            .find(|p| p.name == "host")
            .expect("the host parameter");
        assert_eq!(host.location, "query");
        assert!(
            host.description
                .as_ref()
                .is_some_and(|d| d.contains("Sync only these hosts")),
            "the parameter's description must come through"
        );

        assert!(
            doc.responses.iter().any(|(code, _)| code == "404"),
            "documented status codes must come through"
        );
    }

    #[test]
    fn allow_lists_every_method_a_route_answers() {
        let doc = lookup("/api/v1/sources/src-d42", "GET").expect("a route");
        assert!(doc.methods.contains(&"GET"));
        assert!(doc.methods.contains(&"DELETE"));
    }

    // The breadcrumb question: which ancestors are worth linking.
    #[test]
    fn a_pageable_route_is_recognised() {
        assert!(accepts_limit("/api/v1/sources/src-d42/dataset"));
        assert!(accepts_limit("/api/v1/sources/src-d42/dataset?offset=10"));
        // /hosts returns names only and has no paging
        assert!(!accepts_limit("/api/v1/sources/src-d42/hosts"));
        assert!(!accepts_limit("/api/v1/sources"));
    }

    #[test]
    fn only_gettable_paths_are_linkable() {
        assert!(is_gettable("/api/v1/sources"));
        // A query string is arguments to the route, not part of it
        assert!(is_gettable("/api/v1/sources/src-d42/dataset?limit=200"));
        assert!(is_gettable("/api/v1/sources/src-d42"));
        assert!(is_gettable("/api/v1/sources/src-d42/hosts"));
        // POST-only: a link here could only ever 405
        assert!(!is_gettable("/api/v1/sources/src-d42/sync"));
        assert!(!is_gettable("/api/v1/enrichers/enr-a/run"));
        // Not a route at all
        assert!(!is_gettable("/api/v1/enrichers/enr-a"));
    }
}

// What can be DONE here, rather than read.
//
// Derived from the spec, not from a list someone has to remember to update: an
// action is any non-GET method on this route, plus any POST on a route one
// segment below it. That is exactly `DELETE /sources/{id}` (evict) and
// `POST /sources/{id}/sync` when you are looking at a source, and it will be
// the next such route the day someone adds one.
//
// Children that introduce a NEW path parameter are skipped — the page has no
// value to put in `{hostname}`, so `PUT /sources/{id}/hosts/{hostname}` belongs
// on a host's page, not on the source's.

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn a_source_offers_sync_and_evict() {
        let actions = actions("/api/v1/sources/{id}");
        let sync = actions
            .iter()
            .find(|a| a.suffix == "/sync")
            .expect("sync action");
        assert_eq!(sync.method, "POST");
        assert!(
            sync.parameters.iter().any(|p| p.name == "host"),
            "the form must offer the host parameter"
        );

        let evict = actions
            .iter()
            .find(|a| a.suffix.is_empty() && a.method == "DELETE")
            .expect("evict action");
        assert!(evict.parameters.iter().all(|p| p.location == "query"));
    }

    // A child that needs a path parameter we cannot fill belongs elsewhere.
    #[test]
    fn a_child_with_a_new_capture_is_not_an_action() {
        let actions = actions("/api/v1/sources/{id}/hosts");
        assert!(
            actions.is_empty(),
            "PUT/DELETE on {{hostname}} must not appear on the host LIST page"
        );
    }

    #[test]
    fn a_read_only_route_offers_nothing() {
        assert!(actions("/api/v1/sources/{id}/groups").is_empty());
    }
}
