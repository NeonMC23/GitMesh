//! Application/service layer.
//!
//! This module is the front-end-independent face of GitMesh. It exists so that the
//! CLI, the terminal UI and the GUI all drive **the same** project operations, and so
//! that a front end never has to know how a logical operation is decomposed into
//! physical Git commands.
//!
//! It contains **no Git logic**: every method either reads the project through
//! [`crate::analyzer`] / [`crate::manifest`] or delegates to [`crate::ops`]. What it
//! does own is the *presentation contract* shared by front ends:
//!
//! * [`ProjectSession`] — open a project once, then ask it for status and operations.
//! * [`ChangeState`] / [`RepositoryStateKind`] / [`ProjectStateKind`] — the vocabulary
//!   used to describe changes and health, so the CLI and the GUI cannot disagree.
//! * JSON view models ([`status_view_json`], [`operation_view_json`]) — the payload a
//!   non-TOML front end (the GUI) consumes, built from core types only.
//!
//! Backwards compatibility: the text labels the CLI prints for `status --changes` are
//! reproduced exactly by [`ChangeState::status_column_label`]; the finer-grained
//! labels ([`ChangeState::label`]) are used by richer front ends (the GUI), which have
//! no legacy output to preserve.

use std::path::{Path, PathBuf};

use crate::analyzer::{Analyzer, OwnedChange, ProjectStatus};
use crate::discovery;
use crate::error::Result;
use crate::git::status::{ChangeKind, Head, StatusEntry};
use crate::git::GitRunner;
use crate::json::Json;
use crate::manage;
use crate::manifest;
use crate::model::{GitMeshProject, RepositoryState};
use crate::ops::{
    self, BranchAction, BranchOptions, CommitOptions, OperationObserver, OperationReport,
    OutcomeKind, PushOptions, SyncOptions,
};
use crate::paths::to_slash;
use crate::setup::{self, StepState};

// ------------------------------------------------------------------- session --

/// An opened GitMesh project, with the Git runner it was opened with.
///
/// The session is cheap to clone-construct but not cheap to run: every call that
/// inspects or changes state spawns real Git processes. It is the unit of work a front
/// end holds on to, and it never caches project state — "reopen the project" and
/// "refresh" are therefore the same operation, which is what makes a GUI reload
/// behave exactly like a fresh process.
#[derive(Debug, Clone)]
pub struct ProjectSession {
    runner: GitRunner,
    project: GitMeshProject,
}

impl ProjectSession {
    /// Open the project that contains `start` (the project root itself or any
    /// directory below it).
    ///
    /// Fails with [`crate::Error::ProjectNotFound`] when no `.gitmesh/project.toml`
    /// exists at or above `start`, and with the manifest validation error when the
    /// file exists but is malformed. Both are configuration errors, which the CLI and
    /// the GUI surface to the user instead of guessing.
    pub fn open(start: &Path) -> Result<Self> {
        let runner = GitRunner::detect()?;
        let project = manifest::load_nearest(start)?;
        Ok(ProjectSession { runner, project })
    }

    /// Build a session from an already loaded project (used by tests and by front
    /// ends that constructed the project in memory).
    pub fn with_project(project: GitMeshProject, runner: GitRunner) -> Self {
        ProjectSession { runner, project }
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    pub fn project(&self) -> &GitMeshProject {
        &self.project
    }

    pub fn name(&self) -> &str {
        &self.project.name
    }

    pub fn root(&self) -> &Path {
        &self.project.root
    }

    /// Absolute path of the manifest that defines this project.
    pub fn manifest_path(&self) -> PathBuf {
        manifest::manifest_path(&self.project.root)
    }

    /// Analyzer over this project.
    pub fn analyzer(&self) -> Analyzer<'_> {
        Analyzer::new(&self.project, &self.runner)
    }

    /// Unified status of every configured repository.
    pub fn status(&self) -> ProjectStatus {
        self.analyzer().analyze()
    }

    /// Unified status together with the analyzer that produced it, for callers that
    /// need change ownership or JSON rendering as well.
    pub fn status_with_analyzer(&self) -> (Analyzer<'_>, ProjectStatus) {
        let analyzer = self.analyzer();
        let status = analyzer.analyze();
        (analyzer, status)
    }

    /// Every change in the project, with the owning repository.
    pub fn changes(&self) -> Vec<OwnedChange> {
        let (analyzer, status) = self.status_with_analyzer();
        analyzer.owned_changes(&status)
    }

    /// Changes that are merge conflicts.
    pub fn conflicts(&self) -> Vec<OwnedChange> {
        self.changes()
            .into_iter()
            .filter(|c| c.is_conflict())
            .collect()
    }

    /// Machine-readable status, identical to the CLI's `status --json`.
    pub fn status_json(&self) -> String {
        let (analyzer, status) = self.status_with_analyzer();
        analyzer.status_json(&status).to_pretty_string()
    }

    // -------------------------------------------------------------- operations --
    //
    // The four methods below are the complete set of mutating operations a front end
    // may perform. They add nothing to `crate::ops` except the session's runner and
    // project; in particular they do not classify, retry, reorder or reinterpret
    // outcomes. That classification is produced by the core and consumed verbatim.

    /// One logical commit across the project (one real commit per affected
    /// repository).
    pub fn commit(&self, options: &CommitOptions) -> Result<OperationReport> {
        ops::commit_project(&self.project, &self.runner, options)
    }

    /// One logical branch operation across the project.
    pub fn branch(
        &self,
        action: &BranchAction,
        options: &BranchOptions,
    ) -> Result<OperationReport> {
        ops::branch_operation(&self.project, &self.runner, action, options)
    }

    /// One logical commit, reporting each repository to an observer as it is reached.
    ///
    /// This is what long-running front ends (the GUI) use; the plain [`ProjectSession::commit`]
    /// is exactly this with a silent observer.
    pub fn commit_observed(
        &self,
        options: &CommitOptions,
        observer: &mut OperationObserver<'_>,
    ) -> Result<OperationReport> {
        ops::commit_project_observed(&self.project, &self.runner, options, observer)
    }

    /// One logical branch operation, reporting each repository to an observer.
    pub fn branch_observed(
        &self,
        action: &BranchAction,
        options: &BranchOptions,
        observer: &mut OperationObserver<'_>,
    ) -> Result<OperationReport> {
        ops::branch_operation_observed(&self.project, &self.runner, action, options, observer)
    }

    /// Fetch (`fetch_only`) or pull every selected repository, reporting progress.
    pub fn sync_observed(
        &self,
        options: &SyncOptions,
        fetch_only: bool,
        observer: &mut OperationObserver<'_>,
    ) -> Result<OperationReport> {
        if fetch_only {
            ops::fetch_project_observed(&self.project, &self.runner, options, observer)
        } else {
            ops::pull_project_observed(&self.project, &self.runner, options, observer)
        }
    }

    /// Push every selected repository, reporting each repository to an observer.
    pub fn push_observed(
        &self,
        options: &PushOptions,
        observer: &mut OperationObserver<'_>,
    ) -> Result<OperationReport> {
        ops::push_project_observed(&self.project, &self.runner, options, observer)
    }

    /// Fetch or pull every selected repository.
    pub fn sync(&self, options: &SyncOptions, fetch_only: bool) -> Result<OperationReport> {
        if fetch_only {
            ops::fetch_project(&self.project, &self.runner, options)
        } else {
            ops::pull_project(&self.project, &self.runner, options)
        }
    }

    /// Push every selected repository.
    pub fn push(&self, options: &PushOptions) -> Result<OperationReport> {
        ops::push_project(&self.project, &self.runner, options)
    }
}

// ------------------------------------------------------------- vocabulary --

/// How one change looks to a user, independent of which Git field produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeState {
    /// Unmerged (merge conflict).
    Conflict,
    /// New file that Git does not track yet.
    Untracked,
    /// Staged and modified again afterwards.
    StagedAndModified,
    /// Staged only.
    Staged,
    /// Deleted in the work tree or the index.
    Deleted,
    /// Renamed.
    Renamed,
    /// Copied.
    Copied,
    /// Type change (file, symlink, submodule).
    TypeChanged,
    /// Modified (the default).
    Modified,
    /// Ignored by Git (only present when ignored entries are requested).
    Ignored,
}

impl ChangeState {
    /// Fine-grained label for rich front ends (the GUI's changes view).
    pub fn label(self) -> &'static str {
        match self {
            ChangeState::Conflict => "conflicted",
            ChangeState::Untracked => "untracked",
            ChangeState::StagedAndModified => "staged and modified",
            ChangeState::Staged => "staged",
            ChangeState::Deleted => "deleted",
            ChangeState::Renamed => "renamed",
            ChangeState::Copied => "copied",
            ChangeState::TypeChanged => "type changed",
            ChangeState::Modified => "modified",
            ChangeState::Ignored => "ignored",
        }
    }

    /// Stable machine-readable name (used by the GUI protocol).
    pub fn key(self) -> &'static str {
        match self {
            ChangeState::Conflict => "conflict",
            ChangeState::Untracked => "untracked",
            ChangeState::StagedAndModified => "staged_and_modified",
            ChangeState::Staged => "staged",
            ChangeState::Deleted => "deleted",
            ChangeState::Renamed => "renamed",
            ChangeState::Copied => "copied",
            ChangeState::TypeChanged => "type_changed",
            ChangeState::Modified => "modified",
            ChangeState::Ignored => "ignored",
        }
    }

    /// True when this state must be resolved before the repository can be committed.
    pub fn blocks_commit(self) -> bool {
        matches!(self, ChangeState::Conflict)
    }
}

/// Classify a status entry for display.
///
/// A file that is *deleted*, *renamed*, *copied* or whose type changed says more about
/// what happened than "staged", so those states win over the staging flags. Conflicts
/// and untracked files win over everything.
pub fn change_state(entry: &StatusEntry) -> ChangeState {
    if entry.unmerged.is_some() {
        return ChangeState::Conflict;
    }
    if entry.untracked {
        return ChangeState::Untracked;
    }
    if entry.ignored {
        return ChangeState::Ignored;
    }
    match entry.kind() {
        ChangeKind::Deleted => return ChangeState::Deleted,
        ChangeKind::Renamed => return ChangeState::Renamed,
        ChangeKind::Copied => return ChangeState::Copied,
        ChangeKind::TypeChanged => return ChangeState::TypeChanged,
        _ => {}
    }
    if entry.staged && entry.unstaged {
        return ChangeState::StagedAndModified;
    }
    if entry.staged {
        return ChangeState::Staged;
    }
    ChangeState::Modified
}

