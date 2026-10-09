//! Remote-facing Git knowledge, read from Git's own machine-readable output.
//!
//! This module answers questions the orchestration layers need before they decide what to
//! do: is a remote reachable, does it have commits, which branch is its default, why was a
//! push refused, and how does a local branch relate to a remote-tracking branch. It never
//! changes a repository, and it never guesses from human-readable text when Git offers a
//! structured form (`ls-remote --symref`, `push --porcelain`, `rev-list --left-right`).

use crate::error::Result;

use super::command::{GitRepo, GitRunner};

// ------------------------------------------------------------------ probing --

/// What a remote says about itself, read without changing anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteProbe {
    /// The remote answered.
    Reachable {
        /// The branch `HEAD` points at on the remote, when it has one.
        default_branch: Option<String>,
        /// Every branch the remote has. Empty means the remote has no commits.
        branches: Vec<String>,
    },
    /// The remote could not be read.
    Unreachable {
        /// Git's message, condensed to the part that explains the failure.
        reason: String,
        /// What the user can do about it.
        hint: String,
    },
}

impl RemoteProbe {
    /// True when the remote answered at all.
    pub fn is_reachable(&self) -> bool {
        matches!(self, RemoteProbe::Reachable { .. })
    }

    /// True when the remote answered and has no branch, so it has no commits.
    pub fn is_empty(&self) -> bool {
        matches!(self, RemoteProbe::Reachable { branches, .. } if branches.is_empty())
    }

    /// Branch names the remote has (empty when unreachable or empty).
    pub fn branches(&self) -> &[String] {
        match self {
            RemoteProbe::Reachable { branches, .. } => branches,
            RemoteProbe::Unreachable { .. } => &[],
        }
    }

    /// The remote's default branch, when it answered with one.
    pub fn default_branch(&self) -> Option<&str> {
        match self {
            RemoteProbe::Reachable { default_branch, .. } => default_branch.as_deref(),
            RemoteProbe::Unreachable { .. } => None,
        }
    }
}

/// Ask a remote what it has, without fetching and without touching any repository.
///
/// `url` may be any Git remote: a local path, a `file://` URL, or a network URL. Git's
/// prompts are disabled by the runner, so credentials that are missing make the probe fail
/// with a message instead of waiting for input. The HTTP low-speed limits bound a stalled
/// transfer; SSH host-key prompts are a known exception (see `docs/REPOSITORIES.md`).
pub fn probe_remote(runner: &GitRunner, url: &str) -> RemoteProbe {
    // A URL that starts with `-` would be read as an option. Refuse it before Git sees it.
    if url.starts_with('-') {
        return RemoteProbe::Unreachable {
            reason: format!("'{url}' is not a remote URL"),
            hint: "give the URL of the remote repository, for example a path or https://…"
                .to_string(),
        };
    }
    let out = match runner.run(&[
        "-c",
        "http.lowSpeedLimit=1000",
        "-c",
        "http.lowSpeedTime=20",
        "ls-remote",
        "--symref",
        "--",
        url,
        "HEAD",
        "refs/heads/*",
    ]) {
        Ok(out) => out,
        Err(err) => {
            return RemoteProbe::Unreachable {
                reason: err.to_string(),
                hint: "git could not be run".to_string(),
            }
        }
    };
    if !out.success() {
        return RemoteProbe::Unreachable {
            reason: condense_git_error(&out.stderr),
            hint: transport_hint(&out.stderr),
        };
    }
    let (default_branch, branches) = parse_ls_remote(&out.stdout);
    RemoteProbe::Reachable {
        default_branch,
        branches,
    }
}

/// Parse `git ls-remote --symref` output into the default branch and the branch names.
///
/// `ref: refs/heads/main<TAB>HEAD` names the default branch; `<sha><TAB>refs/heads/<name>`
/// lists a branch. Anything else is ignored.
pub fn parse_ls_remote(output: &str) -> (Option<String>, Vec<String>) {
    let mut default_branch = None;
    let mut branches = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            let target = rest.split('\t').next().unwrap_or("");
            if let Some(name) = target.strip_prefix("refs/heads/") {
                default_branch = Some(name.to_string());
            }
        } else if let Some((_, name)) = line.split_once('\t') {
            if let Some(branch) = name.strip_prefix("refs/heads/") {
                if !branch.is_empty() && !branches.iter().any(|b| b == branch) {
                    branches.push(branch.to_string());
                }
            }
        }
    }
    (default_branch, branches)
}

