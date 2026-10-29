// The use case "log a person in with their directory account" lives in
// auth-rs, shared with unified-gitops: the service account comes from the
// secrets chain, the directory checks the password, and this service's rules
// (domain::ldap) decide what the person may touch. Re-exported here so the
// HTTP handler keeps calling `application::login`; the functions are generic
// over the rule type and take this service's `LdapConfig` as they are.
pub use auth_rs::login::{
    FAILURE_DURATION, LoginError, LoginThrottle, ldap_login, throttled_ldap_login,
};

use crate::domain::ldap::Grant;

// A signed-in person: their account name, DN, and what this service's rules
// granted them.
pub type LoggedIn = auth_rs::LoggedIn<Grant>;
