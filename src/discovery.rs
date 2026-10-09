//! Repository discovery and project configuration.
//!
//! GitMesh never guesses the user's architecture. Discovery answers factual questions
//! ("which directories are Git repositories?") and configuration applies the user's
//! explicit decisions ("this directory becomes an external repository"). Nothing in
//! this module moves, copies, deletes or otherwise touches user files: it only reads
//! the filesystem and Git metadata, and writes the manifest when asked to.
//!
//! Two entry points matter:
//!
//! * [`scan_project`] — read-only inspection producing a [`ProjectScan`] (the tree and
//!   the repositories found in it).
//! * [`assign_repository`] / [`unassign_repository`] — pure configuration operations
//!   that a future GUI/TUI calls, returning a new validated project which
//!   [`crate::manifest::save_project`] persists.

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::git::{GitRepo, GitRunner};
use crate::manifest;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryRole};
use crate::paths::{self, to_slash};

/// Directory names that are never presented as user content.
pub const IGNORED_DIRS: &[&str] = &[".git", ".gitmesh", "node_modules", "target", "__pycache__"];

/// Options for a directory scan.
#[derive(Debug, Clone)]
pub struct ScanOptions {
    /// Maximum directory depth to traverse (the project root is depth 0).
    pub max_depth: usize,
    /// Include hidden directories (leading dot) in the tree.
    pub include_hidden: bool,
    /// Stop descending into a directory that is the root of a Git repository. When
    /// false, the scan descends into nested repositories too.
    pub stop_at_repository_roots: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        ScanOptions {
            max_depth: 6,
            include_hidden: false,
            stop_at_repository_roots: true,
        }
    }
}

/// A directory in the scanned project tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryNode {
    /// Directory name (`"."` for the project root).
    pub name: String,
    /// Path relative to the project root.
    pub relative_path: PathBuf,
    /// True when this directory is the root of a Git repository.
    pub is_repository_root: bool,
    /// Absolute path of the repository that owns this directory, when known.
    pub repository_owner: Option<PathBuf>,
    /// Number of regular files directly in this directory.
    pub file_count: usize,
    /// Children, sorted by name.
    pub children: Vec<DirectoryNode>,
    /// True when the scan stopped here because of `max_depth`.
    pub truncated: bool,
}

impl DirectoryNode {
    /// Depth-first traversal along with the node depth.
    pub fn walk(&self, visitor: &mut impl FnMut(&DirectoryNode, usize)) {
        self.walk_from(0, visitor);
    }

    fn walk_from(&self, depth: usize, visitor: &mut impl FnMut(&DirectoryNode, usize)) {
        visitor(self, depth);
        for child in &self.children {
            child.walk_from(depth + 1, visitor);
        }
    }

    /// Find a node by project-relative path.
    pub fn find(&self, relative: &Path) -> Option<&DirectoryNode> {
        let relative = paths::lexical_normalize(relative);
        if paths::lexical_normalize(&self.relative_path) == relative {
            return Some(self);
        }
        self.children.iter().find_map(|child| child.find(&relative))
    }

    /// All repository roots in the tree, as project-relative paths.
    pub fn repository_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        self.walk(&mut |node, _| {
            if node.is_repository_root {
                roots.push(node.relative_path.clone());
            }
        });
        roots
    }
}

/// A Git repository found on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredRepository {
    /// Absolute path of the repository working directory.
    pub path: PathBuf,
    /// Path relative to the project root.
    pub relative_path: PathBuf,
    /// True when this is the project root itself.
    pub is_project_root: bool,
    /// Absolute path of the nearest enclosing repository, when this repository is
    /// nested inside another one.
    pub nested_inside: Option<PathBuf>,
    /// Current branch, when it could be read.
    pub branch: Option<String>,
    /// True when the repository has at least one commit.
    pub has_commits: bool,
    /// Number of tracked files.
    pub tracked_files: usize,
    /// Primary remote URL, when configured.
    pub remote_url: Option<String>,
}

/// Result of scanning a project directory.
#[derive(Debug, Clone)]
pub struct ProjectScan {
    /// Absolute project root.
    pub root: PathBuf,
    /// True when the `git` executable is usable.
    pub git_available: bool,
    /// True when the project root itself is inside a Git repository.
    pub root_is_repository: bool,
    /// Top level of the repository containing the project root, when there is one.
    pub enclosing_repository: Option<PathBuf>,
    /// The scanned directory tree.
    pub tree: DirectoryNode,
    /// All repositories found, project root first.
    pub repositories: Vec<DiscoveredRepository>,
    /// Non-fatal observations worth telling the user about.
    pub notices: Vec<String>,
}

impl ProjectScan {
    /// Repositories that are not the project root itself.
    pub fn external_candidates(&self) -> impl Iterator<Item = &DiscoveredRepository> {
        self.repositories.iter().filter(|r| !r.is_project_root)
    }
}

