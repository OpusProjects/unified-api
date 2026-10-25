use std::collections::HashMap;
use std::sync::Arc;

use crate::domain::source::Source;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::topology::{Peer, PeerSource, Topology, peers_of};
use crate::ports::cache::CachePort;
use crate::ports::peer::{PeerPort, PeerRequest};
use crate::ports::secrets::SecretsPort;

// The use case "describe the federation from here".
//
// Local facts first (which remote sources read from which URL, and how their
// pulls are going), then each peer asked to describe itself, concurrently,
// with one hop less to spend. A peer that cannot be asked is shown with the
// reason; the map never fails because one instance is down — that is exactly
// when someone opens it.

// Most an instance will pass on: a central and its edges is two levels, and
// a loop of instances that federate each other must end somewhere.
pub const MAX_DEPTH: u8 = 3;

// A topology request waits on its slowest peer; a source's own sync timeout
// (often minutes, for big datasets) would hold the page that long.
const PEER_TIMEOUT_SECONDS: u64 = 8;

// How long to wait for a peer that will itself ask `depth` more levels: its
// own answer, plus the time it may spend waiting on each level below. A flat
// timeout let one dead instance two hops away hide a healthy one: the middle
// peer was still waiting on it when our wait for the middle peer ran out.
fn peer_timeout(depth: u8) -> u64 {
    PEER_TIMEOUT_SECONDS * (u64::from(depth) + 1)
}

pub struct Inputs<'a> {
    pub sources: &'a HashMap<String, Source>,
    pub cache: &'a dyn CachePort,
    pub health: &'a SyncHealthRegistry,
    pub secrets: &'a dyn SecretsPort,
    pub peers: Arc<dyn PeerPort>,
}

