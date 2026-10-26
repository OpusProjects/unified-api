use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    UsernamePassword,
    Token,
    SshKey,
}

// Reference to a credential — comes from configuration YAML.
// Does not store secrets, only knows WHERE to read them from the environment.
// The infrastructure (ESO, docker secrets, .env) is responsible for injecting them.
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    // The display label: the item's `description`, falling back to its
    // identity (see config::take). The identity is the map key — what
    // references, URLs and cache keys use.
    pub name: String,

    #[serde(rename = "type")]
    pub credential_type: CredentialType,

    // KV v2 path (under `secrets.vault.mount`) to read this credential from
    // Vault. When set, `secret_keys` maps our names to fields of that secret
    // (empty = every field verbatim) and env_prefix/secret_file are not
    // consulted. Requires the `secrets.vault:` block in config.yaml —
    // validated at startup, so a vault_path without a Vault fails the deploy
    // rather than the first sync.
    #[serde(default)]
    pub vault_path: Option<String>,

    // Env vars prefix — ex: "SECTION9" → reads SECTION9_USERNAME, SECTION9_PASSWORD
    pub env_prefix: Option<String>,

    // Path to a JSON file with secrets — ex: "/run/secrets/section9-api.json"
    pub secret_file: Option<String>,

    // Mapping: our name → field name in env var or JSON
    // ex: {"username": "USERNAME", "password": "PASSWORD"}
    #[serde(default)]
    pub secret_keys: HashMap<String, String>,

    // Paths to files that the script consumes directly (SSH keys, certificates, etc.)
    // ex: {"ssh_key": "/run/secrets/id_rsa"} → CREDENTIAL_SSH_KEY_PATH=/run/secrets/id_rsa
    #[serde(default)]
    pub file_keys: HashMap<String, String>,
}

impl CredentialType {
    // As written after `type:` in credentials.yaml.
    pub fn name(&self) -> &'static str {
        match self {
            CredentialType::UsernamePassword => "username_password",
            CredentialType::Token => "token",
            CredentialType::SshKey => "ssh_key",
        }
    }
}

impl Credential {
    // Where the secret values are read from, in the order the resolver
    // chain decides it: a Vault path wins over everything, then env vars,
    // then a JSON file. "files" is a credential made only of file_keys
    // (paths handed to the script, an SSH key, a certificate).
    pub fn backend(&self) -> &'static str {
        if self.vault_path.is_some() {
            "vault"
        } else if self.env_prefix.is_some() {
            "env"
        } else if self.secret_file.is_some() {
            "file"
        } else {
            "files"
        }
    }

    // The environment variables an env-backed credential reads, sorted:
    // PREFIX_FIELD for each secret key, spelled exactly as the env resolver
    // spells them. Empty for any other backend.
    pub fn env_vars(&self) -> Vec<String> {
        let (Some(prefix), None) = (&self.env_prefix, &self.vault_path) else {
            return Vec::new();
        };
        let mut vars: Vec<String> = self
            .secret_keys
            .values()
            .map(|suffix| format!("{}_{}", prefix, suffix))
            .collect();
        vars.sort();
        vars
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(yaml: &str) -> Credential {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn env_vars_are_spelled_as_the_resolver_reads_them() {
        let c = credential(
            "name: x\ntype: username_password\nenv_prefix: D42\nsecret_keys:\n  username: USER\n  password: PASS\n",
        );
        assert_eq!(c.backend(), "env");
        assert_eq!(c.env_vars(), vec!["D42_PASS", "D42_USER"]);
    }

    #[test]
    fn a_vault_path_wins_and_reads_no_env_vars() {
        let c = credential(
            "name: x\ntype: token\nvault_path: pcd/github\nenv_prefix: GH\nsecret_keys:\n  token: TOKEN\n",
        );
        assert_eq!(c.backend(), "vault");
        assert!(c.env_vars().is_empty());
    }
}