/// Scan a directory and report the Git repositories inside it.
///
/// Read-only: this function never writes to the project or to any repository.
pub fn scan_project(root: &Path, options: &ScanOptions, runner: &GitRunner) -> Result<ProjectScan> {
    let root = paths::lexical_normalize(root);
    if !root.is_dir() {
        return Err(Error::Other(format!(
            "{} is not a directory",
            root.display()
        )));
    }

    let mut notices = Vec::new();
    let git_available = runner.version().is_ok();
    if !git_available {
        notices.push(
            "the git executable is not available: repository detection is limited to the \
             presence of a .git directory"
                .to_string(),
        );
    }

    let root_query = runner.repo(&root);
    let enclosing_repository = if git_available {
        root_query.top_level()?
    } else {
        None
    };
    let root_is_repository = enclosing_repository
        .as_deref()
        .is_some_and(|top| paths::lexical_normalize(top) == root);
    if let Some(enclosing) = &enclosing_repository {
        if paths::lexical_normalize(enclosing) != root {
            notices.push(format!(
                "the project root is inside the Git repository rooted at {}; GitMesh expects \
                 the project root to be a repository root itself",
                enclosing.display()
            ));
        }
    } else if git_available {
        notices.push(
            "the project root is not a Git repository; run `git init` there before using \
             GitMesh operations that need history"
                .to_string(),
        );
    }

    let mut repositories: Vec<DiscoveredRepository> = Vec::new();
    let mut current_owner: Option<PathBuf> = if root_is_repository {
        Some(root.clone())
    } else {
        None
    };

    let tree = build_tree(
        &root,
        &root,
        options,
        git_available,
        runner,
        &mut repositories,
        &mut current_owner,
        0,
        &mut notices,
    )?;

    // Root repository first, then the rest by path.
    repositories.sort_by(|a, b| {
        b.is_project_root
            .cmp(&a.is_project_root)
            .then_with(|| a.relative_path.cmp(&b.relative_path))
    });

    Ok(ProjectScan {
        root,
        git_available,
        root_is_repository,
        enclosing_repository,
        tree,
        repositories,
        notices,
    })
}

#[allow(clippy::too_many_arguments)]
fn build_tree(
    project_root: &Path,
    dir: &Path,
    options: &ScanOptions,
    git_available: bool,
    runner: &GitRunner,
    repositories: &mut Vec<DiscoveredRepository>,
    current_owner: &mut Option<PathBuf>,
    depth: usize,
    notices: &mut Vec<String>,
) -> Result<DirectoryNode> {
    let relative = paths::project_relative(project_root, dir).unwrap_or_else(|| PathBuf::from("."));
    let name = if relative == Path::new(".") {
        ".".to_string()
    } else {
        dir.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| to_slash(&relative))
    };

    let repo_root_here = depth > 0 && is_repository_root(dir, git_available, runner);

    let mut node = DirectoryNode {
        name,
        relative_path: relative.clone(),
        is_repository_root: repo_root_here,
        repository_owner: current_owner.clone(),
        file_count: 0,
        children: Vec::new(),
        truncated: false,
    };

    if repo_root_here {
        let nested_inside = current_owner.clone();
        if let Some(inner) = &nested_inside {
            notices.push(format!(
                "'{}' is a Git repository nested inside the repository rooted at '{}'; \
                 it can be assigned to its own GitMesh repository, but note that the outer \
                 repository can still track files inside it",
                to_slash(&relative),
                to_slash(&paths::project_relative(project_root, inner).unwrap_or_default())
            ));
        }
        repositories.push(describe_repository(
            dir,
            project_root,
            runner,
            nested_inside,
        ));
        *current_owner = Some(dir.to_path_buf());
    }

    if relative == Path::new(".") {
        node.is_repository_root = false; // represented separately by ProjectScan::root_is_repository
        if is_root_repository(dir, git_available, runner) {
            repositories.push(describe_repository(dir, project_root, runner, None));
            *current_owner = Some(dir.to_path_buf());
        }
    }

    let entries = std::fs::read_dir(dir).map_err(|source| Error::io(dir.to_path_buf(), source))?;
    let mut child_dirs: Vec<PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::io(dir.to_path_buf(), source))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|source| Error::io(path.clone(), source))?;
        let file_name = entry.file_name().to_string_lossy().to_string();

        if file_type.is_dir() {
            if IGNORED_DIRS.contains(&file_name.as_str()) {
                continue;
            }
            if file_name.starts_with('.') && !options.include_hidden {
                continue;
            }
            child_dirs.push(path);
        } else if file_name == ".git" {
            // Handled by the repository-root detection above.
            continue;
        } else if !file_name.starts_with('.') || options.include_hidden {
            node.file_count += 1;
        }
    }
    child_dirs.sort();

    if depth >= options.max_depth {
        if !child_dirs.is_empty() {
            node.truncated = true;
        }
        return Ok(node);
    }

    let saved_owner = current_owner.clone();
    for child in child_dirs {
        let child_is_repo = is_repository_root(&child, git_available, runner);
        if child_is_repo && options.stop_at_repository_roots {
            // Register the repository, but do not descend into it.
            let mut repositories_sink = std::mem::take(repositories);
            let child_node = build_tree(
                project_root,
                &child,
                options,
                git_available,
                runner,
                &mut repositories_sink,
                current_owner,
                depth + 1,
                notices,
            )?;
            *repositories = repositories_sink;
            node.children.push(child_node);
            *current_owner = saved_owner.clone();
            continue;
        }
        let child_node = build_tree(
            project_root,
            &child,
            options,
            git_available,
            runner,
            repositories,
            current_owner,
            depth + 1,
            notices,
        )?;
        node.children.push(child_node);
        *current_owner = saved_owner.clone();
    }

    Ok(node)
}

