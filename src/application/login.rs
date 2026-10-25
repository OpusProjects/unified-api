use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::domain::ldap::{Grant, LdapConfig, grant_for};
use crate::ports::directory::{DirectoryError, DirectoryLogin, DirectoryPort};
use crate::ports::secrets::SecretsPort;

// The use case "log a person in with their directory account".
//
// Three steps, each owned elsewhere: the service account comes from the
// secrets chain (like any connector credential), the directory checks the
// password, and the configured rules decide what the person may touch. This
// is where they meet, so the HTTP handler only translates a form.

#[derive(Debug)]
pub struct LoggedIn {
    pub username: String,
    pub dn: String,
    pub grant: Grant,
}

#[derive(Debug, PartialEq)]
pub enum LoginError {
    // Wrong password or unknown user — deliberately one answer.
    InvalidCredentials,
    // The password was right, but no `access` rule names this person.
    NotAuthorized,
    // The directory or the service account's secret could not be reached;
    // the message is for the log.
    Unavailable(String),
    // This account failed too many times lately: refused without asking
    // the directory, until the window passes.
    TooManyAttempts,
    // Too many logins already in flight.
    Busy,
}

// Failed passwords allowed per account in a window before the form stops
// asking the directory about it. Well under any sensible directory lockout,
// so a script cannot use this form to lock a real account out.
const MAX_FAILURES: u32 = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
// Directory logins in flight at once: each is a TLS connection and two binds.
const MAX_CONCURRENT: usize = 4;
// Every failed login answers after this long in total, however far it got:
// an unknown user (one search) and a wrong password (one more bind) must
// take the same time, or the time says which usernames exist.
pub const FAILURE_DURATION: Duration = Duration::from_millis(1500);

// What the login form guards itself with: recent failures per account, and
// a cap on concurrent directory logins. In memory and per process; it
// forgets on restart, which is acceptable for a rate limit.
pub struct LoginThrottle {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    in_flight: tokio::sync::Semaphore,
}

impl Default for LoginThrottle {
    fn default() -> Self {
        LoginThrottle {
            failures: Mutex::new(HashMap::new()),
            in_flight: tokio::sync::Semaphore::new(MAX_CONCURRENT),
        }
    }
}

impl LoginThrottle {
    // Accounts are named however they were typed; case is not identity.
    fn key(username: &str) -> String {
        username.trim().to_lowercase()
    }

    // Whether this account has failed too often lately to be asked about.
    fn blocked(&self, username: &str) -> bool {
        let mut failures = self.failures.lock().expect("login throttle lock");
        match failures.get(&Self::key(username)) {
            Some((count, since)) if since.elapsed() < FAILURE_WINDOW => *count >= MAX_FAILURES,
            Some(_) => {
                failures.remove(&Self::key(username));
                false
            }
            None => false,
        }
    }

    fn failed(&self, username: &str) {
        let mut failures = self.failures.lock().expect("login throttle lock");
        // Forget windows that have closed, so the map does not grow with
        // every name ever typed at the form.
        failures.retain(|_, (_, since)| since.elapsed() < FAILURE_WINDOW);
        let entry = failures
            .entry(Self::key(username))
            .or_insert((0, Instant::now()));
        entry.0 += 1;
    }

    fn succeeded(&self, username: &str) {
        self.failures
            .lock()
            .expect("login throttle lock")
            .remove(&Self::key(username));
    }
}

// ldap_login behind the throttle: refuses an account that has failed too
// often without asking the directory, caps concurrent logins, and makes
// every wrong password take the same total time.
pub async fn throttled_ldap_login(
    throttle: &LoginThrottle,
    secrets: &dyn SecretsPort,
    directory: &dyn DirectoryPort,
    config: &LdapConfig,
    username: &str,
    password: &str,
) -> Result<LoggedIn, LoginError> {
    let started = Instant::now();
    if throttle.blocked(username) {
        return Err(LoginError::TooManyAttempts);
    }
    let Ok(_permit) =
        tokio::time::timeout(Duration::from_secs(5), throttle.in_flight.acquire()).await
    else {
        return Err(LoginError::Busy);
    };

    let result = ldap_login(secrets, directory, config, username, password).await;
    match &result {
        Err(LoginError::InvalidCredentials) => {
            throttle.failed(username);
            tokio::time::sleep(FAILURE_DURATION.saturating_sub(started.elapsed())).await;
        }
        Ok(_) | Err(LoginError::NotAuthorized) => throttle.succeeded(username),
        _ => {}
    }
    result
}

