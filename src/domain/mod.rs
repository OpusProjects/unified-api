pub mod api_key;
pub mod cache_entry;
pub mod credential;
pub mod dataset;
pub mod endpoint;
pub mod enricher;
pub mod project;
pub mod source;
pub mod sync_mode;

// Default execution timeout for connector/enricher/output scripts.
// Shared by the three config types via #[serde(default = "...")].
pub fn default_timeout_seconds() -> u64 {
    300
}
