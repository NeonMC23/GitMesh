//! Managing the physical repositories of an existing project.
//!
//! This is the post-creation counterpart of [`crate::setup`]: where the setup service
//! turns a directory into a project, this one changes which repositories a project is
//! made of, and how they are configured.
//!
//! It follows the same shape as the setup service on purpose, so that the two flows are
//! one mental model for the user and one code path for every front end:
//!
//! ```text
//! inspect -> plan -> (review) -> apply -> verify
//! ```
//!
//! * [`inspect`] reports the current configuration and state of every repository, and
//!   [`inspect_candidate`] answers the questions the "add a repository" flow has about
//!   one directory before anything is planned.
//! * [`plan`] turns a [`RepositoryManagementRequest`] into a [`RepositoryPlan`]: the typed
//!   list of **changes** the manifest will show, the typed list of **actions** that will
//!   run, the exact manifest text, the blockers, the warnings and the safety guarantees.
//!   It never writes anything.
//! * [`apply`] replays exactly the actions of the plan it is given — nothing else — and
//!   then *verifies every change against the filesystem and the written manifest*, so a
//!   change is only reported as applied when it really is.
//!
//! The plan keeps the two halves of a repository change apart, because they fail in
//! different ways:
//!
//! * **changes** are the user-visible consequences: a repository appears or disappears, an
//!   id changes, a remote is recorded or configured, files stop being tracked by the root
//!   repository. They are what the review screen lists, and what the result verifies.
//! * **actions** are the units of work that run: `git init`, `git remote`, `git rm
//!   --cached`, writing the manifest, and the final verification. Configuration-only
//!   changes (an id, a recorded remote, an entry that appears or disappears) are carried by
//!   the manifest write, which is exactly what the plan says.
//!
//! What this module deliberately does **not** do:
//!
//! * it never deletes, moves or renames anything: removing a repository from GitMesh
//!   removes a manifest entry and nothing else (the directory, `.git`, its history and its
//!   remote stay exactly where they are), and the result proves it;
//! * it never re-initialises an existing repository, and never replaces an `origin` the
//!   user did not explicitly ask to change;
//! * it never touches a repository the operation does not concern;
//! * it never writes the manifest by hand: the target configuration is a
//!   [`GitMeshProject`], rendered and written by [`crate::manifest`], so the file cannot
//!   drift from the schema the rest of GitMesh reads.
//!
//! Git itself is only ever driven through [`crate::discovery`] (which owns `git init`,
//! `git remote` and `git rm --cached` for repository boundaries) — there is no second Git
//! orchestration layer here, and no front end talks to this module's types through
//! anything but the shared [`crate::service`] view models.

use std::path::{Path, PathBuf};

use crate::discovery::{self, OriginChange};
use crate::error::{Error, Result};
use crate::git::history;
use crate::git::GitRunner;
use crate::manifest;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryRole};
use crate::ops::OutcomeKind;
use crate::paths::{self, to_slash};
use crate::setup::{self, StepState, ValidationReport};

/// Longest id GitMesh accepts, so a manifest stays readable.
const MAX_ID_LEN: usize = 64;

// --------------------------------------------------------------- inspection --

/// The state of one configured repository, as found on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryState {
    /// The directory exists and is a Git repository of its own.
    Ready,
    /// The directory exists but holds no Git repository of its own.
    NoRepository,
    /// The directory recorded in the manifest does not exist.
    Missing,
}

impl RepositoryState {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            RepositoryState::Ready => "ready",
            RepositoryState::NoRepository => "no-repository",
            RepositoryState::Missing => "missing",
        }
    }

    /// Wording used in the repository list.
    pub fn sentence(self) -> &'static str {
        match self {
            RepositoryState::Ready => "ready",
            RepositoryState::NoRepository => "no Git repository here yet",
            RepositoryState::Missing => "the directory is missing",
        }
    }
}

/// One configured repository: its configuration, its state, and anything worth knowing
/// before touching it.
///
/// This is the row the repository list shows; facts only, no plan and no change.
#[derive(Debug, Clone)]
pub struct ManagedRepository {
    /// Logical id.
    pub id: String,
    /// Project-relative path (`.` for the root repository).
    pub path: String,
    /// Root or external.
    pub role: RepositoryRole,
    /// Remote recorded in the manifest.
    pub manifest_remote: Option<String>,
    /// `origin` actually configured in Git, if any.
    pub origin: Option<String>,
    /// Provider id when the recorded remote belongs to a known provider.
    pub provider: Option<String>,
    /// Branch hint recorded in the manifest.
    pub branch_hint: Option<String>,
    /// State of the directory.
    pub state: RepositoryState,
    /// True when the directory exists.
    pub exists: bool,
    /// True when the directory is a Git repository root.
    pub is_repository: bool,
    /// True when the repository has at least one commit.
    pub has_commits: bool,
    /// Branch currently checked out.
    pub branch: Option<String>,
    /// Short id of `HEAD`, when there is one.
    pub head: Option<String>,
    /// True when the work tree has no change at all.
    pub clean: bool,
    /// Number of entries Git reports as changed.
    pub changes: usize,
    /// Commits ahead of the upstream.
    pub ahead: Option<u32>,
    /// Commits behind the upstream.
    pub behind: Option<u32>,
    /// Files the root repository tracks inside this repository's directory.
    ///
    /// Non-zero means two repositories claim the same files, which is the ambiguity the
    /// wizard removes with "stop tracking in the root repository".
    pub tracked_by_root: usize,
    /// Problems that need attention.
    pub issues: Vec<String>,
    /// Things worth knowing that are not problems.
    pub warnings: Vec<String>,
}

impl ManagedRepository {
    /// True when this repository can take part in normal GitMesh operations right now.
    pub fn is_usable(&self) -> bool {
        self.state == RepositoryState::Ready
    }

    /// How the repository is hosted, as one line for the list.
    pub fn remote_label(&self) -> String {
        match (&self.manifest_remote, &self.origin) {
            (Some(remote), Some(origin)) if remote == origin => remote.clone(),
            (Some(remote), Some(origin)) => format!("{remote} (origin: {origin})"),
            (Some(remote), None) => format!("{remote} (recorded only)"),
            (None, Some(origin)) => format!("unmanaged origin: {origin}"),
            (None, None) => "local only".to_string(),
        }
    }
}

/// Everything the repository view needs: the configuration, the state of each
/// repository, and the directories that could become repositories.
#[derive(Debug, Clone)]
pub struct RepositoryInspection {
    /// Logical project name.
    pub project_name: String,
    /// Project root.
    pub root: PathBuf,
    /// Manifest that holds the configuration.
    pub manifest_path: PathBuf,
    /// One row per configured repository, root first.
    pub repositories: Vec<ManagedRepository>,
    /// Directories of the project that could become repositories, as the setup scanner
    /// sees them (the same candidates the wizard offers).
    pub candidates: Vec<setup::CandidateDirectory>,
    /// Remarks about the project as a whole.
    pub notices: Vec<String>,
    /// Problems of the project as a whole.
    pub warnings: Vec<String>,
}

impl RepositoryInspection {
    /// The repository with this id, if there is one.
    pub fn repository(&self, id: &str) -> Option<&ManagedRepository> {
        self.repositories.iter().find(|repo| repo.id == id)
    }

    /// Repositories that need attention.
    pub fn needing_attention(&self) -> impl Iterator<Item = &ManagedRepository> {
        self.repositories
            .iter()
            .filter(|repo| !repo.issues.is_empty())
    }
}

/// Read the configuration and the state of every repository it lists.
///
/// Read-only: no Git command that changes anything is run, and the manifest is only read.
pub fn inspect(project: &GitMeshProject, runner: &GitRunner) -> Result<RepositoryInspection> {
    let mut repositories: Vec<ManagedRepository> = Vec::new();
    for repo in project.sorted_repositories() {
        repositories.push(inspect_repository(project, repo, runner));
    }

    let mut notices: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    if !discovery::is_repository_root(&project.root, true, runner) {
        notices.push(
            "the project root is not a Git repository, so the root repository has no history \
             of its own yet"
                .to_string(),
        );
    }
    // The candidate list comes from the setup scanner: one implementation of "which
    // directories could become repositories", shared by the wizard and by this view.
    let candidates = match setup::inspect(&project.root, runner) {
        Ok(inspection) => inspection.candidates,
        Err(err) => {
            warnings.push(format!(
                "the project tree could not be scanned completely: {}",
                short_message(&err)
            ));
            Vec::new()
        }
    };

    Ok(RepositoryInspection {
        project_name: project.name.clone(),
        root: project.root.clone(),
        manifest_path: manifest::manifest_path(&project.root),
        repositories,
        candidates,
        notices,
        warnings,
    })
}

/// Facts about one configured repository.
fn inspect_repository(
    project: &GitMeshProject,
    repo: &PhysicalRepository,
    runner: &GitRunner,
) -> ManagedRepository {
    let path = project.repository_path(repo);
    let exists = path.is_dir();
    let is_repository = exists && discovery::is_repository_root(&path, true, runner);

    let mut branch: Option<String> = None;
    let mut head: Option<String> = None;
    let mut has_commits = false;
    let mut clean = true;
    let mut changes = 0usize;
    let mut ahead: Option<u32> = None;
    let mut behind: Option<u32> = None;
    if is_repository {
        if let Ok(head_state) = runner.repo(&path).head() {
            branch = head_state.branch().map(str::to_string);
        }
        if let Ok(Some(oid)) = runner.repo(&path).head_oid() {
            has_commits = true;
            head = Some(crate::git::short_oid(&oid));
        }
        if let Ok(status) = runner.repo(&path).status() {
            clean = status.is_fully_clean();
            changes = status.entries.iter().filter(|entry| !entry.ignored).count();
            ahead = status.ahead;
            behind = status.behind;
        }
    }

    let origin = if is_repository {
        discovery::origin_url(&path, runner).unwrap_or(None)
    } else {
        None
    };

    let mut issues: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let state = if !exists {
        issues.push(format!(
            "the directory '{}' recorded in the manifest does not exist",
            to_slash(&repo.relative_path)
        ));
        RepositoryState::Missing
    } else if !is_repository {
        issues.push(
            "there is no Git repository in this directory yet; initialize one, or clone an \
             existing repository into it"
                .to_string(),
        );
        RepositoryState::NoRepository
    } else {
        RepositoryState::Ready
    };

    // Remote drift is worth knowing about, but must never block an unrelated operation:
    // GitMesh records what the manifest says and reports what Git has.
    match (&repo.remote_url, &origin) {
        (Some(remote), None) if is_repository => warnings.push(format!(
            "the manifest records the remote {remote} but the repository has no origin"
        )),
        (Some(remote), Some(current)) if remote != current && is_repository => {
            warnings.push(format!(
                "origin points at {current} while the manifest records {remote}; use \"change \
                 remote\" to reconcile them"
            ))
        }
        (None, Some(current)) if is_repository => warnings.push(format!(
            "origin {current} is configured in Git but no remote is recorded in the manifest"
        )),
        _ => {}
    }

    let tracked_by_root = if repo.is_root() {
        0
    } else {
        discovery::count_files_tracked_under(&project.root, &repo.relative_path, runner)
    };
    if tracked_by_root > 0 {
        warnings.push(format!(
            "the root repository still tracks {tracked_by_root} file(s) inside '{}'; the same \
             files belong to two repositories",
            to_slash(&repo.relative_path)
        ));
    }

    ManagedRepository {
        id: repo.id.clone(),
        path: if repo.is_root() {
            ".".to_string()
        } else {
            to_slash(&repo.relative_path)
        },
        role: repo.role,
        manifest_remote: repo.remote_url.clone(),
        origin,
        provider: repo
            .remote_url
            .as_deref()
            .and_then(crate::providers::provider_for_remote)
            .map(|provider| provider.id().to_string()),
        branch_hint: repo.branch.clone(),
        state,
        exists,
        is_repository,
        has_commits,
        branch,
        head,
        clean,
        changes,
        ahead,
        behind,
        tracked_by_root,
        issues,
        warnings,
    }
}

/// What the "add a repository" flow knows about a directory before planning anything.
///
/// Every question the flow asks is answered here, from the filesystem and from the project
/// configuration — never by the browser.
#[derive(Debug, Clone)]
pub struct CandidateInspection {
    /// Project-relative path, normalised.
    pub path: String,
    /// Absolute path.
    pub absolute: PathBuf,
    /// True when the directory exists.
    pub exists: bool,
    /// True when the directory is a Git repository of its own.
    pub is_repository: bool,
    /// True when that repository has at least one commit.
    pub has_commits: bool,
    /// True when the directory exists and holds no entries at all.
    pub empty_directory: bool,
    /// Branch currently checked out.
    pub branch: Option<String>,
    /// `origin` configured in Git, if any.
    pub origin: Option<String>,
    /// Files the root repository tracks inside this directory.
    pub tracked_by_root: usize,
    /// Git repositories nested inside this directory (project-relative).
    pub nested_repositories: Vec<String>,
    /// The id GitMesh would use if the user does not pick one.
    pub suggested_id: String,
    /// Set when the directory already is a configured repository.
    pub managed_as: Option<String>,
    /// Reasons the directory cannot become a repository.
    pub blockers: Vec<String>,
    /// Things the user should know before confirming.
    pub warnings: Vec<String>,
}

impl CandidateInspection {
    /// True when the directory can become a repository.
    pub fn can_add(&self) -> bool {
        self.blockers.is_empty()
    }

    /// What GitMesh would do with the directory, in one sentence.
    pub fn consequence(&self) -> String {
        if !self.exists {
            return "nothing to do: the directory does not exist".to_string();
        }
        if self.is_repository {
            let mut text =
                "adopt the existing Git repository (its history and remote are kept)".to_string();
            if let Some(origin) = &self.origin {
                text.push_str(&format!("; origin {origin} is already configured"));
            }
            text
        } else {
            "create a Git repository in it (git init -b main)".to_string()
        }
    }
}

/// Answer every question the "add a repository" flow has about one directory.
pub fn inspect_candidate(
    project: &GitMeshProject,
    raw_path: &str,
    runner: &GitRunner,
) -> Result<CandidateInspection> {
    let relative = paths::normalize_relative(raw_path.trim())
        .map_err(|e| Error::Other(format!("invalid repository path: {e}")))?;
    if paths::is_root_relative(&relative) {
        return Err(Error::Other(
            "the project root is always the root repository; choose a directory inside the \
             project"
                .to_string(),
        ));
    }
    let absolute = project.root.join(&relative);
    if !paths::is_within(&project.root, &absolute) {
        return Err(Error::OutsideProject {
            path: absolute,
            root: project.root.clone(),
        });
    }

    let check = discovery::check_assignment(project, &relative, runner)?;
    let exists = check.exists;
    let is_repository = check.is_repository;
    let (has_commits, branch, origin) = if is_repository {
        (
            runner.repo(&absolute).head_oid()?.is_some(),
            runner
                .repo(&absolute)
                .head()
                .ok()
                .and_then(|head| head.branch().map(str::to_string)),
            discovery::origin_url(&absolute, runner)?,
        )
    } else {
        (false, None, None)
    };
    let nested_repositories = if exists && !is_repository {
        discovery::scan_nested(&absolute, runner)
            .into_iter()
            .map(|nested| to_slash(&relative.join(nested)))
            .collect()
    } else {
        Vec::new()
    };
    // Only a repository of its own counts: the root repository owns every path that is not
    // assigned elsewhere, and saying so would read as "already managed".
    let managed_as = project
        .repository_for_relative(&relative)
        .filter(|repo| !repo.is_root())
        .map(|repo| repo.id.clone());

    let blockers = check.blockers.clone();
    let mut warnings = check.warnings.clone();
    if !nested_repositories.is_empty() {
        warnings.push(format!(
            "'{}' contains the Git repository '{}'; GitMesh does not manage nested repositories \
             and will not touch it",
            to_slash(&relative),
            nested_repositories[0]
        ));
    }
    let empty_directory = exists && !is_repository && discovery::is_empty_directory(&absolute);
    if is_repository && !has_commits {
        warnings.push(format!(
            "'{}' has no commits yet: pull and push skip it until it has a commit (a remote \
             that already has history is adopted when it is connected)",
            to_slash(&relative)
        ));
    }
    let tracked_by_root = if exists {
        discovery::count_files_tracked_under(&project.root, &relative, runner)
    } else {
        0
    };
    if tracked_by_root > 0 {
        warnings.push(format!(
            "the root repository tracks {tracked_by_root} file(s) inside '{}'; adding it as a \
             repository without stopping that makes the same files belong to two repositories",
            to_slash(&relative)
        ));
    }

    Ok(CandidateInspection {
        path: to_slash(&relative),
        absolute,
        exists,
        is_repository,
        has_commits,
        empty_directory,
        branch,
        origin,
        tracked_by_root,
        nested_repositories,
        suggested_id: discovery::suggest_id(&relative, project),
        managed_as,
        blockers,
        warnings,
    })
}

// ------------------------------------------------------------------ request --

/// One repository-management intent.
///
/// A request is a list of these, applied in order. The graphical interface sends one at a
/// time; the command line can batch them, and the plan is the same either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryIntent {
    /// Bring a directory of the project in as a repository.
    ///
    /// Covers the three cases the interface offers: adopting a directory that already is a
    /// Git repository, initialising a new one, and recording a remote without touching Git.
    Add {
        /// Project-relative path.
        path: String,
        /// Logical id; empty means "use the suggestion".
        id: String,
        /// Remote URL to record.
        remote: Option<String>,
        /// Branch hint to record.
        branch: Option<String>,
        /// Create a Git repository when the directory does not hold one.
        initialize: bool,
        /// Configure `origin` in Git as well as recording the remote.
        configure_remote: bool,
        /// Stop tracking the directory's files in the root repository.
        untrack_from_root: bool,
    },
    /// Clone a remote into a missing or empty directory of the project, and record it as a
    /// repository. The remote is read before anything is planned, and nothing that exists
    /// is overwritten.
    Clone {
        /// Project-relative path of the new repository.
        path: String,
        /// Logical id; empty means "use the suggestion".
        id: String,
        /// URL or local path of the remote to clone.
        remote: String,
        /// Branch hint to record.
        branch: Option<String>,
    },
    /// Remove a repository from the project configuration. Its directory, its `.git`, its
    /// history and its remote are never touched.
    Remove {
        /// Logical id.
        id: String,
        /// Confirms the ownership consequence when the root repository currently tracks
        /// files inside the directory.
        confirm_takeover: bool,
    },
    /// Give a repository another logical id. The directory is never renamed.
    Rename {
        /// Current id.
        id: String,
        /// New id.
        new_id: String,
    },
    /// Record a different remote for one repository, and optionally configure it in Git.
    SetRemote {
        /// Logical id.
        id: String,
        /// New URL; `None` clears the recorded remote (Git is left alone).
        remote: Option<String>,
        /// Also run `git remote add` / `git remote set-url` when a URL is given.
        configure: bool,
    },
}

