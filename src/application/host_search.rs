use std::collections::{BTreeSet, HashMap};

use crate::application::views::snapshot;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::source::{AdvertisedScopeRegistry, Source};
use crate::domain::view::View;
use crate::ports::cache::CachePort;

// "Where is this host?" across every source at once: which sources hold it,
// how old each one's data is, and which endpoints it reaches. The question an
// operator brings to the UI most, answered from the cache (no gather) so it
// is as cheap as a few map scans.

// A query shorter than this matches too much to be useful (every host has an
// "a" in it), and costs a scan of everything to say so.
pub const MIN_QUERY: usize = 2;

pub struct HostSource {
    pub source_id: String,
    pub age_seconds: u64,
}

pub struct HostMatch {
    pub host: String,
    pub sources: Vec<HostSource>,
    pub endpoints: Vec<String>,
}

pub struct SearchResult {
    pub matches: Vec<HostMatch>,
    // How many hosts matched in all, before the limit: "20 of 312".
    pub total: usize,
}

// How well a host name matches: the whole name, its start, or anywhere.
// Lower sorts first, so typing a full name puts that host on top however
// many others contain it.
fn rank(name: &str, needle: &str) -> u8 {
    if name == needle {
        0
    } else if name.starts_with(needle) {
        1
    } else {
        2
    }
}

// Case-insensitive, on any part of the name. Only sources the caller may
// read are searched, and only endpoints they may run are named: search must
// not reveal a host's existence through a source the caller cannot open.
// What a search reads: the configured sources, views and endpoints, the
// cache, and the advertised scopes views resolve their members with.
pub struct Catalog<'a> {
    pub sources: &'a HashMap<String, Source>,
    pub views: &'a HashMap<String, View>,
    pub endpoints: &'a HashMap<String, OutputEndpoint>,
    pub cache: &'a dyn CachePort,
    pub scopes: &'a AdvertisedScopeRegistry,
}

