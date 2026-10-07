//! Change analysis: one logical project status over many physical repositories.
//!
//! The analyzer is the read-only heart of GitMesh. It answers two questions that
//! everything else depends on:
//!
//! 1. **What is the state of the project?** — one [`ProjectStatus`] aggregating every
//!    physical repository.
//! 2. **Which physical repository owns this path?** — ownership is derived from the
//!    manifest with the longest-match rule (see
//!    [`GitMeshProject::repository_for_relative`]), never guessed from the filesystem.
//!
//! Repository handles are always obtained through [`crate::discovery::verified_repo`],
//! which refuses to act unless the directory really is the configured repository's
//! root. That guarantee is what makes it impossible for a project-wide operation to
//! run a Git command against the wrong repository.

use std::path::{Path, PathBuf};

use crate::discovery::verified_repo;
use crate::error::{Error, Result};
use crate::git::{short_oid, GitRepo, GitRunner, StatusEntry};
use crate::json::Json;
use crate::model::{change_counts, GitMeshProject, PhysicalRepository, RepositoryState};
use crate::paths::{self, to_slash};

/// Aggregated state of a logical project.
#[derive(Debug, Clone)]
pub struct ProjectStatus {
    /// Logical project name.
    pub name: String,
    /// Absolute project root.
    pub root: PathBuf,
    /// State of every physical repository, root first.
    pub repositories: Vec<RepositoryState>,
    /// Observations that do not fit a single repository (ownership conflicts,
    /// configuration remarks, ...).
    pub notices: Vec<String>,
}

impl ProjectStatus {
    /// Repositories that could not be inspected.
    pub fn unavailable(&self) -> impl Iterator<Item = &RepositoryState> {
        self.repositories.iter().filter(|r| !r.is_usable())
    }

    /// Repositories with changes (tracked or untracked).
    pub fn changed(&self) -> impl Iterator<Item = &RepositoryState> {
        self.repositories.iter().filter(|r| r.has_changes())
    }

    /// Repositories with merge conflicts.
    pub fn conflicted(&self) -> impl Iterator<Item = &RepositoryState> {
        self.repositories.iter().filter(|r| r.has_conflicts())
    }

    /// Repositories with nothing to do.
    pub fn clean(&self) -> impl Iterator<Item = &RepositoryState> {
        self.repositories
            .iter()
            .filter(|r| r.is_usable() && !r.has_changes())
    }

    /// True when any repository has a change.
    pub fn has_changes(&self) -> bool {
        self.changed().next().is_some()
    }

    /// True when any repository has a merge conflict.
    pub fn has_conflicts(&self) -> bool {
        self.conflicted().next().is_some()
    }

    /// Total number of changes across repositories.
    pub fn total_changes(&self) -> usize {
        self.repositories
            .iter()
            .filter_map(|r| r.status.as_ref())
            .map(|s| s.entries.iter().filter(|e| !e.ignored).count())
            .sum()
    }

    /// All branches the repositories are on, for detecting a "split brain" project
    /// where physical repositories ended up on different branches.
    pub fn branches(&self) -> Vec<(String, Option<String>)> {
        self.repositories
            .iter()
            .map(|r| (r.id.clone(), r.branch()))
            .collect()
    }

    /// The branch the logical project is on.
    ///
    /// The root repository decides: it owns the project root, so its branch is the
    /// project's branch. Everything else is reported as an outlier.
    pub fn reference_branch(&self) -> Option<String> {
        self.repositories
            .iter()
            .find(|r| r.role == crate::model::RepositoryRole::Root)
            .and_then(|r| r.branch())
    }

    /// Repositories that are not on the project branch, and repositories whose branch
    /// could not be determined. Empty when the whole project is on one branch.
    pub fn inconsistent_branches(&self) -> Vec<&RepositoryState> {
        let Some(reference) = self.reference_branch() else {
            return Vec::new();
        };
        self.repositories
            .iter()
            .filter(|r| r.branch().as_deref() != Some(reference.as_str()))
            .collect()
    }
}

/// One change, attributed to the physical repository that owns it.
#[derive(Debug, Clone)]
pub struct OwnedChange {
    /// Logical repository id (`root`, `engine`, ...).
    pub repository_id: String,
    /// Repository role.
    pub role: crate::model::RepositoryRole,
    /// Project-relative path of the owning repository (`.` for the root repository).
    ///
    /// Carried on the change itself so that front ends can group changes by repository
    /// without a second lookup into the project.
    pub repository_path: String,
    /// Path relative to the owning repository (what `git` reports).
    pub repo_relative: String,
    /// Path relative to the project root (what the user sees).
    pub logical_path: String,
    /// The underlying status entry.
    pub entry: StatusEntry,
}