impl RepositoryIntent {
    /// Stable machine-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            RepositoryIntent::Add { .. } => "add",
            RepositoryIntent::Clone { .. } => "clone",
            RepositoryIntent::Remove { .. } => "remove",
            RepositoryIntent::Rename { .. } => "rename",
            RepositoryIntent::SetRemote { .. } => "set-remote",
        }
    }

    /// The repository id the intent concerns (empty for an add without an id yet).
    pub fn target(&self) -> &str {
        match self {
            RepositoryIntent::Add { id, .. } => id,
            RepositoryIntent::Clone { id, .. } => id,
            RepositoryIntent::Remove { id, .. } => id,
            RepositoryIntent::Rename { id, .. } => id,
            RepositoryIntent::SetRemote { id, .. } => id,
        }
    }
}

/// What the user asks for: a list of intents against one project.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryManagementRequest {
    /// Intents, applied in order.
    pub intents: Vec<RepositoryIntent>,
}

impl RepositoryManagementRequest {
    /// A request with a single intent.
    pub fn one(intent: RepositoryIntent) -> Self {
        RepositoryManagementRequest {
            intents: vec![intent],
        }
    }
}

// ------------------------------------------------------------------- changes --

/// The kind of consequence the manifest will show.
///
/// These are the user-visible changes: one row per thing the user asked for, plus the
/// things GitMesh has to tell them about. The physical work they imply is listed
/// separately, as [`RepositoryAction`]s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryChangeKind {
    /// A directory becomes a managed repository.
    AddRepository,
    /// Clone a remote into a new directory.
    CloneRepository,
    /// An existing Git repository is managed as it is.
    AdoptRepository,
    /// A Git repository is created in a directory.
    InitializeRepository,
    /// A repository leaves the project configuration (nothing on disk changes).
    RemoveRepositoryFromManifest,
    /// A repository gets another logical id (nothing on disk changes).
    RenameRepository,
    /// `origin` is added.
    ConfigureRemote,
    /// `origin` is replaced.
    UpdateRemote,
    /// The recorded remote stays as it is.
    KeepExistingRemote,
    /// The remote is recorded in the manifest without touching Git.
    RecordRemote,
    /// The recorded remote is cleared (Git is left alone).
    ClearRemote,
    /// Files stop being tracked by the root repository.
    UntrackFromRoot,
    /// A repository is already configured exactly as requested.
    AlreadyManaged,
}

impl RepositoryChangeKind {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            RepositoryChangeKind::AddRepository => "add-repository",
            RepositoryChangeKind::CloneRepository => "clone-repository",
            RepositoryChangeKind::AdoptRepository => "adopt-repository",
            RepositoryChangeKind::InitializeRepository => "initialize-repository",
            RepositoryChangeKind::RemoveRepositoryFromManifest => "remove-repository",
            RepositoryChangeKind::RenameRepository => "rename-repository",
            RepositoryChangeKind::ConfigureRemote => "configure-remote",
            RepositoryChangeKind::UpdateRemote => "update-remote",
            RepositoryChangeKind::KeepExistingRemote => "keep-existing-remote",
            RepositoryChangeKind::RecordRemote => "record-remote",
            RepositoryChangeKind::ClearRemote => "clear-remote",
            RepositoryChangeKind::UntrackFromRoot => "untrack-from-root",
            RepositoryChangeKind::AlreadyManaged => "already-managed",
        }
    }

    /// Heading used in the review screen.
    pub fn heading(self) -> &'static str {
        match self {
            RepositoryChangeKind::AddRepository
            | RepositoryChangeKind::CloneRepository
            | RepositoryChangeKind::AdoptRepository
            | RepositoryChangeKind::InitializeRepository
            | RepositoryChangeKind::AlreadyManaged => "Repositories",
            RepositoryChangeKind::RemoveRepositoryFromManifest
            | RepositoryChangeKind::RenameRepository => "Configuration",
            RepositoryChangeKind::ConfigureRemote
            | RepositoryChangeKind::UpdateRemote
            | RepositoryChangeKind::KeepExistingRemote
            | RepositoryChangeKind::RecordRemote
            | RepositoryChangeKind::ClearRemote => "Remotes",
            RepositoryChangeKind::UntrackFromRoot => "Ownership",
        }
    }

    /// True when this change alters the manifest.
    pub fn changes_configuration(self) -> bool {
        !matches!(
            self,
            RepositoryChangeKind::KeepExistingRemote | RepositoryChangeKind::AlreadyManaged
        )
    }
}

/// One consequence of the plan, with the exact before/after values.
#[derive(Debug, Clone)]
pub struct RepositoryChange {
    /// What kind of change this is.
    pub kind: RepositoryChangeKind,
    /// Repository id (the new id, for a rename).
    pub id: String,
    /// Project-relative path.
    pub path: String,
    /// Previous value, when there was one (old id, old remote).
    pub before: Option<String>,
    /// New value, when there is one.
    pub after: Option<String>,
    /// One sentence the user reads.
    pub detail: String,
    /// Whether it will run, is already in place, or is refused.
    pub state: StepState,
}

impl RepositoryChange {
    /// True when this change will be applied.
    pub fn is_planned(&self) -> bool {
        self.state == StepState::Planned
    }

    /// True when nothing has to be done for this change.
    pub fn is_already_in_place(&self) -> bool {
        matches!(self.state, StepState::AlreadySatisfied(_))
    }
}

// ------------------------------------------------------------------- actions --

/// The unit of work a plan executes.
///
/// Every action is either a physical change to one repository, the manifest write, or the
/// final verification. Configuration-only changes (an id, a recorded remote, an entry that
/// appears or disappears) are carried by [`RepositoryChange`]s and land with the manifest
/// write: that split is what keeps the plan honest about what actually runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryActionKind {
    /// Use the Git repository that is already there, as it is.
    AdoptRepository,
    /// Run `git clone` into the planned directory.
    CloneRepository,
    /// `git init` in a directory that has no repository of its own.
    InitializeRepository,
    /// Add `origin` to a repository that has none.
    ConfigureRemote,
    /// Replace the `origin` of a repository, with explicit confirmation.
    UpdateRemote,
    /// Stop tracking a directory's files in the root repository.
    UntrackFromRoot,
    /// Check out the remote's existing history into a repository that has no commits yet.
    AdoptRemoteHistory,
    /// Write the configuration to `.gitmesh/project.toml`.
    UpdateManifest,
    /// Re-open the project and check every repository after the change.
    VerifyProject,
}

impl RepositoryActionKind {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            RepositoryActionKind::AdoptRepository => "adopt-repository",
            RepositoryActionKind::CloneRepository => "clone-repository",
            RepositoryActionKind::InitializeRepository => "initialize-repository",
            RepositoryActionKind::ConfigureRemote => "configure-remote",
            RepositoryActionKind::UpdateRemote => "update-remote",
            RepositoryActionKind::UntrackFromRoot => "untrack-from-root",
            RepositoryActionKind::AdoptRemoteHistory => "adopt-remote-history",
            RepositoryActionKind::UpdateManifest => "update-manifest",
            RepositoryActionKind::VerifyProject => "verify-project",
        }
    }

    /// Heading used in the review screen.
    pub fn heading(self) -> &'static str {
        match self {
            RepositoryActionKind::AdoptRepository
            | RepositoryActionKind::InitializeRepository
            | RepositoryActionKind::CloneRepository => "Repositories",
            RepositoryActionKind::ConfigureRemote | RepositoryActionKind::UpdateRemote => "Remotes",
            RepositoryActionKind::AdoptRemoteHistory => "Repositories",
            RepositoryActionKind::UntrackFromRoot => "Ownership",
            RepositoryActionKind::UpdateManifest => "Configuration",
            RepositoryActionKind::VerifyProject => "Validation",
        }
    }

    /// True when this action can change the project.
    ///
    /// Adopting is using what is there and verifying is reading: a plan made only of those
    /// has nothing to do, which is what repeating an operation has to report.
    pub fn changes_anything(self) -> bool {
        !matches!(
            self,
            RepositoryActionKind::AdoptRepository | RepositoryActionKind::VerifyProject
        )
    }

    /// Short role label, the way the interface names the progress row.
    pub fn role(self) -> &'static str {
        match self {
            RepositoryActionKind::AdoptRepository
            | RepositoryActionKind::InitializeRepository
            | RepositoryActionKind::CloneRepository => "Repository",
            RepositoryActionKind::ConfigureRemote | RepositoryActionKind::UpdateRemote => "Remote",
            RepositoryActionKind::AdoptRemoteHistory => "History",
            RepositoryActionKind::UntrackFromRoot => "Root index",
            RepositoryActionKind::UpdateManifest => "Manifest",
            RepositoryActionKind::VerifyProject => "Validation",
        }
    }
}

/// One step of a management plan.
#[derive(Debug, Clone)]
pub struct RepositoryAction {
    /// What kind of change this is.
    pub kind: RepositoryActionKind,
    /// Repository id the action concerns, or `manifest` / `project`.
    pub target: String,
    /// Project-relative path.
    pub path: String,
    /// Sentence used in the review screen.
    pub detail: String,
    /// Whether it runs, is already satisfied, or is blocked.
    pub state: StepState,
}

impl RepositoryAction {
    /// True when this action will be executed.
    pub fn planned(&self) -> bool {
        self.state == StepState::Planned
    }

    /// True when nothing has to be done for this action.
    pub fn is_already_in_place(&self) -> bool {
        matches!(self.state, StepState::AlreadySatisfied(_))
    }

    /// Row identifier used by the interface: two actions can concern the same target (the
    /// manifest and the verification both mention the project), so the kind is part of it.
    pub fn row_id(&self) -> String {
        format!("{}:{}", self.kind.label(), self.target)
    }
}

/// What an existing repository looked like at plan time, so the result can prove that a
/// removal did not touch it.
#[derive(Debug, Clone)]
pub struct PlannedRemoval {
    /// Logical id that leaves the configuration.
    pub id: String,
    /// Project-relative path.
    pub path: String,
    /// True when the directory existed and was a Git repository of its own.
    pub is_repository: bool,
    /// `HEAD` at plan time.
    pub head: Option<String>,
    /// `origin` at plan time.
    pub origin: Option<String>,
    /// Files the root repository tracked inside it at plan time.
    pub tracked_by_root: usize,
}

// ---------------------------------------------------------------------- plan --

/// The complete, reviewable description of a repository-management operation.
///
/// The same value drives the review screen and the execution: [`apply`] replays `actions`
/// and nothing else, and verifies `changes` afterwards.
#[derive(Debug, Clone)]
pub struct RepositoryPlan {
    /// Fingerprint of everything the plan contains (see [`RepositoryPlan::fingerprint`]).
    pub id: String,
    /// The request this plan answers.
    pub request: RepositoryManagementRequest,
    /// The configuration as it is now.
    pub project: GitMeshProject,
    /// The configuration the plan would write.
    pub target: GitMeshProject,
    /// Project name.
    pub name: String,
    /// Project root.
    pub root: PathBuf,
    /// Manifest path.
    pub manifest_path: PathBuf,
    /// Manifest text as it is on disk (`None` when there is none).
    pub manifest_before: Option<String>,
    /// Manifest text the plan would write.
    pub manifest_after: String,
    /// True when the plan writes a different manifest.
    pub manifest_changes: bool,
    /// What changes, from the user's point of view.
    pub changes: Vec<RepositoryChange>,
    /// What runs, in execution order.
    pub actions: Vec<RepositoryAction>,
    /// What the removals looked like before, so the result can prove they were kept.
    pub removals: Vec<PlannedRemoval>,
    /// Reasons the whole plan cannot be applied.
    pub blockers: Vec<String>,
    /// Things worth knowing before confirming.
    pub warnings: Vec<String>,
    /// Remarks that are not warnings (nothing to do, kept as it is, ...).
    pub notices: Vec<String>,
    /// What the plan guarantees, in plain sentences.
    pub safety: Vec<String>,
    /// Ids whose recorded remote the plan guarantees is `origin` after the run.
    pub expected_origins: Vec<String>,
}

impl RepositoryPlan {
    /// True when the plan can be applied.
    pub fn is_ready(&self) -> bool {
        self.blockers.is_empty()
    }

    /// Changes that will be applied.
    pub fn planned_changes(&self) -> impl Iterator<Item = &RepositoryChange> {
        self.changes
            .iter()
            .filter(|change| change.state == StepState::Planned)
    }

    /// Changes that are already in place.
    pub fn satisfied_changes(&self) -> impl Iterator<Item = &RepositoryChange> {
        self.changes
            .iter()
            .filter(|change| matches!(change.state, StepState::AlreadySatisfied(_)))
    }

    /// Changes the plan refuses, with their reasons.
    pub fn blocked_changes(&self) -> impl Iterator<Item = &RepositoryChange> {
        self.changes
            .iter()
            .filter(|change| matches!(change.state, StepState::Blocked(_)))
    }

    /// Actions that will be executed.
    pub fn planned_actions(&self) -> impl Iterator<Item = &RepositoryAction> {
        self.actions
            .iter()
            .filter(|action| action.state == StepState::Planned)
    }

    /// Actions that are already satisfied.
    pub fn satisfied_actions(&self) -> impl Iterator<Item = &RepositoryAction> {
        self.actions
            .iter()
            .filter(|action| matches!(action.state, StepState::AlreadySatisfied(_)))
    }

    /// Actions the plan refuses.
    pub fn blocked_actions(&self) -> impl Iterator<Item = &RepositoryAction> {
        self.actions
            .iter()
            .filter(|action| matches!(action.state, StepState::Blocked(_)))
    }

    /// True when the plan would change nothing at all.
    ///
    /// Repeating an operation is safe by construction: the second plan reports what is
    /// already there and executes nothing.
    pub fn is_noop(&self) -> bool {
        self.is_ready()
            && !self
                .planned_actions()
                .any(|action| action.kind.changes_anything())
    }

    /// Fingerprint of the plan: the configuration it was made from, the configuration it
    /// would write, every change, every action and every blocker.
    ///
    /// The interface sends it back when the user confirms, so a plan that no longer matches
    /// the project on disk can never be executed silently: the identifiers differ and the
    /// user is asked to review again.
    pub fn fingerprint(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!("root={}", to_slash(&self.root)));
        parts.push(format!(
            "before={}",
            self.manifest_before.clone().unwrap_or_default()
        ));
        parts.push(format!("after={}", self.manifest_after));
        for intent in &self.request.intents {
            parts.push(format!("intent={}", describe_intent(intent)));
        }
        for change in &self.changes {
            parts.push(format!(
                "change={}|{}|{}|{}|{}|{}",
                change.kind.label(),
                change.id,
                change.path,
                change.before.clone().unwrap_or_default(),
                change.after.clone().unwrap_or_default(),
                change.state.label()
            ));
        }
        for action in &self.actions {
            parts.push(format!(
                "action={}|{}|{}|{}",
                action.kind.label(),
                action.target,
                action.path,
                action.state.label()
            ));
        }
        for removal in &self.removals {
            parts.push(format!(
                "removal={}|{}|{}",
                removal.id,
                removal.path,
                removal.head.clone().unwrap_or_default()
            ));
        }
        for blocker in &self.blockers {
            parts.push(format!("blocker={blocker}"));
        }
        fnv1a(&parts.join("\n"))
    }

    /// Short human-readable summary of what will happen.
    pub fn summary(&self) -> String {
        if !self.is_ready() {
            return format!(
                "{} problem(s) must be fixed before the configuration can change",
                self.blocked_changes().count()
            );
        }
        let planned = self.planned_changes().count();
        if planned == 0 {
            return "nothing to do: the project is already configured as requested".to_string();
        }
        format!(
            "{} to the configuration, {} to run",
            count_label(planned, "change", "changes"),
            count_label(self.planned_actions().count(), "step", "steps")
        )
    }
}

/// Canonical text of one intent, used by the fingerprint.
fn describe_intent(intent: &RepositoryIntent) -> String {
    match intent {
        RepositoryIntent::Add {
            path,
            id,
            remote,
            branch,
            initialize,
            configure_remote,
            untrack_from_root,
        } => format!(
            "add {path} id={id} remote={} branch={} init={initialize} configure={configure_remote} \
             untrack={untrack_from_root}",
            remote.clone().unwrap_or_default(),
            branch.clone().unwrap_or_default()
        ),
        RepositoryIntent::Clone {
            path,
            id,
            remote,
            branch,
        } => format!(
            "clone {remote} into {path} id={id} branch={}",
            branch.clone().unwrap_or_default()
        ),
        RepositoryIntent::Remove {
            id,
            confirm_takeover,
        } => format!("remove {id} confirm={confirm_takeover}"),
        RepositoryIntent::Rename { id, new_id } => format!("rename {id} to {new_id}"),
        RepositoryIntent::SetRemote {
            id,
            remote,
            configure,
        } => format!(
            "set-remote {id} remote={} configure={configure}",
            remote.clone().unwrap_or_default()
        ),
    }
}