fn is_root_repository(dir: &Path, git_available: bool, runner: &GitRunner) -> bool {
    if !git_available {
        return dir.join(".git").exists();
    }
    matches!(runner.repo(dir).top_level(), Ok(Some(top)) if paths::lexical_normalize(&top) == paths::lexical_normalize(dir))
}

/// True when `dir` is the root of its own Git repository.
///
/// Uses Git itself when available; falls back to the presence of a `.git` entry.
pub fn is_repository_root(dir: &Path, git_available: bool, runner: &GitRunner) -> bool {
    is_root_repository(dir, git_available, runner)
}

fn describe_repository(
    dir: &Path,
    project_root: &Path,
    runner: &GitRunner,
    nested_inside: Option<PathBuf>,
) -> DiscoveredRepository {
    let relative_path =
        paths::project_relative(project_root, dir).unwrap_or_else(|| PathBuf::from("."));
    let repo = runner.repo(dir);
    let head = repo.head().ok();
    let branch = head.as_ref().and_then(|h| h.branch().map(str::to_string));
    let has_commits = repo.head_oid().ok().flatten().is_some();
    let tracked_files = repo
        .run_optional(&["ls-files"])
        .ok()
        .flatten()
        .map(|out| out.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    let remote_url = repo.remotes().ok().and_then(|remotes| {
        remotes
            .first()
            .and_then(|r| r.fetch_url().map(str::to_string))
    });

    DiscoveredRepository {
        path: dir.to_path_buf(),
        relative_path,
        is_project_root: paths::lexical_normalize(dir) == paths::lexical_normalize(project_root),
        nested_inside: nested_inside.map(|p| paths::lexical_normalize(&p)),
        branch,
        has_commits,
        tracked_files,
        remote_url,
    }
}

/// The repository root that contains `path`, asked from Git itself.
pub fn find_repository_root(path: &Path, runner: &GitRunner) -> Result<Option<PathBuf>> {
    let query_path = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| Error::Other(format!("{} has no parent directory", path.display())))?
    };
    let repo = runner.repo(query_path);
    Ok(repo.top_level()?.map(|p| paths::lexical_normalize(&p)))
}

// ------------------------------------------------------------- configuration --

/// Result of checking whether a directory may become an external repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentCheck {
    /// Project-relative path of the candidate.
    pub relative_path: PathBuf,
    /// True when the directory exists.
    pub exists: bool,
    /// True when the directory is a Git repository root.
    pub is_repository: bool,
    /// True when GitMesh would raise its own repository at this path (`git init`
    /// needed) — allowed, but reported so the UI can ask for confirmation.
    pub requires_git_init: bool,
    /// Reasons the assignment cannot be applied.
    pub blockers: Vec<String>,
    /// Non-blocking remarks.
    pub warnings: Vec<String>,
}

impl AssignmentCheck {
    pub fn can_assign(&self) -> bool {
        self.blockers.is_empty()
    }
}

/// Check whether `relative_path` can become an external repository.
///
/// Detects every configuration mistake GitMesh knows about: non-existent paths,
/// paths outside the project, paths already assigned, paths overlapping an assigned
/// repository, and paths nested inside another external repository.
pub fn check_assignment(
    project: &GitMeshProject,
    relative_path: &Path,
    runner: &GitRunner,
) -> Result<AssignmentCheck> {
    let relative = paths::normalize_relative(&to_slash(relative_path))
        .map_err(|e| Error::Other(format!("invalid repository path: {e}")))?;
    let mut blockers = Vec::new();
    let mut warnings = Vec::new();

    if paths::is_root_relative(&relative) {
        blockers.push("the project root is always the root repository".to_string());
    }

    let absolute = project.root.join(&relative);
    let exists = absolute.exists();
    if !exists {
        blockers.push(format!(
            "directory '{}' does not exist",
            to_slash(&relative)
        ));
    }

    blockers.extend(ownership_conflicts(project, &relative));

    let is_repository = exists && is_repository_root(&absolute, true, runner);
    let requires_git_init = exists && !is_repository;
    if requires_git_init {
        warnings.push(format!(
            "'{}' is not a Git repository yet; GitMesh can create one there (git init) or you \
             can clone an existing repository into it",
            to_slash(&relative)
        ));
    }

    Ok(AssignmentCheck {
        relative_path: relative,
        exists,
        is_repository,
        requires_git_init,
        blockers,
        warnings,
    })
}

/// Configuration conflicts that stop a directory from becoming an external repository.
///
/// This is the *pure* half of [`check_assignment`]: it looks only at the project
/// configuration and the path, never at the filesystem or at Git. The setup planner runs
/// exactly these rules on a project it is still assembling, so the wizard and the CLI
/// cannot disagree about which layouts are legal.
pub fn assignment_conflicts(project: &GitMeshProject, relative_path: &Path) -> Vec<String> {
    let relative = paths::lexical_normalize(relative_path);
    let mut blockers = Vec::new();
    if paths::is_root_relative(&relative) {
        blockers.push("the project root is always the root repository".to_string());
    }
    blockers.extend(ownership_conflicts(project, &relative));
    blockers
}

