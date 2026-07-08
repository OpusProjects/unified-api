use std::collections::HashSet;
use std::sync::Arc;

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

// The full set of configured keys, injected as a router Extension.
// Arc<[...]> instead of Vec: the middleware clones this on every request, and
// cloning an Arc is a pointer copy instead of a Vec deep-copy.
#[derive(Clone)]
pub struct ApiKeys(pub Arc<[ResolvedApiKey]>);

pub async fn require_api_key(mut request: Request, next: Next) -> Result<Response, StatusCode> {
    let keys = request
        .extensions()
        .get::<ApiKeys>()
        .expect("ApiKeys extension missing")
        .clone();

    // No keys configured = open API (main.rs warns loudly about this).
    // Everything is admin so handlers don't need a special "no auth" path.
    if keys.0.is_empty() {
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
    for key in keys.0.iter() {
        // ct_eq always compares all bytes (if lengths match) — a normal ==
        // short-circuits on the first different byte and that time delta
        // leaks info to guess the secret byte-by-byte.
        if bool::from(token.as_bytes().ct_eq(key.secret.as_bytes())) {
            matched = Some(key);
        }
    }

    match matched {
        Some(key) => {
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
            .layer(axum::Extension(ApiKeys(keys.into())))
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
