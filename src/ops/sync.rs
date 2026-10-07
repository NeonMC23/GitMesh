//! Unified fetch and pull.
//!
//! `gitmesh fetch` and `gitmesh pull` run Git's own fetch/pull in every physical
//! repository. GitMesh adds orchestration, not Git behaviour:
//!
//! * every repository is fetched/pulled independently and one failure never stops the
//!   others;
//! * local work is never discarded: the default pull strategy is `--ff-only`, so a
//!   repository that has diverged is reported instead of being merged or reset
//!   silently;
//! * conflicts are detected and reported with the files involved, and the conflicted
//!   state is left visible in the repository rather than hidden;
//! * repositories without a remote or without an upstream are skipped with an
//!   explanation, not treated as failures.

use crate::analyzer::Analyzer;
use crate::error::Result;
use crate::git::GitRepo;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryState};
use crate::ops::util::{self, RepositorySelection};
use crate::ops::{OperationReport, OutcomeKind, RepoOutcome};

/// How a logical pull should integrate upstream commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PullStrategy {
    /// `git pull --ff-only`: refuses to change anything when the branches diverged.
    #[default]
    FastForwardOnly,
    /// `git pull --no-rebase`: creates a merge commit (conflicts are reported).
    Merge,
    /// `git pull --rebase`: replays local commits on top of upstream.
    Rebase,
}

impl PullStrategy {
    fn label(self) -> &'static str {
        match self {
            PullStrategy::FastForwardOnly => "ff-only",
            PullStrategy::Merge => "merge",
            PullStrategy::Rebase => "rebase",
        }
    }

    fn pull_args(self) -> Vec<&'static str> {
        match self {
            PullStrategy::FastForwardOnly => vec!["pull", "--ff-only"],
            PullStrategy::Merge => vec!["pull", "--no-rebase", "--no-edit"],
            PullStrategy::Rebase => vec!["pull", "--rebase", "--no-edit"],
        }
    }
}

/// Options for fetch/pull.
#[derive(Debug, Clone, Default)]
pub struct SyncOptions {
    /// Which repositories to consider.
    pub selection: RepositorySelection,
    /// Pull strategy (ignored by fetch).
    pub strategy: PullStrategy,
    /// Report what would happen without contacting the remotes.
    pub dry_run: bool,
    /// Prune deleted branches while fetching.
    pub prune: bool,
}

impl SyncOptions {
    pub fn new() -> Self {
        SyncOptions {
            selection: RepositorySelection::All,
            strategy: PullStrategy::default(),
            dry_run: false,
            prune: true,
        }
    }
}

/// Fetch every selected repository.
pub fn fetch_project(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &SyncOptions,
) -> Result<OperationReport> {
    fetch_project_observed(
        project,
        runner,
        options,
        &mut crate::ops::OperationObserver::silent(),
    )
}

/// Same as [`fetch_project`], reporting each repository as the loop reaches it.
pub fn fetch_project_observed(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &SyncOptions,
    observer: &mut crate::ops::OperationObserver<'_>,
) -> Result<OperationReport> {
    options.selection.validate(project)?;
    let analyzer = Analyzer::new(project, runner);
    let outcomes = util::each_repository_observed(
        project,
        &analyzer,
        runner,
        &options.selection,
        observer,
        |repo, state, git| fetch_one(repo, state, git, options),
    );
    Ok(OperationReport::new("fetch", options.dry_run, outcomes))
}

/// Pull every selected repository.
pub fn pull_project(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &SyncOptions,
) -> Result<OperationReport> {
    pull_project_observed(
        project,
        runner,
        options,
        &mut crate::ops::OperationObserver::silent(),
    )
}

/// Same as [`pull_project`], reporting each repository as the loop reaches it.
pub fn pull_project_observed(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &SyncOptions,
    observer: &mut crate::ops::OperationObserver<'_>,
) -> Result<OperationReport> {
    options.selection.validate(project)?;
    let analyzer = Analyzer::new(project, runner);
    let outcomes = util::each_repository_observed(
        project,
        &analyzer,
        runner,
        &options.selection,
        observer,
        |repo, state, git| pull_one(repo, state, git, options),
    );
    Ok(OperationReport::new("pull", options.dry_run, outcomes))
}

