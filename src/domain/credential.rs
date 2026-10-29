use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    UsernamePassword,
    Token,
    SshKey,
    // Not a login a script uses: the connection to a Vault that other
    // credentials read from (they name it with `vault:`), as AWX models a
    // HashiCorp Vault lookup as a credential of its own.
    Vault,
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

    // KV v2 path (under the Vault's `mount`) to read this credential from.
    // When set, `secret_keys` maps our names to fields of that secret (empty
    // = every field verbatim) and env_vars/env_prefix/secret_file are not
    // consulted. `vault` names the `type: vault` credential to read it from
    // (absent = the deprecated `secrets.vault:` block in config.yaml) —
    // validated at startup, so a vault_path without a Vault fails the deploy
    // rather than the first sync.
    #[serde(default)]
    pub vault_path: Option<String>,

    #[serde(default)]
    pub vault: Option<String>,

    // A `type: vault` credential's own settings: where the Vault is and how
    // to log in to it (a token in env_vars.token, or Kubernetes auth). Refused
    // on any other type.
    #[serde(default)]
    pub address: Option<String>,

    // The KV v2 mount the vault_path values live under (default "secret")
    #[serde(default)]
    pub mount: Option<String>,

    // Kubernetes auth: the pod's service-account JWT (at jwt_path) is
    // exchanged for a client token under this role, instead of env_vars.token.
    #[serde(default)]
    pub kubernetes_role: Option<String>,

    #[serde(default)]
    pub jwt_path: Option<String>,

    // Bound on every request to this Vault (default 10)
    #[serde(default)]
    pub timeout_seconds: Option<u64>,

    // How long a value read from this Vault is reused (default 60; 0 = read
    // on every resolution) — also how long a rotation takes to be seen. Only
    // a Vault has one: env vars cannot change under a running process, and a
    // file is a cheap local read that should pick a rotation up at once.
    #[serde(default)]
    pub cache_ttl_seconds: Option<u64>,

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
            CredentialType::Vault => "vault",
        }
    }
}

impl Credential {
    // Where the secret values are read from, in the order the resolver
    // chain decides it: a Vault path wins over everything, then env vars,
    // then a JSON file. "files" is a credential made only of file_keys
    // (paths handed to the script, an SSH key, a certificate).
    pub fn backend(&self) -> &'static str {
        if self.vault_path.is_some() || self.credential_type == CredentialType::Vault {
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
        let connection = [
            ("address", self.address.is_some()),
            ("mount", self.mount.is_some()),
            ("kubernetes_role", self.kubernetes_role.is_some()),
            ("jwt_path", self.jwt_path.is_some()),
            ("timeout_seconds", self.timeout_seconds.is_some()),
            ("cache_ttl_seconds", self.cache_ttl_seconds.is_some()),
        ];
        if self.credential_type == CredentialType::Vault {
            match &self.address {
                None => errors.push(format!(
                    "Credential '{id}' is a Vault but has no address (https://vault.example.com:8200)"
                )),
                Some(a) if !(a.starts_with("https://") || a.starts_with("http://")) => errors
                    .push(format!(
                        "Credential '{id}': address '{a}' must be an http:// or https:// address"
                    )),
                Some(_) => {}
            }
            for field in self.env_vars.keys().filter(|f| *f != "token") {
                errors.push(format!(
                    "Credential '{id}': env_vars.{field} is not a field of a Vault — the only \
                     one is `token`, the variable holding its token"
                ));
            }
            match (
                self.env_vars.contains_key("token"),
                self.kubernetes_role.is_some(),
            ) {
                (true, true) => errors.push(format!(
                    "Credential '{id}' logs in to Vault both with env_vars.token and \
                     kubernetes_role — keep one"
                )),
                (false, false) => errors.push(format!(
                    "Credential '{id}' has no way to log in to Vault: set env_vars: {{token: \
                     <VAR>}} or kubernetes_role"
                )),
                _ => {}
            }
            for (key, set) in [
                ("vault_path", self.vault_path.is_some()),
                ("vault", self.vault.is_some()),
                ("env_prefix", self.env_prefix.is_some()),
                ("secret_file", self.secret_file.is_some()),
                ("secret_keys", !self.secret_keys.is_empty()),
                ("file_keys", !self.file_keys.is_empty()),
            ] {
                if set {
                    errors.push(format!(
                        "Credential '{id}' is a Vault and sets {key}, which only the \
                         credentials reading from a Vault use"
                    ));
                }
            }
            if self.timeout_seconds == Some(0) {
                errors.push(format!(
                    "Credential '{id}': timeout_seconds 0 would fail every request to this \
                     Vault — give it a bound in seconds (default 10)"
                ));
            }
            if self.jwt_path.is_some() && self.kubernetes_role.is_none() {
                errors.push(format!(
                    "Credential '{id}' sets jwt_path without kubernetes_role — it is only \
                     read for Kubernetes auth"
                ));
            }
            return errors;
        }
        for (key, set) in connection {
            if set {
                errors.push(format!(
                    "Credential '{id}' sets {key}, which only a `type: vault` credential uses"
                ));
            }
        }
        if self.vault.is_some() && self.vault_path.is_none() {
            errors.push(format!(
                "Credential '{id}' names vault '{}' but no vault_path to read there",
                self.vault.as_deref().unwrap_or_default()
            ));
        }
        if !self.env_vars.is_empty() && self.env_prefix.is_some() {
            errors.push(format!(
                "Credential '{id}' sets both env_vars and env_prefix — keep env_vars, \
                 which names each variable in full"
            ));
        }
        // One credential reads from one place. env_vars beside a vault_path
        // or a secret_file used to load, and one side was silently never
        // read (Vault wins over env_vars; env_vars over the file).
        if !self.env_vars.is_empty() {
            for (key, set) in [
                ("vault_path", self.vault_path.is_some()),
                ("secret_file", self.secret_file.is_some()),
                ("secret_keys", !self.secret_keys.is_empty()),
            ] {
                if set {
                    errors.push(format!(
                        "Credential '{id}' sets {key} beside env_vars — a credential reads \
                         from one place: env_vars names each variable, so keep it or {key}, \
                         not both"
                    ));
                }
            }
        } else {
            // The same rule for the older forms: a Vault path wins over the
            // env prefix, which wins over the file, and the loser was never
            // read, silently.
            let places: Vec<&str> = [
                ("vault_path", self.vault_path.is_some()),
                ("env_prefix", self.env_prefix.is_some()),
                ("secret_file", self.secret_file.is_some()),
            ]
            .into_iter()
            .filter_map(|(key, set)| set.then_some(key))
            .collect();
            if places.len() > 1 {
                errors.push(format!(
                    "Credential '{id}' reads from {} — a credential reads from one place: \
                     keep one",
                    places.join(" and ")
                ));
            }
        }
        errors
    }

