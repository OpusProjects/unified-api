use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

use crate::domain::dataset::Dataset;
use crate::domain::source::OutputFormat;
use crate::ports::connector::{
    ConnectorError, ConnectorOutput, ConnectorPort, ConnectorResult, DatasetAges,
};

// Federation connector: another unified-api instance is the source.
//
//   central sources.yaml:
//     src-madrid:
//       connector_type: "remote"
//       script_path: "src-ssh"          # source id ON THE REMOTE instance
//       credential_ids: ["cred-edge"]   # token credential = remote API key
//       config:
//         url: "https://unified-api-mad.example.com"
//
// GET {url}/api/v1/sources/{id}/dataset returns exactly the Dataset shape a
// connector must produce — the API itself is the federation protocol. A
// second call to /status recovers how old the data already is at the origin
// (dataset age + per-host ages) so the local cache entry can be built with
// truthful freshness instead of pretending the data was born on transfer.
pub struct RemoteConnector;

impl Default for RemoteConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteConnector {
    pub fn new() -> Self {
        Self
    }
}

// The slice of the remote /status response we care about (its full schema
// lives in the remote's OpenAPI; unknown fields are ignored on purpose so
// versions can drift).
#[derive(Deserialize)]
struct RemoteStatus {
    dataset_age_seconds: u64,
    #[serde(default)]
    hosts: Vec<RemoteHostStatus>,
}

#[derive(Deserialize)]
struct RemoteHostStatus {
    hostname: String,
    age_seconds: u64,
}

impl ConnectorPort for RemoteConnector {
    fn execute(
        &self,
        // For this connector the "script" is the source id on the remote side
        script_path: &str,
        _args: &[String],
        _output_format: OutputFormat,
        config: &HashMap<String, String>,
        credentials: &HashMap<String, String>,
    ) -> Pin<Box<dyn Future<Output = ConnectorResult> + Send + '_>> {
        let remote_source = script_path.to_string();
        let config = config.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            let base_url = config
                .get("url")
                .map(|u| u.trim_end_matches('/').to_string())
                .ok_or_else(|| connector_error("remote connector requires 'url' in config"))?;

            let http_timeout: u64 = config
                .get("http_timeout_seconds")
                .and_then(|t| t.parse().ok())
                .unwrap_or(30);

            // Self-signed certs happen in real infra; opt-in, never default
            let insecure = config.get("insecure_tls").is_some_and(|v| v == "true");

            // The remote API key comes from a `token` credential
            let api_key = credentials
                .get("token")
                .or_else(|| credentials.get("TOKEN"))
                .or_else(|| credentials.get("api_key"))
                .cloned();

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(http_timeout))
                .danger_accept_invalid_certs(insecure)
                .build()
                .map_err(|e| connector_error(&format!("failed to build HTTP client: {}", e)))?;

            // A host-scoped sync asks the remote for those hosts only. Without
            // this the central pulled the whole dataset across the WAN (megabytes
            // on a facts source) to keep the one host it was asked for.
            //
            // The remote answers a filtered `/dataset` with its paginated
            // envelope rather than a bare Dataset, which deserializes into
            // Dataset all the same: `hostvars` and `groups` are there under the
            // same names and the envelope's extra fields are ignored.
            let host_filter = host_scope_filter(&config);

            // 1. The data
            let dataset_url = format!(
                "{}/api/v1/sources/{}/dataset{}",
                base_url, remote_source, host_filter
            );
            let response = get(&client, &dataset_url, &api_key).await?;
            let dataset: Dataset = response.json().await.map_err(|e| {
                connector_error(&format!(
                    "remote dataset from '{}' is not valid Dataset JSON: {}",
                    dataset_url, e
                ))
            })?;

            // 2. The truth about its age. Failing to get it degrades to
            // "fresh as of now" with a warning — data beats metadata.
            let status_url = format!(
                "{}/api/v1/sources/{}/status{}",
                base_url, remote_source, host_filter
            );
            let ages = match fetch_ages(&client, &status_url, &api_key).await {
                Ok(ages) => Some(ages),
                Err(e) => {
                    warn!(
                        url = %status_url,
                        error = %e.message,
                        "could not read remote ages — treating the dataset as fresh"
                    );
                    None
                }
            };

            debug!(
                url = %base_url,
                source = %remote_source,
                hosts = dataset.hostvars.len(),
                age_propagated = ages.is_some(),
                "Remote dataset fetched"
            );

            Ok(ConnectorOutput { dataset, ages })
        })
    }
}

