use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::domain::api_key::{ApiKeyDef, ApiKeyRole};
use crate::domain::credential::Credential;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::project::GitProject;
use crate::domain::source::Source;

pub struct AppConfig {
    pub server: ServerConfig,
    pub cache: CacheConfig,
    pub projects_config: ProjectsConfig,
    pub credentials: HashMap<String, Credential>,
    pub sources: HashMap<String, Source>,
    pub enrichers: HashMap<String, Enricher>,
    pub projects: HashMap<String, GitProject>,
    pub endpoints: HashMap<String, OutputEndpoint>,
    pub api_keys: HashMap<String, ApiKeyDef>,
}

// HTTP server configuration — config.yaml
#[derive(Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,

    // Origins allowed for CORS. Empty (the default) = no CORS headers at all,
    // which is right for server-to-server consumers. ["*"] = any origin.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,
}

// Cache behavior — config.yaml, `cache:` section (optional)
#[derive(Deserialize, Default)]
pub struct CacheConfig {
    // Without a `persistence` block the cache is purely in-memory (the
    // original behavior): nothing is ever written to disk.
    #[serde(default)]
    pub persistence: Option<PersistenceConfig>,
}

#[derive(Deserialize, Clone)]
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

// Where git projects are cloned — config.yaml, `projects:` section (optional)
#[derive(Deserialize)]
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
struct ServerFile {
    server: ServerConfig,
    #[serde(default)]
    cache: CacheConfig,
    #[serde(default)]
    projects: ProjectsConfig,
}

// Loads all configuration from a directory.
// Expects to find: config.yaml, credentials.yaml, sources.yaml, etc.
// Optional files are simply ignored if they do not exist.
impl AppConfig {
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        let mut errors: Vec<String> = Vec::new();

        // Enrichers must reference existing sources
        for (id, enricher) in &self.enrichers {
            if !self.sources.contains_key(&enricher.source_id) {
                errors.push(format!(
                    "Enricher '{}' references unknown source '{}'",
                    id, enricher.source_id
                ));
            }
        }

