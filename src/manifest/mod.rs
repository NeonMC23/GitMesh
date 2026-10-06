//! The GitMesh project manifest: the persistent description of a logical project.
//!
//! # Format
//!
//! ```toml
//! version = 1
//! name = "my-project"
//!
//! [root]
//! remote = "git@github.com:acme/my-project.git"
//!
//! [[repositories]]
//! id = "engine"
//! path = "engine"
//! remote = "git@github.com:acme/engine.git"
//! branch = "main"
//! ```
//!
//! It is stored at `<project root>/.gitmesh/project.toml`. See
//! `docs/MANIFEST.md` for the full specification.
//!
//! The manifest is an **internal persistent representation**: the CLI (and later the
//! GUI/TUI) creates and updates it. Users are not expected to hand-edit it, but it is
//! plain readable TOML so that it can be reviewed, diffed and version-controlled.
//!
//! After configuration, the manifest is the source of truth: repository ownership is
//! never re-guessed from the filesystem.

pub mod schema;
pub mod validation;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryRole};
use crate::paths;

pub use schema::{ManifestFile, ManifestRepository, ManifestRoot};
pub use validation::{validate_project, ValidationIssue};

/// Directory holding GitMesh metadata, relative to the project root.
pub const METADATA_DIR: &str = ".gitmesh";

/// File name of the manifest inside [`METADATA_DIR`].
pub const MANIFEST_FILE: &str = "project.toml";

/// Current manifest format version.
pub const MANIFEST_VERSION: u32 = 1;

/// Absolute path of the manifest for a project root.
pub fn manifest_path(project_root: &Path) -> PathBuf {
    project_root.join(METADATA_DIR).join(MANIFEST_FILE)
}

/// Absolute path of the metadata directory for a project root.
pub fn metadata_dir(project_root: &Path) -> PathBuf {
    project_root.join(METADATA_DIR)
}

