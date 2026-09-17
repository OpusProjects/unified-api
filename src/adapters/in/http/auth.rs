use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use axum::extract::Request;
use axum::http::Method;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

// A configured key with its secret already resolved from the environment
// (main.rs does the resolution at the boundary; nothing here touches env vars).
#[derive(Clone)]
pub struct ResolvedApiKey {
    pub name: String,
    pub secret: String,
    pub permissions: Permissions,
}

// What a caller is allowed to touch. Admin sees everything; Scoped only the
// listed ids. HashSet instead of Vec: `contains` is O(1) and the lists are
// checked on every request.
#[derive(Clone, Debug)]
pub enum Permissions {
    Admin,
    Scoped {
        sources: HashSet<String>,
        endpoints: HashSet<String>,
    },
}

impl Permissions {
    // Operational routes (project sync, listing projects) are admin-only:
    // they are deploy tooling, not consumer data access.
    pub fn is_admin(&self) -> bool {
        matches!(self, Permissions::Admin)
    }

    pub fn allows_source(&self, id: &str) -> bool {
        match self {
            Permissions::Admin => true,
            Permissions::Scoped { sources, .. } => sources.contains(id),
        }
    }

    pub fn allows_endpoint(&self, id: &str) -> bool {
        match self {
            Permissions::Admin => true,
            Permissions::Scoped { endpoints, .. } => endpoints.contains(id),
        }
    }
}

// The keys in force right now, behind one pointer a reload can replace.
//
// The middleware reads this on every request rather than holding a list, so
// rewriting api_keys.yaml takes effect on the next request — the one thing
// that would otherwise still need a restart on a system whose whole point is
// that a configuration push does not need one. A request already in flight
// keeps the set it authenticated against.
pub struct ApiKeyRegistry {
    keys: RwLock<Arc<[ResolvedApiKey]>>,
}

impl ApiKeyRegistry {
    pub fn new(keys: Vec<ResolvedApiKey>) -> Self {
        Self {
            keys: RwLock::new(keys.into()),
        }
    }

    pub fn load(&self) -> Arc<[ResolvedApiKey]> {
        Arc::clone(&self.keys.read().expect("api key registry lock"))
    }

    pub fn replace(&self, keys: Vec<ResolvedApiKey>) {
        *self.keys.write().expect("api key registry lock") = keys.into();
    }

    pub fn len(&self) -> usize {
        self.load().len()
    }

    pub fn is_empty(&self) -> bool {
        self.load().is_empty()
    }
}

// Turn api_keys.yaml definitions into runtime keys by reading each declared
// env var. A declared-but-missing env var is a hard error: the alternative
// (skip the key with a warn) means a typo silently locks a consumer out. The
// legacy UNIFIED_API_KEY, if set, joins as an admin key — existing
// deployments keep working unchanged.
//
// In the adapter that owns the keys rather than in main, because a
// configuration reload has to do exactly this again, against the file it just
// accepted, and a second copy of the rules is a second set of rules.
pub fn resolve_api_keys(cfg: &crate::config::AppConfig) -> Result<Vec<ResolvedApiKey>, String> {
    use crate::domain::api_key::ApiKeyRole;

    let mut keys = Vec::new();

    // Sorted so the order is deterministic in tests and logs
    let mut ids: Vec<&String> = cfg.api_keys.keys().collect();
    ids.sort();

    for id in ids {
        let def = &cfg.api_keys[id];
        let secret = std::env::var(&def.env).map_err(|_| {
            format!(
                "API key '{}' expects the secret in env var '{}', which is not set",
                id, def.env
            )
        })?;
        if secret.is_empty() {
            return Err(format!(
                "API key '{}': env var '{}' is set but empty",
                id, def.env
            ));
        }

        let permissions = match def.role {
            ApiKeyRole::Admin => Permissions::Admin,
            ApiKeyRole::Restricted => Permissions::Scoped {
                sources: def.sources.iter().cloned().collect(),
                endpoints: def.endpoints.iter().cloned().collect(),
            },
        };

        keys.push(ResolvedApiKey {
            name: def.name.clone(),
            secret,
            permissions,
        });
    }

    if let Ok(secret) = std::env::var("UNIFIED_API_KEY")
        && !secret.is_empty()
    {
        keys.push(ResolvedApiKey {
            name: "default".to_string(),
            secret,
            permissions: Permissions::Admin,
        });
    }

    Ok(keys)
}

