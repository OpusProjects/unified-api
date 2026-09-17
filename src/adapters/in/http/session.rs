use axum::body::Body;
use axum::extract::Request;
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

use crate::adapters::r#in::http::auth::Permissions;

// Browser sessions, for the browsable API.
//
// Why a session at all, when the API already has keys: Swagger's Authorize
// button works because Swagger never navigates — it holds the key in
// JavaScript and attaches it to the `fetch` calls it makes itself. A browsable
// API is the opposite. Every click is a document navigation, and a document
// navigation carries no custom headers. That is the whole reason AWX has a
// login form and a session cookie rather than a header box.
//
// What a session is NOT: a second credential store. Logging in means presenting
// an existing API key, validated by exactly the same constant-time compare as a
// header would be. The session is a short-lived handle on that key, so the
// permission model has one source (`api_keys.yaml`) and the UI inherits it.
pub const COOKIE_NAME: &str = "uapi_session";

pub struct Session {
    // Which key this session is, so it can be revoked with the key and named in
    // the access log exactly as a header-authenticated request is.
    pub key_name: String,
    pub permissions: Permissions,
    // Paired with the session and required on every unsafe request that
    // authenticates by cookie. See `auth::require_api_key` for why.
    pub csrf: String,
    expires_at: Instant,
}

pub struct SessionStore {
    sessions: DashMap<String, Session>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
        }
    }

    // In memory, deliberately: a restart invalidating every session is correct
    // behaviour for an operator console, and it keeps the promise that this
    // service has no external data dependency. The alternative — signed cookies
    // that survive a restart — means a key revoked while the process was down
    // comes back to life with it.
    pub fn create(&self, key_name: String, permissions: Permissions, ttl: Duration) -> Issued {
        // Expired entries are normally dropped when looked up, but a browser
        // that logs in and is never used again is never looked up. Sweeping on
        // login bounds the map by "sessions in use" instead of "logins since
        // the process started", and it costs a walk of a map that is small by
        // construction.
        self.sessions
            .retain(|_, session| Instant::now() < session.expires_at);

        let token = random_token();
        let csrf = random_token();
        self.sessions.insert(
            token.clone(),
            Session {
                key_name,
                permissions,
                csrf: csrf.clone(),
                expires_at: Instant::now() + ttl,
            },
        );
        Issued { token, csrf }
    }

    // Returns the session's identity, or None when the token is unknown or
    // expired. An expired entry is removed on the way out rather than by a
    // sweeper task: the map only grows with logins, and every login eventually
    // gets looked up or outlives the process.
    pub fn get(&self, token: &str) -> Option<Authenticated> {
        let expired = {
            let session = self.sessions.get(token)?;
            if Instant::now() >= session.expires_at {
                true
            } else {
                return Some(Authenticated {
                    key_name: session.key_name.clone(),
                    permissions: session.permissions.clone(),
                    csrf: session.csrf.clone(),
                });
            }
        };
        if expired {
            self.sessions.remove(token);
        }
        None
    }

    pub fn remove(&self, token: &str) {
        self.sessions.remove(token);
    }

    // A configuration reload can rewrite api_keys.yaml. Without this, a key
    // revoked by that reload keeps working in every browser that logged in
    // before it — the console would outlive the revocation, which is exactly
    // the property a revocation is for.
    pub fn retain_keys(&self, live: &[String]) {
        self.sessions
            .retain(|_, session| live.contains(&session.key_name));
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

pub struct Issued {
    pub token: String,
    pub csrf: String,
}

pub struct Authenticated {
    pub key_name: String,
    pub permissions: Permissions,
    pub csrf: String,
}

// 256 bits from the OS, hex-encoded. Never the API key itself: a key in a
// cookie is a key in a browser profile, in a backup, and in every screenshot of
// devtools someone pastes into a ticket.
pub fn new_token() -> String {
    random_token()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random source");
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap());
    }
    out
}

// The CSRF token is compared the same way the API key is: a token that is
// checked with `==` leaks its bytes through timing, and this one guards every
// write the console can make.
pub fn csrf_matches(expected: &str, presented: &str) -> bool {
    bool::from(expected.as_bytes().ct_eq(presented.as_bytes()))
}

