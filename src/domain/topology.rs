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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
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
