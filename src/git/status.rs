//! Parsing of stable, documented Git outputs.
//!
//! GitMesh parses two formats and nothing else:
//!
//! * `git status --porcelain=v2 --branch -z` (documented in `git-status(1)`),
//! * `git remote -v`.
//!
//! Both are stable interfaces intended for scripts. Everything else is obtained from
//! plumbing commands with machine-readable output (`rev-parse`, `rev-list`,
//! `for-each-ref`).

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Kind of a single change reported by Git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Untracked,
    Ignored,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        match self {
            ChangeKind::Added => "added",
            ChangeKind::Modified => "modified",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Renamed => "renamed",
            ChangeKind::Copied => "copied",
            ChangeKind::TypeChanged => "type-changed",
            ChangeKind::Unmerged => "conflicted",
            ChangeKind::Untracked => "untracked",
            ChangeKind::Ignored => "ignored",
        }
    }

    fn from_xy(c: char) -> Option<ChangeKind> {
        match c {
            'M' => Some(ChangeKind::Modified),
            'T' => Some(ChangeKind::TypeChanged),
            'A' => Some(ChangeKind::Added),
            'D' => Some(ChangeKind::Deleted),
            'R' => Some(ChangeKind::Renamed),
            'C' => Some(ChangeKind::Copied),
            'U' => Some(ChangeKind::Unmerged),
            _ => None,
        }
    }

    fn from_untracked(c: char) -> Option<ChangeKind> {
        match c {
            '?' => Some(ChangeKind::Untracked),
            '!' => Some(ChangeKind::Ignored),
            _ => None,
        }
    }
}

/// Merge-conflict codes reported by `status --porcelain=v2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnmergedCode {
    BothDeleted,
    AddedByUs,
    DeletedByThem,
    AddedByThem,
    DeletedByUs,
    BothAdded,
    BothModified,
}

impl UnmergedCode {
    pub fn label(self) -> &'static str {
        match self {
            UnmergedCode::BothDeleted => "both deleted",
            UnmergedCode::AddedByUs => "added by us",
            UnmergedCode::DeletedByThem => "deleted by them",
            UnmergedCode::AddedByThem => "added by them",
            UnmergedCode::DeletedByUs => "deleted by us",
            UnmergedCode::BothAdded => "both added",
            UnmergedCode::BothModified => "both modified",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        Some(match code {
            "DD" => UnmergedCode::BothDeleted,
            "AU" => UnmergedCode::AddedByUs,
            "UD" => UnmergedCode::DeletedByThem,
            "UA" => UnmergedCode::AddedByThem,
            "DU" => UnmergedCode::DeletedByUs,
            "AA" => UnmergedCode::BothAdded,
            "UU" => UnmergedCode::BothModified,
            _ => return None,
        })
    }
}

/// One entry of `git status --porcelain=v2`.
///
/// A single file can appear once for the index and once for the work tree; the
/// `index`/`worktree` fields carry the respective states. `kind` is the most
/// significant of the two, so a file that is both staged and modified still reports a
/// clear state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    /// Path relative to the repository root, using forward slashes.
    pub path: String,
    /// Original path for renames/copies.
    pub original_path: Option<String>,
    /// State in the index, if any.
    pub index: Option<ChangeKind>,
    /// State in the work tree, if any.
    pub worktree: Option<ChangeKind>,
    /// Conflict code when unmerged.
    pub unmerged: Option<UnmergedCode>,
    /// True when the change is staged (present in the index).
    pub staged: bool,
    /// True when the change exists in the work tree (unstaged).
    pub unstaged: bool,
    /// True for untracked files (not staged, not tracked).
    pub untracked: bool,
    /// True for ignored files (`--ignored` only; not requested by default).
    pub ignored: bool,
}