// The `?host=` query string to append to the remote calls, or "" for a full
// sync. Only host scope is translated: the remote has no group filter that
// would also narrow what a group-scoped sync needs, and a full sync wants
// everything by definition.
//
// Hostnames are assumed identical on both sides of a federation link, which is
// what the rest of the chain already relies on (hosts_from_source keys its
// results by the inventory hostname). Percent-encoding is not applied because a
// hostname that needed it would not be a hostname.
fn host_scope_filter(config: &HashMap<String, String>) -> String {
    if config.get("scope").map(String::as_str) != Some("host") {
        return String::new();
    }

    match config.get("target") {
        Some(target) if !target.is_empty() => format!("?host={}", target),
        _ => String::new(),
    }
}

async fn fetch_ages(
    client: &reqwest::Client,
    url: &str,
    api_key: &Option<String>,
) -> Result<DatasetAges, ConnectorError> {
    let response = get(client, url, api_key).await?;
    let status: RemoteStatus = response
        .json()
        .await
        .map_err(|e| connector_error(&format!("remote status is not valid JSON: {}", e)))?;

    Ok(DatasetAges {
        dataset_age_seconds: status.dataset_age_seconds,
        host_ages: status
            .hosts
            .into_iter()
            .map(|h| (h.hostname, h.age_seconds))
            .collect(),
    })
}

async fn get(
    client: &reqwest::Client,
    url: &str,
    api_key: &Option<String>,
) -> Result<reqwest::Response, ConnectorError> {
    let mut request = client.get(url);
    if let Some(key) = api_key {
        request = request.header("x-api-key", key);
    }

    let response = request
        .send()
        .await
        .map_err(|e| connector_error(&format!("request to '{}' failed: {}", url, e)))?;

    match response.status().as_u16() {
        200 => Ok(response),
        401 => Err(connector_error(&format!(
            "'{}' answered 401 — is the token credential the remote API key?",
            url
        ))),
        403 => Err(connector_error(&format!(
            "'{}' answered 403 — the remote key is not allowed to read that source",
            url
        ))),
        404 => Err(connector_error(&format!(
            "'{}' answered 404 — does that source exist (and has it synced) on the remote?",
            url
        ))),
        other => Err(connector_error(&format!(
            "'{}' answered HTTP {}",
            url, other
        ))),
    }
}

fn connector_error(message: &str) -> ConnectorError {
    ConnectorError {
        message: message.to_string(),
        stderr: String::new(),
        exit_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn full_sync_asks_for_everything() {
        assert_eq!(host_scope_filter(&config(&[])), "");
    }

    #[test]
    fn host_scope_becomes_a_host_query() {
        let cfg = config(&[("scope", "host"), ("target", "web01.mad.example.com")]);
        assert_eq!(host_scope_filter(&cfg), "?host=web01.mad.example.com");
    }

    #[test]
    fn host_scope_passes_a_comma_list_through() {
        let cfg = config(&[("scope", "host"), ("target", "a.example,b.example")]);
        assert_eq!(host_scope_filter(&cfg), "?host=a.example,b.example");
    }

    #[test]
    fn group_scope_is_not_translated() {
        let cfg = config(&[("scope", "group"), ("target", "madrid")]);
        assert_eq!(host_scope_filter(&cfg), "");
    }

    #[test]
    fn host_scope_without_a_target_asks_for_everything() {
        // Rather than emitting `?host=`, which the remote would read as a
        // filter matching nothing and answer with an empty dataset
        let cfg = config(&[("scope", "host")]);
        assert_eq!(host_scope_filter(&cfg), "");
    }
}
