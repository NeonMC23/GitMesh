//! GitHub specifics: URL parsing and repository coordinates.
//!
//! Scope for now: understand GitHub URLs and express them in a way that a future API
//! client can use. Anything that requires the network (creating repositories, opening
//! pull requests, reading issues) is intentionally absent — GitHub must never become a
//! dependency of local GitMesh operation.

use super::{Provider, RemoteRef};

/// The GitHub provider.
#[derive(Debug, Clone, Copy, Default)]
pub struct GitHubProvider;

/// Hosts recognised as GitHub (including GitHub Enterprise, which uses `github.` in
/// its hostname by convention — the API is addressed separately).
const KNOWN_HOSTS: &[&str] = &["github.com"];

impl Provider for GitHubProvider {
    fn id(&self) -> &'static str {
        "github"
    }

    fn display_name(&self) -> &'static str {
        "GitHub"
    }

    fn web_base_url(&self) -> &'static str {
        "https://github.com"
    }

    fn matches(&self, remote_url: &str) -> bool {
        let lower = remote_url.to_ascii_lowercase();
        KNOWN_HOSTS.iter().any(|host| lower.contains(host))
    }

    fn parse_remote(&self, remote_url: &str) -> Option<RemoteRef> {
        let (owner, name) = split_owner_and_name(remote_url)?;
        Some(RemoteRef {
            provider: "github",
            owner,
            name,
            url: remote_url.to_string(),
        })
    }
}

/// Split a GitHub remote URL into `(owner, repository)`.
///
/// Supported layouts:
///
/// * `git@github.com:owner/repo.git`
/// * `ssh://git@github.com/owner/repo.git`
/// * `https://github.com/owner/repo.git`
/// * `https://user@github.com/owner/repo`
///
/// GitHub has no nested groups, so the path must be exactly `owner/repo`. Anything
/// else is rejected instead of being guessed at.
pub fn split_owner_and_name(remote_url: &str) -> Option<(String, String)> {
    let trimmed = remote_url.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = if let Some(rest) = trimmed.strip_prefix("git@") {
        // scp-like syntax: host:owner/repo.git
        let (_host, path) = rest.split_once(':')?;
        path.to_string()
    } else {
        let without_scheme = trimmed
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(trimmed);
        // Strip credentials and host.
        let after_host = without_scheme.split_once('/')?.1;
        after_host.to_string()
    };

    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((owner.to_string(), name.to_string()))
}

/// A GitHub repository target, as needed to configure a remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubTarget {
    /// Organisation or user.
    pub owner: String,
    /// Repository name.
    pub name: String,
    /// True when the remote URL exists already and GitMesh only recorded it.
    pub existing: bool,
}

/// A GitHub repository as described by a configured remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepo {
    /// Organisation or user.
    pub owner: String,
    /// Repository name.
    pub name: String,
    /// The remote URL configured locally.
    pub remote_url: String,
    /// True when this remote is reachable without credentials (https) — informational.
    pub https: bool,
}

impl GitHubRepo {
    /// Parse a GitHub repository from a remote URL.
    pub fn from_remote_url(remote_url: &str) -> Option<GitHubRepo> {
        let (owner, name) = split_owner_and_name(remote_url)?;
        Some(GitHubRepo {
            owner,
            name,
            remote_url: remote_url.to_string(),
            https: remote_url.starts_with("https://") || remote_url.starts_with("http://"),
        })
    }

    /// `owner/name`
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Web URL for humans.
    pub fn web_url(&self) -> String {
        format!("https://github.com/{}", self.full_name())
    }

    /// The URL GitMesh would configure for `owner/name` over SSH.
    pub fn ssh_url(&self) -> String {
        format!("git@github.com:{}.git", self.full_name())
    }

    /// The URL GitMesh would configure for `owner/name` over HTTPS.
    pub fn https_url(&self) -> String {
        format!("https://github.com/{}.git", self.full_name())
    }
}

/// Build a remote URL for a GitHub repository GitMesh is about to use.
pub fn remote_url_for(target: &GitHubTarget, prefer_ssh: bool) -> String {
    let repo = GitHubRepo {
        owner: target.owner.clone(),
        name: target.name.clone(),
        remote_url: String::new(),
        https: !prefer_ssh,
    };
    if prefer_ssh {
        repo.ssh_url()
    } else {
        repo.https_url()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_supported_url_layout() {
        let cases = [
            ("git@github.com:acme/engine.git", "acme", "engine"),
            ("git@github.com:acme/engine", "acme", "engine"),
            ("ssh://git@github.com/acme/engine.git", "acme", "engine"),
            ("https://github.com/acme/engine.git", "acme", "engine"),
            ("https://github.com/acme/engine", "acme", "engine"),
            ("https://user@github.com/acme/engine.git", "acme", "engine"),
        ];
        for (url, owner, name) in cases {
            let (parsed_owner, parsed_name) =
                split_owner_and_name(url).unwrap_or_else(|| panic!("{url}"));
            assert_eq!(parsed_owner, owner, "{url}");
            assert_eq!(parsed_name, name, "{url}");
        }
    }

    #[test]
    fn rejects_urls_without_a_repository() {
        assert!(split_owner_and_name("https://github.com/acme").is_none());
        assert!(split_owner_and_name("https://github.com/a/b/c").is_none());
        assert!(split_owner_and_name("").is_none());
        assert!(split_owner_and_name("github.com").is_none());
    }

    #[test]
    fn builds_urls() {
        let repo = GitHubRepo::from_remote_url("git@github.com:acme/engine.git").unwrap();
        assert_eq!(repo.full_name(), "acme/engine");
        assert_eq!(repo.web_url(), "https://github.com/acme/engine");
        assert_eq!(repo.https_url(), "https://github.com/acme/engine.git");
        assert_eq!(repo.ssh_url(), "git@github.com:acme/engine.git");
        assert!(!repo.https);
    }

    #[test]
    fn target_urls_follow_the_preference() {
        let target = GitHubTarget {
            owner: "acme".into(),
            name: "engine".into(),
            existing: true,
        };
        assert_eq!(
            remote_url_for(&target, true),
            "git@github.com:acme/engine.git"
        );
        assert_eq!(
            remote_url_for(&target, false),
            "https://github.com/acme/engine.git"
        );
    }

    #[test]
    fn provider_detects_github_hosts_only() {
        assert!(GitHubProvider.matches("https://github.com/a/b"));
        assert!(GitHubProvider.matches("git@github.com:a/b.git"));
        assert!(!GitHubProvider.matches("https://gitlab.com/a/b"));
        assert!(!GitHubProvider.matches("/srv/git/a.git"));
    }
}
