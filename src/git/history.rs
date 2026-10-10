//! How a local repository relates to a remote, decided before anything is connected, adopted
//! or pushed.
//!
//! This is the single place that answers "may GitMesh connect these histories?". Setup
//! (`setup.rs`), repository management (`manage.rs`) and the push preflight (`ops/push.rs`)
//! all use it, so the command line, the terminal interface and the browser interface apply
//! the same rules.
//!
//! The rules, in short:
//!
//! * A repository with **no commits** that is connected to a remote with history adopts that
//!   history (see [`discovery::adopt_remote_history`]). Local files are kept, and Git refuses
//!   a checkout that would overwrite one.
//! * A repository **with commits** is never re-rooted. If its history shares no commit with
//!   the remote, the configuration is refused (or, for a configuration that is only kept,
//!   reported) and nothing is pushed.
//! * Before a push, the remote is fetched and the destination is checked for unrelated
//!   histories, a diverged or advanced remote branch, and a branch-name mismatch.
//!
//! Only reads happen here, apart from fetching objects and remote-tracking refs. Nothing is
//! merged, reset, rebased or force-pushed, and no branch or working-tree file is changed.

use std::path::Path;

use crate::error::{Error, Result};

use super::command::{GitRepo, GitRunner};
use super::remote::{
    condense_git_error, divergence, probe_remote, transport_hint, Divergence, RemoteProbe,
};

/// What the remote's history is, compared with a local repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryCheck {
    /// The remote has no branches, so there is no history to adopt or to compare with.
    RemoteEmpty,
    /// The remote could not be read, so its history is unknown.
    Unknown {
        /// What Git reported, condensed.
        reason: String,
        /// What the user can do about it.
        hint: String,
    },
    /// The repository has no commits and the remote has history: adopt `branch`.
    Adopt {
        /// The remote branch whose history is checked out as the local branch.
        branch: String,
    },
    /// Both sides have history and they share it. `note` says what needs attention.
    Related {
        /// `None` when the two sides agree (or the local side is simply ahead).
        note: Option<String>,
    },
    /// The local commits and the remote history share no commit.
    Unrelated {
        /// The local branch whose history is unrelated.
        branch: String,
        /// Branches the remote has.
        remote_branches: Vec<String>,
    },
    /// The remote has history, but no branch can be adopted (the wanted one is missing, or
    /// there is no default and several branches).
    NoBranchToAdopt {
        /// Branches the remote has.
        remote_branches: Vec<String>,
        /// The branch the project asked for, when there was one.
        wanted: Option<String>,
    },
}

/// Pick the remote branch to adopt.
///
/// A wanted branch is used only when the remote has it: a branch hint that the remote does not
/// have is not silently replaced by another one. Without a hint the remote's default branch is
/// used, then `main`, then the only branch when there is exactly one.
pub fn adoption_branch(
    default_branch: Option<&str>,
    branches: &[String],
    wanted: Option<&str>,
) -> Option<String> {
    if let Some(wanted) = wanted {
        return branches
            .iter()
            .any(|b| b == wanted)
            .then(|| wanted.to_string());
    }
    if let Some(default) = default_branch {
        if branches.iter().any(|b| b == default) {
            return Some(default.to_string());
        }
    }
    if branches.iter().any(|b| b == "main") {
        return Some("main".to_string());
    }
    match branches {
        [only] => Some(only.clone()),
        _ => None,
    }
}

