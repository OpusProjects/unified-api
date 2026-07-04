use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::domain::credential::Credential;
use crate::domain::endpoint::OutputEndpoint;
use crate::domain::enricher::Enricher;
use crate::domain::project::GitProject;
use crate::domain::source::Source;

pub struct AppConfig {
    pub server: ServerConfig,
    pub credentials: HashMap<String, Credential>,
    pub sources: HashMap<String, Source>,
    pub enrichers: HashMap<String, Enricher>,
    pub projects: HashMap<String, GitProject>,
    pub endpoints: HashMap<String, OutputEndpoint>,
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

// Intermediate struct to parse config.yaml (only has server for now)
#[derive(Deserialize)]
struct ServerFile {
    server: ServerConfig,
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

        // Sources must reference existing projects. The feature to clone git
        // repos does not exist yet, but projects.yaml is already loaded and
        // sources already declare project_id — better for a typo'd id to fail
        // at startup than when the feature arrives.
        for (id, source) in &self.sources {
            if !self.projects.contains_key(&source.project_id) {
                errors.push(format!(
                    "Source '{}' references unknown project '{}'",
                    id, source.project_id
                ));
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

    let config = AppConfig {
        server: server_file.server,
        credentials,
        sources,
        enrichers,
        projects,
        endpoints,
    };

    config.validate()?;

    Ok(config)
}

// Reads and parses a YAML file — fails if it does not exist
fn load_yaml_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<T, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    let parsed = serde_yaml::from_str(&contents)?;
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
