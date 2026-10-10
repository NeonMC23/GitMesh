//! Shared orchestration helpers.
//!
//! Every project-wide operation needs the same three things: decide which
//! repositories take part, inspect each of them without letting one failure stop the
//! others, and never run a Git command against a repository whose identity has not
//! been verified. This module provides exactly that.

use std::path::{Path, PathBuf};

use crate::analyzer::Analyzer;
use crate::error::{Error, Result};
use crate::git::GitRepo;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryRole, RepositoryState};
use crate::paths::to_slash;

use super::{OutcomeKind, RepoOutcome};

/// Which repositories a logical operation should touch.
#[derive(Debug, Clone, Default)]
pub enum RepositorySelection {
    /// Every configured repository (the default).
    #[default]
    All,
    /// Only repositories inside this directory subtree (a project-relative path).
    ///
    /// External repositories always act on their own root; the path only limits which
    /// repositories are considered. Passing a path inside an external repository
    /// selects that repository.
    Subtree(PathBuf),
    /// Only these logical ids.
    Ids(Vec<String>),
    /// Only repositories that have changes.
    Changed,
}

impl RepositorySelection {
    /// Selection from a CLI `--repo` argument list.
    pub fn from_ids(ids: Vec<String>) -> Self {
        if ids.is_empty() {
            RepositorySelection::All
        } else {
            RepositorySelection::Ids(ids)
        }
    }

    /// True when this selection includes the given repository.
    pub fn includes(
        &self,
        project: &GitMeshProject,
        repo: &PhysicalRepository,
        state: &RepositoryState,
    ) -> bool {
        match self {
            RepositorySelection::All => true,
            RepositorySelection::Subtree(path) => {
                // A subtree selects exactly the repository that owns that path. A path
                // inside an external repository therefore selects only that external
                // repository, even though the root repository is conceptually above it.
                let absolute = if path.is_absolute() {
                    path.clone()
                } else {
                    project.root.join(path)
                };
                match project.repository_for_path(&absolute) {
                    Some(owner) => owner.id == repo.id,
                    None => false,
                }
            }
            RepositorySelection::Ids(ids) => ids.iter().any(|id| id == &repo.id),
            RepositorySelection::Changed => state.has_changes(),
        }
    }

    /// Validate the selection against the project so users get a clear error for a
    /// typo instead of a silently empty operation.
    pub fn validate(&self, project: &GitMeshProject) -> Result<()> {
        match self {
            RepositorySelection::Ids(ids) => {
                let unknown: Vec<String> = ids
                    .iter()
                    .filter(|id| project.repository(id).is_none())
                    .cloned()
                    .collect();
                if unknown.is_empty() {
                    Ok(())
                } else {
                    Err(Error::Other(format!(
                        "unknown repository id(s): {}. Known repositories: {}",
                        unknown.join(", "),
                        project
                            .sorted_repositories()
                            .iter()
                            .map(|r| r.id.clone())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )))
                }
            }
            _ => Ok(()),
        }
    }
}

