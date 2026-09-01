use std::collections::HashMap;

use crate::domain::dataset::{Dataset, Group, HostVars};

// Native parser for Ansible STATIC YAML inventories — the classic layout:
//
//   inventory.yaml        all: { hosts: {...}, children: { web: {...} } }
//   group_vars/all.yaml            (or group_vars/all/ holding several files)
//   group_vars/web.yaml
//   host_vars/web01.example.com.yaml
//
// This is pure domain logic: it receives FILE CONTENTS (the adapter does the
// disk IO) and returns a Dataset plus human-readable warnings.
//
// Vars are emitted WHERE THEY ARE DECLARED, not resolved onto every host:
// a group's vars stay on the group, a host's stay on the host, and whoever
// reads the inventory applies precedence. Ansible is the authority on its own
// ordering, so deferring to it is both cheaper and more faithful than
// reimplementing it here.
//
// It was resolved here once, and the cost is why it is not any more: copying
// each group's vars onto every member meant a 1097-host inventory whose
// group_vars/all is 55 KB carried that 55 KB a thousand times over — a 53 MB
// dataset, ~94% of it the same bytes, which OOMKilled the server on sync.
//
// So per host this emits only:
//   1. host inline vars (in the inventory file)
//   2. host_vars/<host>.yaml
// and per group its inline vars merged with its group_vars file. `all` is
// emitted as a group like any other — that is where group_vars/all lands, and
// Ansible applies it to every host because every host is in `all`.
//
// Deliberately unsupported, and loud about it:
//   - INI inventories (YAML only)
//   - host ranges like web[01:20].example.com → error
//   - ansible-vault encrypted content → error naming the file
//   - Jinja templating: {{ ... }} values pass through as literal strings
pub struct StaticInventoryInput {
    // Contents of the inventory YAML file
    pub inventory: String,
    // group name → the files that define it, in the order they are merged.
    // A list because Ansible accepts group_vars/web.yaml *or* group_vars/web/
    // holding several files; each entry is (label for errors, contents).
    pub group_vars: HashMap<String, Vec<(String, String)>>,
    // hostname → its files, same shape and for the same reason
    pub host_vars: HashMap<String, Vec<(String, String)>>,
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
        walk.group(&name, node)?;
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

