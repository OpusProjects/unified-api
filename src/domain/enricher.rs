use serde::Deserialize;
use std::collections::HashMap;

// An Enricher post-processes data that is already in cache.
// It receives the current dataset from a source, transforms it, and returns
// the modified hosts (merge) and/or hosts to delete.
#[derive(Debug, Deserialize, Clone)]
pub struct Enricher {
    pub name: String,

    // The source whose data it enriches — ex: "src-device42"
    pub source_id: String,

    // Script that performs the enrichment
    pub script_path: String,

    // CLI arguments passed to the script (default: none)
    #[serde(default)]
    pub script_args: Vec<String>,

    // Project whose checkout contains the script (None = script_path is a
    // plain filesystem path, absolute or relative to the working directory)
    #[serde(default)]
    pub project_id: Option<String>,

    // Automatic execution interval (0 or None = manual only)
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Free config for the script
    #[serde(default)]
    pub config: HashMap<String, String>,

    // Maximum seconds a run may take before it is aborted (default 300)
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,
}
