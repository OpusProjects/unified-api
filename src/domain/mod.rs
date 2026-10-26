pub mod api_key;
pub mod cache_entry;
pub mod connector;
pub mod credential;
pub mod dataset;
pub mod endpoint;
pub mod enricher;
pub mod group_trim;
pub mod host_filter;
pub mod ldap;
pub mod project;
pub mod source;
pub mod static_inventory;
pub mod sync_health;
pub mod sync_mode;
pub mod topology;
pub mod var_path;
pub mod view;
pub mod yaml;

// Default execution timeout for connector/enricher/output scripts.
// Shared by the three config types via #[serde(default = "...")].
pub fn default_timeout_seconds() -> u64 {
    300
}