/// The part of Git's error output that explains a failure, without the repetition.
pub fn condense_git_error(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    lines
        .iter()
        .find(|line| line.starts_with("fatal:") || line.starts_with("error:"))
        .or_else(|| lines.first())
        .map(|line| {
            line.trim_start_matches("fatal:")
                .trim_start_matches("error:")
                .trim()
                .to_string()
        })
        .unwrap_or_else(|| "git reported an error without a message".to_string())
}

/// Turn Git's transport errors into the action the user can take.
///
/// Order matters: a local path that is not a repository also makes Git say "could not read
/// from remote repository", so the "not found" case is checked before the authentication
/// case.
pub fn transport_hint(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("could not resolve host")
        || lower.contains("network is unreachable")
        || lower.contains("unable to access")
        || lower.contains("connection timed out")
    {
        "the remote host could not be reached: check the network, or the URL".to_string()
    } else if lower.contains("does not appear to be a git repository")
        || lower.contains("repository not found")
        || lower.contains("does not exist")
    {
        "the remote repository could not be found: check the URL and that the repository exists"
            .to_string()
    } else if lower.contains("authentication failed")
        || lower.contains("permission denied")
        || lower.contains("could not read username")
        || lower.contains("terminal prompts disabled")
    {
        "access was refused: check your credentials for this remote".to_string()
    } else {
        "run the same command in a terminal to see the full Git output".to_string()
    }
}

// --------------------------------------------------------------- push refusal --

/// Why the remote refused a push, as Git reports it in `--porcelain` form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushRefusal {
    /// Git refused because the two histories are not in a fast-forward relation: the remote
    /// has commits this repository does not have (`fetch first`), or the update would not
    /// be a fast-forward (`non-fast-forward`). Whether they are divergent or unrelated is
    /// decided by [`divergence`], not by the message.
    HistoryMismatch {
        /// Destination ref, short name (`main`).
        branch: String,
    },
    /// The remote refused the update itself: a hook, a protected branch, a policy.
    RemoteRefused {
        /// Destination ref, short name.
        branch: String,
        /// The reason the remote gave.
        reason: String,
    },
    /// Any other rejection, with Git's reason.
    Other {
        /// Destination ref, short name.
        branch: String,
        /// The reason Git gave.
        reason: String,
    },
}

/// Parse the refused lines of `git push --porcelain`.
///
/// Porcelain lines are `<flag><TAB><from>:<to><TAB><summary>`. A refused ref has the flag
/// `!` and a summary such as `[rejected] (fetch first)` or
/// `[remote rejected] (pre-receive hook declined)`. Accepted refs are ignored.
pub fn parse_push_porcelain(stdout: &str) -> Vec<PushRefusal> {
    let mut refusals = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.split('\t');
        let (Some(flag), Some(refs), Some(summary)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if flag != "!" {
            continue;
        }
        let destination = refs.rsplit(':').next().unwrap_or(refs);
        let branch = destination
            .strip_prefix("refs/heads/")
            .unwrap_or(destination)
            .to_string();
        let reason = parenthesised(summary).unwrap_or_else(|| summary.trim().to_string());
        let refusal = if summary.contains("[remote rejected]") {
            PushRefusal::RemoteRefused { branch, reason }
        } else if reason == "fetch first" || reason == "non-fast-forward" {
            PushRefusal::HistoryMismatch { branch }
        } else {
            PushRefusal::Other { branch, reason }
        };
        refusals.push(refusal);
    }
    refusals
}

/// The text inside the last pair of parentheses, e.g. `fetch first` in
/// `[rejected] (fetch first)`.
fn parenthesised(summary: &str) -> Option<String> {
    let open = summary.rfind('(')?;
    let close = summary.rfind(')')?;
    (close > open).then(|| summary[open + 1..close].trim().to_string())
}

// ----------------------------------------------------------- history relation --

/// How a local branch relates to another commit (usually a remote-tracking branch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    /// Commits the local side has and the other side does not.
    pub ahead: u32,
    /// Commits the other side has and the local side does not.
    pub behind: u32,
    /// True when the two sides have a common ancestor. False means unrelated histories.
    pub shares_history: bool,
}

impl Divergence {
    /// True when both sides have commits the other does not.
    pub fn is_diverged(&self) -> bool {
        self.ahead > 0 && self.behind > 0
    }
}

