//! The central internal model: one logical GitMesh project and the physical
//! repositories it is made of.
//!
//! This module is deliberately free of I/O. It is the shared vocabulary used by the
//! manifest, discovery, the analyzer, orchestration and the UI. Everything else in
//! GitMesh is built around two ideas:
//!
//! * **Logical path**: always project-relative (`engine/src/lib.rs`). This is what the
//!   user sees and what GitMesh uses for ownership decisions.
//! * **Physical repository**: the place where Git actually records history. Every
//!   logical path belongs to exactly one physical repository, determined by the
//!   longest matching configured repository path.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::git::{Head, Remote, RepoStatus};
use crate::paths::to_slash;

/// Role of a physical repository inside a logical project.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryRole {
    /// The repository that owns the project root and everything not assigned to an
    /// external repository.
    Root,
    /// A repository whose boundary was explicitly chosen by the user.
    External,
}

impl RepositoryRole {
    pub fn label(self) -> &'static str {
        match self {
            RepositoryRole::Root => "root",
            RepositoryRole::External => "external",
        }
    }
}

impl fmt::Display for RepositoryRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A physical repository as described by the project configuration.
///
/// This is *configuration*, not state: it says where the repository is and where it
/// is hosted, not what it currently contains. Use [`RepositoryState`] for the live
/// state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalRepository {
    /// Logical identifier, unique in the project (`root`, `engine`, ...).
    pub id: String,
    /// Root or external.
    pub role: RepositoryRole,
    /// Path relative to the project root; `.` for the root repository.
    pub relative_path: PathBuf,
    /// Remote URL, when the repository is hosted somewhere.
    pub remote_url: Option<String>,
    /// Logical branch to use when this repository needs an explicit one. `None` means
    /// "follow whatever GitMesh is told at checkout time"; the configuration is
    /// advisory, Git remains authoritative for the current branch.
    pub branch: Option<String>,
    /// Absolute path, resolved against the project root.
    pub absolute_path: PathBuf,
}

impl PhysicalRepository {
    /// True for the repository that owns the project root.
    pub fn is_root(&self) -> bool {
        self.role == RepositoryRole::Root
    }

    /// Project-relative path as used in user-facing output and the manifest.
    pub fn relative_slash(&self) -> String {
        to_slash(&self.relative_path)
    }

    /// Display name: the logical id, which is what the user thinks in.
    pub fn display_name(&self) -> &str {
        &self.id
    }
}

/// A logical GitMesh project.
///
/// Invariants (enforced by [`crate::manifest`] and re-checked here):
///
/// 1. exactly one repository has the [`RepositoryRole::Root`] role and its path is
///    the project root itself;
/// 2. repository ids are unique;
/// 3. repository paths are unique and non-overlapping (no repository path is an
///    ancestor of another).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMeshProject {
    /// Logical project name (defaults to the root directory name).
    pub name: String,
    /// Absolute project root directory.
    pub root: PathBuf,
    /// All physical repositories; the root repository is always first.
    pub repositories: Vec<PhysicalRepository>,
}

impl GitMeshProject {
    /// All repositories in a stable order, root first, then externals sorted by path.
    pub fn sorted_repositories(&self) -> Vec<&PhysicalRepository> {
        let mut repos: Vec<&PhysicalRepository> = self.repositories.iter().collect();
        repos.sort_by(|a, b| {
            b.is_root()
                .cmp(&a.is_root())
                .then_with(|| a.relative_path.cmp(&b.relative_path))
        });
        repos
    }

    /// The repository that owns the project root.
    pub fn root_repository(&self) -> &PhysicalRepository {
        self.repositories
            .iter()
            .find(|r| r.is_root())
            .expect("a GitMeshProject always contains a root repository")
    }

    /// Look up a repository by logical id.
    pub fn repository(&self, id: &str) -> Option<&PhysicalRepository> {
        self.repositories.iter().find(|r| r.id == id)
    }

    /// All repositories that are not the root repository.
    pub fn external_repositories(&self) -> impl Iterator<Item = &PhysicalRepository> {
        self.repositories.iter().filter(|r| !r.is_root())
    }

    /// Absolute path of a repository working directory.
    pub fn repository_path(&self, repo: &PhysicalRepository) -> PathBuf {
        if repo.relative_path == Path::new(".") {
            self.root.clone()
        } else {
            self.root.join(&repo.relative_path)
        }
    }

    /// The repository that owns a project-relative path.
    ///
    /// Ownership is decided by the **longest** matching repository path, which is what
    /// makes ownership unambiguous even though the root repository is conceptually an
    /// ancestor of every external repository.
    pub fn repository_for_relative(&self, relative: &Path) -> Option<&PhysicalRepository> {
        let relative = normalize_logical(relative);
        let mut best: Option<&PhysicalRepository> = None;
        for repo in &self.repositories {
            let candidate = normalize_logical(&repo.relative_path);
            let matches = candidate == Path::new(".")
                || relative == candidate
                || relative.starts_with(&candidate);
            if !matches {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    component_count(&candidate)
                        > component_count(&normalize_logical(&current.relative_path))
                }
            };
            if better {
                best = Some(repo);
            }
        }
        best
    }

    /// The repository that owns an absolute path inside the project.
    pub fn repository_for_path(&self, path: &Path) -> Option<&PhysicalRepository> {
        let relative = crate::paths::project_relative(&self.root, path)?;
        self.repository_for_relative(&relative)
    }

    /// Total number of physical repositories.
    pub fn len(&self) -> usize {
        self.repositories.len()
    }

    pub fn is_empty(&self) -> bool {
        self.repositories.is_empty()
    }
}

