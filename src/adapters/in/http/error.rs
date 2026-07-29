use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use utoipa::ToSchema;

// One error shape for the whole API.
//
// Handlers used to return a bare StatusCode, which axum renders with an EMPTY
// body. A consumer receiving 404 from /dataset could not tell "this source is
// not in the cache yet" from "this source id does not exist", and a 403 said
// nothing about which permission was missing — the information existed at the
// point of failure and was thrown away on the way out.
//
// The output endpoints already answered with {"error": ...}, so this is the
// shape consumers parse; the rest of the API now agrees with it.

// The wire format. Separate from ApiError because the status code travels in
// the HTTP status, not in the body.
#[derive(Debug, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Human-readable explanation. Stable enough to log, not to match on.
    pub error: String,
}

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    // Shorthands for the three cases nearly every id route needs, so the
    // wording stays identical across handlers instead of drifting per file.
    pub fn source_forbidden(id: &str) -> Self {
        Self::forbidden(format!(
            "this API key is not allowed to access source '{}'",
            id
        ))
    }

    pub fn source_not_cached(id: &str) -> Self {
        Self::not_found(format!(
            "source '{}' is not in the cache (never synced, or evicted)",
            id
        ))
    }

    pub fn source_not_configured(id: &str) -> Self {
        Self::not_found(format!("source '{}' is not configured", id))
    }

    pub fn admin_only() -> Self {
        Self::forbidden("this route requires an admin API key")
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    // Refusing a refresh is worth its own wording, because the two ways it can
    // be refused have completely different fixes and both used to be one bland
    // 403 or nothing at all.
    pub fn refresh_needs_hosts() -> Self {
        Self::bad_request(
            "refresh=true requires ?host=: a refresh of a whole source on a read \
             would gather the entire inventory, so the hosts have to be named. \
             POST the source's /sync endpoint for a full refresh.",
        )
    }

    pub fn refresh_not_allowed(id: &str) -> Self {
        Self::forbidden(format!(
            "source '{}' does not allow on-demand refresh — set \
             allow_on_demand_refresh: true on it to let a read trigger a gather",
            id
        ))
    }
}

// Implementing IntoResponse is what lets a handler return
// Result<T, ApiError>: axum calls this to turn the error into a response.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}
