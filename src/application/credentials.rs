use std::collections::HashMap;

use crate::domain::credential::{Credential, delivery_id, variable_part};
use crate::domain::ldap::LdapConfig;
use crate::domain::project::GitProject;
use crate::domain::source::Source;
use crate::ports::secrets::{SecretsError, SecretsPort};

// Resolves a list of credential_ids against the SecretsPort and combines
// all key-value pairs into a single HashMap, each field under two names:
// - `<id>_<field>` always (see `domain::credential::delivery_id`): `cred-vcenter-aa1`'s username is
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
    let mut seen = std::collections::HashSet::new();
    // A credential listed twice is one credential: counted twice, its every
    // field would look shared and lose its bare name.
    for credential_id in credential_ids.iter().filter(|id| seen.insert(id.as_str())) {
        match secrets.resolve(credential_id).await {
            Ok(fields) => resolved.push((credential_id, fields)),
            Err(e) => {
                return Err(SecretsError {
                    message: format!("credential '{}': {}", credential_id, e.message),
                });
            }
        }
    }

    // How many of these credentials have each field, compared as the
    // variable name it becomes (`TOKEN` and `token` are one CREDENTIAL_TOKEN).
    let mut holders: HashMap<String, usize> = HashMap::new();
    for (_, fields) in &resolved {
        for field in fields.keys() {
            *holders.entry(variable_part(field)).or_default() += 1;
        }
    }

    // Every name delivered, keyed by the variable it becomes, with where it
    // came from: two different values never land on one variable. That is
    // an error naming both, not a silent last-one-wins.
    let mut all_credentials = HashMap::new();
    let mut origin: HashMap<String, (String, String)> = HashMap::new();
    let mut deliver = |name: String, value: &String, credential_id: &str, field: &str| {
        let variable = variable_part(&name);
        match origin.get(&variable) {
            Some((other, other_field)) if other != credential_id || other_field != field => {
                return Err(SecretsError {
                    message: format!(
                        "credentials '{other}' (field '{other_field}') and '{credential_id}' \
                         (field '{field}') would both reach the connector as CREDENTIAL_{} — \
                         rename one of the fields",
                        variable.to_uppercase()
                    ),
                });
            }
            _ => {}
        }
        origin.insert(variable, (credential_id.to_string(), field.to_string()));
        all_credentials.insert(name, value.clone());
        Ok(())
    };
    for (credential_id, fields) in &resolved {
        let prefix = format!("{}_", delivery_id(credential_id));
        for (field, value) in fields {
            let variable = variable_part(field);
            let unshared = holders[&variable] == 1;
            // A field already named after its credential (`avamar_aa1_url`
            // on cred-avamar-aa1, the hand-made form that predates this) is
            // not prefixed twice: scripts that discover sites by scanning
            // CREDENTIAL_AVAMAR_<SITE>_URL would otherwise find a second,
            // phantom site AA1_AVAMAR_AA1. Only an unshared field with
            // something after the prefix: `ssh_key_path` on cred-ssh, shared
            // with another credential, is a field like any other.
            let hand_made =
                unshared && variable.len() > prefix.len() && variable.starts_with(&prefix);
            if hand_made {
                deliver(field.clone(), value, credential_id, field)?;
                continue;
            }
            deliver(format!("{prefix}{field}"), value, credential_id, field)?;
            if unshared {
                deliver(field.clone(), value, credential_id, field)?;
            }
        }
    }
    Ok(all_credentials)
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
        .filter(|(_, project)| project.credential_id() == Some(credential_id))
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
        assert_eq!(delivery_id("cred-vcenter-aa1"), "vcenter_aa1");
        assert_eq!(delivery_id("cred_ucs.a6"), "ucs_a6");
        assert_eq!(delivery_id("Edge-PQ-DC4"), "edge_pq_dc4");
    }

    // However the hand-made field is written, it is not prefixed again.
    #[tokio::test]
    async fn the_no_double_prefix_rule_ignores_case() {
        let secrets =
            MockSecrets::new().with("cred-avamar-aa1", [("AVAMAR_AA1_URL", "https://aa1")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-avamar-aa1"]))
            .await
            .unwrap();
        let names: Vec<&String> = resolved.keys().collect();
        assert_eq!(names, ["AVAMAR_AA1_URL"]);
    }

    // A shared field is a field like any other, even when its name happens
    // to start with the credential's id: never handed out bare.
    #[tokio::test]
    async fn a_shared_field_starting_with_its_id_is_still_prefixed() {
        let secrets = MockSecrets::new()
            .with("cred-ssh", [("ssh_key_path", "/a")])
            .with("cred-ssh-dc4", [("ssh_key_path", "/b")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-ssh", "cred-ssh-dc4"]))
            .await
            .unwrap();
        assert!(!resolved.contains_key("ssh_key_path"));
        assert_eq!(resolved["ssh_ssh_key_path"], "/a");
        assert_eq!(resolved["ssh_dc4_ssh_key_path"], "/b");
    }

    // Two values that would reach one variable fail the resolution, naming
    // both, instead of one silently winning.
    #[tokio::test]
    async fn two_values_on_one_variable_are_an_error() {
        let secrets = MockSecrets::new()
            .with("cred-avamar", [("aa1_url", "old")])
            .with("cred-avamar-aa1", [("url", "new")]);
        let err = resolve_credentials(&secrets, &ids(&["cred-avamar", "cred-avamar-aa1"]))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("CREDENTIAL_AVAMAR_AA1_URL"),
            "{}",
            err.message
        );

        let secrets = MockSecrets::new()
            .with("cred-a", [("TOKEN", "1")])
            .with("cred-b", [("token", "2")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-a", "cred-b"]))
            .await
            .unwrap();
        assert!(!resolved.contains_key("token") && !resolved.contains_key("TOKEN"));
    }

    #[tokio::test]
    async fn a_credential_listed_twice_keeps_its_bare_names() {
        let secrets = MockSecrets::new().with("cred-gh", [("token", "t")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-gh", "cred-gh"]))
            .await
            .unwrap();
        assert_eq!(resolved["token"], "t");
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