/// Ownership problems: the path is already assigned, or it overlaps an assigned one.
fn ownership_conflicts(project: &GitMeshProject, relative: &Path) -> Vec<String> {
    let mut blockers = Vec::new();

    // Already assigned? (`repository_for_relative` also answers for paths *inside* an
    // assigned repository, which is a different problem and is reported as an overlap
    // below, with the accuracy the message needs.)
    if let Some(existing) = project.repository_for_relative(relative) {
        let same_path =
            paths::lexical_normalize(&existing.relative_path) == paths::lexical_normalize(relative);
        if !existing.is_root() && same_path {
            blockers.push(format!(
                "directory '{}' is already assigned to repository '{}'",
                to_slash(relative),
                existing.id
            ));
        }
    }

    // Overlaps with an assigned repository?
    for repo in project.external_repositories() {
        let assigned = paths::lexical_normalize(&repo.relative_path);
        if paths::is_strict_ancestor(&assigned, relative) {
            blockers.push(format!(
                "directory '{}' is inside the repository '{}' ('{}'); nested repository \
                 boundaries are not supported",
                to_slash(relative),
                repo.id,
                to_slash(&assigned)
            ));
        } else if paths::is_strict_ancestor(relative, &assigned) {
            blockers.push(format!(
                "directory '{}' contains the configured repository '{}' ('{}')",
                to_slash(relative),
                repo.id,
                to_slash(&assigned)
            ));
        }
    }

    blockers
}

/// Options used when a user marks a directory as an external repository.
#[derive(Debug, Clone, Default)]
pub struct AssignOptions {
    /// Logical identifier; defaults to the directory name.
    pub id: Option<String>,
    /// Remote URL to record (and configure as `origin` when the repository has none).
    pub remote_url: Option<String>,
    /// Create a Git repository in the directory when it is not one yet.
    pub init_git: bool,
    /// Logical branch hint.
    pub branch: Option<String>,
}

/// Mark a directory as an external repository.
///
/// Pure configuration: returns a new project. File system changes (`git init`, adding a
/// remote) only happen when [`AssignOptions::init_git`] is set, and they are limited to
/// the assigned directory.
pub fn assign_repository(
    project: &GitMeshProject,
    relative_path: &Path,
    options: &AssignOptions,
    runner: &GitRunner,
) -> Result<GitMeshProject> {
    let check = check_assignment(project, relative_path, runner)?;
    if !check.can_assign() {
        return Err(Error::InvalidConfiguration(check.blockers));
    }

    let absolute = project.root.join(&check.relative_path);
    let is_repo = is_repository_root(&absolute, true, runner);
    if !is_repo {
        if !options.init_git {
            return Err(Error::InvalidConfiguration(vec![format!(
                "'{}' is not a Git repository; pass --git-init to create one there",
                to_slash(&check.relative_path)
            )]));
        }
        initialize_repository(&absolute, runner)?;
    }

    if let Some(remote) = options.remote_url.as_deref() {
        ensure_origin_remote(&absolute, remote, runner)?;
    }

    let id = options
        .id
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| suggest_id(&check.relative_path, project));

    let mut candidate = project.clone();
    candidate.repositories.push(PhysicalRepository {
        id,
        role: RepositoryRole::External,
        absolute_path: absolute,
        relative_path: check.relative_path.clone(),
        remote_url: options.remote_url.clone().or_else(|| {
            runner
                .repo(project.root.join(&check.relative_path))
                .remotes()
                .ok()
                .and_then(|r| r.first().and_then(|r| r.fetch_url().map(str::to_string)))
        }),
        branch: options.branch.clone(),
    });
    candidate
        .repositories
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

    manifest::validation::validate_project(&candidate).map_err(Error::InvalidConfiguration)?;
    Ok(candidate)
}

/// Remove an external repository from the configuration.
///
/// Never touches the filesystem: the directory keeps its content and, if it is a Git
/// repository, its own history. It simply stops being managed as a separate physical
/// repository and falls back under the root repository.
pub fn unassign_repository(project: &GitMeshProject, id: &str) -> Result<GitMeshProject> {
    let repo = project
        .repository(id)
        .ok_or_else(|| Error::UnknownRepository(id.to_string()))?;
    if repo.is_root() {
        return Err(Error::UnknownRepository(format!(
            "{id} (the root repository cannot be removed)"
        )));
    }
    let mut candidate = project.clone();
    candidate.repositories.retain(|r| r.id != id);
    Ok(candidate)
}

/// Rename an external repository (logical id only).
pub fn rename_repository(
    project: &GitMeshProject,
    id: &str,
    new_id: &str,
) -> Result<GitMeshProject> {
    if project.repository(new_id).is_some() {
        return Err(Error::InvalidConfiguration(vec![format!(
            "a repository with the id '{new_id}' already exists"
        )]));
    }
    let mut candidate = project.clone();
    let repo = candidate
        .repositories
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or_else(|| Error::UnknownRepository(id.to_string()))?;
    repo.id = new_id.to_string();
    manifest::validation::validate_project(&candidate).map_err(Error::InvalidConfiguration)?;
    Ok(candidate)
}

