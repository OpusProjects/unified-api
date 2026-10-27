use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::credentials::{check, users_of};

// The credentials configured on this instance, for the UI's Credentials
// page: what each one is, where its secrets come from, whether they resolve
// right now, and what depends on it.
//
// NEVER a secret value. The page answers "is it there, and who needs it" —
// the first questions when a sync fails with an authentication error, or
// before a credential is rotated or removed — and none of that needs a value.
// What it shows is what credentials.yaml already says (variable names, file
// paths, a Vault path) plus a yes/no. Admin-only all the same, like the
// configuration API that holds the same file: a map of where every secret
// lives is not for every key.

#[derive(Serialize, ToSchema)]
pub struct CredentialInfo {
    pub credential_id: String,
    pub name: String,
    /// `username_password`, `token` or `ssh_key`
    #[serde(rename = "type")]
    pub credential_type: &'static str,
    /// Where the secrets are read from: `vault`, `env`, `file`, or `files`
    /// (only file paths handed to the script)
    pub backend: &'static str,
    pub vault_path: Option<String>,
    /// The environment variables read, for an env-backed credential
    pub env_vars: Vec<String>,
    pub secret_file: Option<String>,
    /// The names the connector receives the secrets under (not their values)
    pub secret_keys: Vec<String>,
    /// Files handed to the script by path (an SSH key, a certificate)
    pub file_keys: Vec<FileKeyInfo>,
    /// Whether the secrets resolve right now, asked as a sync asks
    pub resolves: bool,
    /// Why not: the resolver's message, naming what is missing
    pub error: Option<String>,
    pub used_by: CredentialUsersInfo,
}

#[derive(Serialize, ToSchema)]
pub struct FileKeyInfo {
    pub name: String,
    pub path: String,
}

#[derive(Serialize, ToSchema)]
pub struct CredentialUsersInfo {
    pub sources: Vec<String>,
    pub projects: Vec<String>,
    /// Whether the LDAP login's service account binds with it
    pub ldap: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/credentials",
    tag = "Credentials",
    responses(
        (status = 200, description = "Configured credentials: where their secrets come from, whether they resolve now, and what uses them. Never a secret value", body = Vec<CredentialInfo>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_credentials(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<CredentialInfo>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();

    let mut credentials = Vec::with_capacity(config.credentials.len());
    for (id, credential) in &config.credentials {
        let error = check(state.secrets.as_ref(), id).await;
        let users = users_of(id, &config.sources, &config.projects, config.ldap.as_ref());

        // The fields it delivers, whichever form names them.
        let secret_keys = credential.fields();
        let mut file_keys: Vec<FileKeyInfo> = credential
            .file_keys
            .iter()
            .map(|(name, path)| FileKeyInfo {
                name: name.clone(),
                path: path.clone(),
            })
            .collect();
        file_keys.sort_by(|a, b| a.name.cmp(&b.name));

        credentials.push(CredentialInfo {
            credential_id: id.clone(),
            name: credential.name.clone(),
            credential_type: credential.credential_type.name(),
            backend: credential.backend(),
            vault_path: credential.vault_path.clone(),
            env_vars: credential.env_var_names(),
            secret_file: credential.secret_file.clone(),
            secret_keys,
            file_keys,
            resolves: error.is_none(),
            error,
            used_by: CredentialUsersInfo {
                sources: users.sources,
                projects: users.projects,
                ldap: users.ldap,
            },
        });
    }

    credentials.sort_by(|a, b| a.credential_id.cmp(&b.credential_id));
    Ok(Json(credentials))
}
