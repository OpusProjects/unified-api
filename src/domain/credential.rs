// Credential and CredentialType live in unified-secrets (the crate every
// secrets adapter reads them from, shared with unified-gitops); this module
// re-exports them so the rest of the service keeps naming
// `crate::domain::credential::Credential`.
pub use unified_secrets::credential::{Credential, CredentialType, delivery_id, variable_part};

// What this service checks about a credential when it loads its
// configuration, in its own words. A trait because Credential is a type from
// another crate: Rust only lets a crate add inherent methods to its own types,
// so the service's rules arrive as an extension trait instead — bring
// `CredentialRules` into scope and `credential.errors(id)` works as before.
pub trait CredentialRules {
    fn fields(&self) -> Vec<String>;
    fn errors(&self, id: &str) -> Vec<String>;
    fn deprecation(&self, id: &str) -> Option<String>;
}

impl CredentialRules for Credential {
    // The secret fields it delivers (not its file_keys), sorted.
    fn fields(&self) -> Vec<String> {
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
    fn errors(&self, id: &str) -> Vec<String> {
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
    fn deprecation(&self, id: &str) -> Option<String> {
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
