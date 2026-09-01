use serde::Deserialize;
use std::collections::HashMap;

// An Enricher post-processes data that is already in cache.
//
// Two modes:
// - Script-based: runs a script that receives the target dataset on stdin
//   and returns a partial dataset (modified/removed hosts).
// - Declarative merge: copies specified fields from one source's hostvars
//   into another — no script needed.
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Enricher {
    pub name: String,

    // The dataset being enriched — ex: "src-ssh-pq-facts"
    pub target_id: String,

    // Script that performs the enrichment (required for script-based mode)
    #[serde(default)]
    pub script_path: Option<String>,

    // CLI arguments passed to the script (default: none)
    #[serde(default)]
    pub script_args: Vec<String>,

    // Project whose checkout contains the script (None = script_path is a
    // plain filesystem path, absolute or relative to the working directory)
    #[serde(default)]
    pub project_id: Option<String>,

    // Declarative merge: the source dataset to copy fields from
    #[serde(default)]
    pub source_id: Option<String>,

    // Declarative merge: which keys to take from the source, on a group's vars
    // as well as a host's. A narrowing, so absent means every var the source
    // declares -- the way being in a group carries all of its vars in Ansible.
    // An empty list names nothing and takes nothing, which is not the same.
    #[serde(default)]
    pub fields: Option<Vec<String>>,

    // Automatic execution interval (0 or None = manual only)
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Cron cadence (UTC, 5-field + optional seconds) as the alternative to
    // the interval — same semantics as a source's schedule: validated at
    // startup, exclusive with a non-zero interval, no jitter, backoff by
    // letting occurrences pass.
    #[serde(default)]
    pub schedule: Option<String>,

    // Free config for the script
    #[serde(default)]
    pub config: HashMap<String, String>,

    // Maximum seconds a run may take before it is aborted (default 300)
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,
}

impl Enricher {
    pub fn is_declarative(&self) -> bool {
        self.source_id.is_some()
    }
}