/// Compare a repository with the remote at `url`.
///
/// `repo_dir` is the repository directory; it is only read when `has_commits` is true (the
/// comparison then fetches the remote's objects, without writing any ref or `FETCH_HEAD`).
/// `wanted_branch` is the branch the project records for the repository, when there is one.
pub fn check_remote_history(
    runner: &GitRunner,
    repo_dir: Option<&Path>,
    has_commits: bool,
    url: &str,
    wanted_branch: Option<&str>,
) -> HistoryCheck {
    let (default_branch, branches) = match probe_remote(runner, url) {
        RemoteProbe::Unreachable { reason, hint } => return HistoryCheck::Unknown { reason, hint },
        RemoteProbe::Reachable {
            default_branch,
            branches,
        } => (default_branch, branches),
    };
    if branches.is_empty() {
        return HistoryCheck::RemoteEmpty;
    }

    let repo_dir = match (has_commits, repo_dir) {
        (true, Some(dir)) => dir,
        _ => {
            return match adoption_branch(default_branch.as_deref(), &branches, wanted_branch) {
                Some(branch) => HistoryCheck::Adopt { branch },
                None => HistoryCheck::NoBranchToAdopt {
                    remote_branches: branches,
                    wanted: wanted_branch.map(str::to_string),
                },
            };
        }
    };

    let repo = runner.repo(repo_dir);
    let local_branch = wanted_branch.map(str::to_string).or_else(|| {
        repo.head()
            .ok()
            .and_then(|head| head.branch().map(str::to_string))
    });
    let heads = match ls_remote_heads(runner, url) {
        Ok(heads) => heads,
        Err(err) => {
            return HistoryCheck::Unknown {
                reason: err.to_string(),
                hint: "the remote's branches could not be listed".to_string(),
            }
        }
    };
    if let Err(reason) = fetch_objects(&repo, url, &heads) {
        return HistoryCheck::Unknown {
            reason,
            hint: "the remote's history could not be fetched for comparison".to_string(),
        };
    }

    if let Some(local) = local_branch.clone() {
        if let Some((_, oid)) = heads.iter().find(|(name, _)| *name == local) {
            return match divergence(&repo, oid) {
                Ok(Some(shape)) => classify(&shape, &local, url),
                _ => HistoryCheck::Unknown {
                    reason: format!("could not compare '{local}' with the remote"),
                    hint: "the remote's objects could not be read".to_string(),
                },
            };
        }
    }

    // The local branch is not on the remote. It is only safe to push as a new branch when it
    // builds on some branch the remote already has; otherwise the push would create a second,
    // unconnected history.
    let shares_any = heads
        .iter()
        .any(|(_, oid)| matches!(divergence(&repo, oid), Ok(Some(shape)) if shape.shares_history));
    if shares_any {
        HistoryCheck::Related {
            note: Some(format!(
                "the remote has no branch '{}' yet; the first push creates it on top of its history",
                local_branch.unwrap_or_else(|| "(detached)".to_string())
            )),
        }
    } else {
        HistoryCheck::Unrelated {
            branch: local_branch.unwrap_or_else(|| "(detached)".to_string()),
            remote_branches: branches,
        }
    }
}

fn classify(shape: &Divergence, local: &str, url: &str) -> HistoryCheck {
    if !shape.shares_history {
        return HistoryCheck::Unrelated {
            branch: local.to_string(),
            remote_branches: Vec::new(),
        };
    }
    let note = if shape.is_diverged() {
        Some(format!(
            "'{local}' has diverged from {url} ({} local, {} remote commit(s)); pull with a merge \
             or a rebase before pushing",
            shape.ahead, shape.behind
        ))
    } else if shape.behind > 0 {
        Some(format!(
            "{url} has {} commit(s) that this repository does not have; pull them before pushing",
            shape.behind
        ))
    } else {
        None
    };
    HistoryCheck::Related { note }
}

/// Fetch the named branches of `url` into the object store only: no ref and no `FETCH_HEAD`
/// changes. Git accepts explicit branch names here, but not a glob without a destination.
fn fetch_objects(
    repo: &GitRepo<'_>,
    url: &str,
    heads: &[(String, String)],
) -> std::result::Result<(), String> {
    if heads.is_empty() {
        return Ok(());
    }
    let mut args: Vec<String> = vec![
        "fetch".into(),
        "--quiet".into(),
        "--no-write-fetch-head".into(),
        "--".into(),
        url.into(),
    ];
    args.extend(heads.iter().map(|(name, _)| format!("refs/heads/{name}")));
    let out = repo.run(&args).map_err(|err| err.to_string())?;
    if out.success() {
        Ok(())
    } else {
        Err(condense_git_error(&out.stderr))
    }
}

/// `(branch name, commit id)` for every branch of the remote.
fn ls_remote_heads(runner: &GitRunner, url: &str) -> Result<Vec<(String, String)>> {
    let out = runner.run(&["ls-remote", "--heads", "--", url])?;
    if !out.success() {
        return Err(Error::Other(condense_git_error(&out.stderr)));
    }
    Ok(out
        .stdout
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            let branch = name.strip_prefix("refs/heads/")?;
            Some((branch.to_string(), oid.to_string()))
        })
        .collect())
}

