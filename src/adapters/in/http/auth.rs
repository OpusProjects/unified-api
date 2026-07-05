use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use subtle::ConstantTimeEq;

#[derive(Clone)]
pub struct ApiKey(pub Option<String>);

pub async fn require_api_key(request: Request, next: Next) -> Result<Response, StatusCode> {
    let api_key = request
        .extensions()
        .get::<ApiKey>()
        .expect("ApiKey extension missing");

    let expected = match &api_key.0 {
        Some(key) => key,
        None => return Ok(next.run(request).await),
    };

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

    // Constant-time comparison: a normal == would short-circuit on the first
    // different byte, and that time delta leaks info to guess the key byte-by-byte.
    // ct_eq always compares all bytes (if lengths match).
    match token {
        Some(t) if bool::from(t.as_bytes().ct_eq(expected.as_bytes())) => {
            Ok(next.run(request).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
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

    async fn ok_handler() -> &'static str {
        "ok"
    }

    fn test_app(api_key: Option<String>) -> Router {
        let key = ApiKey(api_key);
        Router::new()
            .route("/protected", get(ok_handler))
            .layer(middleware::from_fn(require_api_key))
            .layer(axum::Extension(key))
    }

    #[tokio::test]
    async fn no_key_configured_allows_all() {
        let app = test_app(None);
        let req = HttpRequest::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn valid_bearer_token_passes() {
        let app = test_app(Some("secret123".to_string()));
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("authorization", "Bearer secret123")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = test_app(Some("secret123".to_string()));
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
        let app = test_app(Some("secret123".to_string()));
        let req = HttpRequest::builder()
            .uri("/protected")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_bearer_scheme_returns_401() {
        let app = test_app(Some("secret123".to_string()));
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
        let app = test_app(Some("secret123".to_string()));
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
        let app = test_app(Some("secret123".to_string()));
        let req = HttpRequest::builder()
            .uri("/protected")
            .header("x-api-key", "wrongkey")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
}