fn component_count(path: &Path) -> usize {
    path.components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .count()
}

/// Normalise a logical path for ownership comparisons.
fn normalize_logical(path: &Path) -> PathBuf {
    let cleaned = crate::paths::lexical_normalize(path);
    if cleaned.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        cleaned
    }
}

// ------------------------------------------------------------------- live state --

/// Live state of one physical repository.
///
/// A repository that could not be inspected is still represented: `error` explains
/// why, and `exists`/`is_repository` say what was found. This keeps project-wide
/// operations resilient (one broken repository must not hide the others) while never
/// pretending that a failure is a success.
#[derive(Debug, Clone)]
pub struct RepositoryState {
    /// Logical id.
    pub id: String,
    /// Root or external.
    pub role: RepositoryRole,
    /// Absolute working directory.
    pub path: PathBuf,
    /// Path relative to the project root (`/`-separated for display).
    pub relative_path: String,
    /// The configured path exists on disk.
    pub exists: bool,
    /// The path is a Git repository *whose top level is exactly this path*.
    pub is_repository: bool,
    /// `git status` result, when it could be read.
    pub status: Option<RepoStatus>,
    /// Configured remotes.
    pub remotes: Vec<Remote>,
    /// Long-running operation currently in progress (`merge`, `rebase`, ...).
    pub in_progress: Option<crate::git::InProgressOperation>,
    /// Why this repository could not be inspected, if it could not be.
    pub error: Option<String>,
}

impl RepositoryState {
    /// A repository entry that has not been inspected yet.
    pub fn unloaded(repo: &PhysicalRepository, project_root: &Path) -> Self {
        RepositoryState {
            id: repo.id.clone(),
            role: repo.role,
            path: if repo.relative_path == Path::new(".") {
                project_root.to_path_buf()
            } else {
                project_root.join(&repo.relative_path)
            },
            relative_path: repo.relative_slash(),
            exists: false,
            is_repository: false,
            status: None,
            remotes: Vec::new(),
            in_progress: None,
            error: None,
        }
    }

    /// True when GitMesh could inspect this repository.
    pub fn is_usable(&self) -> bool {
        self.error.is_none() && self.exists && self.is_repository && self.status.is_some()
    }

    pub fn head(&self) -> Head {
        self.status
            .as_ref()
            .map(|s| s.head.clone())
            .unwrap_or(Head::Unknown)
    }

    pub fn branch(&self) -> Option<String> {
        self.head().branch().map(str::to_string)
    }

    /// Any change at all in the working tree (tracked or untracked).
    pub fn has_changes(&self) -> bool {
        self.status.as_ref().is_some_and(|s| !s.is_fully_clean())
    }

    /// Tracked changes (staged, unstaged, deleted, renamed, conflicted).
    pub fn has_tracked_changes(&self) -> bool {
        self.status.as_ref().is_some_and(|s| !s.is_clean_tracked())
    }

    pub fn has_conflicts(&self) -> bool {
        self.status.as_ref().is_some_and(RepoStatus::has_conflicts)
    }

    pub fn ahead(&self) -> Option<u32> {
        self.status.as_ref().and_then(|s| s.ahead)
    }

    pub fn behind(&self) -> Option<u32> {
        self.status.as_ref().and_then(|s| s.behind)
    }

    /// Number of local commits not present upstream.
    pub fn is_ahead(&self) -> bool {
        self.ahead().is_some_and(|n| n > 0)
    }

    /// The upstream URL the repository pushes to, preferring `origin`.
    pub fn primary_remote_url(&self) -> Option<&str> {
        self.remotes
            .iter()
            .find(|r| r.name == "origin")
            .and_then(|r| r.push_url())
            .or_else(|| self.remotes.first().and_then(|r| r.push_url()))
    }

    /// True when the primary remote points at GitHub.
    pub fn is_github(&self) -> bool {
        self.primary_remote_url()
            .map(crate::git::classify_remote)
            .is_some_and(|kind| kind == crate::git::RemoteKind::GitHub)
    }

