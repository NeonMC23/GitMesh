//! Hosting provider foundation (GitHub optional).
//!
//! GitMesh's core is local-only: nothing outside this module ever talks to a hosting
//! provider, and no operation requires one. This module provides the *description* of
//! a hosted remote:
//!
//! * recognising that a remote URL belongs to a known provider,
//! * extracting the organisation/owner, repository name and layout from the URL,
//! * a [`Provider`] trait so that a GitHub API client (or GitLab, or an internal
//!   Gitea) can be added later without changing the core.
//!
//! There is deliberately **no HTTP client** yet and no token handling: adding them
//! now would make GitHub a dependency of local work, which is exactly what the design
//! forbids. The trait below is the seam where such a client will plug in.

pub mod github;

pub use github::{GitHubRepo, GitHubTarget};

use crate::git::{classify_remote, RemoteKind};

/// A hosting provider GitMesh can reason about.
pub trait Provider {
    /// Stable provider id (`github`, ...).
    fn id(&self) -> &'static str;
    /// Human-readable name.
    fn display_name(&self) -> &'static str;
    /// The web base URL, used to render clickable links.
    fn web_base_url(&self) -> &'static str;
    /// True when this provider owns the given remote URL.
    fn matches(&self, remote_url: &str) -> bool;
    /// Parse a remote URL into the coordinates GitMesh needs.
    fn parse_remote(&self, remote_url: &str) -> Option<RemoteRef>;
}

/// Parsed coordinates of a hosted remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    /// Provider id.
    pub provider: &'static str,
    /// Owner or organisation.
    pub owner: String,
    /// Repository name without the `.git` suffix.
    pub name: String,
    /// Remote URL as configured.
    pub url: String,
}

impl RemoteRef {
    /// `owner/name`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Web URL of the repository.
    pub fn web_url(&self) -> String {
        format!(
            "{}/{}/{}",
            provider_web_base(self.provider),
            self.owner,
            self.name
        )
    }

    /// Default HTTPS clone URL for this repository.
    pub fn https_url(&self) -> String {
        format!(
            "{base}/{owner}/{name}.git",
            base = provider_https_base(self.provider),
            owner = self.owner,
            name = self.name
        )
    }
}

fn provider_web_base(provider: &str) -> &'static str {
    match provider {
        "github" => "https://github.com",
        _ => "https://example.invalid",
    }
}

fn provider_https_base(provider: &str) -> &'static str {
    match provider {
        "github" => "https://github.com",
        _ => "https://example.invalid",
    }
}

/// All providers this build knows about.
pub fn known_providers() -> Vec<&'static dyn Provider> {
    vec![&github::GitHubProvider]
}

/// Identify the provider of a remote URL, if any.
pub fn provider_for_remote(remote_url: &str) -> Option<&'static dyn Provider> {
    known_providers()
        .into_iter()
        .find(|provider| provider.matches(remote_url))
}

/// Parse a remote URL into provider coordinates.
pub fn parse_remote(remote_url: &str) -> Option<RemoteRef> {
    provider_for_remote(remote_url).and_then(|provider| provider.parse_remote(remote_url))
}

/// Describe what kind of remote a URL is, without a provider.
pub fn remote_kind(remote_url: &str) -> RemoteKind {
    classify_remote(remote_url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::RemoteKind;

    #[test]
    fn recognises_github_remotes() {
        let remote = parse_remote("git@github.com:acme/engine.git").unwrap();
        assert_eq!(remote.provider, "github");
        assert_eq!(remote.owner, "acme");
        assert_eq!(remote.name, "engine");
        assert_eq!(remote.full_name(), "acme/engine");
        assert_eq!(remote.web_url(), "https://github.com/acme/engine");
        assert_eq!(remote.https_url(), "https://github.com/acme/engine.git");
    }

    #[test]
    fn non_github_remotes_have_no_provider() {
        assert!(parse_remote("git@gitlab.com:acme/engine.git").is_none());
        assert!(parse_remote("/srv/git/engine.git").is_none());
        assert_eq!(
            remote_kind("git@gitlab.com:acme/engine.git"),
            RemoteKind::Ssh
        );
    }

    #[test]
    fn provider_lookup_is_consistent() {
        let provider = provider_for_remote("https://github.com/a/b").unwrap();
        assert_eq!(provider.id(), "github");
        assert!(provider.matches("ssh://git@github.com/a/b.git"));
        assert!(!provider.matches("https://example.com/a/b"));
    }
}