/// Walk up from `start` looking for a GitMesh project root.
///
/// `start` may be a file (its parent is used) or a directory.
pub fn find_project_root(start: &Path) -> Option<PathBuf> {
    let mut current: PathBuf = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    if !current.is_absolute() {
        current = std::env::current_dir().ok()?.join(current);
    }
    loop {
        if manifest_path(&current).is_file() {
            return Some(paths::lexical_normalize(&current));
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Load a project from an explicit project root directory.
pub fn load_from_root(project_root: &Path) -> Result<GitMeshProject> {
    let project_root = paths::lexical_normalize(project_root);
    let path = manifest_path(&project_root);
    if !path.is_file() {
        return Err(Error::ProjectNotFound { root: project_root });
    }
    let text = std::fs::read_to_string(&path).map_err(|source| Error::io(path.clone(), source))?;
    parse_manifest(&text, &project_root, &path)
}

/// Load the project that contains `start` (a path inside the project).
pub fn load_nearest(start: &Path) -> Result<GitMeshProject> {
    let root = find_project_root(start).ok_or_else(|| Error::ProjectNotFound {
        root: start.to_path_buf(),
    })?;
    load_from_root(&root)
}

/// Parse manifest text into a validated project.
pub fn parse_manifest(
    text: &str,
    project_root: &Path,
    source_path: &Path,
) -> Result<GitMeshProject> {
    let file: ManifestFile = toml::from_str(text)
        .map_err(|e| Error::Manifest(format!("{}: {e}", source_path.display())))?;
    file.into_project(project_root)
        .map_err(Error::InvalidConfiguration)
}

/// Serialise a project to manifest text.
pub fn render_manifest(project: &GitMeshProject) -> Result<String> {
    let file = ManifestFile::from_project(project)?;
    toml::to_string_pretty(&file)
        .map_err(|e| Error::Manifest(format!("could not serialise manifest: {e}")))
}

/// Write a project manifest, creating the metadata directory if needed.
///
/// The write is atomic (temporary file + rename) so an interrupted write can never
/// leave a truncated manifest behind.
pub fn save_project(project: &GitMeshProject) -> Result<PathBuf> {
    validate_project(&project.to_owned()).map_err(Error::InvalidConfiguration)?;
    let text = render_manifest(project)?;
    let dir = metadata_dir(&project.root);
    std::fs::create_dir_all(&dir).map_err(|source| Error::io(dir.clone(), source))?;

    // Keep the work tree honest: the metadata directory should not be a source of
    // noise in the root repository's status. GitMesh never commits for the user here,
    // it only suggests ignoring the local metadata (the manifest itself is usually
    // meant to be committed).
    let path = manifest_path(&project.root);
    let tmp = dir.join(format!("{MANIFEST_FILE}.tmp"));
    std::fs::write(&tmp, text.as_bytes()).map_err(|source| Error::io(tmp.clone(), source))?;
    std::fs::rename(&tmp, &path).map_err(|source| Error::io(path.clone(), source))?;
    Ok(path)
}

impl ManifestFile {
    /// Validate and convert into the internal model.
    pub fn into_project(
        self,
        project_root: &Path,
    ) -> std::result::Result<GitMeshProject, Vec<String>> {
        let project_root = paths::lexical_normalize(project_root);
        let mut issues: Vec<String> = Vec::new();

        if self.version != MANIFEST_VERSION {
            issues.push(format!(
                "manifest version {} is not supported by this build (expected {MANIFEST_VERSION})",
                self.version
            ));
        }

        let name = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                project_root
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
            })
            .unwrap_or_else(|| "project".to_string());

        if let Some(declared) = self.name.as_deref() {
            if declared.trim() != declared || declared.contains('\n') {
                issues.push(
                    "project name must not contain leading/trailing whitespace or newlines".into(),
                );
            }
        }

        // ---- root repository: always present, defaults to the project root itself.
        let root_entry = self.root.unwrap_or_default();
        let root_id = root_entry
            .id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("root")
            .to_string();
        let root_path = match root_entry.path.as_deref() {
            None => PathBuf::from("."),
            Some(raw) => match paths::normalize_relative(raw) {
                Ok(p) if paths::is_root_relative(&p) => PathBuf::from("."),
                Ok(p) => {
                    issues.push(format!(
                        "the root repository path must be the project root (got '{}')",
                        paths::to_slash(&p)
                    ));
                    PathBuf::from(".")
                }
                Err(e) => {
                    issues.push(e.to_string());
                    PathBuf::from(".")
                }
            },
        };

        let mut repositories = vec![PhysicalRepository {
            id: root_id,
            role: RepositoryRole::Root,
            relative_path: root_path,
            remote_url: normalize_remote(
                root_entry.remote.as_deref(),
                "root repository",
                &mut issues,
            ),
            branch: normalize_branch(root_entry.branch.as_deref(), "root repository", &mut issues),
            absolute_path: project_root.clone(),
        }];

        for (index, entry) in self.repositories.into_iter().enumerate() {
            let label = entry
                .id
                .clone()
                .unwrap_or_else(|| format!("repository #{index}"));
            let id = entry
                .id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let Some(id) = id else {
                issues.push(format!("repository #{index} is missing a logical id"));
                continue;
            };
            let raw_path = match entry.path.as_deref() {
                Some(p) => p,
                None => {
                    issues.push(format!("repository '{id}' is missing a path"));
                    continue;
                }
            };
            let relative_path = match paths::normalize_relative(raw_path) {
                Ok(p) => {
                    if paths::is_root_relative(&p) {
                        issues.push(format!(
                            "external repository '{id}' must not use the project root as its path"
                        ));
                        continue;
                    }
                    p
                }
                Err(e) => {
                    issues.push(format!("repository '{id}': {e}"));
                    continue;
                }
            };
            repositories.push(PhysicalRepository {
                id: id.clone(),
                role: RepositoryRole::External,
                absolute_path: project_root.join(&relative_path),
                relative_path,
                remote_url: normalize_remote(
                    entry.remote.as_deref(),
                    &format!("repository '{id}'"),
                    &mut issues,
                ),
                branch: normalize_branch(
                    entry.branch.as_deref(),
                    &format!("repository '{id}'"),
                    &mut issues,
                ),
            });
            let _ = label;
        }

        let project = GitMeshProject {
            name,
            root: project_root,
            repositories,
        };

        if let Err(mut structural) = validation::validate_project(&project) {
            issues.append(&mut structural);
        }

        if issues.is_empty() {
            Ok(project)
        } else {
            Err(issues)
        }
    }

    /// Build a manifest representation from an internal project.
    pub fn from_project(project: &GitMeshProject) -> Result<ManifestFile> {
        let root = project.root_repository();
        Ok(ManifestFile {
            version: MANIFEST_VERSION,
            name: Some(project.name.clone()),
            root: Some(ManifestRoot {
                id: Some(root.id.clone()),
                path: Some(".".to_string()),
                remote: root.remote_url.clone(),
                branch: root.branch.clone(),
            }),
            repositories: project
                .external_repositories()
                .map(|repo| ManifestRepository {
                    id: Some(repo.id.clone()),
                    path: Some(paths::to_slash(&repo.relative_path)),
                    remote: repo.remote_url.clone(),
                    branch: repo.branch.clone(),
                })
                .collect(),
        })
    }
}

fn normalize_remote(raw: Option<&str>, owner: &str, issues: &mut Vec<String>) -> Option<String> {
    match raw.map(str::trim) {
        None => None,
        Some("") => {
            issues.push(format!("{owner} has an empty remote URL"));
            None
        }
        Some(url) => {
            if url.contains(char::is_whitespace) {
                issues.push(format!("{owner} remote URL '{url}' contains whitespace"));
                None
            } else if !looks_like_url(url) {
                issues.push(format!(
                    "{owner} remote URL '{url}' is not a supported Git URL \
                     (expected https://, ssh://, git@host:path, file:// or a local path)"
                ));
                None
            } else {
                Some(url.to_string())
            }
        }
    }
}

fn looks_like_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("https://")
        || lower.starts_with("http://")
        || lower.starts_with("ssh://")
        || lower.starts_with("git://")
        || lower.starts_with("file://")
    {
        return true;
    }
    if url.starts_with('/') || url.starts_with("./") || url.starts_with("../") {
        return true;
    }
    // scp-like syntax: user@host:path
    if let Some((user_host, path)) = url.split_once(':') {
        return user_host.contains('@') && !user_host.contains('/') && !path.is_empty();
    }
    // Windows drive letters are handled by the absolute-path branch above on Windows;
    // anything else is rejected explicitly rather than silently accepted.
    false
}

