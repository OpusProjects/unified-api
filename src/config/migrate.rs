// The one-time conversion of a 0.34 configuration directory into the 0.35
// layout.
//
// Until 0.35 the directory was eight fixed files, each an id-keyed map:
//
//   # sources.yaml
//   src-d42:
//     name: "Device42"
//     connector_type: script
//     project_id: prj-inventory
//     script_path: dynamic/d42/inventory.py
//     ...
//
// From 0.35 every item is a list entry under a typed key, `name` is the
// identity, references lose their `_id` suffix, and what a source used to say
// about its SCRIPT moved to a connector item. A pod upgraded in place still
// has the old files on its volume, and refusing to start on them would brick
// it — so startup converts them, once, before loading:
//
// - detect: a legacy file name at the root whose top level is a map of maps
//   (a 0.35 file has lists under its keys, never maps);
// - plan: compute every new file in memory, connectors extracted from the
//   sources and de-duplicated;
// - validate: write the plan into a hidden staging folder beside the real
//   files and load it exactly as startup would — nothing moves if it fails;
// - commit: move the old files into `.legacy-config-<timestamp>/` (hidden,
//   so the loader skips it) and rename the new ones into place.
//
// Comments in the old files cannot survive: they are not data. The backup
// keeps them, next to the new files, for whoever wants to carry them over.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::{REQUIRED_CONFIG_FILE, config_files, is_safe_id};

// The 0.34 files and the section each becomes.
pub const LEGACY_FILES: [(&str, &str); 7] = [
    ("credentials.yaml", "credentials"),
    ("projects.yaml", "projects"),
    ("sources.yaml", "sources"),
    ("views.yaml", "views"),
    ("enrichers.yaml", "enrichers"),
    ("endpoints.yaml", "endpoints"),
    ("api_keys.yaml", "api_keys"),
];

// Reference keys that dropped their `_id` suffix, per section.
const RENAMES: [(&str, &str, &str); 7] = [
    ("projects", "credential_id", "credential"),
    ("enrichers", "target_id", "target"),
    ("enrichers", "source_id", "source"),
    ("enrichers", "project_id", "project"),
    ("endpoints", "source_ids", "sources"),
    ("endpoints", "project_id", "project"),
    ("sources", "credential_ids", "credentials"),
];

// What a source carried about its script, which now belongs to a connector.
const CONNECTOR_KEYS: [&str; 6] = [
    "connector_type",
    "project_id",
    "script_path",
    "script_args",
    "host_args",
    "output_format",
];

// Key order in the files written, per section: identity and label first,
// then references, then the rest in the order the docs describe them. Keys
// not listed (a typo the old loader would have refused too) follow,
// alphabetically, so the new loader names them.
const ORDER: [(&str, &[&str]); 10] = [
    (
        "sources",
        &[
            "connector",
            "credentials",
            "project",
            "path",
            "timeout_seconds",
            "sync_mode",
            "schedule",
            "sync_interval_seconds",
            "ttl_seconds",
            "ttl_overrides",
            "allow_on_demand_refresh",
            "host_sync_updates_group_vars",
            "hosts_from_source",
            "advertise_scope",
            "config",
        ],
    ),
    (
        "connectors",
        &[
            "type",
            "project",
            "location",
            "path",
            "script",
            "script_args",
            "host_args",
            "timeout_seconds",
        ],
    ),
    ("hosts", &["sources"]),
    ("groups", &["sources"]),
    ("views", &["members", "ttl_seconds"]),
    (
        "endpoints",
        &[
            "sources",
            "output",
            "project",
            "script_path",
            "script_args",
            "timeout_seconds",
            "limit",
            "config",
        ],
    ),
    (
        "enrichers",
        &[
            "target",
            "source",
            "project",
            "script_path",
            "script_args",
            "fields",
            "fields_excluded",
            "groups",
            "groups_excluded",
            "schedule",
            "sync_interval_seconds",
            "timeout_seconds",
            "config",
        ],
    ),
    (
        "projects",
        &[
            "git_url",
            "branch",
            "credential",
            "sync_on_boot",
            "schedule",
            "sync_interval_seconds",
            "timeout_seconds",
            "python_venv",
        ],
    ),
    (
        "credentials",
        &[
            "type",
            "vault_path",
            "env_prefix",
            "secret_file",
            "secret_keys",
            "file_keys",
        ],
    ),
    ("api_keys", &["env", "role", "sources", "endpoints"]),
];

// The new layout, computed and not yet written.
#[derive(Debug, Default)]
pub struct MigrationPlan {
    // The legacy files found at the root (they move to the backup).
    pub legacy_files: Vec<String>,
    // Relative path → contents, one file per item: `<section>/<name>.yaml`.
    pub files: BTreeMap<String, String>,
    // Each generated connector and the sources that use it.
    pub connectors: BTreeMap<String, Vec<String>>,
    // Anything worth a line in the log that the files themselves do not say.
    pub notes: Vec<String>,
}

// What a migration did, for the log.
#[derive(Debug)]
pub struct MigrationOutcome {
    pub plan: MigrationPlan,
    pub backup_dir: PathBuf,
}

impl MigrationPlan {
    // One line per fact, for the startup log and `--migrate-config`.
    pub fn summary(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "converted {} into {} item files",
            self.legacy_files.join(", "),
            self.files.len()
        )];
        for (connector, users) in &self.connectors {
            lines.push(format!(
                "connector '{}' extracted from: {}",
                connector,
                users.join(", ")
            ));
        }
        lines.extend(self.notes.iter().cloned());
        lines
    }
}

// The legacy files present at the root of `dir`, if the directory is in the
// 0.34 layout; empty when it is not. An error when a legacy-named file is in
// neither shape, which is the one case where guessing would be wrong.
pub fn detect(dir: &Path) -> Result<Vec<String>, String> {
    let mut legacy = Vec::new();
    let mut empty = Vec::new();
    for (file, _) in LEGACY_FILES {
        let path = dir.join(file);
        if !path.is_file() {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|e| format!("{file}: {e}"))?;
        match shape(&text).map_err(|e| format!("{file}: {e}"))? {
            Shape::Legacy => legacy.push(file.to_string()),
            Shape::Empty => empty.push(file.to_string()),
            Shape::Items => {}
        }
    }
    // An empty legacy file is moved with the others (it is part of what the
    // old directory was), but on its own it says nothing about the layout.
    if !legacy.is_empty() {
        legacy.extend(empty);
        legacy.sort();
    }
    Ok(legacy)
}

enum Shape {
    Legacy,
    Items,
    Empty,
}