    /// Short, user-facing description of the state.
    pub fn summary(&self) -> String {
        if let Some(error) = &self.error {
            return format!("unavailable: {error}");
        }
        if !self.exists {
            return "missing (directory does not exist)".to_string();
        }
        if !self.is_repository {
            return "not a Git repository".to_string();
        }
        let Some(status) = &self.status else {
            return "unknown".to_string();
        };
        let mut parts = vec![status.head.label()];
        let counts = change_counts(status);
        if counts.conflict > 0 {
            parts.push(format!("{} conflicted", counts.conflict));
        }
        if counts.staged > 0 {
            parts.push(format!("{} staged", counts.staged));
        }
        if counts.unstaged > 0 {
            parts.push(format!("{} modified", counts.unstaged));
        }
        if counts.untracked > 0 {
            parts.push(format!("{} untracked", counts.untracked));
        }
        if let Some(ahead) = status.ahead.filter(|a| *a > 0) {
            parts.push(format!("ahead {ahead}"));
        }
        if let Some(behind) = status.behind.filter(|b| *b > 0) {
            parts.push(format!("behind {behind}"));
        }
        if parts.len() == 1 {
            parts.push("clean".to_string());
        }
        parts.join(", ")
    }
}

/// Counts of the different change categories of a status.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChangeCounts {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
    pub conflict: usize,
}

/// Count change categories of a status.
pub fn change_counts(status: &RepoStatus) -> ChangeCounts {
    let mut counts = ChangeCounts::default();
    for entry in &status.entries {
        if entry.is_conflict() {
            counts.conflict += 1;
        }
        if entry.untracked {
            counts.untracked += 1;
            continue;
        }
        if entry.staged {
            counts.staged += 1;
        }
        if entry.unstaged && !entry.is_conflict() {
            counts.unstaged += 1;
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(id: &str, role: RepositoryRole, path: &str) -> PhysicalRepository {
        PhysicalRepository {
            id: id.to_string(),
            role,
            relative_path: PathBuf::from(path),
            remote_url: None,
            branch: None,
            absolute_path: PathBuf::from("/project").join(path),
        }
    }

    fn project() -> GitMeshProject {
        GitMeshProject {
            name: "demo".into(),
            root: PathBuf::from("/project"),
            repositories: vec![
                PhysicalRepository {
                    relative_path: PathBuf::from("."),
                    ..repo("root", RepositoryRole::Root, ".")
                },
                repo("engine", RepositoryRole::External, "engine"),
                repo("renderer", RepositoryRole::External, "renderer"),
                repo("nested", RepositoryRole::External, "engine/sub/nested"),
            ],
        }
    }

    #[test]
    fn finds_root_repository() {
        let project = project();
        assert_eq!(project.root_repository().id, "root");
        assert_eq!(
            project
                .external_repositories()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            vec!["engine", "renderer", "nested"]
        );
    }

    #[test]
    fn ownership_is_decided_by_the_longest_matching_path() {
        let project = project();
        let owner = |p: &str| {
            project
                .repository_for_relative(Path::new(p))
                .map(|r| r.id.clone())
                .unwrap()
        };
        assert_eq!(owner("src/main.rs"), "root");
        assert_eq!(owner("."), "root");
        assert_eq!(owner("engine/src/lib.rs"), "engine");
        assert_eq!(owner("engine"), "engine");
        assert_eq!(owner("renderer/index.js"), "renderer");
        assert_eq!(owner("engine/sub/nested/file"), "nested");
        assert_eq!(owner("engine/sub/other/file"), "engine");
    }

    #[test]
    fn ownership_by_absolute_path() {
        let project = project();
        assert_eq!(
            project
                .repository_for_path(Path::new("/project/engine/src/main.rs"))
                .unwrap()
                .id,
            "engine"
        );
        assert!(project
            .repository_for_path(Path::new("/elsewhere/file"))
            .is_none());
    }

    #[test]
    fn repository_paths_are_resolved_against_the_root() {
        let project = project();
        assert_eq!(
            project.repository_path(project.root_repository()),
            PathBuf::from("/project")
        );
        assert_eq!(
            project.repository_path(project.repository("engine").unwrap()),
            PathBuf::from("/project/engine")
        );
    }

    #[test]
    fn sorted_repositories_put_the_root_first() {
        let project = project();
        let sorted: Vec<&str> = project
            .sorted_repositories()
            .iter()
            .map(|r| r.id.as_str())
            .collect();
        assert_eq!(sorted[0], "root");
        assert_eq!(sorted.len(), 4);
    }

    #[test]
    fn unloaded_state_reports_missing_repositories() {
        let project = project();
        let state = RepositoryState::unloaded(project.repository("engine").unwrap(), &project.root);
        assert!(!state.exists);
        assert!(!state.is_usable());
        assert_eq!(state.summary(), "missing (directory does not exist)");
        assert_eq!(state.relative_path, "engine");
    }

    #[test]
    fn change_counts_classify_conflicts_separately() {
        let raw = "1 .M N... 100644 100644 100644 aaa bbb a\0\
                   1 M. N... 100644 100644 100644 aaa bbb b\0\
                   ? c\0\
                   1 .D N... 100644 100644 100644 aaa bbb d\0";
        let status = crate::git::parse_porcelain_v2(raw, Path::new("/p"), false).unwrap();
        let counts = change_counts(&status);
        assert_eq!(counts.unstaged, 2); // a, d
        assert_eq!(counts.staged, 1); // b
        assert_eq!(counts.untracked, 1); // c
        assert_eq!(counts.conflict, 0);
    }
}
