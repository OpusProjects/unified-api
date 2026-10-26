use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::domain::source::Source;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::topology::{
    Federation, FederationPush, InstanceSummary, Peer, PeerSource, Topology, peers_of,
};
use crate::ports::cache::CachePort;
use crate::ports::peer::{PeerPort, PeerRequest, PushError};
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

#[derive(Clone)]
pub struct Inputs<'a> {
    pub sources: &'a HashMap<String, Source>,
    pub cache: &'a dyn CachePort,
    pub health: &'a SyncHealthRegistry,
    pub secrets: &'a dyn SecretsPort,
    pub peers: Arc<dyn PeerPort>,
    // This instance's `server.name`, for its own answer.
    pub name: Option<&'a str>,
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
                    last_pull_age_seconds: health.last_success_age_seconds,
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
        to_ask.push((index, url.clone(), key_source(&members)));
        peers.push(Peer {
            url,
            sources,
            instance: None,
            error: None,
        });
    }

    // One after another: resolutions come from the secrets cache (env vars
    // are instant), and it is the peer requests below that run concurrently.
    for (index, url, first) in to_ask {
        let request = match peer_request(inputs.secrets, url, first, depth - 1).await {
            Ok(request) => request,
            Err(reason) => {
                peers[index].error = Some(reason);
                continue;
            }
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
        name: inputs.name.map(str::to_string),
        version: env!("CARGO_PKG_VERSION").to_string(),
        sources: Some(visible_count),
        peers,
    }
}

// Which of the sources reading a peer lends the map its key and TLS setting:
// the first by id (peers_of sorts them). One choice for asking a peer and for
// pushing to it — when two sources with different credentials read one peer,
// a push picked by HashMap order could use another key than the map was
// built with (a broken one, even), and flip between them across reloads,
// leaving the peer two maps under two key names.
fn key_source<'s>(members: &[(&str, &'s Source)]) -> &'s Source {
    members[0].1
}

// How to reach a peer: the key and TLS setting of the first source reading
// from it, since whatever lets them pull data lets the map ask who it is (and
// tell it where it sits). `depth` is what the peer may spend asking further.
async fn peer_request(
    secrets: &dyn SecretsPort,
    url: String,
    source: &Source,
    depth: u8,
) -> Result<PeerRequest, String> {
    let credentials =
        crate::application::credentials::resolve_credentials(secrets, &source.credential_ids)
            .await
            .map_err(|e| format!("not asked: {}", e.message))?;
    let api_key = credentials
        .get("token")
        .or_else(|| credentials.get("TOKEN"))
        .or_else(|| credentials.get("api_key"))
        .cloned();
    Ok(PeerRequest {
        url,
        api_key,
        insecure_tls: source
            .config
            .get("insecure_tls")
            .is_some_and(|v| v == "true"),
        timeout_seconds: peer_timeout(depth),
        depth,
    })
}

// How deep the pushed map goes: the same default the Topology page asks for.
const PUSH_DEPTH: u8 = 2;

// The use case "tell every peer where it sits": map the federation from here
// (all sources), then send it to each peer that answered. One that did not is
// already on the map as unreachable; there is nobody to tell. What is sent is
// the map as a peer's admins may see it (Topology::for_push), not ours: our
// peers' own maps were asked with our keys, and our errors name our secrets.
// Returns each push's outcome, by URL, for the caller to log.
pub async fn push_federation(
    inputs: Inputs<'_>,
    sender_url: Option<&str>,
    endpoints: usize,
) -> Vec<(String, Result<(), PushError>)> {
    let sender = summarize(inputs.sources, inputs.cache, endpoints);
    let topology = describe(inputs.clone(), |_| true, PUSH_DEPTH).await;
    let grouped = peers_of(inputs.sources, |_| true);
    let answered: Vec<String> = topology
        .peers
        .iter()
        .filter(|p| p.instance.is_some())
        .map(|p| p.url.clone())
        .collect();
    let pushed = topology.for_push();
    let mut pushes = tokio::task::JoinSet::new();
    let mut outcomes = Vec::new();

    for url in answered {
        let Some(members) = grouped.get(&url) else {
            continue;
        };
        match peer_request(inputs.secrets, url.clone(), key_source(members), 0).await {
            Ok(request) => {
                let body = FederationPush {
                    url: url.clone(),
                    sender_url: sender_url.map(str::to_string),
                    sender: sender.clone(),
                    topology: pushed.clone(),
                };
                let port = Arc::clone(&inputs.peers);
                pushes.spawn(async move { (url, port.push(request, body).await) });
            }
            Err(reason) => outcomes.push((url, Err(PushError::Failed(reason)))),
        }
    }
    while let Some(joined) = pushes.join_next().await {
        if let Ok(outcome) = joined {
            outcomes.push(outcome);
        }
    }
    outcomes.sort_by(|a, b| a.0.cmp(&b.0));
    outcomes
}

