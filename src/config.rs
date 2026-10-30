use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::domain::api_key::{ApiKeyDef, ApiKeyRole};
use crate::domain::connector::Connector;
use crate::domain::credential::{Credential, CredentialRules};
use crate::domain::endpoint::{BUILTIN_SETTINGS, OutputEndpoint, renamed_setting};
use crate::domain::enricher::Enricher;
use crate::domain::host::{GroupDef, HostDef};
use crate::domain::host_filter::HostFilter;
use crate::domain::project::GitProject;
use crate::domain::source::{Source, SourceDef};
use crate::domain::view::View;

// Which files make up a configuration directory, and the rule an item name or
// a path segment must meet: web-rs's, the same rule the config API applies.
pub use web_rs::config::{REQUIRED_CONFIG_FILE, config_files, is_config_path, is_safe_id};

pub struct AppConfig {
    pub server: ServerConfig,
    pub cache: CacheConfig,
    pub projects_config: ProjectsConfig,
    pub secrets_config: SecretsConfig,
    pub config_api: ConfigApiConfig,
    pub credentials: HashMap<String, Credential>,
    // Resolved: each source with its connector's settings folded in (see
    // SourceDef::resolve). The connectors themselves are kept too, for the
    // /connectors listing and for what a reload reports.
    pub sources: HashMap<String, Source>,
    pub connectors: HashMap<String, Connector>,
    // Per-host and per-group settings, already folded into the sources they
    // name (domain::host::fold_into_sources); kept for the counts that
    // --check-config, the startup log and the config API report.
    pub hosts: HashMap<String, HostDef>,
    pub groups: HashMap<String, GroupDef>,
    pub views: HashMap<String, View>,
    pub enrichers: HashMap<String, Enricher>,
    pub projects: HashMap<String, GitProject>,
    pub endpoints: HashMap<String, OutputEndpoint>,
    pub api_keys: HashMap<String, ApiKeyDef>,
}

impl AppConfig {
    // Forms this version still reads but a later one will refuse, each with
    // what to write instead: logged at startup and printed by
    // --check-config, so a configuration repository hears about it before
    // the release that stops accepting them.
    pub fn deprecations(&self) -> Vec<String> {
        let secrets = &self.secrets_config;
        let block = (secrets.cache_ttl_seconds.is_some() || secrets.vault.is_some()).then(|| {
            "config.yaml: the secrets block is deprecated and will be refused in a later \
             release — declare a Vault as a `type: vault` credential (address, mount, \
             env_vars: {token: <VAR>} or kubernetes_role, cache_ttl_seconds) and name it with \
             vault: on the credentials that read from it; env and file credentials need no cache"
                .to_string()
        });
        let mut notes: Vec<String> = self
            .credentials
            .iter()
            .filter_map(|(id, c)| c.deprecation(id))
            .chain(self.api_keys.iter().filter_map(|(id, k)| k.deprecation(id)))
            .chain(self.projects.iter().filter_map(|(id, p)| p.deprecation(id)))
            .chain(block)
            .collect();
        notes.sort();
        notes
    }
}

impl AppConfig {
    fn is_vault(&self, credential_id: &str) -> bool {
        self.credentials
            .get(credential_id)
            .is_some_and(|c| c.credential_type == crate::domain::credential::CredentialType::Vault)
    }
}

fn vault_as_login(who: &str, credential_id: &str) -> String {
    format!(
        "{who} uses credential '{credential_id}', which is a Vault connection: name the \
         credential that reads from it (its vault:) instead"
    )
}

// HTTP server configuration — config.yaml
//
// `deny_unknown_fields`, here and on every other struct that parses a config
// file: a key the schema does not know is a typo, and serde's default is to
// silently drop it and silently apply the field's default. For most settings
// that is a source syncing on an interval nobody chose; for a security setting
// (`metrics_require_auth`) it is failing open. Refusing to start, naming the
// key, is strictly better in every case. The pattern started on the view
// structs (see domain/view.rs) and proved itself there.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,

    // The browsable HTML API: content negotiation, the login form, browser
    // sessions and the form-action middleware. OFF by default and restart-only.
    //
    // Off by default because turning it on is a decision, not an upgrade. It
    // adds a publicly reachable login form, a cookie credential and an HTML
    // renderer to a service whose consumers are AWX and AnsibleForms; a
    // deployment that wants machine-to-machine only should not grow all that
    // because it took a new image. Restart-only because it decides which
    // middlewares and routes are in the router, and the router is built once.
    #[serde(default)]
    pub ui: UiConfig,

    // Origins allowed for CORS. Empty (the default) = no CORS headers at all,
    // which is right for server-to-server consumers. ["*"] = any origin.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    // What /readyz means. false (the default) = ready once ANY source has
    // synced, so a pod serving part of the inventory takes traffic instead of
    // waiting on the slowest source. true = every configured source must have
    // synced first, for deployments where a partial inventory is worse than
    // none (an AWX job that would run against half a datacenter).
    #[serde(default)]
    pub readyz_require_all_sources: bool,
    // Require an API key on /metrics. false (the default) keeps it public
    // alongside the health probes, which is what Prometheus scrape configs
    // expect. true is for a shared network: the exposition labels every
    // source id and host count, which describes the inventory topology to
    // anyone who can reach the port.
    #[serde(default)]
    pub metrics_require_auth: bool,

    // How long a read may wait for an on-demand refresh before it gives up and
    // serves what is cached. Separate from a source's `timeout_seconds`, which
    // bounds a scheduled sync and may reasonably be minutes: a consumer waiting
    // on a page cannot. Reaching it is not an error, the read still answers.
    #[serde(default = "default_refresh_timeout_seconds")]
    pub refresh_timeout_seconds: u64,

    // How many on-demand refreshes may run at once, process-wide. The TTL
    // window already limits repeat requests for the SAME host; this is what
    // limits requests for many DIFFERENT hosts arriving together, which the TTL
    // does not bound at all.
    #[serde(default = "default_refresh_max_concurrent")]
    pub refresh_max_concurrent: usize,

    // How long shutdown waits for the background tasks (syncs, enricher runs,
    // project pulls, the snapshot task) to finish their in-flight work before
    // writing the final cache snapshot anyway. Sized to fit inside a
    // Kubernetes terminationGracePeriodSeconds (default 30) with room for the
    // snapshot write itself; a sync that outlives it keeps the pre-drain
    // behavior — the snapshot is best-effort — rather than blocking exit.
    #[serde(default = "default_shutdown_grace_seconds")]
    pub shutdown_grace_seconds: u64,

    // Largest request body accepted, in bytes, on every route. This was
    // always enforced — axum ships a 2 MB default — but silently: nothing
    // declared it and an oversized push got a bare 413. Now the limit is a
    // named setting, and the 413 carries the standard error body naming it.
    // Size it for the biggest config file a pipeline pushes (a whole-directory
    // PUT is one body) or the largest host-vars document a consumer writes.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    // The address people and other instances reach this one at
    // (https://unified-api-central.example.com). The process only knows the
    // port it listens on; the real address is the ingress's. Optional, and
    // only used to tell the instances this one reads from where it is, so
    // their Topology page can link back to it.
    #[serde(default)]
    pub url: Option<String>,

    // What this instance is called where it is shown among others (the
    // Topology page): "Central", "CDI DC01". Optional; without it the UI
    // derives a name from the instance's address.
    #[serde(default)]
    pub name: Option<String>,
}

fn default_refresh_timeout_seconds() -> u64 {
    15
}

fn default_refresh_max_concurrent() -> usize {
    8
}

fn default_shutdown_grace_seconds() -> u64 {
    20
}

fn default_max_body_bytes() -> usize {
    // axum's own default, kept so making the limit explicit changes nothing
    // for existing deployments.
    2 * 1024 * 1024
}

// Cache behavior — config.yaml, `cache:` section (optional)
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CacheConfig {
    // Without a `persistence` block the cache is purely in-memory (the
    // original behavior): nothing is ever written to disk.
    #[serde(default)]
    pub persistence: Option<PersistenceConfig>,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PersistenceConfig {
    // Snapshot file, e.g. /var/lib/unified-api/cache.json
    pub path: String,

    // How often to write the snapshot (seconds)
    #[serde(default = "default_persistence_interval")]
    pub interval_seconds: u64,
}

fn default_persistence_interval() -> u64 {
    60
}

// Secrets behavior — config.yaml, `secrets:` section (optional, deprecated).
// The struct lives with the chain it configures, in secrets-rs.
pub use secrets_rs::SecretsConfig;

// Where git projects are cloned — config.yaml, `projects:` section (optional)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectsConfig {
    // Working directory for checkouts: one subdirectory per project id
    #[serde(default = "default_projects_dir")]
    pub dir: String,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            dir: default_projects_dir(),
        }
    }
}

fn default_projects_dir() -> String {
    "projects".to_string()
}

// Writing this directory over the API — config.yaml, `config_api:` section
// (optional)
#[derive(Deserialize, Default, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigApiConfig {
    // OFF by default, and deliberately not a thing that can be turned on from
    // the API itself: a running instance whose configuration directory is
    // read-only is the pre-existing behavior, and it stays the behavior for
    // every deployment that does not opt in. Turning it on means an admin key
    // can rewrite every file the loader reads — api_keys.yaml included, which
    // is the same authority as editing the directory the container mounts.
    #[serde(default)]
    pub enabled: bool,
}

// The configuration directory is laid out the way AWX config as code is:
// config.yaml at the root holds the server settings, and every other *.yaml /
// *.yml file, at any depth, holds ITEMS under a typed top-level key:
//
//   # sources/acme/src-d42-acme.yaml
//   sources:
//     - name: src-d42-acme
//       description: "Device42 Acme Inventory"
//       connector: d42
//       ...
//
// The top-level key says what an item is, never the folder: folders are for
// people (by team, by estate) and may nest as deep as they like. One item per
// file is the convention; a file may hold several, of several types. `name`
// is the item's identity: what other items reference, what URLs and cache
// keys use. `description` is the label people read.

// The top-level keys an item file may use.
pub const SECTIONS: [&str; 10] = [
    "sources",
    "hosts",
    "groups",
    "views",
    "endpoints",
    "enrichers",
    "connectors",
    "projects",
    "credentials",
    "api_keys",
];

// The settings a running process cannot adopt without being restarted, pulled
// out of AppConfig as plain comparable values.
//
// A reload swaps what it can (sources, views, enrichers, endpoints, projects,
// credentials, api keys, the refresh limits, the shutdown grace, the metrics
// auth flag, CORS, the body limit) and has to say something honest about the
// rest: the listener is already bound and the snapshot task already holds its
// path and interval. Silently ignoring those
// keys is how a pipeline comes to believe it changed a port it did not
// change — so they are diffed and NAMED in the reload report instead.
// The browsable API's settings.
#[derive(Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    #[serde(default)]
    pub enabled: bool,

    // How long a browser session lives. Fixed when the routes are built (the
    // login handler receives it as an extension), so it is restart-only and
    // listed in RestartOnlySettings: a reload that changes it says so.
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,

    // Log in with a directory account instead of pasting an API key. Read per
    // login from the configuration snapshot, so changing the server or the
    // access rules is a reload — which also ends the LDAP sessions opened
    // under the old rules (application::config::apply).
    #[serde(default)]
    pub ldap: Option<crate::domain::ldap::LdapConfig>,

    // Where a sign-in lands when nothing asked for a page: someone opening
    // /api/v1/login directly, or signing in again after signing out. Unset,
    // it is "/" when this instance serves the UI (`assets_dir`) and the API's
    // own browsable pages otherwise. Read per login, so it reloads.
    #[serde(default)]
    pub home: Option<String>,

    // A built unified-ui (the directory holding its index.html), served at
    // "/": the UI and the API on one address, no proxy in front. The image
    // ships one and sets this. Restart-only: the router is built once.
    #[serde(default)]
    pub assets_dir: Option<String>,
}

