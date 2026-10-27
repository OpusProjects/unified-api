use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use utoipa::ToSchema;

use super::connector::{self, Connector, ConnectorKind};
use super::dataset::Dataset;
use super::sync_mode::SyncMode;

#[derive(Debug, Deserialize, Serialize, ToSchema, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorType {
    #[default]
    Script,
    Ssh,
    // Ansible static YAML inventory read from disk: the built-in
    // `static_inventory` connector (the source's `path`, inside its
    // `project` checkout)
    StaticInventory,
    // Federation: fetch another unified-api instance's cached dataset over
    // HTTP — the built-in `remote` connector (config.url = the remote base
    // URL, config.source = the source id ON THE REMOTE instance). Ages are
    // propagated so freshness stays truthful.
    Remote,
}

// A source as it RUNS: the per-instance settings from its item file, with
// its connector's settings resolved into it at load (see SourceDef::resolve).
//
// Resolving once, at load, is what lets everything downstream — sync, the
// scheduler, the connectors, the views — keep reading one flat struct, and it
// means a reload that changes a connector shows up as a change to every source
// using it, which is exactly the set of sources whose next sync behaves
// differently.
//
// Configuration files are NEVER read as this: their shape is SourceDef.
// `Deserialize` is here for code that builds a resolved source from a compact
// description of it — tests, mostly — which is why most fields have a default
// a fixture can leave out. (`deny_unknown_fields` still applies: a fixture
// with a typo fails like a file would.)
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    // The display label: the item's description, or its identity.
    pub name: String,

    // The connector this source names (`connector:` in its file) — an item
    // name, or one of the built-ins in domain::connector::BUILT_IN.
    #[serde(default)]
    pub connector: String,

    // ---- resolved from the connector -------------------------------------
    #[serde(default)]
    pub connector_type: ConnectorType,

    // ssh only: where script_path is (domain::connector::Location). None for
    // an ssh connector that writes its script inline (script_path is then
    // that script), and for every other connector.
    #[serde(default)]
    pub ssh_location: Option<crate::domain::connector::Location>,

    // The checkout a relative script_path (or static inventory path) resolves
    // in. None for remote, and for ssh unless its script is in a project.
    #[serde(default)]
    pub project_id: Option<String>,

    // What the connector adapter is handed: the script file for script; for
    // ssh the connector's path (a script here, or files on the hosts) or its
    // inline script itself; the inventory file for static_inventory; and the
    // source id ON THE REMOTE instance for remote.
    pub script_path: String,

    // CLI arguments passed to the script, e.g. ["--list"] for scripts that
    // implement the Ansible dynamic inventory interface. For ssh: the
    // streamed script's arguments, or every executable a host path matches.
    #[serde(default)]
    pub script_args: Vec<String>,

    // CLI arguments for a sync that names hosts, REPLACING script_args for
    // that run. `{target}` is substituted with the requested hostnames,
    // comma-joined -- the same value the reserved `target` config key carries.
    //
    // Empty (the default) keeps the old behaviour: a host-scoped sync calls the
    // script exactly like a full one and the requested hosts are picked out of
    // whatever comes back. That is not as wasteful as it sounds -- Ansible's
    // own `--host` was usually implemented as "fetch everything, then filter",
    // since `_meta.hostvars` made per-host calls vestigial -- but a script that
    // CAN fetch one host should be asked for one host.
    //
    // No CLI convention is imposed here: `["--host", "{target}"]` reproduces
    // Ansible's contract exactly, and a script that can do better (a host with
    // its groups, which `--host` has no room for) states its own flag.
    #[serde(default)]
    pub host_args: Vec<String>,

    // Maximum seconds a sync may run before it is aborted — protects the
    // scheduler and API from a hung connector script. The source's own
    // timeout_seconds when it sets one, else its connector's (default 300).
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,

    // ---- the source's own settings ---------------------------------------

    // Whether a host-scoped sync also takes the vars of the groups it places
    // its hosts in. Off by default: a granular call answers about a HOST, and
    // letting it redefine what a group means for every other member is a bigger
    // statement than the sync was asked to make. Worth turning on for a script
    // whose granular mode deliberately describes those groups in full.
    #[serde(default)]
    pub host_sync_updates_group_vars: bool,

    #[serde(default)]
    pub sync_mode: SyncMode,

    // Vec<String> = list of credential ids that the connector needs
    // (`credentials:` in the file)
    #[serde(default)]
    pub credential_ids: Vec<String>,

    // Cron schedule as the alternative to sync_interval_seconds — "sync at
    // 02:30" instead of "sync every N seconds". Standard 5-field cron with an
    // optional leading seconds field, evaluated in UTC; validated at startup
    // and mutually exclusive with a non-zero sync_interval_seconds. Cron
    // sources get no startup jitter (the times were chosen deliberately) but
    // back off on failure exactly like interval sources, by letting
    // occurrences pass.
    #[serde(default)]
    pub schedule: Option<String>,

    // Automatic sync interval in seconds (simple alternative to cron)
    // If None or 0, no automatic sync is performed
    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    // TTL in seconds for this source's cache
    pub ttl_seconds: u64,

    // TTL overrides by group or by host
    #[serde(default)]
    pub ttl_overrides: TtlOverrides,

    // SSH sources only: take the host list from another source's cached
    // dataset instead of a static `config.hosts` — the way to chain
    // "inventory source says WHAT exists, SSH says HOW it is doing".
    #[serde(default)]
    pub hosts_from_source: Option<HostsFromSource>,

    // Whether a READ of this source may trigger a sync of the hosts it asks
    // for (`GET /dataset?host=X&refresh=true`). Off by default: a read that
    // can cause SSH into a datacenter is a capability, not a convenience, and
    // it should be granted per source rather than assumed.
    //
    // Turning it on makes `ttl_seconds` load bearing — it stops being purely
    // informational and becomes the threshold at which a read is willing to pay
    // for a gather. A source with a TTL set loosely at some point in the past
    // deserves a look before this is enabled.
    #[serde(default)]
    pub allow_on_demand_refresh: bool,

    // What this source claims to own, stated explicitly for advertising over
    // `GET /sources/{id}/scope`. Optional: an SSH source with
    // hosts_from_source already advertises its match_pattern, and this block
    // overrides that derivation for sources where the pattern does not tell
    // the story (or does not exist). Must name at least one group or host —
    // an empty explicit claim is refused at startup, because "explicitly
    // nothing" and "explicitly everything" are one typo apart.
    #[serde(default)]
    pub advertise_scope: Option<AdvertisedScope>,

    // Free config for the connector (api_url, filters, etc.)
    #[serde(default)]
    pub config: HashMap<String, String>,
}

