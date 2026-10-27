use serde::{Deserialize, Serialize};

// Configuration shape of one API key — api_keys.yaml.
//
// The SECRET is never in the YAML: `env` names the environment variable that
// holds it, so the config file can live in git while the value comes from the
// deployment (a k8s Secret, Vault via ESO, a .env file...). Rotating a key is
// therefore an external process: swap the env var's value and restart — no
// config or code change.
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
// Here the stakes are highest: this file is the access-control list, and a
// silently dropped key means a permission the operator believes is in force
// and is not.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyDef {
    // The display label: the item's `description`, falling back to its
    // identity (see config::take). The identity is the map key — what
    // references, URLs and cache keys use.
    pub name: String,

    // Where the secret lives: `key` → the full name of the environment
    // variable holding it, the same shape a credential's env_vars has.
    #[serde(default)]
    pub env_vars: std::collections::HashMap<String, String>,

    // DEPRECATED, still read: the variable's name on its own. `deprecation`
    // says what to write instead.
    #[serde(default)]
    pub env: Option<String>,

    // admin = everything; restricted = only what `sources`/`endpoints` list
    #[serde(default)]
    pub role: ApiKeyRole,

    // Source ids this key may read and sync (restricted keys only)
    #[serde(default)]
    pub sources: Vec<String>,

    // Output endpoint ids this key may list and run (restricted keys only)
    #[serde(default)]
    pub endpoints: Vec<String>,
}

// The one field an API key's env_vars may hold.
const KEY_FIELD: &str = "key";

impl ApiKeyDef {
    // The variable holding the secret, whichever form names it. Load
    // validation (`errors`) guarantees exactly one does.
    pub fn env_var(&self) -> &str {
        self.env_vars
            .get(KEY_FIELD)
            .or(self.env.as_ref())
            .map(String::as_str)
            .unwrap_or_default()
    }

    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        let mut unknown: Vec<&String> = self
            .env_vars
            .keys()
            .filter(|field| *field != KEY_FIELD)
            .collect();
        unknown.sort();
        for field in unknown {
            errors.push(format!(
                "API key '{id}': env_vars.{field} is not a field of an API key — the only one is \
                 `key`, the variable holding its secret"
            ));
        }
        match (self.env_vars.contains_key(KEY_FIELD), self.env.is_some()) {
            (true, true) => errors.push(format!(
                "API key '{id}' sets both env_vars.key and env — keep env_vars.key"
            )),
            (false, false) => errors.push(format!(
                "API key '{id}' names no variable for its secret: set env_vars: {{key: <VAR>}}"
            )),
            _ => {}
        }
        errors
    }

    pub fn deprecation(&self, id: &str) -> Option<String> {
        let env = self.env.as_ref()?;
        Some(format!(
            "API key '{id}': env is deprecated and will be refused in a later release; \
             write env_vars: {{key: {env}}}"
        ))
    }
}

// `rename_all = "lowercase"` lets the YAML say `role: admin` instead of
// `role: Admin` — config files shouldn't care about Rust naming style.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyRole {
    Admin,
    // Restricted is the default: forgetting `role:` must never silently
    // hand out admin. #[default] marks which variant Default::default() picks.
    #[default]
    Restricted,
}
