// Builtin output transformers: render cached datasets into a consumer format
// in-process, with no script and no spawned interpreter. `script_path` stays
// for bespoke formats; the builtins cover the common, hot path.
//
// Ansible dynamic inventory is the inverse of Dataset::from_ansible_inventory:
// merge every source's hostvars and groups, apply the endpoint's filters, and
// emit `{ "_meta": { "hostvars": {...} }, "<group>": { hosts, children, vars } }`.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::domain::dataset::{Dataset, Group, HostVars};

// One setting: a dynamic request parameter overrides the endpoint's static
// config, and both are strings (a query string carries no types).
fn setting(params: &serde_json::Value, config: &HashMap<String, String>, key: &str) -> String {
    if let Some(value) = params.get(key).and_then(serde_json::Value::as_str) {
        return value.to_string();
    }
    config.get(key).cloned().unwrap_or_default()
}

fn comma_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

// Render the merged datasets as Ansible dynamic inventory JSON.
//
// Filters (from `config`, each overridable per request via `params`):
//   filter_datacenter — keep hosts whose `datacenter` hostvar equals this
//   filter_os         — keep hosts whose `os` hostvar equals this
//   filter_group      — keep hosts in any of these (comma-separated) groups
//   exclude_vars      — drop these (comma-separated) hostvars from every host
pub fn render_ansible(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> String {
    let filter_datacenter = setting(params, config, "filter_datacenter");
    let filter_os = setting(params, config, "filter_os");
    let filter_group = setting(params, config, "filter_group");
    let exclude_vars = comma_list(&setting(params, config, "exclude_vars"));

    // Sources are a HashMap, whose iteration order is random, so merge them in
    // a stable order (sorted by id): an overlapping host then resolves the same
    // way every render, and the later id wins ("later sources overwrite").
    let mut source_ids: Vec<&String> = datasets.keys().collect();
    source_ids.sort();

    let mut hostvars: HashMap<String, HostVars> = HashMap::new();
    let mut groups: HashMap<String, Group> = HashMap::new();

    for id in source_ids {
        let dataset = &datasets[id];
        for (host, vars) in &dataset.hostvars {
            hostvars
                .entry(host.clone())
                .or_default()
                .extend(vars.clone());
        }
        for (name, group) in &dataset.groups {
            let merged = groups.entry(name.clone()).or_default();
            for host in &group.hosts {
                if !merged.hosts.contains(host) {
                    merged.hosts.push(host.clone());
                }
            }
            for child in &group.children {
                if !merged.children.contains(child) {
                    merged.children.push(child.clone());
                }
            }
            if let Some(group_vars) = &group.vars {
                merged
                    .vars
                    .get_or_insert_with(HashMap::new)
                    .extend(group_vars.clone());
            }
        }
    }

    // Host filters.
    if !filter_datacenter.is_empty() {
        hostvars.retain(|_, vars| {
            vars.get("datacenter").and_then(serde_json::Value::as_str)
                == Some(filter_datacenter.as_str())
        });
    }
    if !filter_os.is_empty() {
        hostvars.retain(|_, vars| {
            vars.get("os").and_then(serde_json::Value::as_str) == Some(filter_os.as_str())
        });
    }
    if !filter_group.is_empty() {
        let mut allowed: HashSet<String> = HashSet::new();
        for name in comma_list(&filter_group) {
            if let Some(group) = groups.get(&name) {
                allowed.extend(group.hosts.iter().cloned());
            }
        }
        hostvars.retain(|host, _| allowed.contains(host));
    }

    // Strip unwanted vars from the survivors.
    if !exclude_vars.is_empty() {
        for vars in hostvars.values_mut() {
            for name in &exclude_vars {
                vars.remove(name);
            }
        }
    }

    // A group keeps only surviving hosts; one left with neither hosts nor
    // children carries nothing and is dropped.
    let survivors: HashSet<&String> = hostvars.keys().collect();
    groups.retain(|_, group| {
        group.hosts.retain(|host| survivors.contains(host));
        !(group.hosts.is_empty() && group.children.is_empty())
    });

    // Emit the Ansible dynamic-inventory shape deterministically (sorted keys
    // and lists) so identical inventory renders byte-for-byte identically.
    let mut inventory = serde_json::Map::new();
    inventory.insert(
        "_meta".to_string(),
        serde_json::json!({ "hostvars": hostvars }),
    );

    let mut group_names: Vec<&String> = groups.keys().collect();
    group_names.sort();
    for name in group_names {
        let group = &groups[name];
        let mut entry = serde_json::Map::new();
        if !group.hosts.is_empty() {
            let mut hosts = group.hosts.clone();
            hosts.sort();
            entry.insert("hosts".to_string(), serde_json::json!(hosts));
        }
        if !group.children.is_empty() {
            let mut children = group.children.clone();
            children.sort();
            entry.insert("children".to_string(), serde_json::json!(children));
        }
        if let Some(vars) = &group.vars
            && !vars.is_empty()
        {
            entry.insert("vars".to_string(), serde_json::json!(vars));
        }
        inventory.insert(name.clone(), serde_json::Value::Object(entry));
    }

    // Pretty, like the script it replaces; the handler sniffs `{`/`[` for JSON.
    serde_json::to_string_pretty(&serde_json::Value::Object(inventory))
        .expect("inventory is plain JSON values and cannot fail to serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dataset(hostvars: serde_json::Value, groups: serde_json::Value) -> Arc<Dataset> {
        Arc::new(Dataset {
            hostvars: serde_json::from_value(hostvars).unwrap(),
            groups: serde_json::from_value(groups).unwrap(),
            remove_hosts: Vec::new(),
        })
    }

    fn render(
        datasets: &HashMap<String, Arc<Dataset>>,
        config: serde_json::Value,
    ) -> serde_json::Value {
        let config: HashMap<String, String> = serde_json::from_value(config).unwrap();
        serde_json::from_str(&render_ansible(datasets, &config, &serde_json::json!({}))).unwrap()
    }

    #[test]
    fn merges_sources_with_later_id_winning() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src-a".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux", "role": "a"}}),
                serde_json::json!({"web": {"hosts": ["h1"]}}),
            ),
        );
        datasets.insert(
            "src-b".to_string(),
            dataset(
                serde_json::json!({"h1": {"role": "b"}, "h2": {"os": "linux"}}),
                serde_json::json!({"web": {"hosts": ["h1", "h2"]}}),
            ),
        );

        let inv = render(&datasets, serde_json::json!({}));

        // src-b sorts after src-a, so its `role` wins; `os` from src-a remains.
        assert_eq!(inv["_meta"]["hostvars"]["h1"]["role"], "b");
        assert_eq!(inv["_meta"]["hostvars"]["h1"]["os"], "linux");
        // group hosts are merged and de-duplicated, sorted.
        assert_eq!(inv["web"]["hosts"], serde_json::json!(["h1", "h2"]));
    }

    #[test]
    fn filters_by_os_and_prunes_emptied_groups() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({
                    "linux1": {"os": "linux"},
                    "win1": {"os": "windows"}
                }),
                serde_json::json!({
                    "linux": {"hosts": ["linux1"]},
                    "windows": {"hosts": ["win1"]}
                }),
            ),
        );

        let inv = render(&datasets, serde_json::json!({"filter_os": "linux"}));

        assert!(inv["_meta"]["hostvars"].get("win1").is_none());
        assert!(inv["_meta"]["hostvars"].get("linux1").is_some());
        // the windows group lost its only host and is dropped
        assert!(inv.get("windows").is_none());
        assert_eq!(inv["linux"]["hosts"], serde_json::json!(["linux1"]));
    }

    #[test]
    fn exclude_vars_strips_named_hostvars() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux", "secret": "x", "serial": "y"}}),
                serde_json::json!({}),
            ),
        );

        let inv = render(
            &datasets,
            serde_json::json!({"exclude_vars": "secret, serial"}),
        );

        let vars = &inv["_meta"]["hostvars"]["h1"];
        assert!(vars.get("secret").is_none());
        assert!(vars.get("serial").is_none());
        assert_eq!(vars["os"], "linux");
    }

    #[test]
    fn params_override_config() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"a": {"os": "linux"}, "b": {"os": "windows"}}),
                serde_json::json!({}),
            ),
        );
        let config: HashMap<String, String> =
            serde_json::from_value(serde_json::json!({"filter_os": "linux"})).unwrap();
        // request asks for windows, overriding the static linux filter
        let out = render_ansible(
            &datasets,
            &config,
            &serde_json::json!({"filter_os": "windows"}),
        );
        let inv: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(inv["_meta"]["hostvars"].get("b").is_some());
        assert!(inv["_meta"]["hostvars"].get("a").is_none());
    }

    #[test]
    fn is_deterministic_across_renders() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src-z".to_string(),
            dataset(
                serde_json::json!({"h2": {}}),
                serde_json::json!({"g": {"hosts": ["h2"]}}),
            ),
        );
        datasets.insert(
            "src-a".to_string(),
            dataset(
                serde_json::json!({"h1": {}}),
                serde_json::json!({"g": {"hosts": ["h1"]}}),
            ),
        );
        let cfg = HashMap::new();
        let first = render_ansible(&datasets, &cfg, &serde_json::json!({}));
        let second = render_ansible(&datasets, &cfg, &serde_json::json!({}));
        assert_eq!(first, second);
    }
}