/// The label `gitmesh status --changes` has always printed for a change.
///
/// This is the CLI's compatibility contract: it is defined by the staging flags only,
/// exactly as before the GUI existed, and must not follow the finer-grained
/// [`ChangeState`] classification (a staged rename still prints `staged`). Front ends
/// that are free to be more precise use [`ChangeState::label`].
pub fn status_column_label(entry: &StatusEntry) -> &'static str {
    if entry.is_conflict() {
        "conflict"
    } else if entry.untracked {
        "untracked"
    } else if entry.staged && entry.unstaged {
        "staged+modified"
    } else if entry.staged {
        "staged"
    } else {
        "modified"
    }
}

/// Classify an owned change for display.
pub fn owned_change_state(change: &OwnedChange) -> ChangeState {
    change_state(&change.entry)
}

/// Health of a single repository, as one value a front end can switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryStateKind {
    /// Missing directory, not a Git repository, or an inspection error.
    Unavailable,
    /// Has unmerged files.
    Conflicted,
    /// Has changes.
    Changed,
    /// Nothing to record.
    Clean,
}

impl RepositoryStateKind {
    pub fn label(self) -> &'static str {
        match self {
            RepositoryStateKind::Unavailable => "unavailable",
            RepositoryStateKind::Conflicted => "conflicted",
            RepositoryStateKind::Changed => "modified",
            RepositoryStateKind::Clean => "clean",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            RepositoryStateKind::Unavailable => "unavailable",
            RepositoryStateKind::Conflicted => "conflicted",
            RepositoryStateKind::Changed => "changed",
            RepositoryStateKind::Clean => "clean",
        }
    }
}

/// Health of one repository state.
pub fn repository_state_kind(state: &RepositoryState) -> RepositoryStateKind {
    if state.error.is_some() || !state.exists || !state.is_repository {
        return RepositoryStateKind::Unavailable;
    }
    if state.has_conflicts() {
        return RepositoryStateKind::Conflicted;
    }
    if state.has_changes() {
        return RepositoryStateKind::Changed;
    }
    RepositoryStateKind::Clean
}

/// Health of the whole logical project (the worst state of its repositories).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStateKind {
    /// At least one repository cannot be inspected.
    Unavailable,
    /// At least one repository has conflicts.
    Conflicted,
    /// At least one repository has changes.
    Changed,
    /// Everything is committed and in sync as far as Git knows.
    Clean,
}

impl ProjectStateKind {
    pub fn label(self) -> &'static str {
        match self {
            ProjectStateKind::Unavailable => "unavailable",
            ProjectStateKind::Conflicted => "conflicted",
            ProjectStateKind::Changed => "modified",
            ProjectStateKind::Clean => "clean",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            ProjectStateKind::Unavailable => "unavailable",
            ProjectStateKind::Conflicted => "conflicted",
            ProjectStateKind::Changed => "changed",
            ProjectStateKind::Clean => "clean",
        }
    }

    /// True when the state must be resolved before routine work.
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            ProjectStateKind::Unavailable | ProjectStateKind::Conflicted
        )
    }
}

/// Health of the whole project.
pub fn project_state_kind(status: &ProjectStatus) -> ProjectStateKind {
    let mut kind = ProjectStateKind::Clean;
    for state in &status.repositories {
        match repository_state_kind(state) {
            RepositoryStateKind::Unavailable => return ProjectStateKind::Unavailable,
            RepositoryStateKind::Conflicted => kind = ProjectStateKind::Conflicted,
            RepositoryStateKind::Changed if kind == ProjectStateKind::Clean => {
                kind = ProjectStateKind::Changed
            }
            _ => {}
        }
    }
    kind
}

/// The branch column of the status table (`-` for repositories that cannot be read).
pub fn branch_cell(state: &RepositoryState) -> String {
    if !state.is_usable() {
        return "-".to_string();
    }
    match state.head() {
        Head::Unknown => "-".to_string(),
        head => head.label(),
    }
}

/// One branch as it exists across the physical repositories.
///
/// This is the "one logical branch" view: a branch may exist in every repository, in
/// some of them, or be checked out in exactly one. GitMesh never invents a branch that
/// does not exist, and never hides the repositories that differ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectBranch {
    /// Branch name.
    pub name: String,
    /// Repositories that have this branch.
    pub present_in: Vec<String>,
    /// Repositories that currently have it checked out.
    pub checked_out_in: Vec<String>,
}

impl ProjectBranch {
    /// True when every repository has this branch.
    pub fn is_everywhere(&self, repository_count: usize) -> bool {
        self.present_in.len() == repository_count
    }

    /// True when this branch is checked out in more than one repository.
    pub fn is_split(&self) -> bool {
        self.checked_out_in.len() > 1
    }
}

/// Every local branch of every readable repository, merged into one list.
///
/// Branch listing is a read-only convenience for front ends; failures to read one
/// repository are reported in the second element of the tuple instead of hiding the
/// branches of the others.
pub fn project_branches(session: &ProjectSession) -> (Vec<ProjectBranch>, Vec<String>) {
    let mut branches: Vec<ProjectBranch> = Vec::new();
    let mut notices: Vec<String> = Vec::new();
    for repo in session.project().sorted_repositories() {
        let names = match discovery::verified_repo(session.runner(), repo, session.root())
            .and_then(|handle| handle.local_branches())
        {
            Ok(names) => names,
            Err(err) => {
                notices.push(format!("{}: {err}", repo.id));
                continue;
            }
        };
        // Which branch is checked out right now, so the UI can mark it.
        let current = session
            .status()
            .repositories
            .iter()
            .find(|state| state.id == repo.id)
            .and_then(|state| state.branch().map(|branch| branch.to_string()));
        for name in names {
            let entry = match branches.iter_mut().find(|b| b.name == name) {
                Some(entry) => entry,
                None => {
                    branches.push(ProjectBranch {
                        name: name.clone(),
                        present_in: Vec::new(),
                        checked_out_in: Vec::new(),
                    });
                    branches.last_mut().expect("just pushed")
                }
            };
            entry.present_in.push(repo.id.clone());
            if current.as_deref() == Some(name.as_str()) {
                entry.checked_out_in.push(repo.id.clone());
            }
        }
    }
    branches.sort_by(|a, b| a.name.cmp(&b.name));
    (branches, notices)
}

/// Machine-readable view of the branch list.
pub fn branches_view_json(session: &ProjectSession) -> Json {
    let (branches, notices) = project_branches(session);
    let total = session.project().repositories.len();
    Json::object([
        (
            "list",
            Json::array(branches.iter().map(|branch| {
                Json::object([
                    ("name", Json::from(branch.name.clone())),
                    (
                        "presentIn",
                        Json::array(branch.present_in.iter().map(|id| Json::from(id.as_str()))),
                    ),
                    (
                        "checkedOutIn",
                        Json::array(
                            branch
                                .checked_out_in
                                .iter()
                                .map(|id| Json::from(id.as_str())),
                        ),
                    ),
                    ("everywhere", Json::from(branch.is_everywhere(total))),
                ])
            })),
        ),
        ("notices", Json::array(notices.into_iter().map(Json::from))),
    ])
}

/// A repository with work left to do, as shown in the commit view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWork {
    /// Logical repository id.
    pub id: String,
    /// Project-relative path.
    pub path: String,
    /// Number of changes in this repository.
    pub files: usize,
    /// Number of those changes that are conflicts.
    pub conflicts: usize,
    /// Repository health.
    pub state: RepositoryStateKind,
    /// True when this repository cannot be committed until conflicts are resolved.
    pub blocked: bool,
}

impl PendingWork {
    /// One-line description, e.g. `engine: 3 files (1 conflict)`.
    pub fn summary(&self) -> String {
        let mut text = format!("{} file(s)", self.files);
        if self.conflicts > 0 {
            text.push_str(&format!(", {} conflict(s)", self.conflicts));
        }
        format!("{}: {text}", self.id)
    }
}

/// Repositories that currently have something to commit, in project order.
///
/// A repository whose *only* content is conflicts is included and marked
/// [`PendingWork::blocked`], because GitMesh will not commit a conflict — that is
/// exactly what the commit view has to tell the user before they press the button.
pub fn pending_work(session: &ProjectSession, status: &ProjectStatus) -> Vec<PendingWork> {
    let analyzer = session.analyzer();
    let mut work: Vec<PendingWork> = Vec::new();
    for change in analyzer.owned_changes(status) {
        let index = match work.iter().position(|w| w.id == change.repository_id) {
            Some(index) => index,
            None => {
                work.push(PendingWork {
                    id: change.repository_id.clone(),
                    path: change.repository_path.clone(),
                    files: 0,
                    conflicts: 0,
                    state: RepositoryStateKind::Clean,
                    blocked: false,
                });
                work.len() - 1
            }
        };
        work[index].files += 1;
        if change.is_conflict() {
            work[index].conflicts += 1;
        }
    }
    // Add repositories that are unavailable: they cannot be committed either, and the
    // user must see them rather than have them silently dropped from the operation.
    for state in &status.repositories {
        if repository_state_kind(state) == RepositoryStateKind::Unavailable
            && !work.iter().any(|w| w.id == state.id)
        {
            work.push(PendingWork {
                id: state.id.clone(),
                path: state.relative_path.clone(),
                files: 0,
                conflicts: 0,
                state: RepositoryStateKind::Unavailable,
                blocked: true,
            });
        }
    }
    // Repository health and the blocked flag, now that the counts are complete.
    for entry in &mut work {
        let kind = status
            .repositories
            .iter()
            .find(|r| r.id == entry.id)
            .map(repository_state_kind)
            .unwrap_or(RepositoryStateKind::Clean);
        entry.state = kind;
        entry.blocked = entry.conflicts > 0 || kind == RepositoryStateKind::Unavailable;
    }
    // Project order is the order of `status.repositories`; sort accordingly.
    work.sort_by_key(|w| {
        status
            .repositories
            .iter()
            .position(|r| r.id == w.id)
            .unwrap_or(usize::MAX)
    });
    work
}

// -------------------------------------------------------------- JSON views --