impl OwnedChange {
    /// True when the change is a merge conflict.
    pub fn is_conflict(&self) -> bool {
        self.entry.is_conflict()
    }
}

/// Read-only analysis of a project.
pub struct Analyzer<'a> {
    runner: &'a GitRunner,
    project: &'a GitMeshProject,
}

impl<'a> Analyzer<'a> {
    pub fn new(project: &'a GitMeshProject, runner: &'a GitRunner) -> Self {
        Analyzer { runner, project }
    }

    /// The project being analysed.
    pub fn project(&self) -> &GitMeshProject {
        self.project
    }

    /// Inspect one configured repository.
    pub fn inspect_repository(&self, repo: &PhysicalRepository) -> RepositoryState {
        let mut state = RepositoryState::unloaded(repo, &self.project.root);
        let path = &state.path;
        if !path.exists() {
            state.error = Some(format!("directory does not exist: {}", path.display()));
            return state;
        }
        state.exists = true;

        let handle = match self.runner.repo(path).top_level() {
            Ok(Some(top)) => {
                if paths::lexical_normalize(&top) == paths::lexical_normalize(path) {
                    Some(self.runner.repo(path))
                } else {
                    state.error = Some(format!(
                        "not a repository root: it is inside the repository rooted at {}",
                        top.display()
                    ));
                    None
                }
            }
            Ok(None) => {
                state.error = Some("not a Git repository".to_string());
                None
            }
            Err(e) => {
                state.error = Some(e.to_string());
                None
            }
        };
        let Some(handle) = handle else {
            return state;
        };
        state.is_repository = true;

        match handle.status() {
            Ok(mut status) => {
                // The root repository must not appear to own content that lives in an
                // external repository (Git reports such a directory as one untracked
                // entry). Ownership comes from the configuration, not from Git.
                if repo.role == crate::model::RepositoryRole::Root {
                    let externals: Vec<String> = self
                        .project
                        .external_repositories()
                        .map(|r| r.relative_slash())
                        .collect();
                    if !externals.is_empty() {
                        status.entries.retain(|entry| {
                            let normalized = entry.path.trim_end_matches('/').replace('\\', "/");
                            !externals.iter().any(|ext| {
                                normalized == *ext || normalized.starts_with(&format!("{ext}/"))
                            })
                        });
                    }
                }
                state.status = Some(status);
            }
            Err(e) => state.error = Some(e.to_string()),
        }
        state.remotes = handle.remotes().unwrap_or_default();
        state.in_progress = handle.operation_in_progress().ok().flatten();
        state
    }

    /// Inspect every repository and aggregate the result.
    pub fn analyze(&self) -> ProjectStatus {
        let repositories: Vec<RepositoryState> = self
            .project
            .sorted_repositories()
            .into_iter()
            .map(|repo| self.inspect_repository(repo))
            .collect();

        let mut notices = Vec::new();
        for state in &repositories {
            if let Some(error) = &state.error {
                notices.push(format!("repository '{}': {error}", state.id));
            }
            if let Some(operation) = state.in_progress {
                notices.push(format!(
                    "repository '{}': {} - finish or abort it before running GitMesh operations",
                    state.id,
                    operation.label()
                ));
            }
        }
        notices.extend(self.ownership_warnings(&repositories));

        ProjectStatus {
            name: self.project.name.clone(),
            root: self.project.root.clone(),
            repositories,
            notices,
        }
    }

    /// Configurations where the root repository still tracks files that belong to an
    /// external repository. GitMesh reports this instead of silently "fixing" it,
    /// because removing files from Git is a destructive operation the user must
    /// choose.
    pub fn ownership_warnings(&self, states: &[RepositoryState]) -> Vec<String> {
        let mut warnings = Vec::new();
        let Some(root) = states
            .iter()
            .find(|s| s.role == crate::model::RepositoryRole::Root)
        else {
            return warnings;
        };
        if !root.is_usable() {
            return warnings;
        }
        let root_repo = self.runner.repo(&root.path);
        for state in states
            .iter()
            .filter(|s| s.role != crate::model::RepositoryRole::Root)
        {
            if !state.exists {
                continue;
            }
            let Ok(tracked) = root_repo.tracked_files_under(Path::new(&state.relative_path)) else {
                continue;
            };
            if tracked.is_empty() {
                continue;
            }
            let sample = tracked
                .iter()
                .take(3)
                .map(|p| to_slash(p))
                .collect::<Vec<_>>()
                .join(", ");
            warnings.push(format!(
                "repository '{}': {} file(s) tracked by the root repository are inside this \
                 repository's directory (for example: {}) - the root repository should stop \
                 tracking '{}' (GitMesh does not change history automatically)",
                state.id,
                tracked.len(),
                sample,
                state.relative_path
            ));
        }
        warnings
    }