// A source as written in its item file (`sources:`). Only what is particular
// to THIS instance of a system: which connector, which credentials, which
// `config`, when to sync and how long data stays fresh. How to talk to the
// system lives on the connector it names.
//
// `name` is the display label (the file's `description`, falling back to the
// identity — see config::take); the identity is the map key.
//
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
// That is also what turns a 0.34 source pasted into a 0.35 file into an error
// naming `connector_type` or `script_path`, rather than a source that half
// works.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SourceDef {
    pub name: String,

    // A connector item's name, or a built-in: `remote`, `static_inventory`.
    pub connector: String,

    #[serde(default)]
    pub credentials: Vec<String>,

    // static_inventory only: the project whose checkout holds the inventory,
    // and the file inside it. Every other connector carries its own project.
    #[serde(default)]
    pub project: Option<String>,

    #[serde(default)]
    pub path: Option<String>,

    // Overrides the connector's timeout for this source only.
    #[serde(default)]
    pub timeout_seconds: Option<u64>,

    #[serde(default)]
    pub host_sync_updates_group_vars: bool,

    #[serde(default)]
    pub sync_mode: SyncMode,

    #[serde(default)]
    pub schedule: Option<String>,

    #[serde(default)]
    pub sync_interval_seconds: Option<u64>,

    pub ttl_seconds: u64,

    #[serde(default)]
    pub ttl_overrides: TtlOverrides,

    #[serde(default)]
    pub hosts_from_source: Option<HostsFromSource>,

    #[serde(default)]
    pub allow_on_demand_refresh: bool,

    #[serde(default)]
    pub advertise_scope: Option<AdvertisedScope>,

    #[serde(default)]
    pub config: HashMap<String, String>,
}