// ------------------------------------------------------------ plan findings --

/// The plan-level messages for one [`HistoryCheck`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryFindings {
    /// Reasons the plan must not run.
    pub blockers: Vec<String>,
    /// Things to check; none of them stops the plan.
    pub warnings: Vec<String>,
    /// Neutral facts.
    pub notices: Vec<String>,
    /// The branch the plan adopts, when it adopts remote history.
    pub adopt_branch: Option<String>,
}

/// Turn a check into plan messages.
///
/// `label` names the repository for the reader (`engine`, or `the project root`). When
/// `newly_configured` is true, the remote is being added or replaced by this plan, so a
/// conflict blocks it. When the remote was already configured and is only kept, the same
/// conflict is reported as a warning: an existing repository is never made unusable by
/// GitMesh, and the push preflight still refuses to push across it.
pub fn findings(
    check: &HistoryCheck,
    label: &str,
    url: &str,
    newly_configured: bool,
) -> HistoryFindings {
    let mut out = HistoryFindings::default();
    let conflict = |out: &mut HistoryFindings, text: String| {
        if newly_configured {
            out.blockers.push(text);
        } else {
            out.warnings.push(text);
        }
    };
    match check {
        HistoryCheck::RemoteEmpty => {}
        HistoryCheck::Unknown { reason, hint } => out.warnings.push(format!(
            "{label}: {url} could not be read to compare histories ({reason}). {hint}. Check it \
             before the first push"
        )),
        HistoryCheck::Adopt { branch } => {
            out.adopt_branch = Some(branch.clone());
            out.notices.push(format!(
                "{label} has no commits and {url} already has history: GitMesh checks out \
                 {url}'s '{branch}' branch instead of starting a new history. Local files are kept"
            ));
        }
        HistoryCheck::Related { note } => {
            if let Some(note) = note {
                out.warnings.push(format!("{label}: {note}"));
            }
        }
        HistoryCheck::Unrelated {
            branch,
            remote_branches,
        } => conflict(
            &mut out,
            format!(
                "{label} has commits of its own on '{branch}', and {url} has history that shares \
                 no commit with them{}. GitMesh will not connect two unrelated histories and will \
                 not push across them. Keep the remote's history (clone it into a new directory \
                 with `gitmesh configure clone`, then bring your files over), or keep this \
                 history and point the repository at a remote that is empty (`gitmesh configure \
                 remote`). Nothing has been changed",
                branches_clause(remote_branches)
            ),
        ),
        HistoryCheck::NoBranchToAdopt {
            remote_branches,
            wanted,
        } => {
            let text = match wanted {
                Some(wanted) => format!(
                    "{label}: {url} has no branch '{wanted}' to adopt{}. Set the repository's \
                     branch to one that exists on the remote",
                    branches_clause(remote_branches)
                ),
                None => format!(
                    "{label}: {url} has history on several branches{} and no default branch to \
                     adopt. Set the repository's branch to the one to adopt",
                    branches_clause(remote_branches)
                ),
            };
            conflict(&mut out, text);
        }
    }
    out
}

fn branches_clause(branches: &[String]) -> String {
    if branches.is_empty() {
        String::new()
    } else {
        format!(" (branches: {})", branches.join(", "))
    }
}

// --------------------------------------------------------------- push check --

/// The outcome of the push preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushVerdict {
    /// The push may run.
    Proceed,
    /// The push must not run. `summary` is the one-line reason, `guidance` the next steps.
    Refuse {
        /// One-line reason.
        summary: String,
        /// What to do next, and what has (not) been changed.
        guidance: Vec<String>,
    },
}

