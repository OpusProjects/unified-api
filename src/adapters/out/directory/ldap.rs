// DirectoryPort → an LDAP server (search-then-bind, one connection per
// login). The adapter lives in auth-rs, shared with unified-gitops.
pub use auth_rs::ldap_directory::LdapDirectory;
