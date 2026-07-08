use serde::Deserialize;

// A GitProject is a reference to a git repo that contains
// connector scripts and/or transformations.
// A single project can have multiple scripts inside
// (ex: device42/fetch.py, vmware/fetch.py, outputs/format.py)
#[derive(Debug, Deserialize, Clone)]
pub struct GitProject {
    pub name: String,
    pub git_url: String,

    // Branch to clone/pull — if not specified, "main"
    #[serde(default = "default_branch")]
    pub branch: String,

    // Credential for private repos (GitHub token, SSH key, etc.)
    pub credential_id: Option<String>,

    // Seconds between periodic re-pulls (0 or None = no periodic sync).
    // Same convention as sources and enrichers.
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Update the checkout at boot? With `false` an EXISTING checkout is used
    // as-is (no network at startup) and updates happen only on demand
    // (POST /api/v1/projects/{id}/sync, e.g. from a pipeline) or on the
    // periodic interval. A MISSING checkout is always cloned regardless —
    // without the scripts there is nothing to execute.
    #[serde(default = "default_true")]
    pub sync_on_boot: bool,
}

fn default_true() -> bool {
    true
}

// Function that returns the default value for branch.
// #[serde(default = "function_name")] calls this function
// when the field does not appear in the YAML.
fn default_branch() -> String {
    "main".to_string()
}