    // The deprecated env form, and the env_vars that replace it, written out.
    pub fn deprecation(&self, id: &str) -> Option<String> {
        if self.vault_path.is_some() && self.vault.is_none() {
            return Some(format!(
                "credential '{id}': a vault_path without `vault:` reads from config.yaml's \
                 secrets.vault, which is deprecated and will be refused in a later release; \
                 declare the Vault as a `type: vault` credential and write vault: <its name>"
            ));
        }
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

// A credential's name as the part of a variable name its fields are also
// delivered under (CREDENTIAL_<ID>_<FIELD>): lower case, a leading `cred-` or
// `cred_` (the naming convention every example uses) dropped, and anything
// that cannot appear in an environment variable name written as `_`.
// `cred-vcenter-aa1` → `vcenter_aa1`. Also what config validation compares,
// so two credentials of one source can never be delivered under one id.
pub fn delivery_id(credential_id: &str) -> String {
    let id = variable_part(credential_id);
    id.strip_prefix("cred_").map(str::to_string).unwrap_or(id)
}

// Any name as it reads inside a variable name: lower case, anything outside
// [a-z0-9] as `_`. A field is compared with a delivery id in this form, so
// `AVAMAR_AA1_URL` and `avamar_aa1_url` are the same field.
pub fn variable_part(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
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

    // One credential reads from one place: env_vars beside a file or a Vault
    // path used to load with one side silently never read.
    #[test]
    fn env_vars_beside_a_file_or_a_vault_path_are_refused() {
        let c = credential(
            "name: x\ntype: token\nenv_vars:\n  token: T\nsecret_file: /run/x.json\nvault_path: a/b\n",
        );
        let errors = c.errors("cred-x");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets vault_path beside env_vars")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets secret_file beside env_vars")),
            "{errors:?}"
        );
    }

    #[test]
    fn the_older_forms_read_from_one_place_too() {
        let c = credential(
            "name: x\ntype: token\nvault_path: a/b\nenv_prefix: GH\nsecret_file: /f.json\nsecret_keys:\n  token: TOKEN\n",
        );
        let errors = c.errors("cred-x");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("reads from vault_path and env_prefix and secret_file")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_vault_with_a_zero_timeout_is_refused() {
        let c = credential(
            "name: v\ntype: vault\naddress: https://v\nenv_vars:\n  token: T\ntimeout_seconds: 0\n",
        );
        assert!(
            c.errors("cred-v")
                .iter()
                .any(|e| e.contains("timeout_seconds 0"))
        );
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
