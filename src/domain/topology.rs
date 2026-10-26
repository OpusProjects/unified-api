use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use utoipa::ToSchema;

use crate::domain::source::{ConnectorType, Source};

// The federation as seen from one instance: which other unified-api
// instances it pulls from (its peers), through which sources, and — asked of
// each peer in turn — what those pull from. A central's topology is the map
// of its edges; an edge's is itself alone.
//
// The same shape at every level, so a peer's answer nests as-is under the
// peer that asked for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Topology {
    /// What the instance is called (its `server.name`), when it says
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The running version, as the instance reports it
    pub version: String,
    /// Sources configured on the instance. Absent when the peer is too old to
    /// describe itself, and only its version could be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<usize>,
    /// The instances it pulls from, by base URL
    #[serde(default)]
    pub peers: Vec<Peer>,
}

// Why a peer was not asked, as a caller outside this instance's admins reads
// it: the resolver's own message names env vars and Vault paths.
const NOT_ASKED: &str = "not asked: this instance cannot authenticate to it";
// A link's last failure, as an instance this one pushes its map to reads it.
const PULL_FAILED: &str = "the last pull failed — its error is on the instance that pulls";

impl Topology {
    // What a caller who is not an admin here may see of the peers. They were
    // asked with THIS instance's key, which may be an admin key over there:
    // their own peers, URLs and counts are that key's view, not the caller's,
    // so only each peer's name and version are kept. And why a peer was not
    // asked stays generic (see NOT_ASKED); /credentials keeps that for
    // admins.
    pub fn redacted(mut self) -> Topology {
        for peer in &mut self.peers {
            if let Some(instance) = peer.instance.as_mut() {
                instance.sources = None;
                instance.peers = Vec::new();
            }
            if peer
                .error
                .as_deref()
                .is_some_and(|e| e.starts_with("not asked:") && !e.contains("depth limit"))
            {
                peer.error = Some(NOT_ASKED.to_string());
            }
        }
        self
    }

    // What this instance pushes to its peers (PUT /topology/federation). The
    // peer's admins are not this instance's: they get the redacted map, and a
    // failing link says it fails, not why — a sync error can name this
    // instance's env vars and Vault paths as well. Every peer gets the same
    // map, its own link included, so siblings still show where it sits.
    pub fn for_push(self) -> Topology {
        let mut topology = self.redacted();
        for peer in &mut topology.peers {
            for source in &mut peer.sources {
                if source.last_error.is_some() {
                    source.last_error = Some(PULL_FAILED.to_string());
                }
            }
        }
        topology
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Peer {
    /// The peer's base URL, as configured on the sources that read from it
    pub url: String,
    /// The local sources that read from this peer
    pub sources: Vec<PeerSource>,
    /// What the peer says about itself, when it could be asked. Absent with
    /// `error` set when it could not, or when the depth limit was reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(no_recursion)]
    pub instance: Option<Box<Topology>>,
    /// Why `instance` is missing: unreachable, refused, too deep
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PeerSource {
    /// The source id on this instance
    pub source_id: String,
    /// The source it reads on the peer
    pub remote_source: String,
    pub cached: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosts: Option<usize>,
    /// Groups in that cache; null when nothing is cached
    #[serde(default)]
    pub groups: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<u64>,
    /// Failed pulls since the last success — 0 while healthy
    pub consecutive_failures: u32,
    /// Seconds since the last pull that worked: when data last moved along
    /// this link. Absent until one has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pull_age_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

// What an instance that reads from this one sends it: the federation as THAT
// instance sees it, and the address it reaches this one at. An edge cannot
// map the federation itself — it reads from nobody, and does not know who
// reads from it — so the instance above tells it, over the same link and
// with the same key its remote sources already use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FederationPush {
    /// This instance's address, as the sender's remote sources spell it
    pub url: String,
    /// The sender's own address (its `server.url`), when it declares one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_url: Option<String>,
    /// The sender's own facts
    #[serde(default)]
    pub sender: InstanceSummary,
    /// The sender's topology, this instance included as one of its peers
    pub topology: Topology,
}

// What the sender says about itself, for the card the edge draws it as: the
// same facts its own Topology page shows on its own card.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct InstanceSummary {
    /// Its source ids, sorted (views left out: they hold no data of their own)
    pub sources: Vec<String>,
    /// Hosts across those sources' caches, added up
    pub hosts: usize,
    /// Groups across those sources' caches, added up
    pub groups: usize,
    pub endpoints: usize,
    /// Age of its oldest cached data; null when nothing is cached
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_age_seconds: Option<u64>,
}

// A pushed map as GET /topology/federation serves it back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Federation {
    /// The API key the sender authenticated with: which instance it was
    pub pushed_by: String,
    /// This instance's address, as the sender spells it
    pub url: String,
    /// The sender's address, when its `server.url` declares one
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_url: Option<String>,
    pub sender: InstanceSummary,
    pub topology: Topology,
    /// Seconds since it arrived. Senders push every minute, so an old one
    /// means the sender stopped (or lost its way here).
    pub age_seconds: u64,
}

// The peers of a configuration: every remote source the caller may see,
// grouped by the URL it reads from (trailing slashes ignored, so two spellings
// of one instance are one peer). Sorted by URL, then source id, so the answer
// is stable. The per-source state (cache, health) is filled in by the caller.
// The instance a remote source pulls from, as one spelling: a trailing slash
// would otherwise make the same peer two. None for any other connector, and
// for a remote one missing its `url` (config validation names that).
pub fn remote_url(source: &Source) -> Option<String> {
    if source.connector_type != ConnectorType::Remote {
        return None;
    }
    source
        .config
        .get("url")
        .map(|url| without_userinfo(url.trim_end_matches('/')))
}

// "https://user:pass@edge:8443" -> "https://edge:8443". An address shown back
// (a remote source's on its page and the topology, a project's git_url) must
// not carry credentials someone wrote into it. What reads the address to
// connect (the remote connector, git) uses the configured value, unaffected.
pub fn without_userinfo(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{}{}", &url[..scheme_end + 3], &rest[at + 1..]),
        None => url.to_string(),
    }
}

