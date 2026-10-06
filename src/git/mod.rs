//! Git integration: the only layer that talks to the Git CLI.
//!
//! Nothing above this layer constructs a Git command line, and nothing in this layer
//! knows about GitMesh projects, manifests or orchestration.

pub mod command;
pub mod status;

pub use command::{CommandOutput, GitRepo, GitRunner, InProgressOperation};
pub use status::{
    classify_remote, parse_porcelain_v2, short_oid, ChangeKind, Head, Remote, RemoteKind,
    RepoStatus, StatusEntry, UnmergedCode,
};
