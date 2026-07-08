use serde::Deserialize;

// Configuration shape of one API key — api_keys.yaml.
//
// The SECRET is never in the YAML: `env` names the environment variable that
// holds it, so the config file can live in git while the value comes from the
// deployment (a k8s Secret, Vault via ESO, a .env file...). Rotating a key is
// therefore an external process: swap the env var's value and restart — no
// config or code change.
#[derive(Debug, Deserialize, Clone)]
pub struct ApiKeyDef {
    pub name: String,

    // Environment variable that holds the secret value of this key
    pub env: String,

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

// `rename_all = "lowercase"` lets the YAML say `role: admin` instead of
// `role: Admin` — config files shouldn't care about Rust naming style.
#[derive(Debug, Deserialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyRole {
    Admin,
    // Restricted is the default: forgetting `role:` must never silently
    // hand out admin. #[default] marks which variant Default::default() picks.
    #[default]
    Restricted,
}
