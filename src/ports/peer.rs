use std::future::Future;
use std::pin::Pin;

use crate::domain::topology::{FederationPush, Topology};

// PeerPort — ask another unified-api instance to describe itself, for the
// federation map (GET /api/v1/topology). Read-only, and never on the data
// path: a peer that cannot be asked is shown as such, nothing fails.
pub trait PeerPort: Send + Sync {
    fn describe(&self, request: PeerRequest) -> PeerFuture;
    // Tell the peer what the federation looks like from here, so its own
    // map can show where it sits (PUT /api/v1/topology/federation).
    fn push(&self, request: PeerRequest, body: FederationPush) -> PushFuture;
}

pub type PeerFuture = Pin<Box<dyn Future<Output = Result<Topology, String>> + Send>>;
pub type PushFuture = Pin<Box<dyn Future<Output = Result<(), PushError>> + Send>>;

#[derive(Debug, PartialEq)]
pub enum PushError {
    // The peer predates the route: nothing wrong, it just cannot show it.
    Unsupported,
    Failed(String),
}

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
