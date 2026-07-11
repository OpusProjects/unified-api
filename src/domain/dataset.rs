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

// An Ansible inventory group
#[derive(Debug, Serialize, Deserialize, Clone)]
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
    // - `all` and `ungrouped` are meta-groups Ansible manages implicitly, so
    //   they are skipped — but skipping data silently is how bugs hide, so
    //   anything dropped that carried information is reported as a warning
    //
    // Malformed input is an ERROR, never a silent skip: a group that doesn't
    // parse aborts the conversion naming the group. Returns the dataset plus
    // human-readable warnings; the caller decides how to surface them (the
    // domain stays free of logging dependencies).
    pub fn from_ansible_inventory(value: serde_json::Value) -> Result<(Self, Vec<String>), String> {
        let obj = value
            .as_object()
            .ok_or("expected a JSON object at the top level")?;

        let mut warnings: Vec<String> = Vec::new();

        let hostvars: HashMap<String, HostVars> = match obj.get("_meta") {
            Some(meta) => {
                let raw = meta
                    .get("hostvars")
                    .ok_or("_meta is present but has no hostvars key")?;
                serde_json::from_value(raw.clone())
                    .map_err(|e| format!("_meta.hostvars is malformed: {}", e))?
            }
            None => {
                warnings.push(
                    "no _meta.hostvars in inventory output — hosts will have no variables"
                        .to_string(),
                );
                HashMap::new()
            }
        };

        let mut groups: HashMap<String, Group> = HashMap::new();

        for (name, group_value) in obj {
            if name == "_meta" {
                continue;
            }
            if name == "all" || name == "ungrouped" {
                // These exist implicitly in every Ansible inventory; keeping
                // them would duplicate membership. Warn if dropping them
                // loses actual information (vars or children).
                let has_vars = group_value
                    .get("vars")
                    .and_then(|v| v.as_object())
                    .is_some_and(|o| !o.is_empty());
                let has_children = group_value
                    .get("children")
                    .and_then(|c| c.as_array())
                    .is_some_and(|c| !c.is_empty());
                if has_vars || has_children {
                    warnings.push(format!(
                        "meta-group '{}' carries vars/children that are dropped in the conversion",
                        name
                    ));
                }
                continue;
            }

            let group = match group_value {
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
                    Group {
                        hosts,
                        children: Vec::new(),
                        vars: None,
                    }
                }
                serde_json::Value::Object(_) => serde_json::from_value(group_value.clone())
                    .map_err(|e| format!("group '{}' is malformed: {}", name, e))?,
                other => {
                    return Err(format!(
                        "group '{}' must be an object or a list of hostnames, got {}",
                        name,
                        json_type_name(other)
                    ));
                }
            };

            groups.insert(name.clone(), group);
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

    // Cheap heuristic used to help misconfigured sources: an object with a
    // `_meta` key is almost certainly Ansible inventory output.
    pub fn looks_like_ansible_inventory(value: &serde_json::Value) -> bool {
        value.get("_meta").is_some()
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
        // `all` carried children, and dropping information must be said aloud
        assert!(warnings.iter().any(|w| w.contains("'all'")));
    }

    #[test]
    fn missing_meta_warns_instead_of_failing() {
        let raw = serde_json::json!({
            "web": {"hosts": ["web01.example.com"]}
        });

        let (dataset, warnings) = Dataset::from_ansible_inventory(raw).unwrap();
        assert_eq!(dataset.groups.len(), 1);
        assert!(dataset.hostvars.is_empty());
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
    fn detects_ansible_looking_output() {
        assert!(Dataset::looks_like_ansible_inventory(&serde_json::json!({
            "_meta": {"hostvars": {}}
        })));
        assert!(!Dataset::looks_like_ansible_inventory(&serde_json::json!({
            "hostvars": {}, "groups": {}
        })));
    }
}
