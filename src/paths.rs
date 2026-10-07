//! Path helpers.
//!
//! GitMesh has to reason about three different kinds of paths:
//!
//! * **absolute** filesystem paths (what the OS uses),
//! * **project-relative** paths (owned by exactly one physical repository, used for
//!   ownership decisions and user-facing output),
//! * **repository-relative** paths (what a `git` invocation inside one repository
//!   understands).
//!
//! All of them are normalised through this module so that comparisons are reliable
//! across platforms (`\` vs `/`, trailing separators, `.` segments, redundant
//! separators). Paths coming from a manifest are always project-relative and are
//! validated to be relative, non-escaping and normalised.

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// Lexically normalise a path without touching the filesystem.
///
/// Resolves `.` and `a/../` segments textually. `..` segments that would escape the
/// beginning of the path are kept only when the path is relative (they are rejected
/// later by [`normalize_relative`]); for absolute paths they are dropped at the root,
/// matching what the operating system does.
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => {
                out.push(component.as_os_str());
                depth = 0;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if depth > 0 {
                    if out.pop() {
                        depth -= 1;
                    }
                } else if !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => {
                out.push(part);
                depth += 1;
            }
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Convert a path to its project-relative form.
///
/// Returns `None` when `path` is not inside `root`. The root itself maps to `.`.
pub fn project_relative(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = lexical_normalize(root);
    let path = lexical_normalize(path);
    let rel = path.strip_prefix(&root).ok()?;
    if rel.as_os_str().is_empty() {
        Some(PathBuf::from("."))
    } else {
        Some(rel.to_path_buf())
    }
}

/// True when `path` is `root` or is a descendant of `root`.
pub fn is_within(root: &Path, path: &Path) -> bool {
    let root = lexical_normalize(root);
    let path = lexical_normalize(path);
    path == root || path.starts_with(&root)
}

/// True when `outer` is an ancestor of, but not equal to, `inner`.
pub fn is_strict_ancestor(outer: &Path, inner: &Path) -> bool {
    let outer = lexical_normalize(outer);
    let inner = lexical_normalize(inner);
    outer != inner && inner.starts_with(&outer)
}

/// Render a path with forward slashes, which is what Git and the manifest use.
///
/// Root and prefix components are handled explicitly so an absolute path renders as
/// `/tmp/project` (not `//tmp/project`) and a Windows path as `C:/project`.
pub fn to_slash(path: &Path) -> String {
    let mut out = String::new();
    let mut needs_separator = false;
    for component in path.components() {
        match component {
            std::path::Component::Prefix(prefix) => {
                out.push_str(&prefix.as_os_str().to_string_lossy());
                needs_separator = false;
            }
            std::path::Component::RootDir => {
                out.push('/');
                needs_separator = false;
            }
            std::path::Component::CurDir => {
                if needs_separator {
                    out.push('/');
                }
                out.push('.');
                needs_separator = true;
            }
            std::path::Component::ParentDir => {
                if needs_separator {
                    out.push('/');
                }
                out.push_str("..");
                needs_separator = true;
            }
            std::path::Component::Normal(part) => {
                if needs_separator {
                    out.push('/');
                }
                out.push_str(&part.to_string_lossy());
                needs_separator = true;
            }
        }
    }
    out
}

/// Shorten a path for display: paths inside a project are shown project-relative.
pub fn display_relative(root: &Path, path: &Path) -> String {
    match project_relative(root, path) {
        Some(rel) => to_slash(&rel),
        None => to_slash(path),
    }
}

/// Parse a manifest path: must be relative, must not escape the project root, must
/// not contain `..` after normalisation, and must not point below `.git`.
pub fn normalize_relative(raw: &str) -> Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::Manifest("repository path must not be empty".into()));
    }
    if raw.contains('\0') {
        return Err(Error::Manifest(format!(
            "repository path '{raw}' contains a NUL byte"
        )));
    }
    let as_path = Path::new(raw);
    if as_path.is_absolute() {
        return Err(Error::Manifest(format!(
            "repository path '{raw}' must be relative to the project root, not absolute"
        )));
    }
    let has_parent = as_path
        .components()
        .any(|c| matches!(c, Component::ParentDir));
    let normalized = lexical_normalize(as_path);
    if has_parent {
        return Err(Error::Manifest(format!(
            "repository path '{raw}' must not contain '..' components"
        )));
    }
    if normalized.as_os_str().is_empty() {
        return Ok(PathBuf::from("."));
    }
    let mut components = normalized.components();
    if let Some(Component::Normal(first)) = components.next() {
        if first == ".git" {
            return Err(Error::Manifest(format!(
                "repository path '{raw}' must not point inside the Git metadata directory"
            )));
        }
    }
    Ok(normalized)
}

