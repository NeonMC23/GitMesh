//! Project creation: from an ordinary directory to a configured GitMesh project.
//!
//! This is the application-layer service behind "make me a GitMesh project out of this
//! folder". It is front-end neutral: `gitmesh init` drives it, the graphical wizard
//! drives it, and a future TUI screen can drive it without anything else changing.
//!
//! The contract is **plan first**:
//!
//! ```text
//!   SetupRequest  ->  SetupPlan  ->  (review)  ->  apply  ->  SetupResult  ->  verify
//! ```
//!
//! * [`plan`] inspects the filesystem but changes nothing. It produces the complete
//!   list of steps, the exact manifest text, and every blocker and warning.
//! * [`apply`] replays exactly that plan, step by step, and never recomputes it. What
//!   the user reviewed is what runs.
//! * [`verify`] re-opens the resulting project through the normal opening path
//!   ([`crate::service::ProjectSession::open`]) and checks the repositories on disk.
//!
//! Safety rules are enforced here rather than in a front end, so every front end gets
//! them:
//!
//! * an existing `.git` directory is never deleted and never re-initialised;
//! * an existing `origin` is only replaced when the request explicitly allows it;
//! * an existing manifest is only replaced when the request explicitly allows it;
//! * no file is moved or deleted — the one index-only operation
//!   ([`SetupStepKind::UntrackFromRoot`]) leaves every file on disk;
//! * ambiguous layouts (overlapping or nested repository boundaries, duplicate ids) are
//!   refused by the plan instead of being applied half way.

use std::path::{Path, PathBuf};

use crate::discovery::{
    self, DirectoryNode, DiscoveredRepository, OriginChange, ProjectScan, ScanOptions,
};
use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::manifest;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryRole};
use crate::ops::OutcomeKind;
use crate::paths::{self, to_slash};
use crate::providers::{self, RemoteRef};
use crate::service::ProjectSession;

// --------------------------------------------------------------- inspection --

/// One directory of the scanned tree, as the wizard presents it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateDirectory {
    /// Directory name (`.` for the project root).
    pub name: String,
    /// Project-relative path.
    pub relative_path: PathBuf,
    /// Depth below the project root (0 = the root itself).
    pub depth: usize,
    /// Files directly in this directory.
    pub files: usize,
    /// Files in this directory and everything below it.
    pub subtree_files: usize,
    /// True when the directory is the root of its own Git repository.
    pub is_repository: bool,
    /// True when it contains a `.git` entry.
    pub has_git_dir: bool,
    /// True when that repository has at least one commit.
    pub has_commits: bool,
    /// Branch currently checked out, when it is a repository.
    pub branch: Option<String>,
    /// `origin` of that repository, when it has one.
    pub remote: Option<String>,
    /// Files tracked by the *root* repository inside this directory.
    ///
    /// Turning the directory into a repository while the root repository still tracks
    /// those files is what makes ownership ambiguous, so the wizard has to know.
    pub tracked_by_root: usize,
    /// Repositories nested inside this directory.
    pub nested_repositories: Vec<PathBuf>,
    /// Subdirectories.
    pub children: Vec<CandidateDirectory>,
}

impl CandidateDirectory {
    /// True when the directory looks like a repository boundary the user probably wants.
    pub fn suggested(&self) -> bool {
        self.is_repository && self.depth > 0
    }

    /// Project-relative path as a slash string.
    pub fn path_label(&self) -> String {
        to_slash(&self.relative_path)
    }
}

/// What the wizard shows after the user picks a directory: facts only, no changes.
#[derive(Debug, Clone)]
pub struct Inspection {
    /// Absolute, normalised project root candidate.
    pub root: PathBuf,
    /// False when the path does not exist or is not a directory.
    pub exists: bool,
    /// Name GitMesh would use for the project.
    pub suggested_name: String,
    /// True when `.gitmesh/project.toml` is present.
    pub is_gitmesh_project: bool,
    /// Path of the manifest, whether or not it exists.
    pub manifest_path: PathBuf,
    /// Why an existing manifest could not be read, if it could not.
    pub manifest_error: Option<String>,
    /// Manifest text of the already configured project (for comparison in the UI).
    pub manifest_text: Option<String>,
    /// True when the root is the top level of its own Git repository.
    pub root_is_repository: bool,
    /// Repository that contains the root, when the root is *not* a repository root.
    pub enclosing_repository: Option<PathBuf>,
    /// Every Git repository found inside the root.
    pub repositories: Vec<DiscoveredRepository>,
    /// The scanned tree, annotated with what the wizard needs.
    pub candidates: Vec<CandidateDirectory>,
    /// Facts worth telling the user before they decide anything.
    pub notices: Vec<String>,
    /// True when the scan stopped early (depth limit).
    pub truncated: bool,
}

/// Inspect a directory as a candidate project root. Changes nothing.
pub fn inspect(root: &Path, runner: &GitRunner) -> Result<Inspection> {
    let root = paths::lexical_normalize(&paths::absolute(root)?);
    let manifest_path = manifest::manifest_path(&root);
    let suggested_name = project_name_for(&root);
    if !root.is_dir() {
        return Ok(Inspection {
            root,
            exists: false,
            suggested_name,
            is_gitmesh_project: false,
            manifest_path,
            manifest_error: None,
            manifest_text: None,
            root_is_repository: false,
            enclosing_repository: None,
            repositories: Vec::new(),
            candidates: Vec::new(),
            notices: Vec::new(),
            truncated: false,
        });
    }

    // The presence of the file decides whether this already is a GitMesh project: a
    // manifest that cannot be read is reported, never mistaken for "no project here".
    let is_gitmesh_project = manifest_path.exists();
    let (manifest_error, manifest_text) = if is_gitmesh_project {
        match std::fs::read_to_string(&manifest_path) {
            Ok(text) => match manifest::parse_manifest(&text, &root, &manifest_path) {
                Ok(project) => (None, manifest::render_manifest(&project).ok()),
                Err(err) => (Some(err.to_string()), None),
            },
            Err(err) => (Some(format!("the manifest could not be read: {err}")), None),
        }
    } else {
        (None, None)
    };

    // The scan is best effort: people point the wizard at directories that contain an
    // unreadable sub-directory, and refusing to show anything at all would be worse than
    // showing exactly what could be read. The failure is always reported as a notice, and
    // the root facts below are answered without the scan.
    let mut notices: Vec<String> = Vec::new();
    let scan = match discovery::scan_project(&root, &ScanOptions::default(), runner) {
        Ok(scan) => Some(scan),
        Err(err) => {
            notices.push(format!(
                "the directory could not be scanned completely ({err}); the tree below the \
                 project root is not shown, and nothing was changed"
            ));
            None
        }
    };
    let candidates = match &scan {
        Some(scan) => candidate_tree(scan, &root, runner)?,
        None => Vec::new(),
    };
    if let Some(scan) = &scan {
        notices.extend(scan.notices.iter().cloned());
    }
    if is_gitmesh_project && manifest_error.is_none() {
        notices.push(format!(
            "'{}' already contains a GitMesh project; opening it is usually what you want",
            readable(&manifest_path, &root)
        ));
    }
    if let Some(error) = &manifest_error {
        notices.push(format!("the existing manifest could not be read: {error}"));
    }

    let root_is_repository = match &scan {
        Some(scan) => scan.root_is_repository,
        None => discovery::is_repository_root(&root, true, runner),
    };
    let enclosing_repository = match &scan {
        Some(scan) => scan.enclosing_repository.clone(),
        None => runner.repo(&root).top_level()?.filter(|top| *top != root),
    };

    Ok(Inspection {
        root,
        exists: true,
        suggested_name,
        is_gitmesh_project,
        manifest_path,
        manifest_error,
        manifest_text,
        root_is_repository,
        enclosing_repository,
        repositories: scan.map(|scan| scan.repositories).unwrap_or_default(),
        candidates,
        notices,
        truncated: false,
    })
}

/// How deep the wizard looks for nested repositories inside a candidate directory.
const NESTED_SCAN_DEPTH: usize = 4;

/// Directories containing a `.git` entry, relative to `dir` (never `dir` itself).
///
/// A plain filesystem walk: this is detection for a warning, not Git work, and it must
/// stay cheap because the wizard runs it for every candidate.
fn nested_git_dirs(dir: &Path, max_depth: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, root: &Path, depth: usize, max_depth: usize, found: &mut Vec<PathBuf>) {
        if depth > max_depth {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if !path.is_dir() || discovery::IGNORED_DIRS.contains(&name.as_str()) {
                continue;
            }
            if path.join(".git").exists() {
                if let Some(relative) = paths::project_relative(root, &path) {
                    found.push(relative);
                }
                continue; // Do not descend into a repository.
            }
            walk(&path, root, depth + 1, max_depth, found);
        }
    }
    let mut found = Vec::new();
    walk(dir, dir, 1, max_depth, &mut found);
    found.sort();
    found
}

fn candidate_tree(
    scan: &ProjectScan,
    root: &Path,
    runner: &GitRunner,
) -> Result<Vec<CandidateDirectory>> {
    // Files the root repository tracks, grouped by the directory that contains them.
    let tracked_by_root = tracked_by_root_counts(scan, root, runner);
    let mut nodes = Vec::new();
    for child in &scan.tree.children {
        nodes.push(candidate_node(child, root, runner, &tracked_by_root, 1)?);
    }
    Ok(nodes)
}

fn candidate_node(
    node: &DirectoryNode,
    root: &Path,
    runner: &GitRunner,
    tracked_by_root: &[(PathBuf, usize)],
    depth: usize,
) -> Result<CandidateDirectory> {
    let relative = node.relative_path.clone();
    let absolute = root.join(&relative);
    let is_repository = node.is_repository_root;
    let has_git_dir = absolute.join(".git").exists();

    let mut children = Vec::new();
    for child in &node.children {
        children.push(candidate_node(
            child,
            root,
            runner,
            tracked_by_root,
            depth + 1,
        )?);
    }
    let subtree_files = node.file_count + children.iter().map(|c| c.subtree_files).sum::<usize>();

    let (has_commits, branch, remote) = if is_repository {
        let repo = root_for_scan(node, root);
        let repo = runner.repo(&repo);
        let has_commits = repo.head_oid()?.is_some();
        let branch = repo
            .head()
            .ok()
            .and_then(|head| head.branch().map(str::to_string));
        let remote = discovery::origin_url(&root_for_scan(node, root), runner)?;
        (has_commits, branch, remote)
    } else {
        (false, None, None)
    };

    let tracked = tracked_by_root
        .iter()
        .filter(|(dir, _)| dir == &relative)
        .map(|(_, count)| *count)
        .sum::<usize>();
    // Nested repositories are looked up on the filesystem rather than through Git: the
    // directory is about to become a repository, and asking Git for every subdirectory
    // would cost one process each. This is the only place nesting is detected for the
    // wizard, so the warning a directory gets and the warning the plan writes agree.
    let nested_here = nested_git_dirs(&absolute, NESTED_SCAN_DEPTH);

    Ok(CandidateDirectory {
        name: node.name.clone(),
        relative_path: relative,
        depth,
        files: node.file_count,
        subtree_files,
        is_repository,
        has_git_dir,
        has_commits,
        branch,
        remote,
        tracked_by_root: tracked,
        nested_repositories: nested_here,
        children,
    })
}

/// The absolute directory a scanned node describes (`node.relative_path` is project-relative).
fn root_for_scan(node: &DirectoryNode, root: &Path) -> PathBuf {
    if paths::is_root_relative(&node.relative_path) {
        root.to_path_buf()
    } else {
        root.join(&node.relative_path)
    }
}

/// How many files the root repository tracks under each directory that contains them.
///
/// Uses the same Git query GitMesh uses everywhere else (`git ls-files`), so the number
/// the wizard shows is the number the analyzer will report as an ownership warning.
fn tracked_by_root_counts(
    scan: &ProjectScan,
    root: &Path,
    runner: &GitRunner,
) -> Vec<(PathBuf, usize)> {
    let mut counts: Vec<(PathBuf, usize)> = Vec::new();
    if !scan.root_is_repository {
        return counts;
    }
    let repo = runner.repo(root);
    for entry in scan.repositories.iter().filter(|r| !r.is_project_root) {
        let Ok(files) = repo.tracked_files_under(&entry.relative_path) else {
            continue;
        };
        if files.is_empty() {
            continue;
        }
        // Attribute each tracked path to the immediate child directory the user can
        // select, so "engine/ has 2 tracked files" is shown on the row for `engine`.
        let mut per_dir: Vec<(PathBuf, usize)> = Vec::new();
        for file in files {
            let Some(parent) = file.parent() else {
                continue;
            };
            match per_dir.iter_mut().find(|(dir, _)| dir == parent) {
                Some((_, count)) => *count += 1,
                None => per_dir.push((parent.to_path_buf(), 1)),
            }
        }
        for (dir, count) in per_dir {
            match counts.iter_mut().find(|(existing, _)| existing == &dir) {
                Some((_, total)) => *total += count,
                None => counts.push((dir, count)),
            }
        }
    }
    counts.sort();
    counts
}

// ------------------------------------------------------------------ request --

