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
use crate::domain::endpoint::{BUILTIN_SETTINGS, EndpointLimit, renamed_setting};
use crate::domain::host_filter::HostFilter;
use crate::domain::var_path::{self, VarSelect};

// Apply an endpoint's limit to the datasets it is about to render: a
// constructed inventory. Everything the sources carry is still merged — the
// limit only decides which HOSTS come out the other side, so a host kept by it
// arrives with every variable, group and membership the other sources gave it.
//
// It runs here, on the datasets, rather than inside merge_and_filter, for one
// reason: a script transformer is handed the datasets whole and does its own
// merging, so a limit implemented in the builtin pipeline would silently not
// apply to scripts. An endpoint's scope must not depend on which transformer
// renders it.
//
// Note what a limited-but-emptied group does here: it keeps its vars. The
// group is left in place with a trimmed host list, so by the time
// merge_and_filter prunes, a group whose members were all outside the limit
// looks exactly like a group that never named hosts — a declaration of what
// the group MEANS, for members settled elsewhere (see the pruning comment
// there). That is the right answer for a limit: it says which hosts the
// inventory contains, not which groups stopped meaning anything.
pub fn apply_limit(
    datasets: &HashMap<String, Arc<Dataset>>,
    limit: &EndpointLimit,
) -> HashMap<String, Arc<Dataset>> {
    let Some(source_id) = limit.by_hosts_from_inventory.as_deref() else {
        // No rule set: nothing to restrict. Cloning the map clones Arcs, not
        // datasets. (Config validation refuses this case at load; handled
        // rather than assumed.)
        return datasets.clone();
    };

    // The hosts of a source are the ones its `hostvars` lists — the same set
    // `GET /sources/{id}/hosts` returns, and the same one a view's ownership
    // asks about.
    //
    // A source that is not here at all yields an EMPTY allowlist, so the
    // render comes out empty rather than unlimited. Both are wrong, but only
    // one is visible: an empty inventory is noticed immediately, while an
    // inventory that quietly grew back to every host looks like a working
    // endpoint. (The handler's 503 on a missing source means this should be
    // unreachable.)
    let allowed: HashSet<&String> = datasets
        .get(source_id)
        .map(|dataset| dataset.hostvars.keys().collect())
        .unwrap_or_default();

    datasets
        .iter()
        .map(|(id, dataset)| {
            // Copy-on-write, the same trick the cache read path uses: the Arc
            // is shared with the cache and every concurrent reader, so
            // Arc::make_mut clones the Dataset behind it exactly once, here,
            // and the cached one is never touched.
            let mut limited = Arc::clone(dataset);
            let owned = Arc::make_mut(&mut limited);
            owned.hostvars.retain(|host, _| allowed.contains(host));
            for group in owned.groups.values_mut() {
                group.hosts.retain(|host| allowed.contains(host));
            }
            (id.clone(), limited)
        })
        .collect()
}

// One setting: a dynamic request parameter overrides the endpoint's static
// config, and both are strings (a query string carries no types).
fn setting(params: &serde_json::Value, config: &HashMap<String, String>, key: &str) -> String {
    if let Some(value) = params.get(key).and_then(serde_json::Value::as_str) {
        return value.to_string();
    }
    config.get(key).cloned().unwrap_or_default()
}