// A 0.35 file holds lists under its top-level keys; a 0.34 file holds maps
// under ids. Nothing written for one layout reads as the other.
fn shape(text: &str) -> Result<Shape, String> {
    match crate::domain::yaml::to_json(text)? {
        Value::Null => Ok(Shape::Empty),
        Value::Object(top) if top.is_empty() => Ok(Shape::Empty),
        Value::Object(top) => {
            let maps = top.values().filter(|v| v.is_object()).count();
            if maps == top.len() {
                Ok(Shape::Legacy)
            } else if maps == 0 {
                Ok(Shape::Items)
            } else {
                Err(
                    "mixes id-keyed items (the 0.34 layout) with lists (0.35) — \
                     convert it by hand"
                        .to_string(),
                )
            }
        }
        _ => Ok(Shape::Items),
    }
}

// Compute the new layout for `dir`, without writing anything. None when the
// directory is not in the old layout.
pub fn plan(dir: &Path) -> Result<Option<MigrationPlan>, String> {
    let legacy = detect(dir)?;
    if legacy.is_empty() {
        return Ok(None);
    }
    let mut plan = MigrationPlan {
        legacy_files: legacy.clone(),
        ..MigrationPlan::default()
    };

    // Section → id → item body (already renamed), in file order.
    let mut sections: BTreeMap<&str, BTreeMap<String, Map<String, Value>>> = BTreeMap::new();
    for (file, section) in LEGACY_FILES {
        if !legacy.iter().any(|f| f == file) {
            continue;
        }
        let text = fs::read_to_string(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
        let typed = crate::domain::yaml::to_json(&text)?;
        let verbatim = crate::domain::yaml::to_text_json(&text)?;
        let Value::Object(items) = keep_plain_text(typed, verbatim) else {
            continue;
        };
        for (id, body) in items {
            let Value::Object(body) = body else {
                return Err(format!("{file}: '{id}' is not a mapping"));
            };
            if !is_safe_id(&id) {
                return Err(format!(
                    "{file}: '{id}' cannot be an item name — names may only use letters, \
                     digits, '-', '_' and '.'"
                ));
            }
            sections
                .entry(section)
                .or_default()
                .insert(id, renamed(section, body));
        }
    }

    // Sources: split each into the source and its connector.
    let sources = sections.remove("sources").unwrap_or_default();
    let (sources, connectors) = extract_connectors(sources, &mut plan)?;
    sections.insert("sources", sources);
    sections.insert("connectors", connectors);

    for (section, items) in &sections {
        for (id, body) in items {
            let path = format!("{section}/{id}.yaml");
            plan.files.insert(path, item_file(section, id, body));
        }
    }
    Ok(Some(plan))
}

// `name` → `description`, the `_id` references renamed.
fn renamed(section: &str, mut body: Map<String, Value>) -> Map<String, Value> {
    if let Some(name) = body.remove("name") {
        body.insert("description".to_string(), name);
    }
    for (s, old, new) in RENAMES {
        if s == section
            && let Some(value) = body.remove(old)
        {
            body.insert(new.to_string(), value);
        }
    }
    body
}

// A plain scalar whose text is not how its value prints (`1.10`, `0x1F`,
// `True`, `+3`), carried through the plan as `{PLAIN: "<its text>"}` and
// written back as that same text. As a number it would be written `1.1` or
// `31`: a `config:` value or a script argument changed on disk, with the
// original only in the backup. Written verbatim, it reads back exactly as the
// 0.34 file did — as text where a field wants text, as the number where one
// wants a number. The key starts with a NUL, which no configuration key has.
const PLAIN: &str = "\u{0}plain";

fn keep_plain_text(typed: Value, text: Value) -> Value {
    match (typed, text) {
        (Value::Object(typed), Value::Object(mut text)) => Value::Object(
            typed
                .into_iter()
                .map(|(k, v)| {
                    let t = text.remove(&k).unwrap_or(Value::Null);
                    (k, keep_plain_text(v, t))
                })
                .collect(),
        ),
        (Value::Array(typed), Value::Array(text)) => Value::Array(
            typed
                .into_iter()
                .zip(text.into_iter().chain(std::iter::repeat(Value::Null)))
                .map(|(v, t)| keep_plain_text(v, t))
                .collect(),
        ),
        (value @ (Value::Number(_) | Value::Bool(_)), Value::String(text)) => {
            let printed = value.to_string();
            if printed == text {
                value
            } else {
                Value::Object(Map::from_iter([(PLAIN.to_string(), Value::String(text))]))
            }
        }
        (value, _) => value,
    }
}

// The text a carried scalar was written as (see PLAIN).
fn plain(value: &Value) -> Option<&str> {
    match value {
        Value::Object(map) if map.len() == 1 => map.get(PLAIN).and_then(Value::as_str),
        _ => None,
    }
}

// A value as text: a string, or a scalar's own text.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(_) | Value::Bool(_) => Some(value.to_string()),
        other => plain(other).map(str::to_string),
    }
}

// A whole number, however it was written (`600`, `0x258`, `+600`).
fn as_u64(value: &Value) -> Option<u64> {
    match plain(value) {
        Some(text) => crate::domain::yaml::to_json(text).ok()?.as_u64(),
        None => value.as_u64(),
    }
}

type Items = BTreeMap<String, Map<String, Value>>;

// What makes two sources share a connector: everything a connector holds
// except its timeout, which a source may override.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone)]
struct ConnectorKey {
    kind: String,
    project: Option<String>,
    // ssh: `host` for the facts a source gathered (script_path is then the
    // pattern of facts files); None otherwise.
    location: Option<String>,
    // ssh: script_path is the script itself, written as `script:`.
    inline: bool,
    script_path: String,
    script_args: Vec<String>,
    host_args: Vec<String>,
    // What the connector is named after: the 0.34 script_path, which for
    // ssh was the label (gather_facts) or the command.
    hint: String,
}