/// What the user wants the project to look like.
///
/// One request produces one plan; the request is part of the plan, so re-planning an
/// unchanged request produces an identical plan (and the same [`SetupPlan::id`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupRequest {
    /// Directory that becomes the project root.
    pub root: PathBuf,
    /// Logical project name; empty means "use the directory name".
    pub name: String,
    /// Remote URL of the root repository.
    pub root_remote: Option<String>,
    /// Branch hint of the root repository.
    pub root_branch: Option<String>,
    /// Create the root repository when the directory is not one yet (`git init`).
    pub create_root_repository: bool,
    /// Configure `origin` in Git for every recorded remote.
    pub set_git_remote: bool,
    /// Replace an existing `.gitmesh/project.toml` (explicit confirmation).
    pub overwrite_manifest: bool,
    /// Replace an existing `origin` of a repository (explicit confirmation).
    pub overwrite_remotes: bool,
    /// Offer the index-only "stop tracking in the root repository" step.
    pub untrack_from_root: bool,
    /// Message for one first commit followed by a push, once the setup succeeded.
    ///
    /// `Some(message)` is a *plan* entry, not an engine action: the setup engine never
    /// commits and never pushes. It records the promise so the reviewer sees it, and
    /// [`SetupPlan::first_publish`] hands it to the caller, which runs the ordinary
    /// commit and push operations. `None` leaves the repositories untouched.
    pub publish_first_commit: Option<String>,
    /// External repositories, in the order the user configured them.
    pub repositories: Vec<RepositoryRequest>,
}

/// One external repository the user selected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryRequest {
    /// Project-relative path of the directory.
    pub path: String,
    /// Logical id; empty means "suggest one".
    pub id: String,
    /// Remote URL to record (and to configure as `origin` when allowed).
    pub remote: Option<String>,
    /// Branch hint recorded in the manifest.
    pub branch: Option<String>,
    /// Create a Git repository there when the directory is not one yet.
    pub create: bool,
    /// Stop tracking this directory in the root repository (index only).
    pub untrack_from_root: bool,
    /// Wizard-only hint used to describe a hosted remote (`private`, `public`, ...).
    ///
    /// It is never written to the manifest: the manifest only records the remote URL.
    pub visibility: Option<String>,
}

/// One first commit and push the caller runs after a successful setup.
///
/// The setup engine only *describes* this: committing and pushing are the ordinary
/// project operations, with their own staging, message and conflict handling. Keeping
/// the description in the plan means the reviewer sees exactly what will follow, and
/// the executor cannot invent a different follow-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirstPublish {
    /// Commit message for the first commit.
    pub message: String,
    /// Ids of the repositories that get a remote here, in plan order.
    pub repositories: Vec<String>,
}

impl FirstPublish {
    /// One sentence for the review step.
    pub fn sentence(&self) -> String {
        format!(
            "after the manifest is written, commit with '{}' and push {} using the \
             ordinary commit and push operations",
            self.message,
            self.repositories.join(", ")
        )
    }
}

/// Suggested manifest id for a directory, as the wizard offers it.
///
/// `MyProject` plus `engine/` suggests `myproject-engine`: the project name keeps the ids
/// readable and the uniqueness is resolved against the project being assembled, not by the
/// interface guessing.
pub fn suggested_repository_id(project: &GitMeshProject, relative: &Path) -> String {
    let base = discovery::suggest_id(relative, project);
    let prefix = id_fragment(&project.name);
    if prefix.is_empty() || base.starts_with(&format!("{prefix}-")) {
        return base;
    }
    let mut candidate = format!("{prefix}-{base}");
    let mut suffix = 2;
    while project.repository(&candidate).is_some() {
        candidate = format!("{prefix}-{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// The characters of a name that can appear in a manifest id.
fn id_fragment(name: &str) -> String {
    let lowered = name.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_dash = true;
    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '.' {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

// --------------------------------------------------------------------- plan --

/// The kind of step a plan is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStepKind {
    /// Create the `.gitmesh` metadata directory.
    CreateMetadataDir,
    /// `git init` a repository that does not exist yet.
    CreateRepository,
    /// Add or update the `origin` remote.
    ConfigureRemote,
    /// Stop tracking a directory in the root repository (files stay on disk).
    UntrackFromRoot,
    /// Write `.gitmesh/project.toml`.
    WriteManifest,
}

impl SetupStepKind {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            SetupStepKind::CreateMetadataDir => "create-metadata",
            SetupStepKind::CreateRepository => "create-repository",
            SetupStepKind::ConfigureRemote => "configure-remote",
            SetupStepKind::UntrackFromRoot => "untrack-from-root",
            SetupStepKind::WriteManifest => "write-manifest",
        }
    }

    /// Heading used in the review screen.
    pub fn heading(self) -> &'static str {
        match self {
            SetupStepKind::CreateMetadataDir => "Project metadata",
            SetupStepKind::CreateRepository => "Repositories",
            SetupStepKind::ConfigureRemote => "Remote configuration",
            SetupStepKind::UntrackFromRoot => "Repository ownership",
            SetupStepKind::WriteManifest => "Manifest",
        }
    }
}

/// Whether a step will run, is already satisfied, or cannot be planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepState {
    /// The step will be executed.
    Planned,
    /// Nothing to do; the reason explains why.
    AlreadySatisfied(String),
    /// The step cannot be planned; the reason explains what to fix.
    Blocked(String),
}

impl StepState {
    /// Stable machine-readable label.
    pub fn label(&self) -> &'static str {
        match self {
            StepState::Planned => "planned",
            StepState::AlreadySatisfied(_) => "already",
            StepState::Blocked(_) => "blocked",
        }
    }

    /// The reason attached to a non-planned state.
    pub fn reason(&self) -> Option<&str> {
        match self {
            StepState::Planned => None,
            StepState::AlreadySatisfied(reason) | StepState::Blocked(reason) => Some(reason),
        }
    }
}

/// One step of a plan: the unit of work the user reviews and the unit that is executed.
#[derive(Debug, Clone)]
pub struct SetupStep {
    /// What kind of change this is.
    pub kind: SetupStepKind,
    /// Repository id the step belongs to (`manifest` for the manifest itself).
    pub target: String,
    /// Project-relative path the step affects.
    pub path: String,
    /// Human sentence used in the review screen.
    pub detail: String,
    /// Whether the step runs or not.
    pub state: StepState,
}

impl SetupStep {
    /// True when this step will be executed.
    pub fn planned(&self) -> bool {
        self.state == StepState::Planned
    }
}

/// What happens to a repository's `origin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteAction {
    /// No remote at all: the repository stays local-only.
    None,
    /// An `origin` is added.
    Add,
    /// An existing `origin` is replaced (explicitly confirmed).
    Update,
    /// The existing `origin` is already what the user wants (or is left alone).
    Keep,
    /// The remote is recorded in the manifest only; Git itself is not touched.
    ///
    /// That is what "record the remote, do not configure Git" means (the wizard's
    /// "configure remotes" checkbox, `gitmesh init --add-git-remote` on the command line).
    /// Nothing about the repository changes, so a record-only remote is not a step and not
    /// something the first publish can push to.
    Record,
}

impl RemoteAction {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            RemoteAction::None => "none",
            RemoteAction::Add => "add",
            RemoteAction::Update => "update",
            RemoteAction::Keep => "keep",
            RemoteAction::Record => "record",
        }
    }
}

/// One repository as the plan sees it: current facts plus what will be done.
#[derive(Debug, Clone)]
pub struct PlannedRepository {
    /// Logical id.
    pub id: String,
    /// Project-relative path.
    pub path: String,
    /// Root or external.
    pub role: RepositoryRole,
    /// Remote recorded in the manifest.
    pub remote: Option<String>,
    /// Provider id when the remote belongs to a known provider (`github`).
    pub provider: Option<String>,
    /// Parsed coordinates of a hosted remote.
    pub hosted: Option<RemoteRef>,
    /// Wizard hint about visibility (never stored in the manifest).
    pub visibility: Option<String>,
    /// True when the directory exists.
    pub exists: bool,
    /// True when the directory already is a Git repository.
    pub is_repository: bool,
    /// True when the directory has at least one commit.
    pub has_commits: bool,
    /// Branch currently checked out (empty repositories have none).
    pub branch: Option<String>,
    /// `origin` found on disk, if any.
    pub current_origin: Option<String>,
    /// True when the plan creates the repository (`git init`).
    pub create: bool,
    /// What happens to `origin`.
    pub remote_action: RemoteAction,
    /// Files the root repository tracks inside this directory.
    pub tracked_by_root: usize,
    /// True when the plan untracks those files from the root repository.
    pub untrack: bool,
}

impl PlannedRepository {
    /// A sentence the user can read, e.g. `engine/ → engine (git init, origin added)`.
    pub fn sentence(&self) -> String {
        let mut actions: Vec<String> = Vec::new();
        match (&self.create, self.is_repository) {
            (true, false) => actions.push("create a Git repository".to_string()),
            (_, true) if self.exists => actions.push("use the existing repository".to_string()),
            _ => actions.push("configure the directory".to_string()),
        }
        match (self.remote_action, self.remote.as_deref()) {
            (RemoteAction::Add, Some(url)) => actions.push(format!("set origin {url}")),
            (RemoteAction::Update, Some(url)) => actions.push(format!("replace origin with {url}")),
            (RemoteAction::Keep, Some(url)) => actions.push(format!("keep origin {url}")),
            (RemoteAction::Record, Some(url)) => actions.push(format!(
                "record origin {url} in the manifest only, without touching Git"
            )),
            (RemoteAction::None, _) => actions.push("no remote".to_string()),
            (action, None) => actions.push(format!("remote {}", action.label())),
        }
        if self.untrack {
            actions.push("stop tracking it in the root repository".to_string());
        }
        format!("{} → {} ({})", self.path, self.id, actions.join(", "))
    }

    /// True when the repository has work planned for it.
    ///
    /// A remote that is only recorded in the manifest is not work: nothing runs for it.
    pub fn will_change(&self) -> bool {
        self.create
            || matches!(self.remote_action, RemoteAction::Add | RemoteAction::Update)
            || self.untrack
    }
}

/// The complete, reviewable description of a project setup.
///
/// The same value drives the review screen and the execution: [`apply`] replays
/// `steps` and nothing else.
#[derive(Debug, Clone)]
pub struct SetupPlan {
    /// Fingerprint of everything the plan contains (see [`SetupPlan::fingerprint`]).
    pub id: String,
    /// The request this plan answers.
    pub request: SetupRequest,
    /// Configuration that will be written.
    pub project: GitMeshProject,
    /// Project name.
    pub name: String,
    /// Absolute project root.
    pub root: PathBuf,
    /// Where the manifest will be written.
    pub manifest_path: PathBuf,
    /// Every repository, planned or already present.
    pub repositories: Vec<PlannedRepository>,
    /// Ordered steps.
    pub steps: Vec<SetupStep>,
    /// Reasons the plan cannot be applied.
    pub blockers: Vec<String>,
    /// Things the user should know; none of them stop the setup.
    pub warnings: Vec<String>,
    /// Neutral facts about the current directory.
    pub notices: Vec<String>,
    /// Exact manifest text.
    pub manifest: String,
    /// Human-readable safety statements, computed from the steps.
    pub safety: Vec<String>,
    /// Ids whose recorded remote the plan guarantees is configured as `origin` when it is
    /// done (the ones it adds, updates, or finds already pointing where the manifest says).
    ///
    /// A remote that is only recorded is not an expectation for Git, so validation never
    /// turns "the user asked for no Git configuration" into a failure.
    pub expected_origins: Vec<String>,
}

impl SetupPlan {
    /// The first commit and push that should follow a successful `apply`.
    ///
    /// `None` when the request did not ask for one, or when no repository gets a remote
    /// here (there would be nothing to publish). The engine never performs it.
    pub fn first_publish(&self) -> Option<FirstPublish> {
        let message = self.request.publish_first_commit.clone()?;
        let repositories: Vec<String> = self
            .repositories
            .iter()
            .filter(|repo| repo.remote_action == RemoteAction::Add)
            .map(|repo| repo.id.clone())
            .collect();
        if repositories.is_empty() {
            return None;
        }
        Some(FirstPublish {
            message: message.trim().to_string(),
            repositories,
        })
    }

    /// True when nothing stops the plan from being applied.
    pub fn is_ready(&self) -> bool {
        self.blockers.is_empty()
    }

    /// Steps that will actually run.
    pub fn planned_steps(&self) -> impl Iterator<Item = &SetupStep> {
        self.steps.iter().filter(|step| step.planned())
    }

    /// Steps that are already satisfied.
    pub fn already_satisfied(&self) -> impl Iterator<Item = &SetupStep> {
        self.steps
            .iter()
            .filter(|step| matches!(step.state, StepState::AlreadySatisfied(_)))
    }

    /// Steps that cannot run.
    pub fn blocked_steps(&self) -> impl Iterator<Item = &SetupStep> {
        self.steps
            .iter()
            .filter(|step| matches!(step.state, StepState::Blocked(_)))
    }

    /// True when applying this plan would change nothing.
    pub fn is_noop(&self) -> bool {
        self.planned_steps().next().is_none()
    }

    /// Steps of one kind, grouped for the review screen.
    pub fn steps_of(&self, kind: SetupStepKind) -> impl Iterator<Item = &SetupStep> {
        self.steps.iter().filter(move |step| step.kind == kind)
    }

    /// Repositories that are created by this plan.
    pub fn created_repositories(&self) -> impl Iterator<Item = &PlannedRepository> {
        self.repositories.iter().filter(|repo| repo.create)
    }

    /// Fingerprint of the plan: request, steps, blockers and manifest text.
    ///
    /// The front end sends it back when the user confirms, so a plan that changed on
    /// disk between the review and the confirmation can never be executed silently:
    /// the identifiers differ and the user is asked to review again.
    pub fn fingerprint(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        parts.push(format!("root={}", to_slash(&self.root)));
        parts.push(format!("name={}", self.name));
        for repo in &self.repositories {
            parts.push(format!(
                "repo={}|{}|{}|{}|{}|{}",
                repo.id,
                repo.path,
                repo.remote.clone().unwrap_or_default(),
                repo.create,
                repo.remote_action.label(),
                repo.untrack
            ));
        }
        for step in &self.steps {
            parts.push(format!(
                "step={}|{}|{}|{}|{}",
                step.kind.label(),
                step.target,
                step.path,
                step.state.label(),
                step.state.reason().unwrap_or("")
            ));
        }
        for blocker in &self.blockers {
            parts.push(format!("blocker={blocker}"));
        }
        parts.push(format!(
            "publish={}",
            self.request
                .publish_first_commit
                .clone()
                .unwrap_or_default()
        ));
        parts.push(format!("manifest={}", self.manifest));
        fnv1a(&parts.join("\n"))
    }

