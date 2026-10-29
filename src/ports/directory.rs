// DirectoryPort — check a username and password against a user directory
// (LDAP / Active Directory) and say which groups the person belongs to — lives
// in unified-auth, shared with unified-gitops. Re-exported here so the ports
// layer still lists every interface the application depends on.
//
// It answers who someone is, never what they may do: access is decided by
// the `access` rules in the configuration (domain::ldap::grant_for).
pub use unified_auth::directory::{
    DirectoryError, DirectoryFuture, DirectoryLogin, DirectoryPort, DirectoryUser,
};