/// FNV-1a, 64 bit, hex — the same dependency-free fingerprint the setup plan uses.
fn fnv1a(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Accumulates one plan while it is derived.
struct Planner {
    project: GitMeshProject,
    target: GitMeshProject,
    manifest_before: Option<String>,
    manifest_after: String,
    changes: Vec<RepositoryChange>,
    actions: Vec<RepositoryAction>,
    removals: Vec<PlannedRemoval>,
    blockers: Vec<String>,
    warnings: Vec<String>,
    notices: Vec<String>,
    expected_origins: Vec<String>,
}

/// Turn a request into a plan. Never writes anything.
pub fn plan(
    project: &GitMeshProject,
    request: &RepositoryManagementRequest,
    runner: &GitRunner,
) -> Result<RepositoryPlan> {
    let root = project.root.clone();
    let manifest_path = manifest::manifest_path(&root);
    let manifest_before = std::fs::read_to_string(&manifest_path).ok();

    let mut planner = Planner {
        project: project.clone(),
        target: project.clone(),
        manifest_before: manifest_before.clone(),
        manifest_after: String::new(),
        changes: Vec::new(),
        actions: Vec::new(),
        removals: Vec::new(),
        blockers: Vec::new(),
        warnings: Vec::new(),
        notices: Vec::new(),
        expected_origins: Vec::new(),
    };

    // The plan is bound to the configuration it was made from: the manifest on disk must
    // describe exactly the project the caller opened, or this would silently be a plan for
    // some other configuration.
    match &manifest_before {
        None => planner.blockers.push(format!(
            "{} does not exist; there is no project to manage",
            manifest_path.display()
        )),
        Some(text) => match manifest::parse_manifest(text, &root, &manifest_path) {
            Ok(on_disk) => {
                if !same_configuration(&on_disk, project) {
                    planner.blockers.push(
                        "the manifest on disk describes a different configuration than the one \
                         that was opened; reopen the project and plan again"
                            .to_string(),
                    );
                }
            }
            Err(err) => planner.blockers.push(format!(
                "the manifest on disk cannot be read: {}",
                short_message(&err)
            )),
        },
    }

    if request.intents.is_empty() {
        planner
            .blockers
            .push("the request does not ask for any change".to_string());
    }
    for intent in &request.intents {
        plan_intent(&mut planner, intent, runner);
    }

    // The configuration is validated with exactly the rules a manifest loaded from disk
    // gets, and the text is produced once, here, so the review shows what will be written.
    planner
        .target
        .repositories
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    if let Err(issues) = manifest::validation::validate_project(&planner.target) {
        planner.blockers.extend(issues);
    }
    planner.manifest_after = match manifest::render_manifest(&planner.target) {
        Ok(text) => text,
        Err(err) => {
            planner.blockers.push(format!(
                "the requested configuration cannot be written: {}",
                short_message(&err)
            ));
            String::new()
        }
    };
    if !planner.manifest_after.is_empty()
        && manifest::parse_manifest(&planner.manifest_after, &root, &manifest_path).is_err()
    {
        planner
            .blockers
            .push("the requested configuration is not valid".to_string());
    }

    let manifest_changes =
        planner.manifest_before.as_deref() != Some(planner.manifest_after.as_str());
    if !planner.manifest_after.is_empty() {
        let manifest_rel = format!("{}/{}", manifest::METADATA_DIR, manifest::MANIFEST_FILE);
        if manifest_changes {
            let ids: Vec<String> = planner
                .target
                .sorted_repositories()
                .iter()
                .map(|repo| repo.id.clone())
                .collect();
            planner.actions.push(RepositoryAction {
                kind: RepositoryActionKind::UpdateManifest,
                target: "manifest".to_string(),
                path: manifest_rel.clone(),
                detail: format!(
                    "write {manifest_rel} ({}: {})",
                    count_label(planner.target.len(), "repository", "repositories"),
                    ids.join(", ")
                ),
                state: StepState::Planned,
            });
        } else {
            planner.actions.push(RepositoryAction {
                kind: RepositoryActionKind::UpdateManifest,
                target: "manifest".to_string(),
                path: manifest_rel.clone(),
                detail: format!("keep {manifest_rel}"),
                state: StepState::AlreadySatisfied(
                    "the manifest on disk already describes this configuration".to_string(),
                ),
            });
        }
    }

    // Verification is part of the plan, not an implicit afterthought: it is the step that
    // re-opens the project and checks every repository through the normal mechanisms.
    planner.actions.push(RepositoryAction {
        kind: RepositoryActionKind::VerifyProject,
        target: "project".to_string(),
        path: ".".to_string(),
        detail: "re-open the project and check every configured repository".to_string(),
        state: StepState::Planned,
    });

    // Remotes the plan guarantees are configured: what it just configured, plus what was
    // already configured and matches the manifest. A remote that is only recorded is not an
    // expectation for Git, and a repository the operation does not concern is never turned
    // into a problem by unrelated drift.
    let mut expected: Vec<String> = planner.expected_origins.clone();
    for repo in planner.target.sorted_repositories() {
        let Some(remote) = repo.remote_url.clone() else {
            continue;
        };
        let previous = planner
            .project
            .repository_for_relative(&repo.relative_path)
            .map(|previous| previous.remote_url.clone());
        if previous != Some(Some(remote.clone())) {
            continue;
        }
        let path = planner.target.repository_path(repo);
        if discovery::origin_url(&path, runner).ok().flatten() == Some(remote) {
            expected.push(repo.id.clone());
        }
    }
    expected.sort();
    expected.dedup();

    let safety = safety_statements(&planner);
    let mut plan = RepositoryPlan {
        id: String::new(),
        request: request.clone(),
        project: planner.project,
        target: planner.target,
        name: project.name.clone(),
        root,
        manifest_path,
        manifest_before,
        manifest_after: planner.manifest_after,
        manifest_changes,
        changes: planner.changes,
        actions: planner.actions,
        removals: planner.removals,
        blockers: planner.blockers,
        warnings: planner.warnings,
        notices: planner.notices,
        safety,
        expected_origins: expected,
    };
    plan.id = plan.fingerprint();
    Ok(plan)
}

/// Plan one intent into the accumulated plan.
fn plan_intent(planner: &mut Planner, intent: &RepositoryIntent, runner: &GitRunner) {
    match intent {
        RepositoryIntent::Add {
            path,
            id,
            remote,
            branch,
            initialize,
            configure_remote,
            untrack_from_root,
        } => plan_add(
            planner,
            path,
            id,
            remote.as_deref(),
            branch.as_deref(),
            *initialize,
            *configure_remote,
            *untrack_from_root,
            runner,
        ),
        RepositoryIntent::Clone {
            path,
            id,
            remote,
            branch,
        } => plan_clone(planner, path, id, remote, branch.as_deref(), runner),
        RepositoryIntent::Remove {
            id,
            confirm_takeover,
        } => plan_remove(planner, id, *confirm_takeover, runner),
        RepositoryIntent::Rename { id, new_id } => plan_rename(planner, id, new_id),
        RepositoryIntent::SetRemote {
            id,
            remote,
            configure,
        } => plan_set_remote(planner, id, remote.as_deref(), *configure, runner),
    }
}

/// Refuse one intent: the change is reported as blocked, with the reason.
fn refuse(
    planner: &mut Planner,
    kind: RepositoryChangeKind,
    id: &str,
    path: &str,
    detail: String,
    reason: String,
) {
    planner.blockers.push(reason.clone());
    planner.changes.push(RepositoryChange {
        kind,
        id: id.to_string(),
        path: path.to_string(),
        before: None,
        after: None,
        detail,
        state: StepState::Blocked(reason),
    });
}

#[allow(clippy::too_many_arguments)]
fn plan_add(
    planner: &mut Planner,
    raw_path: &str,
    id: &str,
    remote: Option<&str>,
    branch: Option<&str>,
    initialize: bool,
    configure_remote: bool,
    untrack_from_root: bool,
    runner: &GitRunner,
) {
    let label = if raw_path.trim().is_empty() {
        "(no directory given)".to_string()
    } else {
        raw_path.trim().to_string()
    };
    let relative = match paths::normalize_relative(raw_path.trim()) {
        Ok(relative) => relative,
        Err(e) => {
            refuse(
                planner,
                RepositoryChangeKind::AddRepository,
                id,
                &label,
                format!("add '{label}' as a repository"),
                format!("'{label}' is not a usable repository path: {e}"),
            );
            return;
        }
    };
    if paths::is_root_relative(&relative) {
        refuse(
            planner,
            RepositoryChangeKind::AddRepository,
            id,
            ".",
            "add the project root as an external repository".to_string(),
            "the project root is always the root repository".to_string(),
        );
        return;
    }

    let path_label = to_slash(&relative);
    let requested_id = id.trim().to_string();
    let id = if requested_id.is_empty() {
        discovery::suggest_id(&relative, &planner.target)
    } else {
        requested_id.clone()
    };

    let tracked_by_root =
        discovery::count_files_tracked_under(&planner.project.root, &relative, runner);

    // Adding a directory that is already configured is the definition of a repeated
    // operation: when the request asks for exactly what is there, the plan reports it and
    // does nothing else. When it asks for something else, the user is pointed at the
    // operation that is meant for it.
    if let Some(existing) = planner.target.repository_for_relative(&relative).cloned() {
        let same_path = paths::lexical_normalize(&existing.relative_path)
            == paths::lexical_normalize(&relative);
        if same_path && !existing.is_root() {
            let wanted_remote = clean_url(remote);
            let remote_matches = match &wanted_remote {
                // No remote asked for: whatever is recorded stays.
                None => true,
                Some(url) => existing.remote_url.as_deref() == Some(url.as_str()),
            };
            // An empty id means "whatever this directory is already called", so repeating
            // the same request cannot look like a rename.
            let id_matches = requested_id.is_empty() || id == existing.id;
            if !id_matches || !remote_matches {
                refuse(
                    planner,
                    RepositoryChangeKind::AddRepository,
                    &id,
                    &path_label,
                    format!("add '{path_label}' as repository '{id}'"),
                    format!(
                        "'{path_label}' already is the GitMesh repository '{}'; use \"rename\" to \
                         change its id, or \"change remote\" to change its remote",
                        existing.id
                    ),
                );
                return;
            }
            planner.notices.push(format!(
                "'{path_label}' already is the GitMesh repository '{}'; nothing is added or \
                 re-initialised",
                existing.id
            ));
            planner.changes.push(RepositoryChange {
                kind: RepositoryChangeKind::AlreadyManaged,
                id: existing.id.clone(),
                path: path_label.clone(),
                before: Some(existing.id.clone()),
                after: Some(existing.id.clone()),
                detail: format!("'{path_label}' already is the repository '{}'", existing.id),
                state: StepState::AlreadySatisfied(
                    "the directory is already a managed repository".to_string(),
                ),
            });
            // The only thing that can still be left to do is the ownership of files the
            // root repository tracks there.
            plan_ownership(
                planner,
                &existing.id,
                &path_label,
                untrack_from_root,
                tracked_by_root,
            );
            return;
        }
    }

    if let Some(problem) = id_problem(&id, &planner.target, None) {
        refuse(
            planner,
            RepositoryChangeKind::AddRepository,
            &id,
            &path_label,
            format!("add '{path_label}' as repository '{id}'"),
            problem,
        );
        return;
    }

    // Configuration-level conflicts, with the same rules the wizard and the CLI use.
    let conflicts = discovery::assignment_conflicts(&planner.target, &relative);
    if !conflicts.is_empty() {
        refuse(
            planner,
            RepositoryChangeKind::AddRepository,
            &id,
            &path_label,
            format!("add '{path_label}' as repository '{id}'"),
            conflicts[0].clone(),
        );
        return;
    }

    let absolute = planner.project.root.join(&relative);
    if !absolute.is_dir() {
        refuse(
            planner,
            RepositoryChangeKind::AddRepository,
            &id,
            &path_label,
            format!("add '{path_label}' as repository '{id}'"),
            format!(
                "the directory '{path_label}' does not exist in this project; to bring a remote in \
                 as a new directory, use configure clone (or the Clone action)"
            ),
        );
        return;
    }
    let is_repository = discovery::is_repository_root(&absolute, true, runner);
    if !is_repository && !initialize {
        refuse(
            planner,
            RepositoryChangeKind::AddRepository,
            &id,
            &path_label,
            format!("add '{path_label}' as repository '{id}'"),
            format!(
                "'{path_label}' is not a Git repository; initialize one there (`git init`), or \
                 adopt an existing repository"
            ),
        );
        return;
    }

    if let Some(url) = clean_url(remote) {
        match crate::git::probe_remote(runner, &url) {
            crate::git::RemoteProbe::Unreachable { reason, hint } => {
                planner.warnings.push(format!(
                    "cannot reach the remote {url}: {reason}. {hint}. It is recorded anyway; check \
                     it before the next sync"
                ));
            }
            crate::git::RemoteProbe::Reachable { branches, .. } => {
                if branches.is_empty() {
                    planner.notices.push(format!(
                        "the remote {url} is empty (it has no commits yet); pushing this repository \
                         will publish it"
                    ));
                } else if !is_repository && discovery::is_empty_directory(&absolute) {
                    refuse(
                        planner,
                        RepositoryChangeKind::AddRepository,
                        &id,
                        &path_label,
                        format!("add '{path_label}' as repository '{id}'"),
                        format!(
                            "'{path_label}' is an empty directory and the remote {url} already has \
                             history; clone it instead (configure clone), so the history is brought \
                             in rather than an unrelated repository being created"
                        ),
                    );
                    return;
                }
            }
        }
    }

    let has_commits = if is_repository {
        runner.repo(&absolute).head_oid().ok().flatten().is_some()
    } else {
        false
    };
    let origin = if is_repository {
        discovery::origin_url(&absolute, runner).unwrap_or(None)
    } else {
        None
    };
    for nested in discovery::scan_nested(&absolute, runner) {
        planner.warnings.push(format!(
            "'{path_label}' contains the Git repository '{}'; GitMesh does not manage nested \
             repositories and will not touch it",
            to_slash(&relative.join(nested))
        ));
    }

    // ---- the repository itself -------------------------------------------------
    if is_repository {
        let mut detail = if has_commits {
            format!(
                "adopt the existing Git repository in '{path_label}' as '{id}' (its history and \
                 remote are kept)"
            )
        } else {
            format!(
                "adopt the existing Git repository in '{path_label}' as '{id}' (it has no \
                 commits yet)"
            )
        };
        if initialize {
            planner.notices.push(format!(
                "'{path_label}' already is a Git repository; it is used as it is and never \
                 re-initialised"
            ));
        }
        if let Some(origin) = &origin {
            detail.push_str(&format!("; origin {origin} is already configured"));
        }
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::AdoptRepository,
            id: id.clone(),
            path: path_label.clone(),
            before: None,
            after: Some(path_label.clone()),
            detail,
            state: StepState::Planned,
        });
        planner.actions.push(RepositoryAction {
            kind: RepositoryActionKind::AdoptRepository,
            target: id.clone(),
            path: path_label.clone(),
            detail: format!("use the existing Git repository in '{path_label}'"),
            state: StepState::AlreadySatisfied(
                "a Git repository already exists here; it is never re-initialised".to_string(),
            ),
        });
    } else {
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::InitializeRepository,
            id: id.clone(),
            path: path_label.clone(),
            before: None,
            after: Some(path_label.clone()),
            detail: format!("create a Git repository in '{path_label}' (git init -b main)"),
            state: StepState::Planned,
        });
        planner.actions.push(RepositoryAction {
            kind: RepositoryActionKind::InitializeRepository,
            target: id.clone(),
            path: path_label.clone(),
            detail: format!("create a Git repository in '{path_label}' (git init -b main)"),
            state: StepState::Planned,
        });
    }
    planner.changes.push(RepositoryChange {
        kind: RepositoryChangeKind::AddRepository,
        id: id.clone(),
        path: path_label.clone(),
        before: None,
        after: Some(path_label.clone()),
        detail: format!("add '{path_label}' to GitMesh as repository '{id}'"),
        state: StepState::Planned,
    });

    // ---- the remote ------------------------------------------------------------
    let recorded = plan_remote_change(
        planner,
        &id,
        &path_label,
        clean_url(remote),
        origin.clone(),
        configure_remote,
    );
    plan_remote_history(
        planner,
        &id,
        &path_label,
        &absolute,
        has_commits,
        recorded.as_deref(),
        configure_remote,
        origin.as_deref(),
        clean_text(branch),
        runner,
    );

    // ---- ownership -------------------------------------------------------------
    plan_ownership(
        planner,
        &id,
        &path_label,
        untrack_from_root,
        tracked_by_root,
    );

    // ---- the configuration -----------------------------------------------------
    planner.target.repositories.push(PhysicalRepository {
        id: id.clone(),
        role: RepositoryRole::External,
        relative_path: relative,
        remote_url: recorded,
        branch: clean_text(branch),
        absolute_path: absolute,
    });
}

/// Plan a clone: a new repository created from a remote, in a directory that is missing or
/// empty. The remote is read first, and nothing that exists is overwritten.
fn plan_clone(
    planner: &mut Planner,
    raw_path: &str,
    id: &str,
    remote: &str,
    branch: Option<&str>,
    runner: &GitRunner,
) {
    let label = if raw_path.trim().is_empty() {
        "(no directory given)".to_string()
    } else {
        raw_path.trim().to_string()
    };
    let relative = match paths::normalize_relative(raw_path.trim()) {
        Ok(relative) => relative,
        Err(e) => {
            refuse(
                planner,
                RepositoryChangeKind::CloneRepository,
                id,
                &label,
                format!("clone into '{label}'"),
                format!("'{label}' is not a usable repository path: {e}"),
            );
            return;
        }
    };
    if paths::is_root_relative(&relative) {
        refuse(
            planner,
            RepositoryChangeKind::CloneRepository,
            id,
            ".",
            "clone into the project root".to_string(),
            "the project root is always the root repository; clone into a directory of its own"
                .to_string(),
        );
        return;
    }
    let path_label = to_slash(&relative);
    let requested_id = id.trim().to_string();
    let id = if requested_id.is_empty() {
        discovery::suggest_id(&relative, &planner.target)
    } else {
        requested_id
    };
    let what = format!("clone into '{path_label}' as repository '{id}'");

    // An exact match only: the root repository contains every path, so a containment lookup
    // would always find it.
    let already = planner
        .target
        .repositories
        .iter()
        .find(|repo| {
            !repo.is_root()
                && paths::lexical_normalize(&repo.relative_path)
                    == paths::lexical_normalize(&relative)
        })
        .cloned();
    if let Some(existing) = already {
        refuse(
            planner,
            RepositoryChangeKind::CloneRepository,
            &id,
            &path_label,
            what,
            format!(
                "'{path_label}' is already the GitMesh repository '{}'; nothing was cloned",
                existing.id
            ),
        );
        return;
    }

    let url = match clean_url(Some(remote)) {
        Some(url) => url,
        None => {
            refuse(
                planner,
                RepositoryChangeKind::CloneRepository,
                &id,
                &path_label,
                what,
                "a clone needs the URL or path of a remote repository".to_string(),
            );
            return;
        }
    };
    if let Some(problem) = id_problem(&id, &planner.target, None) {
        refuse(
            planner,
            RepositoryChangeKind::CloneRepository,
            &id,
            &path_label,
            what,
            problem,
        );
        return;
    }
    let conflicts = discovery::assignment_conflicts(&planner.target, &relative);
    if !conflicts.is_empty() {
        refuse(
            planner,
            RepositoryChangeKind::CloneRepository,
            &id,
            &path_label,
            what,
            conflicts[0].clone(),
        );
        return;
    }

    let absolute = planner.project.root.join(&relative);
    if absolute.exists() {
        let reason = if !absolute.is_dir() {
            Some(format!("'{path_label}' exists and is not a directory"))
        } else if discovery::is_repository_root(&absolute, true, runner) {
            Some(format!(
                "'{path_label}' is already a Git repository; use add to adopt it, so its history \
                 is kept as it is"
            ))
        } else if !discovery::is_empty_directory(&absolute) {
            Some(format!(
                "'{path_label}' is not empty; clone needs a missing or empty directory, so the \
                 files there are left alone"
            ))
        } else {
            None
        };
        if let Some(reason) = reason {
            refuse(
                planner,
                RepositoryChangeKind::CloneRepository,
                &id,
                &path_label,
                what,
                reason,
            );
            return;
        }
    }

    // The remote is read before anything is planned, so an unreachable remote is refused
    // here and nothing is created.
    match crate::git::probe_remote(runner, &url) {
        crate::git::RemoteProbe::Unreachable { reason, hint } => {
            refuse(
                planner,
                RepositoryChangeKind::CloneRepository,
                &id,
                &path_label,
                what,
                format!("the remote could not be read: {reason}. {hint}"),
            );
            return;
        }
        crate::git::RemoteProbe::Reachable { branches, .. } => {
            if branches.is_empty() {
                planner.notices.push(format!(
                    "the remote {url} is empty (it has no commits yet); the clone will have no \
                     commits until the first push"
                ));
            }
        }
    }

    planner.changes.push(RepositoryChange {
        kind: RepositoryChangeKind::CloneRepository,
        id: id.clone(),
        path: path_label.clone(),
        before: None,
        after: Some(path_label.clone()),
        detail: format!("clone {url} into '{path_label}' and track its default branch"),
        state: StepState::Planned,
    });
    planner.actions.push(RepositoryAction {
        kind: RepositoryActionKind::CloneRepository,
        target: id.clone(),
        path: path_label.clone(),
        detail: format!("git clone {url} '{path_label}'"),
        state: StepState::Planned,
    });
    planner.changes.push(RepositoryChange {
        kind: RepositoryChangeKind::AddRepository,
        id: id.clone(),
        path: path_label.clone(),
        before: None,
        after: Some(path_label.clone()),
        detail: format!("add '{path_label}' to GitMesh as repository '{id}'"),
        state: StepState::Planned,
    });
    planner.target.repositories.push(PhysicalRepository {
        id: id.clone(),
        role: RepositoryRole::External,
        relative_path: relative,
        remote_url: Some(url),
        branch: clean_text(branch),
        absolute_path: absolute,
    });
}

