use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::adapters::r#in::http::auth::Permissions;

// Browser sessions, for the browsable API: the store, its tokens and cookie,
// CSRF, and the layer that turns a browser form into an API request come from
// auth-rs, shared with unified-gitops. What is ours is the cookie's name
// and the permissions a session holds.
//
// Why a session at all, when the API already has keys: Swagger's Authorize
// button works because Swagger never navigates — it holds the key in
// JavaScript and attaches it to the `fetch` calls it makes itself. A browsable
// API is the opposite. Every click is a document navigation, and a document
// navigation carries no custom headers. That is the whole reason AWX has a
// login form and a session cookie rather than a header box.
pub const COOKIE_NAME: &str = "uapi_session";

pub use auth_rs::http::{FormControls, Issued, SessionKind, cookie_value, csrf_matches, new_token};

pub type SessionStore = auth_rs::http::SessionStore<Permissions>;
pub type Sessions = auth_rs::http::Sessions<Permissions>;
pub type Authenticated = auth_rs::http::Authenticated<Permissions>;

// Browser forms, translated into what the API already speaks: `_csrf` lifted
// out for the auth middleware, the other fields moved to the query, and
// `_method=DELETE` honoured for a request carrying our session cookie only.
// See auth-rs's `http::session::form_action` for why each of those.
pub async fn form_action(request: Request, next: Next) -> Response {
    auth_rs::http::form_action(COOKIE_NAME, request, next).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    // A cross-site form must not be able to reach a DELETE route. The override
    // exists for a browser session and nothing else.
    #[tokio::test]
    async fn the_method_override_needs_a_session_cookie() {
        use axum::body::Body;
        use axum::http::Request as HttpRequest;
        use axum::routing::post;
        use tower::ServiceExt;

        async fn which_method(method: Method) -> String {
            method.to_string()
        }

        let app = axum::Router::new()
            .route("/x", post(which_method).delete(which_method))
            .layer(axum::middleware::from_fn(form_action));

        let ask = |cookie: Option<&str>| {
            let mut req = HttpRequest::builder()
                .method("POST")
                .uri("/x")
                .header("content-type", "application/x-www-form-urlencoded");
            if let Some(cookie) = cookie {
                req = req.header("cookie", cookie);
            }
            req.body(Body::from("_method=DELETE")).unwrap()
        };

        let body = |resp: Response| async {
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            String::from_utf8(bytes.to_vec()).unwrap()
        };

        // No cookie: the override is ignored, the POST stays a POST
        let resp = app.clone().oneshot(ask(None)).await.unwrap();
        assert_eq!(body(resp).await, "POST");

        // With a session cookie present it is honoured
        let resp = app.oneshot(ask(Some("uapi_session=abc"))).await.unwrap();
        assert_eq!(body(resp).await, "DELETE");
    }

    #[test]
    fn cookie_is_found_among_others() {
        let header = "theme=dark; uapi_session=deadbeef; other=1";
        assert_eq!(cookie_value(header, COOKIE_NAME), Some("deadbeef"));
        assert_eq!(cookie_value("theme=dark", COOKIE_NAME), None);
        // A cookie whose name merely ends with ours must not match
        assert_eq!(cookie_value("not_uapi_session=x", COOKIE_NAME), None);
    }
}