/// True when the relative path refers to the project root itself.
pub fn is_root_relative(rel: &Path) -> bool {
    rel.as_os_str().is_empty() || rel == Path::new(".") || rel == Path::new("./")
}

/// Resolve a path argument to an absolute, lexically normalised path.
///
/// The path does not have to exist (that is what makes it usable for "open this
/// directory" flows), and nothing is resolved through symlinks: GitMesh treats the
/// path the user typed as the path the user meant.
pub fn absolute(path: &std::path::Path) -> Result<std::path::PathBuf> {
    use std::path::PathBuf;
    if path.is_absolute() {
        return Ok(lexical_normalize(path));
    }
    let cwd = std::env::current_dir().map_err(|e| crate::Error::io(PathBuf::from("."), e))?;
    Ok(lexical_normalize(&cwd.join(path)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_curdir_and_trailing_slashes() {
        assert_eq!(lexical_normalize(Path::new("./a/b/")), PathBuf::from("a/b"));
        assert_eq!(lexical_normalize(Path::new("a//b")), PathBuf::from("a/b"));
        assert_eq!(lexical_normalize(Path::new(".")), PathBuf::from("."));
    }

    #[test]
    fn normalizes_parent_segments() {
        assert_eq!(
            lexical_normalize(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
        assert_eq!(lexical_normalize(Path::new("a/..")), PathBuf::from("."));
        assert_eq!(lexical_normalize(Path::new("../a")), PathBuf::from("../a"));
    }

    #[test]
    fn project_relative_of_root_is_dot() {
        assert_eq!(
            project_relative(Path::new("/p"), Path::new("/p")).unwrap(),
            PathBuf::from(".")
        );
        assert_eq!(
            project_relative(Path::new("/p"), Path::new("/p/a/b")).unwrap(),
            PathBuf::from("a/b")
        );
        assert!(project_relative(Path::new("/p"), Path::new("/other")).is_none());
    }

    #[test]
    fn normalize_relative_rejects_bad_input() {
        assert!(normalize_relative("/abs/path").is_err());
        assert!(normalize_relative("../escape").is_err());
        assert!(normalize_relative("a/../../b").is_err());
        assert!(normalize_relative("").is_err());
        assert!(normalize_relative(".git/config").is_err());
        assert_eq!(
            normalize_relative("./engine/").unwrap(),
            PathBuf::from("engine")
        );
        assert_eq!(normalize_relative(".").unwrap(), PathBuf::from("."));
    }

    #[test]
    fn ancestor_checks() {
        assert!(is_strict_ancestor(Path::new("/p/a"), Path::new("/p/a/b")));
        assert!(!is_strict_ancestor(Path::new("/p/a"), Path::new("/p/a")));
        assert!(!is_strict_ancestor(Path::new("/p/a"), Path::new("/p/ab")));
        assert!(is_within(Path::new("/p"), Path::new("/p/a/b")));
    }

    #[test]
    fn slash_conversion() {
        assert_eq!(to_slash(Path::new("a/b/c")), "a/b/c");
        assert_eq!(to_slash(Path::new("/tmp/project")), "/tmp/project");
        assert_eq!(to_slash(Path::new("/tmp/project/")), "/tmp/project");
        assert_eq!(to_slash(Path::new("./a")), "./a");
        assert_eq!(to_slash(Path::new("/")), "/");
        #[cfg(windows)]
        assert_eq!(to_slash(Path::new(r"a\b")), "a/b");
    }
}
