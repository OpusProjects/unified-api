use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::application::refresh::RefreshCoordinator;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::project::GitProject;
use crate::domain::source::{ConnectorType, Source};
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::view::View;
use crate::ports;

// The shared application state: the ports (as Arc<dyn Trait>, so handlers
// depend on the interface, not the implementation) plus the static
// configuration loaded at startup.
//
// Arc = Atomic Reference Counted — a reference-counted pointer shared across
// threads; each axum handler receives a cheap clone of the same AppState.
pub struct AppState {
    pub cache: Arc<dyn ports::cache::CachePort>,
    pub connector: Arc<dyn ports::connector::ConnectorPort>,
    pub ssh_connector: Arc<dyn ports::connector::ConnectorPort>,
    pub static_connector: Arc<dyn ports::connector::ConnectorPort>,
    pub remote_connector: Arc<dyn ports::connector::ConnectorPort>,
    pub enricher: Arc<dyn ports::enricher::EnricherPort>,
    pub output: Arc<dyn ports::output::OutputPort>,
    pub secrets: Arc<dyn ports::secrets::SecretsPort>,
    pub git: Arc<dyn ports::git::GitPort>,
    pub sources: HashMap<String, Source>,
    // Read-only composites over sources, served on the same routes and sharing
    // the same id space (config validation rejects a collision). They hold no
    // cache entry of their own — a view is resolved from its members on every
    // read, which is what keeps it from being a third copy of the inventory.
    pub views: HashMap<String, View>,
    pub enrichers: HashMap<String, Enricher>,
    pub endpoints: HashMap<String, OutputEndpoint>,
    pub projects: HashMap<String, GitProject>,
    pub projects_dir: PathBuf,
    // Why each source's data looks the way it does (last attempt, last
    // success, last error). Not a port: it is in-process state with no
    // outside world behind it, like the cache's contents.
    pub sync_health: Arc<SyncHealthRegistry>,
    // The same record for the OTHER periodic work, one registry per kind so
    // the id spaces cannot collide: enricher runs (keyed by enricher id),
    // project pulls (keyed by project id) and the cache snapshot task (a
    // single well-known key). Before these, a permanently broken enricher, a
    // project stuck on a stale commit, or a full disk killing persistence
    // were warn! lines and nothing else.
    pub enrich_health: Arc<SyncHealthRegistry>,
    pub project_health: Arc<SyncHealthRegistry>,
    pub snapshot_health: Arc<SyncHealthRegistry>,
    // Coalescing and limits for reads that are allowed to refresh before
    // answering. In-process state like sync_health: no outside world behind it.
    pub refresh: Arc<RefreshCoordinator>,
    // Serialises syncs of one source, so a manual sync landing mid-way through a
    // scheduled one cannot let the older gather win. In-process state like the
    // two above.
    pub syncs: Arc<crate::application::sync::SyncCoordinator>,
    // /readyz turns green only when every configured source has synced, rather
    // than when at least one has (see config::ServerConfig)
    pub readyz_require_all_sources: bool,
}

impl AppState {
    // The enrichment dependencies a sync needs, borrowed from the state that
    // already owns them. Handlers and the scheduler call this rather than
    // assembling the struct themselves, so there is one place to change if it
    // ever needs more.
    pub fn enrichment(&self) -> crate::application::sync::Enrichment<'_> {
        crate::application::sync::Enrichment {
            port: &*self.enricher,
            enrichers: &self.enrichers,
            health: &self.enrich_health,
            projects_dir: &self.projects_dir,
        }
    }

    // The source as a sync should execute it: script_path resolved into the
    // project checkout when the file is there (SSH sources excepted — their
    // script_path is a REMOTE command). Resolved per sync rather than once at
    // boot, so a checkout that appears after startup — a slow clone, a
    // pipeline's first push — is picked up by the next run without a restart.
    // The cost is one Source clone and one stat() next to spawning a process.
    //
    // None = the id is not in sources.yaml, same contract as sources.get().
    pub fn source_for_sync(&self, id: &str) -> Option<crate::domain::source::Source> {
        let mut source = self.sources.get(id)?.clone();
        if !matches!(source.connector_type, ConnectorType::Ssh) {
            source.script_path = crate::application::scripts::resolve_script_path(
                &self.projects_dir,
                id,
                &source.project_id,
                &source.script_path,
            );
        }
        Some(source)
    }
    // Chooses the appropriate connector based on the type declared in the source
    pub fn connector_for(
        &self,
        connector_type: &ConnectorType,
    ) -> &Arc<dyn ports::connector::ConnectorPort> {
        match connector_type {
            ConnectorType::Script => &self.connector,
            ConnectorType::Ssh => &self.ssh_connector,
            ConnectorType::StaticInventory => &self.static_connector,
            ConnectorType::Remote => &self.remote_connector,
        }
    }
}
