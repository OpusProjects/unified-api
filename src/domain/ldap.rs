use serde::{Deserialize, Serialize};
use std::collections::HashSet;

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
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LdapConfig {
    // ldaps://host:636, or ldap://host:389 with `starttls: true`. A plain
    // ldap:// without StartTLS is refused at validation: it would send every
    // password that logs in across the network in the clear.
    pub url: String,

    #[serde(default)]
    pub starttls: bool,

    // PEM file with the CA that signed the directory's certificate, for a
    // private CA. Unset = the system's trust store.
    #[serde(default)]
    pub ca_file: Option<String>,

    // credentials.yaml id whose `username` is the bind DN of the service
    // account that looks users up, and whose `password` is its password.
    pub bind_credential: String,

    // Where users are searched for, e.g. "OU=Users,DC=example,DC=com".
    pub search_base: String,

    // `{username}` is replaced by what was typed, escaped (RFC 4515) so a
    // username cannot rewrite the filter. Active Directory's default.
    #[serde(default = "default_user_filter")]
    pub user_filter: String,

    // Attribute listing the DNs of the user's groups.
    #[serde(default = "default_group_attribute")]
    pub group_attribute: String,
    // Attribute holding the account's own name. Access rules' `users:` and
    // the session's name use ITS value, not what was typed: with a filter
    // that matches on several attributes (an e-mail address, say), what was
    // typed is not the account's name, and must not be able to match a rule
    // written for someone else's.
    #[serde(default = "default_username_attribute")]
    pub username_attribute: String,

    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,

    // Who may log in, and with what. Evaluated top to bottom, all matches
    // combined: admin anywhere is admin; otherwise the scoped lists are
    // unioned. Matching nothing means the login is refused, even with a
    // correct password.
    #[serde(default)]
    pub access: Vec<AccessRule>,
}

fn default_user_filter() -> String {
    "(sAMAccountName={username})".to_string()
}

fn default_username_attribute() -> String {
    "sAMAccountName".to_string()
}

fn default_group_attribute() -> String {
    "memberOf".to_string()
}

fn default_timeout_seconds() -> u64 {
    10
}

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

// The CN of a DN: "CN=DL-ops,OU=Groups,DC=x" → "DL-ops". Escaped commas
// ("CN=Smith\, Jo,...") stay part of the value.
fn common_name(dn: &str) -> Option<&str> {
    let first = split_rdn(dn);
    let (attr, value) = first.split_once('=')?;
    attr.trim()
        .eq_ignore_ascii_case("cn")
        .then_some(value.trim())
}

fn split_rdn(dn: &str) -> &str {
    let mut escaped = false;
    for (index, ch) in dn.char_indices() {
        match ch {
            '\\' if !escaped => escaped = true,
            ',' if !escaped => return &dn[..index],
            _ => escaped = false,
        }
    }
    dn
}

fn group_matches(wanted: &str, member_of: &[String]) -> bool {
    if wanted.contains('=') {
        member_of.iter().any(|dn| dn.eq_ignore_ascii_case(wanted))
    } else {
        member_of
            .iter()
            .filter_map(|dn| common_name(dn))
            .any(|cn| cn.eq_ignore_ascii_case(wanted))
    }
}

// The rules applied to one authenticated person. None = no rule names them.
pub fn grant_for(rules: &[AccessRule], username: &str, member_of: &[String]) -> Option<Grant> {
    let mut matched = false;
    let mut sources = HashSet::new();
    let mut endpoints = HashSet::new();

    for rule in rules {
        let by_user = rule.users.iter().any(|u| u.eq_ignore_ascii_case(username));
        let by_group = rule.groups.iter().any(|g| group_matches(g, member_of));
        if !(by_user || by_group) {
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

// The user filter with the username substituted, escaped so `*`, `(` or `\`
// in a username are literals rather than filter syntax: without it, typing
// `*` as a username would match the first account the search returns.
pub fn user_filter(template: &str, username: &str) -> String {
    template.replace("{username}", &escape_filter_value(username))
}

// RFC 4515: the five characters with a meaning inside a filter value become
// \XX. Written here rather than borrowed from the LDAP client so the domain
// keeps no dependency on an adapter's crate.
fn escape_filter_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '*' => out.push_str("\\2a"),
            '(' => out.push_str("\\28"),
            ')' => out.push_str("\\29"),
            '\\' => out.push_str("\\5c"),
            '\0' => out.push_str("\\00"),
            other => out.push(other),
        }
    }
    out
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

    #[test]
    fn an_escaped_comma_stays_inside_the_cn() {
        assert_eq!(
            common_name("CN=Ops\\, Spain,OU=Groups,DC=x"),
            Some("Ops\\, Spain")
        );
        assert_eq!(common_name("OU=Groups,DC=x"), None);
    }

    #[test]
    fn filter_syntax_in_a_username_is_escaped() {
        assert_eq!(
            user_filter("(sAMAccountName={username})", "*)(uid=*"),
            "(sAMAccountName=\\2a\\29\\28uid=\\2a)"
        );
    }
}
