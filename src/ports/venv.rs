use std::future::Future;
use std::path::Path;
use std::pin::Pin;

// VenvPort — keep a project's Python virtualenv in step with the
// `requirements.txt` in its checkout. The concrete implementation shells out
// to `python3 -m venv` and pip; the trait exists so the project-sync use case
// can be tested without either.
//
// Where things live and how scripts find them are part of the port's
// contract, shared by the adapter that builds venvs and the execution paths
// that use them:

// Virtualenvs live OUTSIDE the checkouts (`<projects.dir>/.venvs/<project>`):
// a pull hard-resets its checkout, and a venv inside it would be wiped — or
// worse, half-wiped — on every update.
pub const VENVS_DIR: &str = ".venvs";

// The reserved config key execution paths use to hand a venv's bin directory
// to the process adapters, which prepend it to the child's PATH — the same
// internal-contract channel `hosts_spec` and `scope` already travel through.
// Scripts see it inside SOURCE_CONFIG/ENDPOINT_CONFIG, which is harmless and
// tells them which environment they run in.
pub const VENV_BIN_CONFIG_KEY: &str = "python_venv_bin";

pub type VenvFuture<'a> = Pin<Box<dyn Future<Output = Result<VenvOutcome, VenvError>> + Send + 'a>>;

#[derive(Debug, PartialEq, Eq)]
pub enum VenvOutcome {
    // Created the venv and/or installed requirements
    Installed,
    // requirements.txt is unchanged since the last install — nothing ran
    Unchanged,
    // The checkout has no requirements.txt: nothing to build, not an error
    NoRequirements,
}

pub trait VenvPort: Send + Sync {
    // Bring `<projects_dir>/.venvs/<project_id>` in step with
    // `<projects_dir>/<project_id>/requirements.txt`. The caller bounds this
    // with the project's timeout_seconds, like the git operation before it.
    fn ensure(&self, projects_dir: &Path, project_id: &str) -> VenvFuture<'_>;
}

#[derive(Debug)]
pub struct VenvError {
    pub message: String,
}