/// Plan what happens to the files the root repository tracks inside a directory that is
/// becoming (or has just become) a repository of its own.
fn plan_ownership(
    planner: &mut Planner,
    id: &str,
    path_label: &str,
    untrack_from_root: bool,
    tracked: usize,
) {
    if tracked == 0 {
        return;
    }
    if untrack_from_root {
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::UntrackFromRoot,
            id: id.to_string(),
            path: path_label.to_string(),
            before: Some(tracked.to_string()),
            after: Some("0".to_string()),
            detail: format!(
                "stop tracking {tracked} file(s) of '{path_label}' in the root repository (the \
                 files stay on disk)"
            ),
            state: StepState::Planned,
        });
        planner.actions.push(RepositoryAction {
            kind: RepositoryActionKind::UntrackFromRoot,
            target: id.to_string(),
            path: path_label.to_string(),
            detail: format!(
                "stop tracking {tracked} file(s) of '{path_label}' in the root repository (the \
                 files stay on disk)"
            ),
            state: StepState::Planned,
        });
    } else {
        planner.warnings.push(format!(
            "the root repository still tracks {tracked} file(s) inside '{path_label}'; the same \
             files would then belong to two repositories (enable \"stop tracking in the root \
             repository\" to fix that without deleting anything)"
        ));
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::UntrackFromRoot,
            id: id.to_string(),
            path: path_label.to_string(),
            before: Some(tracked.to_string()),
            after: Some(tracked.to_string()),
            detail: format!(
                "{tracked} file(s) of '{path_label}' stay tracked by the root repository, so two \
                 repositories own them"
            ),
            state: StepState::AlreadySatisfied(
                "the root repository keeps tracking these files".to_string(),
            ),
        });
    }
}

/// Plan the remote part of an add or a set-remote.
///
/// Returns the URL the manifest should record, and (through `planner`) the change the user
/// reviews and the action that runs, if any.
fn plan_remote_change(
    planner: &mut Planner,
    id: &str,
    path_label: &str,
    wanted: Option<String>,
    current: Option<String>,
    configure: bool,
) -> Option<String> {
    // What the manifest records, the change row and the action, decided in one place so the
    // review and the execution can never disagree.
    let (recorded, kind, action, detail, state) = match (&wanted, &current, configure) {
        // A local-only repository is a valid configuration, never an error.
        (None, None, _) => (
            None,
            RepositoryChangeKind::AlreadyManaged,
            None,
            format!("'{id}' is managed locally; no remote is configured or recorded"),
            StepState::AlreadySatisfied("the repository stays local-only".to_string()),
        ),
        // Git already has one and the user did not ask for a change: keep it, record it.
        (None, Some(origin), _) => (
            Some(origin.clone()),
            RepositoryChangeKind::KeepExistingRemote,
            None,
            format!("keep origin {origin} and record it in the manifest"),
            StepState::AlreadySatisfied("origin already points at this URL".to_string()),
        ),
        (Some(url), None, true) => (
            Some(url.clone()),
            RepositoryChangeKind::ConfigureRemote,
            Some(RepositoryActionKind::ConfigureRemote),
            format!("add origin {url} to '{path_label}'"),
            StepState::Planned,
        ),
        (Some(url), None, false) => (
            Some(url.clone()),
            RepositoryChangeKind::RecordRemote,
            None,
            format!("record {url} in the manifest only, without touching Git"),
            StepState::Planned,
        ),
        (Some(url), Some(origin), _) if url == origin => (
            Some(url.clone()),
            RepositoryChangeKind::KeepExistingRemote,
            None,
            format!("keep origin {url}"),
            StepState::AlreadySatisfied("origin already points at this URL".to_string()),
        ),
        (Some(url), Some(origin), true) => (
            Some(url.clone()),
            RepositoryChangeKind::UpdateRemote,
            Some(RepositoryActionKind::UpdateRemote),
            format!("replace origin {origin} with {url} in '{path_label}' (explicitly confirmed)"),
            StepState::Planned,
        ),
        // A recorded remote that differs from `origin` on disk is refused rather than
        // silently recorded: the manifest would then promise one remote and pushes would use
        // another.
        (Some(url), Some(origin), false) => {
            planner.blockers.push(format!(
                "repository '{id}' already has origin {origin} but '{url}' is requested without \
                 configuring Git; enable remote configuration, or record the URL origin already \
                 has"
            ));
            (
                None,
                RepositoryChangeKind::UpdateRemote,
                None,
                format!("record {url} without configuring Git (origin is {origin})"),
                StepState::Blocked("replacing a remote requires explicit intent".to_string()),
            )
        }
    };

    if kind != RepositoryChangeKind::AlreadyManaged {
        planner.changes.push(RepositoryChange {
            kind,
            id: id.to_string(),
            path: path_label.to_string(),
            before: current.clone(),
            after: recorded.clone(),
            detail,
            state,
        });
    }
    if let Some(kind) = action {
        planner.actions.push(RepositoryAction {
            kind,
            target: id.to_string(),
            path: path_label.to_string(),
            detail: format!(
                "{} origin for '{path_label}'",
                if kind == RepositoryActionKind::UpdateRemote {
                    "replace the"
                } else {
                    "add an"
                }
            ),
            state: StepState::Planned,
        });
        planner.expected_origins.push(id.to_string());
    }
    recorded
}

fn plan_remove(planner: &mut Planner, id: &str, confirm_takeover: bool, runner: &GitRunner) {
    let Some(repo) = planner.target.repository(id).cloned() else {
        // Repeating a removal is safe: the second time there is simply nothing to remove.
        // (The command line keeps its own stricter check for scripts, so a typo there is
        // still an error.)
        planner.notices.push(format!(
            "no repository with the id '{id}' is configured; there is nothing to remove"
        ));
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::AlreadyManaged,
            id: id.to_string(),
            path: ".".to_string(),
            before: Some(id.to_string()),
            after: None,
            detail: format!("no repository '{id}' is configured"),
            state: StepState::AlreadySatisfied(
                "no repository with this id is configured".to_string(),
            ),
        });
        return;
    };
    if repo.is_root() {
        refuse(
            planner,
            RepositoryChangeKind::RemoveRepositoryFromManifest,
            id,
            ".",
            format!("remove repository '{id}' from GitMesh"),
            "the root repository cannot be removed: it owns the project root".to_string(),
        );
        return;
    }

    let path_label = to_slash(&repo.relative_path);
    let absolute = planner.target.repository_path(&repo);
    let is_repository = absolute.is_dir() && discovery::is_repository_root(&absolute, true, runner);
    let head = if is_repository {
        runner.repo(&absolute).head_oid().ok().flatten()
    } else {
        None
    };
    let origin = if is_repository {
        discovery::origin_url(&absolute, runner).unwrap_or(None)
    } else {
        None
    };
    let tracked =
        discovery::count_files_tracked_under(&planner.project.root, &repo.relative_path, runner);

    planner.notices.push(format!(
        "'{id}' leaves the GitMesh configuration only: its directory, its .git, its history and \
         its remote are not touched"
    ));
    if tracked > 0 && !confirm_takeover {
        planner.blockers.push(format!(
            "the root repository tracks {tracked} file(s) inside '{path_label}'; removing '{id}' \
             hands those files back to the root repository, which has to be confirmed"
        ));
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::RemoveRepositoryFromManifest,
            id: id.to_string(),
            path: path_label.clone(),
            before: Some(id.to_string()),
            after: None,
            detail: format!("remove '{id}' from GitMesh (its files and history are kept)"),
            state: StepState::Blocked(format!(
                "the root repository would take back {tracked} file(s); confirm that consequence"
            )),
        });
        return;
    }
    if tracked > 0 {
        planner.warnings.push(format!(
            "after the removal the root repository owns the {tracked} file(s) it tracks inside \
             '{path_label}'"
        ));
    } else if is_repository {
        planner.notices.push(format!(
            "'{path_label}' falls back under the root repository, where its files are not \
             tracked: they will appear as changes in the root repository"
        ));
    }

    planner.changes.push(RepositoryChange {
        kind: RepositoryChangeKind::RemoveRepositoryFromManifest,
        id: id.to_string(),
        path: path_label.clone(),
        before: Some(id.to_string()),
        after: None,
        detail: format!(
            "remove '{id}' at '{path_label}' from GitMesh (the directory, its .git, its history \
             and its remote are kept)"
        ),
        state: StepState::Planned,
    });
    planner.removals.push(PlannedRemoval {
        id: id.to_string(),
        path: path_label,
        is_repository,
        head,
        origin,
        tracked_by_root: tracked,
    });
    planner.expected_origins.retain(|expected| expected != id);
    planner
        .target
        .repositories
        .retain(|candidate| candidate.id != id);
}

fn plan_rename(planner: &mut Planner, id: &str, new_id: &str) {
    let Some(existing) = planner.target.repository(id).cloned() else {
        refuse(
            planner,
            RepositoryChangeKind::RenameRepository,
            id,
            ".",
            format!("rename repository '{id}'"),
            format!("no repository with the id '{id}' is configured"),
        );
        return;
    };
    let path_label = if existing.is_root() {
        ".".to_string()
    } else {
        to_slash(&existing.relative_path)
    };
    let new_id = new_id.trim().to_string();
    if new_id == id {
        planner.changes.push(RepositoryChange {
            kind: RepositoryChangeKind::AlreadyManaged,
            id: id.to_string(),
            path: path_label,
            before: Some(id.to_string()),
            after: Some(new_id),
            detail: format!("'{id}' already has this id"),
            state: StepState::AlreadySatisfied("the repository already has this id".to_string()),
        });
        return;
    }
    if let Some(problem) = id_problem(&new_id, &planner.target, Some(id)) {
        refuse(
            planner,
            RepositoryChangeKind::RenameRepository,
            id,
            &path_label,
            format!("rename repository '{id}' to '{new_id}'"),
            problem,
        );
        return;
    }

    if let Some(repo) = planner
        .target
        .repositories
        .iter_mut()
        .find(|repo| repo.id == id)
    {
        repo.id = new_id.clone();
    }
    planner.changes.push(RepositoryChange {
        kind: RepositoryChangeKind::RenameRepository,
        id: new_id.clone(),
        path: path_label.clone(),
        before: Some(id.to_string()),
        after: Some(new_id.clone()),
        detail: format!(
            "rename repository '{id}' to '{new_id}' (the directory '{path_label}' is not renamed)"
        ),
        state: StepState::Planned,
    });
    planner.expected_origins = planner
        .expected_origins
        .iter()
        .map(|expected| {
            if expected == id {
                new_id.clone()
            } else {
                expected.clone()
            }
        })
        .collect();
}

fn plan_set_remote(
    planner: &mut Planner,
    id: &str,
    remote: Option<&str>,
    configure: bool,
    runner: &GitRunner,
) {
    let Some(repo) = planner.target.repository(id).cloned() else {
        refuse(
            planner,
            RepositoryChangeKind::ConfigureRemote,
            id,
            ".",
            format!("change the remote of '{id}'"),
            format!("no repository with the id '{id}' is configured"),
        );
        return;
    };
    let path_label = if repo.is_root() {
        ".".to_string()
    } else {
        to_slash(&repo.relative_path)
    };
    let absolute = planner.target.repository_path(&repo);
    let is_repository = absolute.is_dir() && discovery::is_repository_root(&absolute, true, runner);
    let origin = if is_repository {
        discovery::origin_url(&absolute, runner).unwrap_or(None)
    } else {
        None
    };
    let wanted = clean_url(remote);

    // Clearing the recorded remote is a manifest-only change: GitMesh never removes a Git
    // remote, and says so.
    if wanted.is_none() {
        match repo.remote_url.clone() {
            None => planner.changes.push(RepositoryChange {
                kind: RepositoryChangeKind::AlreadyManaged,
                id: id.to_string(),
                path: path_label,
                before: None,
                after: None,
                detail: format!("no remote is recorded for '{id}'"),
                state: StepState::AlreadySatisfied("there is no recorded remote".to_string()),
            }),
            Some(previous) => {
                if let Some(origin) = &origin {
                    planner.warnings.push(format!(
                        "origin {origin} stays configured in Git after the recorded remote is \
                         cleared; remove it yourself if you want it gone (`git remote remove \
                         origin`)"
                    ));
                }
                planner.changes.push(RepositoryChange {
                    kind: RepositoryChangeKind::ClearRemote,
                    id: id.to_string(),
                    path: path_label,
                    before: Some(previous.clone()),
                    after: None,
                    detail: format!(
                        "stop recording the remote {previous} for '{id}' (Git is left alone)"
                    ),
                    state: StepState::Planned,
                });
                if let Some(target) = planner
                    .target
                    .repositories
                    .iter_mut()
                    .find(|candidate| candidate.id == id)
                {
                    target.remote_url = None;
                }
            }
        }
        return;
    }

    // Configuring Git needs a repository; a record-only remote does not.
    let configure = configure && is_repository;
    if configure && !repo.is_root() && !is_repository {
        planner.notices.push(format!(
            "'{path_label}' is not a Git repository yet, so origin cannot be configured there; \
             the URL is recorded in the manifest instead"
        ));
    }

    let has_commits = is_repository && runner.repo(&absolute).head_oid().ok().flatten().is_some();
    let recorded = plan_remote_change(planner, id, &path_label, wanted, origin.clone(), configure);
    plan_remote_history(
        planner,
        id,
        &path_label,
        &absolute,
        has_commits,
        recorded.as_deref(),
        configure,
        origin.as_deref(),
        repo.branch.clone(),
        runner,
    );
    if let Some(target) = planner
        .target
        .repositories
        .iter_mut()
        .find(|candidate| candidate.id == id)
    {
        target.remote_url = recorded;
    }
}

/// Compare a repository with the history its remote already has, and plan the adoption when the
/// repository has no commits yet.
///
/// `recorded` is the remote the plan records. Git is only checked against it when `origin` will
/// really be that URL: configured by this plan, or already there. A remote that is only recorded
/// is not compared, because nothing would be pushed to it. `newly_configured` is true when this
/// plan adds or replaces `origin`; that is what makes a conflict block the plan (see
/// [`history::findings`]).
#[allow(clippy::too_many_arguments)]
fn plan_remote_history(
    planner: &mut Planner,
    id: &str,
    path_label: &str,
    absolute: &Path,
    has_commits: bool,
    recorded: Option<&str>,
    configure: bool,
    origin: Option<&str>,
    wanted_branch: Option<String>,
    runner: &GitRunner,
) {
    let Some(url) = recorded else {
        return;
    };
    let git_origin_will_be = configure || origin == Some(url);
    if !git_origin_will_be {
        return;
    }
    let newly_configured = origin != Some(url);
    let dir = has_commits.then_some(absolute);
    let check =
        history::check_remote_history(runner, dir, has_commits, url, wanted_branch.as_deref());
    let label = if path_label == "." {
        "the project root".to_string()
    } else {
        format!("'{path_label}'")
    };
    let found = history::findings(&check, &label, url, newly_configured);
    planner.blockers.extend(found.blockers);
    planner.warnings.extend(found.warnings);
    planner.notices.extend(found.notices);
    if let Some(branch) = found.adopt_branch {
        planner.actions.push(RepositoryAction {
            kind: RepositoryActionKind::AdoptRemoteHistory,
            target: id.to_string(),
            path: path_label.to_string(),
            detail: format!(
                "check out origin's '{branch}' branch as the history of '{path_label}'; local files \
                 are kept and Git refuses to overwrite one"
            ),
            state: StepState::Planned,
        });
    }
}

/// Why an id cannot be used, if it cannot.
fn id_problem(id: &str, project: &GitMeshProject, renaming_from: Option<&str>) -> Option<String> {
    let id = id.trim();
    if id.is_empty() {
        return Some("a repository id must not be empty".to_string());
    }
    if id.len() > MAX_ID_LEN {
        return Some(format!(
            "the id '{id}' is longer than {MAX_ID_LEN} characters"
        ));
    }
    if id == "." || id == ".." {
        return Some(format!("'{id}' is not a usable repository id"));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Some(format!(
            "the id '{id}' contains unsupported characters (use letters, digits, '-', '_' or '.')"
        ));
    }
    if let Some(existing) = project.repository(id) {
        let renaming_itself = Some(id) == renaming_from;
        if !renaming_itself {
            return Some(format!(
                "the id '{id}' is already used by the repository '{}'",
                existing.id
            ));
        }
    }
    None
}

/// Whether two projects describe the same repositories, ignoring ordering.
fn same_configuration(a: &GitMeshProject, b: &GitMeshProject) -> bool {
    if a.name != b.name {
        return false;
    }
    let fingerprint = |project: &GitMeshProject| {
        let mut rows: Vec<(String, String, Option<String>, Option<String>)> = project
            .repositories
            .iter()
            .map(|repo| {
                (
                    repo.id.clone(),
                    repo.relative_slash(),
                    repo.remote_url.clone(),
                    repo.branch.clone(),
                )
            })
            .collect();
        rows.sort();
        rows
    };
    fingerprint(a) == fingerprint(b)
}

/// What the plan guarantees, in the words the review screen shows.
fn safety_statements(planner: &Planner) -> Vec<String> {
    let mut lines = vec![
        "no existing .git directory is deleted or re-initialised".to_string(),
        "no file is moved, renamed or deleted".to_string(),
    ];
    let removals = planner
        .changes
        .iter()
        .filter(|change| {
            change.kind == RepositoryChangeKind::RemoveRepositoryFromManifest && change.is_planned()
        })
        .count();
    if removals > 0 {
        lines.push(
            "removing a repository from GitMesh changes the configuration only: its directory, \
             its .git, its history and its remote stay exactly where they are"
                .to_string(),
        );
    }
    let updates: Vec<&str> = planner
        .changes
        .iter()
        .filter(|change| change.kind == RepositoryChangeKind::UpdateRemote && change.is_planned())
        .map(|change| change.id.as_str())
        .collect();
    if updates.is_empty() {
        lines.push("no existing remote is modified".to_string());
    } else {
        lines.push(format!(
            "the origin of {} is replaced, and only because it was explicitly asked for",
            updates.join(", ")
        ));
    }
    let recorded: Vec<&str> = planner
        .changes
        .iter()
        .filter(|change| change.kind == RepositoryChangeKind::RecordRemote && change.is_planned())
        .map(|change| change.id.as_str())
        .collect();
    if !recorded.is_empty() {
        lines.push(format!(
            "the remote of {} is written to the manifest only: no repository's Git configuration \
             is touched",
            recorded.join(", ")
        ));
    }
    if planner
        .changes
        .iter()
        .any(|change| change.kind == RepositoryChangeKind::UntrackFromRoot && change.is_planned())
    {
        lines.push(
            "files untracked from the root repository stay on disk and in the external \
             repository; only the root repository's index changes"
                .to_string(),
        );
    }
    let concerned: Vec<&str> = planner
        .changes
        .iter()
        .filter(|change| change.is_planned())
        .map(|change| change.id.as_str())
        .collect();
    // Counted against the project as it is *now*: a repository that is being added is not
    // in it yet, and one that is being removed is still in it.
    let untouched = planner
        .project
        .sorted_repositories()
        .iter()
        .filter(|repo| !concerned.contains(&repo.id.as_str()))
        .count();
    if untouched > 0 {
        lines.push(format!(
            "the {} this operation does not concern are left exactly as they are",
            count_label(untouched, "repository", "repositories")
        ));
    }
    if planner.manifest_before.as_deref() != Some(planner.manifest_after.as_str()) {
        lines.push(
            "the manifest is rewritten from the configuration, with the same schema the rest of \
             GitMesh reads"
                .to_string(),
        );
    } else {
        lines.push("the manifest on disk already matches; it is not rewritten".to_string());
    }
    lines
}

