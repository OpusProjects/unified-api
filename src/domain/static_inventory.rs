use std::collections::HashMap;

use crate::domain::dataset::{Dataset, Group, HostVars};

// Native parser for Ansible STATIC YAML inventories — the classic layout:
//
//   inventory.yaml        all: { hosts: {...}, children: { web: {...} } }
//   group_vars/all.yaml
//   group_vars/web.yaml
//   host_vars/web01.example.com.yaml
//
// This is pure domain logic: it receives FILE CONTENTS (the adapter does the
// disk IO) and returns a Dataset plus human-readable warnings.
//
// Variable precedence, lowest to highest (a simplified version of Ansible's
// own ordering — documented in docs/connectors.md):
//   1. `all` inline vars, then group_vars/all.yaml
//   2. each group containing the host, parents before children, alphabetical
//      among groups of the same depth — inline vars then group_vars file
//   3. host inline vars (in the inventory file)
//   4. host_vars/<host>.yaml
//
// Deliberately unsupported, and loud about it:
//   - INI inventories (YAML only)
//   - host ranges like web[01:20].example.com → error
//   - ansible-vault encrypted content → error naming the file
//   - Jinja templating: {{ ... }} values pass through as literal strings
pub struct StaticInventoryInput {
    // Contents of the inventory YAML file
    pub inventory: String,
    // group name (from the filename, extension stripped) → file contents
    pub group_vars: HashMap<String, String>,
    // hostname (from the filename) → file contents
    pub host_vars: HashMap<String, String>,
}