impl SourceDef {
    // The runtime source: this item plus its connector. Every problem that
    // can be known from the two of them is returned, so one load names all
    // of them; references to other items (projects, credentials, sources)
    // are checked afterwards by config validation, against the resolved set.
    pub fn resolve(
        &self,
        id: &str,
        connectors: &HashMap<String, Connector>,
    ) -> Result<Source, Vec<String>> {
        let mut errors = Vec::new();
        let default_timeout = crate::domain::default_timeout_seconds();

        // (connector_type, project, script_path, script_args, host_args,
        // connector timeout)
        let resolved = match self.connector.as_str() {
            connector::REMOTE => {
                for key in ["url", "source"] {
                    if !self.config.contains_key(key) {
                        errors.push(format!(
                            "Source '{id}' uses the remote connector but has no '{key}' in \
                             config — set config.url to the other instance and config.source \
                             to the source id there"
                        ));
                    }
                }
                self.refuse_location(id, "remote", &mut errors);
                let remote_id = self.config.get("source").cloned().unwrap_or_default();
                Some((
                    ConnectorType::Remote,
                    None,
                    remote_id,
                    Vec::new(),
                    Vec::new(),
                    default_timeout,
                ))
            }
            connector::STATIC_INVENTORY => {
                if self.project.is_none() {
                    errors.push(format!(
                        "Source '{id}' uses the static_inventory connector but sets no \
                         project — name the project whose checkout holds the inventory"
                    ));
                }
                let path = match &self.path {
                    Some(path) if !path.trim().is_empty() => path.clone(),
                    _ => {
                        errors.push(format!(
                            "Source '{id}' uses the static_inventory connector but sets no \
                             path — the inventory file inside the project checkout"
                        ));
                        String::new()
                    }
                };
                Some((
                    ConnectorType::StaticInventory,
                    self.project.clone(),
                    path,
                    Vec::new(),
                    Vec::new(),
                    default_timeout,
                ))
            }
            name => match connectors.get(name) {
                None => {
                    errors.push(format!(
                        "Source '{id}' references unknown connector '{name}'"
                    ));
                    None
                }
                Some(c) => {
                    self.refuse_location(id, name, &mut errors);
                    let kind = match c.kind {
                        ConnectorKind::Script => ConnectorType::Script,
                        ConnectorKind::Ssh => ConnectorType::Ssh,
                    };
                    Some((
                        kind,
                        c.project.clone(),
                        // What the adapter is handed: a file, or the
                        // script written in the item.
                        c.script.clone().unwrap_or_else(|| c.path.clone()),
                        c.script_args.clone(),
                        c.host_args.clone(),
                        c.timeout_seconds,
                    ))
                }
            },
        };

        let ssh = connectors
            .get(&self.connector)
            .filter(|c| c.kind == ConnectorKind::Ssh);
        let ssh_location = ssh.and_then(|c| c.ssh_location());
        // What runs is the connector's now; 0.35 asked each source. Refused
        // rather than ignored, so a source still saying it is looked at.
        if ssh.is_some() {
            if self.config.contains_key(connector::SSH_LOCATION_CONFIG_KEY) {
                errors.push(format!(
                    "Source '{id}' sets config.{}, a key unified-api sets itself for its \
                     ssh connector — remove it: the connector's location decides",
                    connector::SSH_LOCATION_CONFIG_KEY
                ));
            }
            for key in ["gather_mode", "fact_path"] {
                if self.config.contains_key(key) {
                    errors.push(format!(
                        "Source '{id}' sets config.{key}, which is no longer read — its \
                         connector '{}' decides what runs (its path, or its script): remove it",
                        self.connector
                    ));
                }
            }
        }

        let Some((
            connector_type,
            project_id,
            script_path,
            script_args,
            host_args,
            connector_timeout,
        )) = resolved
        else {
            return Err(errors);
        };
        if !errors.is_empty() {
            return Err(errors);
        }

        Ok(Source {
            name: self.name.clone(),
            connector: self.connector.clone(),
            connector_type,
            ssh_location,
            project_id,
            script_path,
            script_args,
            host_args,
            timeout_seconds: self.timeout_seconds.unwrap_or(connector_timeout),
            host_sync_updates_group_vars: self.host_sync_updates_group_vars,
            sync_mode: self.sync_mode.clone(),
            credential_ids: self.credentials.clone(),
            schedule: self.schedule.clone(),
            sync_interval_seconds: self.sync_interval_seconds,
            ttl_seconds: self.ttl_seconds,
            ttl_overrides: self.ttl_overrides.clone(),
            hosts_from_source: self.hosts_from_source.clone(),
            allow_on_demand_refresh: self.allow_on_demand_refresh,
            advertise_scope: self.advertise_scope.clone(),
            config: self.config.clone(),
        })
    }

