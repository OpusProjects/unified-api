use std::collections::HashMap;

use crate::domain::credential::Credential;
use crate::domain::ldap::LdapConfig;
use crate::domain::project::GitProject;
use crate::domain::source::Source;
use crate::ports::secrets::{SecretsError, SecretsPort};

// Resolves a list of credential_ids against the SecretsPort and combines
// all key-value pairs into a single HashMap, each field under two names:
// - `<id>_<field>` always (see `id_prefix`): `cred-vcenter-aa1`'s username is
//   `vcenter_aa1_username`, CREDENTIAL_VCENTER_AA1_USERNAME to a script, so a
//   source with one credential per site can tell them apart.
// - `<field>` only when no other of these credentials has that field. It used
//   to be every field, merged in order, and two credentials with a
//   `username` silently left the script one of them; now a shared field is
//   only reachable under its credential's id. The built-in consumers (ssh's
//   `username`, git's `token`, remote's `token`) take one credential and
//   keep reading the bare name.
//
// Receives &[String] (only the ids) and not an entire Source: it doesn't need more,
// and this way the scheduler doesn't have to fabricate a fake Source to call it.
//
// A resolution failure HALTS the use case and propagates to the caller. Previously it
// was swallowed with a warn! and continued with partial or empty credentials —
// the sync would later fail with a confusing connector error, or worse,
// "worked" without the expected authentication.
pub async fn resolve_credentials(
    secrets: &dyn SecretsPort,
    credential_ids: &[String],
) -> Result<HashMap<String, String>, SecretsError> {
    let mut resolved = Vec::with_capacity(credential_ids.len());
    for credential_id in credential_ids {
        match secrets.resolve(credential_id).await {
            Ok(fields) => resolved.push((credential_id, fields)),
            Err(e) => {
                return Err(SecretsError {
                    message: format!("credential '{}': {}", credential_id, e.message),
                });
            }
        }
    }

    // How many of these credentials have each field.
    let mut holders: HashMap<&str, usize> = HashMap::new();
    for (_, fields) in &resolved {
        for field in fields.keys() {
            *holders.entry(field.as_str()).or_default() += 1;
        }
    }

    let mut all_credentials = HashMap::new();
    for (credential_id, fields) in &resolved {
        let prefix = format!("{}_", id_prefix(credential_id));
        for (field, value) in fields {
            // A field already named after its credential (`avamar_aa1_url` on
            // cred-avamar-aa1, the hand-made form that predates this) is
            // not prefixed twice: scripts that discover sites by scanning
            // CREDENTIAL_AVAMAR_<SITE>_URL would otherwise find a second,
            // phantom site AA1_AVAMAR_AA1 and poll the same grid twice.
            let under_id = if field.starts_with(&prefix) {
                field.clone()
            } else {
                format!("{prefix}{field}")
            };
            all_credentials.insert(under_id, value.clone());
            if holders[field.as_str()] == 1 {
                all_credentials.insert(field.clone(), value.clone());
            }
        }
    }
    Ok(all_credentials)
}