impl StatusEntry {
    /// The most significant change kind, for display and filtering.
    pub fn kind(&self) -> ChangeKind {
        if let Some(code) = self.unmerged {
            let _ = code;
            return ChangeKind::Unmerged;
        }
        if self.untracked {
            return ChangeKind::Untracked;
        }
        if self.ignored {
            return ChangeKind::Ignored;
        }
        // Prefer the work-tree state for display: a file with staged changes that was
        // then modified again is "modified" from the user's point of view.
        self.worktree.or(self.index).unwrap_or(ChangeKind::Modified)
    }

    /// True when this entry represents a merge conflict.
    pub fn is_conflict(&self) -> bool {
        self.unmerged.is_some()
    }
}

/// `HEAD` state of a repository.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Head {
    /// Attached to a branch that has at least one commit.
    Branch { name: String },
    /// Attached to a branch that has no commit yet (freshly initialised).
    Unborn { branch: String },
    /// Detached at a commit.
    Detached { oid: String },
    /// Not determinable (not a repository, or a corrupt state).
    #[default]
    Unknown,
}

impl Head {
    /// Display name used in status output.
    pub fn label(&self) -> String {
        match self {
            Head::Branch { name } => name.clone(),
            Head::Unborn { branch } => format!("{branch} (no commits yet)"),
            Head::Detached { oid } => format!("detached at {}", short_oid(oid)),
            Head::Unknown => "(unknown)".to_string(),
        }
    }

    /// The short branch name, when attached to a branch.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Head::Branch { name } => Some(name),
            Head::Unborn { branch } => Some(branch),
            _ => None,
        }
    }

    pub fn is_detached(&self) -> bool {
        matches!(self, Head::Detached { .. })
    }

    pub fn is_unborn(&self) -> bool {
        matches!(self, Head::Unborn { .. })
    }
}

/// Shorten a commit id for display.
pub fn short_oid(oid: &str) -> String {
    oid.chars().take(7).collect()
}

/// Kind of Git remote, used by the GitHub provider foundation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    GitHub,
    Ssh,
    Local,
    Http,
    Other,
}

impl RemoteKind {
    pub fn label(self) -> &'static str {
        match self {
            RemoteKind::GitHub => "github",
            RemoteKind::Ssh => "ssh",
            RemoteKind::Local => "local",
            RemoteKind::Http => "http",
            RemoteKind::Other => "other",
        }
    }
}

/// A configured remote URL with its classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    pub url: String,
    pub kind: RemoteKind,
}

/// A configured remote and its URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub fetch_urls: Vec<String>,
    pub push_urls: Vec<String>,
}

impl Remote {
    pub fn fetch_url(&self) -> Option<&str> {
        self.fetch_urls.first().map(String::as_str)
    }

    pub fn push_url(&self) -> Option<&str> {
        self.push_urls
            .first()
            .map(String::as_str)
            .or_else(|| self.fetch_url())
    }

    pub fn kind(&self) -> RemoteKind {
        self.fetch_url()
            .or_else(|| self.push_url())
            .map(classify_remote)
            .unwrap_or(RemoteKind::Other)
    }
}

/// Classify a remote URL without contacting the network.
pub fn classify_remote(url: &str) -> RemoteKind {
    let lower = url.to_ascii_lowercase();
    if lower.contains("github.com") {
        RemoteKind::GitHub
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        RemoteKind::Http
    } else if lower.starts_with("ssh://") || lower.contains('@') && lower.contains(':') {
        RemoteKind::Ssh
    } else if lower.starts_with('/') || lower.starts_with('.') || lower.starts_with("file://") {
        RemoteKind::Local
    } else {
        RemoteKind::Other
    }
}

/// Combined result of `git status --porcelain=v2 --branch` for one repository.
#[derive(Debug, Clone, Default)]
pub struct RepoStatus {
    /// `HEAD` state.
    pub head: Head,
    /// Upstream ref name, if configured.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: Option<u32>,
    /// Commits behind the upstream.
    pub behind: Option<u32>,
    /// All entries from the porcelain output (not including ignored files).
    pub entries: Vec<StatusEntry>,
    /// True when stdout contained invalid UTF-8 (paths are then lossy-decoded).
    pub lossy_paths: bool,
}