/// Update the remote URL (and optionally the branch hint) of a repository.
pub fn set_repository_remote(
    project: &GitMeshProject,
    id: &str,
    remote_url: Option<String>,
    configure_git_remote: bool,
    runner: &GitRunner,
) -> Result<GitMeshProject> {
    let repo = project
        .repository(id)
        .ok_or_else(|| Error::UnknownRepository(id.to_string()))?
        .clone();

    if configure_git_remote {
        if let Some(url) = remote_url.as_deref() {
            let git_repo = runner.repo(&repo.absolute_path);
            if !git_repo.is_repository() {
                return Err(Error::NotARepository {
                    path: repo.absolute_path.clone(),
                });
            }
            ensure_origin_remote(&repo.absolute_path, url, runner)?;
        }
    }

    let mut candidate = project.clone();
    if let Some(target) = candidate.repositories.iter_mut().find(|r| r.id == id) {
        target.remote_url = remote_url;
    }
    manifest::validation::validate_project(&candidate).map_err(Error::InvalidConfiguration)?;
    Ok(candidate)
}

/// Suggest a logical id for a directory: its lowercased name, made safe for the manifest
/// and made unique within the project.
///
/// Both the CLI (`configure add` without `--id`) and the setup plan use this, so a
/// repository created by the wizard has the same name the CLI would have chosen.
pub fn suggest_id(relative: &Path, project: &GitMeshProject) -> String {
    let base = relative
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase().replace(' ', "-"))
        .unwrap_or_else(|| "repo".to_string());
    let base: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let base = if base.is_empty() {
        "repo".to_string()
    } else {
        base
    };
    if project.repository(&base).is_none() {
        return base;
    }
    for suffix in 2..1000 {
        let candidate = format!("{base}-{suffix}");
        if project.repository(&candidate).is_none() {
            return candidate;
        }
    }
    format!("{base}-{}", project.len())
}

/// What [`ensure_origin_remote`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OriginChange {
    /// `origin` did not exist and now points at the URL.
    Added,
    /// `origin` existed with a different URL and was updated.
    Updated,
    /// `origin` already pointed at that URL; nothing was run.
    Unchanged,
}

impl OriginChange {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            OriginChange::Added => "added",
            OriginChange::Updated => "updated",
            OriginChange::Unchanged => "unchanged",
        }
    }
}

/// Create a Git repository at `path` (`git init -q -b main`).
///
/// This is the only place that raises a repository for GitMesh, and it is shared by
/// `configure add --git-init`, `init --git-init` and the setup wizard so that all of them
/// create identical repositories. Callers check [`is_repository_root`] first, so an
/// existing repository is never re-initialised.
pub fn initialize_repository(path: &Path, runner: &GitRunner) -> Result<()> {
    let repo = runner.repo(path);
    repo.run_checked(&["init", "-q", "-b", "main"])?;
    Ok(())
}

/// True when `path` is an existing directory with no entries at all.
pub fn is_empty_directory(path: &Path) -> bool {
    path.is_dir()
        && std::fs::read_dir(path)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
}

/// Clone `url` into `path`, which must be missing or an empty directory.
///
/// Nothing that exists is overwritten: a non-empty directory is refused before Git runs. When
/// the remote has commits, its default branch is tracked; the return value is that branch, or
/// `None` when the remote is empty and the clone has no commits.
pub fn clone_repository(url: &str, path: &Path, runner: &GitRunner) -> Result<Option<String>> {
    if path.exists() && !is_empty_directory(path) {
        return Err(Error::Other(format!(
            "'{}' is not an empty directory; nothing was cloned",
            path.display()
        )));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::Other(format!("could not create '{}': {e}", parent.display())))?;
    }
    let target = path.to_string_lossy().to_string();
    runner
        .run(&["clone", "--quiet", "--", url, &target])?
        .require("clone")?;

    let repo = runner.repo(path);
    let default = repo
        .run_optional(&[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ])?
        .map(|name| name.trim().trim_start_matches("origin/").to_string())
        .filter(|name| !name.is_empty());
    let has_commits = repo.head_oid()?.is_some();
    match default {
        Some(branch) if has_commits => {
            repo.run_checked(&["branch", "--set-upstream-to", &format!("origin/{branch}")])?;
            Ok(Some(branch))
        }
        _ => Ok(None),
    }
}

/// Point `origin` of the repository at `repo_path` to `url`, adding it when missing.
///
/// Only the remote itself is touched: no fetch, no push, no branch is created. An
/// existing `origin` pointing elsewhere is replaced by *this* function, so callers that
/// must not overwrite a remote (the setup wizard) decide that before calling, and report
/// the change to the user.
pub fn ensure_origin_remote(
    repo_path: &Path,
    url: &str,
    runner: &GitRunner,
) -> Result<OriginChange> {
    let repo = runner.repo(repo_path);
    let existing = repo
        .remotes()?
        .into_iter()
        .find(|remote| remote.name == "origin")
        .and_then(|remote| remote.fetch_url().map(str::to_string));
    match existing {
        Some(current) if current == url => Ok(OriginChange::Unchanged),
        Some(_) => {
            repo.run_checked(&["remote", "set-url", "origin", url])?;
            Ok(OriginChange::Updated)
        }
        None => {
            repo.run_checked(&["remote", "add", "origin", url])?;
            Ok(OriginChange::Added)
        }
    }
}

