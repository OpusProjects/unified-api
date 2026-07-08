use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use tokio::process::Command;
use tracing::{debug, info};

use crate::domain::project::GitProject;
use crate::ports::git::{GitError, GitFuture, GitPort};

// GitPort implementation that shells out to the git binary — the same
// philosophy as the process connector: the tool everyone debugs with is the
// tool the app uses, so `git -C <dir> log` always tells the truth.
pub struct CliGit;

impl Default for CliGit {
    fn default() -> Self {
        Self::new()
    }
}

impl CliGit {
    pub fn new() -> Self {
        Self
    }
}

impl GitPort for CliGit {
    fn ensure(
        &self,
        dir: &Path,
        project: &GitProject,
        credentials: &HashMap<String, String>,
    ) -> GitFuture<'_> {
        let dir = dir.to_path_buf();
        let project = project.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            let envs = auth_env(&credentials);

            if dir.join(".git").exists() {
                // Existing checkout: fetch the branch tip and hard-reset to it.
                // FETCH_HEAD instead of origin/<branch> because shallow clones
                // don't always keep remote-tracking refs up to date.
                debug!(dir = %dir.display(), "Updating git checkout");
                run_git(
                    &[
                        "-C",
                        path_str(&dir)?,
                        "fetch",
                        "--depth",
                        "1",
                        "origin",
                        &project.branch,
                    ],
                    &envs,
                )
                .await?;
                run_git(
                    &["-C", path_str(&dir)?, "reset", "--hard", "FETCH_HEAD"],
                    &envs,
                )
                .await?;
            } else {
                if let Some(parent) = dir.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| GitError {
                            message: format!("create '{}': {}", parent.display(), e),
                        })?;
                }
                info!(url = %project.git_url, dir = %dir.display(), branch = %project.branch, "Cloning project");
                // --depth 1: connector scripts only need the tip, not history
                run_git(
                    &[
                        "clone",
                        "--depth",
                        "1",
                        "--branch",
                        &project.branch,
                        &project.git_url,
                        path_str(&dir)?,
                    ],
                    &envs,
                )
                .await?;
            }

            Ok(())
        })
    }
}

fn path_str(path: &Path) -> Result<&str, GitError> {
    path.to_str().ok_or_else(|| GitError {
        message: format!("path '{}' is not valid UTF-8", path.display()),
    })
}

// Translate resolved credentials into git authentication, WITHOUT putting the
// secret on the command line (argv is world-readable in /proc while git runs):
//
// - `ssh_key_path` (an SshKey credential's file_keys) → GIT_SSH_COMMAND
// - `token` or `username`+`password` → an http.extraHeader Basic auth header,
//   passed through GIT_CONFIG_* environment variables (git ≥ 2.31)
//
// Empty credentials = no env = anonymous access (public repos).
fn auth_env(credentials: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut envs: Vec<(String, String)> = Vec::new();

    if let Some(key_path) = credentials.get("ssh_key_path") {
        envs.push((
            "GIT_SSH_COMMAND".to_string(),
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
                key_path
            ),
        ));
        return envs;
    }

    // Token providers accept Basic auth with the token as password; the
    // username is mostly decorative (GitHub docs use x-access-token).
    let basic = if let Some(token) = credentials.get("token") {
        let user = credentials
            .get("username")
            .map(String::as_str)
            .unwrap_or("x-access-token");
        Some(format!("{}:{}", user, token))
    } else if let (Some(user), Some(password)) =
        (credentials.get("username"), credentials.get("password"))
    {
        Some(format!("{}:{}", user, password))
    } else {
        None
    };

    if let Some(userpass) = basic {
        let header = format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(userpass)
        );
        envs.push(("GIT_CONFIG_COUNT".to_string(), "1".to_string()));
        envs.push((
            "GIT_CONFIG_KEY_0".to_string(),
            "http.extraHeader".to_string(),
        ));
        envs.push(("GIT_CONFIG_VALUE_0".to_string(), header));
    }

    envs
}

async fn run_git(args: &[&str], envs: &[(String, String)]) -> Result<(), GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    // Never let git fall back to an interactive credential prompt — in a
    // container there is no terminal and the process would just hang.
    cmd.env("GIT_TERMINAL_PROMPT", "0");

    let output = cmd.output().await.map_err(|e| GitError {
        message: format!("failed to run git {:?}: {}", args.first().unwrap_or(&""), e),
    })?;

    if !output.status.success() {
        return Err(GitError {
            message: format!(
                "git {} failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_credentials_means_no_env() {
        assert!(auth_env(&HashMap::new()).is_empty());
    }

    #[test]
    fn ssh_key_sets_git_ssh_command() {
        let creds: HashMap<String, String> =
            [("ssh_key_path".to_string(), "/run/secrets/key".to_string())]
                .into_iter()
                .collect();
        let envs = auth_env(&creds);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "GIT_SSH_COMMAND");
        assert!(envs[0].1.contains("-i /run/secrets/key"));
    }

    #[test]
    fn token_sets_basic_auth_header_via_env() {
        let creds: HashMap<String, String> = [("token".to_string(), "s3cr3t".to_string())]
            .into_iter()
            .collect();
        let envs = auth_env(&creds);
        let header = envs
            .iter()
            .find(|(k, _)| k == "GIT_CONFIG_VALUE_0")
            .map(|(_, v)| v.clone())
            .unwrap();
        // "x-access-token:s3cr3t" in base64
        assert_eq!(header, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46czNjcjN0");
        // And the secret never appears in a command-line argument
        assert!(!envs.iter().any(|(_, v)| v.contains("s3cr3t")));
    }

    #[test]
    fn username_password_sets_basic_auth() {
        let creds: HashMap<String, String> = [
            ("username".to_string(), "batou".to_string()),
            ("password".to_string(), "rangiku".to_string()),
        ]
        .into_iter()
        .collect();
        let envs = auth_env(&creds);
        assert!(envs.iter().any(|(k, _)| k == "GIT_CONFIG_KEY_0"));
    }
}
