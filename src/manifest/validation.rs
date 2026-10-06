//! Structural validation of a logical project.
//!
//! Validation is intentionally separate from parsing: the same checks run for
//! manifests loaded from disk, for projects built interactively by the UI, and for
//! projects assembled by tests. A project that passes validation can safely be used
//! for orchestration, because it guarantees that every logical path has exactly one
//! owning repository.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::model::{GitMeshProject, RepositoryRole};
use crate::paths::{self, to_slash};

/// A single validation problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    pub message: String,
}

impl ValidationIssue {
    pub fn new(message: impl Into<String>) -> Self {
        ValidationIssue {
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Validate the structure of a project.
///
/// Returns every problem found (not just the first) so that the user can fix the
/// configuration in one go.
pub fn validate_project(project: &GitMeshProject) -> Result<(), Vec<String>> {
    let mut issues: Vec<String> = Vec::new();

    if project.name.trim().is_empty() {
        issues.push("project name must not be empty".into());
    }
    if project.root.as_os_str().is_empty() {
        issues.push("project root must not be empty".into());
    }

    let roots: Vec<_> = project
        .repositories
        .iter()
        .filter(|r| r.role == RepositoryRole::Root)
        .collect();
    match roots.len() {
        0 => issues.push("the project must define exactly one root repository".into()),
        1 => {
            let root = roots[0];
            if !paths::is_root_relative(&root.relative_path) {
                issues.push(format!(
                    "the root repository must live at the project root, not at '{}'",
                    to_slash(&root.relative_path)
                ));
            }
            if root.absolute_path != project.root {
                issues.push(format!(
                    "the root repository path '{}' does not match the project root '{}'",
                    root.absolute_path.display(),
                    project.root.display()
                ));
            }
        }
        n => issues.push(format!(
            "the project defines {n} root repositories but must define exactly one"
        )),
    }

    // ---- ids: unique, non-empty, safe for display and use as a manifest key.
    let mut ids: HashMap<&str, usize> = HashMap::new();
    for repo in &project.repositories {
        let id = repo.id.trim();
        if id.is_empty() {
            issues.push(format!(
                "repository at '{}' has an empty id",
                to_slash(&repo.relative_path)
            ));
            continue;
        }
        if id != repo.id {
            issues.push(format!(
                "repository id '{id}' must not have surrounding whitespace"
            ));
        }
        if !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            issues.push(format!(
                "repository id '{id}' contains unsupported characters (use letters, digits, '-', '_' or '.')"
            ));
        }
        if let Some(_first) = ids.insert(repo.id.as_str(), 0) {
            issues.push(format!("duplicate repository id '{id}'"));
        }
    }

    // ---- paths: unique and non-overlapping.
    for (index, repo) in project.repositories.iter().enumerate() {
        let path = paths::lexical_normalize(&repo.relative_path);
        if repo.role == RepositoryRole::External && paths::is_root_relative(&path) {
            issues.push(format!(
                "external repository '{}' must not use the project root as its path",
                repo.id
            ));
        }
        if !repo.absolute_path.starts_with(&project.root) {
            issues.push(format!(
                "repository '{}' resolves outside the project root ({})",
                repo.id,
                repo.absolute_path.display()
            ));
        }
        for other in project.repositories.iter().skip(index + 1) {
            let other_path = paths::lexical_normalize(&other.relative_path);
            if path == other_path {
                issues.push(format!(
                    "repositories '{}' and '{}' are assigned the same path '{}'",
                    repo.id,
                    other.id,
                    slash_or_root(&path)
                ));
                continue;
            }
            if paths::is_strict_ancestor(&path, &other_path)
                || paths::is_strict_ancestor(&other_path, &path)
            {
                // The root repository is conceptually the ancestor of everything, and
                // that is exactly the point of GitMesh: it stays allowed. Any other
                // nesting would make ownership ambiguous.
                let involves_root = repo.is_root() || other.is_root();
                if !involves_root {
                    issues.push(format!(
                        "repository paths overlap: '{}' ('{}') and '{}' ('{}') - a directory \
                         cannot belong to two physical repositories",
                        repo.id,
                        slash_or_root(&path),
                        other.id,
                        slash_or_root(&other_path)
                    ));
                }
            }
        }
    }

    // ---- remotes: two repositories pushing to the same URL is almost always a
    // configuration mistake and would make pushes fight each other.
    let mut remotes: HashMap<&str, &str> = HashMap::new();
    for repo in &project.repositories {
        if let Some(url) = repo.remote_url.as_deref() {
            if let Some(previous) = remotes.insert(url, &repo.id) {
                issues.push(format!(
                    "repositories '{previous}' and '{}' use the same remote URL '{url}'",
                    repo.id
                ));
            }
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

fn slash_or_root(path: &Path) -> String {
    if paths::is_root_relative(path) {
        ".".to_string()
    } else {
        to_slash(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PhysicalRepository;
    use std::path::PathBuf;

    fn repo(id: &str, role: RepositoryRole, path: &str) -> PhysicalRepository {
        PhysicalRepository {
            id: id.into(),
            role,
            relative_path: PathBuf::from(path),
            remote_url: None,
            branch: None,
            absolute_path: Path::new("/p").join(path),
        }
    }

    fn project(repos: Vec<PhysicalRepository>) -> GitMeshProject {
        GitMeshProject {
            name: "p".into(),
            root: PathBuf::from("/p"),
            repositories: repos,
        }
    }

    #[test]
    fn accepts_a_well_formed_project() {
        let p = project(vec![
            repo("root", RepositoryRole::Root, "."),
            repo("engine", RepositoryRole::External, "engine"),
        ]);
        assert!(validate_project(&p).is_ok());
    }

    #[test]
    fn requires_exactly_one_root() {
        let p = project(vec![
            repo("a", RepositoryRole::Root, "."),
            repo("b", RepositoryRole::Root, "."),
        ]);
        let issues = validate_project(&p).unwrap_err();
        assert!(
            issues.iter().any(|i| i.contains("2 root repositories")),
            "{issues:?}"
        );

        let none = project(vec![repo("engine", RepositoryRole::External, "engine")]);
        let issues = validate_project(&none).unwrap_err();
        assert!(
            issues.iter().any(|i| i.contains("exactly one root")),
            "{issues:?}"
        );
    }

    #[test]
    fn rejects_overlapping_external_paths() {
        let p = project(vec![
            repo("root", RepositoryRole::Root, "."),
            repo("engine", RepositoryRole::External, "engine"),
            repo("deep", RepositoryRole::External, "engine/deep"),
        ]);
        let issues = validate_project(&p).unwrap_err();
        assert!(issues.iter().any(|i| i.contains("overlap")), "{issues:?}");
    }

    #[test]
    fn allows_the_root_to_be_the_parent_of_external_repositories() {
        let p = project(vec![
            repo("root", RepositoryRole::Root, "."),
            repo("engine", RepositoryRole::External, "engine"),
            repo("renderer", RepositoryRole::External, "renderer"),
        ]);
        assert!(validate_project(&p).is_ok(), "{:?}", validate_project(&p));
    }

    #[test]
    fn rejects_duplicate_ids_and_remotes_and_paths() {
        let mut a = repo("engine", RepositoryRole::External, "engine");
        a.remote_url = Some("git@github.com:acme/x.git".into());
        let mut b = repo("engine", RepositoryRole::External, "engine");
        b.remote_url = Some("git@github.com:acme/x.git".into());
        let issues = validate_project(&project(vec![
            repo("root", RepositoryRole::Root, "."),
            a,
            b,
        ]))
        .unwrap_err();
        assert!(
            issues.iter().any(|i| i.contains("duplicate repository id")),
            "{issues:?}"
        );
        assert!(issues.iter().any(|i| i.contains("same path")), "{issues:?}");
        assert!(
            issues.iter().any(|i| i.contains("same remote URL")),
            "{issues:?}"
        );
    }

    #[test]
    fn rejects_ids_with_unsupported_characters() {
        let p = project(vec![
            repo("root", RepositoryRole::Root, "."),
            repo("my repo", RepositoryRole::External, "x"),
        ]);
        let issues = validate_project(&p).unwrap_err();
        assert!(
            issues.iter().any(|i| i.contains("unsupported characters")),
            "{issues:?}"
        );
    }
}