fn normalize_branch(raw: Option<&str>, owner: &str, issues: &mut Vec<String>) -> Option<String> {
    match raw.map(str::trim) {
        None => None,
        Some("") => {
            issues.push(format!("{owner} has an empty branch name"));
            None
        }
        Some(name) => {
            let invalid = name.starts_with('-')
                || name.starts_with('/')
                || name.ends_with('/')
                || name.ends_with(".lock")
                || name.contains("..")
                || name.contains("//")
                || name.contains(char::is_whitespace)
                || name.contains(|c| {
                    c == '~'
                        || c == '^'
                        || c == ':'
                        || c == '?'
                        || c == '*'
                        || c == '['
                        || c == '\\'
                })
                || name == "@"
                || name.contains("@{");
            if invalid {
                issues.push(format!(
                    "{owner} branch '{name}' is not a valid Git branch name"
                ));
                None
            } else {
                Some(name.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"
version = 1
name = "demo"

[root]
remote = "git@github.com:acme/demo.git"
branch = "main"

[[repositories]]
id = "engine"
path = "engine"
remote = "https://github.com/acme/engine.git"

[[repositories]]
id = "renderer"
path = "vendor/renderer"
"#;

    fn parse(text: &str) -> Result<GitMeshProject> {
        parse_manifest(
            text,
            Path::new("/project"),
            Path::new("/project/.gitmesh/project.toml"),
        )
    }

    #[test]
    fn parses_a_valid_manifest() {
        let project = parse(VALID).unwrap();
        assert_eq!(project.name, "demo");
        assert_eq!(project.len(), 3);
        assert_eq!(project.root_repository().id, "root");
        assert_eq!(
            project.root_repository().remote_url.as_deref(),
            Some("git@github.com:acme/demo.git")
        );
        let engine = project.repository("engine").unwrap();
        assert!(!engine.is_root());
        assert_eq!(engine.relative_path, PathBuf::from("engine"));
        assert_eq!(engine.absolute_path, PathBuf::from("/project/engine"));
        let renderer = project.repository("renderer").unwrap();
        assert_eq!(renderer.relative_path, PathBuf::from("vendor/renderer"));
        assert!(renderer.remote_url.is_none());
    }

    #[test]
    fn root_section_is_optional() {
        let text = "version = 1\nname = \"mini\"\n";
        let project = parse(text).unwrap();
        assert_eq!(project.len(), 1);
        assert_eq!(project.root_repository().relative_path, PathBuf::from("."));
    }

    #[test]
    fn project_name_defaults_to_the_directory_name() {
        let text = "version = 1\n";
        let project = parse(text).unwrap();
        assert_eq!(project.name, "project"); // "/project"
    }

    #[test]
    fn rejects_unsupported_version() {
        let err = parse("version = 99\n").unwrap_err();
        assert!(err.to_string().contains("version 99"), "{err}");
    }

    #[test]
    fn rejects_malformed_toml() {
        let err = parse("version = ").unwrap_err();
        assert!(matches!(err, Error::Manifest(_)), "{err:?}");
    }

    #[test]
    fn detects_duplicate_ids() {
        let text = r#"
version = 1
[[repositories]]
id = "engine"
path = "engine"
[[repositories]]
id = "engine"
path = "other"
"#;
        let err = parse(text).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("duplicate"), "{message}");
        assert!(message.contains("engine"), "{message}");
    }

    #[test]
    fn detects_duplicate_paths() {
        let text = r#"
version = 1
[[repositories]]
id = "a"
path = "shared"
[[repositories]]
id = "b"
path = "shared"
"#;
        let message = parse(text).unwrap_err().to_string();
        assert!(message.contains("same path"), "{message}");
    }

    #[test]
    fn detects_overlapping_paths() {
        let text = r#"
version = 1
[[repositories]]
id = "engine"
path = "engine"
[[repositories]]
id = "nested"
path = "engine/nested"
"#;
        let message = parse(text).unwrap_err().to_string();
        assert!(message.contains("overlap"), "{message}");
    }

    #[test]
    fn rejects_escaping_or_absolute_paths() {
        let absolute = r#"
version = 1
[[repositories]]
id = "bad"
path = "/etc"
"#;
        assert!(parse(absolute)
            .unwrap_err()
            .to_string()
            .contains("relative"));

        let escaping = r#"
version = 1
[[repositories]]
id = "bad"
path = "../outside"
"#;
        assert!(parse(escaping).unwrap_err().to_string().contains(".."));
    }

    #[test]
    fn rejects_root_path_for_external_repository() {
        let text = r#"
version = 1
[[repositories]]
id = "bad"
path = "."
"#;
        let message = parse(text).unwrap_err().to_string();
        assert!(message.contains("project root"), "{message}");
    }

    #[test]
    fn rejects_bad_remote_urls_and_branches() {
        let text = r#"
version = 1
[[repositories]]
id = "engine"
path = "engine"
remote = "not-a-url"
"#;
        assert!(parse(text)
            .unwrap_err()
            .to_string()
            .contains("not a supported Git URL"));

        let bad_branch = r#"
version = 1
[[repositories]]
id = "engine"
path = "engine"
branch = "feature/..bad"
"#;
        assert!(parse(bad_branch)
            .unwrap_err()
            .to_string()
            .contains("valid Git branch"));
    }

    #[test]
    fn rejects_missing_id_or_path() {
        let no_id = "version = 1\n[[repositories]]\npath = \"engine\"\n";
        assert!(parse(no_id)
            .unwrap_err()
            .to_string()
            .contains("missing a logical id"));

        let no_path = "version = 1\n[[repositories]]\nid = \"engine\"\n";
        assert!(parse(no_path)
            .unwrap_err()
            .to_string()
            .contains("missing a path"));
    }

    #[test]
    fn collects_multiple_issues_at_once() {
        let text = r#"
version = 1
[[repositories]]
id = "a"
path = "/absolute"
[[repositories]]
id = "b"
"#;
        let err = parse(text).unwrap_err();
        let Error::InvalidConfiguration(issues) = err else {
            panic!("expected InvalidConfiguration")
        };
        assert!(issues.len() >= 2, "{issues:?}");
    }

    #[test]
    fn round_trips_through_serialisation() {
        let project = parse(VALID).unwrap();
        let text = render_manifest(&project).unwrap();
        let reparsed = parse(&text).unwrap();
        assert_eq!(project, reparsed);
    }

    #[test]
    fn saved_manifest_is_readable_and_discoverable() {
        let dir = crate::testkit::TempDir::new("manifest-tests").unwrap();
        let project =
            parse_manifest(VALID, dir.path(), &dir.path().join(".gitmesh/project.toml")).unwrap();
        let path = save_project(&project).unwrap();
        assert!(path.ends_with(".gitmesh/project.toml"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[repositories]]"));
        assert!(text.contains("name = \"demo\""));

        let found = find_project_root(dir.path()).unwrap();
        assert_eq!(found, paths::lexical_normalize(dir.path()));
        let nested = dir.path().join("engine/src");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            find_project_root(&nested).unwrap(),
            paths::lexical_normalize(dir.path())
        );
        assert_eq!(load_from_root(dir.path()).unwrap().name, "demo");
        assert_eq!(load_nearest(&nested).unwrap().name, "demo");
    }

    #[test]
    fn missing_project_reports_a_helpful_error() {
        let dir = crate::testkit::TempDir::new("manifest-tests").unwrap();
        let err = load_from_root(dir.path()).unwrap_err();
        assert!(err.to_string().contains("no GitMesh project found"));
        assert!(err.to_string().contains("gitmesh init"));
    }

    #[test]
    fn save_rejects_invalid_projects() {
        let mut project = parse(VALID).unwrap();
        project.repositories[1].relative_path = PathBuf::from("vendor/renderer");
        // engine now overlaps nothing, but make it overlap on purpose:
        project.repositories[1].relative_path = PathBuf::from("vendor");
        let err = save_project(&project).unwrap_err();
        assert!(err.to_string().contains("overlap"), "{err}");
    }
}
