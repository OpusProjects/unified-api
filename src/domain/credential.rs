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

    // Our field name → the full name of the environment variable holding it.
    // ex: {"username": "SECTION9_USERNAME"} → reads SECTION9_USERNAME and
    // delivers it as `username`. The full name rather than a prefix, so a
    // search for the variable finds the credential that reads it.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,

    // DEPRECATED, still read: the older env form, a prefix joined to each
    // secret_keys value — "SECTION9" + {"username": "USERNAME"} reads
    // SECTION9_USERNAME. `deprecation` says what env_vars to write instead.
    pub env_prefix: Option<String>,

    // Path to a JSON file with secrets — ex: "/run/secrets/section9-api.json"
    pub secret_file: Option<String>,

    // Mapping: our name → field name in the store: the Vault secret, the JSON
    // file, or (deprecated) the suffix after env_prefix.
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
        } else if !self.env_vars.is_empty() || self.env_prefix.is_some() {
            "env"
        } else if self.secret_file.is_some() {
            "file"
        } else {
            "files"
        }
    }

    // The environment variables an env-backed credential reads, sorted and
    // spelled exactly as the env resolver reads them. Empty for any other
    // backend.
    pub fn env_var_names(&self) -> Vec<String> {
        if self.vault_path.is_some() {
            return Vec::new();
        }
        let mut vars: Vec<String> = if !self.env_vars.is_empty() {
            self.env_vars.values().cloned().collect()
        } else if let Some(prefix) = &self.env_prefix {
            self.secret_keys
                .values()
                .map(|suffix| format!("{}_{}", prefix, suffix))
                .collect()
        } else {
            return Vec::new();
        };
        vars.sort();
        vars
    }

    // The secret fields it delivers (not its file_keys), sorted.
    pub fn fields(&self) -> Vec<String> {
        let names = if self.vault_path.is_none() && !self.env_vars.is_empty() {
            self.env_vars.keys()
        } else {
            self.secret_keys.keys()
        };
        let mut fields: Vec<String> = names.cloned().collect();
        fields.sort();
        fields
    }

    // What the item says that cannot mean anything: two env forms at once, or
    // secret_keys with nowhere to read them from.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if !self.env_vars.is_empty() && self.env_prefix.is_some() {
            errors.push(format!(
                "Credential '{id}' sets both env_vars and env_prefix — keep env_vars, \
                 which names each variable in full"
            ));
        }
        if !self.env_vars.is_empty()
            && !self.secret_keys.is_empty()
            && self.vault_path.is_none()
            && self.secret_file.is_none()
        {
            errors.push(format!(
                "Credential '{id}' sets secret_keys beside env_vars — env_vars already \
                 maps each field to its variable; remove secret_keys"
            ));
        }
        errors
    }

    // The deprecated env form, and the env_vars that replace it, written out.
    pub fn deprecation(&self, id: &str) -> Option<String> {
        let prefix = self.env_prefix.as_ref()?;
        if self.vault_path.is_some() || !self.env_vars.is_empty() {
            return None;
        }
        let mut pairs: Vec<String> = self
            .secret_keys
            .iter()
            .map(|(field, suffix)| format!("{field}: {prefix}_{suffix}"))
            .collect();
        pairs.sort();
        Some(format!(
            "credential '{id}': env_prefix + secret_keys is deprecated and will be refused \
             in a later release; write env_vars: {{{}}}",
            pairs.join(", ")
        ))
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
        assert_eq!(c.env_var_names(), vec!["D42_PASS", "D42_USER"]);
        assert_eq!(
            c.deprecation("cred-d42").unwrap(),
            "credential 'cred-d42': env_prefix + secret_keys is deprecated and will be \
             refused in a later release; write env_vars: {password: D42_PASS, username: D42_USER}"
        );
    }

    #[test]
    fn env_vars_name_each_variable_in_full() {
        let c = credential(
            "name: x\ntype: username_password\nenv_vars:\n  username: D42_USER\n  password: D42_PASS\n",
        );
        assert_eq!(c.backend(), "env");
        assert_eq!(c.env_var_names(), vec!["D42_PASS", "D42_USER"]);
        assert_eq!(c.fields(), vec!["password", "username"]);
        assert!(c.deprecation("cred-d42").is_none());
        assert!(c.errors("cred-d42").is_empty());
    }

    #[test]
    fn two_env_forms_at_once_are_refused() {
        let c = credential(
            "name: x\ntype: token\nenv_vars:\n  token: GH_TOKEN\nenv_prefix: GH\nsecret_keys:\n  token: TOKEN\n",
        );
        let errors = c.errors("cred-gh");
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(errors[0].contains("both env_vars and env_prefix"));
        assert!(errors[1].contains("secret_keys beside env_vars"));
    }

    #[test]
    fn a_vault_path_wins_and_reads_no_env_vars() {
        let c = credential(
            "name: x\ntype: token\nvault_path: pcd/github\nenv_prefix: GH\nsecret_keys:\n  token: TOKEN\n",
        );
        assert_eq!(c.backend(), "vault");
        assert!(c.env_var_names().is_empty());
    }
}
