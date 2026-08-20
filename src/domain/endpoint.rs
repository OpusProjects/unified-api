use serde::Deserialize;
use std::collections::HashMap;

// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OutputEndpoint {
    pub name: String,

    // Which sources feed this endpoint
    #[serde(default)]
    pub source_ids: Vec<String>,

    // How the datasets become the response. Exactly one of these is set
    // (enforced in config validation, like an enricher's source_id/script_path):
    //   output      — a builtin, in-process transformer, e.g. `output: ansible`
    //   script_path — an external transformer script (the pluggable path)
    #[serde(default)]
    pub output: Option<OutputFormat>,

    // Script that transforms the datasets into the final format
    #[serde(default)]
    pub script_path: Option<String>,

    // CLI arguments passed to the script (script_path only; default: none)
    #[serde(default)]
    pub script_args: Vec<String>,

    // Project whose checkout contains the script (script_path only; None = the
    // path is a plain filesystem path, absolute or relative to the working dir)
    #[serde(default)]
    pub project_id: Option<String>,

    // Free config for the transformer (builtin or script)
    #[serde(default)]
    pub config: HashMap<String, String>,

    // Maximum seconds the script may take before it is aborted (script_path
    // only — a builtin runs in-process). Default 300.
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,
}

// A builtin, in-process transformer — no script, no spawned interpreter. The
// common output formats live in the binary so the hot path (an AWX inventory
// refresh) need not fork one every request, and so a config typo fails at load
// naming the field. A bespoke format still uses `script_path`.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    // `output: ansible` — merge the sources and render Ansible dynamic
    // inventory (`_meta.hostvars` plus one key per group).
    Ansible,
}