pub async fn describe(inputs: Inputs<'_>, visible: impl Fn(&str) -> bool, depth: u8) -> Topology {
    let depth = depth.min(MAX_DEPTH);
    let visible_count = inputs.sources.keys().filter(|id| visible(id)).count();
    let grouped = peers_of(inputs.sources, &visible);

    let mut peers: Vec<Peer> = Vec::with_capacity(grouped.len());
    let mut asks = tokio::task::JoinSet::new();
    let mut to_ask = Vec::new();
    let mut asked = Vec::new();

    for (index, (url, members)) in grouped.into_iter().enumerate() {
        let sources = members
            .iter()
            .map(|(id, source)| {
                let entry = inputs.cache.get(id);
                let health = inputs.health.get(id).unwrap_or_default();
                PeerSource {
                    source_id: id.to_string(),
                    remote_source: source.script_path.clone(),
                    cached: entry.is_some(),
                    hosts: entry.as_ref().map(|e| e.dataset.hostvars.len()),
                    groups: entry.as_ref().map(|e| e.dataset.groups.len()),
                    age_seconds: entry.as_ref().map(|e| e.age_seconds()),
                    consecutive_failures: health.consecutive_failures,
                    last_error: health.last_error,
                }
            })
            .collect();

        if depth == 0 {
            peers.push(Peer {
                url,
                sources,
                instance: None,
                error: Some("not asked: depth limit reached".to_string()),
            });
            continue;
        }

        // The key and TLS setting the sources reading this peer already use:
        // whatever lets them pull data lets the map ask who it is. Resolved
        // below, once every peer is listed.
        let (_, first) = members[0];
        to_ask.push((index, url.clone(), first));
        peers.push(Peer {
            url,
            sources,
            instance: None,
            error: None,
        });
    }

    // One after another: resolutions come from the secrets cache (env vars
    // are instant), and it is the peer requests below that run concurrently.
    let mut keys = Vec::with_capacity(to_ask.len());
    for (_, _, first) in &to_ask {
        keys.push(
            crate::application::credentials::resolve_credentials(
                inputs.secrets,
                &first.credential_ids,
            )
            .await,
        );
    }

    for ((index, url, first), resolved) in to_ask.into_iter().zip(keys) {
        let api_key = match resolved {
            Ok(credentials) => credentials
                .get("token")
                .or_else(|| credentials.get("TOKEN"))
                .or_else(|| credentials.get("api_key"))
                .cloned(),
            Err(e) => {
                peers[index].error = Some(format!("not asked: {}", e.message));
                continue;
            }
        };
        let request = PeerRequest {
            url,
            api_key,
            insecure_tls: first
                .config
                .get("insecure_tls")
                .is_some_and(|v| v == "true"),
            timeout_seconds: peer_timeout(depth - 1),
            depth: depth - 1,
        };
        let port = Arc::clone(&inputs.peers);
        asks.spawn(async move { (index, port.describe(request).await) });
        asked.push(index);
    }

    // Every grouped peer was pushed in order, asked or not, so the index a
    // task carries is its place in `peers` whatever order answers arrive in.
    while let Some(joined) = asks.join_next().await {
        let Ok((index, answer)) = joined else {
            continue;
        };
        if let Some(peer) = peers.get_mut(index) {
            match answer {
                Ok(topology) => peer.instance = Some(Box::new(topology)),
                Err(reason) => peer.error = Some(reason),
            }
        }
    }

    // A task that panicked returned no index: its peer would be left with
    // neither an answer nor a reason, which the contract never allows.
    for index in asked {
        let peer = &mut peers[index];
        if peer.instance.is_none() && peer.error.is_none() {
            peer.error = Some("not described: an internal error stopped the request".to_string());
        }
    }

    Topology {
        version: env!("CARGO_PKG_VERSION").to_string(),
        sources: Some(visible_count),
        peers,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A peer that will ask its own peers gets their wait on top of its own,
    // or a dead instance two hops away hides the healthy one in between.
    #[test]
    fn a_peer_asking_further_gets_longer_than_its_own_peers() {
        assert_eq!(peer_timeout(0), PEER_TIMEOUT_SECONDS);
        assert!(peer_timeout(1) > peer_timeout(0) + PEER_TIMEOUT_SECONDS - 1);
    }
    use crate::adapters::out::cache::memory::MemoryCache;
    use crate::adapters::out::secrets::mock::MockSecrets;
    use crate::ports::peer::PeerFuture;

    // dc1 answers as an edge with no peers of its own; dc2 is down.
    struct Peers;

    impl PeerPort for Peers {
        fn describe(&self, request: PeerRequest) -> PeerFuture {
            Box::pin(async move {
                assert_eq!(request.api_key.as_deref(), Some("edge-key"));
                if request.url.contains("dc2") {
                    return Err("unreachable".to_string());
                }
                Ok(Topology {
                    version: "0.33.0".to_string(),
                    sources: Some(3),
                    peers: Vec::new(),
                })
            })
        }
    }

    fn remote(url: &str) -> Source {
        serde_saphyr::from_str(&format!(
            "name: x\nproject_id: p\nscript_path: src-ssh-facts\nttl_seconds: 60\nconnector_type: remote\n\
             credential_ids: [cred-edge]\nconfig:\n  url: \"{url}\"\n"
        ))
        .unwrap()
    }

    fn sources() -> HashMap<String, Source> {
        [
            ("src-edge-dc1", remote("https://dc1.example:8443")),
            ("src-edge-dc2", remote("https://dc2.example:8443")),
        ]
        .into_iter()
        .map(|(id, s)| (id.to_string(), s))
        .collect()
    }

    #[tokio::test]
    async fn every_peer_is_described_or_says_why_not() {
        let sources = sources();
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();
        let secrets = MockSecrets::new().with("cred-edge", [("token", "edge-key")]);
        let topology = describe(
            Inputs {
                sources: &sources,
                cache: &cache,
                health: &health,
                secrets: &secrets,
                peers: Arc::new(Peers),
            },
            |_| true,
            2,
        )
        .await;

        assert_eq!(topology.sources, Some(2));
        assert_eq!(topology.peers.len(), 2);
        let dc1 = &topology.peers[0];
        assert_eq!(dc1.url, "https://dc1.example:8443");
        assert_eq!(dc1.instance.as_ref().unwrap().version, "0.33.0");
        assert_eq!(dc1.sources[0].remote_source, "src-ssh-facts");
        let dc2 = &topology.peers[1];
        assert!(dc2.instance.is_none());
        assert_eq!(dc2.error.as_deref(), Some("unreachable"));
    }

    // Depth 0 maps this instance's peers without asking them anything: the
    // hop budget ran out, and a loop of instances must end.
    #[tokio::test]
    async fn no_depth_left_means_no_peer_is_asked() {
        let sources = sources();
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();
        let secrets = MockSecrets::new();
        let topology = describe(
            Inputs {
                sources: &sources,
                cache: &cache,
                health: &health,
                secrets: &secrets,
                peers: Arc::new(Peers),
            },
            |_| true,
            0,
        )
        .await;
        assert!(topology.peers.iter().all(|p| p.instance.is_none()));
        assert!(
            topology.peers[0]
                .error
                .as_deref()
                .unwrap()
                .contains("depth")
        );
    }
}