fn fetch_one(
    repo: &PhysicalRepository,
    state: &RepositoryState,
    git: &GitRepo<'_>,
    options: &SyncOptions,
) -> RepoOutcome {
    let base = |kind: OutcomeKind, summary: String| {
        RepoOutcome::new(&repo.id, repo.role, repo.relative_slash(), kind, summary)
    };

    if state.remotes.is_empty() {
        return base(OutcomeKind::Skipped, "no remote configured".to_string())
            .with_detail("add one with `git remote add origin <url>`".to_string());
    }
    if options.dry_run {
        return base(
            OutcomeKind::Success,
            format!("would fetch from {}", describe_remotes(state)),
        );
    }

    let mut args = vec!["fetch", "--all"];
    if options.prune {
        args.push("--prune");
    }
    match git.run(&args) {
        Ok(out) if out.success() => {
            let mut outcome = base(
                OutcomeKind::Success,
                format!("fetched from {}", describe_remotes(state)),
            );
            let updates: Vec<String> = out
                .stderr
                .lines()
                .filter(|l| l.contains("->") || l.contains("new branch"))
                .map(|l| l.trim().to_string())
                .collect();
            outcome.details.extend(updates);
            outcome
        }
        Ok(out) => base(OutcomeKind::Failed, "fetch failed".to_string())
            .with_detail(util::concise_git_error(&out.stderr))
            .with_detail(classify_transport_hint(&out.stderr)),
        Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
            .with_detail(err.to_string()),
    }
}

fn pull_one(
    repo: &PhysicalRepository,
    state: &RepositoryState,
    git: &GitRepo<'_>,
    options: &SyncOptions,
) -> RepoOutcome {
    let base = |kind: OutcomeKind, summary: String| {
        RepoOutcome::new(&repo.id, repo.role, repo.relative_slash(), kind, summary)
    };

    // ---- preconditions ----------------------------------------------------
    if let Some(operation) = util::has_operation_in_progress(state) {
        return base(
            OutcomeKind::Failed,
            format!("{operation}: finish or abort it first"),
        );
    }
    if state.has_conflicts() {
        let files: Vec<String> = state
            .status
            .as_ref()
            .map(|s| s.conflicts().map(|e| e.path.clone()).collect())
            .unwrap_or_default();
        return base(
            OutcomeKind::Conflict,
            format!("{} unresolved conflict(s)", files.len()),
        )
        .with_details(files);
    }
    if state.head().is_detached() {
        return base(
            OutcomeKind::Failed,
            "detached HEAD: check out a branch before pulling".to_string(),
        );
    }
    if state.head().is_unborn() {
        return base(
            OutcomeKind::Skipped,
            "repository has no commits yet".to_string(),
        );
    }
    if state.remotes.is_empty() {
        return base(OutcomeKind::Skipped, "no remote configured".to_string());
    }
    let upstream = match state.status.as_ref().and_then(|s| s.upstream.clone()) {
        Some(upstream) => upstream,
        None => {
            return base(
                OutcomeKind::Skipped,
                "no upstream branch configured".to_string(),
            )
            .with_detail(
                "set one with `git push -u <remote> <branch>` (gitmesh push does this automatically)"
                    .to_string(),
            );
        }
    };

    // The `ahead`/`behind` numbers from `git status` are only as fresh as the last
    // fetch, so refresh the remote-tracking refs first. Fetching never touches the
    // work tree and never changes the remote.
    let mut refreshed = false;
    if !options.dry_run {
        let remote = remote_name_ref(&upstream);
        match git.run(&["fetch", "--prune", remote.as_str()]) {
            Ok(out) if out.success() => refreshed = true,
            Ok(out) => {
                let mut outcome = base(
                    OutcomeKind::Failed,
                    format!("cannot pull: fetching from '{upstream}' failed"),
                );
                outcome.details.push(util::concise_git_error(&out.stderr));
                outcome.details.push(classify_transport_hint(&out.stderr));
                return outcome;
            }
            Err(err) => {
                return base(OutcomeKind::Failed, "git could not be run".to_string())
                    .with_detail(err.to_string())
            }
        }
    }
    let (ahead, behind) = if refreshed {
        git.ahead_behind().ok().flatten().unwrap_or((0, 0))
    } else {
        (state.ahead().unwrap_or(0), state.behind().unwrap_or(0))
    };

    // Divergence is reported rather than resolved behind the user's back — unless the
    // caller explicitly asked for merge or rebase.
    if ahead > 0 && behind > 0 && options.strategy == PullStrategy::FastForwardOnly {
        return base(
            OutcomeKind::Failed,
            format!("diverged from '{upstream}' ({ahead} ahead, {behind} behind)"),
        )
        .with_detail(
            "GitMesh will not merge or rebase automatically; rerun with --merge or --rebase, \
             or resolve it manually"
                .to_string(),
        );
    }
    if ahead == 0 && behind == 0 {
        return base(
            OutcomeKind::Success,
            format!("already up to date with '{upstream}'"),
        );
    }
    if state.has_tracked_changes() {
        return base(
            OutcomeKind::Failed,
            "uncommitted changes would be affected by the pull".to_string(),
        )
        .with_detail(
            "commit them first (`gitmesh commit -m ...`); GitMesh never discards local work"
                .to_string(),
        );
    }
    if options.dry_run {
        return base(
            OutcomeKind::Success,
            format!(
                "would pull {behind} commit(s) from '{upstream}' ({}, based on the last fetch)",
                options.strategy.label()
            ),
        );
    }

    // ---- pull -------------------------------------------------------------
    match git.run(&options.strategy.pull_args()) {
        Ok(out) if out.success() => {
            let updated = git
                .ahead_behind()
                .ok()
                .flatten()
                .map(|(_, behind)| behind)
                .unwrap_or(0);
            let mut outcome = base(
                OutcomeKind::Success,
                if behind == 0 {
                    "already up to date".to_string()
                } else {
                    format!("updated with {behind} commit(s) from '{upstream}'")
                },
            );
            if updated > 0 {
                outcome
                    .details
                    .push(format!("{updated} commit(s) still to pull"));
            }
            outcome
        }
        Ok(out) => {
            let conflicted = state_has_conflicts(git);
            let mut outcome = if conflicted {
                base(
                    OutcomeKind::Conflict,
                    format!("pull from '{upstream}' produced conflicts"),
                )
            } else {
                base(
                    OutcomeKind::Failed,
                    format!("pull from '{upstream}' failed"),
                )
            };
            if let Some(stderr) = util::concise_git_error_opt(&out.stderr) {
                outcome.details.push(stderr);
            }
            if conflicted {
                if let Ok(status) = git.status() {
                    let files: Vec<String> = status.conflicts().map(|c| c.path.clone()).collect();
                    outcome.details.push(format!(
                        "resolve these files and commit, or run `git merge --abort` in {}",
                        state.path.display()
                    ));
                    outcome.details.extend(files);
                }
            } else if state_is_diverged(git) {
                outcome.details.push(
                    "the branch diverged from upstream; rerun with --merge or --rebase".to_string(),
                );
            } else {
                outcome.details.push(classify_transport_hint(&out.stderr));
            }
            outcome
        }
        Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
            .with_detail(err.to_string()),
    }
}