pub fn search(
    query: &str,
    limit: usize,
    catalog: Catalog<'_>,
    may_read: impl Fn(&str) -> bool,
    may_run: impl Fn(&str) -> bool,
) -> SearchResult {
    let Catalog {
        sources,
        views,
        endpoints,
        cache,
        scopes,
    } = catalog;
    let needle = query.trim().to_lowercase();
    if needle.chars().count() < MIN_QUERY {
        return SearchResult {
            matches: Vec::new(),
            total: 0,
        };
    }

    // Each readable source's matching hosts, by exact name: a host is the
    // same host wherever it is listed under the same name.
    let mut found: HashMap<String, Vec<HostSource>> = HashMap::new();
    // The matching hosts of EVERY source, readable or not, for the endpoint
    // check below: a key may run an endpoint whose limiting source it cannot
    // open, and the endpoint's output would show the host all the same.
    let mut held: HashMap<&str, BTreeSet<String>> = HashMap::new();
    let mut ids: Vec<&String> = sources.keys().collect();
    ids.sort();
    for id in ids {
        let Some(entry) = cache.get(id) else { continue };
        let readable = may_read(id);
        let age_seconds = entry.age_seconds();
        let mut here = BTreeSet::new();
        for host in entry.dataset.hostvars.keys() {
            if host.to_lowercase().contains(&needle) {
                if readable {
                    found.entry(host.clone()).or_default().push(HostSource {
                        source_id: id.clone(),
                        age_seconds,
                    });
                }
                here.insert(host.clone());
            }
        }
        held.insert(id.as_str(), here);
    }

    // A view the caller may read, by the hosts IT serves: a key granted only
    // a view finds its hosts here, listed under the view, and only those the
    // view serves (its members' other hosts stay out of reach, as on the
    // view's own routes).
    let mut view_ids: Vec<&String> = views.keys().filter(|id| may_read(id)).collect();
    view_ids.sort();
    for id in view_ids {
        let snap = snapshot(cache, sources, scopes, id, &views[id.as_str()]);
        let age_seconds = snap.age_seconds();
        for host in snap.hosts() {
            if host.to_lowercase().contains(&needle) {
                found.entry(host.to_string()).or_default().push(HostSource {
                    source_id: id.clone(),
                    age_seconds,
                });
            }
        }
    }

    let total = found.len();
    let mut hosts: Vec<(String, Vec<HostSource>)> = found.into_iter().collect();
    // Best match first; among names equal but for case, the one spelled as
    // typed; then by name, so the order never depends on the map's.
    let typed = query.trim();
    hosts.sort_by(|(a, _), (b, _)| {
        let (la, lb) = (a.to_lowercase(), b.to_lowercase());
        rank(&la, &needle)
            .cmp(&rank(&lb, &needle))
            .then_with(|| (a.as_str() != typed).cmp(&(b.as_str() != typed)))
            .then_with(|| la.cmp(&lb))
            .then_with(|| a.cmp(b))
    });
    hosts.truncate(limit);

    let holds = |source_id: &str, host: &str| {
        held.get(source_id)
            .is_some_and(|hosts| hosts.contains(host))
    };
    let mut endpoint_ids: Vec<&String> = endpoints.keys().filter(|id| may_run(id)).collect();
    endpoint_ids.sort();

    // An endpoint missing data from any source answers 503, not an
    // inventory: it reaches no host, so it is left out entirely.
    endpoint_ids.retain(|id| {
        endpoints[id.as_str()]
            .source_ids
            .iter()
            .all(|s| cache.get(s).is_some())
    });

    // A builtin with settings of its own (a `group`, a host filter) keeps
    // only some of its sources' hosts. Those are worked out the way a call
    // works them out, once per search and only for such endpoints: an
    // endpoint without settings is answered from the sources alone, cheaply.
    let filtered: HashMap<&str, BTreeSet<String>> = endpoint_ids
        .iter()
        .filter_map(|id| {
            let endpoint = &endpoints[id.as_str()];
            if endpoint.output.is_none() || endpoint.config.is_empty() {
                return None;
            }
            let datasets: HashMap<String, std::sync::Arc<crate::domain::dataset::Dataset>> =
                endpoint
                    .source_ids
                    .iter()
                    .filter_map(|s| cache.get(s).map(|e| (s.clone(), e.dataset.clone())))
                    .collect();
            let datasets = match &endpoint.limit {
                Some(limit) => crate::application::output::apply_limit(&datasets, limit),
                None => datasets,
            };
            let kept = crate::application::output::default_hosts(&datasets, &endpoint.config);
            Some((id.as_str(), kept.into_iter().collect()))
        })
        .collect();

    let matches = hosts
        .into_iter()
        .map(|(host, sources)| {
            // In an endpoint's output when one of its sources holds the host;
            // under a limit, only when the limiting source does, since that
            // source decides which hosts the output keeps at all; under its
            // own settings, only when they keep it.
            let endpoints = endpoint_ids
                .iter()
                .filter(|id| {
                    if let Some(kept) = filtered.get(id.as_str()) {
                        return kept.contains(&host);
                    }
                    let endpoint = &endpoints[id.as_str()];
                    match endpoint
                        .limit
                        .as_ref()
                        .and_then(|l| l.by_hosts_from_inventory.as_deref())
                    {
                        Some(limiting) => holds(limiting, &host),
                        None => endpoint.source_ids.iter().any(|s| holds(s, &host)),
                    }
                })
                .map(|id| (*id).clone())
                .collect();
            HostMatch {
                host,
                sources,
                endpoints,
            }
        })
        .collect();

    SearchResult { matches, total }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::domain::cache_entry::CacheEntry;
    use crate::domain::dataset::Dataset;

    fn source() -> Source {
        serde_saphyr::from_str("name: x\nproject_id: p\nscript_path: s.py\nttl_seconds: 60\n")
            .unwrap()
    }

    fn endpoint(sources: &[&str], limit: Option<&str>) -> OutputEndpoint {
        let mut yaml = format!(
            "name: e\nsource_ids: [{}]\noutput: ansible\n",
            sources.join(", ")
        );
        if let Some(l) = limit {
            yaml.push_str(&format!("limit:\n  by_hosts_from_inventory: {l}\n"));
        }
        serde_saphyr::from_str(&yaml).unwrap()
    }

    fn cache_with(entries: &[(&str, &[&str])]) -> MemoryCache {
        let cache = MemoryCache::new();
        for (id, hosts) in entries {
            let hostvars: serde_json::Map<String, serde_json::Value> = hosts
                .iter()
                .map(|h| (h.to_string(), serde_json::json!({})))
                .collect();
            let dataset: Dataset =
                serde_json::from_value(serde_json::json!({ "hostvars": hostvars, "groups": {} }))
                    .unwrap();
            cache.set(id, CacheEntry::new(dataset, 60));
        }
        cache
    }

    #[test]
    fn finds_a_host_in_every_source_and_ranks_the_exact_name_first() {
        let empty_views: HashMap<String, View> = HashMap::new();
        let scopes = AdvertisedScopeRegistry::new();
        let sources: HashMap<String, Source> = [("src-a", source()), ("src-b", source())]
            .map(|(k, v)| (k.to_string(), v))
            .into();
        let cache = cache_with(&[
            ("src-a", &["web1.example", "web10.example", "db1.example"]),
            ("src-b", &["WEB1.example", "web1.example"]),
        ]);
        let found = search(
            "web1.example",
            10,
            Catalog {
                sources: &sources,
                views: &empty_views,
                endpoints: &HashMap::new(),
                cache: &cache,
                scopes: &scopes,
            },
            |_| true,
            |_| true,
        );
        assert_eq!(found.matches[0].host, "web1.example");
        let held: Vec<&str> = found.matches[0]
            .sources
            .iter()
            .map(|s| s.source_id.as_str())
            .collect();
        assert_eq!(held, ["src-a", "src-b"]);
        // Case-insensitive: WEB1 matches too, as its own name.
        assert!(found.matches.iter().any(|m| m.host == "WEB1.example"));
        assert!(!found.matches.iter().any(|m| m.host == "db1.example"));
    }

    #[test]
    fn a_source_the_key_may_not_read_is_not_searched() {
        let empty_views: HashMap<String, View> = HashMap::new();
        let scopes = AdvertisedScopeRegistry::new();
        let sources: HashMap<String, Source> = [("src-open", source()), ("src-closed", source())]
            .map(|(k, v)| (k.to_string(), v))
            .into();
        let cache = cache_with(&[("src-open", &["a1"]), ("src-closed", &["a2"])]);
        let found = search(
            "a",
            10,
            Catalog {
                sources: &sources,
                views: &empty_views,
                endpoints: &HashMap::new(),
                cache: &cache,
                scopes: &scopes,
            },
            |id| id == "src-open",
            |_| true,
        );
        assert_eq!(found.total, 0, "one character is below the minimum");
        let found = search(
            "a2",
            10,
            Catalog {
                sources: &sources,
                views: &empty_views,
                endpoints: &HashMap::new(),
                cache: &cache,
                scopes: &scopes,
            },
            |id| id == "src-open",
            |_| true,
        );
        assert_eq!(
            found.total, 0,
            "a host only in an unreadable source stays hidden"
        );
    }

    #[test]
    fn an_endpoint_under_a_limit_includes_only_the_limiting_sources_hosts() {
        let empty_views: HashMap<String, View> = HashMap::new();
        let scopes = AdvertisedScopeRegistry::new();
        let sources: HashMap<String, Source> = [("src-inv", source()), ("src-facts", source())]
            .map(|(k, v)| (k.to_string(), v))
            .into();
        let endpoints: HashMap<String, OutputEndpoint> = [
            (
                "ep-limited",
                endpoint(&["src-inv", "src-facts"], Some("src-inv")),
            ),
            ("ep-all", endpoint(&["src-inv", "src-facts"], None)),
        ]
        .map(|(k, v)| (k.to_string(), v))
        .into();
        let cache = cache_with(&[
            ("src-inv", &["in-both", "inv-only"]),
            ("src-facts", &["in-both", "facts-only"]),
        ]);
        let reach = |q: &str| {
            search(
                q,
                10,
                Catalog {
                    sources: &sources,
                    views: &empty_views,
                    endpoints: &endpoints,
                    cache: &cache,
                    scopes: &scopes,
                },
                |_| true,
                |_| true,
            )
            .matches[0]
                .endpoints
                .clone()
        };
        assert_eq!(reach("in-both"), ["ep-all", "ep-limited"]);
        // Facts alone cannot add a host to a limited endpoint.
        assert_eq!(reach("facts-only"), ["ep-all"]);
    }

    // An endpoint's own settings decide too: a `group` it keeps leaves out a
    // host its sources hold; and an endpoint missing a source answers 503,
    // so it reaches nothing.
    #[test]
    fn an_endpoint_lists_a_host_only_if_its_own_output_would() {
        let empty_views: HashMap<String, View> = HashMap::new();
        let scopes = AdvertisedScopeRegistry::new();
        let sources: HashMap<String, Source> = [("src-inv", source()), ("src-gone", source())]
            .map(|(k, v)| (k.to_string(), v))
            .into();
        let mut linux = endpoint(&["src-inv"], None);
        linux
            .config
            .insert("group".to_string(), "linux".to_string());
        let endpoints: HashMap<String, OutputEndpoint> = [
            ("ep-linux", linux),
            ("ep-broken", endpoint(&["src-inv", "src-gone"], None)),
        ]
        .map(|(k, v)| (k.to_string(), v))
        .into();
        let cache = MemoryCache::new();
        let dataset: Dataset = serde_json::from_value(serde_json::json!({
            "hostvars": { "web1": {}, "win1": {} },
            "groups": { "linux": { "hosts": ["web1"] } }
        }))
        .unwrap();
        cache.set("src-inv", CacheEntry::new(dataset, 60));

        let reach = |q: &str| {
            search(
                q,
                10,
                Catalog {
                    sources: &sources,
                    views: &empty_views,
                    endpoints: &endpoints,
                    cache: &cache,
                    scopes: &scopes,
                },
                |_| true,
                |_| true,
            )
            .matches[0]
                .endpoints
                .clone()
        };
        assert_eq!(reach("web1"), ["ep-linux"]);
        assert!(reach("win1").is_empty(), "outside the endpoint's group");
    }
}
