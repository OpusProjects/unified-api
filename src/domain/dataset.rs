use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// A Dataset is a complete Ansible inventory, as produced by a connector.
// Serialize + Deserialize: can be converted to/from JSON/YAML in both directions.
// Clone: allows making copies of the struct (Rust defaults to moving, not copying).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Dataset {
    // HashMap<String, HostVars> = dict[str, HostVars] in Python
    // Maps hostname → its variables
    #[serde(default)]
    pub hostvars: HashMap<String, HostVars>,

    // HashMap<String, Group> = the inventory groups (dc06, oraclelinux8, etc.)
    #[serde(default)]
    pub groups: HashMap<String, Group>,

    // Hosts to delete — only the enricher uses this
    // Normal connectors do not return this
    #[serde(default)]
    pub remove_hosts: Vec<String>,
}

// A host's variables: a free key-value dictionary
// serde_json::Value is like "any" — can be string, number, bool, list, etc.
pub type HostVars = HashMap<String, serde_json::Value>;

// An Ansible inventory group
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Group {
    // Vec<String> = list[str] in Python
    // #[serde(default)] = if it doesn't appear in YAML/JSON, uses an empty Vec
    #[serde(default)]
    pub hosts: Vec<String>,

    #[serde(default)]
    pub children: Vec<String>,

    // Option<HostVars> = can exist or not (like Optional in Python)
    // None = has no group variables, Some({...}) = has them
    #[serde(default)]
    pub vars: Option<HostVars>,
}
