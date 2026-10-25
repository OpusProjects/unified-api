use std::future::Future;
use std::pin::Pin;

use crate::domain::ldap::LdapConfig;

// DirectoryPort — check a username and password against a user directory
// (LDAP / Active Directory) and say which groups the person belongs to.
//
// It answers who someone is, never what they may do: access is decided by
// the `access` rules in the configuration (domain::ldap::grant_for), so the
// port has no notion of roles at all.
pub trait DirectoryPort: Send + Sync {
    fn authenticate<'a>(&'a self, request: DirectoryLogin<'a>) -> DirectoryFuture<'a>;
}

pub type DirectoryFuture<'a> =
    Pin<Box<dyn Future<Output = Result<DirectoryUser, DirectoryError>> + Send + 'a>>;

pub struct DirectoryLogin<'a> {
    pub config: &'a LdapConfig,
    // The service account that searches for the user, resolved from
    // `bind_credential` by the caller.
    pub bind_dn: &'a str,
    pub bind_password: &'a str,
    pub username: &'a str,
    pub password: &'a str,
}

#[derive(Debug, Clone)]
pub struct DirectoryUser {
    pub dn: String,
    // The account's own name (`username_attribute`), when the entry has it.
    pub username: Option<String>,
    pub member_of: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub enum DirectoryError {
    // Unknown user or wrong password — one variant for both, because the
    // difference is exactly what someone guessing usernames wants to learn.
    InvalidCredentials,
    // The directory could not answer: unreachable, TLS refused, the service
    // account rejected, a timeout. The message is for the log, not the user.
    Unavailable(String),
}