/// Machine-readable view of the project as a whole (GUI "project header").
pub fn project_view_json(session: &ProjectSession, status: &ProjectStatus) -> Json {
    let project = session.project();
    let root = project.root_repository();
    let kind = project_state_kind(status);
    let reference = status.reference_branch();
    let inconsistent: Vec<String> = status
        .inconsistent_branches()
        .iter()
        .map(|state| state.id.clone())
        .collect();
    let counts = (
        status.changed().count(),
        status.clean().count(),
        status.conflicted().count(),
        status.unavailable().count(),
    );

    Json::object([
        ("opened", Json::from(true)),
        ("name", Json::from(project.name.clone())),
        ("root", Json::from(to_slash(&project.root))),
        ("manifest", Json::from(to_slash(&session.manifest_path()))),
        (
            "rootRepository",
            Json::object([
                ("id", Json::from(root.id.clone())),
                ("path", Json::from(root.relative_slash())),
            ]),
        ),
        (
            "branch",
            Json::object([
                ("name", Json::opt(reference.clone())),
                ("consistent", Json::from(inconsistent.is_empty())),
                (
                    "outliers",
                    Json::array(inconsistent.into_iter().map(Json::from)),
                ),
            ]),
        ),
        (
            "state",
            Json::object([
                ("key", Json::from(kind.key())),
                ("label", Json::from(kind.label())),
                ("needsAttention", Json::from(kind.needs_attention())),
            ]),
        ),
        (
            "counts",
            Json::object([
                ("repositories", Json::from(status.repositories.len())),
                ("changed", Json::from(counts.0)),
                ("clean", Json::from(counts.1)),
                ("conflicted", Json::from(counts.2)),
                ("unavailable", Json::from(counts.3)),
                ("changes", Json::from(status.total_changes())),
            ]),
        ),
        (
            "notices",
            Json::array(status.notices.iter().map(|n| Json::from(n.as_str()))),
        ),
    ])
}

/// Machine-readable view of one repository row.
pub fn repository_view_json(state: &RepositoryState) -> Json {
    let kind = repository_state_kind(state);
    let counts = state
        .status
        .as_ref()
        .map(crate::model::change_counts)
        .unwrap_or_default();
    Json::object([
        ("id", Json::from(state.id.clone())),
        ("role", Json::from(state.role.label())),
        ("path", Json::from(state.relative_path.clone())),
        ("absolutePath", Json::from(to_slash(&state.path))),
        ("branch", Json::opt(state.branch())),
        ("head", Json::from(branch_cell(state))),
        (
            "state",
            Json::object([
                ("key", Json::from(kind.key())),
                ("label", Json::from(kind.label())),
            ]),
        ),
        ("exists", Json::from(state.exists)),
        ("isRepository", Json::from(state.is_repository)),
        ("changed", Json::from(state.has_changes())),
        ("conflicted", Json::from(state.has_conflicts())),
        ("summary", Json::from(state.summary())),
        (
            "counts",
            Json::object([
                ("staged", Json::from(counts.staged)),
                ("unstaged", Json::from(counts.unstaged)),
                ("untracked", Json::from(counts.untracked)),
                ("conflict", Json::from(counts.conflict)),
            ]),
        ),
        (
            "sync",
            Json::object([
                (
                    "upstream",
                    Json::opt(state.status.as_ref().and_then(|s| s.upstream.clone())),
                ),
                ("ahead", Json::from(state.ahead().unwrap_or(0))),
                ("behind", Json::from(state.behind().unwrap_or(0))),
            ]),
        ),
        (
            "remotes",
            Json::array(state.remotes.iter().map(|remote| {
                Json::object([
                    ("name", Json::from(remote.name.clone())),
                    (
                        "url",
                        Json::opt(remote.fetch_urls.first().map(|u| Json::from(u.as_str()))),
                    ),
                    ("kind", Json::from(remote.kind().label())),
                ])
            })),
        ),
        (
            "remote",
            Json::opt(state.primary_remote_url().map(Json::from)),
        ),
        ("isGitHub", Json::from(state.is_github())),
        ("error", Json::opt(state.error.clone().map(Json::from))),
        (
            "operationInProgress",
            Json::opt(state.in_progress.map(|op| Json::from(op.label()))),
        ),
    ])
}

/// Machine-readable view of one change.
pub fn change_view_json(change: &OwnedChange) -> Json {
    let state = owned_change_state(change);
    let entry = &change.entry;
    Json::object([
        ("path", Json::from(change.logical_path.clone())),
        ("relativePath", Json::from(change.repo_relative.clone())),
        ("repository", Json::from(change.repository_id.clone())),
        ("repositoryPath", Json::from(change.repository_path.clone())),
        ("repositoryRole", Json::from(change.role.label())),
        (
            "type",
            Json::object([
                ("key", Json::from(state.key())),
                ("label", Json::from(state.label())),
                ("blocksCommit", Json::from(state.blocks_commit())),
            ]),
        ),
        ("staged", Json::from(entry.staged)),
        ("unstaged", Json::from(entry.unstaged)),
        ("untracked", Json::from(entry.untracked)),
        ("conflict", Json::from(entry.is_conflict())),
        (
            "originalPath",
            Json::opt(entry.original_path.clone().map(Json::from)),
        ),
        (
            "gitCode",
            Json::opt(entry.unmerged.map(|code| Json::from(code.label()))),
        ),
    ])
}

/// The full status payload a GUI renders: project header, repositories, changes and
/// pending work — one round trip, one consistent snapshot of the project.
pub fn status_view_json(session: &ProjectSession, status: &ProjectStatus) -> Json {
    let analyzer = session.analyzer();
    let changes = analyzer.owned_changes(status);
    let pending = pending_work(session, status);
    Json::object([
        ("project", project_view_json(session, status)),
        (
            "repositories",
            Json::array(status.repositories.iter().map(repository_view_json)),
        ),
        ("changes", Json::array(changes.iter().map(change_view_json))),
        (
            "pending",
            Json::array(pending.iter().map(|work| {
                Json::object([
                    ("id", Json::from(work.id.clone())),
                    ("path", Json::from(work.path.clone())),
                    ("files", Json::from(work.files)),
                    ("conflicts", Json::from(work.conflicts)),
                    ("blocked", Json::from(work.blocked)),
                    ("state", Json::from(work.state.key())),
                    ("summary", Json::from(work.summary())),
                ])
            })),
        ),
        (
            "conflicts",
            Json::array(
                changes
                    .iter()
                    .filter(|change| change.is_conflict())
                    .map(change_view_json),
            ),
        ),
    ])
}

/// Machine-readable outcome of one logical operation, with the refreshed status and
/// the work that is still outstanding.
pub fn operation_view_json(report: &OperationReport, status: &ProjectStatus) -> Json {
    let project = Json::object([
        ("name", Json::from(status.name.clone())),
        ("state", Json::from(project_state_kind(status).key())),
    ]);
    Json::object([
        ("report", report.to_json()),
        (
            "outcome",
            Json::object([
                ("kind", Json::from(report_kind(report))),
                ("success", Json::from(report.is_success())),
                ("partial", Json::from(report.is_partial())),
                ("exitCode", Json::from(report.exit_code() as i64)),
            ]),
        ),
        (
            "counts",
            Json::object([
                ("succeeded", Json::from(report.success().count())),
                ("skipped", Json::from(report.skipped().count())),
                ("conflicted", Json::from(report.conflicts().count())),
                ("failed", Json::from(report.failures().count())),
            ]),
        ),
        (
            "summaryLines",
            Json::array(report.summary_lines().into_iter().map(Json::from)),
        ),
        (
            "detailLines",
            Json::array(report.detail_lines().into_iter().map(Json::from)),
        ),
        ("project", project),
        (
            "status",
            Json::array(status.repositories.iter().map(repository_view_json)),
        ),
    ])
}

// ------------------------------------------------------------ setup views --

/// Machine-readable view of a hosted remote GitMesh would configure.
///
/// Only a *description*: no repository is created and no credential is involved.
pub fn github_remote_view_json(plan: &crate::providers::github::GitHubRemotePlan) -> Json {
    Json::object([
        ("provider", Json::from("github")),
        ("owner", Json::from(plan.owner.clone())),
        ("name", Json::from(plan.name.clone())),
        ("fullName", Json::from(plan.full_name())),
        ("url", Json::from(plan.url.clone())),
        ("sshUrl", Json::from(plan.ssh_url.clone())),
        ("httpsUrl", Json::from(plan.https_url.clone())),
        ("webUrl", Json::from(plan.web_url.clone())),
        ("visibility", Json::from(plan.visibility.label())),
        ("command", Json::from(plan.create_command())),
        ("note", Json::from(plan.note())),
        ("createsRepository", Json::from(false)),
    ])
}

/// Machine-readable view of a directory the user is about to turn into a project.
///
/// Everything here is a fact found on disk; the wizard changes nothing until the user
/// confirms a plan.
pub fn inspection_view_json(inspection: &setup::Inspection, name_hint: Option<&str>) -> Json {
    // A scratch project, only used to offer the wizard default ids that are unique inside
    // the selection. It is never written anywhere and never validated as configuration.
    let mut scratch = crate::discovery::initial_project(
        &inspection.root,
        name_hint.map(|name| name.to_string()),
        None,
        None,
    )
    .unwrap_or_else(|_| {
        crate::discovery::initial_project(&inspection.root, None, None, None)
            .expect("a project with no remote is always valid")
    });
    let mut next_id = |node: &setup::CandidateDirectory| -> String {
        let id = setup::suggested_repository_id(&scratch, &node.relative_path);
        scratch.repositories.push(crate::model::PhysicalRepository {
            id: id.clone(),
            role: crate::model::RepositoryRole::External,
            relative_path: node.relative_path.clone(),
            remote_url: None,
            branch: None,
            absolute_path: inspection.root.join(&node.relative_path),
        });
        id
    };
    Json::object([
        ("kind", Json::from("inspection")),
        ("root", Json::from(to_slash(&inspection.root))),
        ("exists", Json::from(inspection.exists)),
        (
            "suggestedName",
            Json::from(inspection.suggested_name.clone()),
        ),
        (
            "isGitMeshProject",
            Json::from(inspection.is_gitmesh_project),
        ),
        (
            "manifest",
            Json::object([
                ("path", Json::from(to_slash(&inspection.manifest_path))),
                ("exists", Json::from(inspection.manifest_path.is_file())),
                (
                    "error",
                    Json::opt(inspection.manifest_error.clone().map(Json::from)),
                ),
                (
                    "text",
                    Json::opt(inspection.manifest_text.clone().map(Json::from)),
                ),
            ]),
        ),
        (
            "rootIsRepository",
            Json::from(inspection.root_is_repository),
        ),
        (
            "enclosingRepository",
            Json::opt(
                inspection
                    .enclosing_repository
                    .as_ref()
                    .map(|path| Json::from(to_slash(path))),
            ),
        ),
        (
            "repositories",
            Json::array(inspection.repositories.iter().map(|repo| {
                Json::object([
                    ("id", Json::from(to_slash(&repo.relative_path))),
                    ("path", Json::from(to_slash(&repo.relative_path))),
                    ("isRoot", Json::from(repo.is_project_root)),
                    ("hasCommits", Json::from(repo.has_commits)),
                    ("branch", Json::opt(repo.branch.clone().map(Json::from))),
                    ("remote", Json::opt(repo.remote_url.clone().map(Json::from))),
                    ("trackedFiles", Json::from(repo.tracked_files)),
                    (
                        "nestedInside",
                        Json::opt(
                            repo.nested_inside
                                .as_ref()
                                .map(|path| Json::from(to_slash(path))),
                        ),
                    ),
                ])
            })),
        ),
        (
            "candidates",
            Json::array(
                inspection
                    .candidates
                    .iter()
                    .map(|node| candidate_view_json(node, &mut next_id)),
            ),
        ),
        (
            "notices",
            Json::array(inspection.notices.iter().map(|n| Json::from(n.as_str()))),
        ),
        ("truncated", Json::from(inspection.truncated)),
    ])
}