    // `project` and `path` are the static_inventory connector's: on any other
    // source they would look like they select a script, and they do not.
    fn refuse_location(&self, id: &str, connector: &str, errors: &mut Vec<String>) {
        for (key, set) in [
            ("project", self.project.is_some()),
            ("path", self.path.is_some()),
        ] {
            if set {
                errors.push(format!(
                    "Source '{id}' sets {key}, which only a static_inventory source uses — \
                     the '{connector}' connector decides what runs"
                ));
            }
        }
    }
}

// An explicit ownership claim for scope advertising — the same group/host
// vocabulary a view's Ownership uses, minus the resolving source (that is the
// CONSUMER's business: the central decides which of its inventories the
// groups resolve against).
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedScope {
    #[serde(default)]
    pub groups: Vec<String>,

    #[serde(default)]
    pub hosts: Vec<String>,
}

// What a source answers when asked "what do you own?" — derived from config,
// never from cache contents (the same reasoning as view ownership being
// DECLARED: a host absent from today's gather is still this source's
// responsibility).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeClaim {
    pub groups: Vec<String>,
    pub hosts: Vec<String>,
    // True when the claim is "everything my inventory dependency knows" — an
    // SSH source whose hosts_from_source has an empty match_pattern. Stated
    // as a flag rather than silently returning empty lists, because a
    // consumer must be able to tell "claims everything" from "claims nothing".
    pub catch_all: bool,
}

// Last-known advertised scopes of REMOTE member sources, keyed by the local
// source id, written by every sync of a remote source that answers /scope.
// In-process state like the health registries — and like them it lives
// outside the cache on purpose: an edge that stops answering keeps its
// last-known scope, because stale routing beats no routing (the same stance
// the cache takes about stale data). Not persisted: after a restart the
// scope is re-learned by the first sync, and view members fall back to their
// declared groups/hosts (or claim nothing) until then.
#[derive(Debug, Default)]
pub struct AdvertisedScopeRegistry {
    inner: std::sync::Mutex<HashMap<String, ScopeClaim>>,
}

impl AdvertisedScopeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, source_id: &str, claim: ScopeClaim) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(source_id.to_string(), claim);
    }

    pub fn get(&self, source_id: &str) -> Option<ScopeClaim> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(source_id)
            .cloned()
    }
}

impl Source {
    // The scope this source advertises, in precedence order: the explicit
    // advertise_scope block, else the hosts_from_source match_pattern it
    // already gathers by, else nothing (None — an ordinary script source
    // makes no ownership claim unless its operator writes one).
    pub fn advertised_scope(&self) -> Option<ScopeClaim> {
        if let Some(explicit) = &self.advertise_scope {
            return Some(ScopeClaim {
                groups: explicit.groups.clone(),
                hosts: explicit.hosts.clone(),
                catch_all: false,
            });
        }
        self.hosts_from_source.as_ref().map(|hfs| ScopeClaim {
            groups: hfs.match_pattern.groups.clone(),
            hosts: hfs.match_pattern.hosts.clone(),
            catch_all: hfs.match_pattern.is_empty(),
        })
    }
}

