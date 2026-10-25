use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::{
    ApiKeys, AuthContext, AuthMethod, BrowserIdentity, Permissions,
};
use crate::adapters::r#in::http::error::ErrorBody;
use crate::domain::api_key::ApiKeyRole;

// Who can get in, and how — read-only, for the UI's Users page.
//
// Read-only by design: users and the directory are declared in git
// (config.yaml `server.ui.ldap`, api_keys.yaml) and pushed like the rest of
// the configuration, so this route describes what is in force rather than
// offering a second place to change it. Everyone authenticated sees who THEY
// are; only an admin sees the rest, because the access rules and key scopes
// are a map of who holds what.

#[derive(Serialize, ToSchema)]
pub struct AuthInfo {
    pub me: Me,
    pub methods: Methods,
    /// The LDAP configuration and its access rules. Admin only; absent when
    /// LDAP login is not configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ldap: Option<LdapInfo>,
    /// The API keys in force, by name and scope — never a secret. Admin only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_keys: Option<Vec<ApiKeyInfo>>,
}

#[derive(Serialize, ToSchema)]
pub struct Me {
    /// The key's name, or `ldap:<username>`. Null on an open instance.
    pub name: Option<String>,
    /// How this request authenticated: `open`, `api_key` or `ldap`
    pub method: &'static str,
    pub role: &'static str,
    /// Source ids a restricted caller may read and sync (empty for admin)
    pub sources: Vec<String>,
    /// Endpoint ids a restricted caller may run (empty for admin)
    pub endpoints: Vec<String>,
    /// The token a browser session sends as `X-CSRF-Token` on every write.
    /// Only for a session (cookie) caller; a header-authenticated caller
    /// needs none. Safe to return: another site cannot read this response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csrf_token: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub struct Methods {
    /// API keys are always accepted, as a header or through the login form
    pub api_key: bool,
    /// Whether the login form offers a directory account
    pub ldap: bool,
}

#[derive(Serialize, ToSchema)]
pub struct LdapInfo {
    pub url: String,
    pub starttls: bool,
    pub ca_file: Option<String>,
    /// credentials.yaml id holding the service account (its secret is not shown)
    pub bind_credential: String,
    pub search_base: String,
    pub user_filter: String,
    pub group_attribute: String,
    pub timeout_seconds: u64,
    pub access: Vec<AccessRuleInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct AccessRuleInfo {
    pub name: String,
    pub groups: Vec<String>,
    pub users: Vec<String>,
    pub role: &'static str,
    pub sources: Vec<String>,
    pub endpoints: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ApiKeyInfo {
    pub name: String,
    pub role: &'static str,
    pub sources: Vec<String>,
    pub endpoints: Vec<String>,
}

fn role_name(role: ApiKeyRole) -> &'static str {
    match role {
        ApiKeyRole::Admin => "admin",
        ApiKeyRole::Restricted => "restricted",
    }
}

// Sorted so the page and any diff of this response are stable.
fn scope(permissions: &Permissions) -> (&'static str, Vec<String>, Vec<String>) {
    match permissions {
        Permissions::Admin => ("admin", Vec::new(), Vec::new()),
        Permissions::Scoped { sources, endpoints } => {
            let mut sources: Vec<String> = sources.iter().cloned().collect();
            let mut endpoints: Vec<String> = endpoints.iter().cloned().collect();
            sources.sort();
            endpoints.sort();
            ("restricted", sources, endpoints)
        }
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/auth",
    tag = "Auth",
    responses(
        (status = 200, description = "Who the caller is, and — for an admin — every way in: the LDAP configuration with its access rules and the API keys by name", body = AuthInfo),
        (status = 401, description = "Not authenticated", body = ErrorBody)
    )
)]
pub async fn get_auth(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    session: Option<axum::Extension<BrowserIdentity>>,
) -> Json<AuthInfo> {
    let config = state.config();
    let (role, sources, endpoints) = scope(&auth.permissions);
    let admin = auth.permissions.is_admin();

    let me = Me {
        name: auth.key_name.clone(),
        method: match auth.method {
            AuthMethod::Open => "open",
            AuthMethod::ApiKey => "api_key",
            AuthMethod::Ldap => "ldap",
        },
        role,
        sources,
        endpoints,
        csrf_token: session.map(|axum::Extension(identity)| identity.csrf),
    };

    let ldap = config.ldap.as_ref().filter(|_| admin).map(|ldap| LdapInfo {
        url: ldap.url.clone(),
        starttls: ldap.starttls,
        ca_file: ldap.ca_file.clone(),
        bind_credential: ldap.bind_credential.clone(),
        search_base: ldap.search_base.clone(),
        user_filter: ldap.user_filter.clone(),
        group_attribute: ldap.group_attribute.clone(),
        timeout_seconds: ldap.timeout_seconds,
        access: ldap
            .access
            .iter()
            .map(|rule| AccessRuleInfo {
                name: rule.name.clone(),
                groups: rule.groups.clone(),
                users: rule.users.clone(),
                role: role_name(rule.role),
                sources: rule.sources.clone(),
                endpoints: rule.endpoints.clone(),
            })
            .collect(),
    });

    let api_keys = admin.then(|| {
        let mut list: Vec<ApiKeyInfo> = keys
            .0
            .load()
            .iter()
            .map(|key| {
                let (role, sources, endpoints) = scope(&key.permissions);
                ApiKeyInfo {
                    name: key.name.clone(),
                    role,
                    sources,
                    endpoints,
                }
            })
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    });

    Json(AuthInfo {
        me,
        methods: Methods {
            api_key: true,
            ldap: config.ldap.is_some(),
        },
        ldap,
        api_keys,
    })
}