fn extract_connectors(sources: Items, plan: &mut MigrationPlan) -> Result<(Items, Items), String> {
    let mut out = Items::new();
    // key → (source id, timeout) of every source using it, in id order
    let mut groups: BTreeMap<ConnectorKey, Vec<(String, u64)>> = BTreeMap::new();
    let default_timeout = crate::domain::default_timeout_seconds();

    for (id, mut body) in sources {
        let text = |body: &Map<String, Value>, key: &str| -> Option<String> {
            body.get(key).and_then(as_text)
        };
        let list = |body: &Map<String, Value>, key: &str| -> Vec<String> {
            body.get(key)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|v| as_text(v).unwrap_or_else(|| v.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        };
        let kind = text(&body, "connector_type").unwrap_or_else(|| "script".to_string());
        let project = text(&body, "project_id");
        let script_path = text(&body, "script_path").unwrap_or_default();
        let script_args = list(&body, "script_args");
        let host_args = list(&body, "host_args");
        let output_format = text(&body, "output_format");
        for key in CONNECTOR_KEYS {
            body.remove(key);
        }

        match kind.as_str() {
            "remote" => {
                // script_path was the source id on the remote instance; it
                // now sits in config beside the URL. project_id was required
                // and never used.
                let config = body
                    .entry("config")
                    .or_insert_with(|| Value::Object(Map::new()));
                let Value::Object(config) = config else {
                    return Err(format!(
                        "sources.yaml: '{id}' has a config that is not a map"
                    ));
                };
                if config.contains_key("source") {
                    return Err(format!(
                        "sources.yaml: '{id}' is a remote source whose config already has a \
                         'source' key, which 0.35 uses for the remote source id"
                    ));
                }
                config.insert("source".to_string(), Value::String(script_path));
                body.insert("connector".to_string(), Value::String("remote".into()));
                note_ignored(plan, &id, &script_args, &host_args, &output_format);
            }
            "static_inventory" => {
                body.insert(
                    "connector".to_string(),
                    Value::String("static_inventory".into()),
                );
                if let Some(project) = project {
                    body.insert("project".to_string(), Value::String(project));
                }
                body.insert("path".to_string(), Value::String(script_path));
                note_ignored(plan, &id, &script_args, &host_args, &output_format);
            }
            "script" | "ssh" => {
                let ssh = kind == "ssh";
                if ssh && project.is_some() {
                    // Required in 0.34 and never used: an ssh source runs its
                    // script_path on the remote host.
                    plan.notes.push(format!(
                        "source '{id}': its project_id was dropped — an ssh connector \
                         runs nothing from a checkout"
                    ));
                }
                // 0.36 reads one output, Ansible inventory JSON; 0.34's
                // default was the other one. The script is not ours to
                // change, so the note says what its syncs will need.
                if !ssh && output_format.as_deref() != Some("ansible") {
                    plan.notes.push(format!(
                        "source '{id}': its script prints the {{\"hostvars\", \"groups\"}} \
                         format, which 0.36 no longer reads — its syncs fail until it prints \
                         Ansible inventory JSON (host variables under _meta.hostvars, each \
                         group a top-level key)"
                    ));
                }
                let hint = script_path.clone();
                // What an ssh source's gather_mode meant moves to its
                // connector: facts become files on the host, a script
                // becomes the connector's own `script`.
                let (location, inline, script_path, script_args) = if ssh {
                    let mut config = body.get_mut("config").and_then(Value::as_object_mut);
                    let mut take = |key: &str| {
                        config
                            .as_deref_mut()
                            .and_then(|c| c.remove(key))
                            .and_then(|v| as_text(&v))
                    };
                    let mode = take("gather_mode").unwrap_or_else(|| "facts".to_string());
                    let fact_path =
                        take("fact_path").unwrap_or_else(|| "/etc/ansible/facts.d".to_string());
                    if mode == "facts" {
                        let dir = crate::domain::connector::host_path_from_0_35(&fact_path);
                        plan.notes.push(format!(
                            "source '{id}': its facts are read from {dir}/*.fact — 0.35 also \
                             read *.json there"
                        ));
                        (
                            Some("host".to_string()),
                            false,
                            format!("{dir}/*.fact"),
                            Vec::new(),
                        )
                    } else {
                        // 0.35 appended the arguments to the command raw;
                        // a script's own arguments are $1, $2 instead, so
                        // they go into its line as they were.
                        let script =
                            crate::domain::connector::command_from_0_35(&script_path, &script_args);
                        (None, true, script, Vec::new())
                    }
                } else {
                    (None, false, script_path, script_args)
                };
                let key = ConnectorKey {
                    kind: kind.clone(),
                    project: if ssh { None } else { project },
                    location,
                    inline,
                    script_path,
                    script_args,
                    host_args: if ssh { Vec::new() } else { host_args },
                    hint,
                };
                let timeout = body
                    .get("timeout_seconds")
                    .and_then(as_u64)
                    .unwrap_or(default_timeout);
                groups.entry(key).or_default().push((id.clone(), timeout));
            }
            other => {
                return Err(format!(
                    "sources.yaml: '{id}' has an unknown connector_type '{other}'"
                ));
            }
        }
        out.insert(id, body);
    }

    // Name each connector, then point its sources at it.
    let names = connector_names(groups.keys());
    let mut connectors = Items::new();
    for ((key, users), name) in groups.into_iter().zip(names) {
        // The timeout most of its sources used is the connector's; a source
        // that differs keeps its own as an override.
        let mut tally: BTreeMap<u64, usize> = BTreeMap::new();
        for (_, t) in &users {
            *tally.entry(*t).or_default() += 1;
        }
        let timeout = tally
            .iter()
            .max_by_key(|(t, n)| (**n, **t))
            .map(|(t, _)| *t)
            .unwrap_or(default_timeout);

        let mut body = Map::new();
        body.insert("type".to_string(), Value::String(key.kind.clone()));
        if let Some(project) = &key.project {
            body.insert("project".to_string(), Value::String(project.clone()));
        }
        if let Some(location) = &key.location {
            body.insert("location".to_string(), Value::String(location.clone()));
        }
        // `path`, as on every item where `project` + `path` name a file in a
        // checkout; a source's 0.34 `script_path` is that file. An ssh
        // script is the connector's own `script`.
        let field = if key.inline { "script" } else { "path" };
        body.insert(field.to_string(), Value::String(key.script_path.clone()));
        if !key.script_args.is_empty() {
            body.insert("script_args".to_string(), strings(&key.script_args));
        }
        if !key.host_args.is_empty() {
            body.insert("host_args".to_string(), strings(&key.host_args));
        }
        if timeout != default_timeout {
            body.insert("timeout_seconds".to_string(), Value::from(timeout));
        }

        for (source_id, t) in &users {
            let source = out.get_mut(source_id).expect("grouped from out");
            source.insert("connector".to_string(), Value::String(name.clone()));
            if *t == timeout {
                source.remove("timeout_seconds");
            } else {
                source.insert("timeout_seconds".to_string(), Value::from(*t));
            }
        }
        plan.connectors.insert(
            name.clone(),
            users.iter().map(|(id, _)| id.clone()).collect(),
        );
        connectors.insert(name, body);
    }
    Ok((out, connectors))
}

// One distinct name per connector, in `keys` order. Each takes its base name
// when it is the first to want it and it is free; the rest take the lowest
// free `<base>-N`. Free means: not a built-in connector's name (a script in a
// folder called `remote` would otherwise be refused by validation), and not
// any name already handed out — counting repeats of one base was not enough,
// as the second `foo.py` became `foo-2` while a script `foo-2.py` was also
// `foo-2`, and one connector silently replaced the other.
fn connector_names<'k>(keys: impl Iterator<Item = &'k ConnectorKey>) -> Vec<String> {
    let bases: Vec<String> = keys.map(connector_base_name).collect();
    let mut used: HashSet<String> = crate::domain::connector::BUILT_IN
        .iter()
        .map(|b| b.to_string())
        .collect();
    let mut names: Vec<Option<String>> = bases
        .iter()
        .map(|base| used.insert(base.clone()).then(|| base.clone()))
        .collect();
    for (name, base) in names.iter_mut().zip(&bases) {
        if name.is_none() {
            let free = (2..)
                .map(|n| format!("{base}-{n}"))
                .find(|candidate| !used.contains(candidate))
                .expect("an unbounded range finds a free name");
            used.insert(free.clone());
            *name = Some(free);
        }
    }
    names.into_iter().flatten().collect()
}

