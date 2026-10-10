//! Error handling for GitMesh.
//!
//! Every error carries enough context to be shown to a human without further
//! processing: which repository, which directory, which command failed. Errors are
//! never silently swallowed; where an operation legitimately continues past a
//! failure (per-repository resilience in `crate::ops`), the failure is converted into
//! an explicit outcome record instead of being dropped.

use std::path::PathBuf;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level GitMesh error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Filesystem failure, annotated with the path that caused it.
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The `git` executable could not be found or executed.
    #[error("the git executable could not be run ({program}): {source}. GitMesh drives Git through the Git CLI and requires it on PATH.")]
    GitUnavailable {
        program: String,
        #[source]
        source: std::io::Error,
    },

    /// A Git command returned a non-zero exit status where success was required.
    #[error("git command failed: `{command}`{in_repo}\n{stderr}")]
    GitCommand {
        command: String,
        in_repo: String,
        stderr: String,
        #[allow(dead_code)]
        stdout: String,
        #[allow(dead_code)]
        code: Option<i32>,
    },

    /// The Git executable reported an unexpected, unparseable result.
    #[error("could not parse git output ({context}): {detail}")]
    GitParse { context: String, detail: String },

    /// Manifest is syntactically or semantically invalid.
    #[error("invalid GitMesh manifest: {0}")]
    Manifest(String),

    /// A semantic problem with the project configuration, given as a list of issues.
    #[error("project configuration is invalid:\n{}", format_issue_list(.0))]
    InvalidConfiguration(Vec<String>),

    /// No GitMesh project was found at (or above) the given path.
    #[error(
        "no GitMesh project found at or above {root} (expected a `{dir}/project.toml` manifest; \
         run `gitmesh init` to create one)",
        dir = crate::manifest::METADATA_DIR
    )]
    ProjectNotFound { root: PathBuf },

    /// A requested logical repository does not exist in the manifest.
    #[error("unknown repository '{0}' in this project")]
    UnknownRepository(String),

    /// The target path is not inside the project root.
    #[error("path {path} is outside the GitMesh project root {root}")]
    OutsideProject { path: PathBuf, root: PathBuf },

    /// The directory is not a Git repository / the operation requires one.
    #[error("{path} is not a Git repository")]
    NotARepository { path: PathBuf },

    /// A logical operation failed in at least one repository. The structured report is
    /// always available to the caller; this error is only used when the caller asked
    /// for a "fail fast" behaviour.
    #[error("{operation} failed in {} of {} repositories", .failures, .total)]
    OperationFailed {
        operation: String,
        failures: usize,
        total: usize,
    },

    /// Unsupported / not yet implemented feature, reported explicitly rather than
    /// doing something surprising.
    #[error("unsupported: {0}")]
    Unsupported(String),

    /// The command line asked for something impossible (bad path, conflicting
    /// options). Reported with the usage exit code.
    #[error("{0}")]
    Usage(String),

    /// Anything that does not fit the categories above.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Build an [`Error::Io`] with path context.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    /// Returns `true` when the error is a configuration/usage problem rather than an
    /// operational failure. Used to pick an exit code.
    pub fn is_configuration_error(&self) -> bool {
        matches!(
            self,
            Error::Manifest(_)
                | Error::InvalidConfiguration(_)
                | Error::UnknownRepository(_)
                | Error::OutsideProject { .. }
                | Error::Usage(_)
        )
    }
}

fn format_issue_list(issues: &[String]) -> String {
    issues
        .iter()
        .map(|i| format!("  - {i}"))
        .collect::<Vec<_>>()
        .join("\n")
}
