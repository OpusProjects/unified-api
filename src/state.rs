use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::application::refresh::RefreshCoordinator;
use crate::domain::credential::Credential;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::project::GitProject;
use crate::domain::source::{ConnectorType, Source};
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::view::View;
use crate::ports;

// The part of the configuration a running process can adopt without being
// restarted — everything that is only ever READ, per request or per tick.
//
// It used to be five plain fields on AppState, which made it structurally
// impossible to change any of it without a new process. Grouping it behind
// one Arc is what makes a reload a single pointer swap: a request that is
// already running keeps the snapshot it started with (so a sync cannot see
// half of an old configuration and half of a new one), and the next request
// picks up the new one.
pub struct RuntimeConfig {
    pub sources: HashMap<String, Source>,
    // Carried but never resolved from here — the secrets chain holds its own
    // copy. It lives in the snapshot so a reload can say WHICH credential
    // changed, which is the first question asked when a sync starts failing
    // right after a configuration push.
    pub credentials: HashMap<String, Credential>,
    // Read-only composites over sources, served on the same routes and sharing
    // the same id space (config validation rejects a collision). They hold no
    // cache entry of their own — a view is resolved from its members on every
    // read, which is what keeps it from being a third copy of the inventory.
    pub views: HashMap<String, View>,
    pub enrichers: HashMap<String, Enricher>,
    pub endpoints: HashMap<String, OutputEndpoint>,
    pub projects: HashMap<String, GitProject>,
    // Carried for the same reason as `credentials`: the chain is rebuilt from
    // it on every reload, and a reload that changed only the Vault address
    // should be able to say so.
    pub secrets: crate::config::SecretsConfig,
    // /readyz turns green only when every configured source has synced, rather
    // than when at least one has (see config::ServerConfig)
    pub readyz_require_all_sources: bool,
    // Whether /metrics needs an API key. Read by the metrics HANDLER on every
    // scrape (the route itself is always registered public), which is what
    // makes the flag reloadable — router placement could not be.
    pub metrics_require_auth: bool,
    // Read at a single moment — shutdown — so it reloads trivially: main takes
    // it from whatever snapshot is current when the drain starts.
    pub shutdown_grace_seconds: u64,
    // The on-demand refresh limits. The RefreshCoordinator holds the live
    // budget and semaphore; these are the declared values a reload diffs and
    // then re-applies onto it (application::config::apply calls resize).
    pub refresh_timeout_seconds: u64,
    pub refresh_max_concurrent: usize,
}

// Bumped on every reload, and watched by the scheduler: the tasks of the
// previous generation are told to finish and a fresh set is spawned from the
// new configuration. A watch channel rather than a callback because that is
// already how shutdown reaches those tasks — one wait point, two reasons to
// wake up.
pub struct ReloadNotifier {
    tx: tokio::sync::watch::Sender<u64>,
}

impl ReloadNotifier {
    pub fn new() -> Self {
        Self {
            tx: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.tx.subscribe()
    }

    pub fn generation(&self) -> u64 {
        *self.tx.borrow()
    }

    // send_replace rather than send: a process with no scheduler listening
    // (every test that builds a router) must still be able to reload, and
    // send() reports "no receivers" as an error.
    pub fn bump(&self) -> u64 {
        let next = self.generation() + 1;
        self.tx.send_replace(next);
        next
    }
}

impl Default for ReloadNotifier {
    fn default() -> Self {
        Self::new()
    }
}

// The shared application state: the ports (as Arc<dyn Trait>, so handlers
// depend on the interface, not the implementation) plus the configuration
// loaded at startup and, where the configuration API is enabled, replaced
// under the running process by a reload.
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
    pub venv: Arc<dyn ports::venv::VenvPort>,
    // The same object as `secrets` above, held once more as the narrow trait
    // that can REPLACE the chain: a reload rebuilds it from the new
    // credentials, and nothing else in the process has to know it happened.
    pub secrets_swap: Arc<dyn ports::secrets::SecretsSwapPort>,
    // The reloadable configuration. Private: reads go through config(), which
    // hands out an Arc snapshot rather than a lock guard — a handler holding a
    // guard across an await would block every reload behind its slowest
    // connector run.
    // pub(crate) only so AppBuilder can fill it — everything else reads it
    // through config() and writes it through swap_config().
    pub(crate) config: RwLock<Arc<RuntimeConfig>>,
    // Where the configuration directory is, and the settings the live process
    // was built from. Both only exist to answer a reload honestly: what is on
    // disk, and which of its keys this process cannot adopt without a restart.
    pub config_store: Option<Arc<dyn ports::config_store::ConfigStorePort>>,
    pub(crate) live_settings: RwLock<Option<crate::config::RestartOnlySettings>>,
    // The restart-only keys the last APPLIED reload changed, kept so /metrics
    // can export the count at scrape time: a pod running on a configuration
    // it could only partially adopt is otherwise visible only to whoever
    // queries GET /api/v1/config on that one pod. Empty at boot (the process
    // was just built from the directory) and replaced whole on every reload,
    // so a follow-up reload that reverts the change clears it.
    pub(crate) restart_pending: RwLock<Vec<String>>,
    pub reload: ReloadNotifier,
    // The checkout root. Restart-only: the boot clones, the snapshot of script
    // paths every execution resolves, and the project tasks all take it from
    // here, and moving it under a running process would strand every checkout
    // that is already on disk.
    pub projects_dir: PathBuf,
    // Why each source's data looks the way it does (last attempt, last
    // success, last error). Not a port: it is in-process state with no
    // outside world behind it, like the cache's contents.
    pub sync_health: Arc<SyncHealthRegistry>,
    // Last-known advertised scopes of remote member sources (see
    // domain::source::AdvertisedScopeRegistry): written by syncs, read by
    // view members with `advertised: true`. In-process state like the rest.
    pub advertised_scopes: Arc<crate::domain::source::AdvertisedScopeRegistry>,
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
    // Serialises syncs of one source (a manual sync landing mid-way through a
    // scheduled one cannot let the older gather win) and coalesces queued full
    // syncs onto one gather. In-process state like the two above.
    pub syncs: Arc<crate::application::sync::SyncCoordinator>,
    // Memoizes each view's host-union count for /metrics, invalidated by the
    // cache generation (see application::views::ViewHostsMemo for why).
    pub view_hosts_memo: crate::application::views::ViewHostsMemo,
}