/// Run a project-wide operation, one repository at a time.
///
/// `step` receives the verified Git handle and the inspected state of one repository
/// and returns its outcome. The loop:
///
/// * inspects the repository first ([`Analyzer::inspect_repository`]);
/// * reports repositories that are missing, are not Git repositories, or cannot be
///   inspected as `Failed` (never as success);
/// * verifies that the directory really is the configured repository root before
///   handing out a usable handle;
/// * always continues with the next repository.
pub fn each_repository<F>(
    project: &GitMeshProject,
    analyzer: &Analyzer<'_>,
    runner: &crate::git::GitRunner,
    selection: &RepositorySelection,
    step: F,
) -> Vec<RepoOutcome>
where
    F: FnMut(&PhysicalRepository, &RepositoryState, &GitRepo<'_>) -> RepoOutcome,
{
    each_repository_observed(
        project,
        analyzer,
        runner,
        selection,
        &mut crate::ops::OperationObserver::silent(),
        step,
    )
}

/// Same as [`each_repository`], reporting each repository to an observer as the loop
/// progresses. This is the only place where per-repository progress is produced, so
/// every front end sees the same sequence.
pub fn each_repository_observed<F>(
    project: &GitMeshProject,
    analyzer: &Analyzer<'_>,
    runner: &crate::git::GitRunner,
    selection: &RepositorySelection,
    observer: &mut crate::ops::OperationObserver<'_>,
    mut step: F,
) -> Vec<RepoOutcome>
where
    F: FnMut(&PhysicalRepository, &RepositoryState, &GitRepo<'_>) -> RepoOutcome,
{
    let mut outcomes = Vec::new();

    for repo in project.sorted_repositories() {
        let state = analyzer.inspect_repository(repo);
        let path = repo.relative_slash();

        if matches!(selection, RepositorySelection::Ids(_))
            && !selection.includes(project, repo, &state)
        {
            continue; // explicitly deselected repositories are not reported at all
        }

        if let Some(error) = &state.error {
            observer.repository_started(repo);
            // A repository that cannot be inspected is a failure, never a success —
            // the reason is carried in the summary and the details.
            outcomes.push(
                RepoOutcome::new(
                    &repo.id,
                    repo.role,
                    path,
                    OutcomeKind::Failed,
                    unavailable_summary(&state),
                )
                .with_detail(error.clone()),
            );
            let outcome = outcomes.last().expect("just pushed");
            observer.repository_finished(outcome);
            continue;
        }

        if !selection.includes(project, repo, &state) {
            continue;
        }

        observer.repository_started(repo);
        match crate::discovery::verified_repo(runner, repo, &project.root) {
            Ok(handle) => outcomes.push(step(repo, &state, &handle)),
            Err(err) => outcomes.push(
                RepoOutcome::new(
                    &repo.id,
                    repo.role,
                    path,
                    OutcomeKind::Failed,
                    "cannot be used",
                )
                .with_detail(err.to_string()),
            ),
        }
        let outcome = outcomes.last().expect("just pushed");
        observer.repository_finished(outcome);
    }

    outcomes
}

/// Build a short failure summary for a repository that could not be inspected.
fn unavailable_summary(state: &RepositoryState) -> String {
    if !state.exists {
        "missing (directory does not exist)".to_string()
    } else if !state.is_repository {
        "not a Git repository".to_string()
    } else {
        "unavailable".to_string()
    }
}

/// Describe the current branch of a repository for reporting purposes.
pub fn branch_label(state: &RepositoryState) -> String {
    state.branch().unwrap_or_else(|| state.head().label())
}

/// True when a repository is in the middle of a merge/rebase/cherry-pick.
pub fn has_operation_in_progress(state: &RepositoryState) -> Option<&'static str> {
    state.in_progress.map(|op| op.label())
}

/// Pathspec exclusions for the root repository.
///
/// The root repository must never stage or commit files that belong to an external
/// repository. Because Git sees a nested repository as a single directory, `git add`
/// would otherwise record it as a gitlink. GitMesh therefore excludes those
/// directories explicitly.
pub fn exclusion_pathspecs(project: &GitMeshProject) -> Vec<String> {
    project
        .external_repositories()
        .map(|repo| format!(":(exclude){}", to_slash(&repo.relative_path)))
        .collect()
}

/// True when this repository is the root repository and the project has externals.
pub fn needs_exclusions(project: &GitMeshProject, repo: &PhysicalRepository) -> bool {
    repo.role == RepositoryRole::Root && project.external_repositories().next().is_some()
}

/// Does `path` (repository-relative) live inside an external repository of this
/// project? Used to filter root status entries that actually belong elsewhere.
pub fn relative_owned_by_external<'a>(
    project: &'a GitMeshProject,
    path: &str,
) -> Option<&'a PhysicalRepository> {
    let normalized = path.trim_end_matches('/');
    let candidate = Path::new(normalized);
    project.external_repositories().find(|repo| {
        repo.relative_path == candidate || crate::paths::is_within(&repo.relative_path, candidate)
    })
}