/// Stop tracking `relative` in the repository rooted at `repo_root`.
///
/// Only the index changes: the files stay on disk, and if they also live in a repository
/// of their own nothing about that repository is touched. This is the one implementation
/// of "the parent repository must stop owning these files", used by the setup wizard and
/// by repository management.
pub fn untrack_from_root(repo_root: &Path, relative: &Path, runner: &GitRunner) -> Result<()> {
    let repo = runner.repo(repo_root);
    if !is_repository_root(repo_root, true, runner) {
        return Err(Error::NotARepository {
            path: repo_root.to_path_buf(),
        });
    }
    repo.run_checked(&[
        "rm".into(),
        "-r".into(),
        "--cached".into(),
        "-q".into(),
        "--".into(),
        relative.as_os_str().to_os_string(),
    ])?;
    Ok(())
}

/// Files the repository at `repo_root` tracks inside `relative`.
///
/// Zero means the parent repository owns nothing there, which is the state a boundary
/// change has to reach; non-zero means two repositories claim the same files.
pub fn count_files_tracked_under(repo_root: &Path, relative: &Path, runner: &GitRunner) -> usize {
    let repo = runner.repo(repo_root);
    if !is_repository_root(repo_root, true, runner) {
        return 0;
    }
    repo.tracked_files_under(relative)
        .map(|files| files.len())
        .unwrap_or(0)
}

/// URL currently configured as `origin`, if there is one.
pub fn origin_url(repo_path: &Path, runner: &GitRunner) -> Result<Option<String>> {
    Ok(runner
        .repo(repo_path)
        .remotes()?
        .into_iter()
        .find(|remote| remote.name == "origin")
        .and_then(|remote| remote.fetch_url().map(str::to_string)))
}

/// Git repositories nested inside `dir`, as paths relative to `dir`.
///
/// Read-only and shallow in intent: it answers "does this directory contain another
/// repository?" for a directory that is about to become a repository of its own, which
/// is exactly when GitMesh has to warn instead of guessing. Reuses the project scanner
/// so nesting is detected the same way everywhere.
pub fn scan_nested(dir: &Path, runner: &GitRunner) -> Vec<PathBuf> {
    let options = ScanOptions::default();
    match scan_project(dir, &options, runner) {
        Ok(scan) => scan
            .repositories
            .into_iter()
            .filter(|repo| !repo.is_project_root)
            .map(|repo| repo.relative_path)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A fresh project for `root`, containing only the root repository.
pub fn initial_project(
    root: &Path,
    name: Option<String>,
    root_remote: Option<String>,
    branch: Option<String>,
) -> Result<GitMeshProject> {
    let root = paths::lexical_normalize(root);
    let name = name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .or_else(|| root.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "project".to_string());
    let project = GitMeshProject {
        name,
        root: root.clone(),
        repositories: vec![PhysicalRepository {
            id: "root".to_string(),
            role: RepositoryRole::Root,
            relative_path: PathBuf::from("."),
            remote_url: root_remote,
            branch,
            absolute_path: root,
        }],
    };
    manifest::validation::validate_project(&project).map_err(Error::InvalidConfiguration)?;
    Ok(project)
}

/// Inspect a single configured repository (used by discovery reporting).
pub fn describe_configured_repository(
    project: &GitMeshProject,
    repo: &PhysicalRepository,
    runner: &GitRunner,
) -> Result<DiscoveredRepository> {
    let path = project.repository_path(repo);
    let nested_inside = if repo.is_root() {
        None
    } else {
        runner.repo(&path).top_level().ok().flatten()
    };
    Ok(describe_repository(
        &path,
        &project.root,
        runner,
        nested_inside,
    ))
}

/// Convenience: is `dir` (inside `project`) already covered by a configured
/// repository?
pub fn configured_owner<'a>(
    project: &'a GitMeshProject,
    dir: &Path,
) -> Option<&'a PhysicalRepository> {
    let relative = paths::project_relative(&project.root, dir)?;
    project.repository_for_relative(&relative)
}