// This instance's own card, as an edge draws it: the facts the instance's own
// Topology page shows for itself, from its configuration and cache.
fn summarize(
    sources: &HashMap<String, Source>,
    cache: &dyn CachePort,
    endpoints: usize,
) -> InstanceSummary {
    let mut summary = InstanceSummary {
        sources: sources.keys().cloned().collect(),
        endpoints,
        ..Default::default()
    };
    summary.sources.sort();
    for id in sources.keys() {
        if let Some(entry) = cache.get(id) {
            summary.hosts += entry.dataset.hostvars.len();
            summary.groups += entry.dataset.groups.len();
            let age = entry.age_seconds();
            summary.oldest_age_seconds =
                Some(summary.oldest_age_seconds.map_or(age, |a| a.max(age)));
        }
    }
    summary
}

// A pushed map is dropped once its sender has been silent this long: ten
// missed pushes, so a sender that was removed (or reconfigured to read from
// elsewhere) does not leave its picture here forever.
const FEDERATION_EXPIRES: Duration = Duration::from_secs(600);

// The maps pushed here, one per sender (by the key it authenticated with: two
// instances reading from this one each keep their own). In memory only: after
// a restart the next push, within a minute, brings it back.
#[derive(Default)]
pub struct FederationStore {
    pushed: RwLock<HashMap<String, (FederationPush, Instant)>>,
}