    /// Every change in the project, with its owning repository.
    pub fn owned_changes(&self, status: &ProjectStatus) -> Vec<OwnedChange> {
        let mut owned = Vec::new();
        for state in &status.repositories {
            let Some(repo) = self.project.repository(&state.id) else {
                continue;
            };
            let Some(repo_status) = &state.status else {
                continue;
            };
            for entry in &repo_status.entries {
                if entry.ignored {
                    continue;
                }
                owned.push(OwnedChange {
                    repository_id: state.id.clone(),
                    role: state.role,
                    repository_path: repo.relative_slash(),
                    logical_path: logical_path_of(repo, state.role, &entry.path),
                    repo_relative: entry.path.clone(),
                    entry: entry.clone(),
                });
            }
        }
        owned
    }

    /// Resolve which repository owns an absolute or project-relative path, refusing
    /// ambiguous or out-of-project paths.
    pub fn owner_of_path(&self, path: &Path) -> Result<&PhysicalRepository> {
        let absolute = if path.is_absolute() {
            paths::lexical_normalize(path)
        } else {
            paths::lexical_normalize(&self.project.root.join(path))
        };
        if !paths::is_within(&self.project.root, &absolute) {
            return Err(Error::OutsideProject {
                path: absolute,
                root: self.project.root.clone(),
            });
        }
        let relative = paths::project_relative(&self.project.root, &absolute).ok_or_else(|| {
            Error::OutsideProject {
                path: absolute.clone(),
                root: self.project.root.clone(),
            }
        })?;
        self.project
            .repository_for_relative(&relative)
            .ok_or_else(|| {
                Error::Other(format!(
                    "no repository owns '{}' in project '{}'",
                    to_slash(&relative),
                    self.project.name
                ))
            })
    }

