use std::collections::HashSet;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

use crate::adapters::r#in::http::error::ApiError;
use crate::adapters::r#in::http::session::COOKIE_NAME;

// API keys, the middleware that checks them and the browser sessions behind
// it come from unified-auth, shared with unified-gitops; what stays here is
// what only this service has: what a key may touch (Permissions), how
// api_keys.yaml becomes keys, and the error shape a refusal renders in.
//
// The shared types are generic over the permissions type. These aliases fix
// it to ours, so the rest of the service names them as before.
pub use unified_auth::http::{AuthMethod, BrowserIdentity, match_token, presented_token};

// A configured key with its secret already resolved from the environment
// (main.rs does the resolution at the boundary; nothing here touches env vars).
// Its `id` is its `name` in the api_keys item — what a peer is recorded under
// when it pushes the federation map; its `name` is the label.
pub type ResolvedApiKey = unified_auth::http::ResolvedApiKey<Permissions>;
pub type ApiKeyRegistry = unified_auth::http::ApiKeyRegistry<Permissions>;
pub type ApiKeys = unified_auth::http::ApiKeys<Permissions>;
pub type AuthContext = unified_auth::http::AuthContext<Permissions>;

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

// What an instance with no API keys grants every caller.
impl unified_auth::http::Permission for Permissions {
    fn admin() -> Self {
        Permissions::Admin
    }
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
        let var = def.env_var();
        let secret = std::env::var(var).map_err(|_| {
            format!(
                "API key '{}' expects the secret in env var '{}', which is not set",
                id, var
            )
        })?;
        if secret.is_empty() {
            return Err(format!(
                "API key '{}': env var '{}' is set but empty",
                id, var
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
            id: id.clone(),
            name: def.name.clone(),
            secret,
            permissions,
        });
    }

    if let Ok(secret) = std::env::var("UNIFIED_API_KEY")
        && !secret.is_empty()
    {
        keys.push(ResolvedApiKey {
            id: "default".to_string(),
            name: "default".to_string(),
            secret,
            permissions: Permissions::Admin,
        });
    }

    Ok(keys)
}

// LDAP login without a single API key is refused outright. With no keys the
// middleware below runs the API OPEN (every caller admin) and never consults
// a session, so an "LDAP-only" instance would show a sign-in page that
// anyone could walk past. Checked where keys are resolved: at start-up (the
// process will not run) and before a reload is committed (it is refused).
pub fn ldap_requires_keys(
    cfg: &crate::config::AppConfig,
    keys: &[ResolvedApiKey],
) -> Result<(), String> {
    if cfg.server.ui.ldap.is_some() && keys.is_empty() {
        return Err(
            "server.ui.ldap is configured but no API key is: without one \
                    the API runs with no authentication at all, and the LDAP sign-in \
                    would protect nothing. Declare at least one key in api_keys.yaml \
                    (an admin key also stays the way in when the directory is down)"
                .to_string(),
        );
    }
    Ok(())
}

// Every request through the API, authenticated by unified-auth's middleware:
// a key in a header, a browser session, or — with no keys configured — every
// caller as admin. This wrapper passes it our session cookie and renders a
// refusal as an ApiError, so a 401 carries the {"error": ...} body every other
// failure in the API carries.
pub async fn require_api_key(request: Request, next: Next) -> Result<Response, ApiError> {
    unified_auth::http::require_api_key::<Permissions>(COOKIE_NAME, request, next)
        .await
        .map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    // An LDAP-only instance would run open (no key = no authentication), so
    // the combination is refused; one key is enough to close it.
    #[test]
    fn ldap_without_any_api_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  ui:\n    enabled: true\n    ldap:\n      \
             url: \"ldaps://ldap.example.com\"\n      bind_credential: cred-ldap\n      search_base: \"DC=x\"\n      \
             access:\n        - name: ops\n          groups: [DL-ops]\n          role: admin\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("credentials.yaml"),
            "credentials:\n  - name: cred-ldap\n    description: \"LDAP\"\n    type: username_password\n    env_prefix: \"LDAP\"\n",
        )
        .unwrap();
        let cfg = crate::config::load_config(dir.path().to_str().unwrap()).unwrap();

        let err = ldap_requires_keys(&cfg, &[]).expect_err("no key with LDAP is refused");
        assert!(err.contains("no authentication"), "{err}");

        let key = ResolvedApiKey {
            id: "admin".to_string(),
            name: "admin".to_string(),
            secret: "s".to_string(),
            permissions: Permissions::Admin,
        };
        assert!(ldap_requires_keys(&cfg, &[key]).is_ok());
    }
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::http::StatusCode;
    use axum::middleware;
    use axum::routing::get;
    use std::sync::Arc;
    use tower::ServiceExt;

    // The handler echoes who authenticated, so tests can assert the
    // middleware attached the right AuthContext, not just that it let us in.
    async fn whoami(axum::Extension(auth): axum::Extension<AuthContext>) -> String {
        auth.key_name.unwrap_or_else(|| "open".to_string())
    }

    fn admin_key(name: &str, secret: &str) -> ResolvedApiKey {
        ResolvedApiKey {
            id: name.to_string(),
            name: name.to_string(),
            secret: secret.to_string(),
            permissions: Permissions::Admin,
        }
    }

    fn test_app(keys: Vec<ResolvedApiKey>) -> Router {
        Router::new()
            .route("/protected", get(whoami))
            .layer(middleware::from_fn(require_api_key))
            .layer(axum::Extension(ApiKeys::new(Arc::new(
                ApiKeyRegistry::new(keys),
            ))))
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
            .layer(axum::Extension(ApiKeys::new(Arc::clone(&registry))));

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
