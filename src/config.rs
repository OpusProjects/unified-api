use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use crate::domain::api_key::{ApiKeyDef, ApiKeyRole};
use crate::domain::credential::Credential;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::project::GitProject;
use crate::domain::source::Source;
use crate::domain::view::View;

pub struct AppConfig {
    pub server: ServerConfig,
    pub cache: CacheConfig,
    pub projects_config: ProjectsConfig,
    pub secrets_config: SecretsConfig,
    pub credentials: HashMap<String, Credential>,
    pub sources: HashMap<String, Source>,
    pub views: HashMap<String, View>,
    pub enrichers: HashMap<String, Enricher>,
    pub projects: HashMap<String, GitProject>,
    pub endpoints: HashMap<String, OutputEndpoint>,
    pub api_keys: HashMap<String, ApiKeyDef>,
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

// Secrets behavior — config.yaml, `secrets:` section (optional)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretsConfig {
    // How long a resolved credential may be reused before the backend is asked
    // again. Resolution runs on every sync of every source — free against env
    // vars, a request storm against a networked backend. The TTL is also the
    // rotation latency: a rotated secret is picked up within this many
    // seconds. 0 disables the cache (every sync resolves fresh).
    #[serde(default = "default_credential_cache_ttl")]
    pub cache_ttl_seconds: u64,

    // Native Vault resolution (KV v2). Credentials that carry `vault_path`
    // read from this Vault; those without keep resolving from env/files, so
    // adoption is per credential. Absent = no Vault, and any vault_path in
    // credentials.yaml fails validation at startup. The struct lives with the
    // adapter it configures (adapters/out/secrets/vault.rs).
    #[serde(default)]
    pub vault: Option<crate::adapters::out::secrets::vault::VaultConfig>,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            cache_ttl_seconds: default_credential_cache_ttl(),
            vault: None,
        }
    }
}

fn default_credential_cache_ttl() -> u64 {
    60
}

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
}

// Loads all configuration from a directory.
// Expects to find: config.yaml, credentials.yaml, sources.yaml, etc.
// Optional files are simply ignored if they do not exist.
impl AppConfig {
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut errors: Vec<String> = Vec::new();

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
                    "Enricher '{}' needs either source_id (declarative) or script_path (script)",
                    id
                ));
            }
        }

        // Endpoints must reference existing sources
        for (id, endpoint) in &self.endpoints {
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

        // Sources with credential_ids must reference existing credentials
        for (id, source) in &self.sources {
            for cred_id in &source.credential_ids {
                if !self.credentials.contains_key(cred_id) {
                    errors.push(format!(
                        "Source '{}' references unknown credential '{}'",
                        id, cred_id
                    ));
                }
            }
        }

        // Sources must reference existing projects — the checkout of that
        // project is where a relative script_path resolves first.
        for (id, source) in &self.sources {
            if !self.projects.contains_key(&source.project_id) {
                errors.push(format!(
                    "Source '{}' references unknown project '{}'",
                    id, source.project_id
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
                    "Source '{}' has an empty advertise_scope — name at least one                      group or host, or remove the block",
                    id
                ));
            }
        }

        // A cron schedule must parse, and a source cannot serve two masters:
        // schedule and sync_interval_seconds each define the cadence. The
        // field spent its first year as "reserved for future" and silently
        // ignored — precisely the config-that-does-nothing trap the strict
        // parsing exists to kill, so now that it works, junk in it fails loud.
        for (id, source) in &self.sources {
            if let Some(expression) = &source.schedule {
                if let Err(e) = crate::adapters::r#in::scheduler::parse_cron(expression) {
                    errors.push(format!(
                        "Source '{}' has an invalid cron schedule '{}': {}",
                        id, expression, e
                    ));
                }
                if source.sync_interval_seconds.is_some_and(|secs| secs > 0) {
                    errors.push(format!(
                        "Source '{}' sets both schedule and sync_interval_seconds — pick one",
                        id
                    ));
                }
            }
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

        // Remote (federation) sources need the remote base URL
        for (id, source) in &self.sources {
            if source.connector_type == crate::domain::source::ConnectorType::Remote
                && !source.config.contains_key("url")
            {
                errors.push(format!(
                    "Source '{}' is a remote source but has no 'url' in config",
                    id
                ));
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
        // credential.
        if self.secrets_config.vault.is_none() {
            for (id, credential) in &self.credentials {
                if credential.vault_path.is_some() {
                    errors.push(format!(
                        "Credential '{}' sets vault_path but config.yaml has no \
                         secrets.vault block to resolve it against",
                        id
                    ));
                }
            }
        }

        // Private projects must reference existing credentials
        for (id, project) in &self.projects {
            if let Some(ref cred_id) = project.credential_id
                && !self.credentials.contains_key(cred_id)
            {
                errors.push(format!(
                    "Project '{}' references unknown credential '{}'",
                    id, cred_id
                ));
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(format!("Configuration errors:\n  - {}", errors.join("\n  - ")).into())
        }
    }

    // Script paths are NOT resolved into project checkouts here. They used to
    // be (a one-shot rewrite after the boot clones), which coupled serving to
    // the clones finishing and froze whatever the disk looked like at startup.
    // Resolution now happens per execution — see application::scripts.
}

pub fn load_config(config_dir: &str) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let dir = Path::new(config_dir);

    // config.yaml is mandatory — without server config we cannot start
    let server_file: ServerFile = load_yaml_file(&dir.join("config.yaml"))?;

    // The rest are optional — if they do not exist, empty HashMap
    let credentials = load_optional_yaml(&dir.join("credentials.yaml"))?;
    let sources = load_optional_yaml(&dir.join("sources.yaml"))?;
    let views = load_optional_yaml(&dir.join("views.yaml"))?;
    let enrichers = load_optional_yaml(&dir.join("enrichers.yaml"))?;
    let projects = load_optional_yaml(&dir.join("projects.yaml"))?;
    let endpoints = load_optional_yaml(&dir.join("endpoints.yaml"))?;
    let api_keys = load_optional_yaml(&dir.join("api_keys.yaml"))?;

    let config = AppConfig {
        server: server_file.server,
        cache: server_file.cache,
        projects_config: server_file.projects,
        secrets_config: server_file.secrets,
        credentials,
        sources,
        views,
        enrichers,
        projects,
        endpoints,
        api_keys,
    };

    config.validate()?;

    Ok(config)
}

// Reads and parses a YAML file — fails if it does not exist
fn load_yaml_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<T, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let parsed = serde_yaml_ng::from_str(&contents)?;
    Ok(parsed)
}

