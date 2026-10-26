use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use crate::domain::project::GitProject;

// Boxed future alias, same pattern as the other ports (see connector.rs
// for why Pin<Box<dyn Future>>).
pub type GitFuture<'a> = Pin<Box<dyn Future<Output = Result<(), GitError>> + Send + 'a>>;
pub type RevisionFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

// GitPort — keep a local checkout of a project's repository up to date.
// The concrete implementation shells out to the git binary; the trait exists
// so tests (and a future libgit2/gix adapter) can swap it.
pub trait GitPort: Send + Sync {
    // Make `dir` an up-to-date checkout of the project's branch:
    // clone when the directory has no checkout yet, fetch + reset when it does.
    // `credentials` come already resolved from the SecretsPort (empty for
    // public repos).
    fn ensure(
        &self,
        dir: &Path,
        project: &GitProject,
        credentials: &HashMap<String, String>,
    ) -> GitFuture<'_>;

    // The commit the checkout in `dir` is at (full id), for the projects page:
    // "which scripts are running" is answered by a commit. None when it cannot
    // tell (no checkout yet); the default lets test doubles leave it out.
    fn revision(&self, _dir: &Path) -> RevisionFuture<'_> {
        Box::pin(async { None })
    }
}

#[derive(Debug)]
pub struct GitError {
    pub message: String,
}