pub async fn ldap_login(
    secrets: &dyn SecretsPort,
    directory: &dyn DirectoryPort,
    config: &LdapConfig,
    username: &str,
    password: &str,
) -> Result<LoggedIn, LoginError> {
    let username = username.trim();

    let bind = secrets
        .resolve(&config.bind_credential)
        .await
        .map_err(|e| {
            LoginError::Unavailable(format!(
                "LDAP bind credential '{}': {}",
                config.bind_credential, e.message
            ))
        })?;
    let (Some(bind_dn), Some(bind_password)) = (bind.get("username"), bind.get("password")) else {
        return Err(LoginError::Unavailable(format!(
            "LDAP bind credential '{}' must provide `username` (the bind DN) and `password`",
            config.bind_credential
        )));
    };

    let user = directory
        .authenticate(DirectoryLogin {
            config,
            bind_dn,
            bind_password,
            username,
            password,
        })
        .await
        .map_err(|e| match e {
            DirectoryError::InvalidCredentials => LoginError::InvalidCredentials,
            DirectoryError::Unavailable(message) => LoginError::Unavailable(message),
        })?;

    // The directory's name for the account, not the typed spelling: rules
    // and the session name follow the account (see `username_attribute`).
    let username = user.username.as_deref().unwrap_or(username);

    let grant = grant_for(&config.access, username, &user.member_of).ok_or_else(|| {
        tracing::warn!(
            username,
            groups = user.member_of.len(),
            "LDAP login refused: no access rule names this user or their groups"
        );
        LoginError::NotAuthorized
    })?;

    Ok(LoggedIn {
        username: username.to_string(),
        dn: user.dn,
        grant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::secrets::mock::MockSecrets;
    use crate::domain::api_key::ApiKeyRole;
    use crate::domain::ldap::AccessRule;
    use crate::ports::directory::{DirectoryFuture, DirectoryUser};

    // A directory that knows one person: jdoe / right-password, in DL-ops.
    struct OnePerson;

    impl DirectoryPort for OnePerson {
        fn authenticate<'a>(&'a self, request: DirectoryLogin<'a>) -> DirectoryFuture<'a> {
            Box::pin(async move {
                assert_eq!(request.bind_dn, "CN=svc-unified");
                if request.username == "jdoe" && request.password == "right-password" {
                    Ok(DirectoryUser {
                        dn: "CN=John Doe,OU=Users,DC=x".to_string(),
                        username: Some("jdoe".to_string()),
                        member_of: vec!["CN=DL-ops,OU=Groups,DC=x".to_string()],
                    })
                } else {
                    Err(DirectoryError::InvalidCredentials)
                }
            })
        }
    }

    fn config(groups: &[&str]) -> LdapConfig {
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
            access: vec![AccessRule {
                name: "ops".to_string(),
                groups: groups.iter().map(|g| g.to_string()).collect(),
                users: Vec::new(),
                role: ApiKeyRole::Admin,
                sources: Vec::new(),
                endpoints: Vec::new(),
            }],
        }
    }

    fn secrets() -> MockSecrets {
        MockSecrets::new().with(
            "cred-ldap",
            [("username", "CN=svc-unified"), ("password", "svc-secret")],
        )
    }

    #[tokio::test]
    async fn a_member_of_an_admin_group_logs_in_as_admin() {
        let logged = ldap_login(
            &secrets(),
            &OnePerson,
            &config(&["DL-ops"]),
            " jdoe ",
            "right-password",
        )
        .await
        .expect("login");
        assert_eq!(logged.username, "jdoe");
        assert_eq!(logged.grant, Grant::Admin);
    }

    #[tokio::test]
    async fn a_wrong_password_is_invalid_credentials() {
        let err = ldap_login(&secrets(), &OnePerson, &config(&["DL-ops"]), "jdoe", "nope")
            .await
            .unwrap_err();
        assert_eq!(err, LoginError::InvalidCredentials);
    }

    // The directory accepting the password is not access: only a rule is.
    #[tokio::test]
    async fn a_right_password_without_a_rule_is_not_authorized() {
        let err = ldap_login(
            &secrets(),
            &OnePerson,
            &config(&["DL-other"]),
            "jdoe",
            "right-password",
        )
        .await
        .unwrap_err();
        assert_eq!(err, LoginError::NotAuthorized);
    }

    #[tokio::test]
    async fn a_missing_bind_credential_is_unavailable_and_named() {
        let err = ldap_login(
            &MockSecrets::new(),
            &OnePerson,
            &config(&["DL-ops"]),
            "jdoe",
            "x",
        )
        .await
        .unwrap_err();
        let LoginError::Unavailable(message) = err else {
            panic!("expected Unavailable, got {err:?}");
        };
        assert!(message.contains("cred-ldap"), "{message}");
    }

    // The directory names whoever the filter found; rules follow that name.
    struct Named(&'static str);

    impl DirectoryPort for Named {
        fn authenticate<'a>(&'a self, _request: DirectoryLogin<'a>) -> DirectoryFuture<'a> {
            let name = self.0;
            Box::pin(async move {
                Ok(DirectoryUser {
                    dn: format!("CN={name},DC=x"),
                    username: Some(name.to_string()),
                    member_of: Vec::new(),
                })
            })
        }
    }

    fn named_rule(users: &[&str]) -> LdapConfig {
        let mut cfg = config(&[]);
        cfg.access[0].groups.clear();
        cfg.access[0].users = users.iter().map(|u| u.to_string()).collect();
        cfg
    }

    #[tokio::test]
    async fn user_rules_match_the_account_the_directory_found_not_the_typing() {
        let secrets = MockSecrets::new().with(
            "cred-ldap",
            [("username", "CN=svc-unified"), ("password", "svc")],
        );
        // Typed someone else's name, but the directory says it is "mallory":
        // the rule written for "jdoe" must not apply.
        let refused = ldap_login(
            &secrets,
            &Named("mallory"),
            &named_rule(&["jdoe"]),
            "jdoe",
            "pw",
        )
        .await;
        assert_eq!(refused.unwrap_err(), LoginError::NotAuthorized);

        // Typed an e-mail address the filter maps to jdoe: jdoe's rule applies,
        // and the session is named after the account.
        let logged = ldap_login(
            &secrets,
            &Named("jdoe"),
            &named_rule(&["jdoe"]),
            "j.doe@x.com",
            "pw",
        )
        .await
        .expect("jdoe's rule");
        assert_eq!(logged.username, "jdoe");
    }
}
