// One error shape for the whole API: web-rs's ApiError and ErrorBody (the
// `web` repository), shared with unified-gitops. Every failure is a status and
// {"error": ...}, never a bare StatusCode with an empty body.
pub use web_rs::{ApiError, ErrorBody};

// The refusals this API's source routes share, worded once so they stay
// identical across handlers. A trait, because Rust allows inherent methods
// only where a type is defined: import it next to ApiError to write
// `ApiError::source_forbidden(id)`.
pub trait SourceErrors {
    // Shorthands for the three cases nearly every id route needs, so the
    // wording stays identical across handlers instead of drifting per file.
    fn source_forbidden(id: &str) -> ApiError;

    fn source_not_cached(id: &str) -> ApiError;

    fn source_not_configured(id: &str) -> ApiError;

    // Refusing a refresh is worth its own wording, because the two ways it can
    // be refused have completely different fixes and both used to be one bland
    // 403 or nothing at all.
    fn refresh_needs_hosts() -> ApiError;

    fn refresh_not_allowed(id: &str) -> ApiError;
}

impl SourceErrors for ApiError {
    fn source_forbidden(id: &str) -> ApiError {
        ApiError::forbidden(format!(
            "this API key is not allowed to access source '{}'",
            id
        ))
    }

    fn source_not_cached(id: &str) -> ApiError {
        ApiError::not_found(format!(
            "source '{}' is not in the cache (never synced, or evicted)",
            id
        ))
    }

    fn source_not_configured(id: &str) -> ApiError {
        ApiError::not_found(format!("source '{}' is not configured", id))
    }

    fn refresh_needs_hosts() -> ApiError {
        ApiError::bad_request(
            "refresh=true requires ?host=: a refresh of a whole source on a read \
             would gather the entire inventory, so the hosts have to be named. \
             POST the source's /sync endpoint for a full refresh.",
        )
    }

    fn refresh_not_allowed(id: &str) -> ApiError {
        ApiError::forbidden(format!(
            "source '{}' does not allow on-demand refresh — set \
             allow_on_demand_refresh: true on it to let a read trigger a gather",
            id
        ))
    }
}