fn candidate_view_json(
    node: &setup::CandidateDirectory,
    next_id: &mut impl FnMut(&setup::CandidateDirectory) -> String,
) -> Json {
    Json::object([
        ("name", Json::from(node.name.clone())),
        ("path", Json::from(to_slash(&node.relative_path))),
        ("depth", Json::from(node.depth)),
        ("files", Json::from(node.files)),
        ("subtreeFiles", Json::from(node.subtree_files)),
        ("isRepository", Json::from(node.is_repository)),
        ("hasGitDir", Json::from(node.has_git_dir)),
        ("hasCommits", Json::from(node.has_commits)),
        ("branch", Json::opt(node.branch.clone().map(Json::from))),
        ("remote", Json::opt(node.remote.clone().map(Json::from))),
        ("trackedByRoot", Json::from(node.tracked_by_root)),
        (
            "nestedRepositories",
            Json::array(
                node.nested_repositories
                    .iter()
                    .map(|path| Json::from(to_slash(path))),
            ),
        ),
        ("suggested", Json::from(node.suggested())),
        ("suggestedId", Json::from(next_id(node))),
        (
            "children",
            Json::array(
                node.children
                    .iter()
                    .map(|child| candidate_view_json(child, next_id)),
            ),
        ),
    ])
}

/// Machine-readable view of a setup plan: what the review screen renders, and what the
/// interface sends back when the user confirms (the `id` must match).
pub fn setup_plan_view_json(plan: &setup::SetupPlan) -> Json {
    Json::object([
        ("kind", Json::from("plan")),
        ("id", Json::from(plan.id.clone())),
        ("ready", Json::from(plan.is_ready())),
        ("noop", Json::from(plan.is_noop())),
        ("summary", Json::from(plan.summary())),
        (
            "project",
            Json::object([
                ("name", Json::from(plan.name.clone())),
                ("root", Json::from(to_slash(&plan.root))),
                ("manifest", Json::from(to_slash(&plan.manifest_path))),
            ]),
        ),
        ("manifest", Json::from(plan.manifest.clone())),
        (
            "repositories",
            Json::array(plan.repositories.iter().map(|repo| {
                Json::object([
                    ("id", Json::from(repo.id.clone())),
                    ("path", Json::from(repo.path.clone())),
                    ("role", Json::from(repo.role.label())),
                    ("remote", Json::opt(repo.remote.clone().map(Json::from))),
                    ("provider", Json::opt(repo.provider.clone().map(Json::from))),
                    (
                        // Everything the review step needs about a hosted remote, built by
                        // the provider layer: the URLs, the visibility and the exact command
                        // that creates the repository *on the provider*, which GitMesh does
                        // not do and does not need a token for.
                        "hosted",
                        match &repo.hosted {
                            Some(hosted) => {
                                let visibility = repo
                                    .visibility
                                    .as_deref()
                                    .map(crate::providers::github::Visibility::parse)
                                    .unwrap_or(crate::providers::github::Visibility::Private);
                                match crate::providers::github::plan_remote(
                                    &hosted.owner,
                                    &hosted.name,
                                    crate::providers::github::RemoteScheme::Ssh,
                                    visibility,
                                ) {
                                    Ok(hosted_plan) => github_remote_view_json(&hosted_plan),
                                    Err(_) => Json::Null,
                                }
                            }
                            None => Json::Null,
                        },
                    ),
                    (
                        "visibility",
                        Json::opt(repo.visibility.clone().map(Json::from)),
                    ),
                    ("exists", Json::from(repo.exists)),
                    ("isRepository", Json::from(repo.is_repository)),
                    ("hasCommits", Json::from(repo.has_commits)),
                    ("branch", Json::opt(repo.branch.clone().map(Json::from))),
                    (
                        "currentOrigin",
                        Json::opt(repo.current_origin.clone().map(Json::from)),
                    ),
                    ("create", Json::from(repo.create)),
                    ("remoteAction", Json::from(repo.remote_action.label())),
                    ("trackedByRoot", Json::from(repo.tracked_by_root)),
                    ("untrack", Json::from(repo.untrack)),
                    ("changes", Json::from(repo.will_change())),
                    ("sentence", Json::from(repo.sentence())),
                ])
            })),
        ),
        (
            // The follow-up the caller runs after a successful apply. It is part of the
            // preview because it is part of the plan: the executor does not invent it.
            "publish",
            match plan.first_publish() {
                Some(publish) => Json::object([
                    ("message", Json::from(publish.message.clone())),
                    (
                        "repositories",
                        Json::array(
                            publish
                                .repositories
                                .iter()
                                .map(|id| Json::from(id.as_str())),
                        ),
                    ),
                    ("sentence", Json::from(publish.sentence())),
                    (
                        "safety",
                        Json::from(
                            "the first commit is made once the manifest exists, with the \
                             message shown here, and only in the repositories listed above",
                        ),
                    ),
                ]),
                None => Json::Null,
            },
        ),
        (
            "steps",
            Json::array(plan.steps.iter().map(|step| {
                Json::object([
                    ("kind", Json::from(step.kind.label())),
                    ("heading", Json::from(step.kind.heading())),
                    ("target", Json::from(step.target.clone())),
                    ("path", Json::from(step.path.clone())),
                    ("detail", Json::from(step.detail.clone())),
                    ("state", Json::from(step.state.label())),
                    ("reason", Json::opt(step.state.reason().map(Json::from))),
                ])
            })),
        ),
        (
            "counts",
            Json::object([
                ("planned", Json::from(plan.planned_steps().count())),
                ("already", Json::from(plan.already_satisfied().count())),
                ("blocked", Json::from(plan.blocked_steps().count())),
                (
                    "createRepositories",
                    Json::from(plan.created_repositories().count()),
                ),
            ]),
        ),
        (
            "safety",
            Json::array(plan.safety.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "blockers",
            Json::array(plan.blockers.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "warnings",
            Json::array(plan.warnings.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "notices",
            Json::array(plan.notices.iter().map(|line| Json::from(line.as_str()))),
        ),
    ])
}

/// Machine-readable view of a setup result, including what still has to be fixed.
pub fn setup_result_view_json(result: &setup::SetupResult) -> Json {
    let validation = match &result.validation {
        Some(report) => validation_view_json(report),
        None => Json::Null,
    };
    Json::object([
        ("kind", Json::from("setup")),
        ("planId", Json::from(result.plan_id.clone())),
        ("status", Json::from(result.kind.label())),
        ("sentence", Json::from(result.kind.sentence())),
        ("summary", Json::from(result.summary())),
        ("dryRun", Json::from(result.dry_run)),
        ("exitCode", Json::from(result.exit_code() as i64)),
        (
            "counts",
            Json::object([
                ("succeeded", Json::from(result.succeeded())),
                ("skipped", Json::from(result.skipped())),
                ("failed", Json::from(result.failures().count())),
            ]),
        ),
        (
            "steps",
            Json::array(result.outcomes.iter().map(|outcome| {
                Json::object([
                    ("kind", Json::from(outcome.kind.label())),
                    ("heading", Json::from(outcome.kind.heading())),
                    ("target", Json::from(outcome.target.clone())),
                    ("path", Json::from(outcome.path.clone())),
                    ("outcome", Json::from(outcome.outcome.label())),
                    ("symbol", Json::from(outcome.symbol())),
                    ("summary", Json::from(outcome.summary.clone())),
                    (
                        "details",
                        Json::array(outcome.details.iter().map(|d| Json::from(d.as_str()))),
                    ),
                ])
            })),
        ),
        (
            "outcomes",
            Json::array(result.outcomes.iter().map(|outcome| {
                Json::object([
                    ("id", Json::from(outcome.target.clone())),
                    ("path", Json::from(outcome.path.clone())),
                    ("outcome", Json::from(outcome.outcome.label())),
                    ("symbol", Json::from(outcome.symbol())),
                    ("summary", Json::from(outcome.summary.clone())),
                ])
            })),
        ),
        (
            "manifest",
            Json::opt(
                result
                    .manifest_path
                    .as_ref()
                    .map(|path| Json::from(to_slash(path))),
            ),
        ),
        (
            "project",
            match &result.project {
                Some(project) => Json::object([
                    ("name", Json::from(project.name.clone())),
                    ("root", Json::from(to_slash(&project.root))),
                    ("repositories", Json::from(project.len() as i64)),
                ]),
                None => Json::Null,
            },
        ),
        ("validation", validation),
        (
            "refused",
            Json::array(result.refused.iter().map(|line| Json::from(line.as_str()))),
        ),
    ])
}

// ------------------------------------------------------ repository management --

/// Machine-readable view of the repositories of an opened project: the configuration, the
/// state found on disk, and everything the interface has to point out.
///
/// This is the read-only half of the management feature: the *changes* are described by
/// [`management_plan_view_json`] and [`management_result_view_json`], never guessed by a
/// front end.
pub fn management_inspection_view_json(inspection: &manage::RepositoryInspection) -> Json {
    let usable = inspection
        .repositories
        .iter()
        .filter(|repo| repo.is_usable())
        .count();
    let attention = inspection
        .repositories
        .iter()
        .filter(|repo| !repo.issues.is_empty())
        .count();
    Json::object([
        ("kind", Json::from("repositories")),
        (
            "project",
            Json::object([
                ("name", Json::from(inspection.project_name.clone())),
                ("root", Json::from(to_slash(&inspection.root))),
                ("manifest", Json::from(to_slash(&inspection.manifest_path))),
            ]),
        ),
        (
            "repositories",
            Json::array(
                inspection
                    .repositories
                    .iter()
                    .map(managed_repository_view_json),
            ),
        ),
        (
            "candidates",
            Json::array(management_candidates_json(inspection)),
        ),
        (
            "counts",
            Json::object([
                ("repositories", Json::from(inspection.repositories.len())),
                ("usable", Json::from(usable)),
                ("needingAttention", Json::from(attention)),
            ]),
        ),
        (
            "notices",
            Json::array(inspection.notices.iter().map(|n| Json::from(n.as_str()))),
        ),
        (
            "warnings",
            Json::array(inspection.warnings.iter().map(|w| Json::from(w.as_str()))),
        ),
    ])
}

/// Machine-readable view of one configured repository.
pub fn managed_repository_view_json(repo: &manage::ManagedRepository) -> Json {
    Json::object([
        ("id", Json::from(repo.id.clone())),
        ("role", Json::from(repo.role.label())),
        ("path", Json::from(repo.path.clone())),
        (
            "remote",
            Json::opt(repo.manifest_remote.clone().map(Json::from)),
        ),
        ("origin", Json::opt(repo.origin.clone().map(Json::from))),
        ("remoteLabel", Json::from(repo.remote_label())),
        ("provider", Json::opt(repo.provider.clone().map(Json::from))),
        (
            "branchHint",
            Json::opt(repo.branch_hint.clone().map(Json::from)),
        ),
        (
            "state",
            Json::object([
                ("key", Json::from(repo.state.label())),
                ("label", Json::from(repo.state.sentence())),
            ]),
        ),
        ("exists", Json::from(repo.exists)),
        ("isRepository", Json::from(repo.is_repository)),
        ("hasCommits", Json::from(repo.has_commits)),
        ("branch", Json::opt(repo.branch.clone().map(Json::from))),
        ("head", Json::opt(repo.head.clone().map(Json::from))),
        ("clean", Json::from(repo.clean)),
        ("changes", Json::from(repo.changes)),
        ("ahead", Json::from(repo.ahead.map(i64::from).unwrap_or(0))),
        (
            "behind",
            Json::from(repo.behind.map(i64::from).unwrap_or(0)),
        ),
        ("trackedByRoot", Json::from(repo.tracked_by_root)),
        ("usable", Json::from(repo.is_usable())),
        (
            "issues",
            Json::array(repo.issues.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "warnings",
            Json::array(repo.warnings.iter().map(|line| Json::from(line.as_str()))),
        ),
    ])
}

/// Candidate directories of a project inspection, with the id each one would get.
fn management_candidates_json(inspection: &manage::RepositoryInspection) -> Vec<Json> {
    let mut scratch = GitMeshProject {
        name: inspection.project_name.clone(),
        root: inspection.root.clone(),
        repositories: Vec::new(),
    };
    let mut next_id = |node: &setup::CandidateDirectory| -> String {
        let id = setup::suggested_repository_id(&scratch, &node.relative_path);
        scratch.repositories.push(crate::model::PhysicalRepository {
            id: id.clone(),
            role: crate::model::RepositoryRole::External,
            relative_path: node.relative_path.clone(),
            remote_url: None,
            branch: None,
            absolute_path: inspection.root.join(&node.relative_path),
        });
        id
    };
    inspection
        .candidates
        .iter()
        .map(|node| candidate_view_json(node, &mut next_id))
        .collect()
}

/// Machine-readable view of what would happen to one directory that could become a
/// repository: the answers the interface asks for before offering the button.
pub fn management_candidate_view_json(candidate: &manage::CandidateInspection) -> Json {
    Json::object([
        ("path", Json::from(candidate.path.clone())),
        ("absolutePath", Json::from(to_slash(&candidate.absolute))),
        ("exists", Json::from(candidate.exists)),
        ("isRepository", Json::from(candidate.is_repository)),
        ("hasCommits", Json::from(candidate.has_commits)),
        ("emptyDirectory", Json::from(candidate.empty_directory)),
        (
            "branch",
            Json::opt(candidate.branch.clone().map(Json::from)),
        ),
        (
            "origin",
            Json::opt(candidate.origin.clone().map(Json::from)),
        ),
        ("trackedByRoot", Json::from(candidate.tracked_by_root)),
        (
            "nestedRepositories",
            Json::array(
                candidate
                    .nested_repositories
                    .iter()
                    .map(|path| Json::from(path.clone())),
            ),
        ),
        ("suggestedId", Json::from(candidate.suggested_id.clone())),
        (
            "managedAs",
            Json::opt(candidate.managed_as.clone().map(Json::from)),
        ),
        ("canAdd", Json::from(candidate.can_add())),
        ("consequence", Json::from(candidate.consequence())),
        (
            "blockers",
            Json::array(candidate.blockers.iter().map(|b| Json::from(b.as_str()))),
        ),
        (
            "warnings",
            Json::array(candidate.warnings.iter().map(|w| Json::from(w.as_str()))),
        ),
    ])
}

/// Machine-readable view of a management plan: what the review panel shows, and what the
/// interface sends back when the user confirms (the `id` must match).
///
/// The plan is the single source of truth for both the preview and the execution: the
/// manifest preview here *is* the manifest the execution writes.
pub fn management_plan_view_json(plan: &manage::RepositoryPlan) -> Json {
    Json::object([
        ("kind", Json::from("plan")),
        ("flow", Json::from("repository")),
        ("id", Json::from(plan.id.clone())),
        ("ready", Json::from(plan.is_ready())),
        ("noop", Json::from(plan.is_noop())),
        ("summary", Json::from(plan.summary())),
        (
            "project",
            Json::object([
                ("name", Json::from(plan.name.clone())),
                ("root", Json::from(to_slash(&plan.root))),
                ("manifest", Json::from(to_slash(&plan.manifest_path))),
            ]),
        ),
        (
            "manifest",
            Json::object([
                (
                    "before",
                    Json::opt(plan.manifest_before.clone().map(Json::from)),
                ),
                ("after", Json::from(plan.manifest_after.clone())),
                ("changes", Json::from(plan.manifest_changes)),
            ]),
        ),
        (
            "changes",
            Json::array(plan.changes.iter().map(|change| {
                Json::object([
                    ("kind", Json::from(change.kind.label())),
                    ("heading", Json::from(change.kind.heading())),
                    (
                        "configuration",
                        Json::from(change.kind.changes_configuration()),
                    ),
                    ("id", Json::from(change.id.clone())),
                    ("path", Json::from(change.path.clone())),
                    ("before", Json::opt(change.before.clone().map(Json::from))),
                    ("after", Json::opt(change.after.clone().map(Json::from))),
                    ("detail", Json::from(change.detail.clone())),
                    ("state", Json::from(change.state.label())),
                    ("symbol", Json::from(plan_state_symbol(&change.state))),
                    ("reason", Json::opt(change.state.reason().map(Json::from))),
                ])
            })),
        ),
        (
            "actions",
            Json::array(plan.actions.iter().map(|action| {
                Json::object([
                    ("kind", Json::from(action.kind.label())),
                    ("heading", Json::from(action.kind.heading())),
                    ("role", Json::from(action.kind.role())),
                    ("target", Json::from(action.target.clone())),
                    ("path", Json::from(action.path.clone())),
                    ("detail", Json::from(action.detail.clone())),
                    ("state", Json::from(action.state.label())),
                    ("symbol", Json::from(plan_state_symbol(&action.state))),
                    ("reason", Json::opt(action.state.reason().map(Json::from))),
                ])
            })),
        ),
        (
            // What a removal does *not* touch, recorded while the directory is still
            // there, so the result can prove nothing of it was deleted.
            "removals",
            Json::array(plan.removals.iter().map(|removal| {
                Json::object([
                    ("id", Json::from(removal.id.clone())),
                    ("path", Json::from(removal.path.clone())),
                    ("isRepository", Json::from(removal.is_repository)),
                    ("head", Json::opt(removal.head.clone().map(Json::from))),
                    ("origin", Json::opt(removal.origin.clone().map(Json::from))),
                    ("trackedByRoot", Json::from(removal.tracked_by_root)),
                ])
            })),
        ),
        (
            "counts",
            Json::object([
                ("changes", Json::from(plan.changes.len())),
                ("plannedChanges", Json::from(plan.planned_changes().count())),
                (
                    "satisfiedChanges",
                    Json::from(plan.satisfied_changes().count()),
                ),
                ("blockedChanges", Json::from(plan.blocked_changes().count())),
                ("actions", Json::from(plan.actions.len())),
                ("plannedActions", Json::from(plan.planned_actions().count())),
                (
                    "satisfiedActions",
                    Json::from(plan.satisfied_actions().count()),
                ),
                ("blockedActions", Json::from(plan.blocked_actions().count())),
            ]),
        ),
        (
            "safety",
            Json::array(plan.safety.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "blockers",
            Json::array(plan.blockers.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "recovery",
            Json::array(plan.recovery.iter().map(|action| {
                let (path, id, remote) = match action {
                    manage::PlanRecovery::CloneRemote { path, id, remote } => (path, id, remote),
                };
                Json::object([
                    ("kind", Json::from(action.label())),
                    ("sentence", Json::from(action.sentence())),
                    ("command", Json::from(action.command(&plan.root))),
                    ("path", Json::from(path.as_str())),
                    ("id", Json::from(id.as_str())),
                    ("remote", Json::from(remote.as_str())),
                ])
            })),
        ),
        (
            "warnings",
            Json::array(plan.warnings.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "notices",
            Json::array(plan.notices.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "expectedOrigins",
            Json::array(
                plan.expected_origins
                    .iter()
                    .map(|line| Json::from(line.as_str())),
            ),
        ),
    ])
}

/// Symbol of a plan row: what will run, what is already there, what is blocked.
fn plan_state_symbol(state: &StepState) -> &'static str {
    match state {
        StepState::Planned => "…",
        StepState::AlreadySatisfied(_) => "–",
        StepState::Blocked(_) => "✗",
    }
}

/// Machine-readable view of a management result: what each unit of work did, and the
/// evidence that each change really happened.
pub fn management_result_view_json(result: &manage::RepositoryManagementResult) -> Json {
    let validation = match &result.validation {
        Some(report) => validation_view_json(report),
        None => Json::Null,
    };
    Json::object([
        ("kind", Json::from("result")),
        ("flow", Json::from("repository")),
        ("planId", Json::from(result.plan_id.clone())),
        ("dryRun", Json::from(result.dry_run)),
        ("status", Json::from(result.kind.label())),
        ("sentence", Json::from(result.sentence())),
        ("summary", Json::from(result.kind.sentence())),
        ("exitCode", Json::from(result.exit_code() as i64)),
        ("success", Json::from(result.is_success())),
        (
            "counts",
            Json::object([
                ("succeeded", Json::from(result.succeeded())),
                ("skipped", Json::from(result.skipped())),
                ("failed", Json::from(result.failures().count())),
                ("applied", Json::from(result.applied().count())),
                ("notApplied", Json::from(result.not_applied().count())),
            ]),
        ),
        (
            "changes",
            Json::array(result.changes.iter().map(|outcome| {
                let change = &outcome.change;
                Json::object([
                    ("kind", Json::from(change.kind.label())),
                    ("heading", Json::from(change.kind.heading())),
                    ("id", Json::from(change.id.clone())),
                    ("path", Json::from(change.path.clone())),
                    ("before", Json::opt(change.before.clone().map(Json::from))),
                    ("after", Json::opt(change.after.clone().map(Json::from))),
                    ("detail", Json::from(change.detail.clone())),
                    ("outcome", Json::from(outcome.outcome.label())),
                    ("symbol", Json::from(outcome.outcome.symbol())),
                    (
                        "evidence",
                        Json::array(outcome.evidence.iter().map(|e| Json::from(e.as_str()))),
                    ),
                ])
            })),
        ),
        (
            "actions",
            Json::array(result.actions.iter().map(|outcome| {
                Json::object([
                    ("kind", Json::from(outcome.kind.label())),
                    ("heading", Json::from(outcome.kind.heading())),
                    ("row", Json::from(outcome.row_id())),
                    ("target", Json::from(outcome.target.clone())),
                    ("path", Json::from(outcome.path.clone())),
                    ("outcome", Json::from(outcome.outcome.label())),
                    ("symbol", Json::from(outcome.symbol())),
                    ("summary", Json::from(outcome.summary.clone())),
                    (
                        "details",
                        Json::array(outcome.details.iter().map(|d| Json::from(d.as_str()))),
                    ),
                ])
            })),
        ),
        (
            "manifest",
            Json::opt(
                result
                    .manifest_path
                    .as_ref()
                    .map(|path| Json::from(to_slash(path))),
            ),
        ),
        (
            "project",
            match &result.project {
                Some(project) => Json::object([
                    ("name", Json::from(project.name.clone())),
                    ("root", Json::from(to_slash(&project.root))),
                    (
                        "repositories",
                        Json::array(project.sorted_repositories().iter().map(|repo| {
                            Json::object([
                                ("id", Json::from(repo.id.clone())),
                                ("path", Json::from(repo.relative_slash())),
                                ("role", Json::from(repo.role.label())),
                            ])
                        })),
                    ),
                ]),
                None => Json::Null,
            },
        ),
        ("validation", validation),
        (
            "refused",
            Json::array(result.refused.iter().map(|line| Json::from(line.as_str()))),
        ),
        (
            "warnings",
            Json::array(result.warnings.iter().map(|line| Json::from(line.as_str()))),
        ),
    ])
}

/// Machine-readable view of the validation of a project.
pub fn validation_view_json(report: &setup::ValidationReport) -> Json {
    Json::object([
        ("ok", Json::from(report.ok)),
        ("root", Json::from(to_slash(&report.root))),
        ("manifest", Json::from(to_slash(&report.manifest_path))),
        (
            "project",
            Json::opt(report.project_name.clone().map(Json::from)),
        ),
        (
            "repositories",
            Json::array(report.repositories.iter().map(|check| {
                Json::object([
                    ("id", Json::from(check.id.clone())),
                    ("path", Json::from(check.path.clone())),
                    ("role", Json::from(check.role.label())),
                    ("exists", Json::from(check.exists)),
                    ("isRepository", Json::from(check.is_repository)),
                    (
                        "manifestRemote",
                        Json::opt(check.manifest_remote.clone().map(Json::from)),
                    ),
                    ("origin", Json::opt(check.origin.clone().map(Json::from))),
                    ("remoteOk", Json::from(check.remote_ok)),
                    ("branch", Json::opt(check.branch.clone().map(Json::from))),
                    ("ok", Json::from(check.is_ok())),
                    (
                        "issues",
                        Json::array(check.issues.iter().map(|i| Json::from(i.as_str()))),
                    ),
                ])
            })),
        ),
        (
            "issues",
            Json::array(report.issues.iter().map(|i| Json::from(i.as_str()))),
        ),
    ])
}

/// Coarse classification of a report, for front ends that only need one word.
pub fn report_kind(report: &OperationReport) -> &'static str {
    if report.is_success() {
        if report.success().count() == 0 {
            "nothing_to_do"
        } else {
            "success"
        }
    } else if report.is_partial() {
        "partial"
    } else {
        "failed"
    }
}

/// Symbols used in text summaries (identical to the CLI).
pub fn outcome_symbol(kind: OutcomeKind) -> &'static str {
    kind.symbol()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use crate::testkit::RepoFixture;

    fn open_session(fixture: &RepoFixture) -> ProjectSession {
        ProjectSession::open(fixture.path()).expect("project opens")
    }

    #[test]
    fn opening_a_missing_project_reports_a_clear_error() {
        let fixture = RepoFixture::new();
        // The fixture root is a Git repository but not a GitMesh project.
        std::fs::remove_dir_all(fixture.path().join(".gitmesh")).ok();
        let err = ProjectSession::open(fixture.path()).unwrap_err();
        assert!(
            matches!(err, Error::ProjectNotFound { .. }),
            "expected a not-found error, got: {err}"
        );
        assert!(
            err.to_string().contains("no GitMesh project found"),
            "{err}"
        );
    }

    #[test]
    fn opening_a_malformed_manifest_reports_a_configuration_error() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // Two repositories configured on the same path: a configuration error the
        // session must surface instead of opening a half-broken project.
        std::fs::write(
            fixture.path().join(".gitmesh/project.toml"),
            "version = 1\nname = \"demo\"\n\n[repositories]\n",
        )
        .unwrap();
        let err = ProjectSession::open(fixture.path()).unwrap_err();
        assert!(err.is_configuration_error(), "{err}");
        assert!(
            err.to_string().to_lowercase().contains("configuration")
                || err.to_string().to_lowercase().contains("invalid")
                || err.to_string().to_lowercase().contains("manifest"),
            "{err}"
        );
    }

    #[test]
    fn session_exposes_project_identity() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let session = open_session(&fixture);
        assert_eq!(session.name(), "demo");
        assert_eq!(session.root(), fixture.path());
        assert!(session.manifest_path().ends_with(".gitmesh/project.toml"));
    }

    #[test]
    fn opening_from_a_nested_directory_finds_the_same_project() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let nested = ProjectSession::open(&fixture.path().join("engine")).expect("nested open");
        assert_eq!(nested.root(), fixture.path());
        assert_eq!(nested.project().repositories.len(), 2);
    }

    #[test]
    fn project_view_reports_one_logical_project() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        let session = open_session(&fixture);
        let status = session.status();
        let json = project_view_json(&session, &status).to_string();
        assert!(json.contains("\"name\":\"demo\""));
        assert!(json.contains("\"state\":{\"key\":\"clean\""));
        assert!(json.contains("\"branch\":{\"name\":\"main\""));
        assert!(json.contains("\"repositories\":3"));
        assert!(json.contains("\"rootRepository\":{\"id\":\"root\""));
    }

    #[test]
    fn change_state_covers_every_git_state() {
        use crate::git::status::ChangeKind;
        let entry = |index, worktree, staged, unstaged, untracked| StatusEntry {
            path: "x".into(),
            original_path: None,
            index,
            worktree,
            unmerged: None,
            staged,
            unstaged,
            untracked,
            ignored: false,
        };
        assert_eq!(
            change_state(&entry(None, Some(ChangeKind::Modified), false, true, false)),
            ChangeState::Modified
        );
        assert_eq!(
            change_state(&entry(Some(ChangeKind::Added), None, true, false, false)),
            ChangeState::Staged
        );
        assert_eq!(
            change_state(&entry(
                Some(ChangeKind::Modified),
                Some(ChangeKind::Modified),
                true,
                true,
                false
            )),
            ChangeState::StagedAndModified
        );
        assert_eq!(
            change_state(&entry(None, Some(ChangeKind::Deleted), false, true, false)),
            ChangeState::Deleted
        );
        assert_eq!(
            change_state(&entry(Some(ChangeKind::Renamed), None, true, false, false)),
            ChangeState::Renamed
        );
        assert_eq!(
            change_state(&entry(None, None, false, false, true)),
            ChangeState::Untracked
        );
        // The CLI-facing label is unchanged for every state it could already print,
        // including a *staged* rename (which the finer-grained state calls "renamed").
        assert_eq!(
            status_column_label(&entry(None, Some(ChangeKind::Deleted), false, true, false)),
            "modified"
        );
        assert_eq!(
            status_column_label(&entry(Some(ChangeKind::Renamed), None, true, false, false)),
            "staged"
        );
        assert_eq!(
            status_column_label(&entry(
                Some(ChangeKind::Modified),
                Some(ChangeKind::Modified),
                true,
                true,
                false
            )),
            "staged+modified"
        );
        assert_eq!(
            status_column_label(&entry(None, None, false, false, true)),
            "untracked"
        );
    }

    #[test]
    fn project_state_follows_the_worst_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let session = open_session(&fixture);
        assert_eq!(
            project_state_kind(&session.status()),
            ProjectStateKind::Clean
        );

        fixture.write("engine/a.txt", "change");
        let session = open_session(&fixture);
        assert_eq!(
            project_state_kind(&session.status()),
            ProjectStateKind::Changed
        );

        // A configured repository that is not on disk makes the project unavailable.
        std::fs::remove_dir_all(fixture.path().join("engine")).unwrap();
        let session = open_session(&fixture);
        let status = session.status();
        assert_eq!(project_state_kind(&status), ProjectStateKind::Unavailable);
        assert!(project_state_kind(&status).needs_attention());
    }

    #[test]
    fn changes_carry_the_owning_repository_and_logical_path() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "root change");
        fixture.write("engine/lib.rs", "engine change");
        let session = open_session(&fixture);
        let changes = session.changes();
        assert_eq!(changes.len(), 2);
        let engine = changes
            .iter()
            .find(|c| c.repository_id == "engine")
            .expect("engine change present");
        // engine/lib.rs is a file of the project, owned by the engine repository.
        assert_eq!(engine.logical_path, "engine/lib.rs");
        assert_eq!(engine.repo_relative, "lib.rs");
        assert_eq!(engine.repository_path, "engine");
        assert_eq!(owned_change_state(engine), ChangeState::Untracked);
        let root = changes
            .iter()
            .find(|c| c.repository_id == "root")
            .expect("root change present");
        assert_eq!(root.logical_path, "src/main.rs");
        assert_eq!(root.repository_path, ".");
        let json = change_view_json(engine).to_string();
        assert!(json.contains("\"repository\":\"engine\""));
        assert!(json.contains("\"path\":\"engine/lib.rs\""));
    }

    #[test]
    fn deleted_and_renamed_changes_are_classified() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write_and_commit("engine/keep.txt", "keep\n");
        fixture.write_and_commit("engine/move.txt", "move\n");
        std::fs::remove_file(fixture.path().join("engine/keep.txt")).unwrap();
        std::fs::rename(
            fixture.path().join("engine/move.txt"),
            fixture.path().join("engine/moved.txt"),
        )
        .unwrap();
        fixture.add_all("engine");
        let session = open_session(&fixture);
        let changes = session.changes();
        let deleted = changes
            .iter()
            .find(|c| c.repo_relative == "keep.txt")
            .expect("deleted entry");
        assert_eq!(owned_change_state(deleted), ChangeState::Deleted);
        let renamed = changes
            .iter()
            .find(|c| c.repo_relative == "moved.txt")
            .expect("renamed entry");
        assert_eq!(owned_change_state(renamed), ChangeState::Renamed);
        assert_eq!(renamed.entry.original_path.as_deref(), Some("move.txt"));
        // Renamed files stay visible in the GUI view with their original path.
        let json = change_view_json(renamed).to_string();
        assert!(json.contains("\"key\":\"renamed\""));
        assert!(json.contains("\"originalPath\":\"move.txt\""));
    }

    #[test]
    fn pending_work_blocks_conflicted_repositories() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "lib.rs");
        fixture.write("engine/unrelated.txt", "still changeable");
        fixture.write("src/root.txt", "root change");

        let session = open_session(&fixture);
        let status = session.status();
        let pending = pending_work(&session, &status);
        let engine = pending.iter().find(|w| w.id == "engine").expect("engine");
        assert!(engine.blocked, "{engine:?}");
        assert_eq!(engine.conflicts, 1);
        assert_eq!(engine.state, RepositoryStateKind::Conflicted);
        let root = pending.iter().find(|w| w.id == "root").expect("root");
        assert!(!root.blocked);
        assert_eq!(root.state, RepositoryStateKind::Changed);
        // The conflict is reported as a conflict, not as a generic change.
        assert_eq!(session.conflicts().len(), 1);
        let json = status_view_json(&session, &status).to_string();
        assert!(json.contains("\"conflicts\":[{\"path\":\"engine/lib.rs\""));
    }

    #[test]
    fn unavailable_repositories_appear_in_pending_work() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        std::fs::remove_dir_all(fixture.path().join("engine")).unwrap();
        let session = open_session(&fixture);
        let status = session.status();
        let pending = pending_work(&session, &status);
        let engine = pending.iter().find(|w| w.id == "engine").expect("engine");
        assert!(engine.blocked);
        assert_eq!(engine.state, RepositoryStateKind::Unavailable);
    }

    #[test]
    fn status_view_round_trips_every_change() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let session = open_session(&fixture);
        let status = session.status();
        let json = status_view_json(&session, &status).to_string();
        assert!(json.contains("\"changes\":[{"));
        assert!(json.contains("\"path\":\"engine/lib.rs\""));
        assert!(json.contains("\"path\":\"src/main.rs\""));
        assert!(json.contains("\"blocked\":false"));
    }

    #[test]
    fn commit_through_the_session_produces_real_commits() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let session = open_session(&fixture);
        let report = session
            .commit(&CommitOptions::new("session commit"))
            .expect("commit runs");
        assert_eq!(report.counts(), (2, 0, 0, 0));
        assert!(report.is_success());
        // Real, independent commits in both repositories — no synthetic global commit.
        assert!(fixture
            .git_ok(".", &["log", "-1", "--pretty=%s"])
            .contains("session commit"));
        assert!(fixture
            .git_ok("engine", &["log", "-1", "--pretty=%s"])
            .contains("session commit"));
        assert!(!fixture
            .git_ok("engine", &["log", "--oneline"])
            .contains("session commit\n\n"));
        let root_head = fixture.git_ok(".", &["rev-parse", "HEAD"]);
        let engine_head = fixture.git_ok("engine", &["rev-parse", "HEAD"]);
        assert_ne!(
            root_head, engine_head,
            "each repository has its own history"
        );
    }

    #[test]
    fn commit_reports_are_not_reinterpreted() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/lib.rs", "y");
        let session = open_session(&fixture);
        let report = session
            .commit(&CommitOptions::new("only engine"))
            .expect("commit runs");
        // One real commit, one skipped repository: the core's classification, verbatim.
        assert_eq!(report.counts(), (1, 1, 0, 0));
        let status = session.status();
        let json = operation_view_json(&report, &status).to_string();
        assert!(json.contains("\"kind\":\"success\""));
        assert!(json.contains("\"skipped\":1"));
        assert!(json.contains("\"operation\":\"commit\""));
        assert!(json.contains("\"exitCode\":0"));
    }

    #[test]
    fn commit_dry_run_changes_nothing() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/lib.rs", "y");
        let session = open_session(&fixture);
        let mut options = CommitOptions::new("dry");
        options.dry_run = true;
        let report = session.commit(&options).expect("dry run");
        assert!(report.dry_run);
        assert!(!fixture
            .git_ok("engine", &["log", "--oneline"])
            .contains("dry"));
        // Still reported as outstanding work, so the GUI shows the pending state.
        let status = session.status();
        assert_eq!(pending_work(&session, &status).len(), 1);
    }

    #[test]
    fn branch_operation_through_the_session_is_uniform() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let session = open_session(&fixture);
        let report = session
            .branch(
                &BranchAction::Create {
                    name: "feature/x".into(),
                },
                &BranchOptions::default(),
            )
            .expect("branch runs");
        assert_eq!(report.counts(), (2, 0, 0, 0));

        let show = session
            .branch(&BranchAction::Show, &BranchOptions::default())
            .expect("show runs");
        assert_eq!(show.counts(), (2, 0, 0, 0));

        // A checkout across the project puts every repository on one branch.
        let report = session
            .branch(
                &BranchAction::Checkout {
                    name: "feature/x".into(),
                    create: false,
                },
                &BranchOptions::default(),
            )
            .expect("checkout runs");
        assert!(report.is_success());
        let status = session.status();
        assert_eq!(status.reference_branch().as_deref(), Some("feature/x"));
        assert!(status.inconsistent_branches().is_empty());
        assert_eq!(
            fixture
                .git_ok("engine", &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "feature/x"
        );
    }

    #[test]
    fn inconsistent_branches_are_reported_not_hidden() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // One repository moves to another branch behind GitMesh's back.
        fixture.git_ok("engine", &["checkout", "-q", "-b", "side"]);
        let session = open_session(&fixture);
        let status = session.status();
        assert_eq!(status.reference_branch().as_deref(), Some("main"));
        let outliers: Vec<&str> = status
            .inconsistent_branches()
            .iter()
            .map(|s| s.id.as_str())
            .collect();
        assert_eq!(outliers, vec!["engine"]);
        let json = project_view_json(&session, &status).to_string();
        assert!(json.contains("\"consistent\":false"));
        assert!(json.contains("\"outliers\":[\"engine\"]"));
    }

    #[test]
    fn sync_and_push_reports_are_aggregated_per_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.publish("engine", "engine.git");

        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let session = open_session(&fixture);
        session
            .commit(&CommitOptions::new("work"))
            .expect("commit runs");

        let push = session.push(&PushOptions::default()).expect("push runs");
        assert_eq!(push.counts(), (2, 0, 0, 0));

        let fetch = session.sync(&SyncOptions::new(), true).expect("fetch runs");
        assert_eq!(fetch.counts(), (2, 0, 0, 0));

        let pull = session.sync(&SyncOptions::new(), false).expect("pull runs");
        assert_eq!(pull.counts(), (2, 0, 0, 0));

        // Nothing left to push is skipped, not a failure.
        let push_again = session.push(&PushOptions::default()).expect("push runs");
        assert_eq!(push_again.counts(), (0, 2, 0, 0));
        assert_eq!(report_kind(&push_again), "nothing_to_do");
        assert!(push_again.is_success());
    }

    #[test]
    fn partial_failure_is_reported_as_partial() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.publish("engine", "engine.git");

        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let session = open_session(&fixture);
        session
            .commit(&CommitOptions::new("work"))
            .expect("commit runs");

        // Break one remote: the other repository must still be pushed.
        std::fs::remove_dir_all(fixture.bare_path("engine.git")).unwrap();
        let report = session.push(&PushOptions::default()).expect("push runs");
        assert!(report.is_partial());
        assert_eq!(report_kind(&report), "partial");
        assert!(report.success().any(|o| o.id == "root"));
        assert!(report.failures().any(|o| o.id == "engine"));
        assert_eq!(report.exit_code(), 1);
        let status = session.status();
        let json = operation_view_json(&report, &status).to_string();
        assert!(json.contains("\"partial\":true"));
        assert!(json.contains("\"failed\":1"));
    }

    #[test]
    fn pull_conflict_is_reported_and_survives_a_reopen() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.publish("engine", "engine.git");

        // Another developer pushes a conflicting change to the engine repository.
        let other = fixture.clone_outside(&fixture.bare_path("engine.git"), "other-engine");
        std::fs::write(other.join("lib.rs"), "theirs\n").unwrap();
        fixture
            .runner()
            .repo(&other)
            .run_checked(&["add", "-A"])
            .unwrap();
        fixture
            .runner()
            .repo(&other)
            .run_checked(&["commit", "-q", "-m", "theirs"])
            .unwrap();
        fixture
            .runner()
            .repo(&other)
            .run_checked(&["push", "origin", "HEAD"])
            .unwrap();

        // We commit a conflicting change locally and pull.
        std::fs::write(fixture.path().join("engine/lib.rs"), "ours\n").unwrap();
        let session = open_session(&fixture);
        session
            .commit(&CommitOptions::new("ours"))
            .expect("commit runs");
        let report = session
            .sync(
                &SyncOptions {
                    strategy: crate::ops::PullStrategy::Merge,
                    ..SyncOptions::new()
                },
                false,
            )
            .expect("pull runs");
        assert_eq!(report.counts(), (1, 0, 1, 0));
        assert!(report.conflicts().any(|o| o.id == "engine"));
        assert!(report.success().any(|o| o.id == "root"));
        assert_eq!(report.exit_code(), 1);
        assert!(report.is_partial());

        // A fresh session (the GUI reopening the project) still sees the conflict.
        let reopened = open_session(&fixture);
        let status = reopened.status();
        assert!(status.has_conflicts());
        assert_eq!(project_state_kind(&status), ProjectStateKind::Conflicted);
        let pending = pending_work(&reopened, &status);
        assert!(pending.iter().any(|w| w.id == "engine" && w.blocked));
        // And the conflict blocks a commit in that repository only.
        let commit = reopened
            .commit(&CommitOptions::new("cannot"))
            .expect("commit runs");
        assert!(commit.conflicts().any(|o| o.id == "engine"));
    }

    #[test]
    fn fetch_reports_unreachable_remotes_without_touching_other_repositories() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.publish("engine", "engine.git");
        std::fs::remove_dir_all(fixture.bare_path("engine.git")).unwrap();

        let session = open_session(&fixture);
        let report = session.sync(&SyncOptions::new(), true).expect("fetch runs");
        let engine = report
            .outcomes
            .iter()
            .find(|o| o.id == "engine")
            .expect("engine outcome");
        assert!(engine.is_problem(), "{engine:?}");
        assert!(report.success().any(|o| o.id == "root"));
        assert!(!report.is_success());
    }

    #[test]
    fn operation_view_carries_details_and_counts() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        let session = open_session(&fixture);
        let report = session
            .commit(&CommitOptions::new("one repo"))
            .expect("commit runs");
        let status = session.status();
        let json = operation_view_json(&report, &status).to_string();
        assert!(json.contains("\"detailLines\""));
        assert!(json.contains("\"succeeded\":1"));
        assert!(json.contains("\"status\":["));
    }

    fn set_mode(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(mode);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    fn add_intent(path: &str, id: &str) -> manage::RepositoryIntent {
        manage::RepositoryIntent::Add {
            path: path.to_string(),
            id: id.to_string(),
            remote: None,
            branch: None,
            initialize: true,
            configure_remote: false,
            untrack_from_root: true,
        }
    }

    fn plan_management(
        session: &ProjectSession,
        fixture: &RepoFixture,
        intent: manage::RepositoryIntent,
    ) -> manage::RepositoryPlan {
        manage::plan(
            session.project(),
            &manage::RepositoryManagementRequest::one(intent),
            fixture.runner(),
        )
        .expect("the plan is built")
    }

    #[test]
    fn the_management_inspection_view_lists_repositories_candidates_and_state() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        let session = open_session(&fixture);
        let inspection = manage::inspect(session.project(), fixture.runner()).unwrap();
        let json = management_inspection_view_json(&inspection).to_string();

        assert!(json.contains("\"kind\":\"repositories\""), "{json}");
        assert!(
            json.contains("\"project\":{\"name\":\"project\""),
            "the project is named: {json}"
        );
        assert!(json.contains("\"id\":\"root\",\"role\":\"root\""), "{json}");
        assert!(json.contains("\"state\":{\"key\":\"ready\""), "{json}");
        assert!(json.contains("\"usable\":true"), "{json}");
        // The directory that is not configured yet is offered as a candidate, with the id
        // it would get, because that is what the interface shows before the button.
        assert!(
            json.contains("\"candidates\":[{\"name\":\"engine\""),
            "{json}"
        );
        assert!(json.contains("\"path\":\"engine\""), "{json}");
        assert!(json.contains("\"needingAttention\":0"), "{json}");
    }

    #[test]
    fn the_management_plan_view_describes_the_change_before_it_runs() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        fixture.write("new-module/lib.rs", "pub fn go() {}\n");
        // The root repository already tracks the file: bringing the directory in as a
        // repository of its own has to say what happens to that ownership.
        fixture.add_all(".");
        fixture.commit(".", "root tracks the module");
        let session = open_session(&fixture);
        let plan = plan_management(&session, &fixture, add_intent("new-module", ""));
        let json = management_plan_view_json(&plan).to_string();

        assert!(json.contains("\"kind\":\"plan\""), "{json}");
        assert!(json.contains("\"flow\":\"repository\""), "{json}");
        assert!(
            json.contains(&format!("\"id\":\"{}\"", plan.id)),
            "the plan id is what the interface reviews: {json}"
        );
        assert!(json.contains("\"ready\":true"), "{json}");
        assert!(json.contains("\"noop\":false"), "{json}");
        assert!(json.contains("changes to the configuration"), "{json}");
        // The preview is the manifest the execution writes, not a second rendering.
        assert!(
            json.contains(&format!(
                "\"manifest\":{{\"before\":{},\"after\":",
                Json::from(plan.manifest_before.clone().unwrap_or_default()).compact()
            )),
            "{json}"
        );
        for kind in [
            "add-repository",
            "initialize-repository",
            "untrack-from-root",
        ] {
            assert!(
                json.contains(&format!("\"kind\":\"{kind}\"")),
                "{kind} in {json}"
            );
        }
        assert!(json.contains("\"kind\":\"update-manifest\""), "{json}");
        assert!(
            !json.contains("\"symbol\":\"✗\""),
            "nothing is blocked here: {json}"
        );
        assert!(
            json.matches("\"symbol\":\"…\"").count() > 0,
            "planned rows say what will run: {json}"
        );
    }

    #[test]
    fn the_management_result_view_proves_every_change_and_reports_failures() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        fixture.write("new-module/lib.rs", "pub fn go() {}\n");
        let session = open_session(&fixture);
        let plan = plan_management(&session, &fixture, add_intent("new-module", ""));
        let result = manage::apply(
            &plan,
            false,
            fixture.runner(),
            &mut manage::RepositoryObserver::silent(),
        );
        let json = management_result_view_json(&result).to_string();

        assert!(json.contains("\"kind\":\"result\""), "{json}");
        assert!(json.contains("\"success\":true"), "{json}");
        assert!(json.contains("\"exitCode\":0"), "{json}");
        assert!(json.contains("\"failed\":0"), "{json}");
        assert!(json.contains("\"notApplied\":0"), "{json}");
        assert!(json.contains("\"sentence\":\""), "{json}");
        assert!(
            json.contains("\"outcome\":\"applied\""),
            "the changes are reported as applied: {json}"
        );
        assert!(
            json.contains("\"symbol\":\"✓\""),
            "the actions carry the shared symbols: {json}"
        );
        assert!(
            json.contains("\"evidence\":[\""),
            "every change carries the proof it happened: {json}"
        );
        assert!(
            json.contains("\"repositories\":[{\"id\":\"root\""),
            "the result carries the resulting project: {json}"
        );
        assert!(json.contains("\"validation\":{\"ok\":true"), "{json}");

        // Repeating the operation: the interface must be able to say "nothing to do"
        // rather than showing a change that did not happen. The project is reopened first,
        // exactly as the interface does when an operation finishes.
        let session = open_session(&fixture);
        let again = plan_management(&session, &fixture, add_intent("new-module", ""));
        let again_json = management_plan_view_json(&again).to_string();
        assert!(again_json.contains("\"noop\":true"), "{again_json}");
        assert!(again_json.contains("nothing to do"), "{again_json}");

        let replay = manage::apply(
            &plan,
            true,
            fixture.runner(),
            &mut manage::RepositoryObserver::silent(),
        );
        let replay_json = management_result_view_json(&replay).to_string();
        assert!(replay_json.contains("\"dryRun\":true"), "{replay_json}");
        assert!(replay_json.contains("Dry run"), "{replay_json}");
        assert!(replay_json.contains("\"applied\":0"), "{replay_json}");
        assert!(replay_json.contains("\"validation\":null"), "{replay_json}");
    }

    #[test]
    fn the_management_result_view_reports_a_failure_without_hiding_the_rest() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        // The directory exists but cannot be initialised: a real failure the interface has
        // to show as such, next to the repositories that were configured fine.
        fixture.write("good/lib.rs", "fn main() {}\n");
        fixture.write("blocked/lib.rs", "fn main() {}\n");
        fixture.write("new-module/lib.rs", "fn main() {}\n");
        let blocked = fixture.path().join("blocked");
        set_mode(&blocked, 0o500);

        let session = open_session(&fixture);
        let plan = manage::plan(
            session.project(),
            &manage::RepositoryManagementRequest {
                intents: vec![
                    add_intent("good", ""),
                    add_intent("blocked", ""),
                    add_intent("new-module", ""),
                ],
            },
            fixture.runner(),
        )
        .unwrap();
        let result = manage::apply(
            &plan,
            false,
            fixture.runner(),
            &mut manage::RepositoryObserver::silent(),
        );
        let json = management_result_view_json(&result).to_string();

        assert!(json.contains("\"success\":false"), "{json}");
        assert!(json.contains("\"exitCode\":1"), "{json}");
        assert!(json.contains("\"outcome\":\"failed\""), "{json}");
        assert!(json.contains("\"symbol\":\"✗\""), "{json}");
        assert!(
            json.contains("\"kind\":\"verify-project\""),
            "the verification says the project is not what the plan promised: {json}"
        );
        // The repositories that were configured are reported as succeeding: one failure
        // never hides the rest.
        assert!(json.contains("\"target\":\"good\""), "{json}");
        assert!(json.contains("\"target\":\"new-module\""), "{json}");
        assert!(json.contains("\"outcome\":\"success\""), "{json}");
        // And the failed one keeps the real Git error for the detail panel.
        assert!(json.contains("Permission denied"), "{json}");

        // Restore the permissions so the temporary directory can be cleaned up.
        set_mode(&blocked, 0o700);
    }

    #[test]
    fn the_management_candidate_view_answers_the_questions_before_adding() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        fixture.write("plain/lib.rs", "pub fn go() {}\n");
        fixture.init_repo("already");
        let session = open_session(&fixture);

        let plain =
            manage::inspect_candidate(session.project(), "plain", fixture.runner()).unwrap();
        let plain_json = management_candidate_view_json(&plain).to_string();
        assert!(
            plain_json.contains("\"isRepository\":false"),
            "{plain_json}"
        );
        assert!(plain_json.contains("\"canAdd\":true"), "{plain_json}");
        assert!(
            plain_json.contains("\"suggestedId\":\"plain\""),
            "{plain_json}"
        );
        assert!(plain_json.contains("git init"), "{plain_json}");
        assert!(plain_json.contains("\"blockers\":[]"), "{plain_json}");

        let existing =
            manage::inspect_candidate(session.project(), "already", fixture.runner()).unwrap();
        let existing_json = management_candidate_view_json(&existing).to_string();
        assert!(
            existing_json.contains("\"isRepository\":true"),
            "{existing_json}"
        );
        assert!(
            existing_json.contains("adopt") || existing_json.contains("already a Git repository"),
            "{existing_json}"
        );

        // A path that is not there is refused before anything is offered, never after
        // something was created.
        let missing =
            manage::inspect_candidate(session.project(), "nope", fixture.runner()).unwrap();
        let missing_json = management_candidate_view_json(&missing).to_string();
        assert!(missing_json.contains("\"exists\":false"), "{missing_json}");
        assert!(missing_json.contains("\"canAdd\":false"), "{missing_json}");
        assert!(
            !missing_json.contains("\"blockers\":[]"),
            "the reason is given: {missing_json}"
        );
    }

    #[test]
    fn refresh_after_an_external_change_sees_it() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let session = open_session(&fixture);
        assert!(session.changes().is_empty());
        // A change made by anything else (another tool, another developer, a build).
        fixture.write("engine/new.txt", "external");
        assert_eq!(session.changes().len(), 1);
    }
}