/// Compare `HEAD` with `target` (for example `origin/main`). `None` when `target` does not
/// exist, which is how a branch the remote has deleted, or never had, shows up.
pub fn divergence(git: &GitRepo<'_>, target: &str) -> Result<Option<Divergence>> {
    if !ref_exists(git, target)? {
        return Ok(None);
    }
    let counts = git.run(&[
        "rev-list",
        "--left-right",
        "--count",
        &format!("HEAD...{target}"),
    ])?;
    if !counts.success() {
        return Ok(None);
    }
    let mut parts = counts.stdout.split_whitespace();
    let (Some(ahead), Some(behind)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let shares_history = git
        .run(&["merge-base", "HEAD", target])
        .map(|out| out.success())
        .unwrap_or(false);
    Ok(Some(Divergence {
        ahead: ahead.parse().unwrap_or(0),
        behind: behind.parse().unwrap_or(0),
        shares_history,
    }))
}

/// True when `name` resolves to a commit (a branch, a remote-tracking branch, a tag).
pub fn ref_exists(git: &GitRepo<'_>, name: &str) -> Result<bool> {
    Ok(git
        .run_optional(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ])?
        .is_some())
}

/// True when the configured upstream of a branch no longer exists locally as a
/// remote-tracking branch. After a fetch with prune this means the remote deleted it.
pub fn upstream_is_gone(git: &GitRepo<'_>, upstream: &str) -> Result<bool> {
    Ok(!ref_exists(git, &format!("refs/remotes/{upstream}"))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_remote_of_an_empty_remote_has_no_branches_and_no_default() {
        assert_eq!(parse_ls_remote(""), (None, Vec::new()));
    }

    #[test]
    fn ls_remote_reads_the_default_and_every_branch_once() {
        let out = "ref: refs/heads/trunk\tHEAD\n\
                   aaaa\tHEAD\n\
                   aaaa\trefs/heads/trunk\n\
                   bbbb\trefs/heads/feature/x\n\
                   bbbb\trefs/heads/feature/x\n\
                   cccc\trefs/tags/v1\n";
        let (default, branches) = parse_ls_remote(out);
        assert_eq!(default.as_deref(), Some("trunk"));
        assert_eq!(branches, vec!["trunk".to_string(), "feature/x".to_string()]);
    }

    #[test]
    fn porcelain_distinguishes_history_mismatch_from_a_remote_refusal() {
        let out = "To /r.git\n\
                   !\trefs/heads/main:refs/heads/main\t[rejected] (fetch first)\n\
                   !\trefs/heads/dev:refs/heads/dev\t[remote rejected] (pre-receive hook declined)\n\
                   !\trefs/heads/x:refs/heads/x\t[rejected] (non-fast-forward)\n\
                   *\trefs/heads/new:refs/heads/new\t[new branch]\n\
                   Done\n";
        let refusals = parse_push_porcelain(out);
        assert_eq!(
            refusals,
            vec![
                PushRefusal::HistoryMismatch {
                    branch: "main".into()
                },
                PushRefusal::RemoteRefused {
                    branch: "dev".into(),
                    reason: "pre-receive hook declined".into()
                },
                PushRefusal::HistoryMismatch { branch: "x".into() },
            ]
        );
    }

    #[test]
    fn porcelain_keeps_unknown_rejections_in_git_words() {
        let out = "!\trefs/heads/main:refs/heads/main\t[rejected] (already exists)\n";
        assert_eq!(
            parse_push_porcelain(out),
            vec![PushRefusal::Other {
                branch: "main".into(),
                reason: "already exists".into()
            }]
        );
    }

    #[test]
    fn condensed_errors_keep_the_fatal_line() {
        let stderr = "warning: something\nfatal: '/x' does not appear to be a git repository\n\
                      fatal: Could not read from remote repository.\n";
        assert_eq!(
            condense_git_error(stderr),
            "'/x' does not appear to be a git repository"
        );
    }

    #[test]
    fn transport_hints_name_the_action() {
        assert!(
            transport_hint("fatal: could not resolve host: example.invalid")
                .contains("could not be reached")
        );
        assert!(
            transport_hint("fatal: 'x' does not appear to be a git repository")
                .contains("could not be found")
        );
        assert!(
            transport_hint("fatal: Authentication failed for 'https://x'").contains("credentials")
        );
    }

    #[test]
    fn a_remote_starting_with_a_dash_is_never_passed_to_git() {
        let runner = GitRunner::default();
        let probe = probe_remote(&runner, "--upload-pack=touch /tmp/pwned");
        assert!(!probe.is_reachable());
    }
}