// The session cookie out of a Cookie header. Written by hand rather than with a
// cookie crate: one name, no attributes to parse on the way in, and the jar
// crates bring a dependency tree for a `split(';')`.
pub fn cookie_value<'h>(header: &'h str, name: &str) -> Option<&'h str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key.trim() == name).then_some(value.trim())
    })
}

// The store, injected as a router Extension the way the key registry is.
#[derive(Clone)]
pub struct Sessions(pub Arc<SessionStore>);

// What a browser form said, lifted out of the body before anything downstream
// reads the request. Absent for every request that is not a form post.
#[derive(Clone, Default)]
pub struct FormControls {
    pub csrf: Option<String>,
}

// Browser forms, translated into what the API already speaks.
//
// Two jobs, both of which exist because an HTML form is a poor HTTP client:
//
//   - It can only send GET and POST. The DELETE routes are reached with a POST
//     carrying `_method=DELETE`, translated here and NOWHERE else — the JSON
//     API keeps its real verbs, and a JSON client never goes through this path
//     because it does not post urlencoded bodies.
//   - It cannot set a header, so its CSRF token travels in the body. Lifting it
//     out here keeps the auth middleware from having to parse bodies, which is
//     not its job and would make it the one place that buffers a request.
//
//   - Its real fields (`host`, `group`, …) are what the operator typed, and the
//     handlers read those from the QUERY. They are moved there, because a form
//     cannot put them in a URL by itself once it is POSTing.
//
// The body is then replaced with an empty one: every route a form can reach
// (sync, run, evict, delete) takes no body at all, so once the fields are
// lifted out there is nothing left to carry. A JSON client never passes
// through here — it does not post this content-type.
pub async fn form_action(request: Request, next: Next) -> Response {
    let is_form = request.method() == Method::POST
        && request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|ct| ct.starts_with("application/x-www-form-urlencoded"));

    if !is_form {
        return next.run(request).await;
    }

    let (mut parts, body) = request.into_parts();
    // Small by construction: a couple of control fields. A form body larger
    // than this is not one of ours.
    let Ok(bytes) = axum::body::to_bytes(body, 8 * 1024).await else {
        // Say what went wrong. Falling through with an empty body used to
        // reach the auth middleware with no CSRF token and answer 403, which
        // sends whoever hit it looking for a permissions problem that is not
        // there.
        return (
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "form body too large",
        )
            .into_response();
    };

    let mut csrf = None;
    let mut method_override = None;
    // Everything that is not a control field is a real parameter the operator
    // typed, and the handlers read those from the QUERY — `?host=`, `?group=`.
    // Moving them there is the whole point of the form: without this, typing a
    // hostname into the Sync field and pressing the button re-gathered the
    // entire source, silently doing far more work than was asked for.
    let mut passthrough: Vec<String> = Vec::new();
    for pair in String::from_utf8_lossy(&bytes).split('&') {
        match pair.split_once('=') {
            Some(("_csrf", value)) => csrf = Some(value.to_string()),
            Some(("_method", value)) => method_override = Some(value.to_ascii_uppercase()),
            // A browser submits every field, empty ones included; an empty
            // `?host=` is not the same request as no `host` at all.
            Some((name, value)) if !name.starts_with('_') && !value.is_empty() => {
                passthrough.push(format!("{name}={value}"));
            }
            _ => {}
        }
    }

    if !passthrough.is_empty() {
        parts.uri = merge_query(&parts.uri, &passthrough);
    }

    parts.extensions.insert(FormControls { csrf });

    // Only DELETE, and only for a request that carries a session cookie.
    //
    // The cookie requirement is the important half. Without it, ANY urlencoded
    // POST could become a DELETE — including one from a page on the internet
    // aimed at an instance with no API keys configured, where every caller is
    // admin and no CSRF token is demanded. A browser form cannot otherwise
    // reach a DELETE route at all, so honouring `_method` unconditionally
    // handed the web a write primitive it did not have before. A caller
    // holding an API key loses nothing: it can send a real DELETE.
    let from_a_session = parts
        .headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| cookie_value(c, COOKIE_NAME).is_some());

    if from_a_session && method_override.as_deref() == Some("DELETE") {
        parts.method = Method::DELETE;
        parts.headers.remove(header::CONTENT_TYPE);
    }
    parts.headers.remove(header::CONTENT_LENGTH);

    next.run(Request::from_parts(parts, Body::empty())).await
}