    // Second pass: the host's OWN vars. Group vars are not folded in here —
    // they are emitted on the group, and the consumer resolves them.
    let mut hostvars: HashMap<String, HostVars> = HashMap::new();
    for host in walk.host_memberships.keys() {
        let mut vars: HostVars = HashMap::new();

        // 1. inline host vars, 2. host_vars/<host>.yaml
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

    // Dataset groups: every declared group with its direct hosts, children and
    // its own vars.
    //
    // `all` is emitted like any other group, because it is where group_vars/all
    // lands and nothing else carries those vars now that they are not copied
    // onto each host. `ungrouped` stays out: Ansible synthesises it for hosts
    // that are in no group, so emitting it would be inventing membership.
    let mut groups: HashMap<String, Group> = HashMap::new();
    for (name, info) in &walk.groups {
        if name == "ungrouped" {
            continue;
        }
        let vars = group_effective_vars
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned();
        groups.insert(
            name.clone(),
            Group {
                // Deduplicated as well as sorted: merging several declarations
                // of one group can name the same host or child twice.
                hosts: {
                    let mut hosts = info.hosts.clone();
                    hosts.sort();
                    hosts.dedup();
                    hosts
                },
                children: {
                    let mut children = info.children.clone();
                    children.sort();
                    children.dedup();
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
    hosts: Vec<String>,
    children: Vec<String>,
    inline_vars: HostVars,
}

#[derive(Default)]
struct Walk {
    groups: HashMap<String, GroupInfo>,
    // host -> groups it appears under directly
    host_memberships: HashMap<String, Vec<String>>,
    host_inline_vars: HashMap<String, HostVars>,
}

impl Walk {
    fn group(&mut self, name: &str, node: &serde_yaml_ng::Value) -> Result<(), String> {
        // Taken out of the map rather than built fresh: a group may be declared
        // more than once (under two parents, or twice under the same one), and
        // replacing what was there dropped every host the earlier declaration
        // carried. Silently — the host stayed in `hostvars`, so nothing looked
        // wrong until an inventory rendered from `groups` failed to target it.
        let mut info = self.groups.remove(name).unwrap_or(GroupInfo {
            hosts: Vec::new(),
            children: Vec::new(),
            inline_vars: HashMap::new(),
        });

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
                            // Recursing re-enters `group` for the child, which
                            // merges into whatever that child already had — so
                            // the second parent adds to the first rather than
                            // replacing it. This is why `info` is held out of
                            // the map across the recursion.
                            self.group(&child, child_node)?;
                        }
                    }
                    "vars" => {
                        // Extended, not replaced: a group declared twice may
                        // carry vars in both places, and the later declaration
                        // should add to the earlier rather than erase it.
                        let vars =
                            yaml_vars(value).map_err(|e| format!("group '{}': {}", name, e))?;
                        info.inline_vars.extend(vars);
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
    files: &HashMap<String, Vec<(String, String)>>,
    kind: &str,
) -> Result<HashMap<String, HostVars>, String> {
    let mut parsed = HashMap::new();
    for (name, parts) in files {
        // Merged in the order the adapter listed them -- alphabetical for a
        // directory -- so a key set twice takes the later file's value, which
        // is what Ansible does.
        let mut vars = HostVars::new();
        for (label, contents) in parts {
            check_not_vaulted(&format!("{}/{}", kind, label), contents)?;
            let value: serde_yaml_ng::Value = serde_yaml_ng::from_str(contents)
                .map_err(|e| format!("{}/{} is not valid YAML: {}", kind, label, e))?;
            let file_vars = yaml_vars(&value).map_err(|e| format!("{}/{}: {}", kind, label, e))?;
            vars.extend(file_vars);
        }
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

    // The tests below each define a name from a single file; the adapter is
    // what turns a directory into several. Named for the file it stands in for.
    fn one(name: &str, contents: &str) -> Vec<(String, String)> {
        vec![(format!("{}.yaml", name), contents.to_string())]
    }

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
        // inline host var, which is the host's own
        assert_eq!(dataset.hostvars["web02.example.com"]["http_port"], 8080);
        // the group's var is NOT copied onto its members
        assert!(!dataset.hostvars["web01.example.com"].contains_key("ntp_server"));
        // it stays on the group, once
        assert_eq!(
            dataset.groups["web"].vars.as_ref().unwrap()["ntp_server"],
            "ntp.example.com"
        );
        // `all` is a group like any other: it holds group_vars/all
        assert_eq!(dataset.groups.len(), 3);
        assert_eq!(dataset.groups["all"].hosts, vec!["standalone.example.com"]);
        assert_eq!(dataset.groups["all"].children, vec!["db", "web"]);
        assert_eq!(
            dataset.groups["web"].hosts,
            vec!["web01.example.com", "web02.example.com"]
        );
    }

    #[test]
    fn group_vars_files_land_on_their_own_group() {
        let mut inv = input(BASIC);
        inv.group_vars.insert(
            "all".to_string(),
            one("all", "timezone: UTC\nntp_server: global.ntp\n"),
        );
        // group file overrides the group's inline var
        inv.group_vars
            .insert("web".to_string(), one("web", "ntp_server: web.ntp\n"));
        inv.host_vars.insert(
            "web02.example.com".to_string(),
            one("web02.example.com", "http_port: 9090\n"),
        );

        let (dataset, _) = parse(&inv).unwrap();

        // group_vars/all lands on `all`, once, rather than on every host
        let all = dataset.groups["all"]
            .vars
            .as_ref()
            .expect("all carries vars");
        assert_eq!(all["timezone"], "UTC");
        assert_eq!(all["ntp_server"], "global.ntp");
        assert!(!dataset.hostvars["standalone.example.com"].contains_key("timezone"));
        assert!(!dataset.hostvars["db01.example.com"].contains_key("ntp_server"));

        // within a group, its file still beats its inline var
        assert_eq!(
            dataset.groups["web"].vars.as_ref().unwrap()["ntp_server"],
            "web.ntp"
        );

        // host_vars file still beats the inline host var -- both are the
        // host's own, so this one is resolved here
        assert_eq!(dataset.hostvars["web02.example.com"]["http_port"], 9090);
    }

    #[test]
    fn a_parent_and_child_each_keep_their_own_vars() {
        let inv = input(
            r#"
all:
  children:
    region-a:
      vars:
        dns: region.dns
      children:
        zone-a:
          hosts:
            host01.example.com: {}
          vars:
            dns: zone.dns
"#,
        );

        let (dataset, _) = parse(&inv).unwrap();
        // Both values survive, on their own group. Which one wins for the host
        // is Ansible's call when it reads the inventory -- the child, by depth.
        assert_eq!(
            dataset.groups["region-a"].vars.as_ref().unwrap()["dns"],
            "region.dns"
        );
        assert_eq!(
            dataset.groups["zone-a"].vars.as_ref().unwrap()["dns"],
            "zone.dns"
        );
        assert!(!dataset.hostvars["host01.example.com"].contains_key("dns"));
        // structure preserved: region-a has zone-a as child
        assert_eq!(dataset.groups["region-a"].children, vec!["zone-a"]);
    }

    #[test]
    fn vaulted_file_is_an_error() {
        let mut inv = input(BASIC);
        inv.host_vars.insert(
            "web01.example.com".to_string(),
            one(
                "web01.example.com",
                "$ANSIBLE_VAULT;1.1;AES256\n6338386437...",
            ),
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
            .insert("ghosts".to_string(), one("ghosts", "x: 1\n"));
        inv.host_vars.insert(
            "nope.example.com".to_string(),
            one("nope.example.com", "y: 2\n"),
        );

        let (_, warnings) = parse(&inv).unwrap();
        assert!(warnings.iter().any(|w| w.contains("group_vars/ghosts")));
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("host_vars/nope.example.com"))
        );
    }

    // Declaring one group under two parents is ordinary Ansible. The second
    // declaration used to replace the first, so every host the first carried
    // vanished from the group — silently, since the host stayed in `hostvars`
    // and nothing looked wrong until an inventory rendered from `groups` failed
    // to target it.
    #[test]
    fn a_group_declared_under_two_parents_keeps_every_host() {
        let inv = input(
            r#"
all:
  children:
    dc1:
      children:
        web:
          hosts:
            web01.example.com:
    dc2:
      children:
        web:
          hosts:
            web02.example.com:
"#,
        );
        let (dataset, _) = parse(&inv).unwrap();

        assert_eq!(
            dataset.groups["web"].hosts,
            vec!["web01.example.com", "web02.example.com"]
        );
        assert_eq!(dataset.hostvars.len(), 2);
    }

    // Both ancestries must survive in the structure: a group declared under two
    // parents is reachable from each, which is what lets a consumer resolve the
    // vars of every one of them onto the host.
    #[test]
    fn a_group_declared_under_two_parents_is_a_child_of_each() {
        let inv = input(
            r#"
all:
  children:
    dc1:
      vars:
        site: site-a
      children:
        web:
          hosts:
            web01.example.com:
    dc2:
      vars:
        region: emea
      children:
        web: {}
"#,
        );
        let (dataset, _) = parse(&inv).unwrap();

        assert_eq!(dataset.groups["dc1"].children, vec!["web"]);
        assert_eq!(dataset.groups["dc2"].children, vec!["web"]);
        assert_eq!(dataset.groups["web"].hosts, vec!["web01.example.com"]);
        assert_eq!(
            dataset.groups["dc1"].vars.as_ref().unwrap()["site"],
            "site-a"
        );
        assert_eq!(
            dataset.groups["dc2"].vars.as_ref().unwrap()["region"],
            "emea"
        );
    }

    #[test]
    fn a_group_declared_twice_merges_its_vars_and_children() {
        let inv = input(
            r#"
all:
  children:
    dc1:
      children:
        web:
          vars:
            role: frontend
          children:
            web_cache:
              hosts:
                cache01.example.com:
    dc2:
      children:
        web:
          vars:
            tier: public
          hosts:
            web02.example.com:
"#,
        );
        let (dataset, _) = parse(&inv).unwrap();

        let web = &dataset.groups["web"];
        assert_eq!(web.children, vec!["web_cache"]);
        assert_eq!(web.hosts, vec!["web02.example.com"]);
        let group_vars = web.vars.as_ref().expect("web carries vars");
        assert_eq!(group_vars["role"], "frontend");
        assert_eq!(group_vars["tier"], "public");
    }

    // Merging must not let one declaration's host appear twice in the group.
    #[test]
    fn a_host_named_in_both_declarations_is_listed_once() {
        let inv = input(
            r#"
all:
  children:
    dc1:
      children:
        web:
          hosts:
            web01.example.com:
    dc2:
      children:
        web:
          hosts:
            web01.example.com:
"#,
        );
        let (dataset, _) = parse(&inv).unwrap();
        assert_eq!(dataset.groups["web"].hosts, vec!["web01.example.com"]);
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