// A credential id as a variable-name prefix: lower case, a leading `cred-`
// (the naming convention every example uses) dropped, and anything that
// cannot appear in an environment variable name written as `_`.
// `cred-vcenter-aa1` → `vcenter_aa1`.
fn id_prefix(credential_id: &str) -> String {
    let id = credential_id.to_lowercase();
    let id = id
        .strip_prefix("cred-")
        .or_else(|| id.strip_prefix("cred_"))
        .unwrap_or(&id);
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

// What names a credential in its configuration: the sources that pass it
// to their connector, the projects that clone with it, the directory
// login's service account, and — for a Vault — the credentials reading from
// it. Sorted, so a page and a diff of it are stable.
pub struct CredentialUsers {
    pub credentials: Vec<String>,
    pub sources: Vec<String>,
    pub projects: Vec<String>,
    pub ldap: bool,
}

pub fn users_of(
    credential_id: &str,
    credentials: &HashMap<String, Credential>,
    sources: &HashMap<String, Source>,
    projects: &HashMap<String, GitProject>,
    ldap: Option<&LdapConfig>,
) -> CredentialUsers {
    let mut source_ids: Vec<String> = sources
        .iter()
        .filter(|(_, source)| source.credential_ids.iter().any(|c| c == credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    source_ids.sort();
    let mut project_ids: Vec<String> = projects
        .iter()
        .filter(|(_, project)| project.credential_id.as_deref() == Some(credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    project_ids.sort();
    // A Vault's users are the credentials reading from it (their `vault:`).
    let mut credential_ids: Vec<String> = credentials
        .iter()
        .filter(|(_, c)| c.vault.as_deref() == Some(credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    credential_ids.sort();
    CredentialUsers {
        credentials: credential_ids,
        sources: source_ids,
        projects: project_ids,
        ldap: ldap.is_some_and(|l| l.bind_credential == credential_id),
    }
}

// Whether a credential's secrets resolve right now, asked exactly as a sync
// asks (through the same resolver chain and its cache), so "resolves" here
// means a sync will get them. None when it does; otherwise the resolver's
// own message, which names what is missing (the env var, the file, the
// Vault path) and never a value. The values themselves are dropped here.
pub async fn check(secrets: &dyn SecretsPort, credential_id: &str) -> Option<String> {
    secrets
        .resolve(credential_id)
        .await
        .err()
        .map(|e| e.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::secrets::mock::MockSecrets;

    fn ids(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    #[tokio::test]
    async fn each_field_is_also_delivered_under_its_credential() {
        let secrets = MockSecrets::new()
            .with(
                "cred-vcenter-aa1",
                [("username", "a"), ("url", "https://aa1")],
            )
            .with(
                "cred-vcenter-dc4",
                [("username", "d"), ("url", "https://dc4")],
            );
        let resolved =
            resolve_credentials(&secrets, &ids(&["cred-vcenter-aa1", "cred-vcenter-dc4"]))
                .await
                .unwrap();

        assert_eq!(resolved["vcenter_aa1_username"], "a");
        assert_eq!(resolved["vcenter_dc4_url"], "https://dc4");
        // Shared by both: only under each id, never one of them at random.
        assert!(!resolved.contains_key("username"));
        assert!(!resolved.contains_key("url"));
    }

    // The hand-made form: each site's fields already carry its name. Nothing
    // may change for a script scanning CREDENTIAL_AVAMAR_<SITE>_URL.
    #[tokio::test]
    async fn a_field_already_named_after_its_credential_is_not_prefixed_twice() {
        let secrets = MockSecrets::new()
            .with("cred-avamar-aa1", [("avamar_aa1_url", "https://aa1")])
            .with("cred-avamar-dc4", [("avamar_dc4_url", "https://dc4")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-avamar-aa1", "cred-avamar-dc4"]))
            .await
            .unwrap();

        let mut names: Vec<&String> = resolved.keys().collect();
        names.sort();
        assert_eq!(names, ["avamar_aa1_url", "avamar_dc4_url"]);
    }

    #[tokio::test]
    async fn a_field_no_other_credential_has_keeps_its_bare_name() {
        let secrets = MockSecrets::new()
            .with("cred-d42", [("username", "u"), ("password", "p")])
            .with("cred-fleet", [("ssh_key_path", "/k")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-d42", "cred-fleet"]))
            .await
            .unwrap();

        assert_eq!(resolved["username"], "u");
        assert_eq!(resolved["ssh_key_path"], "/k");
        assert_eq!(resolved["d42_password"], "p");
        assert_eq!(resolved["fleet_ssh_key_path"], "/k");
    }

    #[test]
    fn an_id_becomes_a_variable_prefix() {
        assert_eq!(id_prefix("cred-vcenter-aa1"), "vcenter_aa1");
        assert_eq!(id_prefix("cred_ucs.a6"), "ucs_a6");
        assert_eq!(id_prefix("Edge-PQ-DC4"), "edge_pq_dc4");
    }

    #[test]
    fn a_vaults_users_are_the_credentials_reading_from_it() {
        let credential = |yaml: &str| -> Credential { serde_saphyr::from_str(yaml).unwrap() };
        let credentials = HashMap::from([
            (
                "cred-vault".to_string(),
                credential("name: V\ntype: vault\naddress: https://v\nenv_vars:\n  token: T\n"),
            ),
            (
                "cred-d42".to_string(),
                credential("name: D\ntype: token\nvault: cred-vault\nvault_path: d42\n"),
            ),
        ]);
        let users = users_of(
            "cred-vault",
            &credentials,
            &HashMap::new(),
            &HashMap::new(),
            None,
        );
        assert_eq!(users.credentials, vec!["cred-d42"]);
    }

    #[tokio::test]
    async fn no_credential_ids_resolve_to_an_empty_map() {
        let secrets = MockSecrets::new();
        let resolved = resolve_credentials(&secrets, &[])
            .await
            .expect("empty is fine");
        assert!(resolved.is_empty());
    }

    // The property the 0.6-era rewrite bought: a failing credential HALTS the
    // sync with the credential named, instead of continuing with partial
    // credentials and failing later with a confusing connector error.
    #[tokio::test]
    async fn a_failing_credential_halts_and_names_itself() {
        let secrets = MockSecrets::new();
        let err = resolve_credentials(&secrets, &["cred-ghost".to_string()])
            .await
            .expect_err("mock knows no credentials");
        assert!(
            err.message.contains("cred-ghost"),
            "error was: {}",
            err.message
        );
    }
}
