use std::collections::HashMap;

use crate::domain::ldap::LdapConfig;
use crate::domain::project::GitProject;
use crate::domain::source::Source;
use crate::ports::secrets::{SecretsError, SecretsPort};

// Resolves a list of credential_ids against the SecretsPort and combines
// all key-value pairs into a single HashMap.
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
    let mut all_credentials = HashMap::new();

    for credential_id in credential_ids {
        match secrets.resolve(credential_id).await {
            Ok(creds) => all_credentials.extend(creds),
            Err(e) => {
                return Err(SecretsError {
                    message: format!("credential '{}': {}", credential_id, e.message),
                });
            }
        }
    }

    Ok(all_credentials)
}

// What names a credential in its configuration: the sources that pass it
// to their connector, the projects that clone with it, and the directory
// login's service account. Sorted, so a page and a diff of it are stable.
pub struct CredentialUsers {
    pub sources: Vec<String>,
    pub projects: Vec<String>,
    pub ldap: bool,
}

pub fn users_of(
    credential_id: &str,
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
    CredentialUsers {
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