/// Repositories nested in a repository's working tree that GitMesh does not manage.
///
/// `entries` are repository-relative paths from `git status` (untracked directories and
/// tracked gitlinks come out as one entry each). An entry is nested when the directory holds a
/// `.git` of its own. Ignored paths are never passed in, so no ignore rule is added or changed,
/// and a nested repository is never staged into the repository that contains it.
pub fn nested_repositories(repo_path: &Path, entries: &[&str]) -> Vec<String> {
    entries
        .iter()
        .map(|entry| entry.trim_end_matches('/'))
        .filter(|entry| !entry.is_empty() && *entry != ".")
        .filter(|entry| {
            let dir = repo_path.join(entry);
            dir.is_dir() && dir.join(".git").exists()
        })
        .map(str::to_string)
        .collect()
}

/// Is `path` (repository-relative) the nested repository `nested`, or inside one?
pub fn is_within_nested(nested: &[String], path: &str) -> bool {
    let path = path.trim_end_matches('/');
    nested
        .iter()
        .any(|n| path == n || path.starts_with(&format!("{n}/")))
}

/// `git add` pathspec exclusions for one repository: its nested repositories, and in the root
/// repository also the external repositories of the project.
pub fn staging_exclusions(
    project: &GitMeshProject,
    repo: &PhysicalRepository,
    nested: &[String],
) -> Vec<String> {
    let mut specs: Vec<String> = nested.iter().map(|n| format!(":(exclude){n}")).collect();
    if needs_exclusions(project, repo) {
        specs.extend(exclusion_pathspecs(project));
    }
    specs
}

/// Like [`concise_git_error`], but `None` when Git printed nothing useful.
pub fn concise_git_error_opt(stderr: &str) -> Option<String> {
    let cleaned: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.join("; "))
    }
}

/// Truncate a Git error message for display, keeping the informative tail.
pub fn concise_git_error(stderr: &str) -> String {
    let cleaned: Vec<String> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    if cleaned.is_empty() {
        return "git reported no error message".to_string();
    }
    cleaned.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    #[test]
    fn selection_all_includes_every_repository() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let analyzer = Analyzer::new(&project, fixture.runner());
        let selection = RepositorySelection::All;
        for repo in &project.repositories {
            let state = analyzer.inspect_repository(repo);
            assert!(selection.includes(&project, repo, &state));
        }
    }

    #[test]
    fn selection_by_id_is_validated() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        assert!(RepositorySelection::from_ids(vec!["engine".into()])
            .validate(&project)
            .is_ok());
        let err = RepositorySelection::from_ids(vec!["nope".into()])
            .validate(&project)
            .unwrap_err();
        assert!(err.to_string().contains("unknown repository id"));
        assert!(err.to_string().contains("engine"));
    }

    #[test]
    fn selection_by_subtree_resolves_external_repositories() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        let analyzer = Analyzer::new(&project, fixture.runner());
        let selection = RepositorySelection::Subtree(PathBuf::from("engine/src"));
        let engine = project.repository("engine").unwrap();
        let engine_state = analyzer.inspect_repository(engine);
        assert!(selection.includes(&project, engine, &engine_state));
        let renderer = project.repository("renderer").unwrap();
        let renderer_state = analyzer.inspect_repository(renderer);
        assert!(!selection.includes(&project, renderer, &renderer_state));
        let root = project.root_repository();
        let root_state = analyzer.inspect_repository(root);
        assert!(!selection.includes(&project, root, &root_state));
    }

    #[test]
    fn each_repository_continues_after_a_missing_repository() {
        let fixture = RepoFixture::new();
        let mut project = fixture.project_with(&[("root", ".")]);
        project.repositories.push(crate::model::PhysicalRepository {
            id: "ghost".into(),
            role: RepositoryRole::External,
            relative_path: PathBuf::from("ghost"),
            remote_url: None,
            branch: None,
            absolute_path: fixture.path().join("ghost"),
        });
        let analyzer = Analyzer::new(&project, fixture.runner());
        let mut visited = Vec::new();
        let outcomes = each_repository(
            &project,
            &analyzer,
            fixture.runner(),
            &RepositorySelection::All,
            |repo, _state, _git| {
                visited.push(repo.id.clone());
                RepoOutcome::new(
                    &repo.id,
                    repo.role,
                    repo.relative_slash(),
                    OutcomeKind::Success,
                    "ok",
                )
            },
        );
        assert_eq!(visited, vec!["root"]);
        assert_eq!(outcomes.len(), 2);
        let ghost = outcomes.iter().find(|o| o.id == "ghost").unwrap();
        assert_eq!(ghost.kind, OutcomeKind::Failed);
        assert!(ghost.summary.contains("missing"));
    }

    #[test]
    fn exclusion_pathspecs_cover_external_repositories() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("vendor-lib", "vendor/lib"),
        ]);
        let specs = exclusion_pathspecs(&project);
        assert!(specs.contains(&":(exclude)engine".to_string()));
        assert!(specs.contains(&":(exclude)vendor/lib".to_string()));
        assert!(needs_exclusions(&project, project.root_repository()));
        assert!(!needs_exclusions(
            &project,
            project.repository("engine").unwrap()
        ));
    }

    #[test]
    fn ownership_filter_identifies_paths_owned_elsewhere() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        assert!(relative_owned_by_external(&project, "engine/").is_some());
        assert!(relative_owned_by_external(&project, "engine/src/lib.rs").is_some());
        assert!(relative_owned_by_external(&project, "src/lib.rs").is_none());
    }
}