    /// Open a verified Git handle for a logical repository id.
    pub fn repo_handle(&self, id: &str) -> Result<GitRepo<'_>> {
        let repo = self
            .project
            .repository(id)
            .ok_or_else(|| Error::UnknownRepository(id.to_string()))?;
        verified_repo(self.runner, repo, &self.project.root)
    }

    /// Project-relative path of a change reported by one repository.
    pub fn logical_path_of_state(&self, state: &RepositoryState, repo_relative: &str) -> String {
        match self.project.repository(&state.id) {
            Some(repo) => logical_path_of(repo, state.role, repo_relative),
            None => repo_relative.replace('\\', "/"),
        }
    }

    /// Machine-readable representation of a project status.
    ///
    /// Every change carries both its repository-relative path (what Git reports) and
    /// its logical path (what the user sees), so consumers do not have to reimplement
    /// the ownership rules.
    pub fn status_json(&self, status: &ProjectStatus) -> Json {
        Json::object([
            ("project", Json::from(status.name.clone())),
            ("root", Json::from(to_slash(&status.root))),
            (
                "repositories",
                Json::array(
                    status
                        .repositories
                        .iter()
                        .map(|state| self.repository_json(state)),
                ),
            ),
            (
                "notices",
                Json::array(status.notices.iter().map(|n| Json::from(n.as_str()))),
            ),
        ])
    }

    /// Machine-readable representation of one repository state.
    pub fn repository_json(&self, state: &RepositoryState) -> Json {
        let status = state.status.as_ref();
        let counts = status.map(change_counts).unwrap_or_default();
        Json::object([
            ("id", Json::from(state.id.clone())),
            ("role", Json::from(state.role.label())),
            ("path", Json::from(state.relative_path.clone())),
            ("exists", Json::from(state.exists)),
            ("is_repository", Json::from(state.is_repository)),
            (
                "head",
                Json::from(
                    status
                        .map(|s| s.head.label())
                        .unwrap_or_else(|| "(unknown)".to_string()),
                ),
            ),
            ("branch", Json::opt(state.branch().map(Json::from))),
            (
                "detached",
                Json::from(status.is_some_and(|s| s.is_detached())),
            ),
            ("ahead", Json::opt(state.ahead())),
            ("behind", Json::opt(state.behind())),
            (
                "upstream",
                Json::opt(status.and_then(|s| s.upstream.clone()).map(Json::from)),
            ),
            (
                "remotes",
                Json::array(
                    state
                        .remotes
                        .iter()
                        .map(|r| {
                            Json::object([
                                ("name", Json::from(r.name.clone())),
                                ("url", Json::opt(r.fetch_url().map(Json::from))),
                                ("kind", Json::from(r.kind().label())),
                                (
                                    "provider",
                                    Json::opt(
                                        r.fetch_url()
                                            .and_then(crate::providers::parse_remote)
                                            .map(|remote| Json::from(remote.provider)),
                                    ),
                                ),
                            ])
                        })
                        .collect::<Vec<_>>(),
                ),
            ),
            ("staged", Json::from(counts.staged)),
            ("unstaged", Json::from(counts.unstaged)),
            ("untracked", Json::from(counts.untracked)),
            ("conflicts", Json::from(counts.conflict)),
            (
                "changes",
                Json::array(
                    status
                        .map(|s| {
                            s.entries
                                .iter()
                                .filter(|e| !e.ignored)
                                .map(|e| {
                                    Json::object([
                                        ("path", Json::from(e.path.clone())),
                                        (
                                            "logical_path",
                                            Json::from(self.logical_path_of_state(state, &e.path)),
                                        ),
                                        (
                                            "original_path",
                                            Json::opt(e.original_path.clone().map(Json::from)),
                                        ),
                                        ("kind", Json::from(e.kind().label())),
                                        ("staged", Json::from(e.staged)),
                                        ("unstaged", Json::from(e.unstaged)),
                                        ("untracked", Json::from(e.untracked)),
                                        ("conflict", Json::from(e.is_conflict())),
                                    ])
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                ),
            ),
            ("error", Json::opt(state.error.clone().map(Json::from))),
        ])
    }

    /// Human-readable summary of one repository, e.g. `root  main  ~2 staged`.
    pub fn describe(&self, state: &RepositoryState) -> String {
        format!("{} ({}) {}", state.id, state.relative_path, state.summary())
    }
}

fn logical_path_of(
    repo: &PhysicalRepository,
    role: crate::model::RepositoryRole,
    path: &str,
) -> String {
    let repo_relative = Path::new(path);
    if role == crate::model::RepositoryRole::Root {
        return path.replace('\\', "/");
    }
    let base = to_slash(&repo.relative_path);
    if base == "." || base.is_empty() {
        return repo_relative.to_string_lossy().replace('\\', "/");
    }
    format!(
        "{base}/{}",
        repo_relative.to_string_lossy().replace('\\', "/")
    )
}

pub fn short_head(state: &RepositoryState) -> String {
    match state.head() {
        crate::git::Head::Detached { oid } => short_oid(&oid),
        _ => state.branch().unwrap_or_else(|| "-".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;
    use crate::model::RepositoryRole;
    use crate::testkit::RepoFixture;

    #[test]
    fn analyzes_a_clean_project() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let analyzer = Analyzer::new(&project, fixture.runner());
        let status = analyzer.analyze();
        assert_eq!(status.repositories.len(), 2);
        assert!(!status.has_changes());
        assert!(!status.has_conflicts());
        assert!(status.notices.is_empty(), "{:?}", status.notices);
    }

    #[test]
    fn attributes_changes_to_the_right_repository() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        let analyzer = Analyzer::new(&project, fixture.runner());

        fixture.write("src/main.rs", "root change");
        fixture.write("engine/src/lib.rs", "engine change");
        fixture.write("renderer/index.js", "renderer change");

        let status = analyzer.analyze();
        let owned = analyzer.owned_changes(&status);
        let by_repo = |id: &str| {
            let mut paths: Vec<String> = owned
                .iter()
                .filter(|c| c.repository_id == id)
                .map(|c| c.logical_path.clone())
                .collect();
            paths.sort();
            paths
        };
        assert_eq!(by_repo("root"), vec!["src/main.rs"]);
        assert_eq!(by_repo("engine"), vec!["engine/src/lib.rs"]);
        assert_eq!(by_repo("renderer"), vec!["renderer/index.js"]);
    }

    #[test]
    fn engine_file_is_never_attributed_to_the_root() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let analyzer = Analyzer::new(&project, fixture.runner());
        let owner = analyzer
            .owner_of_path(Path::new("engine/src/lib.rs"))
            .unwrap();
        assert_eq!(owner.id, "engine");
        assert_eq!(owner.role, RepositoryRole::External);
        let owner = analyzer.owner_of_path(Path::new("src/main.rs")).unwrap();
        assert_eq!(owner.id, "root");
    }

    #[test]
    fn reports_missing_repositories_and_keeps_going() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let mut project = project;
        project.repositories.push(crate::model::PhysicalRepository {
            id: "ghost".into(),
            role: RepositoryRole::External,
            relative_path: PathBuf::from("ghost"),
            remote_url: None,
            branch: None,
            absolute_path: project_root_of(&project).join("ghost"),
        });
        let analyzer = Analyzer::new(&project, fixture.runner());
        let status = analyzer.analyze();
        let ghost = status
            .repositories
            .iter()
            .find(|r| r.id == "ghost")
            .unwrap();
        assert!(!ghost.is_usable());
        assert!(ghost.error.as_deref().unwrap().contains("does not exist"));
        assert!(status.notices.iter().any(|n| n.contains("ghost")));
        // The healthy repository is still reported.
        assert!(status
            .repositories
            .iter()
            .any(|r| r.id == "root" && r.is_usable()));
    }

    #[test]
    fn detects_repository_that_is_not_its_own_root() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        std::fs::create_dir_all(fixture.path().join("sub")).unwrap();
        let mut project = project;
        project.repositories.push(crate::model::PhysicalRepository {
            id: "sub".into(),
            role: RepositoryRole::External,
            relative_path: PathBuf::from("sub"),
            remote_url: None,
            branch: None,
            absolute_path: fixture.path().join("sub"),
        });
        let analyzer = Analyzer::new(&project, fixture.runner());
        let status = analyzer.analyze();
        let sub = status.repositories.iter().find(|r| r.id == "sub").unwrap();
        assert!(!sub.is_usable());
        assert!(sub
            .error
            .as_deref()
            .unwrap()
            .contains("not a repository root"));
    }

    #[test]
    fn warns_when_the_root_still_tracks_files_of_an_external_repository() {
        let fixture = RepoFixture::new();
        // root tracks files inside engine/
        fixture.write_and_commit("engine/legacy.txt", "old");
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let analyzer = Analyzer::new(&project, fixture.runner());
        let status = analyzer.analyze();
        assert!(
            status
                .notices
                .iter()
                .any(|n| n.contains("tracked by the root repository")),
            "{:?}",
            status.notices
        );
    }

    #[test]
    fn status_json_is_valid_and_complete() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/new.txt", "x");
        let analyzer = Analyzer::new(&project, fixture.runner());
        let status = analyzer.analyze();
        let json = analyzer.status_json(&status).to_string();
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"repositories\""));
        assert!(json.contains("\"untracked\":1"));
        // Logical paths are resolved against the project root.
        assert!(
            json.contains("\"logical_path\":\"engine/new.txt\""),
            "{json}"
        );
        assert!(json.contains("\"path\":\"new.txt\""));
    }

    #[test]
    fn ownership_helper_maps_repo_relative_to_logical_paths() {
        let project = GitMeshProject {
            name: "p".into(),
            root: PathBuf::from("/p"),
            repositories: vec![PhysicalRepository {
                id: "engine".into(),
                role: RepositoryRole::External,
                relative_path: PathBuf::from("libs/engine"),
                remote_url: None,
                branch: None,
                absolute_path: PathBuf::from("/p/libs/engine"),
            }],
        };
        let root = PhysicalRepository {
            id: "root".into(),
            role: RepositoryRole::Root,
            relative_path: PathBuf::from("."),
            remote_url: None,
            branch: None,
            absolute_path: PathBuf::from("/p"),
        };
        assert_eq!(
            logical_path_of(&root, RepositoryRole::Root, "src/a.rs"),
            "src/a.rs"
        );
        assert_eq!(
            logical_path_of(
                &project.repositories[0],
                RepositoryRole::External,
                "src/a.rs"
            ),
            "libs/engine/src/a.rs"
        );
    }

    #[test]
    fn manifest_round_trip_is_used_by_the_fixture() {
        // Guards against the fixture drifting away from the real manifest code path.
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let text = manifest::render_manifest(&project).unwrap();
        assert!(text.contains("id = \"engine\""));
        let reparsed = manifest::parse_manifest(&text, &project.root, Path::new("test")).unwrap();
        assert_eq!(reparsed, project);
    }

    fn project_root_of(project: &GitMeshProject) -> PathBuf {
        project.root.clone()
    }
}
