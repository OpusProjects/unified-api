use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use std::collections::HashMap;
use std::sync::Arc;
use tower::ServiceExt;

use unified_api::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use unified_api::adapters::out::secrets::mock::MockSecrets;
use unified_api::domain::api_key::ApiKeyRole;
use unified_api::domain::ldap::{AccessRule, LdapConfig};
use unified_api::ports::directory::{
    DirectoryError, DirectoryFuture, DirectoryLogin, DirectoryPort, DirectoryUser,
};

// LDAP login, end to end through the real router: the form, the session it
// opens, and what that session may do. The directory is a double; the
// adapter's own behaviour (empty passwords, unreachable servers) is tested
// next to it.

// Knows jdoe (in DL-ops) and asmith (in DL-readers), both with "right".
struct Directory;

impl DirectoryPort for Directory {
    fn authenticate<'a>(&'a self, request: DirectoryLogin<'a>) -> DirectoryFuture<'a> {
        Box::pin(async move {
            let group = match request.username {
                "jdoe" => "CN=DL-ops,OU=Groups,DC=x",
                "asmith" => "CN=DL-readers,OU=Groups,DC=x",
                "nobody" => "CN=DL-elsewhere,OU=Groups,DC=x",
                _ => return Err(DirectoryError::InvalidCredentials),
            };
            if request.password != "right" {
                return Err(DirectoryError::InvalidCredentials);
            }
            Ok(DirectoryUser {
                dn: format!("CN={},OU=Users,DC=x", request.username),
                username: Some(request.username.to_string()),
                member_of: vec![group.to_string()],
            })
        })
    }
}

fn ldap() -> LdapConfig {
    let rule = |name: &str, group: &str, role: ApiKeyRole, sources: &[&str]| AccessRule {
        name: name.to_string(),
        groups: vec![group.to_string()],
        users: Vec::new(),
        role,
        sources: sources.iter().map(|s| s.to_string()).collect(),
        endpoints: Vec::new(),
    };
    LdapConfig {
        url: "ldaps://ldap.example.com".to_string(),
        starttls: false,
        ca_file: None,
        bind_credential: "cred-ldap".to_string(),
        search_base: "DC=x".to_string(),
        user_filter: "(sAMAccountName={username})".to_string(),
        group_attribute: "memberOf".to_string(),
        username_attribute: "sAMAccountName".to_string(),
        timeout_seconds: 5,
        access: vec![
            rule("Operations", "DL-ops", ApiKeyRole::Admin, &[]),
            rule(
                "Readers",
                "DL-readers",
                ApiKeyRole::Restricted,
                &["src-vmware"],
            ),
        ],
    }
}

fn app(ldap: Option<LdapConfig>) -> axum::Router {
    app_home(ldap, "/api/v1/")
}

fn app_home(ldap: Option<LdapConfig>, home: &str) -> axum::Router {
    let ui = unified_api::config::UiConfig {
        enabled: true,
        ldap,
        home: Some(home.to_string()),
        ..Default::default()
    };
    unified_api::AppBuilder::new()
        .sources(HashMap::new())
        .ui(ui)
        .secrets(Arc::new(MockSecrets::new().with(
            "cred-ldap",
            [("username", "CN=svc-unified,DC=x"), ("password", "svc")],
        )))
        .directory(Arc::new(Directory))
        // With keys configured the API is not open, so a session is the only
        // way these requests authenticate.
        .api_keys(vec![ResolvedApiKey {
            id: "AWX".to_string(),
            name: "AWX".to_string(),
            secret: "awx-secret".to_string(),
            permissions: Permissions::Admin,
        }])
        .build()
}

async fn body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8_lossy(&bytes).to_string()
}

fn cookie(resp: &axum::response::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| {
            let pair = c.split(';').next()?;
            let (key, value) = pair.split_once('=')?;
            (key == name && !value.is_empty()).then(|| value.to_string())
        })
}