pub fn parse(input: &StaticInventoryInput) -> Result<(Dataset, Vec<String>), String> {
    let mut warnings: Vec<String> = Vec::new();

    check_not_vaulted("inventory file", &input.inventory)?;

    let root: serde_yaml_ng::Value = serde_yaml_ng::from_str(&input.inventory)
        .map_err(|e| format!("inventory file is not valid YAML: {}", e))?;
    let root = root
        .as_mapping()
        .ok_or("inventory file must be a YAML mapping of groups")?;

    // Parse the group_vars / host_vars files up front, so a broken file is
    // reported by name even if its group/host never matches anything.
    let group_file_vars = parse_vars_files(&input.group_vars, "group_vars")?;
    let host_file_vars = parse_vars_files(&input.host_vars, "host_vars")?;

    // First pass: walk the group tree collecting structure.
    let mut walk = Walk::default();
    for (name, node) in root {
        let name = key_as_string(name)?;
        walk.group(&name, node, 0)?;
    }

    // Effective vars per group = inline vars, overridden by its group_vars file
    let mut group_effective_vars: HashMap<String, HostVars> = HashMap::new();
    for (name, info) in &walk.groups {
        let mut vars = info.inline_vars.clone();
        if let Some(file_vars) = group_file_vars.get(name) {
            vars.extend(file_vars.clone());
        }
        group_effective_vars.insert(name.clone(), vars);
    }
    // group_vars/ files for groups that don't exist deserve a mention
    for name in group_file_vars.keys() {
        if !walk.groups.contains_key(name) && name != "all" {
            warnings.push(format!(
                "group_vars/{} has no matching group in the inventory",
                name
            ));
        }
    }

    // Second pass: flatten precedence into per-host effective vars.
    let mut hostvars: HashMap<String, HostVars> = HashMap::new();
    for (host, direct) in &walk.host_memberships {
        let mut vars: HostVars = HashMap::new();

        // 1. `all` (inline + file), which contains every host by definition
        if let Some(all_vars) = group_effective_vars.get("all") {
            vars.extend(all_vars.clone());
        }

        // 2. every group containing the host (directly or via ancestors),
        //    parents before children, alphabetical within the same depth
        let mut chain: Vec<(usize, String)> = Vec::new();
        for group in direct {
            let mut current = Some(group.clone());
            while let Some(name) = current {
                if name != "all" {
                    let depth = walk.groups.get(&name).map(|g| g.depth).unwrap_or(0);
                    chain.push((depth, name.clone()));
                }
                current = walk.parents.get(&name).cloned();
            }
        }
        chain.sort();
        chain.dedup();
        for (_, group) in &chain {
            if let Some(group_vars) = group_effective_vars.get(group) {
                vars.extend(group_vars.clone());
            }
        }

        // 3. inline host vars, 4. host_vars/<host>.yaml
        if let Some(inline) = walk.host_inline_vars.get(host) {
            vars.extend(inline.clone());
        }
        if let Some(file_vars) = host_file_vars.get(host) {
            vars.extend(file_vars.clone());
        }

        hostvars.insert(host.clone(), vars);
    }
    for name in host_file_vars.keys() {
        if !walk.host_memberships.contains_key(name) {
            warnings.push(format!(
                "host_vars/{} has no matching host in the inventory",
                name
            ));
        }
    }

    // Dataset groups: everything except the implicit all/ungrouped, with
    // direct hosts, children, and the group's own (unflattened) vars.
    let mut groups: HashMap<String, Group> = HashMap::new();
    for (name, info) in &walk.groups {
        if name == "all" || name == "ungrouped" {
            continue;
        }
        let vars = group_effective_vars
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned();
        groups.insert(
            name.clone(),
            Group {
                hosts: {
                    let mut hosts = info.hosts.clone();
                    hosts.sort();
                    hosts
                },
                children: {
                    let mut children = info.children.clone();
                    children.sort();
                    children
                },
                vars,
            },
        );
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

struct GroupInfo {
    depth: usize,
    hosts: Vec<String>,
    children: Vec<String>,
    inline_vars: HostVars,
}

#[derive(Default)]
struct Walk {
    groups: HashMap<String, GroupInfo>,
    // child group -> parent group
    parents: HashMap<String, String>,
    // host -> groups it appears under directly
    host_memberships: HashMap<String, Vec<String>>,
    host_inline_vars: HashMap<String, HostVars>,
}

impl Walk {
    fn group(
        &mut self,
        name: &str,
        node: &serde_yaml_ng::Value,
        depth: usize,
    ) -> Result<(), String> {
        let mut info = GroupInfo {
            depth,
            hosts: Vec::new(),
            children: Vec::new(),
            inline_vars: HashMap::new(),
        };

        // A group may be null (empty) or a mapping with hosts/children/vars
        if let Some(mapping) = node.as_mapping() {
            for (key, value) in mapping {
                match key_as_string(key)?.as_str() {
                    "hosts" => {
                        let hosts = value.as_mapping().ok_or_else(|| {
                            format!("group '{}': hosts must be a mapping of hostnames", name)
                        })?;
                        for (host, host_vars) in hosts {
                            let host = key_as_string(host)?;
                            if host.contains('[') {
                                return Err(format!(
                                    "host '{}' looks like an Ansible range pattern, which is not supported",
                                    host
                                ));
                            }
                            info.hosts.push(host.clone());
                            self.host_memberships
                                .entry(host.clone())
                                .or_default()
                                .push(name.to_string());
                            let vars = yaml_vars(host_vars)
                                .map_err(|e| format!("host '{}': {}", host, e))?;
                            self.host_inline_vars.entry(host).or_default().extend(vars);
                        }
                    }
                    "children" => {
                        let children = value.as_mapping().ok_or_else(|| {
                            format!("group '{}': children must be a mapping of groups", name)
                        })?;
                        for (child, child_node) in children {
                            let child = key_as_string(child)?;
                            info.children.push(child.clone());
                            self.parents.insert(child.clone(), name.to_string());
                            self.group(&child, child_node, depth + 1)?;
                        }
                    }
                    "vars" => {
                        info.inline_vars =
                            yaml_vars(value).map_err(|e| format!("group '{}': {}", name, e))?;
                    }
                    other => {
                        return Err(format!(
                            "group '{}': unknown key '{}' (expected hosts/children/vars)",
                            name, other
                        ));
                    }
                }
            }
        } else if !node.is_null() {
            return Err(format!(
                "group '{}' must be a mapping (hosts/children/vars) or empty",
                name
            ));
        }

        self.groups.insert(name.to_string(), info);
        Ok(())
    }
}

// Convert a YAML vars mapping into HostVars (JSON values). Null = no vars.
fn yaml_vars(value: &serde_yaml_ng::Value) -> Result<HostVars, String> {
    if value.is_null() {
        return Ok(HashMap::new());
    }
    let mapping = value.as_mapping().ok_or("vars must be a mapping")?;
    let mut vars = HostVars::new();
    for (key, val) in mapping {
        let key = key_as_string(key)?;
        let json = serde_json::to_value(val).map_err(|e| format!("var '{}': {}", key, e))?;
        vars.insert(key, json);
    }
    Ok(vars)
}

fn parse_vars_files(
    files: &HashMap<String, String>,
    kind: &str,
) -> Result<HashMap<String, HostVars>, String> {
    let mut parsed = HashMap::new();
    for (name, contents) in files {
        check_not_vaulted(&format!("{}/{}", kind, name), contents)?;
        let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(contents)
            .map_err(|e| format!("{}/{} is not valid YAML: {}", kind, name, e))?;
        let vars = yaml_vars(&value).map_err(|e| format!("{}/{}: {}", kind, name, e))?;
        parsed.insert(name.clone(), vars);
    }
    Ok(parsed)
}

// Encrypted content must never leak into hostvars looking like data, and we
// cannot decrypt it — fail loudly naming the file.
fn check_not_vaulted(what: &str, contents: &str) -> Result<(), String> {
    if contents.trim_start().starts_with("$ANSIBLE_VAULT") {
        return Err(format!(
            "{} is ansible-vault encrypted — unified-api cannot decrypt it",
            what
        ));
    }
    Ok(())
}

fn key_as_string(key: &serde_yaml_ng::Value) -> Result<String, String> {
    key.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("expected a string key, got: {:?}", key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(inventory: &str) -> StaticInventoryInput {
        StaticInventoryInput {
            inventory: inventory.to_string(),
            group_vars: HashMap::new(),
            host_vars: HashMap::new(),
        }
    }

    const BASIC: &str = r#"
all:
  hosts:
    standalone.example.com:
      ansible_connection: local
  children:
    web:
      hosts:
        web01.example.com: {}
        web02.example.com:
          http_port: 8080
      vars:
        ntp_server: ntp.example.com
    db:
      hosts:
        db01.example.com: {}
"#;

    #[test]
    fn parses_hosts_groups_and_inline_vars() {
        let (dataset, warnings) = parse(&input(BASIC)).unwrap();

        assert!(warnings.is_empty());
        assert_eq!(dataset.hostvars.len(), 4);
        // inline host var
        assert_eq!(dataset.hostvars["web02.example.com"]["http_port"], 8080);
        // inline group var flattened into member hosts
        assert_eq!(
            dataset.hostvars["web01.example.com"]["ntp_server"],
            "ntp.example.com"
        );
        // `all` is implicit, not a Dataset group
        assert_eq!(dataset.groups.len(), 2);
        assert_eq!(
            dataset.groups["web"].hosts,
            vec!["web01.example.com", "web02.example.com"]
        );
        // group vars also kept on the group itself
        assert_eq!(
            dataset.groups["web"].vars.as_ref().unwrap()["ntp_server"],
            "ntp.example.com"
        );
    }

    #[test]
    fn group_vars_files_and_precedence() {
        let mut inv = input(BASIC);
        inv.group_vars.insert(
            "all".to_string(),
            "timezone: Europe/Madrid\nntp_server: global.ntp\n".to_string(),
        );
        // group file overrides the group's inline var
        inv.group_vars
            .insert("web".to_string(), "ntp_server: web.ntp\n".to_string());
        inv.host_vars.insert(
            "web02.example.com".to_string(),
            "http_port: 9090\n".to_string(),
        );

        let (dataset, _) = parse(&inv).unwrap();

        // all < group: web hosts get the web override
        assert_eq!(
            dataset.hostvars["web01.example.com"]["ntp_server"],
            "web.ntp"
        );
        // hosts outside web keep the all-level value
        assert_eq!(
            dataset.hostvars["db01.example.com"]["ntp_server"],
            "global.ntp"
        );
        // group_vars/all reaches every host, including ungrouped ones
        assert_eq!(
            dataset.hostvars["standalone.example.com"]["timezone"],
            "Europe/Madrid"
        );
        // host_vars file beats the inline host var
        assert_eq!(dataset.hostvars["web02.example.com"]["http_port"], 9090);
    }

    #[test]
    fn child_group_vars_override_parent() {
        let inv = input(
            r#"
all:
  children:
    europe:
      vars:
        dns: eu.dns
      children:
        spain:
          hosts:
            madrid.example.com: {}
          vars:
            dns: es.dns
"#,
        );

        let (dataset, _) = parse(&inv).unwrap();
        assert_eq!(dataset.hostvars["madrid.example.com"]["dns"], "es.dns");
        // structure preserved: europe has spain as child
        assert_eq!(dataset.groups["europe"].children, vec!["spain"]);
    }

    #[test]
    fn vaulted_file_is_an_error() {
        let mut inv = input(BASIC);
        inv.host_vars.insert(
            "web01.example.com".to_string(),
            "$ANSIBLE_VAULT;1.1;AES256\n6338386437...".to_string(),
        );

        let err = parse(&inv).unwrap_err();
        assert!(err.contains("ansible-vault"), "error was: {}", err);
        assert!(err.contains("web01.example.com"));
    }

    #[test]
    fn host_range_pattern_is_an_error() {
        let inv = input(
            r#"
all:
  children:
    web:
      hosts:
        "web[01:20].example.com": {}
"#,
        );
        let err = parse(&inv).unwrap_err();
        assert!(err.contains("range"), "error was: {}", err);
    }

    #[test]
    fn unknown_group_key_is_an_error() {
        let inv = input(
            r#"
all:
  children:
    web:
      host:
        web01.example.com: {}
"#,
        );
        let err = parse(&inv).unwrap_err();
        assert!(err.contains("unknown key 'host'"), "error was: {}", err);
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        let err = parse(&input("all: [unclosed")).unwrap_err();
        assert!(err.contains("not valid YAML"));
    }

    #[test]
    fn orphan_vars_files_warn() {
        let mut inv = input(BASIC);
        inv.group_vars
            .insert("ghosts".to_string(), "x: 1\n".to_string());
        inv.host_vars
            .insert("nope.example.com".to_string(), "y: 2\n".to_string());

        let (_, warnings) = parse(&inv).unwrap();
        assert!(warnings.iter().any(|w| w.contains("group_vars/ghosts")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("host_vars/nope.example.com"))
        );
    }

    #[test]
    fn jinja_templates_pass_through_as_strings() {
        let inv = input(
            r#"
all:
  hosts:
    localhost:
      ansible_python_interpreter: "{{ ansible_playbook_python }}"
"#,
        );
        let (dataset, _) = parse(&inv).unwrap();
        assert_eq!(
            dataset.hostvars["localhost"]["ansible_python_interpreter"],
            "{{ ansible_playbook_python }}"
        );
    }
}