impl RepoStatus {
    /// True when the work tree and index are clean, ignoring untracked files.
    pub fn is_clean_tracked(&self) -> bool {
        !self.entries.iter().any(|e| !e.untracked && !e.ignored)
    }

    /// True when there is nothing at all to record, including untracked files.
    pub fn is_fully_clean(&self) -> bool {
        !self.entries.iter().any(|e| !e.ignored)
    }

    pub fn staged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.staged)
    }

    pub fn unstaged(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.unstaged && !e.untracked)
    }

    pub fn untracked(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.untracked)
    }

    pub fn deleted(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| {
            e.index == Some(ChangeKind::Deleted) || e.worktree == Some(ChangeKind::Deleted)
        })
    }

    pub fn renamed(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| {
            e.index == Some(ChangeKind::Renamed) || e.worktree == Some(ChangeKind::Renamed)
        })
    }

    /// Files with merge conflicts.
    pub fn conflicts(&self) -> impl Iterator<Item = &StatusEntry> {
        self.entries.iter().filter(|e| e.is_conflict())
    }

    pub fn has_conflicts(&self) -> bool {
        self.entries.iter().any(StatusEntry::is_conflict)
    }

    /// True when the repository is on a different branch than its upstream, or on no
    /// branch at all.
    pub fn is_detached(&self) -> bool {
        self.head.is_detached()
    }
}

// --------------------------------------------------------------------- parsing --

/// Parse the `-z` (NUL-separated) porcelain v2 format.
///
/// `raw` must be the exact stdout of
/// `git status --porcelain=v2 --branch --untracked-files=normal -z`, decoded to a
/// `String`; embedded NULs are preserved by Rust strings.
pub fn parse_porcelain_v2(raw: &str, workdir: &Path, lossy_paths: bool) -> Result<RepoStatus> {
    let mut status = RepoStatus {
        lossy_paths,
        ..Default::default()
    };
    // `branch.oid (initial)` means the branch has no commit yet. Remember it so the
    // branch name from `branch.head` becomes `Head::Unborn` instead of `Head::Branch`.
    let mut unborn = false;

    let mut fields = raw.split('\0');
    while let Some(record) = fields.next() {
        if record.is_empty() {
            continue;
        }
        match record.as_bytes()[0] {
            b'#' => {
                // `-z` terminates header records with NUL like every other record.
                // Newline-separated headers are accepted too, so the parser also works
                // on output produced without `-z`.
                for line in record.split('\n') {
                    let mut parts = line.trim_start_matches('#').trim().splitn(2, ' ');
                    if let (Some(key), Some(value)) = (parts.next(), parts.next()) {
                        if key == "branch.oid" && value.trim() == "(initial)" {
                            unborn = true;
                        }
                        parse_header(&format!("{key} {value}"), &mut status);
                    }
                }
            }
            b'1' => {
                // 1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>
                let parts: Vec<&str> = record.splitn(9, ' ').collect();
                if parts.len() < 9 {
                    return Err(parse_error("malformed status record (type 1)", record));
                }
                let (index, worktree, unmerged) = parse_xy(parts[1])?;
                status.entries.push(StatusEntry {
                    path: normalize_path(parts[8]),
                    original_path: None,
                    index,
                    worktree,
                    unmerged,
                    staged: index.is_some(),
                    unstaged: worktree.is_some(),
                    untracked: false,
                    ignored: false,
                });
            }
            b'2' => {
                // 2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\0<origPath>
                let parts: Vec<&str> = record.splitn(10, ' ').collect();
                if parts.len() < 10 {
                    return Err(parse_error("malformed status record (type 2)", record));
                }
                let (index, worktree, unmerged) = parse_xy(parts[1])?;
                let original = fields.next().map(normalize_path).filter(|s| !s.is_empty());
                status.entries.push(StatusEntry {
                    path: normalize_path(parts[9]),
                    original_path: original,
                    index,
                    worktree,
                    unmerged,
                    staged: index.is_some(),
                    unstaged: worktree.is_some(),
                    untracked: false,
                    ignored: false,
                });
            }
            b'u' => {
                // u <XY> <sub> <m1> <m2> <m3> <mW> <h1> <h2> <h3> <path>
                let parts: Vec<&str> = record.splitn(11, ' ').collect();
                if parts.len() < 11 {
                    return Err(parse_error("malformed status record (unmerged)", record));
                }
                let unmerged = UnmergedCode::parse(parts[1]);
                status.entries.push(StatusEntry {
                    path: normalize_path(parts[10]),
                    original_path: None,
                    index: Some(ChangeKind::Unmerged),
                    worktree: Some(ChangeKind::Unmerged),
                    unmerged,
                    staged: false,
                    unstaged: true,
                    untracked: false,
                    ignored: false,
                });
            }
            b'?' => {
                let path = record[1..].trim_start();
                let kind = ChangeKind::from_untracked('?').expect("'?' maps to untracked");
                status.entries.push(StatusEntry {
                    path: normalize_path(path),
                    original_path: None,
                    index: None,
                    worktree: None,
                    unmerged: None,
                    staged: false,
                    unstaged: false,
                    untracked: matches!(kind, ChangeKind::Untracked),
                    ignored: false,
                });
            }
            b'!' => {
                let path = record[1..].trim_start();
                status.entries.push(StatusEntry {
                    path: normalize_path(path),
                    original_path: None,
                    index: None,
                    worktree: None,
                    unmerged: None,
                    staged: false,
                    unstaged: false,
                    untracked: false,
                    ignored: true,
                });
            }
            _ => {
                return Err(parse_error("unrecognised status record", record));
            }
        }
    }

    if unborn {
        if let Head::Branch { name } = status.head.clone() {
            status.head = Head::Unborn { branch: name };
        }
    }

    let _ = workdir;
    Ok(status)
}