fn note_ignored(
    plan: &mut MigrationPlan,
    id: &str,
    script_args: &[String],
    host_args: &[String],
    output_format: &Option<String>,
) {
    if !script_args.is_empty() || !host_args.is_empty() || output_format.is_some() {
        plan.notes.push(format!(
            "source '{id}': script_args/host_args/output_format were dropped — its \
             built-in connector never used them"
        ));
    }
}

fn strings(items: &[String]) -> Value {
    Value::Array(items.iter().cloned().map(Value::String).collect())
}

// A readable name for a generated connector: the script's own name, or its
// folder when the file is called something generic (`d42/inventory.py` is
// "d42"); `ssh-<command>` for an ssh connector with a one-word path,
// `ssh-script` for one that carries a whole remote script.
fn connector_base_name(key: &ConnectorKey) -> String {
    let path = key.hint.trim();
    if key.kind == "ssh" {
        return if is_safe_id(path) && !path.contains('.') {
            format!("ssh-{}", path.replace('_', "-"))
        } else {
            "ssh-script".to_string()
        };
    }
    let p = Path::new(path);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let generic = ["inventory", "main", "index", "run", "script"];
    let chosen = if generic.iter().any(|g| stem.starts_with(g)) {
        p.parent()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or(stem)
    } else {
        stem
    };
    let cleaned: String = chosen
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "connector".to_string()
    } else {
        cleaned
    }
}

// One item file: `---`, the section key, the item with `name` first.
fn item_file(section: &str, id: &str, body: &Map<String, Value>) -> String {
    let order: &[&str] = ORDER
        .iter()
        .find(|(s, _)| *s == section)
        .map(|(_, o)| *o)
        .unwrap_or(&[]);
    let mut keys: Vec<&String> = Vec::new();
    for wanted in std::iter::once(&"description").chain(order.iter()) {
        if let Some((k, _)) = body.get_key_value(*wanted) {
            keys.push(k);
        }
    }
    let mut rest: Vec<&String> = body
        .keys()
        .filter(|k| k.as_str() != "description" && !order.contains(&k.as_str()))
        .collect();
    rest.sort();
    keys.extend(rest);

    let mut out = format!("---\n{section}:\n  - name: {}\n", scalar_key(id));
    for key in keys {
        emit_entry(&mut out, 4, key, &body[key]);
    }
    out
}

// A minimal YAML writer: block mappings, flow lists of scalars, block lists
// of mappings, strings double-quoted (a JSON string is a valid YAML
// double-quoted scalar) or as a literal block when they span lines. Enough
// for configuration, and what it writes reads back through domain::yaml as
// the same values.
fn emit_entry(out: &mut String, indent: usize, key: &str, value: &Value) {
    let pad = " ".repeat(indent);
    match value {
        _ if plain(value).is_some() => {
            out.push_str(&format!("{pad}{}: {}\n", scalar_key(key), inline(value)));
        }
        Value::Object(map) if !map.is_empty() => {
            out.push_str(&format!("{pad}{}:\n", scalar_key(key)));
            for (k, v) in nested_order(map) {
                emit_entry(out, indent + 2, k, v);
            }
        }
        // Lists of mappings, and lists too long to read on one line, as block
        // lists: an endpoint's eight sources read better one per line.
        Value::Array(items)
            if items
                .iter()
                .any(|v| (v.is_object() && plain(v).is_none()) || v.is_array())
                || indent + key.len() + inline(value).len() > 96 =>
        {
            out.push_str(&format!("{pad}{}:\n", scalar_key(key)));
            for item in items {
                emit_list_item(out, indent + 2, item);
            }
        }
        Value::String(s) if literal_block(s) => {
            out.push_str(&format!(
                "{pad}{}: {}\n",
                scalar_key(key),
                block(s, indent + 2)
            ));
        }
        other => {
            out.push_str(&format!("{pad}{}: {}\n", scalar_key(key), inline(other)));
        }
    }
}

fn emit_list_item(out: &mut String, indent: usize, item: &Value) {
    let pad = " ".repeat(indent);
    match item {
        Value::Object(map) if !map.is_empty() && plain(item).is_none() => {
            let mut first = true;
            for (k, v) in nested_order(map) {
                let mut entry = String::new();
                emit_entry(&mut entry, indent + 2, k, v);
                if first {
                    // "    key: v" → "  - key: v"
                    entry.replace_range(indent..indent + 2, "- ");
                    first = false;
                }
                out.push_str(&entry);
            }
        }
        other => out.push_str(&format!("{pad}- {}\n", inline(other))),
    }
}

// Inside an item, what a nested mapping is ABOUT goes first (a view member's
// `source`, a remote's `url`); the rest stays alphabetical.
fn nested_order(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    const FIRST: [&str; 3] = ["source", "url", "name"];
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by_key(|(k, _)| {
        (
            FIRST.iter().position(|f| f == k).unwrap_or(FIRST.len()),
            k.to_string(),
        )
    });
    entries
}

fn inline(value: &Value) -> String {
    if let Some(text) = plain(value) {
        return text.to_string();
    }
    match value {
        Value::Null => "null".to_string(),
        Value::Object(map) if map.is_empty() => "{}".to_string(),
        // Flow style: `["--list"]`, which is how the files were written by hand.
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(inline).collect::<Vec<_>>().join(", ")
        ),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        other => other.to_string(),
    }
}

// A multi-line string reads better as a literal block — a remote script was
// written that way. Leading whitespace or control characters other than the
// newlines would need an indentation indicator or escapes, so those stay
// double-quoted, which is always exact.
fn literal_block(s: &str) -> bool {
    s.contains('\n')
        && !s.starts_with([' ', '\t', '\n'])
        && !s.chars().any(|c| c.is_control() && c != '\n')
        && s.lines().all(|l| l.trim_end() == l)
}

fn block(s: &str, indent: usize) -> String {
    let pad = " ".repeat(indent);
    let body = s.trim_end_matches('\n');
    let trailing = s.len() - body.len();
    let chomp = match trailing {
        0 => "|-",
        1 => "|",
        _ => "|+",
    };
    let mut out = chomp.to_string();
    for line in body.split('\n') {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(&pad);
            out.push_str(line);
        }
    }
    for _ in 1..trailing {
        out.push('\n');
    }
    out
}

