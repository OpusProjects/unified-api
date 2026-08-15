use std::collections::HashMap;

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
