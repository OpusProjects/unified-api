use axum::extract::Form;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use std::time::Duration;

use crate::adapters::r#in::http::auth::{ApiKeys, match_token};
use crate::adapters::r#in::http::html::{Html, THEMES};
use crate::adapters::r#in::http::session::{COOKIE_NAME, Sessions, cookie_value};

// The login form for the browsable API.
//
// Public routes, outside the key middleware — a page that asks for a credential
// cannot itself require one. They exist only when the UI is enabled.
//
// What is validated is an API KEY, against the same registry and the same
// constant-time compare a header goes through. There is no second credential
// store, no user table and no password: the console is a way to use a key you
// already have from a browser, which cannot send headers.

// A short-lived token planted when the form is served and required back when
// it is submitted, so only a form THIS server rendered can log anybody in.
//
// Without it, a page on the internet can auto-submit a login form carrying the
// ATTACKER's API key: the browser navigates top-level to our origin, so the
// response is first-party and the cookie is stored (SameSite=Strict governs
// sending an existing cookie, not setting a new one). The victim then works —
// and is audit-logged — under someone else's key without noticing.
const LOGIN_COOKIE: &str = "uapi_login";

#[derive(Deserialize)]
pub struct LoginForm {
    pub key: String,
    #[serde(default)]
    pub _csrf: String,
}

fn esc(s: &str) -> String {
    Html::text(s).as_str().to_string()
}

// Rendered by hand rather than through the JSON page: there is no resource here
// and nothing to negotiate. It borrows the same palette so the login does not
// look like a different product.
fn login_page(
    theme: &str,
    message: Option<&str>,
    status: StatusCode,
    challenge: Option<&str>,
) -> Response {
    let notice = match message {
        Some(text) => format!("<p class=\"bad\">{}</p>", esc(text)),
        None => String::new(),
    };

    let body = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>log in — unified-api</title>
<style>
{palette}
* {{ box-sizing: border-box; }}
body {{
  margin: 0; min-height: 100vh; display: grid; place-items: center;
  background: var(--bg); color: var(--fg);
  font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
}}
form {{
  background: var(--panel); border: 1px solid var(--rule); border-radius: 10px;
  padding: 28px 30px; width: min(420px, 92vw);
}}
h1 {{ margin: 0 0 4px; font-size: 18px; }}
p.lead {{ margin: 0 0 20px; color: var(--dim); font-size: 13px; }}
label {{ display: block; font-size: 12px; color: var(--dim); margin-bottom: 6px; }}
input {{
  width: 100%; padding: 10px 12px; border-radius: 6px;
  border: 1px solid var(--rule); background: var(--bg); color: var(--fg);
  font-family: inherit; font-size: 14px;
}}
button {{
  margin-top: 16px; width: 100%; padding: 10px 12px; border: 0; border-radius: 6px;
  background: var(--link); color: var(--bg); font-family: inherit;
  font-size: 14px; font-weight: 700; cursor: pointer;
}}
.bad {{ color: #ff7b72; font-size: 13px; margin: 0 0 14px; }}
.foot {{ margin: 18px 0 0; color: var(--dim); font-size: 12px; }}
.foot a {{ color: var(--link); }}
</style>
</head>
<body>
<form method="post" action="/api/v1/login">
  <h1>unified-api</h1>
  <p class="lead">Sign in with an API key to browse from this browser.</p>
  {notice}
  <input type="hidden" name="_csrf" value="{challenge}">
  <label for="key">API key</label>
  <input id="key" name="key" type="password" autocomplete="current-password" autofocus>
  <button type="submit">Log in</button>
  <p class="foot">A key sent as <code>X-API-Key</code> needs no session.
  <a href="/swagger-ui/">OpenAPI</a></p>
</form>
</body>
</html>"#,
        palette = crate::adapters::r#in::http::html::palette_css(theme),
        notice = notice,
        challenge = esc(challenge.unwrap_or("")),
    );

    (
        status,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            // The same policy the rendered pages get. This page is outside the
            // negotiate layer that sets it, and it is the one page that takes a
            // credential — the last place to leave unframeable.
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(crate::adapters::r#in::http::html::CSP),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, private"),
            ),
        ],
        body,
    )
        .into_response()
}