impl UiConfig {
    pub fn home(&self) -> &str {
        match (&self.home, &self.assets_dir) {
            (Some(home), _) => home,
            (None, Some(_)) => "/",
            (None, None) => "/api/v1/",
        }
    }
}

fn default_session_ttl_seconds() -> u64 {
    3600
}

// A path on this origin, safe to redirect to: web-rs's rule, the same the
// sign-in form applies to `next`.
pub use web_rs::url::is_local_path;

// By hand rather than derived: a derived Default gives a session TTL of zero
// (serde's default only applies when parsing), so a UiConfig built in code
// issued sessions that were expired the moment they were created.
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_ttl_seconds: default_session_ttl_seconds(),
            ldap: None,
            home: None,
            assets_dir: None,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct RestartOnlySettings {
    pub host: String,
    pub port: u16,
    pub persistence_path: Option<String>,
    pub persistence_interval_seconds: Option<u64>,
    pub projects_dir: String,
    pub config_api_enabled: bool,
    pub ui_enabled: bool,
    pub session_ttl_seconds: u64,
    pub ui_assets_dir: Option<String>,
}

impl RestartOnlySettings {
    pub fn from_config(cfg: &AppConfig) -> Self {
        Self {
            host: cfg.server.host.clone(),
            port: cfg.server.port,
            persistence_path: cfg.cache.persistence.as_ref().map(|p| p.path.clone()),
            persistence_interval_seconds: cfg
                .cache
                .persistence
                .as_ref()
                .map(|p| p.interval_seconds),
            projects_dir: cfg.projects_config.dir.clone(),
            config_api_enabled: cfg.config_api.enabled,
            ui_enabled: cfg.server.ui.enabled,
            session_ttl_seconds: cfg.server.ui.session_ttl_seconds,
            ui_assets_dir: cfg.server.ui.assets_dir.clone(),
        }
    }

    // The config.yaml keys that differ, named as they are written in the file
    // so the answer can be pasted into a search.
    pub fn changed_keys(&self, other: &Self) -> Vec<String> {
        let mut keys = Vec::new();
        let mut check = |differs: bool, key: &str| {
            if differs {
                keys.push(key.to_string());
            }
        };
        check(self.host != other.host, "server.host");
        check(self.port != other.port, "server.port");
        check(
            self.persistence_path != other.persistence_path
                || self.persistence_interval_seconds != other.persistence_interval_seconds,
            "cache.persistence",
        );
        check(self.projects_dir != other.projects_dir, "projects.dir");
        check(
            self.config_api_enabled != other.config_api_enabled,
            "config_api.enabled",
        );
        check(self.ui_enabled != other.ui_enabled, "server.ui.enabled");
        check(
            self.session_ttl_seconds != other.session_ttl_seconds,
            "server.ui.session_ttl_seconds",
        );
        check(
            self.ui_assets_dir != other.ui_assets_dir,
            "server.ui.assets_dir",
        );
        keys
    }
}

// Intermediate struct to parse config.yaml (server + optional sections)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    server: ServerConfig,
    #[serde(default)]
    cache: CacheConfig,
    #[serde(default)]
    projects: ProjectsConfig,
    #[serde(default)]
    secrets: SecretsConfig,
    #[serde(default)]
    config_api: ConfigApiConfig,
}

