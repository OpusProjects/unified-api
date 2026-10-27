pub mod api_key;
pub mod cache_entry;
pub mod connector;
pub mod credential;
pub mod dataset;
pub mod endpoint;
pub mod enricher;
pub mod group_trim;
pub mod host;
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

// The wall clock now, in unix seconds: when a run started, as a history
// reports it. A clock before 1970 reads as 0 rather than failing a sync.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// Default execution timeout for connector/enricher/output scripts.
// Shared by the three config types via #[serde(default = "...")].
pub fn default_timeout_seconds() -> u64 {
    300
}