#[cfg(test)]
mod nested_repository_tests {
    use super::*;
    use crate::testkit::TempDir;

    #[test]
    fn a_directory_with_its_own_git_is_nested_and_others_are_not() {
        let tmp = TempDir::new("util-nested").unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("scratch/.git")).unwrap();
        std::fs::create_dir_all(root.join("plain/src")).unwrap();
        std::fs::write(root.join("notes.md"), "x").unwrap();

        let entries = ["scratch/", "plain/", "notes.md", "."];
        assert_eq!(
            nested_repositories(root, &entries),
            vec!["scratch".to_string()]
        );
    }

    #[test]
    fn a_git_file_marks_a_nested_repository_too() {
        // Worktrees and submodule checkouts use a `.git` file rather than a directory.
        let tmp = TempDir::new("util-nested-file").unwrap();
        std::fs::create_dir_all(tmp.path().join("linked")).unwrap();
        std::fs::write(tmp.path().join("linked/.git"), "gitdir: elsewhere\n").unwrap();
        assert_eq!(
            nested_repositories(tmp.path(), &["linked"]),
            vec!["linked".to_string()]
        );
    }

    #[test]
    fn paths_inside_a_nested_repository_are_recognised() {
        let nested = vec!["scratch".to_string()];
        assert!(is_within_nested(&nested, "scratch"));
        assert!(is_within_nested(&nested, "scratch/"));
        assert!(is_within_nested(&nested, "scratch/src/a.rs"));
        assert!(!is_within_nested(&nested, "scratchpad/a.rs"));
        assert!(!is_within_nested(&nested, "notes.md"));
    }

    #[test]
    fn staging_excludes_nested_repositories_in_every_repository() {
        let project = crate::model::GitMeshProject {
            name: "p".into(),
            root: PathBuf::from("/p"),
            repositories: vec![crate::model::PhysicalRepository {
                id: "engine".into(),
                role: RepositoryRole::External,
                relative_path: PathBuf::from("engine"),
                remote_url: None,
                branch: None,
                absolute_path: PathBuf::from("/p/engine"),
            }],
        };
        let nested = vec!["vendor/lib".to_string()];
        let engine = project.repository("engine").unwrap();
        assert_eq!(
            staging_exclusions(&project, engine, &nested),
            vec![":(exclude)vendor/lib".to_string()]
        );
        assert!(staging_exclusions(&project, engine, &[]).is_empty());
    }
}
