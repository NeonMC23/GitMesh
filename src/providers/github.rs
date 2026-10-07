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

/// How GitMesh would reach a hosted repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteScheme {
    /// `git@github.com:owner/name.git`
    Ssh,
    /// `https://github.com/owner/name.git`
    Https,
}

impl RemoteScheme {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            RemoteScheme::Ssh => "ssh",
            RemoteScheme::Https => "https",
        }
    }

    /// Parse the label used by the interface and the command line.
    pub fn parse(value: &str) -> Option<RemoteScheme> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ssh" => Some(RemoteScheme::Ssh),
            "https" | "http" => Some(RemoteScheme::Https),
            _ => None,
        }
    }
}

/// Visibility of a repository on the host.
///
/// GitMesh never creates a repository, so this value is only used to phrase the command
/// the user runs themselves; it defaults to the safest option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Only the owner and collaborators see it.
    Private,
    /// Anyone can read it.
    Public,
    /// Visible to the organisation (the GitHub Enterprise/org option).
    Internal,
}

impl Visibility {
    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Public => "public",
            Visibility::Internal => "internal",
        }
    }

    /// Parse a label coming from a front end; unknown values fall back to private.
    pub fn parse(value: &str) -> Visibility {
        match value.trim().to_ascii_lowercase().as_str() {
            "public" => Visibility::Public,
            "internal" => Visibility::Internal,
            _ => Visibility::Private,
        }
    }

    /// The `gh` flag for this visibility.
    pub fn gh_flag(self) -> &'static str {
        match self {
            Visibility::Private => "--private",
            Visibility::Public => "--public",
            Visibility::Internal => "--internal",
        }
    }
}

/// What a front end needs to configure (and describe) a GitHub remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRemotePlan {
    /// Organisation or user.
    pub owner: String,
    /// Repository name.
    pub name: String,
    /// Remote URL GitMesh would record.
    pub url: String,
    /// The same repository over SSH.
    pub ssh_url: String,
    /// The same repository over HTTPS.
    pub https_url: String,
    /// Web URL for humans.
    pub web_url: String,
    /// Visibility the user selected (for the instructions only).
    pub visibility: Visibility,
}

impl GitHubRemotePlan {
    /// `owner/name`.
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The command that creates the repository on GitHub.
    ///
    /// GitMesh never runs it: it has no GitHub API client, no tokens and no business
    /// creating hosted repositories behind the user's back. The command is returned so
    /// the user can copy it, and the same text is shown in the interface.
    pub fn create_command(&self) -> String {
        format!(
            "gh repo create {}/{} {}",
            self.owner,
            self.name,
            self.visibility.gh_flag()
        )
    }

    /// The sentence a front end shows next to the remote.
    pub fn note(&self) -> String {
        format!(
            "GitMesh does not create repositories on GitHub. Create {}/{} ({}) there first \
             — for example `{}` — then push. No token is needed, and none is stored.",
            self.owner,
            self.name,
            self.visibility.label(),
            self.create_command()
        )
    }
}

/// Validate an owner/repository pair and describe the remote GitMesh would configure.
///
/// Rejects anything that could not be a GitHub path instead of guessing, so a typo
/// surfaces while the user is still looking at the field.
pub fn plan_remote(
    owner: &str,
    name: &str,
    scheme: RemoteScheme,
    visibility: Visibility,
) -> std::result::Result<GitHubRemotePlan, String> {
    let owner = owner.trim();
    let name = name.trim().trim_end_matches(".git");
    if owner.is_empty() {
        return Err("the GitHub organisation or user is required".to_string());
    }
    if name.is_empty() {
        return Err("the GitHub repository name is required".to_string());
    }
    for (label, value) in [("organisation", owner), ("repository name", name)] {
        if value.contains('/') || value.contains(char::is_whitespace) {
            return Err(format!(
                "the GitHub {label} '{value}' must not contain spaces or '/'"
            ));
        }
        if value.starts_with('.') || value.starts_with('-') {
            return Err(format!("the GitHub {label} '{value}' is not valid"));
        }
    }
    let repo = GitHubRepo {
        owner: owner.to_string(),
        name: name.to_string(),
        remote_url: String::new(),
        https: scheme == RemoteScheme::Https,
    };
    Ok(GitHubRemotePlan {
        owner: repo.owner.clone(),
        name: repo.name.clone(),
        url: match scheme {
            RemoteScheme::Ssh => repo.ssh_url(),
            RemoteScheme::Https => repo.https_url(),
        },
        ssh_url: repo.ssh_url(),
        https_url: repo.https_url(),
        web_url: repo.web_url(),
        visibility,
    })
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
    fn plans_a_github_remote_and_never_invents_credentials() {
        let plan = plan_remote(
            "acme",
            "myproject-engine.git",
            RemoteScheme::Ssh,
            Visibility::Private,
        )
        .unwrap();
        assert_eq!(plan.url, "git@github.com:acme/myproject-engine.git");
        assert_eq!(
            plan.https_url,
            "https://github.com/acme/myproject-engine.git"
        );
        assert_eq!(plan.web_url, "https://github.com/acme/myproject-engine");
        assert_eq!(plan.full_name(), "acme/myproject-engine");
        // The private visibility is the safest default and drives the instructions.
        assert_eq!(plan.visibility, Visibility::Private);
        assert_eq!(
            plan.create_command(),
            "gh repo create acme/myproject-engine --private"
        );
        assert!(plan
            .note()
            .contains("does not create repositories on GitHub"));
        assert!(plan
            .note()
            .contains("No token is needed, and none is stored."));
        assert!(!plan.note().contains("token="));
        assert_eq!(Visibility::parse("PUBLIC"), Visibility::Public);
        assert_eq!(Visibility::parse("nonsense"), Visibility::Private);
        assert_eq!(RemoteScheme::parse("HTTPS"), Some(RemoteScheme::Https));
        assert_eq!(RemoteScheme::parse("ftp"), None);
    }

    #[test]
    fn refuses_impossible_github_names() {
        assert!(plan_remote("", "engine", RemoteScheme::Ssh, Visibility::Private).is_err());
        assert!(plan_remote("acme", "  ", RemoteScheme::Ssh, Visibility::Private).is_err());
        assert!(plan_remote("a/b", "engine", RemoteScheme::Ssh, Visibility::Private).is_err());
        assert!(plan_remote("acme", "my engine", RemoteScheme::Ssh, Visibility::Private).is_err());
        assert!(plan_remote("acme", "-engine", RemoteScheme::Ssh, Visibility::Private).is_err());
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