    /// Short human-readable summary of what will happen.
    pub fn summary(&self) -> String {
        let planned = self.planned_steps().count();
        let already = self.already_satisfied().count();
        let blocked = self.blocked_steps().count();
        if !self.is_ready() {
            return format!("{blocked} problem(s) must be fixed before the project can be created");
        }
        if planned == 0 {
            return "nothing to do: the project is already set up as requested".to_string();
        }
        let mut parts = vec![format!("{planned} change(s)")];
        if already > 0 {
            parts.push(format!("{already} already in place"));
        }
        let created = self.created_repositories().count();
        if created > 0 {
            parts.push(format!("{created} repository(ies) created"));
        }
        parts.join(", ")
    }
}

/// FNV-1a, 64 bit, hex. Deterministic and dependency-free: the identifier only has to be
/// stable for the lifetime of a plan.
fn fnv1a(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

// ------------------------------------------------------------------ planning --

/// Turn a request into a plan. Never writes anything.
pub fn plan(request: &SetupRequest, runner: &GitRunner) -> Result<SetupPlan> {
    let root = paths::lexical_normalize(&paths::absolute(&request.root)?);
    if !root.is_dir() {
        return Err(Error::Other(format!(
            "{} is not a directory",
            root.display()
        )));
    }

    let name = resolve_name(&request.name, &root);
    // "Record the remote, do not configure Git" is a request-level decision, so it is
    // resolved once here and every repository follows it.
    let configure_remotes = request.set_git_remote;
    let mut blockers: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut notices: Vec<String> = Vec::new();

    // A first commit without a message would silently become a default, so the plan
    // refuses instead: the message is part of what the user reviews.
    if let Some(message) = &request.publish_first_commit {
        if message.trim().is_empty() {
            blockers.push(
                "the first commit needs a message: type one, or turn off 'commit and push \
                 after setup'"
                    .to_string(),
            );
        }
    }

    // ---- root repository -------------------------------------------------
    // `git init` never runs on an existing repository; the check is here so the plan can
    // say "already a repository" instead of planning a no-op step.
    let root_is_repository = discovery::is_repository_root(&root, true, runner);
    let root_origin = if root_is_repository {
        discovery::origin_url(&root, runner)?
    } else {
        None
    };
    // A directory that is inside another repository can still become a repository; Git
    // allows the nesting and GitMesh reports it. It is worth a warning, not a refusal.
    if !root_is_repository {
        if let Some(enclosing) = runner.repo(&root).top_level()? {
            let enclosing = paths::lexical_normalize(&enclosing);
            if enclosing != root {
                warnings.push(format!(
                    "the project root sits inside the Git repository at {}; the root \
                     repository will be nested inside it and GitMesh will not touch that \
                     repository",
                    enclosing.display()
                ));
            }
        }
    }
    if request.create_root_repository && root_is_repository {
        notices.push(
            "the project root is already a Git repository; it is used as it is and never \
             re-initialised"
                .to_string(),
        );
    }

    let root_remote = clean_url(request.root_remote.as_deref());
    if request.root_remote.is_some() && root_remote.is_none() {
        blockers.push("the root repository remote is empty".to_string());
    }

    let mut project = GitMeshProject {
        name: name.clone(),
        root: root.clone(),
        repositories: vec![PhysicalRepository {
            id: "root".to_string(),
            role: RepositoryRole::Root,
            relative_path: PathBuf::from("."),
            remote_url: root_remote.clone().or_else(|| root_origin.clone()),
            branch: clean_text(request.root_branch.as_deref()),
            absolute_path: root.clone(),
        }],
    };

    let mut repositories: Vec<PlannedRepository> = vec![PlannedRepository {
        id: "root".to_string(),
        path: ".".to_string(),
        role: RepositoryRole::Root,
        remote: project.repositories[0].remote_url.clone(),
        provider: provider_id(project.repositories[0].remote_url.as_deref()),
        hosted: hosted_ref(project.repositories[0].remote_url.as_deref()),
        visibility: None,
        exists: true,
        is_repository: root_is_repository,
        has_commits: if root_is_repository {
            runner.repo(&root).head_oid()?.is_some()
        } else {
            false
        },
        branch: if root_is_repository {
            runner
                .repo(&root)
                .head()
                .ok()
                .and_then(|head| head.branch().map(str::to_string))
        } else {
            None
        },
        current_origin: root_origin.clone(),
        create: request.create_root_repository && !root_is_repository,
        remote_action: planned_remote_action(
            configure_remotes,
            project.repositories[0].remote_url.as_deref(),
            root_origin.as_deref(),
        ),
        tracked_by_root: 0,
        untrack: false,
    }];

    if !root_is_repository && !request.create_root_repository {
        warnings.push(
            "the project root is not a Git repository and the plan does not create one; \
             GitMesh will not be able to commit in the root repository until it exists"
                .to_string(),
        );
    }
    if matches!(repositories[0].remote_action, RemoteAction::Update) && !request.overwrite_remotes {
        blockers.push(format!(
            "the root repository already has origin '{}' and the request asks for '{}'; \
             replacing a remote must be confirmed explicitly",
            root_origin.clone().unwrap_or_default(),
            root_remote.clone().unwrap_or_default()
        ));
    }
    if let Some((wanted, existing)) = record_only_conflict(
        configure_remotes,
        root_remote.as_deref(),
        root_origin.as_deref(),
    ) {
        blockers.push(format!(
            "the root repository already has origin '{existing}' and the request records \
             '{wanted}' without configuring Git; enable remote configuration, or record \
             the URL origin already has"
        ));
    }

    // ---- external repositories ------------------------------------------
    // Selections that cannot be planned at all: they are reported as blocked steps so
    // the review screen shows exactly which row is the problem.
    let mut blocked_selections: Vec<SetupStep> = Vec::new();
    for (index, entry) in request.repositories.iter().enumerate() {
        let label = if entry.path.trim().is_empty() {
            format!("repository #{index}")
        } else {
            entry.path.trim().to_string()
        };
        let refuse = |reason: String, blocked: &mut Vec<SetupStep>| {
            blocked.push(SetupStep {
                kind: SetupStepKind::CreateRepository,
                target: label.clone(),
                path: label.clone(),
                detail: format!("'{label}' cannot be set up as a repository"),
                state: StepState::Blocked(reason.clone()),
            });
            reason
        };
        let relative = match paths::normalize_relative(entry.path.trim()) {
            Ok(path) => path,
            Err(err) => {
                blockers.push(refuse(format!("'{label}': {err}"), &mut blocked_selections));
                continue;
            }
        };
        let path_label = to_slash(&relative);

        let conflicts = discovery::assignment_conflicts(&project, &relative);
        if !conflicts.is_empty() {
            let first = conflicts.first().cloned().unwrap_or_default();
            blockers.extend(conflicts);
            refuse(first, &mut blocked_selections);
            continue;
        }

        let absolute = root.join(&relative);
        let exists = absolute.exists();
        if !exists {
            blockers.push(refuse(
                format!("directory '{path_label}' does not exist inside the project root"),
                &mut blocked_selections,
            ));
            continue;
        }
        if !absolute.is_dir() {
            blockers.push(refuse(
                format!("'{path_label}' is not a directory"),
                &mut blocked_selections,
            ));
            continue;
        }

        let is_repository = discovery::is_repository_root(&absolute, true, runner);
        let repo_query = runner.repo(&absolute);
        let has_commits = is_repository && repo_query.head_oid()?.is_some();
        let branch = if is_repository {
            repo_query
                .head()
                .ok()
                .and_then(|head| head.branch().map(str::to_string))
        } else {
            None
        };
        let current_origin = if is_repository {
            discovery::origin_url(&absolute, runner)?
        } else {
            None
        };

        let requested_remote = clean_url(entry.remote.as_deref());
        if entry.remote.is_some() && requested_remote.is_none() {
            blockers.push(refuse(
                format!("'{path_label}': the remote URL is empty"),
                &mut blocked_selections,
            ));
            continue;
        }
        let remote = requested_remote.clone().or_else(|| current_origin.clone());
        let action = planned_remote_action(
            configure_remotes,
            remote.as_deref(),
            current_origin.as_deref(),
        );
        if action == RemoteAction::Update && !request.overwrite_remotes {
            blockers.push(format!(
                "'{path_label}': origin currently points at '{}' and the request asks for \
                 '{}'; replacing a remote must be confirmed explicitly",
                current_origin.clone().unwrap_or_default(),
                requested_remote.clone().unwrap_or_default()
            ));
        }
        if let Some((wanted, existing)) = record_only_conflict(
            configure_remotes,
            requested_remote.as_deref(),
            current_origin.as_deref(),
        ) {
            blockers.push(format!(
                "'{path_label}': origin currently points at '{existing}' and the request \
                 records '{wanted}' without configuring Git; enable remote configuration, \
                 or record the URL origin already has"
            ));
        }

        let id = {
            let requested = entry.id.trim().to_string();
            if requested.is_empty() {
                discovery::suggest_id(&relative, &project)
            } else {
                requested
            }
        };
        let mut id_problems: Vec<String> = Vec::new();
        if project.repository(&id).is_some() {
            id_problems.push(format!(
                "the id '{id}' is already used by another repository in this project"
            ));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            id_problems.push(format!(
                "repository id '{id}' contains unsupported characters (use letters, \
                 digits, '-', '_' or '.')"
            ));
        }
        if !id_problems.is_empty() {
            let first = id_problems.first().cloned().unwrap_or_default();
            blockers.extend(id_problems);
            refuse(first, &mut blocked_selections);
            continue;
        }

        let tracked_by_root = count_files_tracked_by_root(&root, &relative, runner);
        let untrack = entry.untrack_from_root && request.untrack_from_root && tracked_by_root > 0;
        if tracked_by_root > 0 && !untrack {
            warnings.push(format!(
                "the root repository still tracks {tracked_by_root} file(s) inside \
                 '{path_label}'; the same files would then belong to two repositories \
                 (enable \"stop tracking in the root repository\" to fix that without \
                 deleting anything)"
            ));
        }

        let create = entry.create && !is_repository;
        if !is_repository && !entry.create {
            warnings.push(format!(
                "'{path_label}' is not a Git repository yet and the plan does not create \
                 one; it will be recorded as a repository, but GitMesh will report it as \
                 unavailable until a repository exists there"
            ));
        }
        if is_repository && entry.create {
            notices.push(format!(
                "'{path_label}' is already a Git repository; it is used as it is and never \
                 re-initialised"
            ));
        }
        for nested in nested_git_dirs(&absolute, NESTED_SCAN_DEPTH) {
            warnings.push(format!(
                "'{path_label}' contains another Git repository at '{path_label}/{}'; GitMesh \
                 does not manage nested repositories and will not touch it",
                to_slash(&nested)
            ));
        }

        project.repositories.push(PhysicalRepository {
            id: id.clone(),
            role: RepositoryRole::External,
            relative_path: relative.clone(),
            remote_url: remote.clone(),
            branch: clean_text(entry.branch.as_deref()),
            absolute_path: absolute.clone(),
        });
        repositories.push(PlannedRepository {
            id,
            path: path_label,
            role: RepositoryRole::External,
            remote,
            provider: provider_id(requested_remote.as_deref()),
            hosted: hosted_ref(requested_remote.as_deref()),
            visibility: clean_text(entry.visibility.as_deref()),
            exists,
            is_repository,
            has_commits,
            branch,
            current_origin,
            create,
            remote_action: action,
            tracked_by_root,
            untrack,
        });
    }

    // ---- the configuration itself --------------------------------------
    // The candidate project is validated with the same rules a manifest loaded from disk
    // gets, and the manifest text is produced once, here, so the preview shows exactly
    // what will be written.
    project
        .repositories
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    if let Err(issues) = manifest::validation::validate_project(&project) {
        blockers.extend(issues);
    }
    let manifest_path = manifest::manifest_path(&root);
    let manifest = manifest::render_manifest(&project)?;
    if let Err(err) = manifest::parse_manifest(&manifest, &root, &manifest_path) {
        blockers.push(format!("the generated configuration is not valid: {err}"));
    }

    let metadata_dir = manifest::metadata_dir(&root);
    let existing_manifest = std::fs::read_to_string(&manifest_path).ok();
    if let Some(existing) = &existing_manifest {
        if existing != &manifest && !request.overwrite_manifest {
            blockers.push(format!(
                "{} already exists and differs from the requested configuration; replacing \
                 it must be confirmed explicitly",
                manifest_path.display()
            ));
        }
        if existing == &manifest {
            notices.push("the manifest on disk already matches this configuration".to_string());
        }
    }

    // ---- steps -----------------------------------------------------------
    let mut steps: Vec<SetupStep> = Vec::new();
    steps.push(SetupStep {
        kind: SetupStepKind::CreateMetadataDir,
        target: "manifest".to_string(),
        path: format!("{}/", manifest::METADATA_DIR),
        detail: format!("create {}", readable(&metadata_dir, &root)),
        state: if metadata_dir.is_dir() {
            StepState::AlreadySatisfied(format!("{} already exists", manifest::METADATA_DIR))
        } else {
            StepState::Planned
        },
    });

    for repo in &repositories {
        let label = if repo.role == RepositoryRole::Root {
            "the project root".to_string()
        } else {
            format!("'{}'", repo.path)
        };
        if repo.create {
            steps.push(SetupStep {
                kind: SetupStepKind::CreateRepository,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!("create a Git repository in {label} (git init -b main)"),
                state: StepState::Planned,
            });
        } else if repo.is_repository {
            steps.push(SetupStep {
                kind: SetupStepKind::CreateRepository,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!("use the existing Git repository in {label}"),
                state: StepState::AlreadySatisfied(
                    "a Git repository already exists; it is never re-initialised".to_string(),
                ),
            });
        } else {
            steps.push(SetupStep {
                kind: SetupStepKind::CreateRepository,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!("no Git repository in {label}"),
                state: StepState::AlreadySatisfied(
                    "the plan does not create a repository here".to_string(),
                ),
            });
        }

        if !repo.is_repository && !repo.create {
            // Nothing to configure on a directory that is not a repository yet.
            continue;
        }
        match repo.remote_action {
            RemoteAction::Add => steps.push(SetupStep {
                kind: SetupStepKind::ConfigureRemote,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!(
                    "add origin {} for {label}",
                    repo.remote.clone().unwrap_or_default()
                ),
                state: StepState::Planned,
            }),
            RemoteAction::Update => steps.push(SetupStep {
                kind: SetupStepKind::ConfigureRemote,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!(
                    "replace origin {} → {} for {label} (explicitly confirmed)",
                    repo.current_origin.clone().unwrap_or_default(),
                    repo.remote.clone().unwrap_or_default()
                ),
                state: if request.overwrite_remotes {
                    StepState::Planned
                } else {
                    StepState::Blocked(
                        "replacing an existing remote must be confirmed explicitly".to_string(),
                    )
                },
            }),
            RemoteAction::Keep => steps.push(SetupStep {
                kind: SetupStepKind::ConfigureRemote,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!(
                    "keep origin {} for {label}",
                    repo.current_origin.clone().unwrap_or_default()
                ),
                state: StepState::AlreadySatisfied(
                    "the configured remote already matches".to_string(),
                ),
            }),
            // Nothing runs for a record-only remote: the manifest is the only place it
            // appears, and the review screen says so through the repository sentence.
            RemoteAction::Record | RemoteAction::None => {}
        }

        if repo.untrack {
            steps.push(SetupStep {
                kind: SetupStepKind::UntrackFromRoot,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!(
                    "stop tracking {} file(s) of '{}' in the root repository (the files stay \
                     on disk)",
                    repo.tracked_by_root, repo.path
                ),
                state: if repository_created_by(&steps, &repo.id) || root_is_repository {
                    StepState::Planned
                } else {
                    StepState::Blocked(
                        "the root repository would have to exist to stop tracking files"
                            .to_string(),
                    )
                },
            });
        } else if repo.tracked_by_root > 0 {
            steps.push(SetupStep {
                kind: SetupStepKind::UntrackFromRoot,
                target: repo.id.clone(),
                path: repo.path.clone(),
                detail: format!(
                    "{} file(s) inside '{}' stay tracked by the root repository",
                    repo.tracked_by_root, repo.path
                ),
                state: StepState::AlreadySatisfied(
                    "the root repository keeps tracking these files".to_string(),
                ),
            });
        }
    }

    // Selections that could not be planned, reported exactly like the other steps.
    steps.extend(blocked_selections);

    steps.push(SetupStep {
        kind: SetupStepKind::WriteManifest,
        target: "manifest".to_string(),
        path: format!("{}/{}", manifest::METADATA_DIR, manifest::MANIFEST_FILE),
        detail: match &existing_manifest {
            Some(existing) if existing == &manifest => {
                format!("keep {}", readable(&manifest_path, &root))
            }
            Some(_) => format!(
                "replace {} with the requested configuration (explicitly confirmed)",
                readable(&manifest_path, &root)
            ),
            None => format!("write {}", readable(&manifest_path, &root)),
        },
        state: if existing_manifest.as_deref() == Some(manifest.as_str()) {
            StepState::AlreadySatisfied("the manifest on disk is already up to date".to_string())
        } else if existing_manifest.is_some() && !request.overwrite_manifest {
            StepState::Blocked(
                "an existing manifest would be replaced without confirmation".to_string(),
            )
        } else {
            StepState::Planned
        },
    });

    if !configure_remotes
        && repositories
            .iter()
            .any(|repo| repo.remote_action == RemoteAction::Record)
    {
        warnings.push(
            "remotes are recorded in the manifest but not configured in Git: GitMesh will \
             not run `git remote add` (turn \"configure remotes\" on, or add the remote \
             yourself) until you ask for it"
                .to_string(),
        );
    }

    let safety = safety_statements(&steps, &repositories, &existing_manifest, &manifest);
    // Remotes the setup is responsible for: the ones it configures, and the ones that
    // already point where the manifest says. A record-only remote is not one of them.
    let expected_origins: Vec<String> = repositories
        .iter()
        .filter(|repo| {
            repo.remote.is_some()
                && matches!(
                    repo.remote_action,
                    RemoteAction::Add | RemoteAction::Update | RemoteAction::Keep
                )
        })
        .map(|repo| repo.id.clone())
        .collect();
    let mut plan = SetupPlan {
        id: String::new(),
        request: request.clone(),
        project,
        name,
        root,
        manifest_path,
        repositories,
        steps,
        blockers,
        warnings,
        notices,
        manifest,
        safety,
        expected_origins,
    };
    plan.id = plan.fingerprint();
    Ok(plan)
}

fn repository_created_by(steps: &[SetupStep], id: &str) -> bool {
    steps.iter().any(|step| {
        step.kind == SetupStepKind::CreateRepository
            && step.target == id
            && step.state == StepState::Planned
    })
}

/// Whether the plan touches an existing `origin` or manifest, expressed for humans.
fn safety_statements(
    steps: &[SetupStep],
    repositories: &[PlannedRepository],
    existing_manifest: &Option<String>,
    manifest: &str,
) -> Vec<String> {
    let mut lines = vec![
        "no existing .git directory is deleted or re-initialised".to_string(),
        "no file is moved, renamed or deleted".to_string(),
    ];
    let updates: Vec<&str> = repositories
        .iter()
        .filter(|repo| repo.remote_action == RemoteAction::Update)
        .map(|repo| repo.id.as_str())
        .collect();
    let recorded: Vec<&str> = repositories
        .iter()
        .filter(|repo| repo.remote_action == RemoteAction::Record)
        .map(|repo| repo.id.as_str())
        .collect();
    if !recorded.is_empty() {
        lines.push(format!(
            "the remote of {} is written to the manifest only: no repository's Git \
             configuration is touched",
            recorded.join(", ")
        ));
    }
    if updates.is_empty() {
        lines.push("no existing remote is modified".to_string());
    } else {
        lines.push(format!(
            "the origin of {} is replaced, and only because you confirmed it",
            updates.join(", ")
        ));
    }
    let untracked: usize = steps
        .iter()
        .filter(|step| step.kind == SetupStepKind::UntrackFromRoot && step.planned())
        .count();
    if untracked > 0 {
        lines.push(
            "files untracked from the root repository stay on disk and in the external \
             repository; only the root repository's index changes"
                .to_string(),
        );
    }
    match existing_manifest {
        Some(existing) if existing != manifest => lines.push(
            "the existing manifest is replaced, and only because you confirmed it".to_string(),
        ),
        _ => lines.push("an existing manifest is never replaced without confirmation".to_string()),
    }
    lines
}

// ---------------------------------------------------------------- execution --

/// Outcome of one step after execution.
#[derive(Debug, Clone)]
pub struct SetupStepOutcome {
    /// Kind of the step.
    pub kind: SetupStepKind,
    /// Repository id, or `manifest`.
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

impl SetupStepOutcome {
    fn new(step: &SetupStep, outcome: OutcomeKind, summary: impl Into<String>) -> Self {
        SetupStepOutcome {
            kind: step.kind,
            target: step.target.clone(),
            path: step.path.clone(),
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

    /// Symbol for the summary line (same alphabet as every other GitMesh operation).
    pub fn symbol(&self) -> &'static str {
        self.outcome.symbol()
    }

    /// One summary line.
    pub fn line(&self) -> String {
        format!("{} {:<20} {}", self.symbol(), self.target, self.summary)
    }
}

/// How a setup ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupKind {
    /// Every step succeeded or was already satisfied.
    Complete,
    /// Part of the project was created and something failed.
    Partial,
    /// Nothing could be created.
    Failed,
}

impl SetupKind {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            SetupKind::Complete => "complete",
            SetupKind::Partial => "partial",
            SetupKind::Failed => "failed",
        }
    }

    /// Sentence used in the result panel.
    pub fn sentence(self) -> &'static str {
        match self {
            SetupKind::Complete => "Project setup completed",
            SetupKind::Partial => "Project setup completed with errors",
            SetupKind::Failed => "Project setup failed",
        }
    }
}