// Loads all configuration from a directory.
// Expects to find: config.yaml, credentials.yaml, sources.yaml, etc.
// Optional files are simply ignored if they do not exist.
impl AppConfig {
    // Fails with every problem at once (see validate_errors), so one
    // --check-config run — or one rejected API write — reports the whole list
    // instead of the first line of it.
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        let errors = self.validate_errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors::new(errors).into())
        }
    }

    // The same checks, as a list rather than as one joined string. The config
    // API hands this to the caller verbatim — a pipeline that pushed eight
    // files wants the problems as items it can render, not a blob it has to
    // split on "\n  - ".
    pub fn validate_errors(&self) -> Vec<String> {
        let mut errors: Vec<String> = Vec::new();

        if let Some(url) = &self.server.url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            errors.push(format!(
                "server.url '{url}' must be an http:// or https:// address"
            ));
        }

        // The deprecated config.yaml Vault: a 0 bound fails every request.
        if self
            .secrets_config
            .vault
            .as_ref()
            .is_some_and(|v| v.timeout_seconds == 0)
        {
            errors.push(
                "secrets.vault.timeout_seconds 0 would fail every request to that Vault — \
                 give it a bound in seconds (default 10)"
                    .to_string(),
            );
        }

        // A credential's or an API key's own shape: where its secret is read.
        for (id, credential) in &self.credentials {
            errors.extend(credential.errors(id));
        }
        for (id, key) in &self.api_keys {
            errors.extend(key.errors(id));
        }

        // Enrichers must reference existing sources
        for (id, enricher) in &self.enrichers {
            if self.views.contains_key(&enricher.target_id) {
                // An enricher WRITES into a cache entry, and a view has none:
                // it composes other sources' entries at read time. Enrich the
                // member instead and the view serves the result.
                errors.push(format!(
                    "Enricher '{}' targets view '{}' — a view has no cache entry to write \
                     into; target one of its member sources instead",
                    id, enricher.target_id
                ));
            } else if !self.sources.contains_key(&enricher.target_id) {
                errors.push(format!(
                    "Enricher '{}' references unknown target '{}'",
                    id, enricher.target_id
                ));
            }
            if let Some(ref source_id) = enricher.source_id
                && !self.sources.contains_key(source_id)
            {
                errors.push(format!(
                    "Enricher '{}' references unknown source '{}'",
                    id, source_id
                ));
            }
            if enricher.source_id.is_none() && enricher.script_path.is_none() {
                errors.push(format!(
                    "Enricher '{}' needs either source (declarative) or script_path (script)",
                    id
                ));
            }
        }

        // Endpoints must reference existing sources
        for (id, endpoint) in &self.endpoints {
            // Exactly one transformer: a builtin `output` or a `script_path`.
            match (endpoint.output.is_some(), endpoint.script_path.is_some()) {
                (false, false) => errors.push(format!(
                    "Endpoint '{}' needs either output (builtin) or script_path (script)",
                    id
                )),
                (true, true) => errors.push(format!(
                    "Endpoint '{}' sets both output and script_path — choose one",
                    id
                )),
                _ => {}
            }
            // Script-only knobs are a mistake with a builtin output, not a no-op.
            if endpoint.output.is_some() {
                if endpoint.project_id.is_some() {
                    errors.push(format!(
                        "Endpoint '{}' sets project, which a builtin output ignores",
                        id
                    ));
                }
                if !endpoint.script_args.is_empty() {
                    errors.push(format!(
                        "Endpoint '{}' sets script_args, which a builtin output ignores",
                        id
                    ));
                }
                if endpoint.timeout_seconds.is_some() {
                    errors.push(format!(
                        "Endpoint '{}' sets timeout_seconds, which a builtin output ignores \
                         — a builtin runs in-process, not under a script timeout",
                        id
                    ));
                }
                // A builtin reads exactly BUILTIN_SETTINGS; any other key in its
                // config would be silently ignored, which is how a typo or a name
                // from before 0.32 ends up serving an unfiltered inventory.
                let mut keys: Vec<&String> = endpoint.config.keys().collect();
                keys.sort();
                for key in keys {
                    if BUILTIN_SETTINGS.contains(&key.as_str()) {
                        continue;
                    }
                    let hint = renamed_setting(key)
                        .map(|new| format!(" — it is called {} now", new))
                        .unwrap_or_default();
                    errors.push(format!(
                        "Endpoint '{}' config has unknown setting '{}'{} (a builtin understands: {})",
                        id,
                        key,
                        hint,
                        BUILTIN_SETTINGS.join(", ")
                    ));
                }
                // A static filter is checked here too, so a malformed one fails
                // the load instead of every render.
                let host = endpoint
                    .config
                    .get("host")
                    .map(String::as_str)
                    .unwrap_or("");
                let filter = endpoint
                    .config
                    .get("filter")
                    .map(String::as_str)
                    .unwrap_or("");
                if let Err(e) = HostFilter::parse(host, filter) {
                    errors.push(format!("Endpoint '{}' config: {}", id, e));
                }
            }
            for source_id in &endpoint.source_ids {
                if self.views.contains_key(source_id) {
                    // Worth its own message: an endpoint script is fed whole
                    // cached datasets on stdin, and a view has no cache entry
                    // of its own — it composes other sources' entries at read
                    // time. Listing the members is what the operator wants.
                    errors.push(format!(
                        "Endpoint '{}' references view '{}' — output endpoints read cached \
                         sources, not views; list the view's member sources instead",
                        id, source_id
                    ));
                } else if !self.sources.contains_key(source_id) {
                    errors.push(format!(
                        "Endpoint '{}' references unknown source '{}'",
                        id, source_id
                    ));
                }
            }
            // A limit restricts the endpoint to part of what it merges, so the
            // source it names has to be part of what it merges. Naming one from
            // outside would silently intersect against data the endpoint never
            // reads (and never waits for), which renders as an empty inventory
            // with nothing to explain it.
            if let Some(limit) = &endpoint.limit {
                if limit.is_empty() {
                    errors.push(format!(
                        "Endpoint '{}' sets limit but names no rule — remove it, or set \
                         by_hosts_from_inventory",
                        id
                    ));
                }
                if let Some(from) = &limit.by_hosts_from_inventory
                    && !endpoint.source_ids.contains(from)
                {
                    errors.push(format!(
                        "Endpoint '{}' limits by hosts from '{}', which is not one of its \
                         sources — add it to sources",
                        id, from
                    ));
                }
            }
        }

        // Views: a read-only composite over sources. The rules exist because
        // every one of them turns into empty data or a wrong route at runtime
        // rather than into a visible failure.
        for (id, view) in &self.views {
            // Views are served on the /sources routes, so the two id spaces are
            // really one. A collision would silently shadow whichever the
            // handler happens to look up first.
            if self.sources.contains_key(id) {
                errors.push(format!(
                    "View '{}' has the same id as a source — views are served on the \
                     source routes, so the ids must not collide",
                    id
                ));
            }
            if view.members.is_empty() {
                errors.push(format!("View '{}' has no members", id));
            }

            let mut seen: HashSet<&String> = HashSet::new();
            for member in &view.members {
                if self.views.contains_key(&member.source) {
                    errors.push(format!(
                        "View '{}' has member '{}' which is another view — views do not nest",
                        id, member.source
                    ));
                } else if !self.sources.contains_key(&member.source) {
                    errors.push(format!(
                        "View '{}' references unknown source '{}'",
                        id, member.source
                    ));
                }
                // An advertised LOCAL member must have something to route by
                // at startup: its source's own claim, or declared fallback
                // groups/hosts. Only a REMOTE member may rely on the claim its
                // syncs will fetch — and even there a fallback is what covers
                // the window before the first sync. Without either, the
                // member claims nothing, ever, which is a config error worth
                // naming rather than an eternally empty slice of the view.
                if member.owns.advertised {
                    let member_source = self.sources.get(&member.source);
                    let is_remote = member_source.is_some_and(|source| {
                        source.connector_type == crate::domain::source::ConnectorType::Remote
                    });
                    let has_local_claim =
                        member_source.is_some_and(|source| source.advertised_scope().is_some());
                    let has_fallback =
                        !member.owns.groups.is_empty() || !member.owns.hosts.is_empty();
                    if !is_remote && !has_local_claim && !has_fallback {
                        errors.push(format!(
                            "View '{}' member '{}' uses advertised ownership, but the source \
                             advertises no scope and no fallback groups/hosts are declared — \
                             the member would claim nothing, ever",
                            id, member.source
                        ));
                    }
                }
                if !seen.insert(&member.source) {
                    errors.push(format!(
                        "View '{}' lists source '{}' twice — the second member could never \
                         win a host, since the first claim wins",
                        id, member.source
                    ));
                }
                if !self.sources.contains_key(&member.owns.source) {
                    errors.push(format!(
                        "View '{}' member '{}' resolves ownership against unknown source '{}'",
                        id, member.source, member.owns.source
                    ));
                }
            }
        }

        // Sources' credentials must exist, and be logins: a Vault connection
        // is read FROM, never handed to a script (its token would be).
        for (id, source) in &self.sources {
            for cred_id in &source.credential_ids {
                if !self.credentials.contains_key(cred_id) {
                    errors.push(format!(
                        "Source '{}' references unknown credential '{}'",
                        id, cred_id
                    ));
                } else if self.is_vault(cred_id) {
                    errors.push(vault_as_login(&format!("Source '{id}'"), cred_id));
                }
            }
            // Two credentials delivered under one id would overwrite each
            // other's CREDENTIAL_<ID>_<FIELD> values, silently.
            let mut by_id: HashMap<String, Vec<&String>> = HashMap::new();
            for cred_id in &source.credential_ids {
                by_id
                    .entry(crate::domain::credential::delivery_id(cred_id))
                    .or_default()
                    .push(cred_id);
            }
            let mut clashes: Vec<(String, Vec<&String>)> = by_id
                .into_iter()
                .filter(|(_, ids)| ids.iter().collect::<HashSet<_>>().len() > 1)
                .collect();
            clashes.sort();
            for (delivered, ids) in clashes {
                errors.push(format!(
                    "Source '{id}' uses credentials {} that are all delivered as \
                     CREDENTIAL_{}_* — rename one",
                    ids.iter()
                        .map(|c| format!("'{c}'"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    delivered.to_uppercase()
                ));
            }
            // The built-in ssh and remote connectors read one login's bare
            // fields (username and the key, the token). A second credential
            // sharing a field takes its bare name away, and they would
            // silently log in as root or send no token.
            let distinct: HashSet<&String> = source.credential_ids.iter().collect();
            if distinct.len() > 1
                && matches!(
                    source.connector_type,
                    crate::domain::source::ConnectorType::Ssh
                        | crate::domain::source::ConnectorType::Remote
                )
            {
                errors.push(format!(
                    "Source '{id}' lists {} credentials, but its {} connector logs in with \
                     one — keep one",
                    distinct.len(),
                    if source.connector_type == crate::domain::source::ConnectorType::Ssh {
                        "ssh"
                    } else {
                        "remote"
                    }
                ));
            }
        }

        // Projects must exist — the checkout of that project is where a
        // relative path resolves first. A connector names its own; of
        // the sources only a static_inventory one names a project (the others
        // took theirs from a connector already checked here, and reporting it
        // again per source would bury the one line that needs fixing).
        let mut connector_ids: Vec<&String> = self.connectors.keys().collect();
        connector_ids.sort();
        for id in connector_ids {
            if let Some(project) = &self.connectors[id].project
                && !self.projects.contains_key(project)
            {
                errors.push(format!(
                    "Connector '{}' references unknown project '{}'",
                    id, project
                ));
            }
        }
        for (id, source) in &self.sources {
            if source.connector_type == crate::domain::source::ConnectorType::StaticInventory
                && let Some(project) = &source.project_id
                && !self.projects.contains_key(project)
            {
                errors.push(format!(
                    "Source '{}' references unknown project '{}'",
                    id, project
                ));
            }
        }

        // hosts_from_source: only meaningful on SSH sources, must reference
        // an existing source (not itself), and conflicts with a static
        // config.hosts (which list would win?)
        for (id, source) in &self.sources {
            if let Some(ref hfs) = source.hosts_from_source {
                if source.connector_type != crate::domain::source::ConnectorType::Ssh {
                    errors.push(format!(
                        "Source '{}' sets hosts_from_source but is not an ssh source",
                        id
                    ));
                }
                if hfs.source == *id {
                    errors.push(format!(
                        "Source '{}' cannot use itself as hosts_from_source",
                        id
                    ));
                } else if !self.sources.contains_key(&hfs.source) {
                    errors.push(format!(
                        "Source '{}' references unknown source '{}' in hosts_from_source",
                        id, hfs.source
                    ));
                }
                if source.config.contains_key("hosts") {
                    errors.push(format!(
                        "Source '{}' sets both config.hosts and hosts_from_source — pick one",
                        id
                    ));
                }
            }
        }

        // An explicit advertise_scope must claim SOMETHING: an empty block is
        // one typo away from "claims everything", and the derivation already
        // has a spelled-out catch-all (an empty hosts_from_source pattern).
        for (id, source) in &self.sources {
            if let Some(scope) = &source.advertise_scope
                && scope.groups.is_empty()
                && scope.hosts.is_empty()
            {
                errors.push(format!(
                    "Source '{}' has an empty advertise_scope — name at least one \
                     group or host, or remove the block",
                    id
                ));
            }
        }

        // A cron schedule must parse, and a source cannot serve two masters:
        // schedule and sync_interval_seconds each define the cadence. The
        // field spent its first year as "reserved for future" and silently
        // ignored — precisely the config-that-does-nothing trap the strict
        // parsing exists to kill, so now that it works, junk in it fails loud.
        let mut check_cadence =
            |kind: &str, id: &str, schedule: &Option<String>, interval: Option<u64>| {
                if let Some(expression) = schedule {
                    if let Err(e) = crate::adapters::r#in::scheduler::parse_cron(expression) {
                        errors.push(format!(
                            "{} '{}' has an invalid cron schedule '{}': {}",
                            kind, id, expression, e
                        ));
                    }
                    if interval.is_some_and(|secs| secs > 0) {
                        errors.push(format!(
                            "{} '{}' sets both schedule and sync_interval_seconds — pick one",
                            kind, id
                        ));
                    }
                }
            };
        for (id, source) in &self.sources {
            check_cadence("Source", id, &source.schedule, source.sync_interval_seconds);
        }
        for (id, enricher) in &self.enrichers {
            check_cadence(
                "Enricher",
                id,
                &enricher.schedule,
                enricher.sync_interval_seconds,
            );
        }
        for (id, project) in &self.projects {
            check_cadence(
                "Project",
                id,
                &project.schedule,
                project.sync_interval_seconds,
            );
        }

        // ssh_known_hosts must name an existing file, checked at startup —
        // a typo'd path would otherwise fail every host of every sync at
        // runtime instead of failing the deploy.
        for (id, source) in &self.sources {
            if let Some(path) = source.config.get("ssh_known_hosts") {
                if source.connector_type != crate::domain::source::ConnectorType::Ssh {
                    errors.push(format!(
                        "Source '{}' sets ssh_known_hosts but is not an ssh source",
                        id
                    ));
                } else if !std::path::Path::new(path).is_file() {
                    errors.push(format!(
                        "Source '{}': ssh_known_hosts file '{}' does not exist",
                        id, path
                    ));
                }
            }
        }

        // Enrichers and endpoints with a project must reference an existing one
        for (id, enricher) in &self.enrichers {
            if let Some(ref project_id) = enricher.project_id
                && !self.projects.contains_key(project_id)
            {
                errors.push(format!(
                    "Enricher '{}' references unknown project '{}'",
                    id, project_id
                ));
            }
        }
        for (id, endpoint) in &self.endpoints {
            if let Some(ref project_id) = endpoint.project_id
                && !self.projects.contains_key(project_id)
            {
                errors.push(format!(
                    "Endpoint '{}' references unknown project '{}'",
                    id, project_id
                ));
            }
        }

        // Restricted API keys must reference existing sources and endpoints —
        // a typo'd id would otherwise just deny access with no explanation.
        // (Admin keys ignore the lists, so referencing anything is pointless
        // but harmless; only restricted keys are validated.)
        for (id, key) in &self.api_keys {
            if key.role == ApiKeyRole::Restricted {
                for source_id in &key.sources {
                    // A view is granted exactly like a source, by its id: it is
                    // served on the source routes and shares their id space.
                    // That is what decouples the two — a key granted the view
                    // needs no access to the members, because the members are
                    // internal topology and the view is the contract.
                    if !self.sources.contains_key(source_id) && !self.views.contains_key(source_id)
                    {
                        errors.push(format!(
                            "API key '{}' references unknown source or view '{}'",
                            id, source_id
                        ));
                    }
                }
                for endpoint_id in &key.endpoints {
                    if !self.endpoints.contains_key(endpoint_id) {
                        errors.push(format!(
                            "API key '{}' references unknown endpoint '{}'",
                            id, endpoint_id
                        ));
                    }
                }
            }
        }

        // A vault_path needs a Vault to read it from — caught at startup,
        // where the fix is obvious, not at the first sync that needs the
        // credential: the `type: vault` credential it names, or (deprecated)
        // config.yaml's secrets.vault when it names none.
        for (id, credential) in &self.credentials {
            if credential.vault_path.is_none() {
                continue;
            }
            match &credential.vault {
                Some(vault) if !self.credentials.contains_key(vault) => errors.push(format!(
                    "Credential '{id}' references unknown vault '{vault}'"
                )),
                Some(vault) if !self.is_vault(vault) => errors.push(format!(
                    "Credential '{id}' names '{vault}' as its vault, which is not a \
                     `type: vault` credential"
                )),
                Some(_) => {}
                None if self.secrets_config.vault.is_none() => errors.push(format!(
                    "Credential '{id}' sets vault_path but names no vault to read it from — \
                     write vault: <a type: vault credential>"
                )),
                None => {}
            }
        }

        // Private projects must reference existing credentials
        for (id, project) in &self.projects {
            errors.extend(project.errors(id));
            if let Some(cred_id) = project.credential_id() {
                if !self.credentials.contains_key(cred_id) {
                    errors.push(format!(
                        "Project '{}' references unknown credential '{}'",
                        id, cred_id
                    ));
                } else if self.is_vault(cred_id) {
                    errors.push(vault_as_login(&format!("Project '{id}'"), cred_id));
                }
            }
        }

        if let Some(ldap) = &self.server.ui.ldap {
            errors.extend(self.ldap_errors(ldap));
        }

        if let Some(home) = &self.server.ui.home
            && !is_local_path(home)
        {
            errors.push(format!(
                "server.ui.home '{home}' must be a path on this server, such as \"/\" — \
                 a sign-in redirects there, and another host would make it an open redirect"
            ));
        }

        // The UI signs in through the API's own login page and its session
        // cookie, which exist only with the browsable UI on.
        if self.server.ui.assets_dir.is_some() && !self.server.ui.enabled {
            errors.push(
                "server.ui.assets_dir is set but server.ui.enabled is false — the UI \
                 signs in through the login page, which only exists with the UI enabled"
                    .to_string(),
            );
        }

        errors
    }

    // Everything about `server.ui.ldap` that can be known without contacting
    // the directory, so a bad push fails validation instead of every login.
    fn ldap_errors(&self, ldap: &crate::domain::ldap::LdapConfig) -> Vec<String> {
        let mut errors = Vec::new();

        // The login form and the session are the browsable API's; without it
        // there is nothing for LDAP to log anybody into.
        if !self.server.ui.enabled {
            errors.push(
                "server.ui.ldap is set but server.ui.enabled is false — LDAP login is \
                 part of the browsable UI"
                    .to_string(),
            );
        }

        let url = ldap.url.to_ascii_lowercase();
        if url.starts_with("ldap://") {
            if !ldap.starttls {
                errors.push(format!(
                    "server.ui.ldap.url '{}' is plain LDAP — every password would cross \
                     the network in the clear; use ldaps:// or set starttls: true",
                    ldap.url
                ));
            }
        } else if url.starts_with("ldaps://") {
            if ldap.starttls {
                errors.push(
                    "server.ui.ldap: starttls applies to ldap:// URLs; an ldaps:// \
                     connection is already TLS"
                        .to_string(),
                );
            }
        } else {
            errors.push(format!(
                "server.ui.ldap.url '{}' must start with ldaps:// or ldap://",
                ldap.url
            ));
        }

        if !self.credentials.contains_key(&ldap.bind_credential) {
            errors.push(format!(
                "server.ui.ldap.bind_credential references unknown credential '{}'",
                ldap.bind_credential
            ));
        } else if self.is_vault(&ldap.bind_credential) {
            errors.push(vault_as_login(
                "server.ui.ldap.bind_credential",
                &ldap.bind_credential,
            ));
        }
        if !ldap.user_filter.contains("{username}") {
            errors.push(format!(
                "server.ui.ldap.user_filter '{}' has no {{username}} placeholder — it \
                 would match the same entry whoever logs in",
                ldap.user_filter
            ));
        }
        if ldap.access.is_empty() {
            errors.push(
                "server.ui.ldap.access is empty — nobody could log in; name the groups \
                 or users who may"
                    .to_string(),
            );
        }
        for rule in &ldap.access {
            if rule.groups.is_empty() && rule.users.is_empty() {
                errors.push(format!(
                    "server.ui.ldap.access '{}' names no groups and no users",
                    rule.name
                ));
            }
            for id in &rule.sources {
                if !self.sources.contains_key(id) && !self.views.contains_key(id) {
                    errors.push(format!(
                        "server.ui.ldap.access '{}' references unknown source '{}'",
                        rule.name, id
                    ));
                }
            }
            for id in &rule.endpoints {
                if !self.endpoints.contains_key(id) {
                    errors.push(format!(
                        "server.ui.ldap.access '{}' references unknown endpoint '{}'",
                        rule.name, id
                    ));
                }
            }
        }
        errors
    }

    // Script paths are NOT resolved into project checkouts here. They used to
    // be (a one-shot rewrite after the boot clones), which coupled serving to
    // the clones finishing and froze whatever the disk looked like at startup.
    // Resolution now happens per execution — see application::scripts.
}