// ---------------------------------------------------------------- execution --

/// Outcome of one action after execution.
#[derive(Debug, Clone)]
pub struct RepositoryActionOutcome {
    /// Kind of the action.
    pub kind: RepositoryActionKind,
    /// Repository id, or `manifest` / `project`.
    pub target: String,
    /// Project-relative path.
    pub path: String,
    /// Success, skipped (nothing to do) or failed.
    pub outcome: OutcomeKind,
    /// One-line explanation.
    pub summary: String,
    /// Extra lines: Git's own message, what the user has to do next.
    pub details: Vec<String>,
}

impl RepositoryActionOutcome {
    fn new(action: &RepositoryAction, outcome: OutcomeKind, summary: impl Into<String>) -> Self {
        RepositoryActionOutcome {
            kind: action.kind,
            target: action.target.clone(),
            path: action.path.clone(),
            outcome,
            summary: summary.into(),
            details: Vec::new(),
        }
    }

    fn with_details<I, S>(mut self, details: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.details.extend(details.into_iter().map(Into::into));
        self
    }

    /// Row identifier, matching [`RepositoryAction::row_id`].
    pub fn row_id(&self) -> String {
        format!("{}:{}", self.kind.label(), self.target)
    }

    /// Symbol for the summary line (the same alphabet every GitMesh operation uses).
    pub fn symbol(&self) -> &'static str {
        self.outcome.symbol()
    }

    /// One summary line.
    pub fn line(&self) -> String {
        format!("{} {:<22} {}", self.symbol(), self.target, self.summary)
    }
}

/// What really happened to one planned change, checked after the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeOutcome {
    /// The change is in place: verified against the manifest and the filesystem.
    Applied,
    /// There was nothing to do.
    AlreadyInPlace,
    /// The change did not happen.
    NotApplied,
    /// A dry run: the change would be applied.
    Planned,
}

impl ChangeOutcome {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            ChangeOutcome::Applied => "applied",
            ChangeOutcome::AlreadyInPlace => "already-in-place",
            ChangeOutcome::NotApplied => "not-applied",
            ChangeOutcome::Planned => "planned",
        }
    }

    /// Symbol used in the result panel.
    pub fn symbol(self) -> &'static str {
        match self {
            ChangeOutcome::Applied => "✓",
            ChangeOutcome::AlreadyInPlace => "–",
            ChangeOutcome::NotApplied => "✗",
            ChangeOutcome::Planned => "…",
        }
    }
}

/// One change after the run, with the evidence that it really happened.
#[derive(Debug, Clone)]
pub struct RepositoryChangeOutcome {
    /// The change that was planned.
    pub change: RepositoryChange,
    /// What happened to it.
    pub outcome: ChangeOutcome,
    /// What was checked to decide: the manifest entry, the directory, `.git`, the remote.
    pub evidence: Vec<String>,
}

/// Result of applying a plan.
#[derive(Debug, Clone)]
pub struct RepositoryManagementResult {
    /// Fingerprint of the executed plan.
    pub plan_id: String,
    /// True when nothing was changed.
    pub dry_run: bool,
    /// Complete, partial or failed (the vocabulary the setup service uses).
    pub kind: setup::SetupKind,
    /// One outcome per action, in execution order.
    pub actions: Vec<RepositoryActionOutcome>,
    /// One outcome per change, with its evidence.
    pub changes: Vec<RepositoryChangeOutcome>,
    /// The project as it is on disk after the run.
    pub project: Option<GitMeshProject>,
    /// Manifest that was written (absent when nothing was written).
    pub manifest_path: Option<PathBuf>,
    /// Validation of the resulting project.
    pub validation: Option<ValidationReport>,
    /// Why the plan was refused, when it was.
    pub refused: Vec<String>,
    /// What the plan warned about, carried into the result.
    pub warnings: Vec<String>,
}

impl RepositoryManagementResult {
    /// True when nothing failed and every change is in place.
    pub fn is_success(&self) -> bool {
        self.kind == setup::SetupKind::Complete
    }

    /// Exit code, following the GitMesh contract (0 = ok, 1 = something failed).
    pub fn exit_code(&self) -> u8 {
        match self.kind {
            setup::SetupKind::Complete => 0,
            setup::SetupKind::Partial | setup::SetupKind::Failed => 1,
        }
    }

    /// Number of actions that succeeded.
    pub fn succeeded(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| action.outcome == OutcomeKind::Success)
            .count()
    }

    /// Number of actions that had nothing to do.
    pub fn skipped(&self) -> usize {
        self.actions
            .iter()
            .filter(|action| action.outcome == OutcomeKind::Skipped)
            .count()
    }

    /// Actions that failed.
    pub fn failures(&self) -> impl Iterator<Item = &RepositoryActionOutcome> {
        self.actions
            .iter()
            .filter(|action| action.outcome == OutcomeKind::Failed)
    }

    /// Changes that are in place after the run.
    pub fn applied(&self) -> impl Iterator<Item = &RepositoryChangeOutcome> {
        self.changes
            .iter()
            .filter(|change| change.outcome == ChangeOutcome::Applied)
    }

    /// Changes that did not happen.
    pub fn not_applied(&self) -> impl Iterator<Item = &RepositoryChangeOutcome> {
        self.changes
            .iter()
            .filter(|change| change.outcome == ChangeOutcome::NotApplied)
    }

    /// One sentence for the result panel.
    pub fn sentence(&self) -> String {
        match self.kind {
            setup::SetupKind::Complete if self.dry_run => {
                "Dry run: nothing was changed".to_string()
            }
            setup::SetupKind::Complete if self.applied().count() == 0 => {
                "Nothing to do: the project is already configured as requested".to_string()
            }
            setup::SetupKind::Complete => format!(
                "{} applied, nothing failed",
                count_label(self.applied().count(), "change", "changes")
            ),
            setup::SetupKind::Partial => format!(
                "{} applied, {} failed",
                count_label(self.applied().count(), "change", "changes"),
                count_label(self.failures().count(), "step", "steps")
            ),
            setup::SetupKind::Failed => {
                if !self.refused.is_empty() {
                    "The plan was refused; nothing was changed".to_string()
                } else {
                    "Nothing could be changed".to_string()
                }
            }
        }
    }
}

/// Progress seam: the same shape as the setup observer, so a front end reports both flows
/// the same way without knowing about each other.
#[derive(Default)]
pub struct RepositoryObserver<'a> {
    on_start: Option<&'a mut dyn FnMut(&RepositoryAction)>,
    on_end: Option<&'a mut dyn FnMut(&RepositoryActionOutcome)>,
}

impl<'a> RepositoryObserver<'a> {
    /// An observer that does nothing.
    pub fn silent() -> Self {
        RepositoryObserver::default()
    }

