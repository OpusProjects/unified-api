use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone)]
pub struct OutputEndpoint {
    pub name: String,

    // Which sources feed this endpoint
    #[serde(default)]
    pub source_ids: Vec<String>,

    // Script that transforms the datasets into the final format
    pub script_path: String,

    // Project whose checkout contains the script (None = script_path is a
    // plain filesystem path, absolute or relative to the working directory)
    #[serde(default)]
    pub project_id: Option<String>,

    // Free config for the transformation script
    #[serde(default)]
    pub config: HashMap<String, String>,

    // Maximum seconds the transformer may take before it is aborted (default 300)
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,
}
