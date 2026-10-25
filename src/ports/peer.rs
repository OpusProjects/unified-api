use std::future::Future;
use std::pin::Pin;

use crate::domain::topology::Topology;

// PeerPort — ask another unified-api instance to describe itself, for the
// federation map (GET /api/v1/topology). Read-only, and never on the data
// path: a peer that cannot be asked is shown as such, nothing fails.
pub trait PeerPort: Send + Sync {
    fn describe(&self, request: PeerRequest) -> PeerFuture;
}

pub type PeerFuture = Pin<Box<dyn Future<Output = Result<Topology, String>> + Send>>;

// Owned, so the requests to every peer can run concurrently on their own tasks.
pub struct PeerRequest {
    pub url: String,
    // The API key the sources reading from this peer already use.
    pub api_key: Option<String>,
    pub insecure_tls: bool,
    pub timeout_seconds: u64,
    // How many further hops the peer may take asking its own peers.
    pub depth: u8,
}