// Dynamic host list: which source to read, which slice of it, and how to
// connect to each host.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostsFromSource {
    // Source id whose cached dataset provides the hosts
    pub source: String,

    // Which hosts to take. Absent/empty = every host in the dataset.
    #[serde(default)]
    pub match_pattern: MatchPattern,

    #[serde(default)]
    pub connect_via: ConnectVia,
}

// The UNION of: members of the listed groups + the individually listed
// hosts. Names are matched exactly.
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MatchPattern {
    #[serde(default)]
    pub groups: Vec<String>,

    #[serde(default)]
    pub hosts: Vec<String>,
}

impl MatchPattern {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.hosts.is_empty()
    }
}

// Which address the SSH connector should dial for each host. The *_then_*
// variants produce a fallback: candidates are tried in order and a
// CONNECTION failure (timeout, refused, DNS) moves to the next one — an
// authentication failure does not (it's the same server saying no).
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectVia {
    // The inventory hostname itself (relies on DNS)
    #[default]
    Hostname,
    // The host's `ansible_host` variable; hosts without it are skipped
    AnsibleHost,
    AnsibleHostThenHostname,
    HostnameThenAnsibleHost,
}

// One resolved host for the SSH connector: the inventory name (results are
// always keyed by it, whatever address ends up connecting) plus the ordered
// connection candidates. Serialize/Deserialize because this travels to the
// connector as JSON inside its config map (`hosts_spec`).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct HostSpec {
    pub name: String,
    pub addresses: Vec<String>,
}

impl HostsFromSource {
    // Pure resolution against a dataset: pick the hosts the pattern selects
    // and compute each one's connection candidates. Returns warnings (as
    // data, the caller logs) for pattern entries that match nothing and for
    // hosts skipped because a required variable is missing.
    pub fn resolve(&self, dataset: &Dataset) -> (Vec<HostSpec>, Vec<String>) {
        let mut warnings = Vec::new();

        let mut selected: Vec<&String> = if self.match_pattern.is_empty() {
            dataset.hostvars.keys().collect()
        } else {
            let mut picked: Vec<&String> = Vec::new();
            for group in &self.match_pattern.groups {
                match dataset.groups.get(group) {
                    Some(g) => {
                        for host in &g.hosts {
                            // group members may lack a hostvars entry; they
                            // are still hosts we can try to reach
                            picked.push(host);
                        }
                    }
                    None => warnings.push(format!(
                        "match_pattern group '{}' does not exist in source '{}'",
                        group, self.source
                    )),
                }
            }
            for host in &self.match_pattern.hosts {
                if dataset.hostvars.contains_key(host)
                    || dataset.groups.values().any(|g| g.hosts.contains(host))
                {
                    picked.push(host);
                } else {
                    warnings.push(format!(
                        "match_pattern host '{}' does not exist in source '{}'",
                        host, self.source
                    ));
                }
            }
            picked
        };
        selected.sort();
        selected.dedup();

        let mut specs = Vec::new();
        for host in selected {
            let ansible_host = dataset
                .hostvars
                .get(host)
                .and_then(|vars| vars.get("ansible_host"))
                .and_then(|v| v.as_str())
                .map(str::to_string);

            let addresses: Vec<String> = match (self.connect_via, ansible_host) {
                (ConnectVia::Hostname, _) => vec![host.clone()],
                (ConnectVia::AnsibleHost, Some(addr)) => vec![addr],
                (ConnectVia::AnsibleHost, None) => {
                    warnings.push(format!(
                        "host '{}' has no ansible_host variable and connect_via is ansible_host — skipped",
                        host
                    ));
                    continue;
                }
                (ConnectVia::AnsibleHostThenHostname, Some(addr)) => vec![addr, host.clone()],
                (ConnectVia::AnsibleHostThenHostname, None) => vec![host.clone()],
                (ConnectVia::HostnameThenAnsibleHost, Some(addr)) => vec![host.clone(), addr],
                (ConnectVia::HostnameThenAnsibleHost, None) => vec![host.clone()],
            };

            // The same address twice in a row (ansible_host == hostname) is
            // a pointless double timeout
            let mut addresses = addresses;
            addresses.dedup();

            specs.push(HostSpec {
                name: host.clone(),
                addresses,
            });
        }

        (specs, warnings)
    }
}