/// One configured repository after the setup, as verified on disk.
#[derive(Debug, Clone)]
pub struct RepositoryCheck {
    /// Logical id.
    pub id: String,
    /// Project-relative path.
    pub path: String,
    /// Root or external.
    pub role: RepositoryRole,
    /// True when the directory exists.
    pub exists: bool,
    /// True when the directory is a Git repository.
    pub is_repository: bool,
    /// Remote recorded in the manifest.
    pub manifest_remote: Option<String>,
    /// `origin` found in Git.
    pub origin: Option<String>,
    /// True when Git's `origin` matches the manifest (or the manifest records none).
    pub remote_ok: bool,
    /// Branch checked out, when the repository has one.
    pub branch: Option<String>,
    /// Problems found with this repository.
    pub issues: Vec<String>,
}

impl RepositoryCheck {
    /// True when the repository is usable and matches the configuration.
    pub fn is_ok(&self) -> bool {
        self.issues.is_empty()
    }
}

/// Result of re-opening a project through the normal GitMesh opening path.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    /// True when the project opens and every repository checks out.
    pub ok: bool,
    /// Project root that was verified.
    pub root: PathBuf,
    /// Manifest that was read.
    pub manifest_path: PathBuf,
    /// Project name, when it could be read.
    pub project_name: Option<String>,
    /// One entry per configured repository.
    pub repositories: Vec<RepositoryCheck>,
    /// Problems, phrased as things to fix.
    pub issues: Vec<String>,
}

/// Result of applying a plan.
#[derive(Debug, Clone)]
pub struct SetupResult {
    /// Fingerprint of the executed plan.
    pub plan_id: String,
    /// True when nothing was changed.
    pub dry_run: bool,
    /// Complete, partial or failed.
    pub kind: SetupKind,
    /// One outcome per step, in execution order.
    pub outcomes: Vec<SetupStepOutcome>,
    /// The configured project, as written.
    pub project: Option<GitMeshProject>,
    /// Manifest that was written (absent for a dry run).
    pub manifest_path: Option<PathBuf>,
    /// Validation of the resulting project.
    pub validation: Option<ValidationReport>,
    /// Why the plan was refused, when it was.
    pub refused: Vec<String>,
}

impl SetupResult {
    /// True when nothing failed.
    pub fn is_success(&self) -> bool {
        self.kind == SetupKind::Complete
    }

    /// Exit code, following the GitMesh contract (0 = ok, 1 = something failed).
    pub fn exit_code(&self) -> u8 {
        match self.kind {
            SetupKind::Complete => 0,
            SetupKind::Partial | SetupKind::Failed => 1,
        }
    }

    /// Number of steps that succeeded.
    pub fn succeeded(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.outcome == OutcomeKind::Success)
            .count()
    }

    /// Number of steps that had nothing to do.
    pub fn skipped(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.outcome == OutcomeKind::Skipped)
            .count()
    }

    /// Steps that failed.
    pub fn failures(&self) -> impl Iterator<Item = &SetupStepOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.outcome == OutcomeKind::Failed)
    }

    /// Summary sentence of the whole setup.
    pub fn summary(&self) -> String {
        let mut parts = vec![self.kind.sentence().to_string()];
        if self.dry_run {
            parts.push("dry run: nothing was changed".to_string());
        }
        parts.push(format!("{} done", self.succeeded()));
        if self.skipped() > 0 {
            parts.push(format!("{} already in place", self.skipped()));
        }
        let failures = self.failures().count();
        if failures > 0 {
            parts.push(format!("{failures} failed"));
        }
        parts.join(" · ")
    }
}

/// Progress hooks for a setup, mirroring [`crate::ops::OperationObserver`].
///
/// A setup is not a loop over repositories (it also creates the metadata directory and
/// writes the manifest), so it reports through its own observer; front ends that only
/// need the result use [`SetupObserver::silent`].
#[derive(Default)]
pub struct SetupObserver<'a> {
    on_start: Option<&'a mut dyn FnMut(&SetupStep)>,
    on_end: Option<&'a mut dyn FnMut(&SetupStepOutcome)>,
}

impl<'a> SetupObserver<'a> {
    /// An observer that does nothing.
    pub fn silent() -> Self {
        SetupObserver::default()
    }