fn scalar_key(key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !key.starts_with(['-', '.'])
        && !matches!(
            key.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off"
        );
    if plain {
        key.to_string()
    } else {
        serde_json::to_string(key).unwrap_or_default()
    }
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

// `<pid>-<timestamp>-<n>`: a temporary folder name no other check or copy in
// this process (the tests run many at once) or another one can be using.
fn unique_suffix() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{}-{n}", std::process::id(), timestamp())
}

// Load the planned layout as startup would, in a scratch copy of the
// directory: config.yaml, every 0.35 file already there, and the plan.
// `scratch` must be new (or empty); it is left for the caller to remove.
fn validate_in(dir: &Path, plan: &MigrationPlan, scratch: &Path) -> Result<(), String> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for name in config_files(dir)? {
        if plan.legacy_files.contains(&name) {
            continue;
        }
        let text = fs::read_to_string(dir.join(&name)).map_err(|e| format!("{name}: {e}"))?;
        files.insert(name, text);
    }
    for (name, text) in &plan.files {
        if files.contains_key(name) {
            return Err(format!(
                "{name} already exists — the migration will not overwrite it; move it \
                 aside and start again"
            ));
        }
        files.insert(name.clone(), text.clone());
    }
    if !files.contains_key(REQUIRED_CONFIG_FILE) {
        return Err(format!("{REQUIRED_CONFIG_FILE} is missing"));
    }
    write_tree(scratch, &files)?;
    // As startup will load it: a 0.35-form ssh connector already among the
    // item files is converted in memory there, so it must not refuse here.
    super::load_config_upgrading(scratch)
        .map(|_| ())
        .map_err(|e| format!("the converted configuration does not load: {e}"))
}