// A browser session's identity, published on the response for the HTML layer.
#[derive(Clone)]
pub struct BrowserIdentity {
    pub key_name: String,
    pub csrf: String,
}

// Who authenticated this request. The middleware inserts it into the request
// extensions; handlers extract it with Extension<AuthContext> and enforce the
// permissions for the specific id they operate on (the middleware cannot — it
// would have to parse ids out of URLs, which breaks the moment a route moves).
#[derive(Clone)]
pub struct AuthContext {
    // None when the API runs open (no keys configured)
    pub key_name: Option<String>,
    pub permissions: Permissions,
}

// The registry, injected as a router Extension. The middleware clones the
// Arc (a pointer copy) on every request and reads the current key list
// through it, so a reload that replaces the list is picked up without
// rebuilding the router.
#[derive(Clone)]
pub struct ApiKeys(pub Arc<ApiKeyRegistry>);

// The credential a request offers, from either accepted header. Shared by the
// middleware and the /metrics handler, which enforces auth itself (its route
// is always public in the router so the flag can reload — see http::metrics).
pub fn presented_token(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        })
}

// The browser session behind this request, if it has one and the store is
// mounted. Returns the session's identity, never the session token.
fn session_auth(request: &Request) -> Option<crate::adapters::r#in::http::session::Authenticated> {
    use crate::adapters::r#in::http::session::{COOKIE_NAME, Sessions, cookie_value};

    let sessions = request.extensions().get::<Sessions>()?;
    let header = request
        .headers()
        .get(axum::http::header::COOKIE)?
        .to_str()
        .ok()?;
    sessions.0.get(cookie_value(header, COOKIE_NAME)?)
}

// Constant-time comparison per key, and no early break: the scan always
// visits every key so the response time does not reveal WHICH key matched,
// only that one did.
pub fn match_token<'k>(token: &str, keys: &'k [ResolvedApiKey]) -> Option<&'k ResolvedApiKey> {
    let mut matched: Option<&ResolvedApiKey> = None;
    for key in keys.iter() {
        // ct_eq always compares all bytes (if lengths match) — a normal ==
        // short-circuits on the first different byte and that time delta
        // leaks info to guess the secret byte-by-byte.
        if bool::from(token.as_bytes().ct_eq(key.secret.as_bytes())) {
            matched = Some(key);
        }
    }
    matched
}

