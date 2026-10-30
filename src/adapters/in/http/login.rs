use axum::extract::{Form, Query, State};
use axum::response::Response;
use std::sync::Arc;

use crate::AppState;
use crate::adapters::r#in::http::auth::{ApiKeys, Permissions};
use crate::adapters::r#in::http::html::site;
use crate::adapters::r#in::http::session::Sessions;
use crate::application::login::{LoginError, throttled_ldap_login};
use crate::domain::ldap::Grant;

pub use web_rs::login::{LoginForm, LoginQuery, SessionTtl};

// The login form for the browsable API.
//
// Public routes, outside the key middleware — a page that asks for a credential
// cannot itself require one. They exist only when the UI is enabled.
//
// Two ways in, and neither is a credential store of its own. An API KEY is
// checked against the same registry and the same constant-time compare a
// header goes through. A directory account (when `server.ui.ldap` is set) is
// checked by the directory, and what it may touch comes from the configured
// access rules. There is no user table and no password stored here.
//
// The form, its challenge, the key path and the session are web-rs's (the
// `web` repository), shared with the sibling service. What is this service's
// own is the directory: its LDAP block, its throttle, and what its access
// rules grant, which becomes these Permissions.

struct Directory<'a> {
    state: &'a Arc<AppState>,
}

impl web_rs::login::SignIn<Permissions> for Directory<'_> {
    fn ldap_enabled(&self) -> bool {
        self.state.config().ldap.is_some()
    }

    fn home(&self) -> String {
        self.state.config().ui_home.clone()
    }

    async fn ldap_login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<(String, Permissions), LoginError> {
        let config = self.state.config();
        // Asked only when ldap_enabled said yes; a reload in between is the
        // one way to land here, and it reads as the directory being away.
        let Some(ldap) = config.ldap.as_ref() else {
            return Err(LoginError::Unavailable(
                "LDAP login was turned off by a reload".to_string(),
            ));
        };
        let logged = throttled_ldap_login(
            self.state.login_throttle.as_ref(),
            self.state.secrets.as_ref(),
            self.state.directory.as_ref(),
            ldap,
            username,
            password,
        )
        .await?;
        let permissions = match logged.grant {
            Grant::Admin => Permissions::Admin,
            Grant::Scoped { sources, endpoints } => Permissions::Scoped { sources, endpoints },
        };
        Ok((logged.username, permissions))
    }
}

pub async fn login_form(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LoginQuery>,
    headers: axum::http::HeaderMap,
) -> Response {
    web_rs::login::login_form(
        &site(),
        &Directory { state: &state },
        query.next.as_deref(),
        &headers,
    )
}

pub async fn login_submit(
    State(state): State<Arc<AppState>>,
    axum::Extension(keys): axum::Extension<ApiKeys>,
    axum::Extension(sessions): axum::Extension<Sessions>,
    axum::Extension(ttl): axum::Extension<SessionTtl>,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    web_rs::login::login_submit(
        &site(),
        &Directory { state: &state },
        &keys.0,
        &sessions.0,
        ttl.0,
        &headers,
        form,
    )
    .await
}

pub async fn logout(
    axum::Extension(sessions): axum::Extension<Sessions>,
    headers: axum::http::HeaderMap,
    form: Result<Form<LoginQuery>, axum::extract::rejection::FormRejection>,
) -> Response {
    let next = form.ok().and_then(|Form(f)| f.next);
    web_rs::login::logout(&site(), &sessions.0, &headers, next)
}