impl FederationStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, pushed_by: String, push: FederationPush) {
        let mut pushed = self.pushed.write().expect("federation store");
        pushed.retain(|_, (_, at)| at.elapsed() < FEDERATION_EXPIRES);
        pushed.insert(pushed_by, (push, Instant::now()));
    }

    // Newest first: the sender pushing now is the likeliest to be current.
    pub fn list(&self) -> Vec<Federation> {
        let pushed = self.pushed.read().expect("federation store");
        let mut list: Vec<Federation> = pushed
            .iter()
            .filter(|(_, (_, at))| at.elapsed() < FEDERATION_EXPIRES)
            .map(|(by, (push, at))| Federation {
                pushed_by: by.clone(),
                url: push.url.clone(),
                sender_url: push.sender_url.clone(),
                sender: push.sender.clone(),
                topology: push.topology.clone(),
                age_seconds: at.elapsed().as_secs(),
            })
            .collect();
        list.sort_by(|a, b| {
            a.age_seconds
                .cmp(&b.age_seconds)
                .then(a.pushed_by.cmp(&b.pushed_by))
        });
        list
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
    use crate::ports::peer::{PeerFuture, PushFuture};
    use std::sync::Mutex;

    // dc1 answers as an edge with no peers of its own; dc2 is down. Pushes
    // are recorded, by URL.
    #[derive(Default)]
    struct Peers {
        pushed: Mutex<Vec<FederationPush>>,
    }

    impl PeerPort for Peers {
        fn describe(&self, request: PeerRequest) -> PeerFuture {
            Box::pin(async move {
                assert_eq!(request.api_key.as_deref(), Some("edge-key"));
                if request.url.contains("dc2") {
                    return Err("unreachable".to_string());
                }
                Ok(Topology {
                    name: None,
                    version: "0.33.0".to_string(),
                    sources: Some(3),
                    peers: Vec::new(),
                })
            })
        }

        fn push(&self, request: PeerRequest, body: FederationPush) -> PushFuture {
            assert_eq!(request.api_key.as_deref(), Some("edge-key"));
            self.pushed.lock().unwrap().push(body);
            Box::pin(async { Ok(()) })
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
                peers: Arc::new(Peers::default()),
                name: None,
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

    // An instance names itself in its answer, when its server.name says.
    #[tokio::test]
    async fn an_instance_says_its_name() {
        let sources = HashMap::new();
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();
        let secrets = MockSecrets::new();
        let topology = describe(
            Inputs {
                sources: &sources,
                cache: &cache,
                health: &health,
                secrets: &secrets,
                peers: Arc::new(Peers::default()),
                name: Some("Central"),
            },
            |_| true,
            0,
        )
        .await;
        assert_eq!(topology.name.as_deref(), Some("Central"));
    }

    // When data last moved along a link: the last pull that worked, for the
    // UI to animate. Absent until one has.
    #[tokio::test]
    async fn a_peer_source_says_when_it_last_pulled() {
        let sources = sources();
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();
        health.record_success("src-edge-dc1");
        let secrets = MockSecrets::new().with("cred-edge", [("token", "edge-key")]);
        let topology = describe(
            Inputs {
                sources: &sources,
                cache: &cache,
                health: &health,
                secrets: &secrets,
                peers: Arc::new(Peers::default()),
                name: None,
            },
            |_| true,
            0,
        )
        .await;
        assert_eq!(topology.peers[0].sources[0].last_pull_age_seconds, Some(0));
        assert_eq!(topology.peers[1].sources[0].last_pull_age_seconds, None);
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
                peers: Arc::new(Peers::default()),
                name: None,
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

    // Every peer that answered is told the whole map and the address it was
    // reached at; the one that did not answer is not pushed to at all.
    #[tokio::test]
    async fn the_map_is_pushed_to_every_peer_that_answered() {
        let sources = sources();
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();
        let secrets = MockSecrets::new().with("cred-edge", [("token", "edge-key")]);
        let peers = Arc::new(Peers::default());
        let outcomes = push_federation(
            Inputs {
                sources: &sources,
                cache: &cache,
                health: &health,
                secrets: &secrets,
                peers: Arc::clone(&peers) as Arc<dyn PeerPort>,
                name: None,
            },
            Some("https://central.example"),
            6,
        )
        .await;

        assert_eq!(outcomes.len(), 1);
        assert_eq!(
            outcomes[0],
            ("https://dc1.example:8443".to_string(), Ok(()))
        );
        let pushed = peers.pushed.lock().unwrap();
        assert_eq!(pushed[0].url, "https://dc1.example:8443");
        assert_eq!(
            pushed[0].sender_url.as_deref(),
            Some("https://central.example")
        );
        assert_eq!(pushed[0].sender.sources, ["src-edge-dc1", "src-edge-dc2"]);
        assert_eq!(pushed[0].sender.endpoints, 6);
        // Nothing cached yet: no hosts, and no age to report.
        assert_eq!(pushed[0].sender.hosts, 0);
        assert_eq!(pushed[0].sender.oldest_age_seconds, None);
        assert_eq!(pushed[0].topology.peers.len(), 2);
    }

    // An edge's admins are not the central's: the map pushed to dc1 holds no
    // peer's own map (asked with the central's keys) and no error naming the
    // central's secrets — neither a peer's "not asked" reason nor a link's
    // last sync error.
    #[tokio::test]
    async fn the_pushed_map_is_redacted_for_the_peer() {
        struct Deep {
            pushed: Mutex<Vec<FederationPush>>,
        }
        impl PeerPort for Deep {
            fn describe(&self, _request: PeerRequest) -> PeerFuture {
                Box::pin(async {
                    Ok(Topology {
                        name: Some("dc1".to_string()),
                        version: "0.35.0".to_string(),
                        sources: Some(7),
                        peers: vec![Peer {
                            url: "https://behind-dc1.example".to_string(),
                            sources: Vec::new(),
                            instance: None,
                            error: Some("unreachable".to_string()),
                        }],
                    })
                })
            }
            fn push(&self, _request: PeerRequest, body: FederationPush) -> PushFuture {
                self.pushed.lock().unwrap().push(body);
                Box::pin(async { Ok(()) })
            }
        }

        let mut sources = sources();
        let mut dc3 = remote("https://dc3.example:8443");
        dc3.credential_ids = vec!["cred-missing".to_string()];
        sources.insert("src-edge-dc3".to_string(), dc3);
        let cache = MemoryCache::new();
        let health = SyncHealthRegistry::new();
        health.record_failure("src-edge-dc1", "Env var 'CENTRAL_DC1_TOKEN' not set");
        let secrets = MockSecrets::new().with("cred-edge", [("token", "edge-key")]);
        let peers = Arc::new(Deep {
            pushed: Mutex::new(Vec::new()),
        });
        let outcomes = push_federation(
            Inputs {
                sources: &sources,
                cache: &cache,
                health: &health,
                secrets: &secrets,
                peers: Arc::clone(&peers) as Arc<dyn PeerPort>,
                name: None,
            },
            None,
            0,
        )
        .await;
        assert_eq!(outcomes.len(), 2, "dc1 and dc2 answered; dc3 was not asked");

        let pushed = peers.pushed.lock().unwrap();
        let json = serde_json::to_string(&pushed[0].topology).unwrap();
        assert!(!json.contains("CENTRAL_DC1_TOKEN"), "{json}");
        assert!(!json.contains("cred-missing"), "{json}");
        assert!(!json.contains("behind-dc1"), "{json}");
        let dc1 = &pushed[0].topology.peers[0];
        let instance = dc1.instance.as_ref().unwrap();
        assert_eq!(instance.version, "0.35.0");
        assert!(instance.peers.is_empty() && instance.sources.is_none());
        assert_eq!(
            dc1.sources[0].consecutive_failures, 1,
            "a failing link still shows"
        );
        assert!(dc1.sources[0].last_error.is_some());
        let dc3 = &pushed[0].topology.peers[2];
        assert_eq!(
            dc3.error.as_deref(),
            Some("not asked: this instance cannot authenticate to it")
        );
    }

    // Two sources read dc1 with different keys: the map asks with the first
    // by id, and the push must use that same key, however the HashMap of
    // sources happens to iterate.
    #[tokio::test]
    async fn the_push_uses_the_key_the_map_asked_with() {
        #[derive(Default)]
        struct Keys {
            asked: Mutex<Vec<Option<String>>>,
            pushed: Mutex<Vec<Option<String>>>,
        }
        impl PeerPort for Keys {
            fn describe(&self, request: PeerRequest) -> PeerFuture {
                self.asked.lock().unwrap().push(request.api_key);
                Box::pin(async {
                    Ok(Topology {
                        name: None,
                        version: "0.35.0".to_string(),
                        sources: Some(1),
                        peers: Vec::new(),
                    })
                })
            }
            fn push(&self, request: PeerRequest, _body: FederationPush) -> PushFuture {
                self.pushed.lock().unwrap().push(request.api_key);
                Box::pin(async { Ok(()) })
            }
        }

        // A fresh HashMap per round: each gets its own iteration order.
        for _ in 0..16 {
            let mut first = remote("https://dc1.example:8443");
            first.credential_ids = vec!["cred-a".to_string()];
            let mut second = remote("https://dc1.example:8443/");
            second.credential_ids = vec!["cred-b".to_string()];
            let sources: HashMap<String, Source> = [
                ("src-dc1-b".to_string(), second),
                ("src-dc1-a".to_string(), first),
            ]
            .into_iter()
            .collect();
            let cache = MemoryCache::new();
            let health = SyncHealthRegistry::new();
            let secrets = MockSecrets::new()
                .with("cred-a", [("token", "key-a")])
                .with("cred-b", [("token", "key-b")]);
            let peers = Arc::new(Keys::default());
            push_federation(
                Inputs {
                    sources: &sources,
                    cache: &cache,
                    health: &health,
                    secrets: &secrets,
                    peers: Arc::clone(&peers) as Arc<dyn PeerPort>,
                    name: None,
                },
                None,
                0,
            )
            .await;
            let key_a = [Some("key-a".to_string())];
            assert_eq!(*peers.asked.lock().unwrap(), key_a);
            assert_eq!(*peers.pushed.lock().unwrap(), key_a);
        }
    }

    #[test]
    fn the_store_keeps_one_map_per_sender() {
        let store = FederationStore::new();
        let push = |url: &str| FederationPush {
            url: url.to_string(),
            sender_url: None,
            sender: InstanceSummary::default(),
            topology: Topology {
                name: None,
                version: "0.35.0".to_string(),
                sources: Some(1),
                peers: Vec::new(),
            },
        };
        store.record("key-central".to_string(), push("https://old"));
        store.record("key-central".to_string(), push("https://new"));
        store.record("key-other".to_string(), push("https://x"));
        let list = store.list();
        assert_eq!(list.len(), 2);
        let central = list.iter().find(|f| f.pushed_by == "key-central").unwrap();
        assert_eq!(central.url, "https://new");
    }
}