// Check a request's parameters before a builtin renders anything, so a
// mistake is answered as one — a 400 naming it — rather than as an
// inventory that quietly ignored the parameter or matched nothing:
//   - a setting from before 0.32 (it would be ignored like any unknown name,
//     and the caller would get the whole inventory where it asked for a slice)
//   - a setting that is not a string (`filter` may also be a list of them)
//   - a malformed `filter` condition
pub fn check_params(
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> Result<(), String> {
    if let Some(object) = params.as_object() {
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort();
        for key in keys {
            if let Some(new) = renamed_setting(key) {
                return Err(format!(
                    "'{key}' was renamed in 0.32 — it is called {new} now"
                ));
            }
            let value = &object[key];
            let list = key == "filter" && value.is_array();
            if BUILTIN_SETTINGS.contains(&key.as_str()) && !value.is_string() && !list {
                return Err(format!("'{key}' must be a string, got {value}"));
            }
        }
    }
    host_filter(config, params).map(|_| ())
}

// The host filter of a request: `host` and `filter` from the request
// (overriding the endpoint's static config), parsed. On an endpoint every
// `host` entry is matched as a case-insensitive pattern, an exact name being
// a pattern without wildcards. A POST body may send `filter` as a JSON list
// instead of one `;`-joined string — a list is how a typed caller naturally
// builds it, and each entry is then one whole condition, `;` and all.
pub fn host_filter(
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> Result<HostFilter, String> {
    let host = setting(params, config, "host");
    match params.get("filter") {
        Some(serde_json::Value::Array(items)) => {
            let conditions = items
                .iter()
                .map(|item| {
                    item.as_str()
                        .ok_or_else(|| format!("filter entries must be strings, got {item}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            HostFilter::from_conditions(&host, &conditions)
        }
        _ => HostFilter::parse(&host, &setting(params, config, "filter")),
    }
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
// Settings: see domain::endpoint::BUILTIN_SETTINGS. host, group and filter
// select hosts and are ANDed; fields and exclude_fields shape vars here, for
// every format (csv also takes its column order from `fields`).
fn merge_and_filter(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> (HashMap<String, HostVars>, HashMap<String, Group>) {
    let group = setting(params, config, "group");
    let fields = comma_list(&setting(params, config, "fields"));
    let exclude_fields = comma_list(&setting(params, config, "exclude_fields"));
    // The HTTP handler validates the filter before it renders and answers 400
    // for a bad one, so a parse error cannot reach here from a request. If one
    // ever did (a bad value in an endpoint's static config, read by a caller
    // that skipped validation), the render fails closed: an empty inventory,
    // never an unfiltered one.
    let Ok(host_filter) = host_filter(config, params) else {
        return (HashMap::new(), HashMap::new());
    };

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

    // Host filters, ANDed.
    if !group.is_empty() {
        let mut allowed: HashSet<String> = HashSet::new();
        for name in comma_list(&group) {
            if let Some(group) = groups.get(&name) {
                allowed.extend(group.hosts.iter().cloned());
            }
        }
        hostvars.retain(|host, _| allowed.contains(host));
    }
    if !host_filter.is_empty() {
        hostvars.retain(|host, vars| host_filter.matches(host, vars));
    }

    // Shape the survivors' vars. `fields` keeps only the hostvars it names,
    // as on `GET /sources/{id}/dataset`: it picks columns of a host, and group
    // vars are not columns. `exclude_fields` drops the vars it names from
    // groups as well as hosts.
    //
    // A var declared on a group reaches every member of it once the consumer
    // resolves the inventory, so stripping only hostvars would let an excluded
    // name through the group door. exclude_fields is how something is kept out of
    // an endpoint, and a filter with a way around it is not one.
    //
    // Names may be dotted paths into objects (`monitor.name`), as on the
    // dataset route: see domain::var_path.
    let select = VarSelect::new(
        (!fields.is_empty()).then(|| fields.iter().map(String::as_str)),
        exclude_fields.iter().map(String::as_str),
    );
    if !select.is_identity() {
        for vars in hostvars.values_mut() {
            *vars = select.host(vars);
        }
        for group in groups.values_mut() {
            if let Some(vars) = group.vars.as_mut() {
                select.group_vars(vars);
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
    //
    // `children` lists are NOT rewritten here: a parent can be left naming a
    // group this pass dropped. Ansible reads an undefined child as an empty
    // group, and a dropped group had no vars and no children to lose, so the
    // inventory is unchanged by it — documented in docs/endpoints.md for the
    // consumers that walk the tree by name instead of by key.
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

// How many hosts and groups a call with no parameters of its own returns:
// the same merge and filter as the builtins, without rendering anything. For
// a page that wants the endpoint's size without fetching its output.
pub fn counts(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
) -> (usize, usize) {
    let (hostvars, groups) = merge_and_filter(datasets, config, &serde_json::json!({}));
    (hostvars.len(), groups.len())
}

// The hosts a call with no parameters of its own returns, by name: the same
// merge and filter, kept to the names. For a question about one host ("does
// this endpoint include it?") without rendering anything.
pub fn default_hosts(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
) -> HashSet<String> {
    merge_and_filter(datasets, config, &serde_json::json!({}))
        .0
        .into_keys()
        .collect()
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
// format. Filters: see merge_and_filter — `fields` there is what lets a
// search that wants three fields of a thousand hosts not ship all of them.
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
// hostvar name seen (sorted), and `fields` (comma-separated, in `config` or
// per request via `params`) picks and orders them instead. Groups do not
// appear — a row per host is the point; use `group` to select by one.
// Filters: see merge_and_filter.
pub fn render_csv(
    datasets: &HashMap<String, Arc<Dataset>>,
    config: &HashMap<String, String>,
    params: &serde_json::Value,
) -> String {
    let columns = comma_list(&setting(params, config, "fields"));
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
            // a column may be a dotted path, like `fields` itself
            out.push_str(&csv_field(&csv_value(var_path::lookup(vars, column))));
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

        let inv = render(&datasets, serde_json::json!({"filter": "os=linux"}));

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

        let inv = render(&datasets, serde_json::json!({"filter": "os=linux"}));

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
    fn exclude_fields_strips_named_hostvars() {
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
            serde_json::json!({"exclude_fields": "secret, serial"}),
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
    fn exclude_fields_strips_the_name_from_groups_too() {
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

        let inv = render(&datasets, serde_json::json!({"exclude_fields": "secret"}));

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
            serde_json::from_value(serde_json::json!({"filter": "os=linux"})).unwrap();
        // request asks for windows, overriding the static linux filter
        let out = render_ansible(
            &datasets,
            &config,
            &serde_json::json!({"filter": "os=windows"}),
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

    // `fields` reaches the ansible format as well as json and csv. It picks
    // hostvars only; `exclude_fields` reaches group vars too.
    #[test]
    fn fields_and_exclude_fields_shape_the_ansible_format() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux", "secret": "x", "role": "web"}}),
                serde_json::json!({"web": {"hosts": ["h1"], "vars": {"os": "l", "ntp": "n", "secret": "y"}}}),
            ),
        );

        let inv = render(
            &datasets,
            serde_json::json!({"fields": "os,role,secret", "exclude_fields": "secret"}),
        );

        assert_eq!(
            inv["_meta"]["hostvars"]["h1"],
            serde_json::json!({"os": "linux", "role": "web"})
        );
        assert_eq!(
            inv["web"]["vars"],
            serde_json::json!({"os": "l", "ntp": "n"})
        );
    }

    #[test]
    fn check_params_names_the_mistake() {
        let config = HashMap::new();
        let check = |params: serde_json::Value| check_params(&config, &params);
        assert!(check(serde_json::json!({"host": "a*", "filter": ["os=linux"]})).is_ok());
        assert!(
            check(serde_json::json!({"columns": "os"}))
                .unwrap_err()
                .contains("fields")
        );
        assert!(
            check(serde_json::json!({"fields": ["os"]}))
                .unwrap_err()
                .contains("string")
        );
        assert!(
            check(serde_json::json!({"group": 7}))
                .unwrap_err()
                .contains("string")
        );
        assert!(
            check(serde_json::json!({"filter": [1]}))
                .unwrap_err()
                .contains("strings")
        );
        assert!(
            check(serde_json::json!({"filter": "os"}))
                .unwrap_err()
                .contains("no operator")
        );
        // A parameter the builtins do not read is the caller's business.
        assert!(check(serde_json::json!({"limit": 5})).is_ok());
    }

    // A list entry is one condition: a `;` in it belongs to the value.
    #[test]
    fn a_filter_list_entry_is_one_whole_condition() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {"note": "a;b"}, "h2": {"note": "a"}}),
                serde_json::json!({}),
            ),
        );
        let out = render_json(
            &datasets,
            &HashMap::new(),
            &serde_json::json!({"filter": ["note=a;b"]}),
        );
        let merged: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            merged["hostvars"],
            serde_json::json!({"h1": {"note": "a;b"}})
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
            serde_json::json!({"filter": "os=linux", "exclude_fields": "secret"}),
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
    fn dotted_fields_shape_endpoints_and_name_csv_columns() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux", "mon": {"name": "h1", "status": "UP", "services": [1, 2]}}}),
                serde_json::json!({"g": {"hosts": ["h1"], "vars": {"mon": {"poller": "p", "secret": "s"}}}}),
            ),
        );
        let config: HashMap<String, String> = HashMap::new();
        let params =
            serde_json::json!({"fields": "mon.name,mon.status", "exclude_fields": "mon.secret"});
        let (hostvars, groups) = merge_and_filter(&datasets, &config, &params);
        assert_eq!(
            serde_json::to_value(&hostvars["h1"]).unwrap(),
            serde_json::json!({"mon": {"name": "h1", "status": "UP"}})
        );
        assert_eq!(
            serde_json::to_value(groups["g"].vars.as_ref().unwrap()).unwrap(),
            serde_json::json!({"mon": {"poller": "p"}})
        );
        // a csv column may be a path, and reads the value at it
        let out = render_csv(
            &datasets,
            &config,
            &serde_json::json!({"fields": "mon.status,os"}),
        );
        assert_eq!(out, "host,mon.status,os\nh1,UP,linux\n");
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
            serde_json::from_value(serde_json::json!({"fields": "role, os"})).unwrap();

        let out = render_csv(&datasets, &config, &serde_json::json!({}));
        assert_eq!(out, "host,role,os\nh1,web,linux\n");

        // A request parameter replaces the configured column list entirely.
        let out = render_csv(&datasets, &config, &serde_json::json!({"fields": "ram_gb"}));
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

    // =====================================================================
    // Limits: the constructed-inventory half. Merge everything the sources
    // carry, then hand back only the hosts one of them has.
    // =====================================================================

    fn limit_by(source_id: &str) -> EndpointLimit {
        EndpointLimit {
            by_hosts_from_inventory: Some(source_id.to_string()),
        }
    }

    // The shape this exists for: a CMDB that decides which hosts are real, and
    // a hypervisor that knows a great deal about hosts the CMDB never listed.
    fn cmdb_and_vmware() -> HashMap<String, Arc<Dataset>> {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src-cmdb".to_string(),
            dataset(
                serde_json::json!({
                    "motoko.section9.net": {"owner": "section9"},
                    "batou.section9.net": {"owner": "section9"}
                }),
                serde_json::json!({
                    "cmdb_managed": {"hosts": ["motoko.section9.net", "batou.section9.net"]}
                }),
            ),
        );
        datasets.insert(
            "src-vmware".to_string(),
            dataset(
                serde_json::json!({
                    "motoko.section9.net": {"vcpus": 8},
                    "decoy.vmware.local": {"vcpus": 2}
                }),
                serde_json::json!({
                    "cluster_a": {
                        "hosts": ["motoko.section9.net", "decoy.vmware.local"],
                        "vars": {"ntp": "ntp.section9.net"}
                    },
                    "cluster_b": {"hosts": ["decoy.vmware.local"], "vars": {"ntp": "ntp.dmz"}},
                    "cluster_c": {"hosts": ["decoy.vmware.local"]}
                }),
            ),
        );
        datasets
    }

    fn limited_inventory(limit: &EndpointLimit) -> serde_json::Value {
        let datasets = apply_limit(&cmdb_and_vmware(), limit);
        render(&datasets, serde_json::json!({}))
    }

    #[test]
    fn a_limit_keeps_only_the_hosts_the_named_inventory_has() {
        let inv = limited_inventory(&limit_by("src-cmdb"));
        let hostvars = &inv["_meta"]["hostvars"];

        assert!(
            hostvars.get("decoy.vmware.local").is_none(),
            "a host only the other source has must not reach the output: {}",
            inv
        );
        // Both of the limiting inventory's hosts survive -- including the one
        // the other source knows nothing about.
        assert!(hostvars.get("motoko.section9.net").is_some());
        assert!(hostvars.get("batou.section9.net").is_some());
    }

    // The whole point of a limit rather than a source list: everything is still
    // merged, so a surviving host arrives with every variable it collected.
    #[test]
    fn a_surviving_host_keeps_what_the_other_sources_said_about_it() {
        let inv = limited_inventory(&limit_by("src-cmdb"));
        let motoko = &inv["_meta"]["hostvars"]["motoko.section9.net"];

        assert_eq!(motoko["owner"], "section9", "from the limiting source");
        assert_eq!(motoko["vcpus"], 8, "from the source that did not limit");
    }

    #[test]
    fn group_membership_survives_the_limit_minus_the_hosts_it_removed() {
        let inv = limited_inventory(&limit_by("src-cmdb"));

        // The group belongs to the source that was limited away, and it still
        // reaches the output -- carrying its var and the member that survived.
        assert_eq!(
            inv["cluster_a"]["hosts"],
            serde_json::json!(["motoko.section9.net"])
        );
        assert_eq!(inv["cluster_a"]["vars"]["ntp"], "ntp.section9.net");
        assert_eq!(
            inv["cmdb_managed"]["hosts"],
            serde_json::json!(["batou.section9.net", "motoko.section9.net"])
        );
    }

    // A group whose every member was outside the limit is not a group a filter
    // emptied: the limit says which hosts the inventory has, not which groups
    // stopped meaning anything. It keeps its vars, exactly like a group that
    // arrived with vars and no hosts -- and goes only when it carries nothing.
    #[test]
    fn a_group_the_limit_emptied_keeps_its_vars_and_goes_only_when_it_has_none() {
        let inv = limited_inventory(&limit_by("src-cmdb"));

        assert_eq!(
            inv["cluster_b"]["vars"]["ntp"], "ntp.dmz",
            "an emptied group with vars still declares what it means: {}",
            inv
        );
        assert!(
            inv["cluster_b"].get("hosts").is_none(),
            "but it names none of the hosts the limit removed"
        );
        assert!(
            inv.get("cluster_c").is_none(),
            "an emptied group with nothing else to say is dropped"
        );
    }

    // apply_limit trims copies. The Arc it is handed is the cache's own entry,
    // shared with every concurrent reader, and a render must never mutate it.
    #[test]
    fn the_cached_datasets_are_left_untouched() {
        let datasets = cmdb_and_vmware();
        let before = Arc::clone(&datasets["src-vmware"]);

        let _limited = apply_limit(&datasets, &limit_by("src-cmdb"));

        assert!(
            before.hostvars.contains_key("decoy.vmware.local"),
            "the cached dataset lost a host to a render"
        );
        assert_eq!(
            before.groups["cluster_a"].hosts.len(),
            2,
            "the cached dataset lost group membership to a render"
        );
    }

    #[test]
    fn a_limit_that_names_no_rule_changes_nothing() {
        let unlimited = render(&cmdb_and_vmware(), serde_json::json!({}));
        let inv = limited_inventory(&EndpointLimit::default());
        assert_eq!(inv, unlimited);
    }

    // Fail closed. Config validation makes this unreachable (the limit source
    // must be one of the endpoint's sources, and the handler 503s on a source
    // with no cache entry), but the choice matters: an empty inventory is seen,
    // an inventory that quietly grew back to everything is not.
    #[test]
    fn a_limit_naming_a_source_that_is_not_here_renders_no_hosts() {
        let inv = limited_inventory(&limit_by("src-ghost"));

        assert_eq!(inv["_meta"]["hostvars"], serde_json::json!({}));
        assert!(inv.get("cmdb_managed").is_none());
    }

    // The limit runs before the transformer, so it applies to every builtin --
    // and composes with the filters rather than replacing them.
    #[test]
    fn the_limit_reaches_the_json_and_csv_builtins_and_composes_with_a_filter() {
        let datasets = apply_limit(&cmdb_and_vmware(), &limit_by("src-cmdb"));
        let config: HashMap<String, String> = HashMap::new();

        let merged: serde_json::Value =
            serde_json::from_str(&render_json(&datasets, &config, &serde_json::json!({}))).unwrap();
        assert!(merged["hostvars"].get("decoy.vmware.local").is_none());
        assert_eq!(merged["hostvars"]["motoko.section9.net"]["vcpus"], 8);

        let csv = render_csv(&datasets, &config, &serde_json::json!({}));
        assert_eq!(
            csv,
            "host,owner,vcpus\nbatou.section9.net,section9,\nmotoko.section9.net,section9,8\n"
        );

        // A filter still narrows what the limit left.
        let filtered: HashMap<String, String> =
            serde_json::from_value(serde_json::json!({"group": "cluster_a"})).unwrap();
        let inv: serde_json::Value = serde_json::from_str(&render_ansible(
            &datasets,
            &filtered,
            &serde_json::json!({}),
        ))
        .unwrap();
        assert!(inv["_meta"]["hostvars"].get("batou.section9.net").is_none());
        assert!(
            inv["_meta"]["hostvars"]
                .get("motoko.section9.net")
                .is_some()
        );
    }

    // Limiting by a source against itself is the identity, whatever the other
    // sources carry -- the case an operator hits first when trying one out.
    #[test]
    fn limiting_a_single_source_by_itself_changes_nothing() {
        let mut datasets = HashMap::new();
        datasets.insert(
            "src-only".to_string(),
            dataset(
                serde_json::json!({"h1": {"os": "linux"}}),
                serde_json::json!({"web": {"hosts": ["h1"]}}),
            ),
        );

        let limited = apply_limit(&datasets, &limit_by("src-only"));
        let cfg = HashMap::new();
        assert_eq!(
            render_ansible(&limited, &cfg, &serde_json::json!({})),
            render_ansible(&datasets, &cfg, &serde_json::json!({}))
        );
    }
}