pub fn peers_of(
    sources: &HashMap<String, Source>,
    visible: impl Fn(&str) -> bool,
) -> BTreeMap<String, Vec<(&str, &Source)>> {
    let mut peers: BTreeMap<String, Vec<(&str, &Source)>> = BTreeMap::new();
    for (id, source) in sources {
        if !visible(id) {
            continue;
        }
        let Some(url) = remote_url(source) else {
            continue;
        };
        peers.entry(url).or_default().push((id.as_str(), source));
    }
    for members in peers.values_mut() {
        members.sort_by_key(|(id, _)| *id);
    }
    peers
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(connector: &str, url: Option<&str>) -> Source {
        let mut yaml = format!(
            "name: x\nproject_id: p\nscript_path: src-remote\nttl_seconds: 60\nconnector_type: {connector}\n"
        );
        if let Some(url) = url {
            yaml.push_str(&format!("config:\n  url: \"{url}\"\n"));
        }
        serde_saphyr::from_str(&yaml).unwrap()
    }

    fn peer(instance: Option<Topology>, error: Option<&str>) -> Peer {
        Peer {
            url: "https://edge.example".to_string(),
            sources: Vec::new(),
            instance: instance.map(Box::new),
            error: error.map(str::to_string),
        }
    }

    #[test]
    fn a_redacted_map_keeps_each_peers_version_and_no_secret_locations() {
        let behind = Topology {
            name: None,
            version: "0.33.0".to_string(),
            sources: Some(12),
            peers: vec![peer(None, None)],
        };
        let topology = Topology {
            name: None,
            version: "0.34.0".to_string(),
            sources: Some(1),
            peers: vec![
                peer(Some(behind), None),
                peer(None, Some("not asked: Env var 'EDGE_TOKEN' not set")),
                peer(None, Some("not asked: depth limit reached")),
            ],
        };
        let seen = topology.redacted();
        let instance = seen.peers[0].instance.as_ref().unwrap();
        assert_eq!(instance.version, "0.33.0");
        assert!(instance.peers.is_empty() && instance.sources.is_none());
        assert!(
            !seen.peers[1]
                .error
                .as_deref()
                .unwrap()
                .contains("EDGE_TOKEN")
        );
        assert_eq!(
            seen.peers[2].error.as_deref(),
            Some("not asked: depth limit reached")
        );
    }

    #[test]
    fn a_pushed_map_says_a_link_fails_but_not_why() {
        let mut link = peer(None, None);
        link.sources.push(PeerSource {
            source_id: "src-dc1".to_string(),
            remote_source: "src-fleet".to_string(),
            cached: false,
            hosts: None,
            groups: None,
            age_seconds: None,
            consecutive_failures: 3,
            last_pull_age_seconds: None,
            last_error: Some("Vault path 'kv/central/dc1' denied".to_string()),
        });
        let topology = Topology {
            name: None,
            version: "0.35.0".to_string(),
            sources: Some(1),
            peers: vec![link],
        };
        let pushed = topology.for_push();
        let source = &pushed.peers[0].sources[0];
        assert_eq!(source.consecutive_failures, 3);
        let error = source.last_error.as_deref().unwrap();
        assert!(!error.contains("kv/central"), "{error}");
    }

    #[test]
    fn credentials_in_a_remote_url_are_never_shown() {
        assert_eq!(
            remote_url(&source(
                "remote",
                Some("https://svc:s3cret@edge.example:8443/")
            ))
            .as_deref(),
            Some("https://edge.example:8443")
        );
        // An "@" after the host is part of the path, not credentials.
        assert_eq!(
            without_userinfo("https://edge.example/a@b"),
            "https://edge.example/a@b"
        );
    }

    #[test]
    fn remote_url_is_only_a_remote_sources_and_has_one_spelling() {
        assert_eq!(
            remote_url(&source("remote", Some("https://dc1.example:8443/"))).as_deref(),
            Some("https://dc1.example:8443")
        );
        assert_eq!(remote_url(&source("remote", None)), None);
        // A script may carry a `url` of its own; it is not a peer.
        assert_eq!(
            remote_url(&source("script", Some("https://api.example"))),
            None
        );
    }

    #[test]
    fn remote_sources_group_by_peer_url() {
        let sources: HashMap<String, Source> = [
            (
                "src-dc1-facts",
                source("remote", Some("https://dc1.example:8443")),
            ),
            (
                "src-dc1-kvm",
                source("remote", Some("https://dc1.example:8443/")),
            ),
            (
                "src-dc2-facts",
                source("remote", Some("https://dc2.example:8443")),
            ),
            ("src-d42", source("script", None)),
        ]
        .into_iter()
        .map(|(id, s)| (id.to_string(), s))
        .collect();

        let peers = peers_of(&sources, |_| true);
        let urls: Vec<&String> = peers.keys().collect();
        assert_eq!(
            urls,
            ["https://dc1.example:8443", "https://dc2.example:8443"]
        );
        let dc1: Vec<&str> = peers["https://dc1.example:8443"]
            .iter()
            .map(|(id, _)| *id)
            .collect();
        assert_eq!(dc1, ["src-dc1-facts", "src-dc1-kvm"]);
    }

    // A restricted caller sees the peers of the sources it may read, and no
    // others: the URLs of the rest are not its business.
    #[test]
    fn a_caller_sees_only_peers_behind_its_sources() {
        let sources: HashMap<String, Source> = [
            ("src-dc1", source("remote", Some("https://dc1.example"))),
            ("src-dc2", source("remote", Some("https://dc2.example"))),
        ]
        .into_iter()
        .map(|(id, s)| (id.to_string(), s))
        .collect();

        let peers = peers_of(&sources, |id| id == "src-dc1");
        assert_eq!(peers.keys().collect::<Vec<_>>(), ["https://dc1.example"]);
    }

    // A peer's answer is the same shape, so it nests without translation.
    #[test]
    fn a_peer_answer_nests_as_is() {
        let edge = r#"{"version":"0.33.0","sources":3,"peers":[]}"#;
        let topology: Topology = serde_json::from_str(edge).unwrap();
        assert_eq!(topology.sources, Some(3));
        let old = r#"{"version":"0.32.0"}"#;
        let topology: Topology = serde_json::from_str(old).unwrap();
        assert!(topology.peers.is_empty() && topology.sources.is_none());
    }
}