// Which TTL applies to which host. Two callers need this — the status endpoint
// reporting freshness, and the on-demand refresh deciding whether a host is
// stale enough to be worth gathering — and they must not be able to disagree:
// a host reported fresh by one and refreshed by the other is a bug in both
// directions at once.
impl Source {
    // Resolve the GROUP-level overrides into a per-host map, once.
    //
    // One pass over the override list, not a scan of every group for every
    // host: doing it the other way made a full status quadratic on a large
    // source. When a host sits in several groups that each carry an override
    // the winner is whichever is seen first, which is arbitrary but stable
    // within a request.
    pub fn group_ttl_by_host<'a>(&self, dataset: &'a Dataset) -> HashMap<&'a str, u64> {
        let mut by_host: HashMap<&str, u64> = HashMap::new();
        for (group_name, ttl) in &self.ttl_overrides.groups {
            if let Some(group) = dataset.groups.get(group_name) {
                for hostname in &group.hosts {
                    by_host.entry(hostname.as_str()).or_insert(*ttl);
                }
            }
        }
        by_host
    }

    // The TTL for one host: its own override wins, then a group's, then the
    // fallback the caller passes (the cache entry's TTL).
    pub fn effective_ttl(
        &self,
        hostname: &str,
        group_ttls: &HashMap<&str, u64>,
        fallback: u64,
    ) -> u64 {
        self.ttl_overrides
            .hosts
            .get(hostname)
            .copied()
            .or_else(|| group_ttls.get(hostname).copied())
            .unwrap_or(fallback)
    }
}