// Put the form's fields into the request's query string, keeping whatever was
// already there. The values arrive percent-encoded from the browser and go
// back out the same way, so they are moved verbatim rather than decoded and
// re-encoded — this is a transport change, not a parse.
fn merge_query(uri: &axum::http::Uri, extra: &[String]) -> axum::http::Uri {
    let mut query: Vec<String> = uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect();
    query.extend(extra.iter().cloned());

    let mut parts = uri.clone().into_parts();
    let path = parts
        .path_and_query
        .as_ref()
        .map(|pq| pq.path().to_string())
        .unwrap_or_else(|| "/".to_string());

    match format!("{path}?{}", query.join("&")).parse() {
        Ok(path_and_query) => {
            parts.path_and_query = Some(path_and_query);
            axum::http::Uri::from_parts(parts).unwrap_or_else(|_| uri.clone())
        }
        // A field the browser encoded in a way we cannot put back into a URI:
        // the request proceeds without it rather than failing, and the handler
        // answers for the scope it can see.
        Err(_) => uri.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin() -> Permissions {
        Permissions::Admin
    }

    #[test]
    fn a_session_authenticates_as_its_key() {
        let store = SessionStore::new();
        let issued = store.create("AWX".into(), admin(), Duration::from_secs(60));
        let found = store.get(&issued.token).expect("session");
        assert_eq!(found.key_name, "AWX");
        assert_eq!(found.csrf, issued.csrf);
    }

    #[test]
    fn an_unknown_token_authenticates_nothing() {
        let store = SessionStore::new();
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn an_expired_session_stops_working_and_is_dropped() {
        let store = SessionStore::new();
        let issued = store.create("AWX".into(), admin(), Duration::from_secs(0));
        assert!(store.get(&issued.token).is_none());
        assert_eq!(store.len(), 0, "the expired entry must not be kept");
    }

    #[test]
    fn logout_ends_the_session() {
        let store = SessionStore::new();
        let issued = store.create("AWX".into(), admin(), Duration::from_secs(60));
        store.remove(&issued.token);
        assert!(store.get(&issued.token).is_none());
    }

    // The reload case: a key that disappears from api_keys.yaml must not keep
    // a browser logged in.
    #[test]
    fn revoking_a_key_ends_its_sessions_only() {
        let store = SessionStore::new();
        let stays = store.create("AWX".into(), admin(), Duration::from_secs(60));
        let goes = store.create("Retired".into(), admin(), Duration::from_secs(60));

        store.retain_keys(&["AWX".to_string()]);

        assert!(store.get(&stays.token).is_some());
        assert!(store.get(&goes.token).is_none());
    }

    #[test]
    fn tokens_are_unique_and_long() {
        let store = SessionStore::new();
        let a = store.create("k".into(), admin(), Duration::from_secs(60));
        let b = store.create("k".into(), admin(), Duration::from_secs(60));
        assert_ne!(a.token, b.token);
        assert_ne!(a.token, a.csrf, "the CSRF token is not the session token");
        assert_eq!(a.token.len(), 64);
        assert!(a.token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn csrf_compare_accepts_only_the_exact_token() {
        assert!(csrf_matches("abc", "abc"));
        assert!(!csrf_matches("abc", "abd"));
        assert!(!csrf_matches("abc", "ab"));
        assert!(!csrf_matches("abc", ""));
    }

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
    fn form_fields_become_query_parameters() {
        let uri: axum::http::Uri = "/api/v1/sources/src-d42/sync".parse().unwrap();
        let merged = merge_query(&uri, &["host=motoko.section9.net".to_string()]);
        assert_eq!(
            merged.to_string(),
            "/api/v1/sources/src-d42/sync?host=motoko.section9.net"
        );
    }

    #[test]
    fn form_fields_keep_an_existing_query() {
        let uri: axum::http::Uri = "/api/v1/sources/src-d42/sync?refresh_depth=2"
            .parse()
            .unwrap();
        let merged = merge_query(&uri, &["host=a".to_string(), "group=b".to_string()]);
        assert_eq!(merged.query(), Some("refresh_depth=2&host=a&group=b"));
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