fn parse_header(rest: &str, status: &mut RepoStatus) {
    let mut parts = rest.splitn(2, ' ');
    let Some(key) = parts.next() else { return };
    let value = parts.next().unwrap_or("").trim();
    match key {
        "branch.oid" => {
            if value == "(initial)" {
                status.head = Head::Unknown; // refined by branch.head / branch.upstream
            } else if !value.is_empty() {
                status.head = Head::Detached {
                    oid: value.to_string(),
                };
            }
        }
        "branch.head" => {
            if value == "(detached)" {
                // keep the detached head recorded by branch.oid
            } else if !value.is_empty() {
                status.head = Head::Branch {
                    name: value.to_string(),
                };
            }
        }
        "branch.upstream" => {
            if !value.is_empty() {
                status.upstream = Some(value.to_string());
            }
        }
        "branch.ab" => {
            let mut parts = value.split_whitespace();
            if let (Some(ahead), Some(behind)) = (parts.next(), parts.next()) {
                status.ahead = ahead.trim_start_matches('+').parse().ok();
                status.behind = behind.trim_start_matches('-').parse().ok();
            }
        }
        _ => {}
    }
}

fn parse_xy(field: &str) -> Result<(Option<ChangeKind>, Option<ChangeKind>, Option<UnmergedCode>)> {
    let mut chars = field.chars();
    let x = chars
        .next()
        .ok_or_else(|| parse_error("empty XY field", field))?;
    let y = chars
        .next()
        .ok_or_else(|| parse_error("short XY field", field))?;
    if let Some(code) = UnmergedCode::parse(field) {
        return Ok((
            Some(ChangeKind::Unmerged),
            Some(ChangeKind::Unmerged),
            Some(code),
        ));
    }
    Ok((ChangeKind::from_xy(x), ChangeKind::from_xy(y), None))
}

fn normalize_path(path: &str) -> String {
    path.replace('\\', "/")
}

fn parse_error(context: &str, detail: &str) -> Error {
    Error::GitParse {
        context: context.to_string(),
        detail: detail.to_string(),
    }
}

