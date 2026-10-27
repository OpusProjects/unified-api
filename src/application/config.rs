use std::collections::HashMap;
use std::sync::Arc;

use tracing::info;

use crate::config::{AppConfig, RestartOnlySettings};
use crate::{AppState, RuntimeConfig};

// The use case "adopt this configuration without restarting".
//
// What a reload can and cannot do is a property of WHERE each setting is
// read. Anything read per request or per tick — sources, views, enrichers,
// endpoints, projects, credentials — can be swapped, because the next reader
// simply reads the new value. Anything consumed once, at construction — the
// bound socket, the router's CORS and metrics-auth layers, the snapshot
// task's path, the refresh semaphore — cannot, because the thing it built is
// already running. The first set is applied here; the second set is diffed
// and reported (see RestartOnlySettings), never silently dropped.

// Removes every cache entry whose source is not configured, and says which.
// A cache entry only ever belongs to a source (views hold none), so an
// entry without one is left over: from a snapshot written before the
// source was removed, or from a sync that finished after it was.
pub fn drop_unconfigured_entries(state: &AppState) -> Vec<String> {
    let config = state.config();
    let mut dropped: Vec<String> = state
        .cache
        .keys()
        .into_iter()
        .filter(|id| !config.sources.contains_key(id))
        .collect();
    dropped.sort();
    for id in &dropped {
        state.cache.remove(id);
    }
    if !dropped.is_empty() {
        info!(
            sources = ?dropped,
            "Cache entries dropped: their sources are no longer configured"
        );
    }
    dropped
}

// What changed in one section of the configuration, by id.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SectionDelta {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub changed: Vec<String>,
}

impl SectionDelta {
    pub fn between<T: PartialEq>(old: &HashMap<String, T>, new: &HashMap<String, T>) -> Self {
        let mut delta = Self::default();
        for (id, value) in new {
            match old.get(id) {
                None => delta.added.push(id.clone()),
                Some(previous) if previous != value => delta.changed.push(id.clone()),
                Some(_) => {}
            }
        }
        for id in old.keys() {
            if !new.contains_key(id) {
                delta.removed.push(id.clone());
            }
        }
        delta.added.sort();
        delta.removed.sort();
        delta.changed.sort();
        delta
    }

    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn total(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }
}

// What a reload did, in enough detail that a pipeline can log it and an
// operator can tell "nothing changed" from "nothing was applied".
#[derive(Debug, Default)]
pub struct ReloadReport {
    pub generation: u64,
    // Section names that actually changed and are now live
    pub applied: Vec<String>,
    // config.yaml keys that differ from what this process was built with, and
    // therefore need a restart to take effect
    pub restart_required: Vec<String>,
    pub sources: SectionDelta,
    pub connectors: SectionDelta,
    pub views: SectionDelta,
    pub enrichers: SectionDelta,
    pub endpoints: SectionDelta,
    pub projects: SectionDelta,
    pub credentials: SectionDelta,
}

impl ReloadReport {
    pub fn changed_anything(&self) -> bool {
        !self.applied.is_empty()
    }
}

// What a reload WOULD do, without doing any of it. This is what makes "the
// files were written but nobody reloaded" a visible state rather than a
// surprise three hours later.
pub fn pending(state: &AppState, cfg: &AppConfig) -> ReloadReport {
    diff(
        &state.config(),
        &runtime_config(cfg),
        cfg,
        state.live_settings(),
    )
}

// Swap in everything AppState owns, and tell the scheduler to restart its
// tasks against it.
//
// The order matters in one place: the secrets chain is replaced BEFORE the
// sources are, so a source that arrives in this reload and names a credential
// that arrives with it can resolve on its very first sync rather than on the
// one after.
pub fn apply(state: &AppState, cfg: &AppConfig) -> ReloadReport {
    state
        .secrets_swap
        .replace(crate::adapters::out::secrets::build_chain(cfg));

    let next = Arc::new(runtime_config(cfg));
    let previous = state.swap_config(Arc::clone(&next));

    // The refresh coordinator holds a live semaphore and budget rather than
    // reading the snapshot, so a change is pushed onto it. In-flight refreshes
    // finish under the limits they started with (see RefreshCoordinator::resize).
    if previous.refresh_max_concurrent != next.refresh_max_concurrent
        || previous.refresh_timeout_seconds != next.refresh_timeout_seconds
    {
        state
            .refresh
            .resize(next.refresh_max_concurrent, next.refresh_timeout_seconds);
    }

    // A session carries the permissions its rules granted at login. Changed
    // rules (or a different directory) must not leave people working under
    // the old ones until their session expires: those sessions end, and the
    // next login is judged by the new rules. API key sessions are not
    // touched here; the key swap revokes those.
    if previous.ldap != next.ldap {
        let ended = state.sessions.end_directory_sessions();
        if ended > 0 {
            info!(
                ended,
                "LDAP configuration changed; its browser sessions ended"
            );
        }
    }

    // A source this configuration no longer has keeps no data: its entry
    // would otherwise go on being listed, served and snapshotted, and
    // restored on every boot. (A sync of it still in flight can write it
    // back; the scheduler prunes again once the old generation has drained.)
    drop_unconfigured_entries(state);

    let mut report = diff(&previous, &next, cfg, state.live_settings());

    // Remember what this reload could NOT adopt, so the scrape-time gauge
    // (unified_api_config_restart_required) answers for the whole fleet what
    // GET /api/v1/config answers for one pod.
    state.set_restart_pending(report.restart_required.clone());

    // Last, so the tasks restart against a state that is already fully
    // swapped — config and secrets both.
    report.generation = state.reload.bump();

    metrics::counter!("unified_api_config_reloads_total", "outcome" => "success").increment(1);
    info!(
        generation = report.generation,
        applied = ?report.applied,
        restart_required = ?report.restart_required,
        sources_changed = report.sources.total(),
        "Configuration reloaded"
    );

    report
}

