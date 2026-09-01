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

// Merge every source's dataset and apply the endpoint's filters. Shared by
// every builtin: the formats differ only in how the surviving hosts and
// groups are written out, so the merge-and-filter half lives once.
//
// Filters (from `config`, each overridable per request via `params`):
//   filter_datacenter — keep hosts whose `datacenter` hostvar equals this
//   filter_os         — keep hosts whose `os` hostvar equals this
//   filter_group      — keep hosts in any of these (comma-separated) groups
//   exclude_vars      — drop these (comma-separated) hostvars from every host
fn merge_and_filter(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> (HashMap<String, HostVars>, HashMap<String, Group>) {
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

    // Strip unwanted vars from the survivors -- from groups as well as hosts.
    //
    // A var declared on a group reaches every member of it once the consumer
    // resolves the inventory, so stripping only hostvars would let an excluded
    // name through the group door. exclude_vars is how something is kept out of
    // an endpoint, and a filter with a way around it is not one.
    if !exclude_vars.is_empty() {
        for vars in hostvars.values_mut() {
            for name in &exclude_vars {
                vars.remove(name);
            }
        }
        for group in groups.values_mut() {
            if let Some(vars) = group.vars.as_mut() {
                for name in &exclude_vars {
                    vars.remove(name);
                }
            }
        }
    }

    // A group keeps only surviving hosts. What happens to one left empty depends
    // on whether it ever named any, and the two cases are opposites:
    //
    // - it listed members and a filter took them all: the filter's answer for
    //   this group is "nothing", so it goes, along with the hosts it named.
    // - it never listed any: it is a declaration of what the group MEANS, for
    //   members settled somewhere else -- Ansible's `group_by` puts a host into
    //   an existing group of the same name at play time and picks up the vars
    //   it finds there. Dropping it would throw those away between the enricher
    //   writing them and the endpoint answering, and say nothing.
    let survivors: HashSet<&String> = hostvars.keys().collect();
    groups.retain(|_, group| {
        let named_hosts = !group.hosts.is_empty();
        group.hosts.retain(|host| survivors.contains(host));
        if named_hosts {
            !(group.hosts.is_empty() && group.children.is_empty())
        } else {
            !(group.children.is_empty() && group.vars.as_ref().is_none_or(HashMap::is_empty))
        }
    });

    (hostvars, groups)
}

// Render the merged datasets as Ansible dynamic inventory JSON
// (`_meta.hostvars` plus one key per group). Filters: see merge_and_filter.
pub fn render_ansible(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> String {
    let (hostvars, groups) = merge_and_filter(datasets, config, params);

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

// Render the merged datasets in the raw source shape (`hostvars` + `groups`),
// like `GET /sources/{id}/dataset` but merged across sources and filtered —
// for consumers that want the inventory as data rather than in a tool's
// format. Filters: see merge_and_filter.
pub fn render_json(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> String {
    let (hostvars, groups) = merge_and_filter(datasets, config, params);

    // `json!` moves the maps into serde_json::Value, whose object type keeps
    // its keys sorted (a BTreeMap underneath), so identical inventory renders
    // byte-for-byte identically — same guarantee as the other builtins.
    serde_json::to_string_pretty(&serde_json::json!({
        "hostvars": hostvars,
        "groups": groups,
    }))
    .expect("the dataset is plain JSON values and cannot fail to serialize")
}

// Quote a CSV field per RFC 4180: only when it contains a comma, a quote or a
// line break, doubling any embedded quotes.
fn csv_field(raw: &str) -> String {
    if raw.contains(',') || raw.contains('"') || raw.contains('\n') || raw.contains('\r') {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_string()
    }
}

// A hostvar as one CSV cell: strings verbatim (no JSON quotes), a missing or
// null var as an empty cell, and anything structured as compact JSON so no
// information is silently dropped.
fn csv_value(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other)
            .expect("a hostvar is a plain JSON value and cannot fail to serialize"),
    }
}

// Render the surviving hosts as CSV: a header row, then one row per host,
// sorted by hostname. The first column is `host`; the rest default to every
// hostvar name seen (sorted), and `columns` (comma-separated, in `config` or
// per request via `params`) picks and orders them instead. Groups do not
// appear — a row per host is the point; use `filter_group` to select by one.
// Filters: see merge_and_filter.
pub fn render_csv(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> String {
    let columns = comma_list(&setting(params, config, "columns"));
    let (hostvars, _groups) = merge_and_filter(datasets, config, params);

    let columns = if columns.is_empty() {
        let mut seen: Vec<String> = hostvars
            .values()
            .flat_map(|vars| vars.keys().cloned())
            .collect::<HashSet<String>>()
            .into_iter()
            .collect();
        seen.sort();
        seen
    } else {
        columns
    };

    let mut hosts: Vec<&String> = hostvars.keys().collect();
    hosts.sort();

    let mut out = String::new();
    out.push_str("host");
    for column in &columns {
        out.push(',');
        out.push_str(&csv_field(column));
    }
    out.push('\n');
    for host in hosts {
        let vars = &hostvars[host];
        out.push_str(&csv_field(host));
        for column in &columns {
            out.push(',');
            out.push_str(&csv_field(&csv_value(vars.get(column))));
        }
        out.push('\n');
    }
    out
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

    // Two empty groups, opposite answers. One named a host and a filter took it:
    // the filter's answer for that group is "nothing", vars or no vars. The
    // other never named one, so it is a declaration for members settled
    // elsewhere and has to survive to be of any use.
    #[test]
    fn an_emptied_group_is_pruned_but_one_that_never_had_hosts_is_kept() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({
                    "linux1": {"os": "linux"},
                    "win1": {"os": "windows"}
                }),
                serde_json::json!({
                    "windows": {"hosts": ["win1"], "vars": {"ntp": "ntp.win"}},
                    "oraclelinux_9": {"vars": {"repositories_repos": ["ol9-baseos"]}}
                }),
            ),
        );

        let inv = render(&datasets, serde_json::json!({"filter_os": "linux"}));

        assert!(
            inv.get("windows").is_none(),
            "it named a host, the filter removed it, so the group carries nothing here"
        );
        assert_eq!(
            inv["oraclelinux_9"]["vars"]["repositories_repos"],
            serde_json::json!(["ol9-baseos"]),
            "it never named a host: a declaration for members decided elsewhere"
        );
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

    // A var on a group reaches every member once the consumer resolves the
    // inventory, so sweeping only hostvars left a way round the exclusion —
    // and the group is now the ordinary place for a group's vars to live.
    #[test]
    fn exclude_vars_strips_the_name_from_groups_too() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux"}}),
                serde_json::json!({
                    "web": {"hosts": ["h1"], "vars": {"secret": "x", "ntp": "pool"}}
                }),
            ),
        );

        let inv = render(&datasets, serde_json::json!({"exclude_vars": "secret"}));

        assert!(inv["web"]["vars"].get("secret").is_none());
        assert_eq!(inv["web"]["vars"]["ntp"], "pool");
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
    fn json_renders_the_merged_dataset_in_the_raw_shape() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src-a".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux"}}),
                serde_json::json!({"web": {"hosts": ["h1"]}}),
            ),
        );
        datasets.insert(
            "src-b".to_string(),
            dataset(
                serde_json::json!({"h2": {"os": "windows"}}),
                serde_json::json!({"web": {"hosts": ["h2"]}}),
            ),
        );
        let config: HashMap<String, String> = HashMap::new();

        let out = render_json(&datasets, &config, &serde_json::json!({}));
        let merged: serde_json::Value = serde_json::from_str(&out).unwrap();

        // The raw source shape — hostvars + groups, no _meta wrapper.
        assert!(merged.get("_meta").is_none());
        assert_eq!(merged["hostvars"]["h1"]["os"], "linux");
        assert_eq!(merged["hostvars"]["h2"]["os"], "windows");
        assert_eq!(
            merged["groups"]["web"]["hosts"],
            serde_json::json!(["h1", "h2"])
        );
    }

    #[test]
    fn json_applies_the_shared_filters() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({
                    "linux1": {"os": "linux", "secret": "x"},
                    "win1": {"os": "windows"}
                }),
                serde_json::json!({"all-hosts": {"hosts": ["linux1", "win1"]}}),
            ),
        );
        let config: HashMap<String, String> = serde_json::from_value(
            serde_json::json!({"filter_os": "linux", "exclude_vars": "secret"}),
        )
        .unwrap();

        let out = render_json(&datasets, &config, &serde_json::json!({}));
        let merged: serde_json::Value = serde_json::from_str(&out).unwrap();

        assert!(merged["hostvars"].get("win1").is_none());
        assert!(merged["hostvars"]["linux1"].get("secret").is_none());
        // The filtered host is gone from the group too.
        assert_eq!(
            merged["groups"]["all-hosts"]["hosts"],
            serde_json::json!(["linux1"])
        );
    }

    #[test]
    fn csv_defaults_to_every_hostvar_seen_sorted() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({
                    "b-host": {"os": "linux", "ram_gb": 64},
                    "a-host": {"os": "windows", "datacenter": "dc1"}
                }),
                serde_json::json!({}),
            ),
        );
        let config: HashMap<String, String> = HashMap::new();

        let out = render_csv(&datasets, &config, &serde_json::json!({}));
        let lines: Vec<&str> = out.lines().collect();

        // Columns are the union of names, sorted; rows sorted by hostname.
        // A missing var is an empty cell, a number is rendered bare.
        assert_eq!(lines[0], "host,datacenter,os,ram_gb");
        assert_eq!(lines[1], "a-host,dc1,windows,");
        assert_eq!(lines[2], "b-host,,linux,64");
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn csv_columns_setting_picks_and_orders_and_params_override() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux", "ram_gb": 64, "role": "web"}}),
                serde_json::json!({}),
            ),
        );
        let config: HashMap<String, String> =
            serde_json::from_value(serde_json::json!({"columns": "role, os"})).unwrap();

        let out = render_csv(&datasets, &config, &serde_json::json!({}));
        assert_eq!(out, "host,role,os\nh1,web,linux\n");

        // A request parameter replaces the configured column list entirely.
        let out = render_csv(
            &datasets,
            &config,
            &serde_json::json!({"columns": "ram_gb"}),
        );
        assert_eq!(out, "host,ram_gb\nh1,64\n");
    }

    #[test]
    fn csv_quotes_and_serializes_structured_values() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {
                    "comment": "a, \"quoted\" note",
                    "tags": ["x", "y"],
                    "gone": null
                }}),
                serde_json::json!({}),
            ),
        );
        let config: HashMap<String, String> = HashMap::new();

        let out = render_csv(&datasets, &config, &serde_json::json!({}));
        let lines: Vec<&str> = out.lines().collect();

        assert_eq!(lines[0], "host,comment,gone,tags");
        // Embedded commas and quotes get RFC 4180 quoting; null is an empty
        // cell; a list survives as compact JSON (itself quoted for its comma).
        assert_eq!(
            lines[1],
            "h1,\"a, \"\"quoted\"\" note\",,\"[\"\"x\"\",\"\"y\"\"]\""
        );
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