fn write_tree(root: &Path, files: &BTreeMap<String, String>) -> Result<(), String> {
    for (name, text) in files {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(())
}

// Plan and validate without touching `dir`: what `--check-config` runs on a
// directory still in the old layout (a CI checkout mounted read-only).
pub fn check(dir: &Path) -> Result<Option<MigrationPlan>, String> {
    let Some(plan) = plan(dir)? else {
        return Ok(None);
    };
    let scratch = std::env::temp_dir().join(format!("unified-api-migrate-{}", unique_suffix()));
    let result = validate_in(dir, &plan, &scratch);
    let _ = fs::remove_dir_all(&scratch);
    result.map(|()| Some(plan))
}

// Convert `dir` in place, when it is in the old layout. Nothing is moved
// unless the converted configuration loads; a failure part way through the
// moves puts back what had been moved.
pub fn migrate_in_place(dir: &Path) -> Result<Option<MigrationOutcome>, String> {
    let Some(plan) = plan(dir)? else {
        return Ok(None);
    };
    let stamp = timestamp();
    let staging = dir.join(format!(".migrate-staging-{stamp}"));
    if let Err(e) = validate_in(dir, &plan, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(e);
    }

    let backup_dir = dir.join(format!(".legacy-config-{stamp}"));
    let result = commit(dir, &plan, &staging, &backup_dir);
    let _ = fs::remove_dir_all(&staging);
    result.map(|()| Some(MigrationOutcome { plan, backup_dir }))
}

// Where startup should load the configuration from, after converting it if
// it had to be.
#[derive(Debug)]
pub enum StartupConfig {
    // Already in the item-file layout: load it where it is.
    AsIs,
    // Converted in place (see migrate_in_place).
    Migrated(MigrationOutcome),
    // The directory is read-only — a Kubernetes ConfigMap, a `:ro` bind mount
    // — so the conversion was written to a private temporary copy instead,
    // and THAT is what to load. Nothing is lost by it (the source of truth is
    // wherever the read-only files come from, and it still needs converting),
    // and the alternative was a pod that does not start.
    Converted {
        copy: ConvertedCopy,
        plan: MigrationPlan,
    },
}

// The temporary copy a read-only directory was converted into. It is what the
// process runs on, so it lives as long as the process: whoever holds this
// keeps the copy, and dropping it removes it. A copy is locked while in use;
// a process that never dropped its own (killed, crashed) leaves one unlocked,
// and the next start removes it — without that, a crash-looping pod on a
// persistent /tmp (an emptyDir survives container restarts) added one folder
// per restart, forever.
#[derive(Debug)]
pub struct ConvertedCopy {
    dir: PathBuf,
    // Held, never read: the lock lasts as long as the file stays open.
    _lock: fs::File,
}

impl ConvertedCopy {
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

// `Drop` is Rust's destructor: it runs when the value goes out of scope, which
// for main's copy is when main returns after the graceful shutdown.
impl Drop for ConvertedCopy {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const COPY_PREFIX: &str = "unified-api-config-";
const COPY_LOCK: &str = ".in-use";
// A copy older than this with no lock file at all is a leftover from a
// version that did not lock its copies (or of a start that died before it
// could): never a copy being created right now, which locks within
// microseconds of existing.
const UNLOCKED_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

// Remove the converted copies in `temp` that no running process holds.
fn sweep_stale_copies(temp: &Path) {
    let Ok(entries) = fs::read_dir(temp) else {
        return;
    };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(COPY_PREFIX) {
            continue;
        }
        let path = entry.path();
        // Not through a symlink: /tmp is shared, and a link planted under
        // this name must not point the removal anywhere else.
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        let stale = match fs::File::open(path.join(COPY_LOCK)) {
            // Lockable means nobody holds it. The file closes (and the lock
            // goes) before the folder is removed.
            Ok(lock) => lock.try_lock().is_ok(),
            Err(_) => meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > UNLOCKED_GRACE),
        };
        if stale {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

// A new, locked, empty folder for a converted copy.
fn new_copy(temp: &Path) -> Result<ConvertedCopy, String> {
    let dir = temp.join(format!("{COPY_PREFIX}{}", unique_suffix()));
    fs::create_dir(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let lock = fs::File::create(dir.join(COPY_LOCK))
        .and_then(|file| file.lock().map(|()| file))
        .map_err(|e| {
            let _ = fs::remove_dir_all(&dir);
            format!("lock {}: {e}", dir.display())
        })?;
    Ok(ConvertedCopy { dir, _lock: lock })
}

// What main runs before loading: in place when it can, in a temporary copy
// when the directory cannot be written, and an error (nothing changed) when
// the conversion would not load either way.
pub fn prepare_for_startup(dir: &Path) -> Result<StartupConfig, String> {
    if detect(dir)?.is_empty() {
        return Ok(StartupConfig::AsIs);
    }
    if writable(dir) {
        return migrate_in_place(dir).map(|outcome| match outcome {
            Some(outcome) => StartupConfig::Migrated(outcome),
            None => StartupConfig::AsIs,
        });
    }
    let plan = plan(dir)?.ok_or("the directory stopped being in the 0.34 layout")?;
    let temp = std::env::temp_dir();
    sweep_stale_copies(&temp);
    // Dropped on failure, which removes it.
    let copy = new_copy(&temp)?;
    validate_in(dir, &plan, copy.path())?;
    Ok(StartupConfig::Converted { copy, plan })
}

// Whether the migration could write here: a probe folder, created and
// removed. Cheaper to try than to predict (read-only filesystem, a ConfigMap,
// permissions, an overlay).
fn writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".migrate-probe-{}", std::process::id()));
    match fs::create_dir(&probe) {
        Ok(()) => {
            let _ = fs::remove_dir(&probe);
            true
        }
        Err(_) => false,
    }
}

fn commit(dir: &Path, plan: &MigrationPlan, staging: &Path, backup: &Path) -> Result<(), String> {
    fs::create_dir_all(backup).map_err(|e| format!("create {}: {e}", backup.display()))?;
    let mut moved_out: Vec<&String> = Vec::new();
    let mut moved_in: Vec<&String> = Vec::new();

    let rollback = |moved_out: &[&String], moved_in: &[&String]| {
        for name in moved_in {
            let _ = fs::remove_file(dir.join(name));
            let mut parent = dir.join(name);
            while parent.pop() && parent != dir && fs::remove_dir(&parent).is_ok() {}
        }
        for name in moved_out {
            let _ = fs::rename(backup.join(name), dir.join(name));
        }
        let _ = fs::remove_dir(backup);
    };

    for name in &plan.legacy_files {
        if let Err(e) = fs::rename(dir.join(name), backup.join(name)) {
            rollback(&moved_out, &moved_in);
            return Err(format!("back up {name}: {e}"));
        }
        moved_out.push(name);
    }
    for name in plan.files.keys() {
        let target = dir.join(name);
        let step = target
            .parent()
            .map(fs::create_dir_all)
            .unwrap_or(Ok(()))
            .and_then(|()| fs::rename(staging.join(name), &target));
        if let Err(e) = step {
            rollback(&moved_out, &moved_in);
            return Err(format!("write {name}: {e}"));
        }
        moved_in.push(name);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        dir
    }

    const PROJECTS: &str = "prj-inv:\n  name: \"Inventory\"\n  git_url: \"https://example.invalid/inv.git\"\n  credential_id: \"cred-gh\"\n";
    const CREDENTIALS: &str =
        "cred-gh:\n  name: \"GitHub\"\n  type: \"token\"\n  env_prefix: \"GH\"\n";

    #[test]
    fn a_new_layout_is_left_alone() {
        let dir = dir_with(&[("sources.yaml", "sources: []\n")]);
        assert!(detect(dir.path()).unwrap().is_empty());
        assert!(migrate_in_place(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_file_in_neither_layout_is_refused() {
        let dir = dir_with(&[("sources.yaml", "sources: []\nsrc-a:\n  name: a\n")]);
        assert!(detect(dir.path()).unwrap_err().contains("mixes"));
    }

    #[test]
    fn sources_sharing_a_script_share_one_connector() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                concat!(
                    "src-a:\n  name: \"A\"\n  connector_type: script\n  project_id: prj-inv\n",
                    "  script_path: d42/inventory.py\n  script_args: [\"--list\"]\n",
                    "  output_format: ansible\n  credential_ids: [cred-gh]\n  ttl_seconds: 60\n",
                    "  timeout_seconds: 600\n  config:\n    tenant: a\n",
                    "src-b:\n  name: \"B\"\n  project_id: prj-inv\n",
                    "  script_path: d42/inventory.py\n  script_args: [\"--list\"]\n",
                    "  output_format: ansible\n  ttl_seconds: 60\n  timeout_seconds: 600\n",
                    "src-c:\n  name: \"C\"\n  project_id: prj-inv\n",
                    "  script_path: d42/inventory.py\n  script_args: [\"--list\"]\n",
                    "  output_format: ansible\n  ttl_seconds: 60\n",
                ),
            ),
        ]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        assert_eq!(
            plan.connectors["d42"],
            vec!["src-a", "src-b", "src-c"],
            "one connector, every user"
        );
        assert_eq!(
            plan.files["connectors/d42.yaml"],
            "---\nconnectors:\n  - name: d42\n    type: \"script\"\n    project: \"prj-inv\"\n    \
             path: \"d42/inventory.py\"\n    script_args: [\"--list\"]\n    \
             timeout_seconds: 600\n"
        );
        assert_eq!(
            plan.files["sources/src-a.yaml"],
            "---\nsources:\n  - name: src-a\n    description: \"A\"\n    connector: \"d42\"\n    \
             credentials: [\"cred-gh\"]\n    ttl_seconds: 60\n    config:\n      tenant: \"a\"\n"
        );
        // src-c ran on the default timeout, which is not its connector's.
        assert!(plan.files["sources/src-c.yaml"].contains("timeout_seconds: 300"));
        assert!(plan.files["projects/prj-inv.yaml"].contains("credential: \"cred-gh\""));
    }

    // 0.34's ttl_overrides stay on the source, which still reads them.
    #[test]
    fn ttl_overrides_stay_on_the_source() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                concat!(
                    "src-a:\n  name: \"A\"\n  project_id: prj-inv\n  script_path: a.py\n",
                    "  ttl_seconds: 60\n  ttl_overrides:\n    hosts:\n      web01: 30\n",
                ),
            ),
        ]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        assert!(
            plan.files["sources/src-a.yaml"]
                .contains("ttl_overrides:\n      hosts:\n        web01: 30\n"),
            "{}",
            plan.files["sources/src-a.yaml"]
        );
        assert!(!plan.files.keys().any(|f| f.starts_with("hosts/")));
    }

    #[test]
    fn remote_and_static_sources_move_to_the_built_ins() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                concat!(
                    "src-edge:\n  name: \"Edge\"\n  connector_type: remote\n  project_id: prj-inv\n",
                    "  script_path: src-ssh-facts\n  ttl_seconds: 30\n",
                    "  config:\n    url: \"https://edge.example:8443\"\n",
                    "src-vars:\n  name: \"Vars\"\n  connector_type: static_inventory\n",
                    "  project_id: prj-inv\n  script_path: static/inventory.yaml\n  ttl_seconds: 60\n",
                ),
            ),
        ]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        assert!(plan.connectors.is_empty());
        let edge = &plan.files["sources/src-edge.yaml"];
        assert!(edge.contains("connector: \"remote\""), "{edge}");
        assert!(edge.contains("      source: \"src-ssh-facts\""), "{edge}");
        assert!(!edge.contains("prj-inv"), "{edge}");
        let vars = &plan.files["sources/src-vars.yaml"];
        assert!(vars.contains("connector: \"static_inventory\""), "{vars}");
        assert!(vars.contains("project: \"prj-inv\""), "{vars}");
        assert!(vars.contains("path: \"static/inventory.yaml\""), "{vars}");
    }

    #[test]
    fn a_command_keeps_its_arguments_in_its_script_line() {
        let dir = dir_with(&[(
            "sources.yaml",
            concat!(
                "src-r:\n  name: \"Report\"\n  connector_type: ssh\n",
                "  script_path: /usr/local/bin/report\n  script_args: [\"--json\"]\n",
                "  ttl_seconds: 60\n  config:\n    hosts: \"a.example\"\n    gather_mode: script\n",
            ),
        )]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        let file = plan
            .files
            .iter()
            .find(|(name, _)| name.starts_with("connectors/"))
            .map(|(_, body)| body.clone())
            .unwrap();
        let value = crate::domain::yaml::to_json(&file).unwrap();
        assert_eq!(
            value["connectors"][0]["script"],
            "/usr/local/bin/report --json"
        );
        assert!(value["connectors"][0].get("script_args").is_none());
    }

    #[test]
    fn gathered_facts_become_files_on_the_host() {
        let dir = dir_with(&[(
            "sources.yaml",
            concat!(
                "src-facts:\n  name: \"Facts\"\n  connector_type: ssh\n",
                "  script_path: gather_facts\n  ttl_seconds: 60\n  config:\n",
                "    hosts: \"a.example\"\n    gather_mode: facts\n",
                "    fact_path: /opt/facts/\n",
            ),
        )]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        // Named as before, after the 0.34 label.
        let value =
            crate::domain::yaml::to_json(&plan.files["connectors/ssh-gather-facts.yaml"]).unwrap();
        let connector = &value["connectors"][0];
        assert_eq!(connector["location"], "host");
        assert_eq!(connector["path"], "/opt/facts/*.fact");
        // The source keeps its hosts and loses what its connector says now.
        let source = crate::domain::yaml::to_json(&plan.files["sources/src-facts.yaml"]).unwrap();
        let config = &source["sources"][0]["config"];
        assert_eq!(config["hosts"], "a.example");
        assert!(config.get("gather_mode").is_none() && config.get("fact_path").is_none());
        assert!(plan.notes.iter().any(|n| n.contains("*.json")));
    }

    #[test]
    fn a_remote_script_survives_as_a_literal_block() {
        let dir = dir_with(&[(
            "sources.yaml",
            concat!(
                "src-os:\n  name: \"OS\"\n  connector_type: ssh\n  project_id: prj-x\n",
                "  script_path: |-\n    a=1\n    printf '%s' \"$a\" | tr -d '\"\\\\'\n",
                "  ttl_seconds: 60\n  config:\n    hosts: \"a.example\"\n    gather_mode: script\n",
            ),
        )]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        let file = &plan.files["connectors/ssh-script.yaml"];
        // The connector's own script now, no longer a command in path.
        assert!(
            file.contains("    script: |-\n      a=1\n      printf '%s' \"$a\" | tr -d '\"\\\\'\n"),
            "{file}"
        );
        // ...and reads back as the same text.
        let value = crate::domain::yaml::to_json(file).unwrap();
        assert_eq!(
            value["connectors"][0]["script"],
            "a=1\nprintf '%s' \"$a\" | tr -d '\"\\\\'"
        );
        assert!(
            plan.notes
                .iter()
                .any(|n| n.contains("project_id was dropped"))
        );
    }

    #[test]
    fn in_place_backs_up_the_old_files_and_loads() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                "src-a:\n  name: \"A\"\n  project_id: prj-inv\n  script_path: a.py\n  ttl_seconds: 60\n",
            ),
            ("views.yaml", "---\n"),
            (
                "api_keys.yaml",
                "key-a:\n  name: \"A\"\n  env: \"KEY_A\"\n  sources: [src-a]\n",
            ),
        ]);
        let outcome = migrate_in_place(dir.path()).unwrap().expect("migrated");
        assert!(outcome.backup_dir.join("sources.yaml").is_file());
        assert!(
            outcome.backup_dir.join("views.yaml").is_file(),
            "an empty legacy file moves too"
        );
        assert!(!dir.path().join("sources.yaml").exists());
        assert!(dir.path().join("connectors/a.yaml").is_file());
        let cfg = crate::config::load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.sources["src-a"].name, "A");
        assert_eq!(cfg.sources["src-a"].connector, "a");
        assert_eq!(
            cfg.projects["prj-inv"].credential_id.as_deref(),
            Some("cred-gh")
        );
        // Once is once: the next start finds nothing to do.
        assert!(migrate_in_place(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_conversion_that_would_not_load_moves_nothing() {
        let original = "src-a:\n  name: \"A\"\n  project_id: prj-ghost\n  script_path: a.py\n  ttl_seconds: 60\n";
        let dir = dir_with(&[("sources.yaml", original)]);
        let err = migrate_in_place(dir.path()).unwrap_err();
        assert!(err.contains("unknown project 'prj-ghost'"), "{err}");
        assert_eq!(
            fs::read_to_string(dir.path().join("sources.yaml")).unwrap(),
            original
        );
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(leftovers.len(), 2, "{leftovers:?}");
    }

    #[test]
    fn check_validates_without_writing() {
        let dir = dir_with(&[(
            "sources.yaml",
            "src-a:\n  name: \"A\"\n  project_id: prj-x\n  script_path: /opt/a.py\n  ttl_seconds: 60\n",
        )]);
        let err = check(dir.path()).unwrap_err();
        assert!(err.contains("unknown project 'prj-x'"), "{err}");
        assert!(dir.path().join("sources.yaml").is_file());
    }

    #[test]
    fn a_read_only_directory_is_converted_into_a_copy() {
        use std::os::unix::fs::PermissionsExt;
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                "src-a:\n  name: \"A\"\n  project_id: prj-inv\n  script_path: a.py\n  ttl_seconds: 60\n",
            ),
        ]);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        // root ignores permissions: nothing to observe then
        if writable(dir.path()) {
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let prepared = prepare_for_startup(dir.path());
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let StartupConfig::Converted { copy, plan } = prepared.expect("converted") else {
            panic!("expected a converted copy");
        };
        assert!(
            dir.path().join("sources.yaml").is_file(),
            "the original is untouched"
        );
        let cfg =
            crate::config::load_config(copy.path().to_str().unwrap()).expect("the copy loads");
        assert_eq!(cfg.sources["src-a"].connector, "a");
        assert_eq!(plan.connectors["a"], vec!["src-a"]);
        let path = copy.path().to_path_buf();
        drop(copy);
        assert!(!path.exists(), "the copy goes with its holder");
    }

    // Counting repeats of a base name gave the second `foo.py` the name
    // `foo-2`, which a script `foo-2.py` had as its own: one connector
    // replaced the other, and src-c ran `foo.py --x`.
    #[test]
    fn a_generated_name_never_takes_another_connectors() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                concat!(
                    "src-a:\n  name: \"A\"\n  project_id: prj-inv\n  script_path: foo.py\n  ttl_seconds: 60\n",
                    "src-b:\n  name: \"B\"\n  project_id: prj-inv\n  script_path: foo.py\n",
                    "  script_args: [\"--x\"]\n  ttl_seconds: 60\n",
                    "src-c:\n  name: \"C\"\n  project_id: prj-inv\n  script_path: foo-2.py\n  ttl_seconds: 60\n",
                ),
            ),
        ]);
        let outcome = migrate_in_place(dir.path()).unwrap().expect("migrated");
        assert_eq!(
            outcome.plan.connectors.len(),
            3,
            "{:?}",
            outcome.plan.connectors
        );
        let cfg = crate::config::load_config(dir.path().to_str().unwrap()).expect("loads");
        let runs = |id: &str| {
            let s = &cfg.sources[id];
            (s.script_path.clone(), s.script_args.clone())
        };
        assert_eq!(runs("src-a"), ("foo.py".to_string(), vec![]));
        assert_eq!(
            runs("src-b"),
            ("foo.py".to_string(), vec!["--x".to_string()])
        );
        assert_eq!(runs("src-c"), ("foo-2.py".to_string(), vec![]));
        assert_eq!(cfg.sources["src-c"].connector, "foo-2");
        assert_eq!(cfg.sources["src-b"].connector, "foo-3");
    }

    // A script in a folder called `remote` (or `static_inventory`) is not the
    // built-in connector of that name, and must not be refused as if it were.
    #[test]
    fn a_script_named_like_a_built_in_gets_a_name_of_its_own() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                concat!(
                    "src-a:\n  name: \"A\"\n  project_id: prj-inv\n  script_path: remote/inventory.py\n  ttl_seconds: 60\n",
                    "src-b:\n  name: \"B\"\n  project_id: prj-inv\n  script_path: static_inventory.py\n  ttl_seconds: 60\n",
                ),
            ),
        ]);
        let plan = check(dir.path())
            .expect("the conversion loads")
            .expect("legacy");
        let names: Vec<&String> = plan.connectors.keys().collect();
        assert_eq!(names, ["remote-2", "static_inventory-2"]);
    }

    // Number-looking text is text: 0.34 handed `1.10` to the script as
    // "1.10", and the converted files must say exactly what the old ones did.
    #[test]
    fn number_looking_values_are_written_as_they_were() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECTS),
            ("credentials.yaml", CREDENTIALS),
            (
                "sources.yaml",
                concat!(
                    "src-a:\n  name: 2024\n  project_id: prj-inv\n  script_path: a.py\n",
                    "  script_args: [\"--account\", 0x1F, 7]\n  timeout_seconds: +600\n  ttl_seconds: 60\n",
                    "  config:\n    API_VERSION: 1.10\n    ACCOUNT: 0x1F\n",
                    "    BIG: 123456789012345678901234\n    FLAG: True\n    PORT: 80\n",
                ),
            ),
            (
                "enrichers.yaml",
                concat!(
                    "enr-a:\n  name: \"E\"\n  target_id: src-a\n  project_id: prj-inv\n",
                    "  script_path: e.py\n  script_args: [\"--days\", 7, 1.10]\n",
                ),
            ),
        ]);
        let plan = plan(dir.path()).unwrap().expect("legacy");
        let source = &plan.files["sources/src-a.yaml"];
        for line in [
            "    description: 2024\n",
            "      API_VERSION: 1.10\n",
            "      ACCOUNT: 0x1F\n",
            "      BIG: 123456789012345678901234\n",
            "      FLAG: True\n",
            "      PORT: 80\n",
        ] {
            assert!(source.contains(line), "{line:?} in:\n{source}");
        }
        let connector = &plan.files["connectors/a.yaml"];
        assert!(
            connector.contains("script_args: [\"--account\", \"0x1F\", \"7\"]\n"),
            "{connector}"
        );
        assert!(connector.contains("timeout_seconds: 600\n"), "{connector}");
        let enricher = &plan.files["enrichers/enr-a.yaml"];
        assert!(
            enricher.contains("script_args: [\"--days\", 7, 1.10]\n"),
            "{enricher}"
        );

        migrate_in_place(dir.path()).unwrap().expect("migrated");
        let cfg = crate::config::load_config(dir.path().to_str().unwrap()).expect("loads");
        let source = &cfg.sources["src-a"];
        assert_eq!(source.name, "2024");
        assert_eq!(source.config["API_VERSION"], "1.10");
        assert_eq!(source.config["ACCOUNT"], "0x1F");
        assert_eq!(source.config["BIG"], "123456789012345678901234");
        assert_eq!(source.config["FLAG"], "True");
        assert_eq!(source.script_args, ["--account", "0x1F", "7"]);
        assert_eq!(source.timeout_seconds, 600);
        assert_eq!(cfg.enrichers["enr-a"].script_args, ["--days", "7", "1.10"]);
    }

    // Copies of a read-only directory: one still in use is locked and kept,
    // one whose process is gone is removed, and so is an old unlocked one.
    #[test]
    fn stale_converted_copies_are_swept() {
        let temp = tempfile::tempdir().unwrap();
        let live = new_copy(temp.path()).unwrap();
        // What a killed process leaves: the folder, its lock released.
        let unlocked = temp.path().join(format!("{COPY_PREFIX}1-dead"));
        fs::create_dir(&unlocked).unwrap();
        fs::File::create(unlocked.join(COPY_LOCK)).unwrap();
        let fresh = temp.path().join(format!("{COPY_PREFIX}2-starting"));
        fs::create_dir(&fresh).unwrap();
        let unrelated = temp.path().join("something-else");
        fs::create_dir(&unrelated).unwrap();

        sweep_stale_copies(temp.path());

        assert!(live.path().is_dir(), "a copy in use stays");
        assert!(!unlocked.exists(), "an unlocked copy is removed");
        assert!(fresh.exists(), "a folder that may be mid-creation stays");
        assert!(unrelated.exists());
    }

    #[test]
    fn connector_names() {
        let key = |kind: &str, path: &str| ConnectorKey {
            kind: kind.into(),
            project: None,
            location: None,
            inline: false,
            script_path: path.into(),
            script_args: vec![],
            host_args: vec![],
            hint: path.into(),
        };
        assert_eq!(
            connector_base_name(&key("script", "dynamic/d42/inventory.py")),
            "d42"
        );
        assert_eq!(
            connector_base_name(&key("script", "dynamic/taxonomy/inventory_taxonomy_cat.sh")),
            "taxonomy"
        );
        assert_eq!(
            connector_base_name(&key("script", "dynamic/unified-api/src-patching.py")),
            "src-patching"
        );
        assert_eq!(
            connector_base_name(&key("ssh", "gather_facts")),
            "ssh-gather-facts"
        );
        assert_eq!(connector_base_name(&key("ssh", "a=1\nb")), "ssh-script");
    }
}