/// Check a push before it runs.
///
/// The remote is fetched first (remote-tracking refs only; the branch and the work tree are not
/// touched). Then the destination is compared with the local branch:
///
/// * unrelated histories are refused;
/// * a diverged branch, or a remote branch that has commits this branch does not, is refused
///   with the way to integrate them;
/// * a branch the remote does not have is pushed only when it builds on some branch the remote
///   already has, or when the remote is empty. Otherwise the push would create a second,
///   unconnected history under a name the remote may not expect.
///
/// `upstream` is the configured upstream (`origin/main`), when there is one.
pub fn push_preflight(
    git: &GitRepo<'_>,
    remote: &str,
    branch: &str,
    upstream: Option<&str>,
) -> PushVerdict {
    let fetched = match git.run(&["fetch", "--quiet", remote]) {
        Ok(out) if out.success() => None,
        Ok(out) => Some(out.stderr),
        Err(err) => Some(err.to_string()),
    };
    if let Some(stderr) = fetched {
        return PushVerdict::Refuse {
            summary: format!("cannot read '{remote}' before pushing"),
            guidance: vec![
                condense_git_error(&stderr),
                transport_hint(&stderr),
                "nothing was pushed and nothing local was changed".to_string(),
            ],
        };
    }

    let remote_ref = upstream
        .map(str::to_string)
        .unwrap_or_else(|| format!("{remote}/{branch}"));
    match divergence(git, &remote_ref) {
        Ok(Some(shape)) => return judge_existing(&shape, branch, &remote_ref),
        Ok(None) => {}
        Err(err) => {
            return PushVerdict::Refuse {
                summary: format!("cannot compare '{branch}' with {remote_ref}"),
                guidance: vec![err.to_string(), "nothing was pushed".to_string()],
            }
        }
    }

    // The remote has no such branch. Compare with its other branches.
    let others = remote_tracking_refs(git, remote);
    if others.is_empty() {
        return PushVerdict::Proceed;
    }
    let shares_any = others.iter().any(
        |reference| matches!(divergence(git, reference), Ok(Some(shape)) if shape.shares_history),
    );
    if shares_any {
        return PushVerdict::Proceed;
    }
    PushVerdict::Refuse {
        summary: format!(
            "'{branch}' has no commits in common with the branches of '{remote}' ({})",
            others
                .iter()
                .map(|r| r
                    .trim_start_matches(&format!("refs/remotes/{remote}/"))
                    .to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        guidance: vec![
            format!(
                "pushing '{branch}' would create a second history on the remote that is \
                 unrelated to its existing branches (a branch-name mismatch is the usual cause)"
            ),
            format!(
                "if '{branch}' should be the remote's default branch, check out the right branch \
                 or set the repository's branch; to create a new branch on purpose, push it \
                 yourself with `git push -u {remote} {branch}`"
            ),
            "GitMesh never force-pushes; nothing was pushed and nothing local was changed"
                .to_string(),
        ],
    }
}

fn judge_existing(shape: &Divergence, branch: &str, remote_ref: &str) -> PushVerdict {
    if !shape.shares_history {
        return PushVerdict::Refuse {
            summary: format!(
                "rejected: '{branch}' and {remote_ref} have unrelated histories (no common commit)"
            ),
            guidance: vec![
                "pushing would create a second, unconnected history on the remote; neither a pull \
                 nor a merge can join them automatically"
                    .to_string(),
                "decide which history to keep: clone the remote into a new directory with \
                 `gitmesh configure clone` and bring your files over, or point this repository at \
                 an empty remote with `gitmesh configure remote`"
                    .to_string(),
                "GitMesh never force-pushes; nothing was pushed and nothing local was changed"
                    .to_string(),
            ],
        };
    }
    if shape.is_diverged() {
        return PushVerdict::Refuse {
            summary: format!(
                "rejected: '{branch}' has diverged from {remote_ref} ({} local, {} remote commit(s))",
                shape.ahead, shape.behind
            ),
            guidance: vec![
                "run `gitmesh pull --strategy merge` (or `--strategy rebase`) to integrate the \
                 remote commits, then push again"
                    .to_string(),
                "GitMesh never force-pushes; nothing was pushed and nothing local was changed"
                    .to_string(),
            ],
        };
    }
    if shape.behind > 0 {
        return PushVerdict::Refuse {
            summary: format!(
                "rejected: {remote_ref} has {} commit(s) this repository does not have",
                shape.behind
            ),
            guidance: vec![
                "run `gitmesh pull`, then push again".to_string(),
                "nothing was pushed and nothing local was changed".to_string(),
            ],
        };
    }
    PushVerdict::Proceed
}

/// Remote-tracking refs of `remote`, excluding its `HEAD`.
fn remote_tracking_refs(git: &GitRepo<'_>, remote: &str) -> Vec<String> {
    let prefix = format!("refs/remotes/{remote}/");
    git.run(&["for-each-ref", "--format=%(refname)", &prefix])
        .ok()
        .filter(|out| out.success())
        .map(|out| {
            out.stdout
                .lines()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty() && !line.ends_with("/HEAD"))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_wanted_branch_is_used_only_when_the_remote_has_it() {
        let branches = names(&["main", "develop"]);
        assert_eq!(
            adoption_branch(Some("main"), &branches, Some("develop")).as_deref(),
            Some("develop")
        );
        // A hint the remote does not have is never silently replaced.
        assert_eq!(
            adoption_branch(Some("main"), &branches, Some("trunk")),
            None
        );
    }

    #[test]
    fn without_a_hint_the_default_branch_wins_then_main_then_the_only_branch() {
        assert_eq!(
            adoption_branch(Some("master"), &names(&["main", "master"]), None).as_deref(),
            Some("master")
        );
        assert_eq!(
            adoption_branch(None, &names(&["dev", "main"]), None).as_deref(),
            Some("main")
        );
        assert_eq!(
            adoption_branch(None, &names(&["trunk"]), None).as_deref(),
            Some("trunk")
        );
        // Several branches and no way to choose: the caller must ask.
        assert_eq!(adoption_branch(None, &names(&["a", "b"]), None), None);
    }

    #[test]
    fn a_new_configuration_blocks_an_unrelated_history_but_a_kept_one_only_warns() {
        let check = HistoryCheck::Unrelated {
            branch: "main".into(),
            remote_branches: names(&["main"]),
        };
        let blocked = findings(&check, "engine", "/r.git", true);
        assert_eq!(blocked.blockers.len(), 1, "{blocked:?}");
        assert!(
            blocked.blockers[0].contains("shares no commit"),
            "{blocked:?}"
        );

        let kept = findings(&check, "engine", "/r.git", false);
        assert!(kept.blockers.is_empty(), "{kept:?}");
        assert_eq!(kept.warnings.len(), 1, "{kept:?}");
    }

    #[test]
    fn an_empty_remote_and_a_related_remote_produce_no_blocker() {
        assert_eq!(
            findings(&HistoryCheck::RemoteEmpty, "engine", "/r.git", true),
            HistoryFindings::default()
        );
        let related = findings(
            &HistoryCheck::Related { note: None },
            "engine",
            "/r.git",
            true,
        );
        assert_eq!(related, HistoryFindings::default());
    }

    #[test]
    fn an_adoption_is_a_notice_and_names_the_branch() {
        let adopt = findings(
            &HistoryCheck::Adopt {
                branch: "master".into(),
            },
            "engine",
            "/r.git",
            true,
        );
        assert_eq!(adopt.adopt_branch.as_deref(), Some("master"));
        assert!(adopt.blockers.is_empty());
        assert!(adopt.notices[0].contains("'master'"), "{adopt:?}");
    }

    #[test]
    fn divergence_judgement_matches_the_push_refusals() {
        let unrelated = Divergence {
            ahead: 1,
            behind: 1,
            shares_history: false,
        };
        assert!(matches!(
            judge_existing(&unrelated, "main", "origin/main"),
            PushVerdict::Refuse { summary, .. } if summary.contains("unrelated histories")
        ));
        let diverged = Divergence {
            ahead: 1,
            behind: 2,
            shares_history: true,
        };
        assert!(matches!(
            judge_existing(&diverged, "main", "origin/main"),
            PushVerdict::Refuse { summary, .. } if summary.contains("diverged")
        ));
        let behind = Divergence {
            ahead: 0,
            behind: 1,
            shares_history: true,
        };
        assert!(matches!(
            judge_existing(&behind, "main", "origin/main"),
            PushVerdict::Refuse { summary, .. } if summary.contains("commit(s) this repository does not have")
        ));
        let ahead = Divergence {
            ahead: 2,
            behind: 0,
            shares_history: true,
        };
        assert_eq!(
            judge_existing(&ahead, "main", "origin/main"),
            PushVerdict::Proceed
        );
    }
}
