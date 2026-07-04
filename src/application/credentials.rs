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