    /// Called before an action runs.
    pub fn on_action<F>(mut self, f: &'a mut F) -> Self
    where
        F: FnMut(&RepositoryAction),
    {
        self.on_start = Some(f);
        self
    }

    /// Called when an action has its outcome.
    pub fn on_outcome<F>(mut self, f: &'a mut F) -> Self
    where
        F: FnMut(&RepositoryActionOutcome),
    {
        self.on_end = Some(f);
        self
    }

    /// Notify the observer that an action is starting.
    pub fn action_started(&mut self, action: &RepositoryAction) {
        if let Some(f) = self.on_start.as_mut() {
            f(action);
        }
    }

    /// Notify the observer that an action finished.
    pub fn action_finished(&mut self, outcome: &RepositoryActionOutcome) {
        if let Some(f) = self.on_end.as_mut() {
            f(outcome);
        }
    }
}

/// Execute a plan.
///
/// * a plan with blockers is refused as a whole: nothing runs, and every blocker is
///   returned in [`RepositoryManagementResult::refused`];
/// * a plan whose configuration no longer matches the project on disk is refused too — the
///   review and the execution are the same plan, or nothing happens;
/// * an action that is already satisfied is reported as skipped and runs nothing;
/// * an action that fails does not stop the others (they concern different repositories,
///   and the manifest is written for whatever the configuration ends up being);
/// * every change is **verified afterwards** against the manifest, the directory, `.git` and
///   the remote, so the result states what is true, not what was attempted.
pub fn apply(
    plan: &RepositoryPlan,
    dry_run: bool,
    runner: &GitRunner,
    observer: &mut RepositoryObserver<'_>,
) -> RepositoryManagementResult {
    let mut outcomes: Vec<RepositoryActionOutcome> = Vec::new();
    let mut manifest_written: Option<PathBuf> = None;

    if !plan.is_ready() {
        return RepositoryManagementResult {
            plan_id: plan.id.clone(),
            dry_run,
            kind: setup::SetupKind::Failed,
            actions: Vec::new(),
            changes: plan
                .changes
                .iter()
                .map(|change| RepositoryChangeOutcome {
                    change: change.clone(),
                    outcome: ChangeOutcome::NotApplied,
                    evidence: Vec::new(),
                })
                .collect(),
            project: manifest::load_from_root(&plan.root).ok(),
            manifest_path: None,
            validation: None,
            refused: plan.blockers.clone(),
            warnings: plan.warnings.clone(),
        };
    }

    // The plan is bound to the configuration it was made against. If the manifest moved on
    // — another GitMesh process, a hand edit, a checkout — the plan is not executed.
    //
    // The one move that is expected is the one this very plan makes: when the manifest on
    // disk is already exactly what the plan produces, applying it again is the repeated
    // operation, and every action then reports itself as already in place.
    let on_disk = std::fs::read_to_string(&plan.manifest_path).ok();
    let already_applied = on_disk.as_deref() == Some(plan.manifest_after.as_str());
    if !already_applied && on_disk != plan.manifest_before {
        return RepositoryManagementResult {
            plan_id: plan.id.clone(),
            dry_run,
            kind: setup::SetupKind::Failed,
            actions: Vec::new(),
            changes: Vec::new(),
            project: manifest::load_from_root(&plan.root).ok(),
            manifest_path: None,
            validation: None,
            refused: vec![
                "the configuration changed since the plan was reviewed; plan the change again"
                    .to_string(),
            ],
            warnings: plan.warnings.clone(),
        };
    }

    for action in &plan.actions {
        observer.action_started(action);
        let outcome = match &action.state {
            StepState::AlreadySatisfied(reason) => {
                RepositoryActionOutcome::new(action, OutcomeKind::Skipped, reason.clone())
            }
            StepState::Blocked(reason) => {
                RepositoryActionOutcome::new(action, OutcomeKind::Failed, reason.clone())
            }
            // A failed clone means the repository it would record does not exist. The
            // manifest is then left as it was, rather than listing a repository that is not there.
            StepState::Planned
                if action.kind == RepositoryActionKind::UpdateManifest
                    && !dry_run
                    && outcomes.iter().any(|o| {
                        o.kind == RepositoryActionKind::CloneRepository
                            && o.outcome == OutcomeKind::Failed
                    }) =>
            {
                RepositoryActionOutcome::new(
                    action,
                    OutcomeKind::Skipped,
                    "not written, because a clone failed and must not be recorded",
                )
            }
            StepState::Planned if dry_run => RepositoryActionOutcome::new(
                action,
                OutcomeKind::Skipped,
                format!("dry run: {}", action.detail),
            ),
            StepState::Planned => run_action(plan, action, runner),
        };
        if action.kind == RepositoryActionKind::UpdateManifest
            && outcome.outcome == OutcomeKind::Success
        {
            manifest_written = Some(plan.manifest_path.clone());
        }
        observer.action_finished(&outcome);
        outcomes.push(outcome);
    }

    let project = manifest::load_from_root(&plan.root).ok();
    let validation = if project.is_some() && !dry_run {
        Some(setup::verify_expecting(
            &plan.root,
            runner,
            &plan.expected_origins,
        ))
    } else {
        None
    };

    // Every change is checked against what is really there now.
    // Which unit of work made each change real, so a replay that skipped everything is
    // reported as "already in place" instead of "applied".
    let skipped_targets: Vec<String> = outcomes
        .iter()
        .filter(|outcome| {
            outcome.outcome == OutcomeKind::Skipped && outcome.kind.changes_anything()
        })
        .map(|outcome| outcome.target.clone())
        .collect();
    let manifest_was_written = outcomes.iter().any(|outcome| {
        outcome.kind == RepositoryActionKind::UpdateManifest
            && outcome.outcome == OutcomeKind::Success
    });
    let changes: Vec<RepositoryChangeOutcome> = plan
        .changes
        .iter()
        .map(|change| {
            verify_change(
                plan,
                change,
                dry_run,
                project.as_ref(),
                runner,
                &skipped_targets,
                manifest_was_written,
            )
        })
        .collect();

    let failed = outcomes
        .iter()
        .any(|outcome| outcome.outcome == OutcomeKind::Failed);
    let attempted = outcomes
        .iter()
        .filter(|outcome| outcome.outcome != OutcomeKind::Skipped)
        .count();
    let mut kind = if !failed {
        setup::SetupKind::Complete
    } else if attempted > 0 {
        setup::SetupKind::Partial
    } else {
        setup::SetupKind::Failed
    };

    // A change that was planned and did not happen is never reported as a success, even when
    // every action claimed otherwise.
    if kind == setup::SetupKind::Complete
        && changes
            .iter()
            .any(|change| change.outcome == ChangeOutcome::NotApplied)
    {
        kind = setup::SetupKind::Partial;
    }
    if kind == setup::SetupKind::Complete && validation.as_ref().is_some_and(|report| !report.ok) {
        kind = setup::SetupKind::Partial;
    }

    RepositoryManagementResult {
        plan_id: plan.id.clone(),
        dry_run,
        kind,
        actions: outcomes,
        changes,
        project,
        manifest_path: manifest_written,
        validation,
        refused: Vec::new(),
        warnings: plan.warnings.clone(),
    }
}

/// Run one planned action.
fn run_action(
    plan: &RepositoryPlan,
    action: &RepositoryAction,
    runner: &GitRunner,
) -> RepositoryActionOutcome {
    let path = plan.root.join(&action.path);
    let result: Result<RepositoryActionOutcome> = match action.kind {
        // Never planned: an adopted repository is used as it is, and the plan says so.
        RepositoryActionKind::AdoptRepository => Ok(RepositoryActionOutcome::new(
            action,
            OutcomeKind::Skipped,
            "the existing Git repository is used as it is",
        )),
        RepositoryActionKind::InitializeRepository => {
            if path.is_dir() && discovery::is_repository_root(&path, true, runner) {
                // Something created a repository here between the review and the run: nothing
                // is initialised a second time, and the result says what happened.
                Ok(RepositoryActionOutcome::new(
                    action,
                    OutcomeKind::Skipped,
                    "a Git repository already exists here; it is used as it is",
                ))
            } else {
                discovery::initialize_repository(&path, runner).map(|_| {
                    RepositoryActionOutcome::new(
                        action,
                        OutcomeKind::Success,
                        "created a Git repository (git init -b main)",
                    )
                })
            }
        }
        RepositoryActionKind::ConfigureRemote | RepositoryActionKind::UpdateRemote => {
            let url = plan
                .target
                .repository(&action.target)
                .and_then(|repo| repo.remote_url.clone())
                .unwrap_or_default();
            let current = discovery::origin_url(&path, runner).unwrap_or(None);
            match (&current, action.kind) {
                // Nothing to do: the remote is already what the plan wants.
                (Some(existing), _) if *existing == url => Ok(RepositoryActionOutcome::new(
                    action,
                    OutcomeKind::Skipped,
                    format!("origin already points at {url}"),
                )),
                // An `origin` appeared since the review. Replacing it now would be a silent
                // overwrite of something the user never saw, so it is refused.
                (Some(existing), RepositoryActionKind::ConfigureRemote) => {
                    Err(Error::Other(format!(
                        "origin now points at {existing}, while the plan expected to add {url} to \
                         a repository without one — plan the change again"
                    )))
                }
                _ => discovery::ensure_origin_remote(&path, &url, runner).map(|change| {
                    let summary = match change {
                        OriginChange::Added => format!("origin set to {url}"),
                        OriginChange::Updated => format!("origin replaced with {url}"),
                        OriginChange::Unchanged => format!("origin already pointed at {url}"),
                    };
                    RepositoryActionOutcome::new(action, OutcomeKind::Success, summary)
                }),
            }
        }
        RepositoryActionKind::CloneRepository => {
            let url = plan
                .target
                .repository(&action.target)
                .and_then(|repo| repo.remote_url.clone())
                .unwrap_or_default();
            discovery::clone_repository(&url, &path, runner).map(|default| {
                let summary = match default {
                    Some(branch) => format!("cloned {url}; tracking origin/{branch}"),
                    None => format!("cloned {url}; the remote has no commits yet"),
                };
                RepositoryActionOutcome::new(action, OutcomeKind::Success, summary)
            })
        }
        RepositoryActionKind::AdoptRemoteHistory => {
            let wanted = plan
                .target
                .repository(&action.target)
                .and_then(|repo| repo.branch.clone());
            let url = discovery::origin_url(&path, runner)
                .unwrap_or(None)
                .unwrap_or_default();
            let branch = match crate::git::probe_remote(runner, &url) {
                crate::git::RemoteProbe::Reachable {
                    default_branch,
                    branches,
                } => history::adoption_branch(
                    default_branch.as_deref(),
                    &branches,
                    wanted.as_deref(),
                ),
                crate::git::RemoteProbe::Unreachable { .. } => None,
            };
            match branch {
                None => Err(Error::Other(format!(
                    "{url} has no branch to adopt; nothing was changed"
                ))),
                Some(branch) => discovery::adopt_remote_history(&path, &branch, runner).map(|adopted| {
                    match adopted {
                        Some(head) => RepositoryActionOutcome::new(
                            action,
                            OutcomeKind::Success,
                            format!("checked out origin's '{branch}' branch ({head}); local files were kept"),
                        ),
                        None => RepositoryActionOutcome::new(
                            action,
                            OutcomeKind::Skipped,
                            "the repository already has commits; its history is kept as it is",
                        ),
                    }
                }),
            }
        }
        RepositoryActionKind::UntrackFromRoot => {
            let relative = plan
                .target
                .repository(&action.target)
                .map(|repo| repo.relative_path.clone())
                .or_else(|| {
                    plan.project
                        .repository(&action.target)
                        .map(|repo| repo.relative_path.clone())
                })
                .unwrap_or_default();
            let tracked = discovery::count_files_tracked_under(&plan.root, &relative, runner);
            if tracked == 0 {
                // Replaying the plan its own result: nothing is tracked any more, so
                // nothing is run and nothing fails.
                Ok(RepositoryActionOutcome::new(
                    action,
                    OutcomeKind::Skipped,
                    "the root repository does not track any file there",
                ))
            } else {
                discovery::untrack_from_root(&plan.root, &relative, runner).map(|_| {
                    RepositoryActionOutcome::new(
                        action,
                        OutcomeKind::Success,
                        "stopped tracking these files in the root repository (files kept)",
                    )
                })
            }
        }
        RepositoryActionKind::UpdateManifest => {
            let current = std::fs::read_to_string(&plan.manifest_path).ok();
            if current.as_deref() == Some(plan.manifest_after.as_str()) {
                Ok(RepositoryActionOutcome::new(
                    action,
                    OutcomeKind::Skipped,
                    "the manifest on disk already describes this configuration",
                ))
            } else {
                match manifest::save_project(&plan.target) {
                    Ok(path) => Ok(RepositoryActionOutcome::new(
                        action,
                        OutcomeKind::Success,
                        format!("wrote {}", readable(&path, &plan.root)),
                    )),
                    Err(err) => Err(err),
                }
            }
        }
        RepositoryActionKind::VerifyProject => {
            let report = setup::verify_expecting(&plan.root, runner, &plan.expected_origins);
            if report.ok {
                Ok(RepositoryActionOutcome::new(
                    action,
                    OutcomeKind::Success,
                    format!(
                        "the project re-opens and all {} are valid",
                        count_label(report.repositories.len(), "repository", "repositories")
                    ),
                ))
            } else {
                Err(Error::Other(report.issues.join("\n")))
            }
        }
    };

    match result {
        Ok(outcome) => outcome,
        Err(err) => RepositoryActionOutcome::new(action, OutcomeKind::Failed, short_message(&err))
            .with_details(error_details(&err)),
    }
}

/// Decide what happened to one change, with the evidence that supports it.
fn verify_change(
    plan: &RepositoryPlan,
    change: &RepositoryChange,
    dry_run: bool,
    project: Option<&GitMeshProject>,
    runner: &GitRunner,
    skipped_targets: &[String],
    manifest_written: bool,
) -> RepositoryChangeOutcome {
    use RepositoryChangeKind::*;

    let mut evidence: Vec<String> = Vec::new();
    if dry_run {
        return RepositoryChangeOutcome {
            change: change.clone(),
            outcome: if change.is_planned() {
                ChangeOutcome::Planned
            } else if change.is_already_in_place() {
                ChangeOutcome::AlreadyInPlace
            } else {
                ChangeOutcome::NotApplied
            },
            evidence,
        };
    }
    if let StepState::Blocked(_) = change.state {
        return RepositoryChangeOutcome {
            change: change.clone(),
            outcome: ChangeOutcome::NotApplied,
            evidence: vec!["the change was refused before anything ran".to_string()],
        };
    }

    let recorded = project.and_then(|project| project.repository(&change.id));
    let mut outcome = ChangeOutcome::NotApplied;

    match change.kind {
        CloneRepository => {
            let absolute = plan.root.join(&change.path);
            let cloned = discovery::is_repository_root(&absolute, true, runner);
            match (recorded, cloned) {
                (Some(_), true) => {
                    evidence.push(format!(
                        "{}/.git exists and the manifest lists it",
                        change.path
                    ));
                    outcome = ChangeOutcome::Applied;
                }
                (None, _) => evidence.push(format!("the manifest does not list '{}'", change.id)),
                (Some(_), false) => {
                    evidence.push(format!("there is no .git in {}", change.path));
                }
            }
        }
        AddRepository | AdoptRepository => match recorded {
            Some(repo) => {
                evidence.push(format!(
                    "the manifest now lists '{}' at '{}'",
                    repo.id,
                    repo.relative_slash()
                ));
                let absolute = plan.root.join(&repo.relative_path);
                if absolute.is_dir() {
                    evidence.push(format!("the directory '{}' exists", repo.relative_slash()));
                }
                outcome = ChangeOutcome::Applied;
            }
            None => evidence.push(format!("the manifest does not list '{}'", change.id)),
        },
        InitializeRepository => {
            let absolute = plan.root.join(&change.path);
            if discovery::is_repository_root(&absolute, true, runner) {
                evidence.push(format!("{}/.git exists", change.path));
                outcome = ChangeOutcome::Applied;
            } else {
                evidence.push(format!("there is no .git in {}", change.path));
            }
        }
        RemoveRepositoryFromManifest => {
            match recorded {
                None => {
                    evidence.push(format!(
                        "'{}' is no longer in the manifest ({})",
                        change.id,
                        paths::display_relative(&plan.root, &plan.manifest_path)
                    ));
                    outcome = ChangeOutcome::Applied;
                }
                Some(_) => evidence.push(format!("the manifest still lists '{}'", change.id)),
            }
            if outcome == ChangeOutcome::Applied {
                // The proof that matters: what was *not* touched.
                let removal = plan.removals.iter().find(|removal| removal.id == change.id);
                let absolute = plan.root.join(&change.path);
                if absolute.is_dir() {
                    evidence.push(format!("the directory '{}' is still there", change.path));
                } else {
                    evidence.push(format!(
                        "the directory '{}' is gone, which this operation must never do",
                        change.path
                    ));
                    outcome = ChangeOutcome::NotApplied;
                }
                if let Some(removal) = removal {
                    if removal.is_repository {
                        if discovery::is_repository_root(&absolute, true, runner) {
                            evidence.push(format!("'{}/.git' is still there", change.path));
                            match (
                                removal.head.clone(),
                                runner.repo(&absolute).head_oid().ok().flatten(),
                            ) {
                                (Some(before), Some(after)) if before == after => evidence.push(
                                    format!("its history is untouched (HEAD {})", short(&after)),
                                ),
                                (Some(_), Some(after)) => evidence.push(format!(
                                    "its HEAD moved to {}, which this operation must never do",
                                    short(&after)
                                )),
                                (Some(before), None) => evidence.push(format!(
                                    "it had commits ({}) and now has none, which this operation \
                                     must never do",
                                    short(&before)
                                )),
                                _ => evidence.push("it has no commits, as before".to_string()),
                            }
                            match (
                                removal.origin.clone(),
                                discovery::origin_url(&absolute, runner).unwrap_or(None),
                            ) {
                                (Some(before), Some(after)) if before == after => {
                                    evidence.push(format!("its remote {after} is still there"))
                                }
                                (Some(before), after) => evidence.push(format!(
                                    "its remote was {before} and is now {}, which this operation \
                                     must never do",
                                    after.unwrap_or_else(|| "gone".to_string())
                                )),
                                (None, None) => {}
                                (None, Some(after)) => {
                                    evidence.push(format!("its remote {after} is unchanged"))
                                }
                            }
                        } else {
                            evidence.push(format!(
                                "there is no .git in '{}', which this operation must never do",
                                change.path
                            ));
                            outcome = ChangeOutcome::NotApplied;
                        }
                    }
                }
            }
        }
        RenameRepository => match recorded {
            Some(repo) if repo.relative_slash() == change.path => {
                evidence.push(format!(
                    "the manifest now records '{}' at '{}'",
                    repo.id, change.path
                ));
                outcome = ChangeOutcome::Applied;
            }
            _ => evidence.push(format!(
                "'{}' is not in the manifest under the new id",
                change.id
            )),
        },
        ConfigureRemote | UpdateRemote | KeepExistingRemote | RecordRemote | ClearRemote => {
            let repo = recorded
                .cloned()
                .or_else(|| plan.project.repository(&change.id).cloned());
            let Some(repo) = repo else {
                evidence.push(format!(
                    "'{}' is not configured, so its remote cannot be checked",
                    change.id
                ));
                return RepositoryChangeOutcome {
                    change: change.clone(),
                    outcome: ChangeOutcome::NotApplied,
                    evidence,
                };
            };
            let absolute = plan.root.join(&repo.relative_path);
            let is_repository = discovery::is_repository_root(&absolute, true, runner);
            let origin = if is_repository {
                discovery::origin_url(&absolute, runner).unwrap_or(None)
            } else {
                None
            };
            match repo.remote_url.clone() {
                Some(recorded_remote) => {
                    evidence.push(format!("the manifest records {recorded_remote}"));
                    if plan.expected_origins.contains(&repo.id) {
                        if origin.as_deref() == Some(recorded_remote.as_str()) {
                            evidence.push(format!("origin is {recorded_remote}"));
                            outcome = ChangeOutcome::Applied;
                        } else {
                            evidence.push(format!(
                                "origin is {}",
                                origin
                                    .clone()
                                    .unwrap_or_else(|| "not configured".to_string())
                            ));
                        }
                    } else {
                        evidence.push(
                            "the remote is recorded in the manifest; Git was not asked to \
                             configure it"
                                .to_string(),
                        );
                        outcome = ChangeOutcome::Applied;
                    }
                }
                None => {
                    evidence.push("no remote is recorded anymore".to_string());
                    if let Some(origin) = origin {
                        evidence.push(format!(
                            "origin {origin} is still configured in Git (GitMesh never removes a \
                             remote)"
                        ));
                    }
                    outcome = ChangeOutcome::Applied;
                }
            }
        }
        UntrackFromRoot => {
            let relative = plan
                .project
                .repository(&change.id)
                .map(|repo| repo.relative_path.clone())
                .unwrap_or_default();
            let tracked = discovery::count_files_tracked_under(&plan.root, &relative, runner);
            if change.is_planned() {
                if tracked == 0 {
                    evidence.push(
                        "the root repository no longer tracks any file inside this repository"
                            .to_string(),
                    );
                    outcome = ChangeOutcome::Applied;
                } else {
                    evidence.push(format!(
                        "the root repository still tracks {tracked} file(s) inside it"
                    ));
                }
            } else {
                evidence.push(format!(
                    "the root repository still tracks {tracked} file(s) inside it, as the plan \
                     said"
                ));
                outcome = ChangeOutcome::AlreadyInPlace;
            }
        }
        AlreadyManaged => {
            if let Some(repo) = recorded {
                evidence.push(format!(
                    "the manifest already lists '{}' at '{}'",
                    repo.id,
                    repo.relative_slash()
                ));
            }
            outcome = ChangeOutcome::AlreadyInPlace;
        }
    }

    if outcome == ChangeOutcome::Applied {
        // Nothing about this change ran, so it was already in place: the manifest carries
        // the configuration-only changes, the physical actions carry the rest.
        let carried_by_manifest = matches!(
            change.kind,
            RepositoryChangeKind::RemoveRepositoryFromManifest
                | RepositoryChangeKind::RenameRepository
        );
        let did_not_run = if carried_by_manifest {
            !manifest_written
        } else {
            skipped_targets.iter().any(|target| target == &change.id)
        };
        if change.is_already_in_place() || did_not_run {
            outcome = ChangeOutcome::AlreadyInPlace;
        }
    }

    RepositoryChangeOutcome {
        change: change.clone(),
        outcome,
        evidence,
    }
}

// ------------------------------------------------------------------- helpers --

/// Trim a URL and treat an empty string as "no remote".
fn clean_url(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Trim a text field and treat an empty string as absent.
fn clean_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// `3 repositories` / `1 repository`.
fn count_label(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

fn short(oid: &str) -> String {
    crate::git::short_oid(oid)
}

fn short_message(err: &Error) -> String {
    let text = err.to_string();
    text.lines().next().unwrap_or("failed").to_string()
}

fn error_details(err: &Error) -> Vec<String> {
    err.to_string().lines().map(str::to_string).collect()
}

/// A path as the interface names it: relative when it is inside the project.
fn readable(path: &Path, root: &Path) -> String {
    paths::display_relative(root, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::ProjectSession;
    use crate::testkit::RepoFixture;

    /// An "add this directory" intent with the defaults the interface uses.
    fn add(path: &str, id: &str, remote: Option<&str>) -> RepositoryIntent {
        RepositoryIntent::Add {
            path: path.to_string(),
            id: id.to_string(),
            remote: remote.map(str::to_string),
            branch: None,
            initialize: true,
            configure_remote: remote.is_some(),
            untrack_from_root: true,
        }
    }

    /// Plan one intent against the project as it is on disk.
    fn plan_for(fixture: &RepoFixture, intent: RepositoryIntent) -> RepositoryPlan {
        let project = fixture.load_project();
        super::plan(
            &project,
            &RepositoryManagementRequest::one(intent),
            fixture.runner(),
        )
        .expect("plan")
    }

    /// Apply a plan with no observer.
    fn apply(
        fixture: &RepoFixture,
        plan: &RepositoryPlan,
        dry_run: bool,
    ) -> RepositoryManagementResult {
        let mut observer = RepositoryObserver::silent();
        super::apply(plan, dry_run, fixture.runner(), &mut observer)
    }

    /// In-place apply of one intent, as a front end does it.
    fn run(fixture: &RepoFixture, intent: RepositoryIntent) -> RepositoryManagementResult {
        let plan = plan_for(fixture, intent);
        assert!(plan.is_ready(), "unexpected blockers: {:?}", plan.blockers);
        apply(fixture, &plan, false)
    }

    fn change(plan: &RepositoryPlan, kind: RepositoryChangeKind) -> &RepositoryChange {
        plan.changes
            .iter()
            .find(|change| change.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} change in {:?}", plan.changes))
    }

    fn action(plan: &RepositoryPlan, kind: RepositoryActionKind) -> &RepositoryAction {
        plan.actions
            .iter()
            .find(|action| action.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} action in {:?}", plan.actions))
    }

    fn evidence(result: &RepositoryManagementResult, kind: RepositoryChangeKind) -> Vec<String> {
        result
            .changes
            .iter()
            .find(|change| change.change.kind == kind)
            .map(|change| change.evidence.clone())
            .unwrap_or_default()
    }

    // ------------------------------------------------------------- inspection --

    #[test]
    fn inspection_reports_the_configuration_and_the_state_of_every_repository() {
        let fixture = RepoFixture::named("manage-inspect");
        fixture.project_with(&[("root", "."), ("engine", "engine"), ("tools", "tools")]);

        // `tools` loses its directory, `engine` gets an origin that contradicts the
        // manifest, and a plain directory appears that could become a repository.
        std::fs::remove_dir_all(fixture.path().join("tools")).unwrap();
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", "git@example.com:acme/engine.git"],
        );
        fixture.write("new-module/main.rs", "fn main() {}\n");
        // The root repository tracks part of the new directory: two owners for one file.
        fixture.add_all(".");
        fixture.commit(".", "track new-module");

        let project = fixture.load_project();
        let inspection = inspect(&project, fixture.runner()).expect("inspect");

        assert_eq!(inspection.project_name, project.name);
        assert_eq!(inspection.repositories.len(), 3);
        let root = inspection.repository("root").unwrap();
        assert_eq!(root.role, RepositoryRole::Root);
        assert_eq!(root.path, ".");
        assert!(root.is_usable());

        let engine = inspection.repository("engine").unwrap();
        assert_eq!(engine.state, RepositoryState::Ready);
        assert!(engine.is_repository && engine.has_commits);
        assert!(engine.clean, "the engine repository is committed");
        assert_eq!(engine.branch.as_deref(), Some("main"));
        assert_eq!(
            engine.origin.as_deref(),
            Some("git@example.com:acme/engine.git")
        );
        assert!(engine.issues.is_empty());

        let tools = inspection.repository("tools").unwrap();
        assert_eq!(tools.state, RepositoryState::Missing);
        assert!(!tools.is_usable());
        assert!(
            tools.issues[0].contains("does not exist"),
            "{:?}",
            tools.issues
        );

        // The candidates come from the same scanner the wizard uses.
        let paths: Vec<String> = inspection
            .candidates
            .iter()
            .map(|candidate| candidate.path_label())
            .collect();
        assert!(paths.contains(&"new-module".to_string()), "{paths:?}");
        assert!(inspection.manifest_path.is_file());
    }

    #[test]
    fn a_recorded_remote_that_is_not_configured_in_git_is_a_warning_not_an_issue() {
        let fixture = RepoFixture::named("manage-drift");
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // Record a remote in the manifest that Git does not have.
        let mut recorded = project.clone();
        recorded.repositories[1].remote_url = Some("git@github.com:acme/engine.git".into());
        manifest::save_project(&recorded).unwrap();

        let inspection = inspect(&fixture.load_project(), fixture.runner()).unwrap();
        let engine = inspection.repository("engine").unwrap();
        assert!(engine.issues.is_empty(), "{:?}", engine.issues);
        assert!(
            engine
                .warnings
                .iter()
                .any(|warning| warning.contains("has no origin")),
            "{:?}",
            engine.warnings
        );
        assert_eq!(
            engine.remote_label(),
            "git@github.com:acme/engine.git (recorded only)"
        );
    }

    #[test]
    fn the_root_tracking_a_managed_repository_is_reported_as_a_shared_ownership() {
        let fixture = RepoFixture::named("manage-shared");
        fixture.project_with(&[("root", ".")]);
        // The root repository tracks the files of the new directory, and the directory
        // becomes a repository of its own *without* untracking them: two repositories now
        // claim the same files, which is exactly what the interface has to report.
        fixture.write("new-module/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root owns new-module/lib.rs");

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Add {
                path: "new-module".into(),
                id: "new-module".into(),
                remote: None,
                branch: None,
                initialize: true,
                configure_remote: false,
                untrack_from_root: false,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(apply(&fixture, &plan, false).is_success());

        let inspection = inspect(&fixture.load_project(), fixture.runner()).unwrap();
        let module = inspection.repository("new-module").unwrap();
        assert!(module.tracked_by_root >= 1, "{}", module.tracked_by_root);
        assert!(
            module
                .warnings
                .iter()
                .any(|warning| warning.contains("belong to two repositories")),
            "{:?}",
            module.warnings
        );
        assert!(module.is_usable(), "the repository itself is fine");
    }

    #[test]
    fn candidate_inspection_answers_every_question_the_interface_asks() {
        let fixture = RepoFixture::named("manage-candidate");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("new-module/main.rs", "fn main() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks new-module");
        // A directory that contains a repository of its own.
        fixture.init_repo("vendor/dep");
        fixture.write("vendor/dep/README.md", "# dep\n");
        fixture.commit("vendor/dep", "dep");

        let project = fixture.load_project();

        let plain = inspect_candidate(&project, "new-module", fixture.runner()).unwrap();
        assert!(plain.exists && !plain.is_repository);
        assert!(plain.can_add());
        assert_eq!(plain.suggested_id, "new-module");
        assert_eq!(plain.tracked_by_root, 1);
        assert!(plain.managed_as.is_none());
        assert!(plain.consequence().contains("git init"));
        assert!(
            plain
                .warnings
                .iter()
                .any(|warning| warning.contains("two repositories")),
            "{:?}",
            plain.warnings
        );

        let adopted = inspect_candidate(&project, "engine", fixture.runner()).unwrap();
        assert!(adopted.is_repository && adopted.has_commits);
        assert_eq!(adopted.managed_as.as_deref(), Some("engine"));
        assert!(
            adopted
                .blockers
                .iter()
                .any(|blocker| blocker.contains("already assigned")),
            "{:?}",
            adopted.blockers
        );

        let nested = inspect_candidate(&project, "vendor", fixture.runner()).unwrap();
        assert!(!nested.is_repository);
        assert_eq!(nested.nested_repositories, vec!["vendor/dep".to_string()]);
        assert!(
            nested
                .warnings
                .iter()
                .any(|warning| warning.contains("does not manage nested repositories")),
            "{:?}",
            nested.warnings
        );
        assert!(nested.consequence().contains("git init"));

        let missing = inspect_candidate(&project, "absent", fixture.runner()).unwrap();
        assert!(!missing.exists && !missing.can_add());

        // Paths GitMesh refuses to reason about at all.
        assert!(inspect_candidate(&project, ".", fixture.runner()).is_err());
        assert!(inspect_candidate(&project, "../outside", fixture.runner()).is_err());
        assert!(inspect_candidate(&project, "/tmp", fixture.runner()).is_err());
        assert!(inspect_candidate(&project, ".git", fixture.runner()).is_err());
    }

    // ----------------------------------------------------------------- adding --

    #[test]
    fn adding_a_plain_directory_initialises_it_adds_it_and_untracks_it() {
        let fixture = RepoFixture::named("manage-add");
        fixture.write("new-module/main.rs", "fn main() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks new-module");
        fixture.project_with(&[("root", ".")]);

        let plan = plan_for(&fixture, add("new-module", "", None));
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(change(&plan, RepositoryChangeKind::InitializeRepository).is_planned());
        assert!(change(&plan, RepositoryChangeKind::AddRepository).is_planned());
        let untrack = change(&plan, RepositoryChangeKind::UntrackFromRoot);
        assert!(untrack.is_planned(), "{untrack:?}");
        assert_eq!(untrack.before.as_deref(), Some("1"));
        assert!(action(&plan, RepositoryActionKind::InitializeRepository).planned());
        assert!(action(&plan, RepositoryActionKind::UntrackFromRoot).planned());
        assert!(action(&plan, RepositoryActionKind::UpdateManifest).planned());
        assert!(action(&plan, RepositoryActionKind::VerifyProject).planned());
        assert_eq!(plan.manifest_after.matches("[[repositories]]").count(), 1);
        assert!(plan.manifest_after.contains("id = \"new-module\""));
        assert!(plan.safety.iter().any(|line| line.contains("stay on disk")));
        assert!(plan
            .safety
            .iter()
            .any(|line| line.contains("the same schema the rest of GitMesh reads")));

        let result = apply(&fixture, &plan, false);
        assert_eq!(
            result.kind,
            setup::SetupKind::Complete,
            "{:?}",
            result.actions
        );
        assert!(result.is_success() && result.exit_code() == 0);
        assert!(result.validation.as_ref().unwrap().ok);

        // The filesystem is what the plan promised.
        assert!(fixture.path().join("new-module/.git").is_dir());
        assert!(fixture.path().join("new-module/main.rs").is_file());
        let manifest_text =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();
        assert_eq!(
            manifest_text, plan.manifest_after,
            "the manifest is exactly the preview"
        );
        assert!(manifest_text.contains("new-module"));
        // The root repository no longer owns the files of the new repository.
        assert_eq!(
            fixture
                .git_ok(".", &["ls-files", "--", "new-module"])
                .trim(),
            ""
        );
        // And it is a normal GitMesh repository from now on.
        let session = ProjectSession::open(fixture.path()).expect("the project reopens");
        assert!(session.project().repository("new-module").is_some());

        // Every change is reported with the evidence that supports it.
        let add_evidence = evidence(&result, RepositoryChangeKind::AddRepository);
        assert!(
            add_evidence
                .iter()
                .any(|line| line.contains("the manifest now lists")),
            "{add_evidence:?}"
        );
        let init_evidence = evidence(&result, RepositoryChangeKind::InitializeRepository);
        assert!(
            init_evidence
                .iter()
                .any(|line| line.contains(".git exists")),
            "{init_evidence:?}"
        );
        let untrack_evidence = evidence(&result, RepositoryChangeKind::UntrackFromRoot);
        assert!(
            untrack_evidence
                .iter()
                .any(|line| line.contains("no longer tracks")),
            "{untrack_evidence:?}"
        );
    }

    #[test]
    fn adding_an_existing_repository_adopts_it_without_re_initialising_anything() {
        let fixture = RepoFixture::named("manage-adopt");
        fixture.project_with(&[("root", ".")]);
        fixture.init_repo("renderer");
        fixture.write("renderer/index.js", "export {};\n");
        fixture.commit("renderer", "renderer work");
        fixture.git_ok(
            "renderer",
            &[
                "remote",
                "add",
                "origin",
                "git@example.com:acme/renderer.git",
            ],
        );
        let head_before = fixture.git_ok("renderer", &["rev-parse", "HEAD"]);
        let git_dir_before = std::fs::read_dir(fixture.path().join("renderer/.git"))
            .unwrap()
            .count();

        let plan = plan_for(&fixture, add("renderer", "", None));
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(change(&plan, RepositoryChangeKind::AdoptRepository).is_planned());
        let adopt = action(&plan, RepositoryActionKind::AdoptRepository);
        assert!(adopt.is_already_in_place(), "{adopt:?}");
        assert!(adopt
            .state
            .reason()
            .unwrap()
            .contains("never re-initialised"));
        // Nothing about the existing repository is in the plan as work.
        assert!(!plan
            .planned_actions()
            .any(|action| { action.kind == RepositoryActionKind::InitializeRepository }));
        // Its remote is recorded, and Git is left alone.
        let remote = change(&plan, RepositoryChangeKind::KeepExistingRemote);
        assert!(remote.is_already_in_place(), "{remote:?}");
        assert!(plan
            .manifest_after
            .contains("git@example.com:acme/renderer.git"));

        let result = apply(&fixture, &plan, false);
        assert_eq!(result.kind, setup::SetupKind::Complete);
        assert_eq!(
            fixture.git_ok("renderer", &["rev-parse", "HEAD"]),
            head_before
        );
        assert_eq!(
            fixture
                .git_ok("renderer", &["remote", "get-url", "origin"])
                .trim(),
            "git@example.com:acme/renderer.git"
        );
        assert_eq!(
            std::fs::read_dir(fixture.path().join("renderer/.git"))
                .unwrap()
                .count(),
            git_dir_before,
            "the Git directory is the same one, untouched"
        );
        assert!(fixture.load_project().repository("renderer").is_some());
    }

    #[test]
    fn adding_a_directory_that_is_not_a_repository_is_refused_without_initialising_it() {
        let fixture = RepoFixture::named("manage-no-init");
        fixture.project_with(&[("root", ".")]);
        fixture.write("plain/notes.txt", "hello\n");
        let manifest_before =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Add {
                path: "plain".into(),
                id: "plain".into(),
                remote: None,
                branch: None,
                initialize: false,
                configure_remote: false,
                untrack_from_root: false,
            },
        );
        assert!(!plan.is_ready());
        assert!(
            plan.blockers[0].contains("is not a Git repository"),
            "{:?}",
            plan.blockers
        );
        let result = apply(&fixture, &plan, false);
        assert_eq!(result.kind, setup::SetupKind::Failed);
        assert_eq!(result.exit_code(), 1);
        assert_eq!(result.refused, plan.blockers);
        assert!(
            !fixture.path().join("plain/.git").exists(),
            "nothing was initialised"
        );
        assert_eq!(
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap(),
            manifest_before,
            "the manifest was not touched"
        );
    }

    #[test]
    fn duplicate_ids_overlaps_traversal_and_bad_ids_are_refused() {
        let fixture = RepoFixture::named("manage-refusals");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("fresh/main.rs", "fn main() {}\n");

        // An id that is already taken.
        let plan = plan_for(&fixture, add("fresh", "engine", None));
        assert!(!plan.is_ready());
        assert!(
            plan.blockers[0].contains("already used by the repository"),
            "{:?}",
            plan.blockers
        );

        // An id with characters the manifest does not accept.
        let plan = plan_for(&fixture, add("fresh", "not an id", None));
        assert!(!plan.is_ready());
        assert!(plan.blockers[0].contains("unsupported characters"));

        // A path inside an existing repository: overlapping boundaries.
        let plan = plan_for(&fixture, add("engine/src", "deep", None));
        assert!(!plan.is_ready());
        assert!(
            plan.blockers[0].contains("nested repository boundaries"),
            "{:?}",
            plan.blockers
        );

        // A path that contains an existing repository.
        let plan = plan_for(&fixture, add(".", "root-again", None));
        assert!(!plan.is_ready());

        // Paths that escape the project, absolute paths, and `.git` itself.
        for hostile in ["../outside", "/tmp/elsewhere", ".git"] {
            let plan = plan_for(&fixture, add(hostile, "hostile", None));
            assert!(!plan.is_ready(), "'{hostile}' should be refused");
            assert!(
                plan.blockers[0].contains("not a usable repository path")
                    || plan.blockers[0].contains("project root"),
                "'{hostile}': {:?}",
                plan.blockers
            );
        }

        // A directory that does not exist.
        let plan = plan_for(&fixture, add("absent", "absent", None));
        assert!(!plan.is_ready());
        assert!(plan.blockers[0].contains("does not exist"));
    }

    #[test]
    fn adding_a_directory_that_is_already_managed_is_a_noop_or_a_clear_hint() {
        let fixture = RepoFixture::named("manage-repeat-add");
        // An external repository without commits: the root repository cannot track it yet,
        // so re-adding it really is nothing to do.
        fixture.project_without_commits(&[("root", "."), ("engine", "engine")]);

        // Exactly what is there: nothing to do, never an error.
        let plan = plan_for(&fixture, add("engine", "engine", None));
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(plan.is_noop());
        assert!(change(&plan, RepositoryChangeKind::AlreadyManaged).is_already_in_place());
        assert_eq!(
            plan.summary(),
            "nothing to do: the project is already configured as requested"
        );

        let manifest_before =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();
        let result = apply(&fixture, &plan, false);
        assert_eq!(result.kind, setup::SetupKind::Complete);
        assert_eq!(
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap(),
            manifest_before,
            "repeating the operation rewrites nothing"
        );
        assert!(fixture.path().join("engine/.git").is_dir());

        // A different id for the same directory points at the right operation instead.
        let plan = plan_for(&fixture, add("engine", "engine-core", None));
        assert!(!plan.is_ready());
        assert!(plan.blockers[0].contains("rename"), "{:?}", plan.blockers);
    }

    // ----------------------------------------------------------------- remotes --

    #[test]
    fn remote_changes_follow_the_documented_semantics() {
        let fixture = RepoFixture::named("manage-remote");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let url = "git@example.com:acme/engine.git";
        let other = "git@example.com:acme/engine-mirror.git";

        // No remote, configuration asked for: add it.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::SetRemote {
                id: "engine".into(),
                remote: Some(url.into()),
                configure: true,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(change(&plan, RepositoryChangeKind::ConfigureRemote).is_planned());
        assert!(action(&plan, RepositoryActionKind::ConfigureRemote).planned());
        assert!(plan.expected_origins.contains(&"engine".to_string()));
        let result = apply(&fixture, &plan, false);
        assert_eq!(
            result.kind,
            setup::SetupKind::Complete,
            "{:?}",
            result.validation.map(|v| v.issues)
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["remote", "get-url", "origin"])
                .trim(),
            url
        );

        // The same URL again: already satisfied, nothing to run.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::SetRemote {
                id: "engine".into(),
                remote: Some(url.into()),
                configure: true,
            },
        );
        assert!(plan.is_ready() && plan.is_noop());
        assert!(change(&plan, RepositoryChangeKind::KeepExistingRemote).is_already_in_place());

        // A different URL without the explicit intent to configure Git is refused.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::SetRemote {
                id: "engine".into(),
                remote: Some(other.into()),
                configure: false,
            },
        );
        assert!(!plan.is_ready());
        assert!(
            plan.blockers[0].contains("without configuring Git"),
            "{:?}",
            plan.blockers
        );

