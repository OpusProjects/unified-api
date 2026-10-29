use serde::Deserialize;

// A GitProject is a reference to a git repo that contains
// connector scripts and/or transformations.
// A single project can have multiple scripts inside
// (ex: device42/fetch.py, vmware/fetch.py, outputs/format.py)
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GitProject {
    // The display label: the item's `description`, falling back to its
    // identity (see config::take). The identity is the map key — what
    // references, URLs and cache keys use.
    pub name: String,
    pub git_url: String,

    // Branch to clone/pull — if not specified, "main"
    #[serde(default = "default_branch")]
    pub branch: String,

    // Credentials for a private repo (GitHub token, SSH key, etc.):
    // `credentials:`, a list as on every other item, holding at most one —
    // a clone authenticates with one. Absent or empty = a public repo.
    #[serde(default)]
    pub credentials: Vec<String>,

    // DEPRECATED, still read: the singular `credential: <id>`. `deprecation`
    // says what to write instead. Read through `credential_id`.
    #[serde(default)]
    pub credential: Option<String>,

    // Seconds between periodic re-pulls (0 or None = no periodic sync).
    // Same convention as sources and enrichers.
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // Cron cadence (UTC, 5-field + optional seconds) as the alternative to
    // the interval — same semantics as a source's schedule. The boot clone
    // still happens regardless: cron paces the RE-pulls.
    #[serde(default)]
    pub schedule: Option<String>,

    // Maximum seconds a clone/pull may take before it is aborted (default 300,
    // same convention as sources and enrichers). Bounds whatever awaits the
    // sync — an unreachable remote used to hang git (and its caller) forever.
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,

    // Build and maintain a Python virtualenv from the checkout's
    // requirements.txt (default false). The venv lives OUTSIDE the checkout
    // (`<projects.dir>/.venvs/<project>`), is refreshed after every pull
    // whose requirements.txt changed, and its bin/ is prepended to PATH when
    // this project's scripts run — so a `#!/usr/bin/env python3` shebang
    // resolves to the venv's interpreter and pip-installed imports work.
    // A failing install fails the project sync, visibly in its sync_health.
    #[serde(default)]
    pub python_venv: bool,

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

impl GitProject {
    // The credential it clones and pulls with, whichever form names it. Load
    // validation (`errors`) guarantees at most one does.
    pub fn credential_id(&self) -> Option<&str> {
        self.credentials
            .first()
            .or(self.credential.as_ref())
            .map(String::as_str)
    }

    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        // Distinct ones: a credential listed twice is one credential, as on
        // a source.
        let distinct: std::collections::HashSet<&String> = self.credentials.iter().collect();
        if distinct.len() > 1 {
            errors.push(format!(
                "Project '{id}' lists {} credentials — a clone authenticates with one",
                distinct.len()
            ));
        }
        if !self.credentials.is_empty() && self.credential.is_some() {
            errors.push(format!(
                "Project '{id}' sets both credentials and credential — keep credentials"
            ));
        }
        errors
    }

    pub fn deprecation(&self, id: &str) -> Option<String> {
        let credential = self.credential.as_ref()?;
        Some(format!(
            "project '{id}': credential is deprecated and will be refused in a later \
             release; write credentials: [{credential}]"
        ))
    }
}

// Function that returns the default value for branch.
// #[serde(default = "function_name")] calls this function
// when the field does not appear in the YAML.
fn default_branch() -> String {
    "main".to_string()
}

impl GitProject {
    // The part of the project git needs: where to pull from, and which branch.
    pub fn remote(&self) -> unified_git::Remote {
        unified_git::Remote {
            url: self.git_url.clone(),
            branch: self.branch.clone(),
        }
    }
}