/// Remote name of an upstream ref such as `origin/main`.
fn remote_name_ref(upstream: &str) -> String {
    upstream.split('/').next().unwrap_or("origin").to_string()
}

fn state_has_conflicts(git: &GitRepo<'_>) -> bool {
    git.status().map(|s| s.has_conflicts()).unwrap_or(false)
}

/// True when the local branch and its upstream have both moved on.
fn state_is_diverged(git: &GitRepo<'_>) -> bool {
    match git.ahead_behind() {
        Ok(Some((ahead, behind))) => ahead > 0 && behind > 0,
        _ => false,
    }
}

fn describe_remotes(state: &RepositoryState) -> String {
    let names: Vec<&str> = state.remotes.iter().map(|r| r.name.as_str()).collect();
    if names.is_empty() {
        "no remote".to_string()
    } else {
        format!("remote(s) {}", names.join(", "))
    }
}

/// Turn Git's transport errors into an actionable hint.
///
/// Order matters: a local path that is not a repository also makes Git say
/// "could not read from remote repository", so the "not found" case is checked before
/// the authentication case.
pub fn classify_transport_hint(stderr: &str) -> String {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("could not resolve host")
        || lower.contains("network is unreachable")
        || lower.contains("unable to access")
        || lower.contains("connection timed out")
    {
        "Network problem: the remote host could not be reached".to_string()
    } else if lower.contains("repository not found")
        || lower.contains("does not appear to be a git repository")
        || lower.contains("no such file or directory")
    {
        "The remote repository could not be found: check the remote URL".to_string()
    } else if lower.contains("terminal prompts disabled") {
        "Credentials are required but GitMesh does not prompt; configure a credential helper or SSH agent"
            .to_string()
    } else if lower.contains("authentication failed")
        || lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("host key verification failed")
        || lower.contains("could not read from remote repository")
        || lower.contains("could not read username")
        || lower.contains("publickey")
    {
        "Authentication failed: check your credentials or SSH key for this remote".to_string()
    } else {
        "See the git output above for the underlying cause".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    fn options() -> SyncOptions {
        SyncOptions::new()
    }

    #[test]
    fn pull_reports_already_up_to_date() {
        let fixture = RepoFixture::new();
        fixture.publish(".", "remotes/root.git");
        let project = fixture.project_with(&[("root", ".")]);
        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        assert!(report.is_success());
        assert!(report.outcomes[0].summary.contains("already up to date"));
    }

    #[test]
    fn different_repositories_need_different_updates() {
        let fixture = RepoFixture::new();
        // Two repositories published to their own bare remotes, plus a second clone
        // of each that acts as "another developer".
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let bare_root = fixture.publish(".", "remotes/root.git");
        let bare_engine = fixture.publish("engine", "remotes/engine.git");

        // Push a new commit into the root's remote from a clone, and into engine's.
        let other_root = fixture.clone_outside(&bare_root, "other/root");
        std::fs::write(other_root.join("remote-change.txt"), "x").unwrap();
        let other = fixture.runner().repo(&other_root);
        other.run_checked(&["add", "-A"]).unwrap();
        other
            .run_checked(&["commit", "-q", "-m", "remote change"])
            .unwrap();
        other
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        let other_engine = fixture.clone_outside(&bare_engine, "other/engine");
        std::fs::write(other_engine.join("engine-change.txt"), "x").unwrap();
        let other = fixture.runner().repo(&other_engine);
        other.run_checked(&["add", "-A"]).unwrap();
        other
            .run_checked(&["commit", "-q", "-m", "engine remote change"])
            .unwrap();
        other
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        assert!(report.is_success(), "{:#?}", report.outcomes);
        let root = report.outcomes.iter().find(|o| o.id == "root").unwrap();
        assert!(
            root.summary.contains("updated with 1 commit"),
            "{}",
            root.summary
        );
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert!(
            engine.summary.contains("updated with 1 commit"),
            "{}",
            engine.summary
        );
        assert!(fixture.path().join("remote-change.txt").exists());
        assert!(fixture.path().join("engine/engine-change.txt").exists());
    }

    #[test]
    fn one_failing_repository_does_not_stop_the_others() {
        let fixture = RepoFixture::new();
        let mut project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "remotes/root.git");
        fixture.publish("engine", "remotes/engine.git");
        // engine keeps its upstream refs but its remote becomes unreachable: exactly
        // the "one repository cannot talk to its remote" situation.
        fixture.git_ok(
            "engine",
            &["remote", "set-url", "origin", "/nonexistent/repo.git"],
        );
        project.repositories.retain(|r| r.id != "engine");
        project.repositories.push(crate::model::PhysicalRepository {
            id: "engine".into(),
            role: crate::model::RepositoryRole::External,
            relative_path: std::path::PathBuf::from("engine"),
            remote_url: None,
            branch: None,
            absolute_path: fixture.path().join("engine"),
        });

        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        assert!(!report.is_success());
        // engine points at a remote that does not exist, so its fetch fails, while
        // the root still completes its pull.
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Failed);
        assert!(engine.summary.contains("fetch"), "{}", engine.summary);
        assert_eq!(report.counts().0, 1);
    }

    #[test]
    fn fetch_reports_failure_for_an_unreachable_remote() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.git_ok(".", &["remote", "add", "origin", "/nonexistent/repo.git"]);
        let report = fetch_project(&project, fixture.runner(), &options()).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Failed);
        assert!(root
            .details
            .iter()
            .any(|d| d.contains("remote repository could not be found")
                || d.contains("could not be found")));
    }

    #[test]
    fn conflicting_pull_is_reported_as_conflict() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let bare = fixture.publish(".", "remotes/root.git");

        // Another developer changes the same file in a different way.
        let other = fixture.clone_outside(&bare, "other/root");
        std::fs::write(other.join("README.md"), "their content\n").unwrap();
        let other_repo = fixture.runner().repo(&other);
        other_repo.run_checked(&["add", "-A"]).unwrap();
        other_repo
            .run_checked(&["commit", "-q", "-m", "their change"])
            .unwrap();
        other_repo
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        // We commit a conflicting change locally.
        fixture.write_and_commit("README.md", "our content\n");

        let options = SyncOptions {
            strategy: PullStrategy::Merge,
            ..SyncOptions::new()
        };
        let report = pull_project(&project, fixture.runner(), &options).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Conflict);
        assert!(root.summary.contains("conflicts"));
        assert!(root.details.iter().any(|d| d.contains("README.md")));
    }

    #[test]
    fn diverged_branch_is_reported_not_merged() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let bare = fixture.publish(".", "remotes/root.git");

        let other = fixture.clone_outside(&bare, "other/root");
        std::fs::write(other.join("other-file.txt"), "x").unwrap();
        let other_repo = fixture.runner().repo(&other);
        other_repo.run_checked(&["add", "-A"]).unwrap();
        other_repo
            .run_checked(&["commit", "-q", "-m", "their change"])
            .unwrap();
        other_repo
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        fixture.write_and_commit("our-file.txt", "y");

        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Failed);
        assert!(root.summary.contains("diverged"), "{}", root.summary);
        assert!(root.details.iter().any(|d| d.contains("--merge")));
        // Nothing was merged or lost.
        let log = fixture.git_ok(".", &["log", "--oneline"]);
        assert!(log.contains("our change") || log.contains("update our-file.txt"));
        assert!(!fixture.path().join("other-file.txt").exists());
    }

    #[test]
    fn local_modifications_are_never_discarded() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let bare = fixture.publish(".", "remotes/root.git");

        let other = fixture.clone_outside(&bare, "other/root");
        std::fs::write(other.join("new-file.txt"), "remote\n").unwrap();
        let other_repo = fixture.runner().repo(&other);
        other_repo.run_checked(&["add", "-A"]).unwrap();
        other_repo
            .run_checked(&["commit", "-q", "-m", "remote commit"])
            .unwrap();
        other_repo
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        fixture.append("README.md", "local uncommitted edit\n");
        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        let root = &report.outcomes[0];
        assert!(root.kind.is_problem());
        assert!(root
            .details
            .iter()
            .any(|d| d.contains("never discards local work")));
        let content = std::fs::read_to_string(fixture.path().join("README.md")).unwrap();
        assert!(content.contains("local uncommitted edit"));
    }

    #[test]
    fn missing_upstream_is_skipped_with_an_explanation() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        assert_eq!(report.counts(), (0, 2, 0, 0));
        assert!(report
            .outcomes
            .iter()
            .all(|o| o.summary.contains("no remote") || o.summary.contains("no upstream")));
    }

    #[test]
    fn dry_run_does_not_touch_the_work_tree() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let bare = fixture.publish(".", "remotes/root.git");
        let other = fixture.clone_outside(&bare, "other/root");
        std::fs::write(other.join("incoming.txt"), "x").unwrap();
        let other_repo = fixture.runner().repo(&other);
        other_repo.run_checked(&["add", "-A"]).unwrap();
        other_repo
            .run_checked(&["commit", "-q", "-m", "incoming"])
            .unwrap();
        other_repo
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        let options = SyncOptions {
            dry_run: true,
            ..SyncOptions::new()
        };
        // The dry-run uses the last known remote state, so fetch first.
        fixture.git_ok(".", &["fetch", "-q", "origin"]);
        let report = pull_project(&project, fixture.runner(), &options).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Success);
        assert!(root.summary.contains("would pull"), "{}", root.summary);
        assert!(!fixture.path().join("incoming.txt").exists());
    }

    #[test]
    fn pull_skips_repositories_without_commits() {
        let fixture = RepoFixture::new();
        let project = fixture.project_without_commits(&[("root", "."), ("fresh", "fresh")]);
        let report = pull_project(&project, fixture.runner(), &options()).unwrap();
        let fresh = report.outcomes.iter().find(|o| o.id == "fresh").unwrap();
        assert_eq!(fresh.kind, OutcomeKind::Skipped);
        assert!(fresh.summary.contains("no commits"));
    }

    #[test]
    fn transport_hints_are_actionable() {
        assert!(
            classify_transport_hint("fatal: could not resolve host: github.com")
                .to_lowercase()
                .contains("network")
        );
        assert!(
            classify_transport_hint("fatal: Authentication failed for 'https://x'")
                .to_lowercase()
                .contains("authentication failed")
        );
        assert!(classify_transport_hint("fatal: repository not found")
            .to_lowercase()
            .contains("could not be found"));
        assert!(classify_transport_hint(
            "fatal: could not read Username: terminal prompts disabled"
        )
        .to_lowercase()
        .contains("credential"));
        assert!(classify_transport_hint(
            "Host key verification failed.\nfatal: Could not read from remote repository."
        )
        .to_lowercase()
        .contains("authentication"));
        assert!(classify_transport_hint(
            "fatal: '/nonexistent/repo.git' does not appear to be a git repository"
        )
        .to_lowercase()
        .contains("could not be found"));
    }
}
