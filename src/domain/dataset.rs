use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// A Dataset is a complete Ansible inventory, as produced by a connector.
// Serialize + Deserialize: can be converted to/from JSON/YAML in both directions.
// Clone: allows making copies of the struct (Rust defaults to moving, not copying).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Dataset {
    // HashMap<String, HostVars> = dict[str, HostVars] in Python
    // Maps hostname → its variables
    #[serde(default)]
    pub hostvars: HashMap<String, HostVars>,

    // HashMap<String, Group> = the inventory groups (dc06, oraclelinux8, etc.)
    #[serde(default)]
    pub groups: HashMap<String, Group>,

    // Hosts to delete — only the enricher uses this
    // Normal connectors do not return this
    #[serde(default)]
    pub remove_hosts: Vec<String>,
}

// A host's variables: a free key-value dictionary
// serde_json::Value is like "any" — can be string, number, bool, list, etc.
pub type HostVars = HashMap<String, serde_json::Value>;

// `all` and `ungrouped` are Ansible's implicit meta-groups: "every host" and
// "no group at all". They describe the inventory rather than recording a
// membership anyone stated, so they are skipped wherever membership is read
// or written.
pub fn is_meta_group(name: &str) -> bool {
    name == "all" || name == "ungrouped"
}

// An Ansible inventory group
// Default: an empty group (no hosts, no children, no vars) — what a group
// recreated for a retained host starts from.
#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct Group {
    // Vec<String> = list[str] in Python
    // #[serde(default)] = if it doesn't appear in YAML/JSON, uses an empty Vec
    #[serde(default)]
    pub hosts: Vec<String>,

    #[serde(default)]
    pub children: Vec<String>,

    // Option<HostVars> = can exist or not (like Optional in Python)
    // None = has no group variables, Some({...}) = has them
    #[serde(default)]
    pub vars: Option<HostVars>,
}

impl Dataset {
    // Convert standard Ansible dynamic inventory JSON into a Dataset:
    //
    //   {"_meta": {"hostvars": {...}}, "web": {"hosts": [...]}, ...}
    //
    // Differences handled:
    // - hostvars live under `_meta.hostvars` instead of top-level `hostvars`
    // - groups are top-level keys instead of living under `groups`
    // - a group may be the modern object form ({hosts, children, vars}) or the
    //   legacy shorthand: a plain list of hostnames
    // - a host named only in a group is a host: `ansible-inventory --list`
    //   leaves hosts without variables out of `_meta.hostvars`, so each one
    //   gets an empty entry rather than vanishing
    // - `all` and `ungrouped` are meta-groups Ansible manages implicitly, so
    //   they are skipped — but their hosts are still hosts, and vars set on
    //   them are reported as a warning, since the Dataset has nowhere to put
    //   them (their children never are: every group is a child of `all`)
    //
    // Malformed input is an ERROR, never a silent skip: a group that doesn't
    // parse, or carries a key Ansible does not read there (`host` for
    // `hosts`), aborts the conversion naming the group. Returns the dataset
    // plus human-readable warnings; the caller decides how to surface them
    // (the domain stays free of logging dependencies).
    pub fn from_ansible_inventory(value: serde_json::Value) -> Result<(Self, Vec<String>), String> {
        let obj = value
            .as_object()
            .ok_or("expected a JSON object at the top level")?;

        // The shape this type serializes to, {"hostvars", "groups"}, which a
        // script could print before 0.36. Read as Ansible it is groups named
        // hostvars and groups and no host variables — a sync that "works"
        // and serves junk — so it is refused, saying what to print.
        if !obj.contains_key("_meta")
            && (obj.contains_key("hostvars") || obj.contains_key("groups"))
        {
            return Err(
                "this is the {\"hostvars\", \"groups\"} format, which 0.36 no longer \
                 reads: print Ansible inventory JSON, with host variables under \
                 _meta.hostvars and each group as a top-level key"
                    .to_string(),
            );
        }

        let mut warnings: Vec<String> = Vec::new();

        let mut hostvars: HashMap<String, HostVars> = match obj.get("_meta") {
            // `_meta` without hostvars is accepted, as Ansible accepts it:
            // nothing has variables.
            Some(meta) => match meta.get("hostvars") {
                Some(raw) => serde_json::from_value(raw.clone())
                    .map_err(|e| format!("_meta.hostvars is malformed: {}", e))?,
                None => HashMap::new(),
            },
            None => {
                warnings.push(
                    "no _meta.hostvars in inventory output — hosts will have no variables"
                        .to_string(),
                );
                HashMap::new()
            }
        };

        let mut groups: HashMap<String, Group> = HashMap::new();
        let mut members: Vec<String> = Vec::new();

        for (name, group_value) in obj {
            if name == "_meta" {
                continue;
            }
            let group = parse_group(name, group_value)?;
            members.extend(group.hosts.iter().cloned());
            if is_meta_group(name) {
                // These exist implicitly in every Ansible inventory; keeping
                // them would duplicate membership.
                if group.vars.as_ref().is_some_and(|v| !v.is_empty()) {
                    warnings.push(format!(
                        "meta-group '{}' carries vars that are dropped in the conversion",
                        name
                    ));
                }
                continue;
            }
            groups.insert(name.clone(), group);
        }

        for host in members {
            hostvars.entry(host).or_default();
        }

        Ok((
            Dataset {
                hostvars,
                groups,
                remove_hosts: Vec::new(),
            },
            warnings,
        ))
    }
}

