use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use axum::extract::Request;
use axum::http::StatusCode;
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

pub async fn require_api_key(mut request: Request, next: Next) -> Result<Response, StatusCode> {
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
        return Ok(next.run(request).await);
    }

    let token = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .or_else(|| {
            request
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.strip_prefix("Bearer "))
        });

    let Some(token) = token else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    // Constant-time comparison per key (see the ct_eq note below), and no
    // early break: the scan always visits every key so the response time does
    // not reveal WHICH key matched, only that one did.
    let mut matched: Option<&ResolvedApiKey> = None;
    for key in keys.iter() {
        // ct_eq always compares all bytes (if lengths match) — a normal ==
        // short-circuits on the first different byte and that time delta
        // leaks info to guess the secret byte-by-byte.
        if bool::from(token.as_bytes().ct_eq(key.secret.as_bytes())) {
            matched = Some(key);
        }
    }

    match matched {
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
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
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