// TTL Overrides: you can give different TTLs to specific groups or hosts
#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TtlOverrides {
    // HashMap<String, u64> = dict[str, int] in Python
    // ex: {"production": 900} → the "production" group refreshes every 15 min
    #[serde(default)]
    pub groups: HashMap<String, u64>,

    // ex: {"critical-db01": 300} → this host refreshes every 5 min
    #[serde(default)]
    pub hosts: HashMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dataset::Group;

    fn dataset() -> Dataset {
        serde_json::from_value(serde_json::json!({
            "hostvars": {
                "web01.example.com": {"ansible_host": "10.0.0.1"},
                "web02.example.com": {},
                "db01.example.com": {"ansible_host": "10.0.0.9"}
            },
            "groups": {
                "web": {"hosts": ["web01.example.com", "web02.example.com"]},
                "db": {"hosts": ["db01.example.com"]}
            }
        }))
        .unwrap()
    }

    fn hfs(yaml: &str) -> HostsFromSource {
        serde_saphyr::from_str(yaml).unwrap()
    }

    // A source item resolved against two connectors: `facts` (ssh) and `inv`
    // (a script). `body` is the rest of the item, after its connector.
    fn resolved(connector: &str, body: &str) -> Result<Source, Vec<String>> {
        let connectors: HashMap<String, Connector> = [
            ("facts", "name: facts\ntype: ssh\npath: gather_facts\n"),
            (
                "inv",
                "name: inv\nproject: prj\npath: inv.py\nscript_args: [\"--list\"]\ntimeout_seconds: 120\n",
            ),
        ]
        .into_iter()
        .map(|(id, yaml)| (id.to_string(), serde_saphyr::from_str(yaml).unwrap()))
        .collect();
        let def: SourceDef = serde_saphyr::from_str(&format!(
            "name: s\nconnector: {connector}\nttl_seconds: 60\n{body}"
        ))
        .expect("source fixture");
        def.resolve("src-s", &connectors)
    }

    fn source(connector: &str, body: &str) -> Source {
        resolved(connector, body).expect("resolves")
    }

    #[test]
    fn a_source_takes_how_from_its_connector_and_which_from_itself() {
        let s = source("inv", "credentials: [cred-a]\nconfig:\n  tenant: a\n");
        assert_eq!(s.connector, "inv");
        assert_eq!(s.connector_type, ConnectorType::Script);
        assert_eq!(s.project_id.as_deref(), Some("prj"));
        assert_eq!(s.script_path, "inv.py");
        assert_eq!(s.script_args, vec!["--list"]);
        assert_eq!(s.timeout_seconds, 120, "the connector's timeout");
        assert_eq!(s.credential_ids, vec!["cred-a"]);
        assert_eq!(s.config["tenant"], "a");

        let s = source("inv", "timeout_seconds: 900\n");
        assert_eq!(
            s.timeout_seconds, 900,
            "a source may override its connector"
        );
    }

    #[test]
    fn the_built_in_connectors_take_their_location_from_the_source() {
        let s = source(
            "remote",
            "config:\n  url: \"https://edge.example\"\n  source: src-ssh-facts\n",
        );
        assert_eq!(s.connector_type, ConnectorType::Remote);
        assert_eq!(s.script_path, "src-ssh-facts");
        assert_eq!(s.project_id, None);

        let s = source(
            "static_inventory",
            "project: prj\npath: static/inventory.yaml\n",
        );
        assert_eq!(s.connector_type, ConnectorType::StaticInventory);
        assert_eq!(s.script_path, "static/inventory.yaml");
        assert_eq!(s.project_id.as_deref(), Some("prj"));
    }

    #[test]
    fn what_a_connector_needs_from_its_source_is_named() {
        let errors = resolved("remote", "").unwrap_err().join("\n");
        assert!(
            errors.contains("no 'url'") && errors.contains("no 'source'"),
            "{errors}"
        );

        let errors = resolved("static_inventory", "").unwrap_err().join("\n");
        assert!(
            errors.contains("no project") && errors.contains("no path"),
            "{errors}"
        );

        let errors = resolved("inv", "project: prj\npath: x.yaml\n")
            .unwrap_err()
            .join("\n");
        assert!(
            errors.contains("sets project") && errors.contains("sets path"),
            "{errors}"
        );

        let errors = resolved("ghost", "").unwrap_err().join("\n");
        assert!(
            errors.contains("Source 'src-s' references unknown connector 'ghost'"),
            "{errors}"
        );
    }

    #[test]
    fn a_0_34_source_fails_naming_the_keys_that_moved() {
        let err = serde_saphyr::from_str::<SourceDef>(
            "name: s\nconnector_type: script\nproject_id: p\nscript_path: x\nttl_seconds: 60\n",
        )
        .expect_err("legacy keys are unknown keys")
        .to_string();
        assert!(err.contains("connector_type"), "{err}");
    }

    #[test]
    fn empty_pattern_selects_every_host() {
        let (specs, warnings) = hfs("source: src-a\n").resolve(&dataset());
        assert!(warnings.is_empty());
        assert_eq!(specs.len(), 3);
        // default connect_via: the hostname itself
        assert_eq!(specs[0].addresses, vec![specs[0].name.clone()]);
    }

    #[test]
    fn pattern_is_the_union_of_groups_and_hosts() {
        let (specs, warnings) = hfs(
            "source: src-a\nmatch_pattern:\n  groups: [\"web\"]\n  hosts: [\"db01.example.com\"]\n",
        )
        .resolve(&dataset());
        assert!(warnings.is_empty());
        let names: Vec<&str> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["db01.example.com", "web01.example.com", "web02.example.com"]
        );
    }

    #[test]
    fn unknown_group_and_host_warn_but_do_not_fail() {
        let (specs, warnings) = hfs(
            "source: src-a\nmatch_pattern:\n  groups: [\"ghosts\"]\n  hosts: [\"nope.example.com\"]\n",
        )
        .resolve(&dataset());
        assert!(specs.is_empty());
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("ghosts"));
        assert!(warnings[1].contains("nope.example.com"));
    }

    #[test]
    fn ansible_host_strategy_uses_the_var_and_skips_hosts_without_it() {
        let (specs, warnings) =
            hfs("source: src-a\nconnect_via: ansible_host\n").resolve(&dataset());
        // web02 has no ansible_host → skipped with a warning
        assert_eq!(specs.len(), 2);
        assert!(warnings.iter().any(|w| w.contains("web02.example.com")));
        let web01 = specs
            .iter()
            .find(|s| s.name == "web01.example.com")
            .unwrap();
        assert_eq!(web01.addresses, vec!["10.0.0.1".to_string()]);
    }

    #[test]
    fn fallback_strategy_produces_ordered_candidates() {
        let (specs, _) =
            hfs("source: src-a\nconnect_via: ansible_host_then_hostname\n").resolve(&dataset());
        let web01 = specs
            .iter()
            .find(|s| s.name == "web01.example.com")
            .unwrap();
        assert_eq!(
            web01.addresses,
            vec!["10.0.0.1".to_string(), "web01.example.com".to_string()]
        );
        // no ansible_host → single candidate, no warning
        let web02 = specs
            .iter()
            .find(|s| s.name == "web02.example.com")
            .unwrap();
        assert_eq!(web02.addresses, vec!["web02.example.com".to_string()]);
    }

    #[test]
    fn group_members_without_hostvars_are_still_selected() {
        let mut ds = dataset();
        ds.groups.insert(
            "extra".to_string(),
            Group {
                hosts: vec!["bare.example.com".to_string()],
                children: vec![],
                vars: None,
            },
        );
        let (specs, warnings) =
            hfs("source: src-a\nmatch_pattern:\n  groups: [\"extra\"]\n").resolve(&ds);
        assert!(warnings.is_empty());
        assert_eq!(specs[0].name, "bare.example.com");
        assert_eq!(specs[0].addresses, vec!["bare.example.com".to_string()]);
    }

    #[test]
    fn an_explicit_advertise_scope_beats_the_derived_pattern() {
        let source = source(
            "facts",
            concat!(
                "hosts_from_source:\n  source: \"src-inv\"\n  match_pattern:\n    groups: [\"derived\"]\n",
                "advertise_scope:\n  groups: [\"explicit\"]\n",
            ),
        );

        let claim = source.advertised_scope().expect("declared");
        assert_eq!(claim.groups, vec!["explicit"]);
        assert!(!claim.catch_all);
    }

    #[test]
    fn a_hosts_from_source_pattern_is_the_advertised_scope() {
        let source = source(
            "facts",
            concat!(
                "hosts_from_source:\n  source: \"src-inv\"\n  match_pattern:\n",
                "    groups: [\"datacenter_dc2\"]\n    hosts: [\"appliance.dc2.example\"]\n",
            ),
        );

        let claim = source.advertised_scope().expect("declared");
        assert_eq!(claim.groups, vec!["datacenter_dc2"]);
        assert_eq!(claim.hosts, vec!["appliance.dc2.example"]);
        assert!(!claim.catch_all);
    }

    #[test]
    fn an_empty_pattern_advertises_a_spelled_out_catch_all() {
        let source = source("facts", "hosts_from_source:\n  source: \"src-inv\"\n");

        let claim = source.advertised_scope().expect("declared");
        assert!(claim.groups.is_empty() && claim.hosts.is_empty());
        assert!(
            claim.catch_all,
            "claims-everything must be a flag, never bare empty lists"
        );
    }

    #[test]
    fn an_ordinary_source_makes_no_claim() {
        let source = source("inv", "");
        assert!(source.advertised_scope().is_none());
    }
}