// One top-level group of Ansible inventory JSON: the object form, with only
// the keys Ansible reads there, or the legacy list of hostnames.
fn parse_group(name: &str, value: &serde_json::Value) -> Result<Group, String> {
    match value {
        // Legacy shorthand: "web": ["host1", "host2"]
        serde_json::Value::Array(items) => {
            let hosts: Vec<String> = items
                .iter()
                .map(|item| {
                    item.as_str().map(str::to_string).ok_or_else(|| {
                        format!(
                            "group '{}' uses the list form but contains a non-string entry",
                            name
                        )
                    })
                })
                .collect::<Result<_, _>>()?;
            Ok(Group {
                hosts,
                children: Vec::new(),
                vars: None,
            })
        }
        serde_json::Value::Object(keys) => {
            // Group is lenient on purpose (it also reads cache snapshots and
            // other instances' datasets), so a script's typo is caught here.
            if let Some(unknown) = keys
                .keys()
                .find(|k| !matches!(k.as_str(), "hosts" | "children" | "vars"))
            {
                return Err(format!(
                    "group '{}' has a key '{}' — a group takes hosts, children and vars",
                    name, unknown
                ));
            }
            serde_json::from_value(value.clone())
                .map_err(|e| format!("group '{}' is malformed: {}", name, e))
        }
        other => Err(format!(
            "group '{}' must be an object or a list of hostnames, got {}",
            name,
            json_type_name(other)
        )),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_full_ansible_inventory() {
        let raw = serde_json::json!({
            "_meta": {
                "hostvars": {
                    "motoko.section9.net": {"ansible_host": "10.9.1.1", "os": "OracleLinux"},
                    "batou.section9.net": {"ansible_host": "10.9.1.2"}
                }
            },
            "section9": {
                "hosts": ["motoko.section9.net", "batou.section9.net"],
                "vars": {"ntp_server": "ntp.section9.net"}
            },
            "commanders": {
                "hosts": ["motoko.section9.net"],
                "children": ["section9"]
            }
        });

        let (dataset, warnings) = Dataset::from_ansible_inventory(raw).unwrap();

        assert!(warnings.is_empty());
        assert_eq!(dataset.hostvars.len(), 2);
        assert_eq!(
            dataset.hostvars["motoko.section9.net"]["ansible_host"],
            "10.9.1.1"
        );
        assert_eq!(dataset.groups.len(), 2);
        assert_eq!(dataset.groups["section9"].hosts.len(), 2);
        assert_eq!(dataset.groups["commanders"].children, vec!["section9"]);
        assert_eq!(
            dataset.groups["section9"].vars.as_ref().unwrap()["ntp_server"],
            "ntp.section9.net"
        );
    }

    #[test]
    fn accepts_legacy_list_form_groups() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "web": ["web01.example.com", "web02.example.com"]
        });

        let (dataset, _) = Dataset::from_ansible_inventory(raw).unwrap();
        assert_eq!(
            dataset.groups["web"].hosts,
            vec!["web01.example.com", "web02.example.com"]
        );
    }

    #[test]
    fn skips_all_and_ungrouped_meta_groups() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "all": {"children": ["web", "ungrouped"]},
            "ungrouped": {"hosts": []},
            "web": {"hosts": ["web01.example.com"]}
        });

        let (dataset, warnings) = Dataset::from_ansible_inventory(raw).unwrap();
        assert_eq!(dataset.groups.len(), 1);
        assert!(dataset.groups.contains_key("web"));
        // What `ansible-inventory --list` always prints: `all`'s children
        // are every group, so dropping them loses nothing and says nothing.
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn vars_on_a_meta_group_are_said_aloud() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "all": {"vars": {"ntp": "ntp.example"}}
        });
        let (_, warnings) = Dataset::from_ansible_inventory(raw).unwrap();
        assert!(warnings.iter().any(|w| w.contains("'all'")), "{warnings:?}");
    }

    // `ansible-inventory --list` leaves a host with no variables out of
    // `_meta.hostvars`: it is named only in its groups, `ungrouped` and
    // `all` included, and it is a host all the same.
    #[test]
    fn a_host_named_only_in_a_group_is_a_host() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {"a": {"x": 1}}},
            "web": {"hosts": ["a", "b"]},
            "all": {"hosts": ["c"]},
            "ungrouped": {"hosts": ["d"]}
        });
        let (dataset, _) = Dataset::from_ansible_inventory(raw).unwrap();
        let mut hosts: Vec<_> = dataset.hostvars.keys().cloned().collect();
        hosts.sort();
        assert_eq!(hosts, ["a", "b", "c", "d"]);
        assert_eq!(dataset.hostvars["a"]["x"], 1, "variables are kept");
        assert!(dataset.hostvars["b"].is_empty());
    }

    #[test]
    fn meta_without_hostvars_means_no_variables() {
        let raw = serde_json::json!({"_meta": {}, "web": ["h1"]});
        let (dataset, warnings) = Dataset::from_ansible_inventory(raw).unwrap();
        assert!(dataset.hostvars["h1"].is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn a_key_a_group_does_not_take_is_an_error() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "web": {"host": ["h1"]}
        });
        let err = Dataset::from_ansible_inventory(raw).unwrap_err();
        assert!(err.contains("group 'web' has a key 'host'"), "{err}");
    }

    #[test]
    fn missing_meta_warns_instead_of_failing() {
        let raw = serde_json::json!({
            "web": {"hosts": ["web01.example.com"]}
        });

        let (dataset, warnings) = Dataset::from_ansible_inventory(raw).unwrap();
        assert_eq!(dataset.groups.len(), 1);
        assert!(dataset.hostvars["web01.example.com"].is_empty());
        assert!(warnings.iter().any(|w| w.contains("_meta")));
    }

    #[test]
    fn malformed_group_is_an_error_not_a_silent_skip() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "web": {"hosts": "not-a-list"}
        });

        let err = Dataset::from_ansible_inventory(raw).unwrap_err();
        assert!(err.contains("group 'web'"), "error was: {}", err);
    }

    #[test]
    fn list_group_with_non_string_entry_is_an_error() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "web": ["web01.example.com", 42]
        });

        let err = Dataset::from_ansible_inventory(raw).unwrap_err();
        assert!(err.contains("non-string"), "error was: {}", err);
    }

    #[test]
    fn scalar_group_is_an_error() {
        let raw = serde_json::json!({
            "_meta": {"hostvars": {}},
            "web": "oops"
        });

        let err = Dataset::from_ansible_inventory(raw).unwrap_err();
        assert!(err.contains("got a string"), "error was: {}", err);
    }

    #[test]
    fn non_object_top_level_is_an_error() {
        let err = Dataset::from_ansible_inventory(serde_json::json!([1, 2])).unwrap_err();
        assert!(err.contains("JSON object"));
    }

    #[test]
    fn the_hostvars_groups_shape_is_refused() {
        for old in [
            serde_json::json!({"hostvars": {"h1": {"a": 1}}, "groups": {}}),
            serde_json::json!({"groups": {"web": {"hosts": ["h1"]}}}),
        ] {
            let err = Dataset::from_ansible_inventory(old).unwrap_err();
            assert!(err.contains("_meta.hostvars"), "{err}");
        }
    }
}