fn theme_of(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, "uapi_theme"))
        .filter(|t| THEMES.contains(t))
        .unwrap_or("auto")
        .to_string()
}

pub async fn login_form(headers: axum::http::HeaderMap) -> Response {
    let challenge = crate::adapters::r#in::http::session::new_token();
    let mut response = login_page(&theme_of(&headers), None, StatusCode::OK, Some(&challenge));
    if let Ok(value) = HeaderValue::from_str(&format!(
        "{LOGIN_COOKIE}={challenge}; Path=/api/v1/login; HttpOnly; SameSite=Strict; Max-Age=600"
    )) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
    response
}

pub async fn login_submit(
    axum::Extension(keys): axum::Extension<ApiKeys>,
    axum::Extension(sessions): axum::Extension<Sessions>,
    axum::Extension(ttl): axum::Extension<SessionTtl>,
    headers: axum::http::HeaderMap,
    Form(form): Form<LoginForm>,
) -> Response {
    let theme = theme_of(&headers);

    // The form must be one this server handed out. Both halves of the pair
    // have to agree: the cookie planted when the form was rendered, and the
    // hidden field inside it. A cross-site form has neither.
    let challenge = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, LOGIN_COOKIE));

    let matched = challenge.is_some_and(|expected| {
        !form._csrf.is_empty()
            && crate::adapters::r#in::http::session::csrf_matches(expected, &form._csrf)
    });

    if !matched {
        return login_page(
            &theme,
            Some("this login form has expired — reload the page and try again"),
            StatusCode::FORBIDDEN,
            None,
        );
    }

    let registry = keys.0.load();

    // An open API has nothing to log into: every caller is already admin, and a
    // form that accepts anything would suggest otherwise.
    if registry.is_empty() {
        // Deliberately the same answer a wrong key gets. Saying "this instance
        // has no authentication" to an unauthenticated caller is a banner for
        // anyone scanning; the real reason goes to the log, where an operator
        // can see it and a scanner cannot.
        tracing::warn!("login attempted on an instance with no API keys configured");
        return login_page(
            &theme,
            Some("invalid API key"),
            StatusCode::UNAUTHORIZED,
            None,
        );
    }

    let Some(key) = match_token(form.key.trim(), &registry) else {
        // No detail, deliberately: "no such key" and "wrong key" are the same
        // sentence to someone guessing.
        return login_page(
            &theme,
            Some("invalid API key"),
            StatusCode::UNAUTHORIZED,
            None,
        );
    };

    let issued = sessions
        .0
        .create(key.name.clone(), key.permissions.clone(), ttl.0);

    tracing::info!(key_name = %key.name, "browser session opened");

    // Secure is NOT set here: the console is reachable over plain HTTP in a
    // port-forward or a local run, and a Secure cookie would silently never be
    // stored there — a login that appears to work and does nothing. In a real
    // deployment this sits behind TLS at the ingress, and the attributes that
    // do the security work are HttpOnly (no script can read it; there are no
    // scripts anyway) and SameSite=Strict (the browser will not attach it to a
    // request another site caused), backed by the CSRF token on every write.
    let cookie = format!(
        "{COOKIE_NAME}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        issued.token,
        ttl.0.as_secs()
    );

    let mut response = axum::response::Redirect::to("/api/v1/").into_response();
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}

pub async fn logout(
    axum::Extension(sessions): axum::Extension<Sessions>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| cookie_value(c, COOKIE_NAME))
    {
        // Dropped server-side, not merely forgotten by the browser: a cookie
        // someone already copied must stop working too.
        sessions.0.remove(token);
    }

    let mut response = axum::response::Redirect::to("/api/v1/login").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static("uapi_session=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0"),
    );
    response
}

// The session lifetime, injected so it can be read from configuration once and
// not threaded through every handler.
#[derive(Clone, Copy)]
pub struct SessionTtl(pub Duration);

impl Default for SessionTtl {
    fn default() -> Self {
        SessionTtl(Duration::from_secs(3600))
    }
}