// Reads and parses a YAML file — returns empty HashMap if it does not exist
// `T: DeserializeOwned` is a "trait bound": it says T must be deserializable.
// It's like a type constraint in TypeScript or a Protocol in Python.
fn load_optional_yaml<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<HashMap<String, T>, Box<dyn std::error::Error>> {
    if path.exists() {
        load_yaml_file(path)
    } else {
        Ok(HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Config tests need real files on disk.
    // We create a temporary directory with test YAML.

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
            "cred-test:\n  name: \"Test\"\n  type: \"token\"\n  env_prefix: \"TEST\"\n",
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
            "enrich-test:\n  name: \"Test\"\n  target_id: \"src-nonexistent\"\n  script_path: \"test.py\"\n",
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
            "ep-test:\n  name: \"Test\"\n  source_ids: [\"src-ghost\"]\n  script_path: \"test.py\"\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("src-ghost"));
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
            "src-test:\n  name: \"Test\"\n  project_id: \"prj-ghost\"\n  script_path: \"test.py\"\n  ttl_seconds: 60\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        let kh_path = dir.path().join("known_hosts");
        fs::write(
            dir.path().join("sources.yaml"),
            format!(
                "src-fleet:\n  name: \"Fleet\"\n  project_id: \"prj-test\"\n  script_path: \"unused\"\n  connector_type: \"ssh\"\n  ttl_seconds: 60\n  config:\n    hosts: \"a.example\"\n    ssh_known_hosts: \"{}\"\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "src-script:\n  name: \"Script\"\n  project_id: \"prj-test\"\n  script_path: \"test.py\"\n  ttl_seconds: 60\n  config:\n    ssh_known_hosts: \"/anywhere\"\n",
        )
        .unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("not an ssh source"), "{}", err);
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n  credential_id: \"cred-ghost\"\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "src-test:\n  name: \"Test\"\n  project_id: \"prj-test\"\n  script_path: \"test.py\"\n  ttl_seconds: 60\n",
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
            "src-test:\n  name: \"Test\"\n  project_id: \"prj-test\"\n  script_path: \"test.py\"\n  ttl_seconds: 60\n",
        ).unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("enrichers.yaml"),
            "enrich-test:\n  name: \"Test\"\n  target_id: \"src-test\"\n  script_path: \"e.py\"\n  project_id: \"prj-ghost\"\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        // three violations at once: not an ssh source, self-reference is
        // checked on the ssh one, and both hosts + hosts_from_source
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                "src-script:\n  name: \"S\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
                "  hosts_from_source:\n    source: \"src-ssh\"\n",
                "src-ssh:\n  name: \"T\"\n  project_id: \"prj-test\"\n  script_path: \"gather_facts\"\n  ttl_seconds: 60\n",
                "  connector_type: \"ssh\"\n",
                "  hosts_from_source:\n    source: \"src-ssh\"\n",
                "  config:\n    hosts: \"a.example.com\"\n",
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
    fn validate_hosts_from_source_unknown_source() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("projects.yaml"),
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                "src-ssh:\n  name: \"T\"\n  project_id: \"prj-test\"\n  script_path: \"gather_facts\"\n  ttl_seconds: 60\n",
                "  connector_type: \"ssh\"\n",
                "  hosts_from_source:\n    source: \"src-ghost\"\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            "src-a:\n  name: \"A\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
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

    #[test]
    fn validate_view_rules() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("views.yaml"),
            concat!(
                // id collides with a source, and its member is unknown
                "src-a:\n  name: \"Collides\"\n  members:\n",
                "    - source: \"src-ghost\"\n      owns:\n        source: \"src-a\"\n",
                // lists the same member twice, and owns against an unknown source
                "vw-dup:\n  name: \"Dup\"\n  members:\n",
                "    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n",
                "    - source: \"src-a\"\n      owns:\n        source: \"src-nowhere\"\n",
                // no members at all
                "vw-empty:\n  name: \"Empty\"\n  members: []\n",
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
                "vw-inner:\n  name: \"Inner\"\n  members:\n",
                "    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n",
                "vw-outer:\n  name: \"Outer\"\n  members:\n",
                "    - source: \"vw-inner\"\n      owns:\n        source: \"src-a\"\n",
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
            "vw-a:\n  name: \"V\"\n  members:\n    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("endpoints.yaml"),
            "ep-a:\n  name: \"E\"\n  source_ids: [\"vw-a\"]\n  script_path: \"t.py\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("enrichers.yaml"),
            "en-a:\n  name: \"E\"\n  target_id: \"vw-a\"\n  script_path: \"t.py\"\n",
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
            "vw-a:\n  name: \"V\"\n  members:\n    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("api_keys.yaml"),
            "key-forms:\n  name: \"Forms\"\n  env: \"X\"\n  sources: [\"vw-a\"]\n",
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
            "vw-a:\n  name: \"V\"\n  members:\n    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n        grups: [\"x\"]\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        // sync_interval_second (no s): the sync interval this operator thinks
        // they configured would silently not exist
        fs::write(
            dir.path().join("sources.yaml"),
            "src-test:\n  name: \"Test\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n  sync_interval_second: 300\n",
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
            "key-forms:\n  name: \"Forms\"\n  env: \"X\"\n  source: [\"src-a\"]\n",
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
            "vw-a:\n  name: \"V\"\n  members:\n    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n        advertised: true\n",
        )
        .unwrap();
        assert!(load_err(&dir).contains("claim nothing"));

        // A declared fallback makes it valid
        fs::write(
            dir.path().join("views.yaml"),
            "vw-a:\n  name: \"V\"\n  members:\n    - source: \"src-a\"\n      owns:\n        source: \"src-a\"\n        advertised: true\n        groups: [\"dc1\"]\n",
        )
        .unwrap();
        load_config(dir.path().to_str().unwrap()).expect("fallback makes it valid");
    }

    #[test]
    fn an_empty_advertise_scope_fails_validation() {
        let dir = dir_with_one_source();
        fs::write(
            dir.path().join("sources.yaml"),
            "src-a:\n  name: \"A\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n  advertise_scope: {}\n",
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
            "prj-test:\n  name: \"Test\"\n  git_url: \"https://example.com/repo.git\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("sources.yaml"),
            concat!(
                // junk in the schedule
                "src-bad:\n  name: \"B\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
                "  schedule: \"whenever feels right\"\n",
                // schedule AND interval
                "src-both:\n  name: \"C\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n",
                "  schedule: \"0 2 * * *\"\n  sync_interval_seconds: 60\n",
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

        // A well-formed cron-only source loads
        fs::write(
            dir.path().join("sources.yaml"),
            "src-cron:\n  name: \"OK\"\n  project_id: \"prj-test\"\n  script_path: \"x.py\"\n  ttl_seconds: 60\n  schedule: \"30 2 * * *\"\n",
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
            "cred-v:\n  name: \"V\"\n  type: \"token\"\n  vault_path: \"team/api\"\n",
        )
        .unwrap();

        let err = load_config(dir.path().to_str().unwrap())
            .err()
            .expect("a vault_path with nothing to resolve it must not load")
            .to_string();
        assert!(err.contains("secrets.vault"), "error was: {}", err);

        // Declaring the Vault is all it takes
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  vault:\n    address: \"http://vault.example:8200\"\n",
        )
        .unwrap();
        load_config(dir.path().to_str().unwrap()).expect("a configured Vault validates");
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
            "src-test:\n  name: \"Test\"\n  project_id: \"p\"\n  script_path: \"test.py\"\n  credential_ids: [\"cred-missing\"]\n  ttl_seconds: 60\n",
        ).unwrap();

        let result = load_config(dir.path().to_str().unwrap());
        assert!(result.is_err());
        let err = result.err().expect("expected validation error").to_string();
        assert!(err.contains("cred-missing"));
    }
}