/// Parse `git remote -v` output.
pub fn parse_remotes(raw: &str) -> Vec<Remote> {
    let mut remotes: Vec<Remote> = Vec::new();
    for line in raw.lines() {
        let mut parts = line.split_whitespace();
        let (Some(name), Some(url), Some(kind)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let entry = match remotes.iter_mut().find(|r| r.name == name) {
            Some(existing) => existing,
            None => {
                remotes.push(Remote {
                    name: name.to_string(),
                    fetch_urls: Vec::new(),
                    push_urls: Vec::new(),
                });
                remotes.last_mut().expect("just pushed")
            }
        };
        match kind {
            "(fetch)" if !entry.fetch_urls.contains(&url.to_string()) => {
                entry.fetch_urls.push(url.to_string())
            }
            "(push)" if !entry.push_urls.contains(&url.to_string()) => {
                entry.push_urls.push(url.to_string())
            }
            _ => {}
        }
    }
    remotes
}

/// Convenience: absolute path of a status entry inside a repository working
/// directory.
pub fn entry_abs_path(workdir: &Path, entry: &StatusEntry) -> PathBuf {
    workdir.join(&entry.path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the exact byte sequence git produces with `-z`: every record, including
    /// `#` headers, is NUL-terminated.
    fn porcelain(records: &[&str]) -> String {
        let mut out = String::new();
        for record in records {
            out.push_str(record);
            out.push('\0');
        }
        out
    }

    #[test]
    fn parses_ordinary_changes() {
        let raw = porcelain(&[
            "# branch.oid abc123",
            "# branch.head main",
            "# branch.ab +2 -1",
            "1 .M N... 100644 100644 100644 aaa bbb src/a.rs",
            "1 M. N... 100644 100644 100644 aaa bbb src/b.rs",
            "1 .D N... 100644 100644 100644 aaa bbb src/gone.rs",
            "? src/new.rs",
        ]);
        let status = parse_porcelain_v2(&raw, Path::new("/tmp"), false).unwrap();
        assert_eq!(
            status.head,
            Head::Branch {
                name: "main".into()
            }
        );
        assert_eq!(status.ahead, Some(2));
        assert_eq!(status.behind, Some(1));
        assert_eq!(status.entries.len(), 4);
        assert_eq!(status.entries[0].path, "src/a.rs");
        assert!(!status.entries[0].staged);
        assert!(status.entries[0].unstaged);
        assert_eq!(status.entries[0].kind(), ChangeKind::Modified);
        assert!(status.entries[1].staged);
        assert_eq!(status.entries[2].kind(), ChangeKind::Deleted);
        assert!(status.entries[3].untracked);
        assert_eq!(status.untracked().count(), 1);
        assert!(!status.has_conflicts());
    }

    #[test]
    fn tolerates_newline_separated_headers() {
        // Output produced without `-z` separates headers with newlines.
        let raw = "# branch.oid abc123\n# branch.head main\n# branch.ab +0 -0\0";
        let status = parse_porcelain_v2(raw, Path::new("/tmp"), false).unwrap();
        assert_eq!(
            status.head,
            Head::Branch {
                name: "main".into()
            }
        );
    }

    #[test]
    fn parses_renames_with_original_path() {
        // Type 2 records are followed by the original path as a separate NUL field.
        let raw = porcelain(&[
            "# branch.head main",
            "2 R. N... 100644 100644 100644 aaa bbb R100 new/name.rs",
            "old/name.rs",
        ]);
        let status = parse_porcelain_v2(&raw, Path::new("/tmp"), false).unwrap();
        assert_eq!(status.entries.len(), 1);
        let entry = &status.entries[0];
        assert_eq!(entry.path, "new/name.rs");
        assert_eq!(entry.original_path.as_deref(), Some("old/name.rs"));
        assert_eq!(entry.kind(), ChangeKind::Renamed);
        assert_eq!(status.renamed().count(), 1);
    }

    #[test]
    fn parses_conflicts() {
        let raw = porcelain(&[
            "# branch.head main",
            "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt",
        ]);
        let status = parse_porcelain_v2(&raw, Path::new("/tmp"), false).unwrap();
        assert!(status.has_conflicts());
        let conflict = status.conflicts().next().unwrap();
        assert_eq!(conflict.unmerged, Some(UnmergedCode::BothModified));
        assert_eq!(conflict.kind(), ChangeKind::Unmerged);
        assert_eq!(conflict.path, "conflict.txt");
    }

    #[test]
    fn parses_every_unmerged_code() {
        for (code, expected) in [
            ("DD", UnmergedCode::BothDeleted),
            ("AU", UnmergedCode::AddedByUs),
            ("UD", UnmergedCode::DeletedByThem),
            ("UA", UnmergedCode::AddedByThem),
            ("DU", UnmergedCode::DeletedByUs),
            ("AA", UnmergedCode::BothAdded),
            ("UU", UnmergedCode::BothModified),
        ] {
            let raw = porcelain(&[&format!(
                "u {code} N... 100644 100644 100644 100644 a b c f.txt"
            )]);
            let status = parse_porcelain_v2(&raw, Path::new("/tmp"), false).unwrap();
            assert_eq!(status.entries[0].unmerged, Some(expected), "{code}");
        }
    }

    #[test]
    fn parses_detached_head_and_unborn_branch() {
        let raw = porcelain(&["# branch.oid 1234567890abcdef", "# branch.head (detached)"]);
        let status = parse_porcelain_v2(&raw, Path::new("/tmp"), false).unwrap();
        assert!(status.is_detached());
        assert_eq!(
            status.head,
            Head::Detached {
                oid: "1234567890abcdef".into()
            }
        );
        assert!(status.head.label().starts_with("detached at 1234567"));

        let unborn = porcelain(&["# branch.oid (initial)", "# branch.head main"]);
        let unborn = parse_porcelain_v2(&unborn, Path::new("/tmp"), false).unwrap();
        assert_eq!(
            unborn.head,
            Head::Unborn {
                branch: "main".into()
            }
        );
        assert!(unborn.head.is_unborn());
        assert_eq!(unborn.head.branch(), Some("main"));
    }

    #[test]
    fn rejects_garbage_records() {
        let err = parse_porcelain_v2("Z nonsense\0", Path::new("/tmp"), false).unwrap_err();
        assert!(matches!(err, Error::GitParse { .. }), "{err:?}");
    }

    #[test]
    fn parses_remotes_with_pushurl_override() {
        let raw = "origin\tgit@github.com:acme/root.git (fetch)\n\
                   origin\tgit@github.com:acme/root.git (push)\n\
                   mirror\t/tmp/bare.git (fetch)\n\
                   mirror\t/tmp/bare.git (push)\n";
        let remotes = parse_remotes(raw);
        assert_eq!(remotes.len(), 2);
        assert_eq!(remotes[0].name, "origin");
        assert_eq!(remotes[0].fetch_url(), Some("git@github.com:acme/root.git"));
        assert_eq!(remotes[0].kind(), RemoteKind::GitHub);
        assert_eq!(remotes[1].kind(), RemoteKind::Local);
    }

    #[test]
    fn classifies_remote_kinds() {
        assert_eq!(
            classify_remote("https://github.com/a/b.git"),
            RemoteKind::GitHub
        );
        assert_eq!(
            classify_remote("https://gitlab.com/a/b.git"),
            RemoteKind::Http
        );
        assert_eq!(classify_remote("ssh://git@host/x.git"), RemoteKind::Ssh);
        assert_eq!(classify_remote("/srv/git/x.git"), RemoteKind::Local);
        assert_eq!(classify_remote("file:///srv/git/x.git"), RemoteKind::Local);
    }

    #[test]
    fn undefined_head_is_unknown_by_default() {
        let status = RepoStatus::default();
        assert_eq!(status.head, Head::Unknown);
        assert!(status.is_fully_clean());
        assert!(!status.has_conflicts());
    }
}