fn runtime_config(cfg: &AppConfig) -> RuntimeConfig {
    RuntimeConfig {
        sources: cfg.sources.clone(),
        connectors: cfg.connectors.clone(),
        credentials: cfg.credentials.clone(),
        views: cfg.views.clone(),
        enrichers: cfg.enrichers.clone(),
        endpoints: cfg.endpoints.clone(),
        projects: cfg.projects.clone(),
        secrets: cfg.secrets_config.clone(),
        readyz_require_all_sources: cfg.server.readyz_require_all_sources,
        metrics_require_auth: cfg.server.metrics_require_auth,
        cors_allowed_origins: cfg.server.cors_allowed_origins.clone(),
        max_body_bytes: cfg.server.max_body_bytes,
        shutdown_grace_seconds: cfg.server.shutdown_grace_seconds,
        refresh_timeout_seconds: cfg.server.refresh_timeout_seconds,
        refresh_max_concurrent: cfg.server.refresh_max_concurrent,
        ldap: cfg.server.ui.ldap.clone(),
        ui_home: cfg.server.ui.home().to_string(),
        url: cfg
            .server
            .url
            .as_deref()
            .map(|url| crate::domain::topology::without_userinfo(url.trim_end_matches('/'))),
        name: cfg.server.name.clone(),
    }
}

// The whole comparison, in one place, so "what a reload would change" and
// "what a reload did change" can never answer differently.
fn diff(
    previous: &RuntimeConfig,
    next: &RuntimeConfig,
    cfg: &AppConfig,
    live: Option<RestartOnlySettings>,
) -> ReloadReport {
    let mut report = ReloadReport {
        sources: SectionDelta::between(&previous.sources, &next.sources),
        connectors: SectionDelta::between(&previous.connectors, &next.connectors),
        views: SectionDelta::between(&previous.views, &next.views),
        enrichers: SectionDelta::between(&previous.enrichers, &next.enrichers),
        endpoints: SectionDelta::between(&previous.endpoints, &next.endpoints),
        projects: SectionDelta::between(&previous.projects, &next.projects),
        credentials: SectionDelta::between(&previous.credentials, &next.credentials),
        ..ReloadReport::default()
    };

    let mut applied = Vec::new();
    for (name, delta) in [
        ("sources", &report.sources),
        ("connectors", &report.connectors),
        ("views", &report.views),
        ("enrichers", &report.enrichers),
        ("endpoints", &report.endpoints),
        ("projects", &report.projects),
        ("credentials", &report.credentials),
    ] {
        if !delta.is_empty() {
            applied.push(name.to_string());
        }
    }
    if previous.secrets != next.secrets {
        applied.push("secrets".to_string());
    }
    if previous.readyz_require_all_sources != next.readyz_require_all_sources {
        applied.push("server.readyz_require_all_sources".to_string());
    }
    if previous.metrics_require_auth != next.metrics_require_auth {
        applied.push("server.metrics_require_auth".to_string());
    }
    if previous.cors_allowed_origins != next.cors_allowed_origins {
        applied.push("server.cors_allowed_origins".to_string());
    }
    if previous.max_body_bytes != next.max_body_bytes {
        applied.push("server.max_body_bytes".to_string());
    }
    if previous.shutdown_grace_seconds != next.shutdown_grace_seconds {
        applied.push("server.shutdown_grace_seconds".to_string());
    }
    if previous.url != next.url {
        applied.push("server.url".to_string());
    }
    if previous.name != next.name {
        applied.push("server.name".to_string());
    }
    if previous.refresh_timeout_seconds != next.refresh_timeout_seconds {
        applied.push("server.refresh_timeout_seconds".to_string());
    }
    if previous.refresh_max_concurrent != next.refresh_max_concurrent {
        applied.push("server.refresh_max_concurrent".to_string());
    }
    if previous.ldap != next.ldap {
        applied.push("server.ui.ldap".to_string());
    }
    if previous.ui_home != next.ui_home {
        applied.push("server.ui.home".to_string());
    }
    applied.sort();
    report.applied = applied;

    // The live settings stay as main recorded them at boot, deliberately: a
    // reload does not make a changed port true, so the diff has to keep
    // reporting it until a restart actually adopts it.
    if let Some(live) = live {
        report.restart_required = live.changed_keys(&RestartOnlySettings::from_config(cfg));
    }

    report
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delta_names_what_arrived_left_and_changed() {
        let old: HashMap<String, u8> = [("a".into(), 1), ("b".into(), 2)].into_iter().collect();
        let new: HashMap<String, u8> = [("b".into(), 9), ("c".into(), 3)].into_iter().collect();

        let delta = SectionDelta::between(&old, &new);

        assert_eq!(delta.added, vec!["c"]);
        assert_eq!(delta.removed, vec!["a"]);
        assert_eq!(delta.changed, vec!["b"]);
        assert_eq!(delta.total(), 3);
    }

    #[test]
    fn an_identical_map_is_an_empty_delta() {
        let map: HashMap<String, u8> = [("a".into(), 1)].into_iter().collect();
        assert!(SectionDelta::between(&map, &map).is_empty());
    }
}
