use std::collections::HashMap;

use serde::Deserialize;

use crate::domain::source::Source;

// Settings for one host or one group, per source — hosts/<name>.yaml and
// groups/<name>.yaml. The item's identity is the host's or group's name as
// the sources' datasets spell it; each entry under `sources` names a source
// and what that source does differently for it.
//
// Per source rather than for the host everywhere, because a TTL describes a
// source's copy of the host, not the host: bwkftp101 is in Device42, VMware
// and the SSH facts, and only its facts need to be 30 seconds fresh.
//
// Not checked against the data: hosts come and go with the systems the
// sources read, so an item naming a host no source has today is not an
// error, only an item that does nothing.
//
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct HostDef {
    // The display label: the item's `description`, falling back to its
    // identity (see config::take).
    pub name: String,

    // Source id → what that source does for this host.
    #[serde(default)]
    pub sources: HashMap<String, PerSource>,
}

// The same for a group: what a source does for every host in that group of
// ITS dataset, including hosts that join the group later.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct GroupDef {
    pub name: String,

    #[serde(default)]
    pub sources: HashMap<String, PerSource>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PerSource {
    // How long this source's copy stays fresh, in place of the source's own
    // ttl_seconds: what a read with refresh=true is gated by, and what the
    // host's freshness is reported against. A host's own TTL beats a
    // group's.
    pub ttl_seconds: u64,
}

// Fold host and group items into the sources they name: each source ends up
// with the per-host and per-group TTLs that concern it, which is all the
// refresh gate and the freshness reports read. Runs at load, after the
// sources are resolved, as a source's connector is folded in: nothing at
// sync or read time looks at an item.
//
// Returns the references that do not resolve. A view is refused by name: it
// holds no copy of its own, so its members' items are what decide.
pub fn fold_into_sources(
    hosts: &HashMap<String, HostDef>,
    groups: &HashMap<String, GroupDef>,
    sources: &mut HashMap<String, Source>,
    views: &[&String],
) -> Vec<String> {
    let mut errors = Vec::new();
    let items = hosts
        .iter()
        .map(|(id, h)| ("Host", id, &h.sources))
        .chain(groups.iter().map(|(id, g)| ("Group", id, &g.sources)));
    for (kind, id, per_source) in items {
        for (source_id, settings) in per_source {
            let Some(source) = sources.get_mut(source_id) else {
                errors.push(if views.contains(&source_id) {
                    format!(
                        "{kind} '{id}' names view '{source_id}' — a view holds no copy of its \
                         own; name the member source that gathers the host instead"
                    )
                } else {
                    format!("{kind} '{id}' references unknown source '{source_id}'")
                });
                continue;
            };
            let overrides = &mut source.ttl_overrides;
            let map = if kind == "Host" {
                &mut overrides.hosts
            } else {
                &mut overrides.groups
            };
            map.insert(id.clone(), settings.ttl_seconds);
        }
    }
    errors.sort();
    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::source::TtlOverrides;

    fn per(source: &str, ttl: u64) -> HashMap<String, PerSource> {
        HashMap::from([(source.to_string(), PerSource { ttl_seconds: ttl })])
    }

    fn source() -> Source {
        serde_json::from_value(serde_json::json!({
            "name": "S",
            "connector_type": "script",
            "script_path": "x.py",
            "ttl_seconds": 14400,
        }))
        .expect("a minimal source")
    }

    #[test]
    fn items_land_on_the_sources_they_name_and_nowhere_else() {
        let hosts = HashMap::from([(
            "web01".to_string(),
            HostDef {
                name: "Web".into(),
                sources: per("src-a", 30),
            },
        )]);
        let groups = HashMap::from([(
            "production".to_string(),
            GroupDef {
                name: "production".into(),
                sources: per("src-a", 900),
            },
        )]);
        let mut sources = HashMap::from([
            ("src-a".to_string(), source()),
            ("src-b".to_string(), source()),
        ]);

        let errors = fold_into_sources(&hosts, &groups, &mut sources, &[]);

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(
            sources["src-a"].ttl_overrides,
            TtlOverrides {
                hosts: HashMap::from([("web01".to_string(), 30)]),
                groups: HashMap::from([("production".to_string(), 900)]),
            }
        );
        assert_eq!(sources["src-b"].ttl_overrides, TtlOverrides::default());
    }

    #[test]
    fn an_unknown_source_or_a_view_is_refused_by_name() {
        let hosts = HashMap::from([(
            "web01".to_string(),
            HostDef {
                name: "web01".into(),
                sources: per("src-gone", 30),
            },
        )]);
        let groups = HashMap::from([(
            "production".to_string(),
            GroupDef {
                name: "production".into(),
                sources: per("vw-all", 30),
            },
        )]);
        let view = "vw-all".to_string();
        let mut sources = HashMap::new();

        let errors = fold_into_sources(&hosts, &groups, &mut sources, &[&view]);

        assert_eq!(errors.len(), 2);
        assert!(errors[0].contains("Group 'production' names view 'vw-all'"));
        assert!(errors[1].contains("Host 'web01' references unknown source 'src-gone'"));
    }
}