        // Endpoints must reference existing sources
        for (id, endpoint) in &self.endpoints {
            for source_id in &endpoint.source_ids {
                if !self.sources.contains_key(source_id) {
                    errors.push(format!(
                        "Endpoint '{}' references unknown source '{}'",
                        id, source_id
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
                    if !self.sources.contains_key(source_id) {
                        errors.push(format!(
                            "API key '{}' references unknown source '{}'",
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

    // After the project checkouts exist on disk, point script paths into them.
    // Called by main once the boot git sync ran; a no-op without projects.
    //
    // The rewrite is deliberately conservative — a path is only redirected
    // when the file actually exists inside the checkout:
    // - SSH sources are skipped (their script_path is a REMOTE command)
    // - absolute paths are kept as-is
    // - if the project failed to clone, or the file is not in it, the original
    //   path stays (it may be baked into the image or mounted) and syncs keep
    //   working exactly as before this feature existed
    pub fn resolve_script_paths(&mut self, projects_dir: &Path) {
        use crate::domain::source::ConnectorType;

        for (id, source) in self.sources.iter_mut() {
            if matches!(source.connector_type, ConnectorType::Ssh) {
                continue;
            }
            resolve_one(
                projects_dir,
                id,
                &source.project_id,
                &mut source.script_path,
            );
        }
        for (id, enricher) in self.enrichers.iter_mut() {
            if let Some(project_id) = enricher.project_id.clone() {
                resolve_one(projects_dir, id, &project_id, &mut enricher.script_path);
            }
        }
        for (id, endpoint) in self.endpoints.iter_mut() {
            if let Some(project_id) = endpoint.project_id.clone() {
                resolve_one(projects_dir, id, &project_id, &mut endpoint.script_path);
            }
        }
    }
}

fn resolve_one(projects_dir: &Path, owner_id: &str, project_id: &str, script_path: &mut String) {
    if Path::new(script_path.as_str()).is_absolute() {
        return;
    }
    let candidate = projects_dir.join(project_id).join(script_path.as_str());
    if candidate.is_file() {
        tracing::debug!(id = %owner_id, path = %candidate.display(), "Script resolved in project checkout");
        *script_path = candidate.to_string_lossy().into_owned();
    } else if projects_dir.join(project_id).is_dir() {
        // The checkout exists but the script is not in it — likely a typo in
        // config. Keep the original path (it may still resolve against the
        // working directory) but say something.
        tracing::warn!(
            id = %owner_id,
            project = %project_id,
            script = %script_path,
            "Script not found in project checkout, keeping the configured path"
        );
    }
}

pub fn load_config(config_dir: &str) -> Result<AppConfig, Box<dyn std::error::Error>> {
    let dir = Path::new(config_dir);

    // config.yaml is mandatory — without server config we cannot start
    let server_file: ServerFile = load_yaml_file(&dir.join("config.yaml"))?;

    // The rest are optional — if they do not exist, empty HashMap
    let credentials = load_optional_yaml(&dir.join("credentials.yaml"))?;
    let sources = load_optional_yaml(&dir.join("sources.yaml"))?;
    let enrichers = load_optional_yaml(&dir.join("enrichers.yaml"))?;
    let projects = load_optional_yaml(&dir.join("projects.yaml"))?;
    let endpoints = load_optional_yaml(&dir.join("endpoints.yaml"))?;
    let api_keys = load_optional_yaml(&dir.join("api_keys.yaml"))?;

    let config = AppConfig {
        server: server_file.server,
        cache: server_file.cache,
        projects_config: server_file.projects,
        credentials,
        sources,
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

        // Write credentials.yaml in map format
        fs::write(
            dir.path().join("credentials.yaml"),
            "cred-test:\n  name: \"Test\"\n  type: \"token\"\n  vault_path: \"secret/test\"\n",
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
            "enrich-test:\n  name: \"Test\"\n  source_id: \"src-nonexistent\"\n  script_path: \"test.py\"\n",
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
    fn resolve_script_paths_points_into_existing_checkout() {
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
            "src-test:\n  name: \"Test\"\n  project_id: \"prj-test\"\n  script_path: \"fetch.py\"\n  ttl_seconds: 60\n",
        ).unwrap();

        let mut cfg = load_config(dir.path().to_str().unwrap()).unwrap();

        // Simulate the checkout the git adapter would have produced
        let projects_dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(projects_dir.path().join("prj-test")).unwrap();
        fs::write(projects_dir.path().join("prj-test/fetch.py"), "#!/bin/sh\n").unwrap();

        cfg.resolve_script_paths(projects_dir.path());

        let resolved = &cfg.sources["src-test"].script_path;
        assert!(resolved.ends_with("prj-test/fetch.py"));
        assert!(
            Path::new(resolved).is_absolute()
                || resolved.starts_with(projects_dir.path().to_str().unwrap())
        );
    }

    #[test]
    fn resolve_script_paths_keeps_path_when_script_missing() {
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
            "src-test:\n  name: \"Test\"\n  project_id: \"prj-test\"\n  script_path: \"local/fetch.py\"\n  ttl_seconds: 60\n",
        ).unwrap();

        let mut cfg = load_config(dir.path().to_str().unwrap()).unwrap();

        // No checkout at all (clone failed / never ran): path must not change,
        // so scripts baked into the image keep working
        let projects_dir = tempfile::tempdir().unwrap();
        cfg.resolve_script_paths(projects_dir.path());

        assert_eq!(cfg.sources["src-test"].script_path, "local/fetch.py");
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
            "enrich-test:\n  name: \"Test\"\n  source_id: \"src-test\"\n  script_path: \"e.py\"\n  project_id: \"prj-ghost\"\n",
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