// Everything wrong with a configuration directory, as items.
//
// Loading fails in two shapes — a file that does not parse (one error) and a
// directory that parses but does not hang together (one per broken
// reference). ConfigErrors holds either as a list; it is web-rs's, since the
// config API reports the same list.
pub use web_rs::config::ConfigErrors;

pub fn load_config(config_dir: &str) -> Result<AppConfig, Box<dyn std::error::Error>> {
    Ok(load_config_detailed(Path::new(config_dir))?)
}

// The same load, keeping the errors as a list. `load_config` is the thin
// wrapper over it that the tests use. Strict, and the one load: startup,
// --check-config, the config API and a reload all read this version's shape
// and nothing older.
pub fn load_config_detailed(dir: &Path) -> Result<AppConfig, ConfigErrors> {
    load(dir)
}

fn load(dir: &Path) -> Result<AppConfig, ConfigErrors> {
    // config.yaml is mandatory — without server config we cannot start
    let server_file: ServerFile = load_yaml_file(&dir.join(REQUIRED_CONFIG_FILE))
        .map_err(|e| ConfigErrors::single(format!("{REQUIRED_CONFIG_FILE}: {e}")))?;

    // Every other file holds items. All of them are read before anything is
    // reported, so a push with three typos in three files names all three.
    let mut errors = Vec::new();
    let mut entries: HashMap<&str, Vec<Entry>> = HashMap::new();
    let files = config_files(dir).map_err(ConfigErrors::single)?;
    for file in files.iter().filter(|f| *f != REQUIRED_CONFIG_FILE) {
        if !is_config_path(file) {
            errors.push(format!(
                "{file}: rename it — configuration paths may only use letters, digits, \
                 '-', '_' and '.', in segments that do not start with a dot"
            ));
            continue;
        }
        match fs::read_to_string(dir.join(file)) {
            Ok(text) => read_items(file, &text, &mut entries, &mut errors),
            Err(e) => errors.push(format!("{file}: {e}")),
        }
    }
    let mut section = |name: &str| entries.remove(name).unwrap_or_default();
    let (cr, pr, co, so, ho, gr, vi, en, ep, ak) = (
        section("credentials"),
        section("projects"),
        section("connectors"),
        section("sources"),
        section("hosts"),
        section("groups"),
        section("views"),
        section("enrichers"),
        section("endpoints"),
        section("api_keys"),
    );
    let credentials = take("credential", cr, &mut errors);
    let projects = take("project", pr, &mut errors);
    let connectors: HashMap<String, Connector> = take("connector", co, &mut errors);
    let source_defs: HashMap<String, SourceDef> = take("source", so, &mut errors);
    let hosts: HashMap<String, HostDef> = take("host", ho, &mut errors);
    let groups: HashMap<String, GroupDef> = take("group", gr, &mut errors);
    let views: HashMap<String, View> = take("view", vi, &mut errors);
    let enrichers = take("enricher", en, &mut errors);
    let endpoints = take("endpoint", ep, &mut errors);
    let api_keys = take("API key", ak, &mut errors);

    // A connector's own shape first: a source cannot be resolved against a
    // connector that is itself wrong, and its errors would only repeat.
    for (id, connector) in &connectors {
        errors.extend(connector.errors(id));
    }
    if !errors.is_empty() {
        // Cross-references are only worth checking once every item parsed:
        // otherwise one typo'd connector also reports every source using it.
        return Err(ConfigErrors::new(errors));
    }

    let mut sources = HashMap::new();
    for (id, def) in &source_defs {
        match def.resolve(id, &connectors) {
            Ok(source) => {
                sources.insert(id.clone(), source);
            }
            Err(found) => errors.extend(found),
        }
    }
    let view_ids: Vec<&String> = views.keys().collect();
    errors.extend(crate::domain::host::fold_into_sources(
        &hosts,
        &groups,
        &mut sources,
        &view_ids,
    ));
    if !errors.is_empty() {
        errors.sort();
        return Err(ConfigErrors::new(errors));
    }

    let config = AppConfig {
        server: server_file.server,
        cache: server_file.cache,
        projects_config: server_file.projects,
        secrets_config: server_file.secrets,
        config_api: server_file.config_api,
        credentials,
        sources,
        connectors,
        hosts,
        groups,
        views,
        enrichers,
        projects,
        endpoints,
        api_keys,
    };

    let errors = config.validate_errors();
    if errors.is_empty() {
        Ok(config)
    } else {
        Err(ConfigErrors::new(errors))
    }
}

// One item as found: which file, its name, and the rest of its keys.
struct Entry {
    file: String,
    name: String,
    body: serde_json::Map<String, serde_json::Value>,
    // The same item with numbers and booleans as their source text
    // (domain::yaml::to_text_json): what a field that wants text reads.
    text: serde_json::Map<String, serde_json::Value>,
}

// Reads one item file into `entries`, by section. YAML is read with the
// core-schema reader static inventories use (domain::yaml), so `0644` stays
// a string, and an error names a position, never the text — a configuration
// file can hold a secret, and the error goes to the log. It is read twice:
// typed, and with every scalar as its text, which is what a text field (a
// `config:` value, a script argument) is given — `1.10` stays "1.10".
fn read_items(
    file: &str,
    text: &str,
    entries: &mut HashMap<&'static str, Vec<Entry>>,
    errors: &mut Vec<String>,
) {
    use serde_json::Value;
    let top = match crate::domain::yaml::to_json(text) {
        Ok(Value::Object(top)) => top,
        // An empty file is a file with nothing in it yet, not an error.
        Ok(Value::Null) => return,
        Ok(_) => {
            errors.push(format!(
                "{file}: expected a mapping with one of: {}",
                SECTIONS.join(", ")
            ));
            return;
        }
        Err(e) => {
            errors.push(format!("{file}: {e}"));
            return;
        }
    };
    // The same document, so it parses the same; an empty map only if not.
    let mut text_top = match crate::domain::yaml::to_text_json(text) {
        Ok(Value::Object(top)) => top,
        _ => serde_json::Map::new(),
    };
    for (key, value) in top {
        let mut text_items = match text_top.remove(&key) {
            Some(Value::Array(items)) => items.into_iter(),
            _ => Vec::new().into_iter(),
        };
        let Some(section) = SECTIONS.iter().find(|s| **s == key) else {
            // An id with a map under it is the 0.34 layout (items keyed by
            // id), which this version no longer reads. Saying so is the
            // difference between a typo hunt and a one-line answer.
            let hint = if value.is_object() {
                " — this is the 0.34 layout (items keyed by id), which this version \
                 does not read: write it as items, or convert it with unified-api 0.36's \
                 --migrate-config"
            } else {
                ""
            };
            errors.push(format!(
                "{file}: unknown key '{key}' (expected one of: {}){hint}",
                SECTIONS.join(", ")
            ));
            continue;
        };
        let items = match value {
            Value::Array(items) => items,
            Value::Null => continue,
            _ => {
                errors.push(format!("{file}: '{key}' must be a list of items"));
                continue;
            }
        };
        for (i, item) in items.into_iter().enumerate() {
            let mut text_body = match text_items.next() {
                Some(Value::Object(body)) => body,
                _ => serde_json::Map::new(),
            };
            let Value::Object(mut body) = item else {
                errors.push(format!("{file}: {key} item {} is not a mapping", i + 1));
                continue;
            };
            body.remove("name");
            // By its text: `name: 2024` is the item "2024", as an id-keyed
            // 0.34 item of that name was.
            let name = match text_body.remove("name") {
                Some(Value::String(name)) if !name.trim().is_empty() => name,
                _ => {
                    errors.push(format!(
                        "{file}: {key} item {} needs a 'name', its identity",
                        i + 1
                    ));
                    continue;
                }
            };
            if !is_safe_id(&name) {
                errors.push(format!(
                    "{file}: {key} item '{name}' — a name may only use letters, digits, \
                     '-', '_' and '.', and may not start with a dot (it is used in URLs)"
                ));
                continue;
            }
            entries.entry(section).or_default().push(Entry {
                file: file.to_string(),
                name,
                body,
                text: text_body,
            });
        }
    }
}

// One section's items, typed. A name defined twice is an error naming both
// files: which one wins would otherwise depend on the order files are read.
//
// Two adjustments before an item is typed:
// - its `description` becomes the struct's `name` (falling back to the
//   identity). Every item struct's `name` is the label people read; the
//   identity is the map key. Doing it here, once, is what keeps every
//   response that has always answered `name` answering the same thing.
// - a field that wants text takes a scalar's source text (see
//   domain::yaml::from_json_with_text). `config:` maps are free-form
//   string → string (they become environment variables and script settings),
//   and the typed reader before 0.35 took `concurrency: 20` as "20" and
//   `version: 1.10` as "1.10"; read as numbers they were refused ("expected a
//   string") or rewritten (1.1). The same holds for `script_args: [7]`.
fn take<T: serde::de::DeserializeOwned>(
    label: &str,
    entries: Vec<Entry>,
    errors: &mut Vec<String>,
) -> HashMap<String, T> {
    use serde_json::Value;
    let mut out = HashMap::new();
    let mut seen: HashMap<String, String> = HashMap::new();
    for mut entry in entries {
        if let Some(first) = seen.get(&entry.name) {
            errors.push(format!(
                "{}: {label} '{}' is already defined in {first}",
                entry.file, entry.name
            ));
            continue;
        }
        seen.insert(entry.name.clone(), entry.file.clone());

        let text_description = entry.text.remove("description");
        let display = match entry.body.remove("description") {
            Some(Value::String(text)) if !text.trim().is_empty() => text,
            // `description: 2024`: its text, as any text field.
            Some(Value::Number(_) | Value::Bool(_)) => match text_description {
                Some(Value::String(text)) => text,
                _ => entry.name.clone(),
            },
            Some(Value::Null) | None => entry.name.clone(),
            Some(Value::String(_)) => entry.name.clone(),
            Some(_) => {
                errors.push(format!(
                    "{}: {label} '{}': description must be text",
                    entry.file, entry.name
                ));
                continue;
            }
        };
        entry
            .body
            .insert("name".to_string(), Value::String(display));

        match crate::domain::yaml::from_json_with_text::<T>(
            Value::Object(entry.body),
            Value::Object(entry.text),
        ) {
            Ok(item) => {
                out.insert(entry.name, item);
            }
            Err(e) => errors.push(format!("{}: {label} '{}': {e}", entry.file, entry.name)),
        }
    }
    out
}

