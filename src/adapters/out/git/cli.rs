// The GitPort that runs the git binary: git-rs's CliGit, re-exported so
// the composition root names it where the other driven adapters live. Its
// errors come back without any credentials written into a project's address.
pub use git_rs::CliGit;
