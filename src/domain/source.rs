use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::dataset::Dataset;
use super::sync_mode::SyncMode;

#[derive(Debug, Deserialize, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorType {
    #[default]
    Script,
    Ssh,
    // Ansible static YAML inventory read from disk (script_path = path to
    // the inventory file, typically inside a git project checkout)
    StaticInventory,
    // Federation: fetch another unified-api instance's cached dataset over
    // HTTP (script_path = the source id ON THE REMOTE instance; config.url =
    // the remote base URL). Ages are propagated so freshness stays truthful.
    Remote,
}

// What the connector script prints on stdout.
// `Copy` because it's a tiny enum passed around by value everywhere.
#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    // The Dataset shape: {"hostvars": {...}, "groups": {...}}
    #[default]
    Native,
    // Standard Ansible dynamic inventory JSON: hostvars under _meta.hostvars,
    // groups as top-level keys. Converted to a Dataset on the fly, so existing
    // inventory scripts (--list) work unmodified. See Dataset::from_ansible_inventory.
    Ansible,
}

// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub project_id: String,
    pub script_path: String,

    // CLI arguments passed to the script, e.g. ["--list"] for scripts that
    // implement the Ansible dynamic inventory interface. For SSH sources in
    // `script` gather mode they are appended to the remote command.
    #[serde(default)]
    pub script_args: Vec<String>,

    #[serde(default)]
    pub connector_type: ConnectorType,

    // Format of the script's stdout (script connector only)
    #[serde(default)]
    pub output_format: OutputFormat,

    // Maximum seconds a sync may run before it is aborted — protects the
    // scheduler and API from a hung connector script (default 300)
    #[serde(default = "crate::domain::default_timeout_seconds")]
    pub timeout_seconds: u64,

    #[serde(default)]
    pub sync_mode: SyncMode,

    // Vec<String> = list of credential IDs that the connector needs
    #[serde(default)]
    pub credential_ids: Vec<String>,

    // Cron schedule as the alternative to sync_interval_seconds — "sync at
    // 02:30" instead of "sync every N seconds". Standard 5-field cron with an
    // optional leading seconds field, evaluated in UTC; validated at startup
    // and mutually exclusive with a non-zero sync_interval_seconds. Cron
    // sources get no startup jitter (the times were chosen deliberately) but
    // back off on failure exactly like interval sources, by letting
    // occurrences pass.
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

// An explicit ownership claim for scope advertising — the same group/host
// vocabulary a view's Ownership uses, minus the resolving source (that is the
// CONSUMER's business: the central decides which of its inventories the
// groups resolve against).
#[derive(Debug, Deserialize, Clone, Default)]
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
#[derive(Debug, Deserialize, Clone)]
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
#[derive(Debug, Deserialize, Clone, Default)]
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
#[derive(Debug, Deserialize, Clone, Default)]
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
        serde_yaml_ng::from_str(yaml).unwrap()
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
        let source: Source = serde_yaml_ng::from_str(concat!(
            "name: s\nproject_id: p\nscript_path: x\nttl_seconds: 60\n",
            "connector_type: \"ssh\"\n",
            "hosts_from_source:\n  source: \"src-inv\"\n  match_pattern:\n    groups: [\"derived\"]\n",
            "advertise_scope:\n  groups: [\"explicit\"]\n",
        ))
        .expect("source fixture");

        let claim = source.advertised_scope().expect("declared");
        assert_eq!(claim.groups, vec!["explicit"]);
        assert!(!claim.catch_all);
    }

    #[test]
    fn a_hosts_from_source_pattern_is_the_advertised_scope() {
        let source: Source = serde_yaml_ng::from_str(concat!(
            "name: s\nproject_id: p\nscript_path: x\nttl_seconds: 60\n",
            "connector_type: \"ssh\"\n",
            "hosts_from_source:\n  source: \"src-inv\"\n  match_pattern:\n",
            "    groups: [\"datacenter_dc2\"]\n    hosts: [\"appliance.dc2.example\"]\n",
        ))
        .expect("source fixture");

        let claim = source.advertised_scope().expect("declared");
        assert_eq!(claim.groups, vec!["datacenter_dc2"]);
        assert_eq!(claim.hosts, vec!["appliance.dc2.example"]);
        assert!(!claim.catch_all);
    }

    #[test]
    fn an_empty_pattern_advertises_a_spelled_out_catch_all() {
        let source: Source = serde_yaml_ng::from_str(concat!(
            "name: s\nproject_id: p\nscript_path: x\nttl_seconds: 60\n",
            "connector_type: \"ssh\"\n",
            "hosts_from_source:\n  source: \"src-inv\"\n",
        ))
        .expect("source fixture");

        let claim = source.advertised_scope().expect("declared");
        assert!(claim.groups.is_empty() && claim.hosts.is_empty());
        assert!(
            claim.catch_all,
            "claims-everything must be a flag, never bare empty lists"
        );
    }

    #[test]
    fn an_ordinary_source_makes_no_claim() {
        let source: Source =
            serde_yaml_ng::from_str("name: s\nproject_id: p\nscript_path: x\nttl_seconds: 60\n")
                .expect("source fixture");
        assert!(source.advertised_scope().is_none());
    }
}