    /// Called before a step runs.
    pub fn on_step<F>(mut self, f: &'a mut F) -> Self
    where
        F: FnMut(&SetupStep),
    {
        self.on_start = Some(f);
        self
    }

    /// Called when a step has its outcome.
    pub fn on_outcome<F>(mut self, f: &'a mut F) -> Self
    where
        F: FnMut(&SetupStepOutcome),
    {
        self.on_end = Some(f);
        self
    }

    /// Notify the observer that a step is starting.
    pub fn step_started(&mut self, step: &SetupStep) {
        if let Some(f) = self.on_start.as_mut() {
            f(step);
        }
    }

    /// Notify the observer that a step finished.
    pub fn step_finished(&mut self, outcome: &SetupStepOutcome) {
        if let Some(f) = self.on_end.as_mut() {
            f(outcome);
        }
    }
}

/// Execute a plan.
///
/// * a plan with blockers is refused as a whole — nothing is written, and every blocker
///   is returned in [`SetupResult::refused`];
/// * a step that is already satisfied is reported as skipped and runs nothing;
/// * a step that fails does not stop the others, as long as the remaining steps do not
///   depend on it (a repository whose creation failed has its remote step skipped);
/// * the manifest is written whenever the metadata directory is usable, because a
///   partially created project that is honest about itself is more useful than an
///   unconfigured directory — and validation reports exactly what is missing.
pub fn apply(
    plan: &SetupPlan,
    dry_run: bool,
    runner: &GitRunner,
    observer: &mut SetupObserver<'_>,
) -> SetupResult {
    let mut outcomes: Vec<SetupStepOutcome> = Vec::new();
    let mut failed_repositories: Vec<String> = Vec::new();
    let mut manifest_written: Option<PathBuf> = None;

    if !plan.is_ready() {
        return SetupResult {
            plan_id: plan.id.clone(),
            dry_run,
            kind: SetupKind::Failed,
            outcomes: Vec::new(),
            project: None,
            manifest_path: None,
            validation: None,
            refused: plan.blockers.clone(),
        };
    }

    for step in &plan.steps {
        match &step.state {
            StepState::AlreadySatisfied(reason) => {
                observer.step_started(step);
                let outcome = SetupStepOutcome::new(step, OutcomeKind::Skipped, reason.clone());
                observer.step_finished(&outcome);
                outcomes.push(outcome);
                continue;
            }
            StepState::Blocked(reason) => {
                observer.step_started(step);
                let outcome = SetupStepOutcome::new(step, OutcomeKind::Failed, reason.clone());
                observer.step_finished(&outcome);
                outcomes.push(outcome);
                continue;
            }
            StepState::Planned => {}
        }

        observer.step_started(step);

        // A repository whose creation failed cannot have its remote configured, and the
        // manifest is only written once, at the end of the plan.
        if matches!(
            step.kind,
            SetupStepKind::ConfigureRemote | SetupStepKind::UntrackFromRoot
        ) && failed_repositories.contains(&step.target)
        {
            let outcome = SetupStepOutcome::new(
                step,
                OutcomeKind::Skipped,
                format!(
                    "skipped because repository '{}' could not be created",
                    step.target
                ),
            );
            observer.step_finished(&outcome);
            outcomes.push(outcome);
            continue;
        }

        if dry_run {
            let outcome = SetupStepOutcome::new(
                step,
                OutcomeKind::Skipped,
                format!("dry run: {}", step.detail),
            );
            observer.step_finished(&outcome);
            outcomes.push(outcome);
            continue;
        }

        // One step at a time, exactly as the user reviewed them: a failing step never
        // stops the others, and a repository whose creation failed only has its own
        // follow-up steps skipped.
        let result: Result<SetupStepOutcome> = match step.kind {
            SetupStepKind::CreateMetadataDir => {
                let dir = manifest::metadata_dir(&plan.root);
                std::fs::create_dir_all(&dir)
                    .map_err(|source| Error::io(dir.clone(), source))
                    .map(|_| {
                        SetupStepOutcome::new(
                            step,
                            OutcomeKind::Success,
                            format!("created {}", readable(&dir, &plan.root)),
                        )
                    })
            }
            SetupStepKind::CreateRepository => {
                let path = plan.root.join(&step.path);
                discovery::initialize_repository(&path, runner).map(|_| {
                    SetupStepOutcome::new(
                        step,
                        OutcomeKind::Success,
                        "created a Git repository (git init -b main)".to_string(),
                    )
                })
            }
            SetupStepKind::ConfigureRemote => {
                let repo = plan.repositories.iter().find(|repo| repo.id == step.target);
                let path = plan.root.join(&step.path);
                let url = repo
                    .and_then(|repo| repo.remote.clone())
                    .unwrap_or_default();
                discovery::ensure_origin_remote(&path, &url, runner).map(|change| {
                    let summary = match change {
                        OriginChange::Added => format!("origin set to {url}"),
                        OriginChange::Updated => format!("origin replaced with {url}"),
                        OriginChange::Unchanged => format!("origin already pointed at {url}"),
                    };
                    SetupStepOutcome::new(step, OutcomeKind::Success, summary)
                })
            }
            SetupStepKind::UntrackFromRoot => {
                let path = plan.root.join(&step.path);
                runner
                    .repo(&plan.root)
                    .run_checked(&[
                        "rm".into(),
                        "-r".into(),
                        "--cached".into(),
                        "-q".into(),
                        "--".into(),
                        path.as_os_str().to_os_string(),
                    ])
                    .map(|_| {
                        SetupStepOutcome::new(
                            step,
                            OutcomeKind::Success,
                            "stopped tracking these files in the root repository (files kept)"
                                .to_string(),
                        )
                    })
            }
            SetupStepKind::WriteManifest => match manifest::save_project(&plan.project) {
                Ok(path) => {
                    manifest_written = Some(path.clone());
                    Ok(SetupStepOutcome::new(
                        step,
                        OutcomeKind::Success,
                        format!("wrote {}", readable(&path, &plan.root)),
                    ))
                }
                Err(err) => Err(err),
            },
        };

        let outcome = match result {
            Ok(outcome) => outcome,
            Err(err) => {
                if matches!(step.kind, SetupStepKind::CreateRepository) {
                    failed_repositories.push(step.target.clone());
                }
                SetupStepOutcome::new(step, OutcomeKind::Failed, short_message(&err))
                    .with_details(error_details(&err))
            }
        };
        observer.step_finished(&outcome);
        outcomes.push(outcome);
    }

    let failed = outcomes
        .iter()
        .any(|outcome| outcome.outcome == OutcomeKind::Failed);
    let succeeded = outcomes
        .iter()
        .any(|outcome| outcome.outcome == OutcomeKind::Success);
    let mut kind = if !failed {
        SetupKind::Complete
    } else if succeeded {
        SetupKind::Partial
    } else {
        SetupKind::Failed
    };

    // The project exists as soon as the manifest is on disk — including when every step
    // was already satisfied and this run changed nothing. Validation then answers the only
    // question that matters: can GitMesh open what is there now?
    let manifest_on_disk = plan.manifest_path.is_file();
    let project = if manifest_on_disk {
        Some(plan.project.clone())
    } else {
        None
    };
    let validation = if manifest_on_disk && !dry_run {
        Some(verify_expecting(&plan.root, runner, &plan.expected_origins))
    } else {
        None
    };
    // A project that was written but does not open cleanly is never reported as a
    // complete success: the validation result decides.
    if kind == SetupKind::Complete && validation.as_ref().is_some_and(|report| !report.ok) {
        kind = SetupKind::Partial;
    }

    SetupResult {
        plan_id: plan.id.clone(),
        dry_run,
        kind,
        outcomes,
        project,
        manifest_path: manifest_written,
        validation,
        refused: Vec::new(),
    }
}

/// Re-open a project through the normal opening path and check every repository.
///
/// Remotes recorded in the manifest are expected to be configured in Git, which is the
/// normal case for a project that has been set up with remote configuration.
pub fn verify(root: &Path, runner: &GitRunner) -> ValidationReport {
    verify_with(root, runner, true)
}

/// Same check, told whether `origin` is supposed to exist in Git.
///
/// A setup that only *records* remotes (`set_git_remote` off) must not fail validation
/// because Git has no `origin`: the user asked for exactly that, and `verify_with` reports
/// it as the honest state instead of an error.
pub fn verify_with(root: &Path, runner: &GitRunner, expect_origins: bool) -> ValidationReport {
    let expectation = |_repo: &PhysicalRepository| expect_origins;
    verify_project(root, runner, &expectation)
}

/// Same check, told *which* repositories must have their recorded remote configured.
///
/// Repository management uses this: an operation may configure the remote of one
/// repository while another one only records its URL in the manifest, or is left alone
/// with an `origin` that drifted. Only the repositories the operation is responsible for
/// are expectations; the others are reported as they are, without turning into failures
/// of an operation that never touched them.
pub fn verify_expecting(
    root: &Path,
    runner: &GitRunner,
    expected_origins: &[String],
) -> ValidationReport {
    let expectation = |repo: &PhysicalRepository| expected_origins.iter().any(|id| id == &repo.id);
    verify_project(root, runner, &expectation)
}

/// The one implementation of "can GitMesh open what is on disk", parameterised by which
/// repositories must have their recorded remote configured as `origin`.
fn verify_project(
    root: &Path,
    runner: &GitRunner,
    expect_origin: &dyn Fn(&PhysicalRepository) -> bool,
) -> ValidationReport {
    let root =
        paths::lexical_normalize(&paths::absolute(root).unwrap_or_else(|_| root.to_path_buf()));
    let manifest_path = manifest::manifest_path(&root);
    let mut issues: Vec<String> = Vec::new();
    let mut repositories: Vec<RepositoryCheck> = Vec::new();

    if !manifest_path.is_file() {
        issues.push(format!(
            "the manifest {} does not exist",
            readable(&manifest_path, &root)
        ));
        return ValidationReport {
            ok: false,
            root,
            manifest_path,
            project_name: None,
            repositories,
            issues,
        };
    }

    // The normal opening path: discovery, parsing, semantic validation, Git availability.
    let session = match ProjectSession::open(&root) {
        Ok(session) => Some(session),
        Err(err) => {
            issues.push(err.to_string());
            None
        }
    };

    if let Some(session) = &session {
        for repo in session.project().sorted_repositories() {
            let path = session.project().repository_path(repo);
            let exists = path.is_dir();
            // `is_repository_root`, not `is_repository`: a directory inside the project
            // root is "inside a repository" as far as Git is concerned, and only this
            // check answers the question the user cares about — is this directory a
            // repository of its own?
            let is_repository = exists && discovery::is_repository_root(&path, true, runner);
            let origin = if is_repository {
                discovery::origin_url(&path, runner).unwrap_or(None)
            } else {
                None
            };
            let branch = if is_repository {
                runner
                    .repo(&path)
                    .head()
                    .ok()
                    .and_then(|head| head.branch().map(str::to_string))
            } else {
                None
            };
            let mut repo_issues = Vec::new();
            let mut remote_ok = true;
            if !exists {
                repo_issues.push(format!(
                    "the directory '{}' does not exist",
                    to_slash(&repo.relative_path)
                ));
                remote_ok = false;
            } else if !is_repository {
                repo_issues.push(
                    "there is no Git repository here yet (run `git init` in the directory, or \
                     re-run the setup with \"initialise repository\")"
                        .to_string(),
                );
                remote_ok = false;
            } else if let Some(remote) = &repo.remote_url {
                if !expect_origin(repo) {
                    // Record-only: the manifest is the only place the remote has to be.
                } else {
                    match &origin {
                        None => {
                            remote_ok = false;
                            repo_issues.push(format!(
                                "the manifest records the remote {remote} but the repository \
                                 has no origin"
                            ));
                        }
                        Some(current) if current != remote => {
                            remote_ok = false;
                            repo_issues.push(format!(
                                "origin points at {current} but the manifest records {remote}"
                            ));
                        }
                        Some(_) => {}
                    }
                }
            }
            for issue in &repo_issues {
                issues.push(format!("repository '{}': {issue}", repo.id));
            }
            repositories.push(RepositoryCheck {
                id: repo.id.clone(),
                path: repo.relative_slash(),
                role: repo.role,
                exists,
                is_repository,
                manifest_remote: repo.remote_url.clone(),
                origin,
                remote_ok,
                branch,
                issues: repo_issues,
            });
        }
    }

    ValidationReport {
        ok: issues.is_empty(),
        root,
        manifest_path,
        project_name: session.as_ref().map(|s| s.name().to_string()),
        repositories,
        issues,
    }
}

/// Name GitMesh would use for a project rooted at `root`.
pub fn project_name_for(root: &Path) -> String {
    root.file_name()
        .map(|name| name.to_string_lossy().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "project".to_string())
}

fn resolve_name(requested: &str, root: &Path) -> String {
    let trimmed = requested.trim();
    if trimmed.is_empty() {
        project_name_for(root)
    } else {
        trimmed.to_string()
    }
}

/// What the plan does with one repository's `origin`, given whether Git may be touched.
///
/// When Git must not be touched (`set_git_remote` off) a wanted remote that is not present
/// yet becomes [`RemoteAction::Record`]: the manifest records the URL, the repository keeps
/// its Git configuration exactly as it is. Nothing is ever replaced in that mode, which is
/// why it needs no "replace a remote" confirmation.
fn planned_remote_action(
    configure_remotes: bool,
    remote: Option<&str>,
    current: Option<&str>,
) -> RemoteAction {
    match (remote, current) {
        (None, None) => RemoteAction::None,
        (None, Some(_)) => RemoteAction::Keep,
        (Some(_), None) if configure_remotes => RemoteAction::Add,
        (Some(_), None) => RemoteAction::Record,
        (Some(wanted), Some(existing)) if wanted == existing => RemoteAction::Keep,
        (Some(_), Some(_)) if configure_remotes => RemoteAction::Update,
        // A recorded remote that differs from `origin` on disk is not a replacement: Git
        // is left alone. That state is refused by [`record_only_conflict`] rather than
        // silently recorded, because the manifest would then disagree with where a push
        // actually goes.
        (Some(_), Some(_)) => RemoteAction::Record,
    }
}

/// The recorded URL and the `origin` on disk, when recording alone would leave them
/// disagreeing. The plan refuses that instead of producing a project whose manifest
/// promises one remote and whose pushes use another.
fn record_only_conflict(
    configure_remotes: bool,
    remote: Option<&str>,
    current: Option<&str>,
) -> Option<(String, String)> {
    if configure_remotes {
        return None;
    }
    match (remote, current) {
        (Some(wanted), Some(existing)) if wanted != existing => {
            Some((wanted.to_string(), existing.to_string()))
        }
        _ => None,
    }
}

fn provider_id(remote: Option<&str>) -> Option<String> {
    remote
        .and_then(providers::provider_for_remote)
        .map(|provider| provider.id().to_string())
}

fn hosted_ref(remote: Option<&str>) -> Option<RemoteRef> {
    remote.and_then(providers::parse_remote)
}

fn clean_url(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn clean_text(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Files the root repository tracks inside `relative`, counted with `git ls-files`.
fn count_files_tracked_by_root(root: &Path, relative: &Path, runner: &GitRunner) -> usize {
    let repo = runner.repo(root);
    if !repo.is_repository() {
        return 0;
    }
    repo.tracked_files_under(relative)
        .map(|files| files.len())
        .unwrap_or(0)
}

fn short_message(err: &Error) -> String {
    let text = err.to_string();
    text.lines().next().unwrap_or("failed").to_string()
}

fn error_details(err: &Error) -> Vec<String> {
    err.to_string()
        .lines()
        .skip(1)
        .map(str::to_string)
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// `engine/` style display for a path, relative to the project root when possible.
fn readable(path: &Path, root: &Path) -> String {
    match paths::project_relative(root, path) {
        Some(relative) if !paths::is_root_relative(&relative) => to_slash(&relative),
        _ => to_slash(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::OutcomeKind;
    use crate::testkit::{RepoFixture, TempDir};

    /// A plain directory (no `.git`, no manifest) with a Git runner — the starting point
    /// the wizard is designed for.
    fn plain(label: &str) -> (TempDir, GitRunner) {
        let dir = TempDir::new(label).expect("temporary directory");
        let runner = GitRunner::detect().expect("git");
        (dir, runner)
    }

    fn request(root: &Path) -> SetupRequest {
        SetupRequest {
            root: root.to_path_buf(),
            create_root_repository: true,
            set_git_remote: true,
            overwrite_manifest: false,
            overwrite_remotes: false,
            untrack_from_root: true,
            ..SetupRequest::default()
        }
    }

    fn repo(path: &str) -> RepositoryRequest {
        RepositoryRequest {
            path: path.to_string(),
            create: true,
            ..RepositoryRequest::default()
        }
    }

    fn candidate<'a>(inspection: &'a Inspection, path: &str) -> &'a CandidateDirectory {
        fn find<'a>(nodes: &'a [CandidateDirectory], path: &str) -> Option<&'a CandidateDirectory> {
            for node in nodes {
                if node.relative_path == Path::new(path) {
                    return Some(node);
                }
                if let Some(found) = find(&node.children, path) {
                    return Some(found);
                }
            }
            None
        }
        find(&inspection.candidates, path).unwrap_or_else(|| panic!("no candidate at {path}"))
    }

    fn planned(gui: &SetupPlan, kind: SetupStepKind, target: &str) -> bool {
        gui.steps
            .iter()
            .any(|step| step.kind == kind && step.target == target && step.planned())
    }

    // ------------------------------------------------------------- scanning --

    #[test]
    fn inspection_of_an_empty_directory_reports_facts_only() {
        let (dir, runner) = plain("setup-scan-empty");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("include")).unwrap();

        let inspection = inspect(dir.path(), &runner).unwrap();
        assert!(inspection.exists);
        assert!(!inspection.is_gitmesh_project);
        assert!(!inspection.manifest_path.exists());
        assert!(!inspection.root_is_repository);
        assert_eq!(inspection.repositories.len(), 0);
        assert_eq!(candidate(&inspection, "src").path_label(), "src");
        assert!(!candidate(&inspection, "src").suggested());
        assert_eq!(
            inspection.suggested_name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
        // Nothing was created by inspecting.
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitmesh").exists());
    }

    #[test]
    fn inspection_marks_existing_repositories_as_candidates() {
        let fixture = RepoFixture::named("setup-scan-repos");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        fixture.mkdir("tools/src");

        let inspection = inspect(fixture.path(), fixture.runner()).unwrap();
        assert!(inspection.root_is_repository);
        let engine = candidate(&inspection, "engine");
        assert!(engine.is_repository && engine.suggested() && engine.has_commits);
        assert_eq!(engine.branch.as_deref(), Some("main"));
        assert_eq!(engine.files, 1, "engine/lib.rs");
        assert_eq!(engine.subtree_files, 1);
        let tools = candidate(&inspection, "tools");
        assert!(!tools.is_repository && !tools.suggested());
        assert_eq!(tools.depth, 1);
    }

    #[test]
    fn inspection_counts_files_the_root_repository_tracks_inside_a_candidate() {
        let fixture = RepoFixture::named("setup-scan-tracked");
        // The file is committed by the root repository *before* the directory becomes a
        // repository of its own: this is what a real project looks like when someone
        // decides later to split a directory out, and it is the case the wizard has to
        // detect because the files would otherwise belong to two repositories.
        fixture.write("engine/tracked.txt", "owned by the root repository\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks a file inside the future repository");
        fixture.init_repo("engine");

        let inspection = inspect(fixture.path(), fixture.runner()).unwrap();
        let engine = candidate(&inspection, "engine");
        assert_eq!(engine.tracked_by_root, 1, "{engine:?}");
    }

    #[test]
    fn inspection_finds_nested_repositories_and_existing_projects() {
        let fixture = RepoFixture::named("setup-scan-nested");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        fixture.init_repo("engine/vendor/dep");
        fixture.write("engine/vendor/dep/lib.rs", "// vendored\n");
        fixture.commit("engine/vendor/dep", "vendored");

        let inspection = inspect(fixture.path(), fixture.runner()).unwrap();
        let engine = candidate(&inspection, "engine");
        assert!(engine.is_repository && engine.suggested());
        assert_eq!(
            engine.nested_repositories,
            vec![PathBuf::from("vendor/dep")],
            "a repository inside a candidate is reported without asking Git"
        );
        assert!(
            inspection
                .notices
                .iter()
                .any(|notice| notice.contains("nested inside")),
            "{:?}",
            inspection.notices
        );

        // A configured project is recognised, with its manifest text.
        fixture.project_with(&[("root", ".")]);
        let inspection = inspect(fixture.path(), fixture.runner()).unwrap();
        assert!(inspection.is_gitmesh_project);
        assert!(inspection.manifest_text.is_some());
        assert!(inspection.manifest_error.is_none());
    }

    #[test]
    fn inspection_reports_a_malformed_manifest_and_a_missing_directory() {
        let fixture = RepoFixture::named("setup-scan-broken");
        std::fs::create_dir_all(fixture.path().join(".gitmesh")).unwrap();
        std::fs::write(
            fixture.path().join(".gitmesh/project.toml"),
            "version = 1\nname = \"demo\"\n\n[repositories]\n",
        )
        .unwrap();

        let inspection = inspect(fixture.path(), fixture.runner()).unwrap();
        assert!(inspection.is_gitmesh_project);
        assert!(inspection.manifest_error.is_some());
        assert!(
            inspection
                .notices
                .iter()
                .any(|notice| notice.contains("could not be read")),
            "{:?}",
            inspection.notices
        );

        let missing = inspect(&fixture.path().join("nope"), fixture.runner()).unwrap();
        assert!(!missing.exists);
        assert!(missing.candidates.is_empty());
    }

    // ------------------------------------------------------------ planning --

    #[test]
    fn plan_for_a_root_only_project_generates_the_manifest() {
        let (dir, runner) = plain("setup-plan-root");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let plan = super::plan(&request(dir.path()), &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert_eq!(plan.name, dir.path().file_name().unwrap().to_string_lossy());
        assert!(planned(&plan, SetupStepKind::CreateMetadataDir, "manifest"));
        assert!(planned(&plan, SetupStepKind::CreateRepository, "root"));
        assert!(planned(&plan, SetupStepKind::WriteManifest, "manifest"));
        assert_eq!(plan.repositories.len(), 1);
        assert!(plan.manifest.contains("version = 1"));
        assert!(plan.manifest.contains("[root]"));
        assert!(!plan.manifest.contains("[[repositories]]"));
        assert!(plan
            .safety
            .iter()
            .any(|line| line.contains("no existing .git")));
        // Planning did not touch the directory.
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitmesh").exists());
    }

    #[test]
    fn plan_uses_the_requested_and_suggested_names() {
        let (dir, runner) = plain("setup-plan-names");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        std::fs::create_dir_all(dir.join("My Engine")).unwrap();
        std::fs::create_dir_all(dir.join("a/tools")).unwrap();
        std::fs::create_dir_all(dir.join("b/tools")).unwrap();

        let mut req = request(dir.path());
        req.name = " MyProject ".to_string();
        req.repositories = vec![
            repo("engine"),
            repo("My Engine"),
            repo("a/tools"),
            repo("b/tools"),
        ];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert_eq!(plan.name, "MyProject");
        let ids: Vec<&str> = plan
            .repositories
            .iter()
            .map(|repo| repo.id.as_str())
            .collect();
        assert!(ids.contains(&"engine"));
        assert!(ids.contains(&"my-engine"), "{ids:?}");
        assert!(ids.contains(&"tools"));
        assert!(ids.contains(&"tools-2"), "{ids:?}");
        assert!(plan.manifest.contains("id = \"my-engine\""));
        assert!(plan.manifest.contains("id = \"tools-2\""));

        // A custom id wins, a duplicate is refused.
        let mut req = request(dir.path());
        req.repositories = vec![
            RepositoryRequest {
                path: "engine".into(),
                id: "backend".into(),
                create: true,
                ..RepositoryRequest::default()
            },
            RepositoryRequest {
                path: "a/tools".into(),
                id: "backend".into(),
                create: true,
                ..RepositoryRequest::default()
            },
        ];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("already used")),
            "{:?}",
            plan.blockers
        );
    }

    #[test]
    fn plan_never_reinitialises_an_existing_repository_and_reports_it() {
        let fixture = RepoFixture::named("setup-plan-existing");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        let head_before = fixture.git_ok("engine", &["rev-parse", "HEAD"]);

        let mut req = request(fixture.path());
        req.create_root_repository = true;
        req.repositories = vec![repo("engine")];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(!planned(&plan, SetupStepKind::CreateRepository, "engine"));
        assert!(!planned(&plan, SetupStepKind::CreateRepository, "root"));
        assert_eq!(plan.created_repositories().count(), 0);
        let step = plan
            .steps
            .iter()
            .find(|step| step.kind == SetupStepKind::CreateRepository && step.target == "engine")
            .unwrap();
        assert!(
            matches!(&step.state, StepState::AlreadySatisfied(reason) if reason.contains("never re-initialised")),
            "{step:?}"
        );
        assert!(
            plan.notices
                .iter()
                .any(|notice| notice.contains("already a Git repository")),
            "{:?}",
            plan.notices
        );
        assert_eq!(
            fixture.git_ok("engine", &["rev-parse", "HEAD"]),
            head_before
        );
    }

    #[test]
    fn plan_rejects_overlapping_nested_and_invalid_selections() {
        let fixture = RepoFixture::named("setup-plan-overlap");
        fixture.mkdir("engine/core");
        fixture.write("notes.txt", "notes\n");

        let mut req = request(fixture.path());
        req.repositories = vec![repo("engine"), repo("engine/core")];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("is inside the repository 'engine'")),
            "{:?}",
            plan.blockers
        );
        // Selecting the outer directory after the inner one is refused as well.
        let mut reversed = request(fixture.path());
        reversed.repositories = vec![repo("engine/core"), repo("engine")];
        let plan = super::plan(&reversed, fixture.runner()).unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("contains the configured repository")),
            "{:?}",
            plan.blockers
        );
        assert!(plan.blocked_steps().count() > 0);

        // The project root cannot become an external repository.
        let mut req = request(fixture.path());
        req.repositories = vec![repo(".")];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("always the root repository")),
            "{:?}",
            plan.blockers
        );

        // A file and a missing directory are refused with a reason each.
        let mut req = request(fixture.path());
        req.repositories = vec![repo("notes.txt"), repo("does-not-exist")];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("is not a directory")),
            "{:?}",
            plan.blockers
        );
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("does not exist")),
            "{:?}",
            plan.blockers
        );

        // A path outside the project is refused too.
        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "../escape".into(),
            create: true,
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("must not contain '..'")),
            "{:?}",
            plan.blockers
        );
    }

    #[test]
    fn plan_requires_confirmation_before_replacing_a_remote() {
        let fixture = RepoFixture::named("setup-plan-remote");
        let bare = fixture.create_bare("remotes/engine.git");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", bare.to_string_lossy().as_ref()],
        );
        fixture.git_ok("engine", &["push", "-q", "-u", "origin", "main"]);

        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("git@github.com:acme/engine.git".into()),
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("must be confirmed explicitly")),
            "{:?}",
            plan.blockers
        );
        assert!(plan
            .blocked_steps()
            .any(|step| step.kind == SetupStepKind::ConfigureRemote));

        // With the confirmation, the step is planned and the safety lines say so.
        let mut confirmed = req.clone();
        confirmed.overwrite_remotes = true;
        let plan = super::plan(&confirmed, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(planned(&plan, SetupStepKind::ConfigureRemote, "engine"));
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("the origin of engine is replaced")),
            "{:?}",
            plan.safety
        );

        // A request that leaves the remote alone keeps it, recorded in the manifest.
        let mut keep = request(fixture.path());
        keep.repositories = vec![repo("engine")];
        let plan = super::plan(&keep, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let engine = plan.repositories.iter().find(|r| r.id == "engine").unwrap();
        assert_eq!(engine.remote_action, RemoteAction::Keep);
        assert_eq!(
            engine.remote.as_deref(),
            Some(bare.to_string_lossy().as_ref())
        );
        assert!(!planned(&plan, SetupStepKind::ConfigureRemote, "engine"));
        assert!(plan.manifest.contains("remotes/engine.git"));
    }

    #[test]
    fn plan_requires_confirmation_before_replacing_the_manifest() {
        let fixture = RepoFixture::named("setup-plan-manifest");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);

        let mut req = request(fixture.path());
        req.name = "renamed-project".to_string();
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("confirmed explicitly")),
            "{:?}",
            plan.blockers
        );

        req.overwrite_manifest = true;
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(planned(&plan, SetupStepKind::WriteManifest, "manifest"));
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("the existing manifest is replaced")),
            "{:?}",
            plan.safety
        );
    }

    #[test]
    fn plan_is_idempotent_for_an_already_matching_configuration() {
        let fixture = RepoFixture::named("setup-plan-idempotent");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");

        let mut req = request(fixture.path());
        req.name = "idempotent".to_string();
        req.repositories = vec![repo("engine")];
        // Set the project up once, then plan the same request again.
        let first = super::plan(&req, fixture.runner()).unwrap();
        let result = apply(
            &first,
            false,
            fixture.runner(),
            &mut SetupObserver::silent(),
        );
        assert!(result.is_success(), "{}", result.summary());

        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(
            plan.is_noop(),
            "{:?}",
            plan.planned_steps().collect::<Vec<_>>()
        );
        assert_eq!(
            plan.summary(),
            "nothing to do: the project is already set up as requested"
        );
        assert!(
            plan.notices.iter().any(
                |notice| notice.contains("already matches this configuration")
                    || notice.contains("already up to date")
            ),
            "{:?}",
            plan.notices
        );
    }

    #[test]
    fn plan_describes_local_only_and_hosted_repositories() {
        let (dir, runner) = plain("setup-plan-remotes");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        std::fs::create_dir_all(dir.join("tools")).unwrap();

        let mut req = request(dir.path());
        req.repositories = vec![
            RepositoryRequest {
                path: "engine".into(),
                create: true,
                remote: Some("git@github.com:acme/myproject-engine.git".into()),
                visibility: Some("private".into()),
                ..RepositoryRequest::default()
            },
            repo("tools"),
        ];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        let engine = plan.repositories.iter().find(|r| r.id == "engine").unwrap();
        assert_eq!(engine.provider.as_deref(), Some("github"));
        let hosted = engine.hosted.as_ref().unwrap();
        assert_eq!(hosted.full_name(), "acme/myproject-engine");
        assert_eq!(hosted.web_url(), "https://github.com/acme/myproject-engine");
        assert_eq!(engine.visibility.as_deref(), Some("private"));
        assert_eq!(engine.remote_action, RemoteAction::Add);
        assert!(planned(&plan, SetupStepKind::ConfigureRemote, "engine"));

        let tools = plan.repositories.iter().find(|r| r.id == "tools").unwrap();
        assert_eq!(tools.remote_action, RemoteAction::None);
        assert!(tools.remote.is_none());
        // The manifest records the URL and nothing else: no provider, no visibility.
        assert!(plan
            .manifest
            .contains("git@github.com:acme/myproject-engine.git"));
        assert!(!plan.manifest.contains("visibility"));
        assert!(!plan.manifest.contains("provider"));
        // The repository rows explain themselves.
        assert!(
            engine.sentence().contains("acme/myproject-engine"),
            "{}",
            engine.sentence()
        );
        assert!(
            tools.sentence().contains("no remote"),
            "{}",
            tools.sentence()
        );
    }

    #[test]
    fn plan_offers_to_untrack_root_files_and_warns_when_it_does_not() {
        let fixture = RepoFixture::named("setup-plan-untrack");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks engine/lib.rs");
        fixture.init_repo("engine");

        let mut req = request(fixture.path());
        req.repositories = vec![repo("engine")];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let engine = plan.repositories.iter().find(|r| r.id == "engine").unwrap();
        assert_eq!(engine.tracked_by_root, 1);
        assert!(!engine.untrack);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("still tracks 1 file(s)")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("no file is moved")),
            "{:?}",
            plan.safety
        );

        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            untrack_from_root: true,
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        assert!(planned(&plan, SetupStepKind::UntrackFromRoot, "engine"));
        assert!(plan.safety.iter().any(|line| line.contains("stay on disk")));
    }

    #[test]
    fn plan_warns_about_nested_repositories_and_about_not_creating_a_repository() {
        let (dir, runner) = plain("setup-plan-warnings");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        // A nested repository inside the directory that is becoming a repository.
        let nested = dir.join("engine/vendor/dep");
        std::fs::create_dir_all(&nested).unwrap();
        runner
            .repo(&nested)
            .run_checked(&["init", "-q", "-b", "main"])
            .unwrap();
        // A directory that will not get a Git repository.
        std::fs::create_dir_all(dir.join("tools")).unwrap();

        let mut req = request(dir.path());
        req.repositories = vec![
            repo("engine"),
            RepositoryRequest {
                path: "tools".into(),
                create: false,
                ..RepositoryRequest::default()
            },
        ];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("contains another Git repository")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("not a Git repository yet")),
            "{:?}",
            plan.warnings
        );
        // The manifest still lists it: GitMesh reports such a repository as unavailable
        // instead of hiding it.
        assert!(plan.manifest.contains("tools"));
    }

    #[test]
    fn plan_reports_a_branch_hint_and_a_vague_request_is_cleaned_up() {
        let (dir, runner) = plain("setup-plan-branch");
        std::fs::create_dir_all(dir.join("engine")).unwrap();

        let mut req = request(dir.path());
        req.name = "   ".to_string();
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("   ".into()),
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, &runner).unwrap();
        assert_eq!(plan.name, dir.path().file_name().unwrap().to_string_lossy());
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("remote URL is empty")),
            "{:?}",
            plan.blockers
        );

        let mut req = request(dir.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            branch: Some(" develop ".into()),
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(plan.manifest.contains("branch = \"develop\""));
    }

    // ----------------------------------------------------------- execution --

    #[test]
    fn apply_creates_repositories_the_manifest_and_validates_the_project() {
        let (dir, runner) = plain("setup-apply");
        for name in ["src", "include", "engine", "renderer", "tools"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("engine/lib.rs"), "pub fn go() {}\n").unwrap();
        let bare = TempDir::new("setup-apply-remote").unwrap();
        let remote = bare.join("myproject-engine.git");
        runner
            .run(&[
                "init".into(),
                "--bare".into(),
                "-q".into(),
                remote.as_os_str().to_os_string(),
            ])
            .unwrap();

        let mut req = request(dir.path());
        req.name = "MyProject".to_string();
        req.repositories = vec![
            RepositoryRequest {
                path: "engine".into(),
                create: true,
                remote: Some(remote.to_string_lossy().to_string()),
                ..RepositoryRequest::default()
            },
            repo("renderer"),
            repo("tools"),
        ];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert_eq!(result.exit_code(), 0);
        assert!(result.failures().next().is_none());
        assert!(result.succeeded() >= 6, "{}", result.summary());

        // Root and external repositories exist as real Git repositories.
        assert!(dir.join(".git").is_dir());
        for name in ["engine", "renderer", "tools"] {
            assert!(
                dir.join(name).join(".git").is_dir(),
                "{name} has its own .git"
            );
        }
        // The manifest exists, parses, and describes the architecture.
        let manifest = dir.join(".gitmesh/project.toml");
        assert!(manifest.is_file());
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert_eq!(text, plan.manifest);
        let project = manifest::load_from_root(dir.path()).unwrap();
        assert_eq!(project.name, "MyProject");
        assert_eq!(project.len(), 4);
        assert_eq!(
            project.repository("engine").unwrap().remote_url.as_deref(),
            Some(remote.to_string_lossy().as_ref())
        );
        // The remote was configured in Git as well.
        assert_eq!(
            discovery::origin_url(&dir.join("engine"), &runner).unwrap(),
            Some(remote.to_string_lossy().to_string())
        );
        assert_eq!(
            discovery::origin_url(&dir.join("renderer"), &runner).unwrap(),
            None
        );

        // Final validation runs through the normal opening path.
        let validation = result.validation.as_ref().expect("validation");
        assert!(validation.ok, "{:?}", validation.issues);
        assert_eq!(validation.repositories.len(), 4);
        assert!(validation.repositories.iter().all(|check| check.is_ok()));
        assert_eq!(validation.project_name.as_deref(), Some("MyProject"));
        assert!(validation
            .repositories
            .iter()
            .any(|check| check.id == "engine" && check.remote_ok));

        // The user's files are all still there.
        assert!(dir.join("src/main.rs").is_file());
        assert!(dir.join("engine/lib.rs").is_file());
    }

    #[test]
    fn suggested_ids_read_like_the_project_and_stay_unique() {
        let project = discovery::initial_project(
            Path::new("/tmp/MyProject"),
            Some("MyProject".to_string()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            suggested_repository_id(&project, Path::new("engine")),
            "myproject-engine"
        );
        assert_eq!(
            suggested_repository_id(&project, Path::new("vendor/renderer")),
            "myproject-renderer"
        );
        assert_eq!(id_fragment("My Project!"), "my-project");
        assert_eq!(id_fragment("2024/05"), "2024-05");

        // Once an id is taken, the next suggestion moves aside instead of colliding.
        let mut taken = project.clone();
        taken.repositories.push(crate::model::PhysicalRepository {
            id: "myproject-engine".into(),
            role: crate::model::RepositoryRole::External,
            relative_path: PathBuf::from("engine"),
            remote_url: None,
            branch: None,
            absolute_path: PathBuf::from("/tmp/MyProject/engine"),
        });
        assert_eq!(
            suggested_repository_id(&taken, Path::new("engine")),
            "myproject-engine-2"
        );
    }

    #[test]
    fn a_first_publish_is_planned_and_never_run_by_the_setup_engine() {
        let (dir, runner) = plain("setup-first-publish");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        std::fs::write(dir.join("engine/lib.rs"), "pub fn go() {}\n").unwrap();
        std::fs::write(dir.join("README.md"), "# MyProject\n").unwrap();

        let mut req = request(dir.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("/tmp/nowhere/engine.git".into()),
            ..RepositoryRequest::default()
        }];
        req.publish_first_commit = Some("Initial commit".into());

        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let publish = plan
            .first_publish()
            .expect("the plan carries the follow-up");
        assert_eq!(publish.message, "Initial commit");
        assert_eq!(publish.repositories, ["engine"]);
        assert!(publish.sentence().contains("Initial commit"));

        // Applying sets the project up and stops there: committing and pushing are the
        // ordinary project operations, not part of the setup engine.
        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert!(dir.join(".gitmesh/project.toml").is_file());
        let engine = runner.repo(dir.join("engine"));
        assert!(engine.head_oid().unwrap().is_none(), "no commit was made");
        assert_eq!(engine.remotes().unwrap().len(), 1, "origin is configured");

        // Without the request there is nothing to publish.
        req.publish_first_commit = None;
        assert!(super::plan(&req, &runner)
            .unwrap()
            .first_publish()
            .is_none());

        // And a request that configures no remote has nothing to publish either.
        req.publish_first_commit = Some("Initial commit".into());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            ..RepositoryRequest::default()
        }];
        assert!(super::plan(&req, &runner)
            .unwrap()
            .first_publish()
            .is_none());
    }

    #[test]
    fn an_empty_first_commit_message_is_refused_instead_of_defaulted() {
        let (dir, runner) = plain("setup-first-publish-empty");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("/tmp/nowhere/engine.git".into()),
            ..RepositoryRequest::default()
        }];
        req.publish_first_commit = Some("   ".into());

        let plan = super::plan(&req, &runner).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|line| line.contains("first commit")),
            "{:?}",
            plan.blockers
        );
        let refused = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(!refused.is_success(), "{}", refused.summary());
    }

    #[test]
    fn apply_untracks_root_files_without_deleting_them() {
        let fixture = RepoFixture::named("setup-apply-untrack");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks engine/lib.rs");
        fixture.init_repo("engine");

        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            untrack_from_root: true,
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        let result = apply(&plan, false, fixture.runner(), &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert!(fixture.path().join("engine/lib.rs").is_file());
        assert!(
            fixture
                .runner()
                .repo(fixture.path())
                .tracked_files_under(Path::new("engine"))
                .unwrap()
                .is_empty(),
            "the root repository no longer tracks the file"
        );
        // The external repository now owns the file (untracked there until the next
        // commit), and the root repository's index change is staged, not committed:
        // nothing is discarded silently.
        let engine_status = fixture
            .runner()
            .repo(fixture.path().join("engine"))
            .status()
            .unwrap();
        assert!(
            engine_status
                .entries
                .iter()
                .any(|entry| entry.path.ends_with("lib.rs")),
            "the file now belongs to the external repository: {engine_status:?}"
        );
        let root_status = fixture.runner().repo(fixture.path()).status().unwrap();
        assert!(
            root_status
                .entries
                .iter()
                .any(|entry| entry.path.starts_with("engine")),
            "the root repository has the deletion staged: {root_status:?}"
        );
    }

    #[test]
    fn apply_reports_a_partial_failure_and_keeps_the_successful_work() {
        let (dir, runner) = plain("setup-apply-partial");
        for name in ["engine", "tools"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        let mut req = request(dir.path());
        req.repositories = vec![
            repo("engine"),
            RepositoryRequest {
                path: "tools".into(),
                create: true,
                remote: Some("git@github.com:acme/tools.git".into()),
                ..RepositoryRequest::default()
            },
        ];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        // The directory disappears and is replaced by a file between the review and the
        // confirmation: the step fails at execution time, exactly like a permission error.
        std::fs::remove_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools"), "not a directory\n").unwrap();

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert_eq!(result.kind, SetupKind::Partial, "{}", result.summary());
        assert_eq!(result.exit_code(), 1);
        assert!(result.failures().any(|outcome| outcome.target == "tools"));
        assert!(result.outcomes.iter().any(|outcome| {
            outcome.target == "tools"
                && outcome.outcome == OutcomeKind::Skipped
                && outcome.summary.contains("could not be created")
        }));
        // What worked is kept and reported.
        assert!(dir.join(".git").is_dir());
        assert!(dir.join("engine/.git").is_dir());
        assert!(dir.join(".gitmesh/project.toml").is_file());
        let validation = result.validation.as_ref().unwrap();
        assert!(!validation.ok);
        assert!(
            validation
                .issues
                .iter()
                .any(|issue| issue.contains("'tools'")),
            "{:?}",
            validation.issues
        );
        // The message says what to fix rather than claiming success.
        assert!(
            result.summary().contains("completed with errors"),
            "{}",
            result.summary()
        );
    }

    #[test]
    fn apply_refuses_a_blocked_plan_without_touching_anything() {
        let (dir, runner) = plain("setup-apply-blocked");
        std::fs::create_dir_all(dir.join("engine/core")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine"), repo("engine/core")];
        let plan = super::plan(&req, &runner).unwrap();
        assert!(!plan.is_ready());

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert_eq!(result.kind, SetupKind::Failed);
        assert!(!result.refused.is_empty());
        assert!(result.outcomes.is_empty());
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitmesh").exists());
        assert!(!dir.join("engine/.git").exists());
    }

    #[test]
    fn apply_in_dry_run_changes_nothing_but_reports_the_plan() {
        let (dir, runner) = plain("setup-apply-dry");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine")];
        let plan = super::plan(&req, &runner).unwrap();

        let result = apply(&plan, true, &runner, &mut SetupObserver::silent());
        assert!(result.dry_run);
        assert!(result.succeeded() == 0);
        assert!(result
            .outcomes
            .iter()
            .all(|o| o.outcome == OutcomeKind::Skipped));
        assert!(result.summary().contains("dry run"), "{}", result.summary());
        assert!(!dir.join(".git").exists());
        assert!(!dir.join("engine/.git").exists());
        assert!(!dir.join(".gitmesh").exists());
        assert!(result.manifest_path.is_none());
    }

    #[test]
    fn apply_is_idempotent_and_never_touches_a_repository_outside_the_project() {
        let (dir, runner) = plain("setup-apply-idempotent");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let outside = TempDir::new("setup-apply-outside").unwrap();
        let sibling = outside.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        runner
            .repo(&sibling)
            .run_checked(&["init", "-q", "-b", "main"])
            .unwrap();
        std::fs::write(sibling.join("work.txt"), "someone else's work\n").unwrap();
        let sibling_head_before = {
            runner.repo(&sibling).run_checked(&["add", "-A"]).unwrap();
            runner
                .repo(&sibling)
                .run_checked(&[
                    "-c",
                    "user.name=T",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "x",
                ])
                .unwrap();
            runner
                .repo(&sibling)
                .run_checked(&["rev-parse", "HEAD"])
                .unwrap()
        };

        let mut req = request(dir.path());
        req.repositories = vec![repo("engine")];
        let plan = super::plan(&req, &runner).unwrap();
        let first = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(first.is_success(), "{}", first.summary());
        let manifest_before = std::fs::read_to_string(dir.join(".gitmesh/project.toml")).unwrap();
        let root_head_before = runner
            .repo(dir.path())
            .run_checked(&["rev-parse", "HEAD"])
            .ok();

        // Re-plan and re-apply the very same request.
        let plan_again = super::plan(&req, &runner).unwrap();
        assert!(plan_again.is_ready());
        assert!(
            plan_again.is_noop(),
            "{:?}",
            plan_again.planned_steps().collect::<Vec<_>>()
        );
        let second = apply(&plan_again, false, &runner, &mut SetupObserver::silent());
        assert!(second.is_success(), "{}", second.summary());
        assert_eq!(second.succeeded(), 0);
        assert!(second.skipped() >= 4);
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitmesh/project.toml")).unwrap(),
            manifest_before
        );
        assert_eq!(
            runner
                .repo(dir.path())
                .run_checked(&["rev-parse", "HEAD"])
                .ok(),
            root_head_before,
            "no commit is created by a setup"
        );
        assert_eq!(
            runner
                .repo(&sibling)
                .run_checked(&["rev-parse", "HEAD"])
                .unwrap(),
            sibling_head_before
        );
        assert!(sibling.join("work.txt").is_file());
        assert_eq!(runner.repo(&sibling).status().unwrap().entries.len(), 0);
    }

    #[test]
    fn apply_changes_an_existing_remote_only_when_confirmed() {
        let fixture = RepoFixture::named("setup-apply-remote");
        let first = fixture.create_bare("remotes/one.git");
        let second = fixture.create_bare("remotes/two.git");
        fixture.init_repo("engine");
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", first.to_string_lossy().as_ref()],
        );

        // Not confirmed: the plan is blocked, nothing changes.
        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some(second.to_string_lossy().to_string()),
            ..RepositoryRequest::default()
        }];
        let blocked = super::plan(&req, fixture.runner()).unwrap();
        apply(
            &blocked,
            false,
            fixture.runner(),
            &mut SetupObserver::silent(),
        );
        assert_eq!(
            discovery::origin_url(&fixture.path().join("engine"), fixture.runner()).unwrap(),
            Some(first.to_string_lossy().to_string())
        );

        // Confirmed: the remote is replaced and the change is reported.
        req.overwrite_remotes = true;
        let plan = super::plan(&req, fixture.runner()).unwrap();
        let result = apply(&plan, false, fixture.runner(), &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert_eq!(
            discovery::origin_url(&fixture.path().join("engine"), fixture.runner()).unwrap(),
            Some(second.to_string_lossy().to_string())
        );
        assert!(result
            .outcomes
            .iter()
            .any(|outcome| outcome.summary.contains("origin replaced")));
    }

    #[test]
    fn observer_sees_every_step_in_order() {
        let (dir, runner) = plain("setup-apply-observer");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine")];
        let plan = super::plan(&req, &runner).unwrap();

        let mut started: Vec<String> = Vec::new();
        let mut finished: Vec<String> = Vec::new();
        {
            let mut on_step = |step: &SetupStep| started.push(step.kind.label().to_string());
            let mut on_outcome =
                |outcome: &SetupStepOutcome| finished.push(outcome.kind.label().to_string());
            let mut observer = SetupObserver::silent()
                .on_step(&mut on_step)
                .on_outcome(&mut on_outcome);
            apply(&plan, false, &runner, &mut observer);
        }
        assert_eq!(started.len(), plan.steps.len());
        assert_eq!(started, finished);
        assert_eq!(started[0], "create-metadata");
        assert_eq!(started.last().unwrap(), "write-manifest");
    }

    // ---------------------------------------------------------- validation --

    #[test]
    fn verify_reports_a_missing_manifest_and_a_missing_repository() {
        let (dir, runner) = plain("setup-verify");
        let report = verify(dir.path(), &runner);
        assert!(!report.ok);
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.contains("does not exist")));

        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: false,
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, &runner).unwrap();
        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        // The directory has no repository: the setup says so instead of pretending.
        assert_eq!(result.kind, SetupKind::Partial, "{}", result.summary());
        assert_eq!(result.exit_code(), 1);
        let validation = result.validation.as_ref().unwrap();
        assert!(!validation.ok);
        assert!(validation
            .issues
            .iter()
            .any(|issue| issue.contains("no Git repository here yet")));
        let check = validation
            .repositories
            .iter()
            .find(|check| check.id == "engine")
            .unwrap();
        assert!(!check.is_repository && !check.is_ok());
    }

    #[test]
    fn verify_reports_a_remote_that_does_not_match_the_manifest() {
        let fixture = RepoFixture::named("setup-verify-remote");
        fixture.init_repo("engine");
        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("git@github.com:acme/engine.git".into()),
            ..RepositoryRequest::default()
        }];
        let plan = super::plan(&req, fixture.runner()).unwrap();
        let result = apply(&plan, false, fixture.runner(), &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());

        let report = verify(fixture.path(), fixture.runner());
        assert!(report.ok, "{:?}", report.issues);

        // Break the remote in Git: the manifest still records the configured URL.
        fixture.git_ok(
            "engine",
            &["remote", "set-url", "origin", "/tmp/elsewhere.git"],
        );
        let report = verify(fixture.path(), fixture.runner());
        assert!(!report.ok);
        let check = report
            .repositories
            .iter()
            .find(|check| check.id == "engine")
            .unwrap();
        assert!(!check.remote_ok);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.contains("the manifest records")),
            "{:?}",
            report.issues
        );
    }

    // ------------------------------------------------- record-only remotes --

    /// A directory with one subdirectory to turn into a repository.
    fn directory_with_engine(label: &str) -> (TempDir, GitRunner) {
        let (dir, runner) = plain(label);
        let engine = dir.path().join("engine");
        std::fs::create_dir_all(&engine).expect("engine directory");
        std::fs::write(engine.join("lib.rs"), "// engine\n").expect("engine file");
        (dir, runner)
    }

    fn record_only_request(root: &Path) -> SetupRequest {
        let mut request = request(root);
        request.name = "MyProject".to_string();
        // The wizard's "configure remotes" box, unchecked: record, do not touch Git.
        request.set_git_remote = false;
        request.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("git@github.com:acme/engine.git".into()),
            ..RepositoryRequest::default()
        }];
        request
    }

    #[test]
    fn a_remote_can_be_recorded_without_touching_git() {
        let (dir, runner) = directory_with_engine("setup-record-only");
        let mut request = record_only_request(dir.path());
        request.publish_first_commit = Some("Initial commit".into());

        let plan = plan(&request, &runner).expect("plan");
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let engine = plan
            .repositories
            .iter()
            .find(|repo| repo.id == "engine")
            .expect("engine");
        assert_eq!(engine.remote_action, RemoteAction::Record);
        // Nothing runs for it: recording is manifest work, not repository work.
        assert!(
            !plan
                .planned_steps()
                .any(|step| step.kind == SetupStepKind::ConfigureRemote),
            "{:?}",
            plan.planned_steps()
                .map(|s| s.detail.clone())
                .collect::<Vec<_>>()
        );
        assert!(
            plan.warnings
                .iter()
                .any(|line| line.contains("not configured in Git")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("manifest only")),
            "{:?}",
            plan.safety
        );
        assert!(plan.manifest.contains("git@github.com:acme/engine.git"));
        assert!(
            engine.sentence().contains("without touching Git"),
            "{}",
            engine.sentence()
        );
        // Nothing to publish: a repository with no configured remote is not a push target
        // and never a push failure.
        assert!(plan.first_publish().is_none());

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert!(
            result.validation.as_ref().expect("validation").ok,
            "{:?}",
            result.validation.expect("validation").issues
        );
        let engine_path = dir.path().join("engine");
        assert!(engine_path.join(".git/HEAD").is_file(), "still created");
        assert_eq!(
            discovery::origin_url(&engine_path, &runner).expect("origin"),
            None,
            "Git was not touched"
        );
    }

    #[test]
    fn verify_reports_recorded_remotes_only_when_they_are_expected_in_git() {
        let (dir, runner) = directory_with_engine("setup-record-verify");
        let request = record_only_request(dir.path());
        let plan = plan(&request, &runner).expect("plan");
        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());

        // The strict check is what a normal project gets: a manifest remote without an
        // `origin` is drift.
        let strict = verify(dir.path(), &runner);
        assert!(!strict.ok);
        assert!(
            strict
                .issues
                .iter()
                .any(|issue| issue.contains("no origin")),
            "{:?}",
            strict.issues
        );
        // The record-only check accepts exactly what the user asked for.
        let recorded = verify_with(dir.path(), &runner, false);
        assert!(recorded.ok, "{:?}", recorded.issues);
        assert_eq!(recorded.project_name.as_deref(), Some("MyProject"));
    }

    #[test]
    fn a_manifest_that_cannot_be_read_is_still_a_project() {
        let (dir, runner) = directory_with_engine("setup-manifest-unreadable");
        // A directory where the manifest should be: it exists, and it cannot be read as a
        // file. The wizard must say "there is a project here, and it is broken", never
        // "there is no project here".
        std::fs::create_dir_all(dir.path().join(".gitmesh/project.toml")).expect("manifest path");
        let inspection = inspect(dir.path(), &runner).expect("inspect");
        assert!(inspection.is_gitmesh_project);
        assert!(inspection.manifest_error.is_some(), "{inspection:?}");
        assert!(inspection
            .notices
            .iter()
            .any(|notice| notice.contains("could not be read")));
    }

    #[test]
    fn a_second_run_reports_the_project_as_ready_and_opens_it_unchanged() {
        let (dir, runner) = directory_with_engine("setup-noop-rerun");
        let bare_root = TempDir::new("setup-noop-remote").expect("temp");
        let remote = bare_root.join("MyProject.git");
        runner
            .run(&[
                "init".into(),
                "--bare".into(),
                "-q".into(),
                remote.as_os_str().to_os_string(),
            ])
            .expect("bare remote");

        let mut request = request(dir.path());
        request.name = "MyProject".to_string();
        request.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some(remote.to_string_lossy().to_string()),
            ..RepositoryRequest::default()
        }];
        let first = plan(&request, &runner).expect("plan");
        let applied = apply(&first, false, &runner, &mut SetupObserver::silent());
        assert!(applied.is_success(), "{}", applied.summary());

        let manifest_before =
            std::fs::read_to_string(dir.path().join(".gitmesh/project.toml")).expect("manifest");
        let second = plan(&request, &runner).expect("plan again");
        assert!(second.is_noop(), "{}", second.summary());
        assert!(second.is_ready(), "{:?}", second.blockers);
        assert_eq!(second.created_repositories().count(), 0);

        let again = apply(&second, false, &runner, &mut SetupObserver::silent());
        assert!(again.is_success(), "{}", again.summary());
        assert!(
            again.manifest_path.is_none(),
            "nothing was written a second time"
        );
        assert!(again.project.is_some(), "the project is still the result");
        let validation = again.validation.expect("validation runs anyway");
        assert!(validation.ok, "{:?}", validation.issues);
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".gitmesh/project.toml")).expect("manifest"),
            manifest_before,
            "the manifest is byte-for-byte the same"
        );
    }
}
