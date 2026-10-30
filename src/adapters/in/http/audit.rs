// One structured event per mutating operation: web-rs's audit::record (the
// `web` repository), generic over the permissions type and called here with
// this service's AuthContext. See web-rs's docs/usage.md for the fields.
pub use web_rs::audit::record;