// Reads and parses a YAML file — fails if it does not exist
fn load_yaml_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<T, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    // No source snippet in errors: a config file can hold a secret, and the
    // error goes to the log. Only `true`/`false` are booleans, as in 0.31.
    let options = serde_saphyr::options! {
        with_snippet: false,
        strict_booleans: true,
    };
    let parsed = serde_saphyr::from_str_with_options(&contents, options)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Config tests need real files on disk.
    // We create a temporary directory with test YAML.

    fn config_with(yaml: &str) -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.yaml"), yaml).unwrap();
        load_config(dir.path().to_str().unwrap()).expect("fixture must load")
    }

    // A reload swaps what it can and has to be honest about the rest: these
    // are the keys whose value was consumed once, at construction, and cannot
    // be adopted by a process that is already running on them.
    #[test]
    fn the_settings_a_reload_cannot_apply_are_named_one_by_one() {
        let live = RestartOnlySettings::from_config(&config_with(
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        ));
        let proposed = RestartOnlySettings::from_config(&config_with(concat!(
            "server:\n",
            "  host: \"0.0.0.0\"\n",
            "  port: 9091\n",
            "cache:\n",
            "  persistence:\n",
            "    path: \"/var/lib/unified-api/cache.json\"\n",
        )));

        let changed = live.changed_keys(&proposed);

        assert!(changed.contains(&"server.host".to_string()));
        assert!(changed.contains(&"server.port".to_string()));
        assert!(changed.contains(&"cache.persistence".to_string()));
        assert!(
            !changed.contains(&"projects.dir".to_string()),
            "an untouched key must not be reported: {:?}",
            changed
        );
    }

    #[test]
    fn an_unchanged_configuration_needs_no_restart() {
        let yaml = "server:\n  host: \"127.0.0.1\"\n  port: 9090\n";
        let live = RestartOnlySettings::from_config(&config_with(yaml));
        assert!(live.changed_keys(&live).is_empty());
    }

    // Reloadable settings are NOT restart-only: reporting one would tell an
    // operator to restart for something that already took effect.
    #[test]
    fn a_reloadable_setting_is_not_reported_as_needing_a_restart() {
        let live = RestartOnlySettings::from_config(&config_with(
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        ));
        let proposed = RestartOnlySettings::from_config(&config_with(concat!(
            "server:\n",
            "  host: \"127.0.0.1\"\n",
            "  port: 9090\n",
            "  readyz_require_all_sources: true\n",
        )));

        assert!(live.changed_keys(&proposed).is_empty());
    }

    // server.url is shown back as a link: anything but a web address is a typo.
    #[test]
    fn server_url_must_be_a_web_address() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  url: \"central.example\"\n",
        )
        .unwrap();
        let error = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a bare hostname is refused");
        assert!(error.to_string().contains("server.url"), "{error}");
        let cfg = config_with(
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  url: \"https://central.example\"\n",
        );
        assert_eq!(cfg.server.url.as_deref(), Some("https://central.example"));
    }

    #[test]
    fn the_config_api_is_off_unless_it_is_asked_for() {
        let cfg = config_with("server:\n  host: \"127.0.0.1\"\n  port: 9090\n");
        assert!(!cfg.config_api.enabled);

        let cfg = config_with(concat!(
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
            "config_api:\n  enabled: true\n",
        ));
        assert!(cfg.config_api.enabled);
    }

    // The rule the API applies to what it is asked to write and the rule the
    // loader applies to what it finds are one rule, or a file becomes
    // writable that nothing reads (or the other way round, which is worse).
    #[test]
    fn configuration_paths() {
        for ok in ["config.yaml", "a/b/c.yml", "sources/acme/src-d42-acme.yaml"] {
            assert!(is_config_path(ok), "{ok}");
        }
        for bad in [
            "",
            "a.json",
            "../x.yaml",
            "a/../x.yaml",
            "/abs.yaml",
            "a//b.yaml",
            ".hidden.yaml",
            "a/.git/x.yaml",
            "with space.yaml",
            "a\\b.yaml",
            "secrets.json",
        ] {
            assert!(!is_config_path(bad), "{bad:?}");
        }
    }

    // ---- item files -------------------------------------------------------

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        dir
    }

    const CONNECTOR: &str = "connectors:\n  - name: inv\n    project: prj-inv\n    path: inv.py\n";
    const PROJECT: &str =
        "projects:\n  - name: prj-inv\n    git_url: https://example.com/inv.git\n";

    #[test]
    fn items_load_from_any_depth_by_their_top_level_key() {
        let dir = dir_with(&[
            // Folders are for people: a connector may live anywhere.
            ("connectors/inv.yaml", CONNECTOR),
            ("projects.yml", PROJECT),
            (
                "sources/team-a/storage/src-a.yaml",
                "sources:\n  - name: src-a\n    description: The A inventory\n    connector: inv\n    ttl_seconds: 60\n",
            ),
            // Hidden entries are not configuration: the migration's backup,
            // the config API's staging, editors, git.
            (
                ".legacy-config-20261025T000000Z/sources.yaml",
                "nonsense: [",
            ),
            (".git/HEAD.yaml", "nonsense: ["),
            ("sources/.swp.yaml", "nonsense: ["),
            // Neither is anything that is not YAML.
            ("README.md", "# notes"),
        ]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        let source = &cfg.sources["src-a"];
        assert_eq!(source.name, "The A inventory");
        assert_eq!(source.connector, "inv");
        assert_eq!(source.script_path, "inv.py");
        assert!(cfg.connectors.contains_key("inv") && cfg.projects.contains_key("prj-inv"));
    }

    #[test]
    fn without_a_description_the_label_is_the_identity() {
        let dir = dir_with(&[(
            "all.yaml",
            &format!(
                "{CONNECTOR}{PROJECT}sources:\n  - name: src-a\n    connector: inv\n    ttl_seconds: 60\n"
            ),
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.sources["src-a"].name, "src-a");
        assert_eq!(cfg.projects["prj-inv"].name, "prj-inv");
    }

    #[test]
    fn a_configmap_mount_of_symlinks_loads() {
        // What the kubelet writes: the real files in a hidden folder, and a
        // link per key at the top.
        let dir = dir_with(&[
            ("..2026_10_25/connectors-inv.yaml", CONNECTOR),
            ("..2026_10_25/projects.yaml", PROJECT),
        ]);
        std::os::unix::fs::symlink(dir.path().join("..2026_10_25"), dir.path().join("..data"))
            .unwrap();
        for name in ["connectors-inv.yaml", "projects.yaml"] {
            std::os::unix::fs::symlink(format!("..data/{name}"), dir.path().join(name)).unwrap();
        }
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert!(cfg.connectors.contains_key("inv"));
    }

    #[test]
    fn config_maps_take_unquoted_scalars_as_text() {
        let dir = dir_with(&[(
            "all.yaml",
            &format!(
                "{CONNECTOR}{PROJECT}sources:\n  - name: src-a\n    connector: inv\n    ttl_seconds: 60\n    config:\n      concurrency: 20\n      insecure: true\n      mode: \"0644\"\n"
            ),
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        let config = &cfg.sources["src-a"].config;
        assert_eq!(config["concurrency"], "20");
        assert_eq!(config["insecure"], "true");
        assert_eq!(config["mode"], "0644");
    }

    // A text field holds what the file says, not what a number made of it:
    // through numbers `1.10` was "1.1", `0x1F` was "31" and a long id a
    // float in scientific notation — and a bare number in a list of
    // arguments was refused outright.
    #[test]
    fn text_fields_keep_number_looking_values_verbatim() {
        let dir = dir_with(&[(
            "all.yaml",
            &format!(
                "{PROJECT}connectors:\n  - name: inv\n    project: prj-inv\n    path: inv.py\n    \
                 script_args: [\"--days\", 7, 1.10]\n    timeout_seconds: 0x3C\n\
                 sources:\n  - name: src-a\n    description: 2024\n    connector: inv\n    \
                 ttl_seconds: 60\n    config:\n      API_VERSION: 1.10\n      ACCOUNT: 0x1F\n      \
                 BIG: 123456789012345678901234\n      FLAG: True\n      PLUS: +3\n\
                 enrichers:\n  - name: enr-a\n    target: src-a\n    project: prj-inv\n    \
                 script_path: e.py\n    script_args: [\"--days\", 7]\n"
            ),
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        let source = &cfg.sources["src-a"];
        assert_eq!(source.config["API_VERSION"], "1.10");
        assert_eq!(source.config["ACCOUNT"], "0x1F");
        assert_eq!(source.config["BIG"], "123456789012345678901234");
        assert_eq!(source.config["FLAG"], "True");
        assert_eq!(source.config["PLUS"], "+3");
        assert_eq!(source.name, "2024");
        assert_eq!(source.script_args, ["--days", "7", "1.10"]);
        // A field that wants a number still reads one.
        assert_eq!(cfg.connectors["inv"].timeout_seconds, 60);
        assert_eq!(cfg.enrichers["enr-a"].script_args, ["--days", "7"]);
    }

    // A name is always text: `name: 2024` is the item "2024".
    #[test]
    fn a_number_looking_name_is_the_name() {
        let dir = dir_with(&[(
            "p.yaml",
            "projects:\n  - name: 2024\n    git_url: https://example.com/inv.git\n",
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert!(cfg.projects.contains_key("2024"));
    }

    #[test]
    fn duplicate_names_unknown_keys_and_bad_names_are_all_named() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECT),
            ("connectors/one.yaml", CONNECTOR),
            ("connectors/two.yaml", CONNECTOR),
            ("typo.yaml", "sorces:\n  - name: x\n"),
            ("legacy.yaml", "src-a:\n  name: A\n"),
            (
                "nameless.yaml",
                "projects:\n  - git_url: https://example.com/x.git\n",
            ),
            (
                "slash.yaml",
                "projects:\n  - name: a/b\n    git_url: https://example.com/x.git\n",
            ),
            ("bad field.yaml", "projects: []\n"),
        ]);
        let err = load_err(&dir);
        for expected in [
            "connectors/two.yaml: connector 'inv' is already defined in connectors/one.yaml",
            "typo.yaml: unknown key 'sorces'",
            "legacy.yaml: unknown key 'src-a'",
            "this is the 0.34 layout",
            "nameless.yaml: projects item 1 needs a 'name'",
            "slash.yaml: projects item 'a/b'",
            "bad field.yaml: rename it",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    #[test]
    fn unknown_keys_inside_an_item_name_the_file_and_the_item() {
        let dir = dir_with(&[(
            "sources/src-a.yaml",
            "sources:\n  - name: src-a\n    connector: remote\n    ttl_seconds: 60\n    credential_ids: []\n",
        )]);
        let err = load_err(&dir);
        assert!(
            err.contains("sources/src-a.yaml: source 'src-a'") && err.contains("credential_ids"),
            "{err}"
        );
    }

    #[test]
    fn connector_rules_are_checked_at_load() {
        let dir = dir_with(&[(
            "connectors.yaml",
            "connectors:\n  - name: remote\n    path: /opt/x.py\n  - name: loose\n    path: x.py\n  - name: ghostly\n    project: prj-ghost\n    path: x.py\n",
        )]);
        let err = load_err(&dir);
        assert!(
            err.contains("Connector 'remote' uses the name of a built-in"),
            "{err}"
        );
        assert!(
            err.contains("Connector 'loose' has a relative path but no project"),
            "{err}"
        );
        // A connector's own shape is reported before references are: the
        // unknown project is named once the shape is right.
        let dir = dir_with(&[(
            "connectors.yaml",
            "connectors:\n  - name: ghostly\n    project: prj-ghost\n    path: x.py\n",
        )]);
        assert!(
            load_err(&dir).contains("Connector 'ghostly' references unknown project 'prj-ghost'")
        );
    }

    #[test]
    fn a_source_naming_an_unknown_connector_is_named() {
        let dir = dir_with(&[(
            "sources.yaml",
            "sources:\n  - name: src-a\n    connector: d43\n    ttl_seconds: 60\n",
        )]);
        assert!(load_err(&dir).contains("Source 'src-a' references unknown connector 'd43'"));
    }

    #[test]
    fn load_config_from_directory() {
        // tempdir: we create a temporary directory for the test
        let dir = tempfile::tempdir().unwrap();

        // Write minimal config.yaml
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();

        // Write credentials.yaml in map format. (This fixture used to carry a
        // `vault_path` key that no longer exists in the schema — silently
        // ignored for as long as unknown keys were dropped, caught the moment
        // they stopped being.)
        fs::write(
            dir.path().join("credentials.yaml"),
            "credentials:\n  - name: cred-test\n    description: \"Test\"\n    type: \"token\"\n    env_prefix: \"TEST\"\n",
        )
        .unwrap();

        // dir.path().to_str() converts the Path to &str
        let cfg = load_config(dir.path().to_str().unwrap()).unwrap();

        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 9090);
        assert_eq!(cfg.credentials.len(), 1);
        assert!(cfg.credentials.contains_key("cred-test"));
        // sources.yaml does not exist → empty HashMap, no error
        assert_eq!(cfg.sources.len(), 0);
    }

    #[test]
    fn load_config_fails_without_server_config() {
        let dir = tempfile::tempdir().unwrap();
        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn validate_catches_enricher_with_unknown_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("enrichers.yaml"),
            "enrichers:\n  - name: enrich-test\n    description: \"Test\"\n    target: \"src-nonexistent\"\n    script_path: \"test.py\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("src-nonexistent"));
    }

    #[test]
    fn validate_catches_endpoint_with_unknown_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-test\n    description: \"Test\"\n    sources: [\"src-ghost\"]\n    script_path: \"test.py\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("src-ghost"));
    }

    #[test]
    fn validate_catches_endpoint_with_neither_output_nor_script() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-test\n    description: \"Test\"\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("an endpoint with no output and no script_path must fail")
            .to_string();
        assert!(err.contains("either output"), "{}", err);
    }

    #[test]
    fn validate_catches_endpoint_with_both_output_and_script() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-test\n    description: \"Test\"\n    output: ansible\n    script_path: \"x.py\"\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("an endpoint with both output and script_path must fail")
            .to_string();
        assert!(err.contains("choose one"), "{}", err);
    }

    #[test]
    fn builtin_endpoint_config_takes_only_the_shared_settings() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-ok\n    description: \"Ok\"\n    output: json\n    config:\n      host: \"*.aa1.pqe\"\n      group: \"linux\"\n      filter: \"os=OracleLinux\"\n      fields: \"os\"\n      exclude_fields: \"secret\"\n",
        )
        .unwrap();
        load_config(dir.path().to_str().unwrap()).expect("the five shared settings must validate");

        // A name from before 0.32 fails naming what it is called now; an
        // unknown key fails listing what a builtin understands.
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-old\n    description: \"Old\"\n    output: ansible\n    config:\n      filter_os: \"OracleLinux\"\n      colour: \"red\"\n",
        )
        .unwrap();
        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("the load must fail")
            .to_string();
        assert!(
            err.contains("'filter_os' — it is called filter: \"os=<value>\" now"),
            "{err}"
        );
        assert!(
            err.contains(
                "'colour' (a builtin understands: host, group, filter, fields, exclude_fields)"
            ),
            "{err}"
        );

        // A malformed static filter fails the load, not every render.
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-bad\n    description: \"Bad\"\n    output: csv\n    config:\n      filter: \"os\"\n",
        )
        .unwrap();
        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("the load must fail")
            .to_string();
        assert!(
            err.contains("Endpoint 'ep-bad' config: filter condition 'os' has no operator"),
            "{err}"
        );
    }

    #[test]
    fn builtin_output_endpoint_validates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-test\n    description: \"Test\"\n    output: ansible\n",
        )
        .unwrap();

        load_config(dir.path().to_str().unwrap())
            .expect("a builtin-output endpoint with no sources must validate");
    }

    // A limit is what makes an endpoint a constructed inventory, and the three
    // ways to get it wrong all fail at load rather than at render time -- a
    // limit that quietly does nothing, or quietly matches nothing, looks like a
    // working endpoint serving the wrong inventory.
    fn config_dir_with_two_sources(endpoints_yaml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-cmdb\n    description: \"CMDB\"\n    connector: c-src-cmdb\n    ttl_seconds: 60\n  - name: src-vmware\n    description: \"VMware\"\n    connector: c-src-vmware\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-cmdb\n    project: \"prj-test\"\n    path: \"cmdb.py\"\n  - name: c-src-vmware\n    project: \"prj-test\"\n    path: \"vmware.py\"\n",
        )
        .unwrap();
        fs::write(dir.path().join("endpoints.yaml"), endpoints_yaml).unwrap();
        dir
    }

    #[test]
    fn an_endpoint_limited_by_one_of_its_own_sources_validates() {
        let dir = config_dir_with_two_sources(
            "endpoints:\n  - name: ep-built\n    description: \"Built\"\n    sources: [\"src-cmdb\", \"src-vmware\"]\n    output: ansible\n    limit:\n      by_hosts_from_inventory: \"src-cmdb\"\n",
        );

        let cfg = load_config(dir.path().to_str().unwrap())
            .expect("a limit naming one of the endpoint's sources must validate");
        assert_eq!(
            cfg.endpoints["ep-built"]
                .limit
                .as_ref()
                .and_then(|limit| limit.by_hosts_from_inventory.as_deref()),
            Some("src-cmdb")
        );
    }

    #[test]
    fn validate_catches_a_limit_naming_a_source_the_endpoint_does_not_read() {
        // src-vmware exists, but this endpoint never merges it -- so the
        // intersection would be against data the endpoint never waits for.
        let dir = config_dir_with_two_sources(
            "endpoints:\n  - name: ep-built\n    description: \"Built\"\n    sources: [\"src-cmdb\"]\n    output: ansible\n    limit:\n      by_hosts_from_inventory: \"src-vmware\"\n",
        );

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a limit naming a source outside source_ids must fail")
            .to_string();
        assert!(err.contains("src-vmware"), "{}", err);
        assert!(err.contains("add it to sources"), "{}", err);
    }

    #[test]
    fn validate_catches_a_limit_that_names_no_rule() {
        let dir = config_dir_with_two_sources(
            "endpoints:\n  - name: ep-built\n    description: \"Built\"\n    sources: [\"src-cmdb\"]\n    output: ansible\n    limit: {}\n",
        );

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a limit with no rule must fail")
            .to_string();
        assert!(err.contains("names no rule"), "{}", err);
    }

    // deny_unknown_fields reaches inside the limit too: a misspelled rule is
    // named at load instead of silently limiting nothing.
    #[test]
    fn a_typo_inside_the_limit_is_named_at_load() {
        let dir = config_dir_with_two_sources(
            "endpoints:\n  - name: ep-built\n    description: \"Built\"\n    sources: [\"src-cmdb\"]\n    output: ansible\n    limit:\n      by_hosts_from_inventary: \"src-cmdb\"\n",
        );

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("an unknown key inside limit must fail")
            .to_string();
        assert!(err.contains("by_hosts_from_inventary"), "{}", err);
    }

    #[test]
    fn validate_catches_builtin_endpoint_with_timeout() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        // timeout_seconds bounds a script; a builtin runs in-process, so
        // setting it is a mistake to name, like project_id and script_args.
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-test\n    description: \"Test\"\n    output: csv\n    timeout_seconds: 60\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a builtin endpoint setting timeout_seconds must fail")
            .to_string();
        assert!(err.contains("timeout_seconds"), "{}", err);
    }

    #[test]
    fn validate_catches_source_with_unknown_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        // sources.yaml declares a project_id that does not exist in projects.yaml
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-test\n    description: \"Test\"\n    connector: c-src-test\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-test\n    project: \"prj-ghost\"\n    path: \"test.py\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("prj-ghost"));
    }

    #[test]
    fn validate_catches_a_missing_ssh_known_hosts_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        let kh_path = dir.path().join("known_hosts");
        fs::write(
            dir.path().join("sources.yaml"),
            format!(
                "sources:\n  - name: src-fleet\n    description: \"Fleet\"\n    connector: c-src-fleet\n    ttl_seconds: 60\n    config:\n      hosts: \"a.example\"\n      ssh_known_hosts: \"{}\"\nconnectors:\n  - name: c-src-fleet\n    type: ssh\n    location: host\n    path: \"/etc/ansible/facts.d/*.fact\"\n",
                kh_path.display()
            ),
        )
        .unwrap();

        // The file does not exist yet: startup must fail and name the path,
        // not refuse every host of every sync at runtime.
        let result = load_config(dir.path().to_str().unwrap());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("ssh_known_hosts"), "{}", err);

        // Creating the file is all it takes.
        fs::write(&kh_path, "").unwrap();
        load_config(dir.path().to_str().unwrap()).expect("an existing file must validate");
    }

    #[test]
    fn validate_catches_ssh_known_hosts_on_a_non_ssh_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-script\n    description: \"Script\"\n    connector: c-src-script\n    ttl_seconds: 60\n    config:\n      ssh_known_hosts: \"/anywhere\"\nconnectors:\n  - name: c-src-script\n    project: \"prj-test\"\n    path: \"test.py\"\n",
        )
        .unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("not an ssh source"), "{}", err);
    }

    // The LDAP block is checked before anyone tries to log in: a plaintext
    // URL, a service account that does not exist, or a UI that is off are
    // all configuration errors, not login failures.
    #[test]
    fn validate_ldap_rules() {
        let check = |ui: &str, credentials: &str| {
            let dir = tempfile::tempdir().unwrap();
            fs::write(
                dir.path().join("config.yaml"),
                format!("server:\n  host: \"127.0.0.1\"\n  port: 9090\n  ui:\n{ui}"),
            )
            .unwrap();
            fs::write(dir.path().join("credentials.yaml"), credentials).unwrap();
            load_config(dir.path().to_str().unwrap())
                .err()
                .map(|e| e.to_string())
        };
        let cred = "credentials:\n  - name: cred-ldap\n    description: \"LDAP\"\n    type: username_password\n    env_prefix: \"LDAP\"\n";
        let ldap = |url: &str, extra: &str| {
            format!(
                "    enabled: true\n    ldap:\n      url: \"{url}\"\n      bind_credential: cred-ldap\n      \
                 search_base: \"DC=x\"\n      access:\n        - name: ops\n          groups: [DL-ops]\n          \
                 role: admin\n{extra}"
            )
        };

        assert_eq!(check(&ldap("ldaps://ldap.example.com", ""), cred), None);
        assert_eq!(
            check(
                &ldap("ldap://ldap.example.com", "      starttls: true\n"),
                cred
            ),
            None
        );

        let err = check(&ldap("ldap://ldap.example.com", ""), cred).expect("plaintext refused");
        assert!(err.contains("in the clear"), "{err}");

        let err = check(&ldap("ldaps://ldap.example.com", ""), "").expect("unknown credential");
        assert!(err.contains("unknown credential 'cred-ldap'"), "{err}");

        let off = ldap("ldaps://ldap.example.com", "").replace("enabled: true", "enabled: false");
        let err = check(&off, cred).expect("ui off");
        assert!(err.contains("server.ui.enabled is false"), "{err}");

        // A UI to serve needs the login page it signs in through.
        let err = check("    enabled: false\n    assets_dir: \"/srv/ui\"\n", "")
            .expect("assets without the UI refused");
        assert!(err.contains("server.ui.assets_dir"), "{err}");
        assert_eq!(
            check("    enabled: true\n    assets_dir: \"/srv/ui\"\n", ""),
            None
        );

        // Where a login lands must stay on this server: it is a redirect.
        assert_eq!(check("    enabled: true\n    home: \"/\"\n", ""), None);
        for home in ["https://evil.example", "//evil.example", "inventory"] {
            let err = check(&format!("    enabled: true\n    home: \"{home}\"\n"), "")
                .expect("an off-site home refused");
            assert!(err.contains("server.ui.home"), "{err}");
        }
    }

    #[test]
    fn validate_catches_project_with_unknown_credential() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n    credential: \"cred-ghost\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("cred-ghost"));
    }

    #[test]
    fn validate_accepts_source_with_existing_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-test\n    description: \"Test\"\n    connector: c-src-test\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-test\n    project: \"prj-test\"\n    path: \"test.py\"\n",
        ).unwrap();

        let cfg = load_config(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(cfg.projects.len(), 1);
    }

    #[test]
    fn validate_catches_enricher_with_unknown_project() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-test\n    description: \"Test\"\n    connector: c-src-test\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-test\n    project: \"prj-test\"\n    path: \"test.py\"\n",
        ).unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("enrichers.yaml"),
            "enrichers:\n  - name: enrich-test\n    description: \"Test\"\n    target: \"src-test\"\n    script_path: \"e.py\"\n    project: \"prj-ghost\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("prj-ghost"));
    }

    #[test]
    fn validate_hosts_from_source_rules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        // three violations at once: not an ssh source, self-reference is
        // checked on the ssh one, and both hosts + hosts_from_source
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                "connectors:\n  - name: inv\n    project: \"prj-test\"\n    path: \"x.py\"\n  - name: facts\n    type: ssh\n    location: host\n    path: \"/etc/ansible/facts.d/*.fact\"\n",
                "sources:\n",
                "  - name: src-script\n    connector: inv\n    ttl_seconds: 60\n",
                "    hosts_from_source:\n      source: \"src-ssh\"\n",
                "  - name: src-ssh\n    connector: facts\n    ttl_seconds: 60\n",
                "    hosts_from_source:\n      source: \"src-ssh\"\n",
                "    config:\n      hosts: \"a.example.com\"\n",
            ),
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("expected validation errors")
            .to_string();
        assert!(err.contains("not an ssh source"), "missing rule: {}", err);
        assert!(err.contains("cannot use itself"), "missing rule: {}", err);
        assert!(err.contains("pick one"), "missing rule: {}", err);
    }

    #[test]
    fn validate_host_args_only_on_script_sources() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                "connectors:\n  - name: facts\n    type: ssh\n    location: host\n    path: \"/etc/ansible/facts.d/*.fact\"\n",
                "    host_args: [\"--host\", \"{target}\"]\n",
                "sources:\n  - name: src-ssh\n    connector: facts\n    ttl_seconds: 60\n",
                "    config:\n      hosts: \"a.example.com\"\n",
            ),
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("expected a validation error")
            .to_string();
        assert!(
            err.contains("not a script connector"),
            "missing rule: {}",
            err
        );
    }

    #[test]
    fn validate_hosts_from_source_unknown_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                "connectors:\n  - name: facts\n    type: ssh\n    location: host\n    path: \"/etc/ansible/facts.d/*.fact\"\n",
                "sources:\n  - name: src-ssh\n    connector: facts\n    ttl_seconds: 60\n",
                "    hosts_from_source:\n      source: \"src-ghost\"\n",
            ),
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("expected validation error")
            .to_string();
        assert!(err.contains("src-ghost"), "error was: {}", err);
    }

    // A directory with one source and one project, so view tests only have to
    // write views.yaml.
    fn dir_with_one_source() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-a\n    description: \"A\"\n    connector: c-src-a\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-a\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();
        dir
    }

    fn load_err(dir: &tempfile::TempDir) -> String {
        load_config(dir.path().to_str().unwrap())
            .err()
            .expect("expected validation error")
            .to_string()
    }

    // An API key names its variable as a credential does: env_vars, `key`.
    #[test]
    fn an_api_key_names_its_variable_under_env_vars_key() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("keys.yaml"),
            concat!(
                "api_keys:\n",
                "  - name: key-new\n    env_vars:\n      key: KEY_NEW\n    role: admin\n",
                "  - name: key-old\n    env: KEY_OLD\n    role: admin\n",
            ),
        )
        .unwrap();
        let cfg = load_config(dir.path().to_str().unwrap()).expect("both forms load");
        assert_eq!(cfg.api_keys["key-new"].env_var(), "KEY_NEW");
        assert_eq!(cfg.api_keys["key-old"].env_var(), "KEY_OLD");
        assert_eq!(
            cfg.deprecations(),
            vec![
                "API key 'key-old': env is deprecated and will be refused in a later release; \
                 write env_vars: {key: KEY_OLD}"
            ]
        );
    }

    #[test]
    fn an_api_key_with_another_field_or_two_variables_is_refused() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("keys.yaml"),
            concat!(
                "api_keys:\n",
                "  - name: key-typo\n    env_vars:\n      kye: K\n",
                "  - name: key-both\n    env: K\n    env_vars:\n      key: K\n",
            ),
        )
        .unwrap();
        let err = load_err(&dir);
        assert!(
            err.contains("env_vars.kye is not a field of an API key"),
            "{err}"
        );
        assert!(
            err.contains("API key 'key-typo' names no variable"),
            "{err}"
        );
        assert!(err.contains("sets both env_vars.key and env"), "{err}");
    }

    // A host's and a group's TTLs live in their own items and land on the
    // sources they name; the source file no longer says them.
    #[test]
    fn host_and_group_items_set_the_ttls_of_the_sources_they_name() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("web01.example.com.yaml"),
            "hosts:\n  - name: web01.example.com\n    sources:\n      src-a:\n        ttl_seconds: 30\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("production.yaml"),
            "groups:\n  - name: production\n    description: \"Prod\"\n    sources:\n      src-a:\n        ttl_seconds: 900\n",
        )
        .unwrap();

        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");

        let overrides = &cfg.sources["src-a"].ttl_overrides;
        assert_eq!(overrides.hosts["web01.example.com"], 30);
        assert_eq!(overrides.groups["production"], 900);
        assert_eq!((cfg.hosts.len(), cfg.groups.len()), (1, 1));
    }

    #[test]
    fn a_host_item_naming_an_unknown_source_is_refused() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("web01.yaml"),
            "hosts:\n  - name: web01\n    sources:\n      src-gone:\n        ttl_seconds: 30\n",
        )
        .unwrap();
        let err = load_err(&dir);
        assert!(
            err.contains("Host 'web01' references unknown source 'src-gone'"),
            "{err}"
        );
    }

    // Two credentials one source would deliver under one id, and a second
    // credential on a connector that logs in with one, are refused.
    #[test]
    fn a_source_s_credentials_never_collide() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("cred.yaml"),
            "credentials:\n  - name: cred-vc-aa1\n    type: token\n    env_vars:\n      token: A\n  - name: cred_vc_aa1\n    type: token\n    env_vars:\n      token: B\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-a\n    description: \"A\"\n    connector: c-src-a\n    ttl_seconds: 60\n    credentials: [cred-vc-aa1, cred_vc_aa1]\n  - name: src-edge\n    connector: remote\n    ttl_seconds: 60\n    credentials: [cred-vc-aa1, cred_vc_aa1]\n    config:\n      url: https://edge.example\n      source: src-x\nconnectors:\n  - name: c-src-a\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();
        let err = load_err(&dir);
        assert!(
            err.contains("that are all delivered as CREDENTIAL_VC_AA1_*"),
            "{err}"
        );
        assert!(
            err.contains(
                "Source 'src-edge' lists 2 credentials, but its remote connector logs in with one"
            ),
            "{err}"
        );
    }

    // A project names its credential as every item does: credentials, a
    // list. The singular form still loads, deprecated; more than one, or
    // both forms, are refused.
    #[test]
    fn a_project_takes_credentials_like_every_item() {
        let base = |extra: &str| {
            let dir = dir_with_one_source();
            fs::write(
                dir.path().join("cred.yaml"),
                "credentials:\n  - name: cred-gh\n    type: token\n    env_vars:\n      token: GH\n  - name: cred-two\n    type: token\n    env_vars:\n      token: TWO\n",
            )
            .unwrap();
            fs::write(
                dir.path().join("projects.yaml"),
                format!("projects:\n  - name: prj-test\n    git_url: \"https://example.com/repo.git\"\n{extra}"),
            )
            .unwrap();
            dir
        };

        let dir = base("    credentials:\n      - cred-gh\n");
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.projects["prj-test"].credential_id(), Some("cred-gh"));
        assert!(cfg.deprecations().is_empty(), "{:?}", cfg.deprecations());

        let dir = base("    credential: cred-gh\n");
        let cfg = load_config(dir.path().to_str().unwrap()).expect("the old form loads");
        assert_eq!(cfg.projects["prj-test"].credential_id(), Some("cred-gh"));
        assert_eq!(
            cfg.deprecations(),
            vec![
                "project 'prj-test': credential is deprecated and will be refused in a later \
                 release; write credentials: [cred-gh]"
            ]
        );

        let dir =
            base("    credentials:\n      - cred-gh\n      - cred-two\n    credential: cred-gh\n");
        let err = load_err(&dir);
        assert!(err.contains("lists 2 credentials"), "{err}");
        assert!(
            err.contains("sets both credentials and credential"),
            "{err}"
        );
    }

    // A source's own ttl_overrides still work, beside host and group items.
    #[test]
    fn a_source_keeps_its_own_ttl_overrides() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-a\n    description: \"A\"\n    connector: c-src-a\n    ttl_seconds: 60\n    ttl_overrides:\n      hosts:\n        web01: 30\n      groups:\n        production: 900\nconnectors:\n  - name: c-src-a\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("web02.yaml"),
            "hosts:\n  - name: web02\n    sources:\n      src-a:\n        ttl_seconds: 45\n",
        )
        .unwrap();
        let cfg = load_config(dir.path().to_str().unwrap()).expect("both places load");
        let overrides = &cfg.sources["src-a"].ttl_overrides;
        assert_eq!(overrides.hosts["web01"], 30);
        assert_eq!(overrides.hosts["web02"], 45);
        assert_eq!(overrides.groups["production"], 900);
    }

    #[test]
    fn validate_view_rules() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("views.yaml"),
            concat!(
                "views:\n",
                // id collides with a source, and its member is unknown
                "  - name: src-a\n    description: \"Collides\"\n    members:\n",
                "      - source: \"src-ghost\"\n        owns:\n          source: \"src-a\"\n",
                // lists the same member twice, and owns against an unknown source
                "  - name: vw-dup\n    description: \"Dup\"\n    members:\n",
                "      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n",
                "      - source: \"src-a\"\n        owns:\n          source: \"src-nowhere\"\n",
                // no members at all
                "  - name: vw-empty\n    description: \"Empty\"\n    members: []\n",
            ),
        )
        .unwrap();

        let err = load_err(&dir);
        assert!(err.contains("same id as a source"), "missing rule: {}", err);
        assert!(err.contains("src-ghost"), "missing rule: {}", err);
        assert!(err.contains("twice"), "missing rule: {}", err);
        assert!(err.contains("src-nowhere"), "missing rule: {}", err);
        assert!(err.contains("has no members"), "missing rule: {}", err);
    }

    #[test]
    fn validate_rejects_a_view_as_a_views_member() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("views.yaml"),
            concat!(
                "views:\n",
                "  - name: vw-inner\n    description: \"Inner\"\n    members:\n",
                "      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n",
                "  - name: vw-outer\n    description: \"Outer\"\n    members:\n",
                "      - source: \"vw-inner\"\n        owns:\n          source: \"src-a\"\n",
            ),
        )
        .unwrap();

        assert!(load_err(&dir).contains("views do not nest"));
    }

    #[test]
    fn validate_rejects_an_endpoint_or_enricher_pointed_at_a_view() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("views.yaml"),
            "views:\n  - name: vw-a\n    description: \"V\"\n    members:\n      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "endpoints:\n  - name: ep-a\n    description: \"E\"\n    sources: [\"vw-a\"]\n    script_path: \"t.py\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("enrichers.yaml"),
            "enrichers:\n  - name: en-a\n    description: \"E\"\n    target: \"vw-a\"\n    script_path: \"t.py\"\n",
        )
        .unwrap();

        let err = load_err(&dir);
        assert!(err.contains("output endpoints read cached"), "{}", err);
        assert!(err.contains("no cache entry to write into"), "{}", err);
    }

    #[test]
    fn a_restricted_api_key_may_be_granted_a_view_by_its_id() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("views.yaml"),
            "views:\n  - name: vw-a\n    description: \"V\"\n    members:\n      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("api_keys.yaml"),
            "api_keys:\n  - name: key-forms\n    description: \"Forms\"\n    env: \"X\"\n    sources: [\"vw-a\"]\n",
        )
        .unwrap();

        let cfg = load_config(dir.path().to_str().unwrap()).expect("a view id is a valid grant");
        assert_eq!(cfg.views.len(), 1);
    }

    #[test]
    fn a_typo_in_an_ownership_pattern_fails_to_load() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("views.yaml"),
            "views:\n  - name: vw-a\n    description: \"V\"\n    members:\n      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n          grups: [\"x\"]\n",
        )
        .unwrap();

        // Not a validation error but a parse error: an unknown key in the
        // routing table must never deserialize into "claims everything"
        assert!(load_err(&dir).contains("grups"));
    }

    // The same guarantee the views always had, now for every config file: a
    // typo'd key must fail startup naming the key, not silently apply the
    // default it was meant to override.
    #[test]
    fn a_typo_in_a_source_key_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        // sync_interval_second (no s): the sync interval this operator thinks
        // they configured would silently not exist
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-test\n    description: \"Test\"\n    connector: c-src-test\n    ttl_seconds: 60\n    sync_interval_second: 300\nconnectors:\n  - name: c-src-test\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a typo'd source key must not load")
            .to_string();
        assert!(err.contains("sync_interval_second"), "error was: {}", err);
    }

    #[test]
    fn a_typo_in_an_api_key_definition_fails_to_load() {
        let dir = dir_with_one_source();
        // `source:` instead of `sources:` — the grant this key was meant to
        // carry would silently become an empty one
        fs::write(
            dir.path().join("api_keys.yaml"),
            "api_keys:\n  - name: key-forms\n    description: \"Forms\"\n    env: \"X\"\n    source: [\"src-a\"]\n",
        )
        .unwrap();

        assert!(load_err(&dir).contains("source"));
    }

    #[test]
    fn a_typo_in_the_server_config_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        // metrics_required_auth: the security setting would silently stay off
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  metrics_required_auth: true\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a typo'd server key must not load")
            .to_string();
        assert!(err.contains("metrics_required_auth"), "error was: {}", err);
    }

    #[test]
    fn an_unknown_top_level_section_in_config_yaml_fails_to_load() {
        let dir = tempfile::tempdir().unwrap();
        // `caches:` instead of `cache:` — persistence would silently not run
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\ncaches:\n  persistence:\n    path: \"/tmp/cache.json\"\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("an unknown section must not load")
            .to_string();
        assert!(err.contains("caches"), "error was: {}", err);
    }

    #[test]
    fn an_advertised_local_member_needs_a_claim_or_a_fallback() {
        let dir = dir_with_one_source();
        // src-a is a plain script source with no advertise_scope: advertised
        // ownership with no fallback can never route anything
        fs::write(
            dir.path().join("views.yaml"),
            "views:\n  - name: vw-a\n    description: \"V\"\n    members:\n      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n          advertised: true\n",
        )
        .unwrap();
        assert!(load_err(&dir).contains("claim nothing"));

        // A declared fallback makes it valid
        fs::write(
            dir.path().join("views.yaml"),
            "views:\n  - name: vw-a\n    description: \"V\"\n    members:\n      - source: \"src-a\"\n        owns:\n          source: \"src-a\"\n          advertised: true\n          groups: [\"dc1\"]\n",
        )
        .unwrap();
        load_config(dir.path().to_str().unwrap()).expect("fallback makes it valid");
    }

    #[test]
    fn an_empty_advertise_scope_fails_validation() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-a\n    description: \"A\"\n    connector: c-src-a\n    ttl_seconds: 60\n    advertise_scope: {}\nconnectors:\n  - name: c-src-a\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();

        assert!(load_err(&dir).contains("advertise_scope"));
    }

    // The schedule field spent its first year ignored; now that it works,
    // junk in it must fail the deploy, and it cannot coexist with an interval.
    #[test]
    fn validate_cron_schedule_rules() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                "connectors:\n  - name: inv\n    project: \"prj-test\"\n    path: \"x.py\"\n",
                "sources:\n",
                // junk in the schedule
                "  - name: src-bad\n    connector: inv\n    ttl_seconds: 60\n",
                "    schedule: \"whenever feels right\"\n",
                // schedule AND interval
                "  - name: src-both\n    connector: inv\n    ttl_seconds: 60\n",
                "    schedule: \"0 2 * * *\"\n    sync_interval_seconds: 60\n",
            ),
        )
        .unwrap();

        // The same two rules bind enrichers and projects — the cadence is one
        // concept, whatever kind of periodic work it paces
        fs::write(
            dir.path().join("enrichers.yaml"),
            "enrichers:\n  - name: en-bad\n    description: \"E\"\n    target: \"src-both\"\n    script_path: \"e.py\"\n    schedule: \"nope\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            concat!(
                "projects:\n",
                "  - name: prj-test\n    git_url: \"https://example.com/repo.git\"\n",
                "  - name: prj-both\n    git_url: \"https://example.com/p.git\"\n",
                "    schedule: \"0 2 * * *\"\n    sync_interval_seconds: 60\n",
            ),
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("expected validation errors")
            .to_string();
        assert!(err.contains("src-bad"), "missing rule: {}", err);
        assert!(err.contains("invalid cron"), "missing rule: {}", err);
        assert!(err.contains("pick one"), "missing rule: {}", err);
        assert!(err.contains("Enricher 'en-bad'"), "missing rule: {}", err);
        assert!(err.contains("Project 'prj-both'"), "missing rule: {}", err);

        // Well-formed cron-only definitions of every kind load
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-cron\n    description: \"OK\"\n    connector: c-src-cron\n    ttl_seconds: 60\n    schedule: \"30 2 * * *\"\nconnectors:\n  - name: c-src-cron\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("enrichers.yaml"),
            "enrichers:\n  - name: en-cron\n    description: \"E\"\n    target: \"src-cron\"\n    script_path: \"e.py\"\n    schedule: \"45 2 * * *\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "projects:\n  - name: prj-test\n    description: \"Test\"\n    git_url: \"https://example.com/repo.git\"\n    schedule: \"15 2 * * *\"\n",
        )
        .unwrap();
        load_config(dir.path().to_str().unwrap()).expect("a valid cron schedule loads");
    }

    #[test]
    fn a_vault_path_without_a_vault_block_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("credentials.yaml"),
            "credentials:\n  - name: cred-v\n    description: \"V\"\n    type: \"token\"\n    vault_path: \"team/api\"\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a vault_path with nothing to resolve it must not load")
            .to_string();
        assert!(
            err.contains("names no vault to read it from"),
            "error was: {}",
            err
        );

        // The deprecated config.yaml block still resolves it
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  vault:\n    address: \"http://vault.example:8200\"\n",
        )
        .unwrap();
        load_config(dir.path().to_str().unwrap()).expect("a configured Vault validates");
    }

    // A Vault is a credential: others name it with `vault:`, nothing hands it
    // to a script, and config.yaml needs no secrets block.
    #[test]
    fn a_vault_credential_is_named_by_the_credentials_that_read_from_it() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("creds.yaml"),
            concat!(
                "credentials:\n",
                "  - name: cred-vault\n    type: vault\n    address: https://vault.example:8200\n",
                "    env_vars:\n      token: VAULT_TOKEN\n    cache_ttl_seconds: 300\n",
                "  - name: cred-d42\n    type: username_password\n    vault: cred-vault\n",
                "    vault_path: infra/d42\n",
            ),
        )
        .unwrap();
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.credentials["cred-d42"].backend(), "vault");
        assert!(cfg.deprecations().is_empty(), "{:?}", cfg.deprecations());
    }

    #[test]
    fn a_vault_credential_misused_is_refused_naming_why() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("creds.yaml"),
            concat!(
                "credentials:\n",
                // no address, no way to log in
                "  - name: cred-vault\n    type: vault\n",
                // names a credential that is not a Vault
                "  - name: cred-d42\n    type: token\n    vault: cred-gh\n    vault_path: x\n",
                "  - name: cred-gh\n    type: token\n    env_vars:\n      token: GH\n",
                // a Vault's setting on a login
                "  - name: cred-odd\n    type: token\n    address: https://x\n    env_vars:\n      token: T\n",
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-a\n    description: \"A\"\n    connector: c-src-a\n    ttl_seconds: 60\n    credentials: [cred-vault]\nconnectors:\n  - name: c-src-a\n    project: \"prj-test\"\n    path: \"x.py\"\n",
        )
        .unwrap();
        let err = load_err(&dir);
        for expected in [
            "is a Vault but has no address",
            "has no way to log in to Vault",
            "names 'cred-gh' as its vault, which is not a `type: vault` credential",
            "sets address, which only a `type: vault` credential uses",
            "Source 'src-a' uses credential 'cred-vault', which is a Vault connection",
        ] {
            assert!(err.contains(expected), "missing '{expected}' in: {err}");
        }
    }

    #[test]
    fn the_config_yaml_secrets_block_still_loads_and_says_what_to_write() {
        let cfg = config_with(
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  cache_ttl_seconds: 30\n",
        );
        assert_eq!(cfg.secrets_config.cache_ttl_seconds, Some(30));
        let notes = cfg.deprecations();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("the secrets block is deprecated"),
            "{notes:?}"
        );
    }

    #[test]
    fn validate_catches_source_with_unknown_credential() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "sources:\n  - name: src-test\n    description: \"Test\"\n    connector: c-src-test\n    credentials: [\"cred-missing\"]\n    ttl_seconds: 60\nconnectors:\n  - name: c-src-test\n    project: \"p\"\n    path: \"test.py\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("cred-missing"));
    }

    #[test]
    fn output_format_ansible_loads_and_native_is_refused() {
        let dir = dir_with(&[(
            "connectors/c.yaml",
            "connectors:\n  - name: inv\n    path: /opt/inv.py\n    output_format: ansible\n  - name: old\n    path: /opt/old.py\n    output_format: native\n",
        )]);
        // ansible is what every script prints and is read as it is; native
        // is refused by name.
        let strict = match load_config_detailed(dir.path()) {
            Ok(_) => panic!("native must be refused here"),
            Err(e) => e.to_string(),
        };
        assert!(!strict.contains("'inv'"), "{strict}");
        assert!(
            strict.contains("'old' sets output_format: native"),
            "{strict}"
        );
    }

    #[test]
    fn a_source_cannot_set_the_ssh_location_key_itself() {
        let dir = dir_with(&[
            (
                "connectors/c.yaml",
                "connectors:\n  - name: inline\n    type: ssh\n    script: uname -r\n",
            ),
            (
                "sources/s.yaml",
                "sources:\n  - name: src-i\n    connector: inline\n    ttl_seconds: 60\n    config:\n      hosts: a.example.com\n      ssh_location: host\n",
            ),
        ]);
        let err = match load_config_detailed(dir.path()) {
            Ok(_) => panic!("the key belongs to unified-api"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("config.ssh_location"), "{err}");
    }
}
