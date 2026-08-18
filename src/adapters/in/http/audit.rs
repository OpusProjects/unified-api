use axum::http::HeaderMap;

use crate::adapters::r#in::http::auth::AuthContext;

// One structured event per mutating operation, emitted where "who" (the API
// key) and "what" (the action and its target) meet: the handler.
//
// The access log already records every request with its key_name and status;
// what it cannot say is what the request MEANT (a 200 on POST /sync is a
// datacenter gather) or how it ENDED beyond the status (a sync answers 200
// with success: false). This is that missing line, under its own tracing
// target so it can be filtered (`RUST_LOG=audit=info`), routed by a log
// pipeline, or grepped (`"audit"`) independently of log level tuning on the
// rest of the service.
//
// Only operations that RAN are recorded. Denied and misaddressed attempts
// (401/403/404) return before the action happens, and the access log line —
// which carries the same key_name and request_id — already tells that story;
// duplicating it here would make "appears in the audit log" ambiguous
// between "did it" and "tried it".
pub fn record(
    auth: &AuthContext,
    headers: &HeaderMap,
    action: &str,
    resource: &str,
    outcome: &str,
) {
    // "open" = the API is running without keys (main.rs warns loudly at boot);
    // there is no anonymous access once keys exist — auth rejects it first.
    let actor = auth.key_name.as_deref().unwrap_or("open");
    // The id the request-id layer assigned (or the caller sent): the join key
    // to the access log line and to the trigger a script saw.
    let request_id = headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    tracing::info!(
        target: "audit",
        actor,
        action,
        resource,
        request_id,
        outcome,
        "audit"
    );
}