/// A Git repository handle for a configured repository, after verifying that the
/// directory really is that repository's root.
pub fn verified_repo<'a>(
    runner: &'a GitRunner,
    repo: &PhysicalRepository,
    project_root: &Path,
) -> Result<GitRepo<'a>> {
    let path = if repo.relative_path == Path::new(".") {
        project_root.to_path_buf()
    } else {
        project_root.join(&repo.relative_path)
    };
    if !path.is_dir() {
        return Err(Error::Other(format!(
            "repository '{}' is missing: {} does not exist",
            repo.id,
            path.display()
        )));
    }
    let handle = runner.repo(path);
    handle.verify_identity()?;
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> GitRunner {
        GitRunner::detect().unwrap()
    }

    struct Fixture {
        dir: crate::testkit::TempDir,
        runner: GitRunner,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                dir: crate::testkit::TempDir::new("discovery-tests").unwrap(),
                runner: runner(),
            }
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        fn git_init(&self, relative: &str) -> PathBuf {
            let path = if relative == "." {
                self.path().to_path_buf()
            } else {
                self.path().join(relative)
            };
            std::fs::create_dir_all(&path).unwrap();
            let repo = self.runner.repo(&path);
            repo.run_checked(&["init", "-q", "-b", "main"]).unwrap();
            repo.run_checked(&["config", "user.email", "t@example.com"])
                .unwrap();
            repo.run_checked(&["config", "user.name", "T"]).unwrap();
            path
        }

        fn commit(&self, relative: &str, file: &str, content: &str) {
            let path = if relative == "." {
                self.path().to_path_buf()
            } else {
                self.path().join(relative)
            };
            let full = path.join(file);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
            let repo = self.runner.repo(&path);
            repo.run_checked(&["add", "-A"]).unwrap();
            repo.run_checked(&["commit", "-q", "-m", "initial"])
                .unwrap();
        }
    }

    #[test]
    fn scans_a_plain_directory_without_repositories() {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.path().join("src")).unwrap();
        std::fs::write(fx.path().join("README.md"), "hi").unwrap();
        let scan = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        assert!(!scan.root_is_repository);
        assert!(scan.repositories.is_empty());
        assert!(scan.tree.find(Path::new("src")).is_some());
        assert!(scan
            .notices
            .iter()
            .any(|n| n.contains("not a Git repository")));
    }

    #[test]
    fn scans_an_empty_project_directory() {
        let fx = Fixture::new();
        let scan = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        assert!(scan.tree.children.is_empty());
        assert_eq!(scan.tree.file_count, 0);
        assert!(scan.repositories.is_empty());
    }

    #[test]
    fn detects_the_root_repository() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.commit(".", "README.md", "hello");
        let scan = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        assert!(scan.root_is_repository);
        assert_eq!(scan.repositories.len(), 1);
        assert!(scan.repositories[0].is_project_root);
        assert_eq!(scan.repositories[0].tracked_files, 1);
        assert_eq!(scan.repositories[0].branch.as_deref(), Some("main"));
    }

    #[test]
    fn detects_root_plus_one_nested_repository() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.commit(".", "README.md", "root");
        fx.git_init("engine");
        fx.commit("engine", "lib.rs", "fn main() {}");

        let scan = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        assert_eq!(scan.repositories.len(), 2);
        let engine = scan
            .external_candidates()
            .find(|r| r.relative_path == Path::new("engine"))
            .unwrap();
        assert_eq!(
            engine.nested_inside.as_deref(),
            Some(paths::lexical_normalize(fx.path()).as_path())
        );
        assert!(
            scan.tree
                .find(Path::new("engine"))
                .unwrap()
                .is_repository_root
        );
        assert!(scan.notices.iter().any(|n| n.contains("nested inside")));
    }

    #[test]
    fn detects_repositories_several_levels_deep() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.git_init("a/b/c");
        fx.commit("a/b/c", "f.txt", "x");
        let scan = scan_project(
            fx.path(),
            &ScanOptions {
                max_depth: 8,
                ..Default::default()
            },
            &fx.runner,
        )
        .unwrap();
        let deep = scan
            .external_candidates()
            .find(|r| r.relative_path == Path::new("a/b/c"))
            .unwrap();
        assert!(deep.has_commits);
    }

    #[test]
    fn non_git_directories_are_not_repositories() {
        let fx = Fixture::new();
        std::fs::create_dir_all(fx.path().join("docs")).unwrap();
        let scan = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        assert!(scan.repositories.is_empty());
        assert!(
            !scan
                .tree
                .find(Path::new("docs"))
                .unwrap()
                .is_repository_root
        );
    }

    #[test]
    fn finds_the_repository_root_of_a_path() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.git_init("engine");
        let inside = fx.path().join("engine/src");
        std::fs::create_dir_all(&inside).unwrap();
        let found = find_repository_root(&inside, &fx.runner).unwrap().unwrap();
        assert_eq!(found, paths::lexical_normalize(&fx.path().join("engine")));
    }

    #[test]
    fn ignores_git_and_metadata_directories() {
        let fx = Fixture::new();
        fx.git_init(".");
        std::fs::create_dir_all(fx.path().join(".gitmesh")).unwrap();
        std::fs::create_dir_all(fx.path().join("node_modules/pkg")).unwrap();
        let scan = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        assert!(scan.tree.find(Path::new(".git")).is_none());
        assert!(scan.tree.find(Path::new(".gitmesh")).is_none());
        assert!(scan.tree.find(Path::new("node_modules")).is_none());
    }

    #[test]
    fn scan_does_not_modify_anything() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.commit(".", "README.md", "hello");
        std::fs::write(fx.path().join("dirty.txt"), "uncommitted").unwrap();
        let before = std::fs::read_dir(fx.path()).unwrap().count();
        let status_before = fx.runner.repo(fx.path()).status().unwrap();
        let _ = scan_project(fx.path(), &ScanOptions::default(), &fx.runner).unwrap();
        let after = std::fs::read_dir(fx.path()).unwrap().count();
        assert_eq!(before, after);
        let status_after = fx.runner.repo(fx.path()).status().unwrap();
        assert_eq!(status_before.entries.len(), status_after.entries.len());
        assert!(fx.path().join("dirty.txt").is_file());
    }

    #[test]
    fn assignment_requires_an_existing_git_repository() {
        let fx = Fixture::new();
        fx.git_init(".");
        let project = initial_project(fx.path(), None, None, None).unwrap();

        // Missing directory.
        let check = check_assignment(&project, Path::new("nope"), &fx.runner).unwrap();
        assert!(!check.can_assign());
        assert!(check.blockers[0].contains("does not exist"));

        // Existing but not a repository.
        std::fs::create_dir_all(fx.path().join("plain")).unwrap();
        let check = check_assignment(&project, Path::new("plain"), &fx.runner).unwrap();
        assert!(check.can_assign());
        assert!(check.requires_git_init);
        let err = assign_repository(
            &project,
            Path::new("plain"),
            &AssignOptions::default(),
            &fx.runner,
        )
        .unwrap_err();
        assert!(err.to_string().contains("--git-init"), "{err}");

        // With init, it works and becomes a real repository.
        let project = assign_repository(
            &project,
            Path::new("plain"),
            &AssignOptions {
                init_git: true,
                ..Default::default()
            },
            &fx.runner,
        )
        .unwrap();
        assert_eq!(project.len(), 2);
        assert!(fx.runner.repo(fx.path().join("plain")).is_repository());

        // Assigning again is refused.
        let check = check_assignment(&project, Path::new("plain"), &fx.runner).unwrap();
        assert!(
            check
                .blockers
                .iter()
                .any(|b| b.contains("already assigned")),
            "{:?}",
            check.blockers
        );
    }

    #[test]
    fn assignment_rejects_overlapping_and_nested_paths() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.git_init("engine/deep");
        let project = initial_project(fx.path(), None, None, None).unwrap();
        let project = assign_repository(
            &project,
            Path::new("engine/deep"),
            &AssignOptions::default(),
            &fx.runner,
        )
        .unwrap();

        // Nested inside an assigned repository.
        let check = check_assignment(&project, Path::new("engine"), &fx.runner).unwrap();
        assert!(!check.can_assign());
        assert!(
            check
                .blockers
                .iter()
                .any(|b| b.contains("contains the configured repository")),
            "{:?}",
            check.blockers
        );
    }

    #[test]
    fn unassign_and_rename_are_pure_and_validated() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.git_init("engine");
        let project = initial_project(fx.path(), None, None, None).unwrap();
        let project = assign_repository(
            &project,
            Path::new("engine"),
            &AssignOptions::default(),
            &fx.runner,
        )
        .unwrap();

        let renamed = rename_repository(&project, "engine", "core").unwrap();
        assert!(renamed.repository("core").is_some());
        assert!(renamed.repository("engine").is_none());

        // Renaming to an existing id fails.
        assert!(rename_repository(&project, "engine", "root").is_err());

        let unassigned = unassign_repository(&project, "engine").unwrap();
        assert_eq!(unassigned.len(), 1);
        assert!(unassign_repository(&project, "root").is_err());
        assert!(unassign_repository(&project, "nope").is_err());

        // The directory and its Git history are untouched.
        assert!(fx.path().join("engine/.git").exists());
    }

    #[test]
    fn assignment_can_configure_a_remote() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.git_init("engine");
        let project = initial_project(fx.path(), None, None, None).unwrap();
        let options = AssignOptions {
            id: Some("engine".into()),
            remote_url: Some("git@github.com:acme/engine.git".into()),
            ..Default::default()
        };
        let project =
            assign_repository(&project, Path::new("engine"), &options, &fx.runner).unwrap();
        assert_eq!(
            project.repository("engine").unwrap().remote_url.as_deref(),
            Some("git@github.com:acme/engine.git")
        );
        let remotes = fx.runner.repo(fx.path().join("engine")).remotes().unwrap();
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(
            remotes[0].fetch_url(),
            Some("git@github.com:acme/engine.git")
        );
    }

    #[test]
    fn ids_are_derived_uniquely_from_directory_names() {
        let fx = Fixture::new();
        fx.git_init(".");
        fx.git_init("apps/api");
        fx.git_init("services/api");
        let project = initial_project(fx.path(), None, None, None).unwrap();
        let project = assign_repository(
            &project,
            Path::new("apps/api"),
            &AssignOptions::default(),
            &fx.runner,
        )
        .unwrap();
        let project = assign_repository(
            &project,
            Path::new("services/api"),
            &AssignOptions::default(),
            &fx.runner,
        )
        .unwrap();
        let ids: Vec<&str> = project
            .external_repositories()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(ids, vec!["api", "api-2"]);
    }

    #[test]
    fn missing_repositories_are_detected_when_verifying() {
        let fx = Fixture::new();
        fx.git_init(".");
        let project = initial_project(fx.path(), None, None, None).unwrap();
        let mut project = project;
        project.repositories.push(PhysicalRepository {
            id: "ghost".into(),
            role: RepositoryRole::External,
            relative_path: PathBuf::from("ghost"),
            remote_url: None,
            branch: None,
            absolute_path: fx.path().join("ghost"),
        });
        let err = verified_repo(
            &fx.runner,
            project.repository("ghost").unwrap(),
            &project.root,
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing"), "{err}");
    }

    #[test]
    fn verified_repo_rejects_a_directory_that_is_not_its_own_repository_root() {
        let fx = Fixture::new();
        fx.git_init(".");
        std::fs::create_dir_all(fx.path().join("sub")).unwrap();
        let mut project = initial_project(fx.path(), None, None, None).unwrap();
        project.repositories.push(PhysicalRepository {
            id: "sub".into(),
            role: RepositoryRole::External,
            relative_path: PathBuf::from("sub"),
            remote_url: None,
            branch: None,
            absolute_path: fx.path().join("sub"),
        });
        let err = verified_repo(
            &fx.runner,
            project.repository("sub").unwrap(),
            &project.root,
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to operate"), "{err}");
    }
}