// GET the form (which plants the login challenge), then POST it back with
// `fields`, the way a browser does.
async fn submit(app: &axum::Router, fields: &str) -> axum::response::Response {
    let form = app
        .clone()
        .oneshot(Request::get("/api/v1/login").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let challenge = cookie(&form, "uapi_login").expect("login challenge cookie");

    app.clone()
        .oneshot(
            Request::post("/api/v1/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("uapi_login={challenge}"))
                .body(Body::from(format!("_csrf={challenge}&{fields}")))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get_json(app: &axum::Router, uri: &str, session: &str) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header(header::COOKIE, format!("uapi_session={session}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let text = body(resp).await;
    (
        status,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn the_form_asks_for_a_username_only_when_ldap_is_configured() {
    let with = body(
        app(Some(ldap()))
            .oneshot(Request::get("/api/v1/login").body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;
    assert!(with.contains(r#"name="username""#));
    assert!(
        with.contains(r#"name="key""#),
        "the API key stays available"
    );

    let without = body(
        app(None)
            .oneshot(Request::get("/api/v1/login").body(Body::empty()).unwrap())
            .await
            .unwrap(),
    )
    .await;
    assert!(!without.contains(r#"name="username""#));
}

#[tokio::test]
async fn an_admin_group_member_gets_an_admin_session() {
    let app = app(Some(ldap()));
    let resp = submit(&app, "username=jdoe&password=right").await;
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let session = cookie(&resp, "uapi_session").expect("a session cookie");

    let (status, auth) = get_json(&app, "/api/v1/auth", &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(auth["me"]["name"], "ldap:jdoe");
    assert_eq!(auth["me"]["method"], "ldap");
    assert_eq!(auth["me"]["role"], "admin");
    assert_eq!(auth["methods"]["ldap"], true);
    // An admin sees the configuration and the keys, never a secret
    assert_eq!(auth["ldap"]["bind_credential"], "cred-ldap");
    assert_eq!(auth["ldap"]["access"][0]["name"], "Operations");
    assert_eq!(auth["api_keys"][0]["name"], "AWX");
    assert!(!auth.to_string().contains("awx-secret"));
    assert!(!auth.to_string().contains("\"svc\""));
}

#[tokio::test]
async fn a_restricted_member_sees_only_themselves_and_their_scope() {
    let app = app(Some(ldap()));
    let resp = submit(&app, "username=asmith&password=right").await;
    let session = cookie(&resp, "uapi_session").expect("a session cookie");

    let (_, auth) = get_json(&app, "/api/v1/auth", &session).await;
    assert_eq!(auth["me"]["role"], "restricted");
    assert_eq!(auth["me"]["sources"][0], "src-vmware");
    assert!(auth.get("ldap").is_none(), "the rules are an admin's map");
    assert!(auth.get("api_keys").is_none());

    let (status, _) = get_json(&app, "/api/v1/projects", &session).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_wrong_password_opens_nothing() {
    let app = app(Some(ldap()));
    let resp = submit(&app, "username=jdoe&password=wrong").await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(cookie(&resp, "uapi_session").is_none());
    assert!(body(resp).await.contains("invalid username or password"));
}

#[tokio::test]
async fn a_valid_account_no_rule_names_is_refused_and_told_why() {
    let app = app(Some(ldap()));
    let resp = submit(&app, "username=nobody&password=right").await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(cookie(&resp, "uapi_session").is_none());
    assert!(body(resp).await.contains("no access to this instance"));
}

#[tokio::test]
async fn an_api_key_still_logs_in_next_to_ldap() {
    let app = app(Some(ldap()));
    let resp = submit(&app, "key=awx-secret").await;
    let session = cookie(&resp, "uapi_session").expect("a session cookie");
    let (_, auth) = get_json(&app, "/api/v1/auth", &session).await;
    assert_eq!(auth["me"]["name"], "AWX");
    assert_eq!(auth["me"]["method"], "api_key");
}

#[tokio::test]
async fn a_username_on_an_instance_without_ldap_is_refused() {
    let app = app(None);
    let resp = submit(&app, "username=jdoe&password=right").await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(cookie(&resp, "uapi_session").is_none());
}

// After login the browser goes where the page that sent it asked — but only
// somewhere on this origin, or the login becomes an open redirect.
#[tokio::test]
async fn next_redirects_only_within_this_origin() {
    let app = app(Some(ldap()));
    let location = |resp: &axum::response::Response| {
        resp.headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };

    let resp = submit(&app, "username=jdoe&password=right&next=/sources").await;
    assert_eq!(location(&resp), "/sources");

    for evil in [
        "//evil.example/x",
        "https://evil.example",
        "/\\evil.example",
        // Browsers drop tabs and newlines: these become "//evil.example".
        "/%09/evil.example",
        "/%0A/evil.example",
        "/%20/evil.example",
    ] {
        let resp = submit(&app, &format!("username=jdoe&password=right&next={evil}")).await;
        assert_eq!(location(&resp), "/api/v1/", "followed {evil}");
    }
}

// With nowhere asked for, a login lands on `server.ui.home`: "/" where the
// UI is served in front of the API, so signing in never strands a person on
// the API's own pages.
#[tokio::test]
async fn without_next_a_login_lands_on_the_configured_home() {
    let location = |resp: &axum::response::Response| {
        resp.headers()
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let resp = submit(&app(Some(ldap())), "username=jdoe&password=right").await;
    assert_eq!(
        location(&resp),
        "/api/v1/",
        "the default, with no UI in front"
    );

    let resp = submit(&app_home(Some(ldap()), "/"), "username=jdoe&password=right").await;
    assert_eq!(location(&resp), "/");
    // A page that asked still wins over home.
    let resp = submit(
        &app_home(Some(ldap()), "/"),
        "username=jdoe&password=right&next=/sources",
    )
    .await;
    assert_eq!(location(&resp), "/sources");
}

// Signing out from the UI posts next=/, and the sign-in page it lands on
// carries it, so signing straight back in returns to the UI.
#[tokio::test]
async fn sign_out_keeps_where_the_next_sign_in_lands() {
    let app = app(Some(ldap()));
    let logout = |body: &'static str| {
        let app = app.clone();
        async move {
            let mut req = Request::post("/api/v1/logout");
            if !body.is_empty() {
                req = req.header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            }
            let resp = app
                .oneshot(req.body(Body::from(body)).unwrap())
                .await
                .unwrap();
            resp.headers()
                .get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        }
    };
    assert_eq!(logout("next=/").await, "/api/v1/login?next=/");
    assert_eq!(
        logout("next=%2Fsources%3Fhost%3Da%26b").await,
        "/api/v1/login?next=/sources%3Fhost%3Da%26b"
    );
    assert_eq!(logout("next=//evil.example").await, "/api/v1/login");
    assert_eq!(
        logout("").await,
        "/api/v1/login",
        "a bare POST still signs out"
    );
}

// After a refusal the page must still be usable: it carries a fresh
// challenge, so correcting the password and submitting again works without
// reloading.
#[tokio::test]
async fn a_refused_form_can_be_submitted_again() {
    let app = app(Some(ldap()));
    let refused = submit(&app, "username=jdoe&password=wrong").await;
    let challenge = cookie(&refused, "uapi_login").expect("a fresh challenge cookie");
    let page = body(refused).await;
    assert!(
        page.contains(&format!(r#"value="{challenge}""#)),
        "the form carries it too"
    );

    let retry = app
        .clone()
        .oneshot(
            Request::post("/api/v1/login")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .header(header::COOKIE, format!("uapi_login={challenge}"))
                .body(Body::from(format!(
                    "_csrf={challenge}&username=jdoe&password=right"
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::SEE_OTHER);
    assert!(cookie(&retry, "uapi_session").is_some());
}

// One card, and it reopens on the way in that was just tried: a mistyped key
// must not drop the person back on the directory form.
#[tokio::test]
async fn the_card_reopens_on_the_method_that_was_tried() {
    let app = app(Some(ldap()));
    let page = body(submit(&app, "key=wrong").await).await;
    assert!(page.contains(r#"id="mode-key" checked"#), "{page}");

    let page = body(submit(&app, "username=jdoe&password=wrong").await).await;
    assert!(page.contains(r#"id="mode-ldap" checked"#));
    assert_eq!(page.matches(r#"class="card""#).count(), 1);
}

// The UI is a script, not a server-rendered form: it has nowhere to read the
// CSRF token from but `GET /auth`, and its writes only pass with it.
#[tokio::test]
async fn a_session_learns_its_csrf_token_from_auth_and_writes_with_it() {
    let app = app(Some(ldap()));
    let resp = submit(&app, "username=jdoe&password=right").await;
    let session = cookie(&resp, "uapi_session").expect("a session cookie");

    let (status, json) = get_json(&app, "/api/v1/auth", &session).await;
    assert_eq!(status, StatusCode::OK);
    let token = json["me"]["csrf_token"]
        .as_str()
        .expect("a session is given its token")
        .to_string();

    let sync = |csrf: Option<&str>| {
        let mut request = Request::post("/api/v1/sources/src-unknown/sync")
            .header(header::COOKIE, format!("uapi_session={session}"));
        if let Some(csrf) = csrf {
            request = request.header("x-csrf-token", csrf);
        }
        app.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    assert_eq!(sync(None).await.unwrap().status(), StatusCode::FORBIDDEN);
    // Past the CSRF check: refused only because the source does not exist.
    assert_eq!(
        sync(Some(&token)).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

// A federation map is pushed by another instance, with a key sent on the
// request. A browser session cannot push one, however it signed in: one
// opened with a key carried no key identity, and its push was stored under
// the key's label, which could replace a different key's entry.
#[tokio::test]
async fn a_browser_session_cannot_push_a_federation_map() {
    let app = app(Some(ldap()));
    let push = serde_json::json!({
        "url": "https://edge.example",
        "topology": {"version": "0.35.0", "peers": []},
    })
    .to_string();
    for login in ["key=awx-secret", "username=jdoe&password=right"] {
        let resp = submit(&app, login).await;
        let session = cookie(&resp, "uapi_session").expect("a session cookie");
        let (_, json) = get_json(&app, "/api/v1/auth", &session).await;
        let token = json["me"]["csrf_token"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(
                Request::put("/api/v1/topology/federation")
                    .header(header::COOKIE, format!("uapi_session={session}"))
                    .header("x-csrf-token", token)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(push.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{login}");
        assert!(body(resp).await.contains("with an API key"), "{login}");
    }

    // The same push with the key itself is accepted.
    let resp = app
        .clone()
        .oneshot(
            Request::put("/api/v1/topology/federation")
                .header("x-api-key", "awx-secret")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(push))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_header_caller_is_given_no_csrf_token() {
    let resp = app(Some(ldap()))
        .oneshot(
            Request::get("/api/v1/auth")
                .header("x-api-key", "awx-secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body(resp).await).unwrap();
    assert!(json["me"].get("csrf_token").is_none());
}

// Five wrong passwords and the form stops asking the directory about that
// account: a script cannot lock a real account out through this form, and
// the right password does not get in either until the window passes. Paused
// time: the fixed failure delay would otherwise make this test take seconds.
#[tokio::test(start_paused = true)]
async fn repeated_wrong_passwords_lock_the_form_not_the_account() {
    let app = app(Some(ldap()));
    for _ in 0..5 {
        let resp = submit(&app, "username=jdoe&password=wrong").await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let resp = submit(&app, "username=JDoe&password=right").await;
    assert_eq!(
        resp.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "case is not identity"
    );
    assert!(cookie(&resp, "uapi_session").is_none());

    // Another account is unaffected.
    let resp = submit(&app, "username=asmith&password=right").await;
    assert!(cookie(&resp, "uapi_session").is_some());
}

// A wrong password and an unknown user take the same time to answer, so
// timing cannot tell which usernames exist.
#[tokio::test(start_paused = true)]
async fn unknown_users_and_wrong_passwords_take_the_same_time() {
    let app = app(Some(ldap()));
    let timed = |fields: &'static str| {
        let app = app.clone();
        async move {
            let started = tokio::time::Instant::now();
            let resp = submit(&app, fields).await;
            (resp.status(), started.elapsed())
        }
    };
    let (unknown, t_unknown) = timed("username=nobody-at-all&password=x").await;
    let (wrong, t_wrong) = timed("username=jdoe&password=x").await;
    assert_eq!(unknown, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong, StatusCode::UNAUTHORIZED);
    assert_eq!(t_unknown.as_millis() / 100, t_wrong.as_millis() / 100);
}

// With /metrics behind authentication, a signed-in browser reads it with its
// session (the UI's Metrics page), and nobody else gets in without a key.
#[tokio::test]
async fn a_session_reads_protected_metrics() {
    let ui = unified_api::config::UiConfig {
        enabled: true,
        ldap: Some(ldap()),
        ..Default::default()
    };
    let app = unified_api::AppBuilder::new()
        .sources(HashMap::new())
        .ui(ui)
        .metrics_require_auth(true)
        .secrets(Arc::new(MockSecrets::new().with(
            "cred-ldap",
            [("username", "CN=svc-unified,DC=x"), ("password", "svc")],
        )))
        .directory(Arc::new(Directory))
        .api_keys(vec![ResolvedApiKey {
            id: "AWX".to_string(),
            name: "AWX".to_string(),
            secret: "awx-secret".to_string(),
            permissions: Permissions::Admin,
        }])
        .build();
    let resp = submit(&app, "username=jdoe&password=right").await;
    let session = cookie(&resp, "uapi_session").expect("a session cookie");

    let scrape = |cookie: Option<String>| {
        let mut request = Request::get("/metrics");
        if let Some(c) = cookie {
            request = request.header(header::COOKIE, format!("uapi_session={c}"));
        }
        app.clone().oneshot(request.body(Body::empty()).unwrap())
    };
    assert_eq!(
        scrape(Some(session)).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(
        scrape(None).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        scrape(Some("forged".to_string())).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}
