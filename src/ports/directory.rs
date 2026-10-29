// DirectoryPort — check a username and password against a user directory
// (LDAP / Active Directory) and say which groups the person belongs to — lives
// in auth-rs, shared with unified-gitops. Re-exported here so the ports
// layer still lists every interface the application depends on.
//
// It answers who someone is, never what they may do: access is decided by
// the `access` rules in the configuration (domain::ldap::grant_for).
pub use auth_rs::directory::{
    DirectoryError, DirectoryFuture, DirectoryLogin, DirectoryPort, DirectoryUser,
};
