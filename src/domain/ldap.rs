use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use auth_rs::rule_names;

use crate::domain::api_key::ApiKeyRole;

// LDAP login for the browsable API — config.yaml, `server.ui.ldap`.
//
// People log in with their directory account; what they may touch is decided
// HERE, by the `access` rules, in the same vocabulary api_keys.yaml uses
// (admin, or restricted to listed sources and endpoints). The directory says
// who someone is and which groups they belong to; it never decides access on
// its own, so a new group in AD grants nothing until a rule names it.
//
// The service account's password is not in this block: `bind_credential`
// names a credentials.yaml entry, resolved through the same secrets chain
// (env, file, Vault) every connector credential goes through.
//
// The block itself, the directory search and the group matching live in
// auth-rs, shared with unified-gitops; what stays here is what only this
// service has: the rule type (sources and endpoints) and what it grants.
// The `server.ui.ldap` block, with this service's rules as its `access:`.
pub type LdapConfig = auth_rs::LdapConfig<AccessRule>;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AccessRule {
    // A label for the Users page and the logs, e.g. "PCD operations".
    pub name: String,

    // Group CNs ("DL-pcol2operations") or full DNs. A CN matches any group
    // with that CN; a DN only that group. Case-insensitive, as the directory is.
    #[serde(default)]
    pub groups: Vec<String>,

    // Usernames, for the odd person who needs access without a group.
    #[serde(default)]
    pub users: Vec<String>,

    // Restricted unless written: a rule that forgets `role:` must never hand
    // out admin, the same default api_keys.yaml has.
    #[serde(default)]
    pub role: ApiKeyRole,

    #[serde(default)]
    pub sources: Vec<String>,

    #[serde(default)]
    pub endpoints: Vec<String>,
}

// What the rules grant one person. Kept free of the HTTP layer's Permissions
// type (domain does not depend on adapters); the login handler converts.
#[derive(Debug, Clone, PartialEq)]
pub enum Grant {
    Admin,
    Scoped {
        sources: HashSet<String>,
        endpoints: HashSet<String>,
    },
}

// The rules applied to one authenticated person. None = no rule names them.
pub fn grant_for(rules: &[AccessRule], username: &str, member_of: &[String]) -> Option<Grant> {
    let mut matched = false;
    let mut sources = HashSet::new();
    let mut endpoints = HashSet::new();

    for rule in rules {
        if !rule_names(&rule.users, &rule.groups, username, member_of) {
            continue;
        }
        if rule.role == ApiKeyRole::Admin {
            return Some(Grant::Admin);
        }
        matched = true;
        sources.extend(rule.sources.iter().cloned());
        endpoints.extend(rule.endpoints.iter().cloned());
    }

    matched.then_some(Grant::Scoped { sources, endpoints })
}

// What the shared login use case asks of this service's rules.
impl auth_rs::AccessRule for AccessRule {
    type Grant = Grant;

    fn grant_for(rules: &[Self], username: &str, member_of: &[String]) -> Option<Grant> {
        grant_for(rules, username, member_of)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(name: &str, role: ApiKeyRole) -> AccessRule {
        AccessRule {
            name: name.to_string(),
            groups: Vec::new(),
            users: Vec::new(),
            role,
            sources: Vec::new(),
            endpoints: Vec::new(),
        }
    }

    const OPS: &str = "CN=DL-pcol2operations,OU=Groups,DC=example,DC=com";

    #[test]
    fn a_group_cn_matches_the_users_group_dn() {
        let mut admins = rule("ops", ApiKeyRole::Admin);
        admins.groups = vec!["dl-PCOL2operations".to_string()];
        assert_eq!(
            grant_for(&[admins], "jdoe", &[OPS.to_string()]),
            Some(Grant::Admin)
        );
    }

    #[test]
    fn a_full_dn_matches_only_that_group() {
        let mut admins = rule("ops", ApiKeyRole::Admin);
        admins.groups = vec!["CN=DL-pcol2operations,OU=Other,DC=example,DC=com".to_string()];
        assert_eq!(grant_for(&[admins], "jdoe", &[OPS.to_string()]), None);
    }

    #[test]
    fn nobody_named_by_a_rule_gets_nothing() {
        let mut admins = rule("ops", ApiKeyRole::Admin);
        admins.groups = vec!["DL-other".to_string()];
        assert_eq!(grant_for(&[admins], "jdoe", &[OPS.to_string()]), None);
    }

    #[test]
    fn scoped_rules_union_and_admin_wins() {
        let mut vmware = rule("vmware", ApiKeyRole::Restricted);
        vmware.users = vec!["JDoe".to_string()];
        vmware.sources = vec!["src-vmware".to_string()];
        let mut d42 = rule("d42", ApiKeyRole::Restricted);
        d42.groups = vec!["DL-pcol2operations".to_string()];
        d42.sources = vec!["src-d42".to_string()];
        d42.endpoints = vec!["ep-awx".to_string()];

        let grant = grant_for(&[vmware.clone(), d42.clone()], "jdoe", &[OPS.to_string()]);
        let Some(Grant::Scoped { sources, endpoints }) = grant else {
            panic!("expected a scoped grant, got {grant:?}");
        };
        assert!(sources.contains("src-vmware") && sources.contains("src-d42"));
        assert!(endpoints.contains("ep-awx"));

        let mut admins = rule("admins", ApiKeyRole::Admin);
        admins.users = vec!["jdoe".to_string()];
        assert_eq!(
            grant_for(&[vmware, admins, d42], "jdoe", &[]),
            Some(Grant::Admin)
        );
    }
}
