use std::time::Duration;

use serde::Deserialize;

use crate::domain::topology::{FederationPush, Topology};
use crate::ports::peer::{PeerFuture, PeerPort, PeerRequest, PushError, PushFuture};

// PeerPort → the peer's own HTTP API, with the key the remote sources use.
//
// GET /api/v1/topology first. A peer older than that route answers 404; its
// index (GET /api/v1/, readable by any key) still says which version it
// runs, so the map shows it as present but unable to describe its peers.
//
// One client per TLS policy, kept for the life of the process rather than
// built per call: a client owns the connection pool, so a fresh one per
// call meant a fresh DNS lookup, TCP connect and TLS handshake to every peer
// on every page load. Kept, the connections stay open between loads (the
// page refreshes every minute, inside the pool's idle window), and a slow
// resolver is paid once rather than every time.
pub struct HttpPeers {
    verified: reqwest::Client,
    insecure: reqwest::Client,
}

impl HttpPeers {
    pub fn new() -> Self {
        let build = |insecure: bool| {
            reqwest::Client::builder()
                // Same opt-in as the remote connector's: self-signed edges exist
                .danger_accept_invalid_certs(insecure)
                .build()
                .expect("a client with no custom TLS roots cannot fail to build")
        };
        HttpPeers {
            verified: build(false),
            insecure: build(true),
        }
    }
}

impl Default for HttpPeers {
    fn default() -> Self {
        Self::new()
    }
}

impl PeerPort for HttpPeers {
    fn describe(&self, request: PeerRequest) -> PeerFuture {
        let client = if request.insecure_tls {
            self.insecure.clone()
        } else {
            self.verified.clone()
        };
        Box::pin(describe(client, request))
    }

    fn push(&self, request: PeerRequest, body: FederationPush) -> PushFuture {
        let client = if request.insecure_tls {
            self.insecure.clone()
        } else {
            self.verified.clone()
        };
        Box::pin(push(client, request, body))
    }
}

#[derive(Deserialize)]
struct Index {
    version: String,
}

// The client is a cheap handle onto the shared pool (an Arc inside), so
// cloning it per call keeps the connections. The timeout is per request,
// since it is the caller's (the page's) bound, not the pool's.
// The most a peer's answer may weigh. A topology is a few kilobytes; a peer
// that sends megabytes is broken or hostile, and is not read into memory.
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

// One budget for the whole exchange, the 404 fallback included: an old peer
// must not take twice the time the page was promised.
async fn describe(client: reqwest::Client, request: PeerRequest) -> Result<Topology, String> {
    let timeout = Duration::from_secs(request.timeout_seconds);
    match tokio::time::timeout(timeout, ask(&client, &request)).await {
        Ok(answer) => answer,
        Err(_) => Err("did not answer in time".to_string()),
    }
}

// The body, read a chunk at a time and refused past MAX_BODY_BYTES, then
// parsed: `.json()` would read any size whole first.
async fn read_json<T: serde::de::DeserializeOwned>(
    mut response: reqwest::Response,
    what: &str,
) -> Result<T, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("unreadable {what}: {e}"))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(format!(
                "{what} larger than {} MB",
                MAX_BODY_BYTES / 1024 / 1024
            ));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|e| format!("unreadable {what}: {e}"))
}

async fn ask(client: &reqwest::Client, request: &PeerRequest) -> Result<Topology, String> {
    let get = |path: String| {
        let mut call = client.get(format!("{}{path}", request.url));
        if let Some(key) = &request.api_key {
            call = call.header("x-api-key", key);
        }
        call.send()
    };

    let response = get(format!("/api/v1/topology?depth={}", request.depth))
        .await
        .map_err(|e| unreachable_message(&e))?;

    match response.status().as_u16() {
        200 => read_json::<Topology>(response, "topology").await,
        404 => {
            let index = get("/api/v1/".to_string())
                .await
                .map_err(|e| unreachable_message(&e))?;
            if !index.status().is_success() {
                return Err(format!("answered HTTP {}", index.status().as_u16()));
            }
            let index: Index = read_json(index, "index").await?;
            Ok(Topology {
                name: None,
                version: index.version,
                sources: None,
                peers: Vec::new(),
            })
        }
        401 => Err("refused the key (401)".to_string()),
        403 => Err("the key may not describe it (403)".to_string()),
        other => Err(format!("answered HTTP {other}")),
    }
}

async fn push(
    client: reqwest::Client,
    request: PeerRequest,
    body: FederationPush,
) -> Result<(), PushError> {
    let mut call = client
        .put(format!("{}/api/v1/topology/federation", request.url))
        .timeout(Duration::from_secs(request.timeout_seconds))
        .json(&body);
    if let Some(key) = &request.api_key {
        call = call.header("x-api-key", key);
    }
    let response = call
        .send()
        .await
        .map_err(|e| PushError::Failed(unreachable_message(&e)))?;
    match response.status().as_u16() {
        200..=299 => Ok(()),
        // Older than the route: a 404, or a 405 where only GET /topology
        // answers under that path's prefix.
        404 | 405 => Err(PushError::Unsupported),
        401 => Err(PushError::Failed("refused the key (401)".to_string())),
        other => Err(PushError::Failed(format!("answered HTTP {other}"))),
    }
}

// reqwest's error text for a timeout or a refused connection is long and
// nested; the map needs the gist.
fn unreachable_message(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "did not answer in time".to_string()
    } else if error.is_connect() {
        "unreachable".to_string()
    } else {
        format!("request failed: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Whether a dead address refuses at once or swallows the packets until
    // the timeout depends on the network (a WSL host does the latter); either
    // way the map must say the peer cannot be reached, not invent a status.
    #[tokio::test]
    async fn a_dead_address_reads_as_unreachable() {
        let result = HttpPeers::new()
            .describe(PeerRequest {
                url: "http://127.0.0.1:1".to_string(),
                api_key: None,
                insecure_tls: false,
                timeout_seconds: 1,
                depth: 1,
            })
            .await;
        let reason = result.unwrap_err();
        assert!(
            reason == "unreachable" || reason == "did not answer in time",
            "got: {reason}"
        );
    }
}