        // A different URL with the explicit intent replaces it, and says so.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::SetRemote {
                id: "engine".into(),
                remote: Some(other.into()),
                configure: true,
            },
        );
        assert!(plan.is_ready());
        assert!(change(&plan, RepositoryChangeKind::UpdateRemote).is_planned());
        assert!(action(&plan, RepositoryActionKind::UpdateRemote).planned());
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("only because it was explicitly asked for")),
            "{:?}",
            plan.safety
        );
        let result = apply(&fixture, &plan, false);
        assert!(result.is_success(), "{:?}", result.actions);
        assert_eq!(
            fixture
                .git_ok("engine", &["remote", "get-url", "origin"])
                .trim(),
            other
        );

        // Clearing the recorded remote writes the manifest only, and says what it leaves.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::SetRemote {
                id: "engine".into(),
                remote: None,
                configure: false,
            },
        );
        assert!(plan.is_ready());
        assert!(change(&plan, RepositoryChangeKind::ClearRemote).is_planned());
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("stays configured in Git")),
            "{:?}",
            plan.warnings
        );
        let result = apply(&fixture, &plan, false);
        assert!(result.is_success(), "{:?}", result.actions);
        assert!(fixture
            .load_project()
            .repository("engine")
            .unwrap()
            .remote_url
            .is_none());
        assert_eq!(
            fixture
                .git_ok("engine", &["remote", "get-url", "origin"])
                .trim(),
            other,
            "GitMesh never removes a Git remote"
        );
    }

    #[test]
    fn a_recorded_remote_is_validated_in_the_manifest_only() {
        let fixture = RepoFixture::named("manage-record-only");
        fixture.project_with(&[("root", ".")]);
        fixture.write("engine/lib.rs", "pub fn go() {}\n");

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Add {
                path: "engine".into(),
                id: "engine".into(),
                remote: Some("git@example.com:acme/engine.git".into()),
                branch: None,
                initialize: true,
                configure_remote: false,
                untrack_from_root: false,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(change(&plan, RepositoryChangeKind::RecordRemote).is_planned());
        assert!(change(&plan, RepositoryChangeKind::InitializeRepository).is_planned());
        assert!(!plan
            .planned_actions()
            .any(|action| action.kind == RepositoryActionKind::ConfigureRemote));
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("written to the manifest only")),
            "{:?}",
            plan.safety
        );

        let result = apply(&fixture, &plan, false);
        assert_eq!(
            result.kind,
            setup::SetupKind::Complete,
            "{:?}",
            result.actions
        );
        let validation = result.validation.clone().unwrap();
        assert!(validation.ok, "{:?}", validation.issues);
        assert!(
            fixture
                .load_project()
                .repository("engine")
                .unwrap()
                .remote_url
                .is_some(),
            "the URL is in the manifest"
        );
        assert!(
            fixture.git_ok("engine", &["remote"]).trim().is_empty(),
            "and Git was not touched"
        );
    }

    #[test]
    fn a_repository_the_operation_does_not_concern_never_fails_it() {
        let fixture = RepoFixture::named("manage-unrelated-drift");
        let project =
            fixture.project_with(&[("root", "."), ("legacy", "legacy"), ("engine", "engine")]);
        // `legacy` records a remote in the manifest that Git does not have: pre-existing
        // drift that this operation has nothing to do with.
        let mut recorded = project.clone();
        recorded
            .repositories
            .iter_mut()
            .find(|repo| repo.id == "legacy")
            .unwrap()
            .remote_url = Some("git@example.com:acme/legacy.git".into());
        manifest::save_project(&recorded).unwrap();
        let legacy_head = fixture.git_ok("legacy", &["rev-parse", "HEAD"]);

        let result = run(
            &fixture,
            RepositoryIntent::SetRemote {
                id: "engine".into(),
                remote: Some("git@example.com:acme/engine.git".into()),
                configure: true,
            },
        );
        assert_eq!(
            result.kind,
            setup::SetupKind::Complete,
            "{:?}",
            result.actions
        );
        let validation = result.validation.clone().unwrap();
        assert!(validation.ok, "{:?}", validation.issues);
        assert_eq!(
            fixture.git_ok("legacy", &["rev-parse", "HEAD"]),
            legacy_head
        );
        assert!(
            fixture.git_ok("legacy", &["remote"]).trim().is_empty(),
            "its Git configuration was left alone"
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["remote", "get-url", "origin"])
                .trim(),
            "git@example.com:acme/engine.git"
        );
    }

    // ------------------------------------------------------------ ids and paths --

    #[test]
    fn renaming_an_id_changes_the_configuration_and_never_the_directory() {
        let fixture = RepoFixture::named("manage-rename");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let head_before = fixture.git_ok("engine", &["rev-parse", "HEAD"]);

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Rename {
                id: "engine".into(),
                new_id: "engine-core".into(),
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let rename = change(&plan, RepositoryChangeKind::RenameRepository);
        assert!(rename.is_planned());
        assert_eq!(rename.before.as_deref(), Some("engine"));
        assert_eq!(rename.after.as_deref(), Some("engine-core"));
        assert!(rename.detail.contains("is not renamed"));
        assert!(action(&plan, RepositoryActionKind::UpdateManifest).planned());
        assert!(!plan.manifest_after.contains("id = \"engine\""));

        let result = apply(&fixture, &plan, false);
        assert!(result.is_success(), "{:?}", result.actions);
        assert!(
            fixture.path().join("engine").is_dir(),
            "the directory is untouched"
        );
        assert_eq!(
            fixture.git_ok("engine", &["rev-parse", "HEAD"]),
            head_before
        );
        let project = fixture.load_project();
        assert!(project.repository("engine").is_none());
        let renamed = project.repository("engine-core").unwrap();
        assert_eq!(renamed.relative_slash(), "engine");
        // The project still works normally under the new id.
        let session = ProjectSession::open(fixture.path()).unwrap();
        let status = session.status();
        assert!(status
            .repositories
            .iter()
            .any(|repo| repo.id == "engine-core"));
    }

    #[test]
    fn renaming_refuses_taken_and_invalid_ids_and_does_nothing_for_the_same_id() {
        let fixture = RepoFixture::named("manage-rename-refusals");
        fixture.project_with(&[("root", "."), ("engine", "engine"), ("tools", "tools")]);

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Rename {
                id: "engine".into(),
                new_id: "tools".into(),
            },
        );
        assert!(!plan.is_ready());
        assert!(plan.blockers[0].contains("already used by the repository"));

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Rename {
                id: "engine".into(),
                new_id: String::new(),
            },
        );
        assert!(!plan.is_ready());
        assert!(plan.blockers[0].contains("must not be empty"));

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Rename {
                id: "engine".into(),
                new_id: "engine".into(),
            },
        );
        assert!(plan.is_ready() && plan.is_noop());
        assert!(change(&plan, RepositoryChangeKind::AlreadyManaged).is_already_in_place());

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Rename {
                id: "unknown".into(),
                new_id: "anything".into(),
            },
        );
        assert!(!plan.is_ready());
    }

    // ---------------------------------------------------------------- removal --

    #[test]
    fn the_plan_promises_exactly_the_remotes_it_configured() {
        let fixture = RepoFixture::named("manage-expectations");
        fixture.project_with(&[("root", "."), ("legacy", "legacy")]);
        fixture.write("engine/main.rs", "fn main() {}\n");
        fixture.write("tools/main.rs", "fn main() {}\n");
        // `legacy` records a remote Git does not have. That is a warning about that
        // repository, never a failure of an operation that does not concern it.
        let mut project = fixture.load_project();
        project
            .repositories
            .iter_mut()
            .find(|repo| repo.id == "legacy")
            .unwrap()
            .remote_url = Some("git@example.com:acme/legacy.git".to_string());
        manifest::save_project(&project).unwrap();
        let legacy_head = fixture.git_ok("legacy", &["rev-parse", "HEAD"]);

        // Two promises in one plan: `engine` gets `origin` configured in Git, `tools` only
        // gets a recorded one. The validation after the run checks exactly those two.
        let request = RepositoryManagementRequest {
            intents: vec![
                add("engine", "engine", Some("git@example.com:acme/engine.git")),
                RepositoryIntent::Add {
                    path: "tools".into(),
                    id: "tools".into(),
                    remote: Some("git@example.com:acme/tools.git".into()),
                    branch: None,
                    initialize: true,
                    configure_remote: false,
                    untrack_from_root: true,
                },
            ],
        };
        let plan = plan(&fixture.load_project(), &request, fixture.runner()).expect("plan");
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert_eq!(plan.expected_origins, vec!["engine".to_string()]);
        assert!(change(&plan, RepositoryChangeKind::ConfigureRemote).is_planned());
        assert!(change(&plan, RepositoryChangeKind::RecordRemote).is_planned());

        let result = apply(&fixture, &plan, false);
        assert!(result.is_success(), "{:?}", result.validation);
        assert!(
            result
                .validation
                .as_ref()
                .is_some_and(|validation| validation.ok),
            "{:?}",
            result.validation
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["remote", "get-url", "origin"])
                .trim(),
            "git@example.com:acme/engine.git"
        );
        assert!(
            fixture.git("tools", &["remote", "get-url", "origin"]).code != Some(0),
            "a recorded remote never touches Git"
        );
        assert_eq!(
            fixture.git_ok("legacy", &["rev-parse", "HEAD"]),
            legacy_head
        );

        let project = fixture.load_project();
        assert_eq!(
            project.repository("engine").unwrap().remote_url.as_deref(),
            Some("git@example.com:acme/engine.git")
        );
        assert_eq!(
            project.repository("tools").unwrap().remote_url.as_deref(),
            Some("git@example.com:acme/tools.git")
        );
    }

    #[test]
    fn removing_a_repository_keeps_its_directory_git_history_and_remote() {
        let fixture = RepoFixture::named("manage-remove");
        fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", "git@example.com:acme/engine.git"],
        );
        let head_before = fixture.git_ok("engine", &["rev-parse", "HEAD"]);
        let renderer_head = fixture.git_ok("renderer", &["rev-parse", "HEAD"]);
        let root_head = fixture.git_ok(".", &["rev-parse", "HEAD"]);

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "engine".into(),
                confirm_takeover: false,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let removal = change(&plan, RepositoryChangeKind::RemoveRepositoryFromManifest);
        assert!(removal.is_planned());
        assert!(removal
            .detail
            .contains("its history and its remote are kept"));
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("changes the configuration only")),
            "{:?}",
            plan.safety
        );
        assert!(
            plan.notices.iter().any(|line| line.contains("not touched")),
            "{:?}",
            plan.notices
        );
        assert_eq!(plan.removals.len(), 1);
        assert_eq!(plan.removals[0].head.as_deref(), Some(head_before.trim()));

        let result = apply(&fixture, &plan, false);
        assert_eq!(
            result.kind,
            setup::SetupKind::Complete,
            "{:?}",
            result.actions
        );
        let validation = result.validation.clone().unwrap();
        assert!(validation.ok, "{:?}", validation.issues);

        // Nothing physical changed.
        assert!(fixture.path().join("engine").is_dir());
        assert!(fixture.path().join("engine/.git").is_dir());
        assert_eq!(
            fixture.git_ok("engine", &["rev-parse", "HEAD"]),
            head_before
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["remote", "get-url", "origin"])
                .trim(),
            "git@example.com:acme/engine.git"
        );
        // The configuration did.
        let project = fixture.load_project();
        assert!(project.repository("engine").is_none());
        assert!(project.repository("renderer").is_some());
        assert!(
            !std::fs::read_to_string(manifest::manifest_path(fixture.path()))
                .unwrap()
                .contains("engine.git")
        );
        // Unrelated repositories were not touched.
        assert_eq!(
            fixture.git_ok("renderer", &["rev-parse", "HEAD"]),
            renderer_head
        );
        assert_eq!(fixture.git_ok(".", &["rev-parse", "HEAD"]), root_head);

        // The result carries the proof, not just the claim.
        let proof = evidence(&result, RepositoryChangeKind::RemoveRepositoryFromManifest);
        assert!(
            proof
                .iter()
                .any(|line| line.contains("no longer in the manifest")),
            "{proof:?}"
        );
        assert!(
            proof.iter().any(|line| line.contains("is still there")),
            "{proof:?}"
        );
        assert!(
            proof
                .iter()
                .any(|line| line.contains(".git' is still there")),
            "{proof:?}"
        );
        assert!(
            proof
                .iter()
                .any(|line| line.contains("history is untouched")),
            "{proof:?}"
        );
        assert!(
            proof
                .iter()
                .any(|line| line.contains("remote") && line.contains("still there")),
            "{proof:?}"
        );

        // The project keeps working, and the directory is simply owned by the root again.
        let session = ProjectSession::open(fixture.path()).unwrap();
        assert!(session.project().repository("engine").is_none());
        assert!(session
            .status()
            .repositories
            .iter()
            .any(|repo| repo.id == "root"));
    }

    #[test]
    fn removing_the_root_is_refused_and_removing_an_unknown_id_is_nothing_to_do() {
        let fixture = RepoFixture::named("manage-remove-cases");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "root".into(),
                confirm_takeover: false,
            },
        );
        assert!(!plan.is_ready());
        assert!(plan.blockers[0].contains("cannot be removed"));

        // Repeated removals are safe: the second one has nothing to do.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "already-gone".into(),
                confirm_takeover: false,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(plan.is_noop());
        assert!(
            plan.notices
                .iter()
                .any(|line| line.contains("nothing to remove")),
            "{:?}",
            plan.notices
        );
        let result = apply(&fixture, &plan, false);
        assert_eq!(result.kind, setup::SetupKind::Complete);
    }

    #[test]
    fn removing_a_repository_the_root_still_tracks_needs_an_explicit_confirmation() {
        let fixture = RepoFixture::named("manage-remove-takeover");
        fixture.project_with(&[("root", ".")]);
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root owns engine/lib.rs");
        // The directory becomes a repository without untracking: the root still tracks it,
        // so removing it hands those files back to the root repository.
        let plan = plan_for(
            &fixture,
            RepositoryIntent::Add {
                path: "engine".into(),
                id: "engine".into(),
                remote: None,
                branch: None,
                initialize: true,
                configure_remote: false,
                untrack_from_root: false,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(apply(&fixture, &plan, false).is_success());

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "engine".into(),
                confirm_takeover: false,
            },
        );
        assert!(!plan.is_ready());
        assert!(
            plan.blockers[0].contains("hands those files back"),
            "{:?}",
            plan.blockers
        );
        assert!(matches!(
            change(&plan, RepositoryChangeKind::RemoveRepositoryFromManifest).state,
            StepState::Blocked(_)
        ));

        let plan = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "engine".into(),
                confirm_takeover: true,
            },
        );
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("owns the")),
            "{:?}",
            plan.warnings
        );
        let result = apply(&fixture, &plan, false);
        assert!(result.is_success(), "{:?}", result.actions);
        assert!(fixture
            .git_ok(".", &["ls-files", "--", "engine"])
            .contains("lib.rs"));
    }

    // ------------------------------------------------------------- idempotency --

    #[test]
    fn applying_the_same_plan_twice_changes_nothing_the_second_time() {
        let fixture = RepoFixture::named("manage-twice");
        fixture.project_with(&[("root", ".")]);
        fixture.write("new-module/main.rs", "fn main() {}\n");

        let intent = add(
            "new-module",
            "",
            Some("git@example.com:acme/new-module.git"),
        );
        let plan = plan_for(&fixture, intent.clone());
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let result = apply(&fixture, &plan, false);
        assert!(result.is_success(), "{:?}", result.actions);

        // The very same plan object replays as "everything is already there": nothing is
        // initialised, replaced or written a second time.
        let manifest_before =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();
        let replay = apply(&fixture, &plan, false);
        assert_eq!(
            replay.kind,
            setup::SetupKind::Complete,
            "{:?}",
            replay.actions
        );
        assert_eq!(
            replay.succeeded(),
            1,
            "only the read-only verification runs again"
        );
        assert_eq!(
            replay.applied().count(),
            0,
            "nothing is reported as newly applied"
        );
        assert!(
            replay
                .changes
                .iter()
                .all(|change| change.outcome != ChangeOutcome::NotApplied),
            "{:?}",
            replay.changes
        );
        assert_eq!(
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap(),
            manifest_before,
            "the manifest is not rewritten with the same content"
        );
        assert!(fixture.path().join("new-module/.git").is_dir());

        // Planning the same intent again reports it as nothing to do.
        let again = plan_for(&fixture, intent);
        assert!(again.is_noop(), "{:?}", again.actions);
        assert!(again
            .changes
            .iter()
            .any(|change| change.kind == RepositoryChangeKind::AlreadyManaged));

        // And a removal replayed after it happened is nothing to do as well.
        let removal = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "new-module".into(),
                confirm_takeover: false,
            },
        );
        assert!(apply(&fixture, &removal, false).is_success());
        let repeated = plan_for(
            &fixture,
            RepositoryIntent::Remove {
                id: "new-module".into(),
                confirm_takeover: false,
            },
        );
        assert!(repeated.is_noop());
        let replayed = apply(&fixture, &repeated, false);
        assert!(replayed.is_success());
        assert!(
            fixture.path().join("new-module/.git").is_dir(),
            "still on disk"
        );
    }

    #[test]
    fn a_dry_run_reports_everything_and_changes_nothing() {
        let fixture = RepoFixture::named("manage-dry-run");
        fixture.project_with(&[("root", ".")]);
        fixture.write("new-module/main.rs", "fn main() {}\n");
        let manifest_before =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();

        let plan = plan_for(&fixture, add("new-module", "", None));
        let result = apply(&fixture, &plan, true);
        assert!(result.dry_run);
        assert_eq!(result.kind, setup::SetupKind::Complete);
        assert!(result.sentence().contains("Dry run"));
        assert!(result.validation.is_none());
        assert!(result
            .changes
            .iter()
            .all(|change| change.outcome == ChangeOutcome::Planned
                || change.outcome == ChangeOutcome::AlreadyInPlace));
        assert!(!fixture.path().join("new-module/.git").exists());
        assert_eq!(
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn a_stale_plan_is_refused_instead_of_applied() {
        let fixture = RepoFixture::named("manage-stale");
        fixture.project_with(&[("root", ".")]);
        fixture.write("new-module/main.rs", "fn main() {}\n");
        let plan = plan_for(&fixture, add("new-module", "", None));
        assert!(plan.is_ready());

        // Somebody else changed the configuration in between (another GitMesh process, an
        // editor, a checkout).
        let mut edited = fixture.load_project();
        edited.name = "renamed-project".to_string();
        manifest::save_project(&edited).unwrap();
        let manifest_before =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();

        let result = apply(&fixture, &plan, false);
        assert_eq!(result.kind, setup::SetupKind::Failed);
        assert!(
            result.refused[0].contains("changed since the plan was reviewed"),
            "{:?}",
            result.refused
        );
        assert!(!fixture.path().join("new-module/.git").exists());
        assert_eq!(
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap(),
            manifest_before
        );
    }

    #[test]
    fn a_plan_is_bound_to_the_configuration_it_was_made_from() {
        let fixture = RepoFixture::named("manage-bound");
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // The configuration on disk no longer describes the project that was opened.
        let mut other = project.clone();
        other.repositories.clear();
        other.repositories.push(project.root_repository().clone());
        manifest::save_project(&other).unwrap();

        let plan = super::plan(
            &project,
            &RepositoryManagementRequest::one(add("engine", "engine", None)),
            fixture.runner(),
        )
        .unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers[0].contains("describes a different configuration"),
            "{:?}",
            plan.blockers
        );
    }

    // ---------------------------------------------------------- partial failure --

    #[test]
    fn one_repository_failing_does_not_stop_the_others() {
        let fixture = RepoFixture::named("manage-partial");
        fixture.project_with(&[("root", "."), ("untouched", "untouched")]);
        fixture.write("good/main.rs", "fn main() {}\n");
        fixture.write("blocked/main.rs", "fn main() {}\n");
        let blocked = fixture.path().join("blocked");
        let untouched_head = fixture.git_ok("untouched", &["rev-parse", "HEAD"]);

        // `git init` cannot write here, so that one repository fails while the other works.
        let mut permissions = std::fs::metadata(&blocked).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o500);
        std::fs::set_permissions(&blocked, permissions).unwrap();

        let request = RepositoryManagementRequest {
            intents: vec![
                add("good", "good", Some("git@example.com:acme/good.git")),
                add("blocked", "blocked", None),
            ],
        };
        let project = fixture.load_project();
        let plan = super::plan(&project, &request, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        let result = apply(&fixture, &plan, false);
        assert_eq!(
            result.kind,
            setup::SetupKind::Partial,
            "{:?}",
            result.actions
        );
        assert_eq!(result.exit_code(), 1);
        assert!(!result.is_success());
        let failures: Vec<&RepositoryActionOutcome> = result.failures().collect();
        // The repository that could not be created, and the verification that says the
        // project is not what the plan promised.
        assert_eq!(failures.len(), 2, "{:?}", result.actions);
        assert!(
            failures.iter().any(|failure| failure.kind
                == RepositoryActionKind::InitializeRepository
                && failure.target == "blocked"),
            "{:?}",
            result.actions
        );
        assert!(
            failures
                .iter()
                .any(|failure| failure.kind == RepositoryActionKind::VerifyProject),
            "{:?}",
            result.actions
        );
        assert!(
            failures
                .iter()
                .flat_map(|failure| failure.details.iter())
                .any(|detail| detail.contains("Permission denied")),
            "{:?}",
            result.actions
        );
        // The repository that could be created was created, and the manifest records both.
        assert!(fixture.path().join("good/.git").is_dir());
        let manifest_text =
            std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();
        assert!(manifest_text.contains("good") && manifest_text.contains("blocked"));
        // Validation says exactly what is missing, and the unrelated repository is untouched.
        let validation = result.validation.clone().unwrap();
        assert!(!validation.ok);
        assert!(
            validation
                .issues
                .iter()
                .any(|issue| issue.contains("blocked")),
            "{:?}",
            validation.issues
        );
        assert_eq!(
            fixture.git_ok("untouched", &["rev-parse", "HEAD"]),
            untouched_head
        );
        assert!(fixture.git_ok("untouched", &["remote"]).trim().is_empty());

        let mut permissions = std::fs::metadata(&blocked).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&blocked, permissions).unwrap();
    }

    // ------------------------------------------------------------- the plan text --

    #[test]
    fn the_plan_describes_the_change_before_anything_runs() {
        let fixture = RepoFixture::named("manage-plan-text");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("new-module/main.rs", "fn main() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks new-module");

        let request = RepositoryManagementRequest {
            intents: vec![
                add(
                    "new-module",
                    "new-module",
                    Some("git@example.com:acme/new-module.git"),
                ),
                RepositoryIntent::Remove {
                    id: "engine".into(),
                    confirm_takeover: false,
                },
            ],
        };
        let project = fixture.load_project();
        let plan = super::plan(&project, &request, fixture.runner()).unwrap();

        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert_eq!(plan.name, project.name);
        assert_eq!(plan.id.len(), 16);
        assert!(
            plan.summary().contains("changes to the configuration")
                && plan.summary().contains("steps to run"),
            "{}",
            plan.summary()
        );
        assert!(!plan.is_noop());

        // Both intents are visible, in order, with their consequences.
        let kinds: Vec<RepositoryChangeKind> = plan.changes.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&RepositoryChangeKind::InitializeRepository));
        assert!(kinds.contains(&RepositoryChangeKind::AddRepository));
        assert!(kinds.contains(&RepositoryChangeKind::ConfigureRemote));
        assert!(kinds.contains(&RepositoryChangeKind::UntrackFromRoot));
        assert!(kinds.contains(&RepositoryChangeKind::RemoveRepositoryFromManifest));

        // The manifest text is the target configuration, and it is valid on its own.
        assert!(plan.manifest_after.contains("new-module"));
        assert!(!plan.manifest_after.contains("engine"));
        assert!(plan.manifest_changes);
        assert_eq!(
            plan.manifest_after.matches("[[repositories]]").count(),
            1,
            "{}",
            plan.manifest_after
        );

        // The fingerprint changes when the request does, so a reviewed plan cannot be
        // confused with another one.
        let mut other_request = request.clone();
        other_request.intents[0] = add("new-module", "renamed-module", None);
        let other = super::plan(&fixture.load_project(), &other_request, fixture.runner()).unwrap();
        assert_ne!(plan.id, other.id);

        // Apply both changes in one run, as the command line does.
        let result = apply(&fixture, &plan, false);
        assert_eq!(
            result.kind,
            setup::SetupKind::Complete,
            "{:?}",
            result.actions
        );
        let project = fixture.load_project();
        assert!(project.repository("new-module").is_some());
        assert!(project.repository("engine").is_none());
        assert!(
            fixture.path().join("engine").is_dir(),
            "removal kept the directory"
        );
        assert!(fixture.path().join("new-module/.git").is_dir());
    }

    #[test]
    fn the_result_sentence_matches_what_happened() {
        let fixture = RepoFixture::named("manage-sentence");
        fixture.project_without_commits(&[("root", ".")]);
        fixture.write("new-module/main.rs", "fn main() {}\n");

        let plan = plan_for(&fixture, add("new-module", "", None));
        let result = apply(&fixture, &plan, false);
        assert!(
            result.sentence().contains("applied"),
            "{}",
            result.sentence()
        );

        let nothing = plan_for(&fixture, add("new-module", "", None));
        assert_eq!(
            nothing.summary(),
            "nothing to do: the project is already configured as requested"
        );
        let result = apply(&fixture, &nothing, false);
        assert!(
            result.sentence().contains("Nothing to do"),
            "{}",
            result.sentence()
        );
    }
}
