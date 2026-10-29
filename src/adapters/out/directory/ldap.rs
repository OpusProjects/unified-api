// DirectoryPort → an LDAP server (search-then-bind, one connection per
// login). The adapter lives in unified-auth, shared with unified-gitops.
pub use unified_auth::ldap_directory::LdapDirectory;
