use std::sync::OnceLock;

use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

// The metrics recorder is a process-wide global (like the tracing subscriber),
// so it can only be installed once. OnceLock makes repeated AppBuilder::build()
// calls — every integration test builds its own app — share the same recorder
// instead of failing on the second install.
static PROMETHEUS: OnceLock<PrometheusHandle> = OnceLock::new();

fn handle() -> &'static PrometheusHandle {
    PROMETHEUS.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("failed to install Prometheus metrics recorder")
    })
}

// GET /metrics — Prometheus text exposition format. Public like the health
// probes: scrapers don't carry the API key.
pub async fn metrics() -> String {
    handle().render()
}

// Called from the composition root so the recorder exists before the first
// sync runs (metrics recorded before install are silently dropped).
pub fn init() {
    handle();
}
