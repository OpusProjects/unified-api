use std::collections::HashMap;

use crate::ports::secrets::{SecretsError, SecretsPort};

// Resuelve una lista de credential_ids contra el SecretsPort y junta
// todos los pares clave-valor en un solo HashMap.
//
// Recibe &[String] (solo los ids) y no un Source entero: no necesita más,
// y así el scheduler no tiene que fabricar un Source falso para llamarla.
//
// Un fallo de resolución CORTA el caso de uso y sube al caller. Antes se
// tragaba con un warn! y se seguía con credenciales parciales o vacías —
// el sync fallaba después con un error confuso del connector, o peor,
// "funcionaba" sin la autenticación esperada.
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
