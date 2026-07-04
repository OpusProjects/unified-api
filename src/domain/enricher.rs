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

    // Automatic execution interval (0 or None = manual only)
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Free config for the script
    #[serde(default)]
    pub config: HashMap<String, String>,
}
