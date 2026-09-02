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

    // Which part of the merged inventory the endpoint returns. Unset (the
    // default) = all of it, which is what every endpoint did before limits
    // existed. See EndpointLimit.
    #[serde(default)]
    pub limit: Option<EndpointLimit>,

    // Maximum seconds the script may take before it is aborted (script_path
    // only — a builtin runs in-process, so setting this on one is a config
    // error like project_id, not a silent no-op). Unset = the shared default
    // (300), applied where the timeout is armed.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
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
    // `output: json` — the merged, filtered inventory in the raw source shape
    // (`hostvars` + `groups`), for consumers that want the data itself rather
    // than a tool's format.
    Json,
    // `output: csv` — one row per host, sorted; columns from the `columns`
    // config (default: every hostvar name seen). For spreadsheets and
    // importers that speak tables, not JSON.
    Csv,
}

// A constructed inventory: merge everything the endpoint's sources carry, then
// hand back only part of it.
//
// It lives in its own field rather than in the free-form `config:` map, and
// that is deliberate. The `config:` settings are transformer settings: they
// belong to a builtin, and a request may override any of them. A limit is
// neither. It applies before any transformer runs (a script gets limited
// datasets too, not just the builtins), and it decides the endpoint's SCOPE —
// which a caller must not be able to widen by adding a query parameter, since
// an endpoint is granted to keys that may not read its sources raw.
//
// "Various kinds of limit" is the expected shape here, so the rules are
// optional fields of one struct: adding a kind adds a field, and
// deny_unknown_fields still names a typo at load instead of ignoring it.
// Config validation rejects a limit that sets no rule at all.
#[derive(Debug, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct EndpointLimit {
    // Keep only the hosts that this source's dataset lists — the same hosts
    // `GET /sources/{id}/hosts` returns for it, which is how every other part
    // of the app defines "the hosts of a source".
    //
    // Every other source still contributes to those hosts: their variables,
    // their groups and their group membership all survive. What a source
    // cannot do under this limit is ADD a host of its own. The source named
    // must be one of the endpoint's `source_ids` (validated at load).
    #[serde(default)]
    pub by_hosts_from_inventory: Option<String>,
}

impl EndpointLimit {
    // A limit that names no rule is a config mistake, not "limit to nothing"
    // and not a no-op: whichever of the two we picked would be a guess about
    // intent. Config validation calls this and refuses to start.
    pub fn is_empty(&self) -> bool {
        self.by_hosts_from_inventory.is_none()
    }
}
