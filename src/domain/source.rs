use serde::Deserialize;
use std::collections::HashMap;

use super::sync_mode::SyncMode;

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorType {
    #[default]
    Script,
    Ssh,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Source {
    pub name: String,
    pub project_id: String,
    pub script_path: String,

    // CLI arguments passed to the script, e.g. ["--list"] for scripts that
    // implement the Ansible dynamic inventory interface. For SSH sources in
    // `script` gather mode they are appended to the remote command.
    #[serde(default)]
    pub script_args: Vec<String>,

    #[serde(default)]
    pub connector_type: ConnectorType,

    // Maximum seconds a sync may run before it is aborted — protects the
    // scheduler and API from a hung connector script (default 300)
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,

    #[serde(default)]
    pub sync_mode: SyncMode,

    // Vec<String> = list of credential IDs that the connector needs
    #[serde(default)]
    pub credential_ids: Vec<String>,

    // Option<String> = may or may not have a cron schedule (reserved for future)
    pub schedule: Option<String>,

    // Automatic sync interval in seconds (simple alternative to cron)
    // If None or 0, no automatic sync is performed
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // TTL in seconds for this source's cache
    pub ttl_seconds: u64,

    // TTL overrides by group or by host
    #[serde(default)]
    pub ttl_overrides: TtlOverrides,

    // Free config for the connector (api_url, filters, etc.)
    #[serde(default)]
    pub config: HashMap<String, String>,
}

// TTL Overrides: you can give different TTLs to specific groups or hosts
#[derive(Debug, Deserialize, Clone, Default)]
pub struct TtlOverrides {
    // HashMap<String, u64> = dict[str, int] in Python
    // ex: {"production": 900} → the "production" group refreshes every 15 min
    #[serde(default)]
    pub groups: HashMap<String, u64>,

    // ex: {"critical-db01": 300} → this host refreshes every 5 min
    #[serde(default)]
    pub hosts: HashMap<String, u64>,
}
