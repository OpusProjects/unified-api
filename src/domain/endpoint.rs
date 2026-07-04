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

    // Free config for the transformation script
    #[serde(default)]
    pub config: HashMap<String, String>,
}