// Errors as ApiError rather than a bare StatusCode: a middleware rejection
// renders through the same IntoResponse as a handler's, so a 401 carries the
// {"error": ...} body every other failure in the API carries.
pub async fn require_api_key(
    mut request: Request,
    next: Next,
) -> Result<Response, crate::adapters::r#in::http::error::ApiError> {
    let keys = request
        .extensions()
        .get::<ApiKeys>()
        .expect("ApiKeys extension missing")
        .0
        .load();

    // No keys configured = open API (main.rs warns loudly about this).
    // Everything is admin so handlers don't need a special "no auth" path.
    if keys.is_empty() {
        request.extensions_mut().insert(AuthContext {
            key_name: None,
            permissions: Permissions::Admin,
        });
        let mut response = next.run(request).await;
        // The console works here too. There is no ambient credential on an
        // open instance — nothing to forge, and nothing a CSRF token would
        // protect — so the pages get an identity with an empty token and their
        // action buttons render. Without this the browsable API silently lost
        // Sync and Evict on exactly the setup people try it on first, with
        // nothing on the page to say why.
        response.extensions_mut().insert(BrowserIdentity {
            key_name: "open (no API keys configured)".to_string(),
            csrf: String::new(),
        });
        return Ok(response);
    }

    // A browser session, when no header was offered. Checked second on
    // purpose: a request that carries an explicit key is a machine's, and it
    // should authenticate as that key even from inside a browser that happens
    // to hold a cookie.
    if presented_token(request.headers()).is_none()
        && let Some(auth) = session_auth(&request)
    {
        // CSRF, and only here. A cookie is an AMBIENT credential: the browser
        // attaches it to any request any page can cause, so a page on the
        // internet could otherwise make your browser POST a sync — or a
        // DELETE — to this API and succeed. SameSite=Strict on the cookie
        // blocks the cross-site navigation case, but that is one attribute
        // standing between an admin session and a write, so the token is
        // required as well.
        //
        // Header-authenticated requests skip this entirely: they carry nothing
        // ambient, there is nothing to forge, and demanding a token from them
        // would break every existing consumer on the next upgrade.
        // "Safe method" by intent, not by verb. GET is exempt because a GET
        // should not change anything — but this API has one that does:
        // `/dataset?refresh=true` runs a connector script. A read that can SSH
        // into a datacenter is a write for this purpose, and the ambient
        // credential makes it reachable from a page that merely gets an
        // operator to click a link.
        let side_effecting_read = request
            .uri()
            .query()
            .is_some_and(|q| q.split('&').any(|p| p == "refresh=true"));

        if side_effecting_read || !matches!(*request.method(), Method::GET | Method::HEAD) {
            let presented = request
                .headers()
                .get("x-csrf-token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .or_else(|| {
                    request
                        .extensions()
                        .get::<crate::adapters::r#in::http::session::FormControls>()
                        .and_then(|controls| controls.csrf.clone())
                });

            let ok = presented.is_some_and(|presented| {
                crate::adapters::r#in::http::session::csrf_matches(&auth.csrf, &presented)
            });

            if !ok {
                return Err(crate::adapters::r#in::http::error::ApiError::csrf_required());
            }
        }

        tracing::Span::current().record("key_name", auth.key_name.as_str());
        let identity = BrowserIdentity {
            key_name: auth.key_name.clone(),
            csrf: auth.csrf,
        };
        request.extensions_mut().insert(AuthContext {
            key_name: Some(auth.key_name),
            permissions: auth.permissions,
        });
        let mut response = next.run(request).await;
        // On the RESPONSE, not the request: the HTML layer runs OUTSIDE this
        // middleware, so by the time it can look, the request is gone. This is
        // how the page knows whose session it is rendering for, and which CSRF
        // token its action forms must carry.
        response.extensions_mut().insert(identity);
        return Ok(response);
    }

    let Some(token) = presented_token(request.headers()) else {
        return Err(crate::adapters::r#in::http::error::ApiError::missing_api_key());
    };

    match match_token(token, &keys) {
        Some(key) => {
            // The trace layer declared this span field Empty; filling it here
            // puts the authenticated key on the request's access-log line.
            // (An open API and a public route leave it empty — absence means
            // nobody authenticated, not somebody anonymous.)
            tracing::Span::current().record("key_name", key.name.as_str());
            request.extensions_mut().insert(AuthContext {
                key_name: Some(key.name.clone()),
                permissions: key.permissions.clone(),
            });
            Ok(next.run(request).await)
        }
        None => Err(crate::adapters::r#in::http::error::ApiError::invalid_api_key()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::http::StatusCode;
    use axum::middleware;
    use axum::routing::get;
    use tower::ServiceExt;

    // The handler echoes who authenticated, so tests can assert the
    // middleware attached the right AuthContext, not just that it let us in.
    async fn whoami(axum::Extension(auth): axum::Extension<AuthContext>) -> String {
        auth.key_name.unwrap_or_else(|| "open".to_string())
    }

    fn admin_key(name: &str, secret: &str) -> ResolvedApiKey {
        ResolvedApiKey {
            name: name.to_string(),
            secret: secret.to_string(),
            permissions: Permissions::Admin,
        }
    }

    fn test_app(keys: Vec<ResolvedApiKey>) -> Router {
        Router::new()
            .route("/protected", get(whoami))
            .layer(middleware::from_fn(require_api_key))
            .layer(axum::Extension(ApiKeys(Arc::new(ApiKeyRegistry::new(
                keys,
            )))))
    }

    async fn body_string(resp: Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn no_keys_configured_allows_all_as_admin() {
        let app = test_app(vec![]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "open");
    }

    #[tokio::test]
    async fn valid_bearer_token_passes() {
        let app = test_app(vec![admin_key("default", "secret123")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("authorization", "Bearer secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "default");
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = test_app(vec![admin_key("default", "secret123")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("authorization", "Bearer wrongtoken")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // A rejection carries the standard error body, never an empty one.
        let body = body_string(resp).await;
        assert!(body.contains("invalid API key"), "body: {}", body);
    }

    #[tokio::test]
    async fn missing_header_returns_401() {
        let app = test_app(vec![admin_key("default", "secret123")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // "Missing" names its fix — where to put the key — unlike "invalid".
        let body = body_string(resp).await;
        assert!(body.contains("missing API key"), "body: {}", body);
        assert!(body.contains("X-API-Key"), "body: {}", body);
    }

    #[tokio::test]
    async fn non_bearer_scheme_returns_401() {
        let app = test_app(vec![admin_key("default", "secret123")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("authorization", "Basic dXNlcjpwYXNz")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_x_api_key_header_passes() {
        let app = test_app(vec![admin_key("default", "secret123")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("x-api-key", "secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_x_api_key_returns_401() {
        let app = test_app(vec![admin_key("default", "secret123")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("x-api-key", "wrongkey")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn each_key_authenticates_as_itself() {
        let app = test_app(vec![admin_key("awx", "key-a"), admin_key("forms", "key-b")]);
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("x-api-key", "key-b")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "forms");
    }

    // A reload rewrites api_keys.yaml under a running process. The middleware
    // reads the registry per request rather than holding a list, so the new
    // set is in force on the very next one — without this, rotating a key
    // would be the one configuration change that still needed a restart.
    #[tokio::test]
    async fn replacing_the_registry_changes_who_authenticates() {
        let registry = Arc::new(ApiKeyRegistry::new(vec![admin_key("before", "secret-a")]));
        let app = Router::new()
            .route("/protected", get(whoami))
            .layer(middleware::from_fn(require_api_key))
            .layer(axum::Extension(ApiKeys(Arc::clone(&registry))));

        let ask = |secret: &str| {
            HttpRequest::builder()
                .uri("/protected")
                .header("x-api-key", secret)
                .body(Body::empty())
                .unwrap()
        };

        let resp = app.clone().oneshot(ask("secret-a")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        registry.replace(vec![admin_key("after", "secret-b")]);

        let resp = app.clone().oneshot(ask("secret-b")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(body_string(resp).await, "after");

        let resp = app.oneshot(ask("secret-a")).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "the replaced key must stop working"
        );
    }

    #[test]
    fn scoped_permissions_only_allow_listed_ids() {
        let perms = Permissions::Scoped {
            sources: ["src-a".to_string()].into_iter().collect(),
            endpoints: ["ep-a".to_string()].into_iter().collect(),
        };
        assert!(perms.allows_source("src-a"));
        assert!(!perms.allows_source("src-b"));
        assert!(perms.allows_endpoint("ep-a"));
        assert!(!perms.allows_endpoint("ep-b"));
    }

    #[test]
    fn admin_permissions_allow_everything() {
        assert!(Permissions::Admin.allows_source("anything"));
        assert!(Permissions::Admin.allows_endpoint("anything"));
    }
}