impl AppState {
    // The configuration as of right now. An Arc clone, not a borrow: the
    // caller may hold it across await points, and while it does, a concurrent
    // reload swaps the pointer without disturbing it.
    pub fn config(&self) -> Arc<RuntimeConfig> {
        Arc::clone(&self.config.read().expect("runtime config lock"))
    }

    // Install a new configuration, returning the one it replaced so the caller
    // can report what actually changed. Does NOT notify the scheduler — that
    // is a separate step (see ReloadNotifier), because a reload swaps several
    // things and the tasks should restart once, at the end, against all of it.
    pub fn swap_config(&self, next: Arc<RuntimeConfig>) -> Arc<RuntimeConfig> {
        let mut guard = self.config.write().expect("runtime config lock");
        std::mem::replace(&mut guard, next)
    }

    // What the LIVE process was built from, for the keys a reload cannot
    // apply. None in tests and wherever main did not record it.
    pub fn live_settings(&self) -> Option<crate::config::RestartOnlySettings> {
        self.live_settings
            .read()
            .expect("live settings lock")
            .clone()
    }

    pub fn set_live_settings(&self, settings: crate::config::RestartOnlySettings) {
        *self.live_settings.write().expect("live settings lock") = Some(settings);
    }

    // The restart-only keys still awaiting a restart, as of the last applied
    // reload. GET /api/v1/config reports them by name; /metrics as a count.
    pub fn restart_pending(&self) -> Vec<String> {
        self.restart_pending
            .read()
            .expect("restart pending lock")
            .clone()
    }

    pub fn set_restart_pending(&self, keys: Vec<String>) {
        *self.restart_pending.write().expect("restart pending lock") = keys;
    }

    // The enrichment dependencies a sync needs, borrowed from the state that
    // already owns them. Handlers and the scheduler call this rather than
    // assembling the struct themselves, so there is one place to change if it
    // ever needs more.
    // Takes the configuration snapshot rather than reading it, so a sync and
    // the enrichment that follows it see the SAME generation even if a reload
    // lands in between.
    pub fn enrichment<'a>(
        &'a self,
        config: &'a RuntimeConfig,
    ) -> crate::application::sync::Enrichment<'a> {
        crate::application::sync::Enrichment {
            port: &*self.enricher,
            enrichers: &config.enrichers,
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
        let mut source = self.config().sources.get(id)?.clone();
        if !matches!(source.connector_type, ConnectorType::Ssh) {
            source.script_path = crate::application::scripts::resolve_script_path(
                &self.projects_dir,
                id,
                &source.project_id,
                &source.script_path,
            );
            // Same per-execution stance for the project's virtualenv: if one
            // exists on disk, the process adapter puts it on the child's PATH
            if let Some(bin) =
                crate::application::scripts::venv_bin_dir(&self.projects_dir, &source.project_id)
            {
                source
                    .config
                    .insert(crate::ports::venv::VENV_BIN_CONFIG_KEY.to_string(), bin);
            }
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
