//! Unified automatic push.
//!
//! `gitmesh push` performs a real `git push` in every physical repository that has
//! something to push, and skips the rest. GitMesh never claims overall success when
//! one repository failed: the report distinguishes pushed / skipped / rejected /
//! failed per repository, and the exit code reflects it.
//!
//! Transport is Git's own — GitMesh adds no credentials handling, no HTTP layer and no
//! custom protocol. `--dry-run` maps to `git push --dry-run`, so the user can see what
//! would be sent without modifying remote state.

use crate::analyzer::Analyzer;
use crate::error::Result;
use crate::git::GitRepo;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryState};
use crate::ops::sync::classify_transport_hint;
use crate::ops::util::{self, RepositorySelection};
use crate::ops::{OperationReport, OutcomeKind, RepoOutcome};

/// Options for [`push_project`].
#[derive(Debug, Clone)]
pub struct PushOptions {
    /// Which repositories to consider.
    pub selection: RepositorySelection,
    /// Show what would be pushed without changing remote state.
    pub dry_run: bool,
    /// Set the upstream automatically when a repository has none
    /// (`git push --set-upstream origin <branch>`).
    pub set_upstream: bool,
    /// Remote to push to when a repository has no upstream yet.
    pub default_remote: String,
}

impl Default for PushOptions {
    fn default() -> Self {
        PushOptions {
            selection: RepositorySelection::All,
            dry_run: false,
            set_upstream: true,
            default_remote: "origin".to_string(),
        }
    }
}

/// Push every repository that is ahead of its upstream.
pub fn push_project(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &PushOptions,
) -> Result<OperationReport> {
    push_project_observed(
        project,
        runner,
        options,
        &mut crate::ops::OperationObserver::silent(),
    )
}

/// Same as [`push_project`], reporting each repository as the loop reaches it.
pub fn push_project_observed(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &PushOptions,
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
        |repo, state, git| push_one(repo, state, git, options),
    );
    Ok(OperationReport::new("push", options.dry_run, outcomes))
}

fn push_one(
    repo: &PhysicalRepository,
    state: &RepositoryState,
    git: &GitRepo<'_>,
    options: &PushOptions,
) -> RepoOutcome {
    let base = |kind: OutcomeKind, summary: String| {
        RepoOutcome::new(&repo.id, repo.role, repo.relative_slash(), kind, summary)
    };

    if let Some(operation) = util::has_operation_in_progress(state) {
        return base(
            OutcomeKind::Failed,
            format!("{operation}: finish or abort it first"),
        );
    }
    if state.head().is_unborn() {
        return base(
            OutcomeKind::Skipped,
            "repository has no commits yet".to_string(),
        );
    }
    if state.head().is_detached() {
        return base(
            OutcomeKind::Failed,
            "detached HEAD: nothing to push (check out a branch first)".to_string(),
        );
    }
    if state.remotes.is_empty() {
        // A repository that was never given a remote is not a broken repository: there is
        // simply nowhere to push to yet. Reporting it as a failure would make `gitmesh push`
        // exit non-zero for every project that keeps some repositories local — which is a
        // normal, supported layout — and would hide the real failures among it. Fetch and
        // pull have always treated the same situation as skipped.
        return base(OutcomeKind::Skipped, "no remote configured".to_string())
            .with_detail("add one with `git remote add origin <url>`".to_string());
    }

    let upstream = state.status.as_ref().and_then(|s| s.upstream.clone());
    let ahead = state.ahead().unwrap_or(0);

    if upstream.is_some() && ahead == 0 {
        // Still worth reporting if the upstream branch does not exist yet, which Git
        // reports as "everything up-to-date" but which is really a push.
        return base(OutcomeKind::Skipped, "nothing to push".to_string());
    }

    let branch = match state.branch() {
        Some(branch) => branch,
        None => {
            return base(
                OutcomeKind::Failed,
                "no current branch: cannot push".to_string(),
            )
        }
    };

    // A repository that has an upstream but no remote to push it to.
    let remote_name = upstream
        .as_deref()
        .and_then(|u| u.split('/').next())
        .map(str::to_string)
        .unwrap_or_else(|| options.default_remote.clone());

    if upstream.is_none() && !options.set_upstream {
        return base(OutcomeKind::Skipped, "no upstream configured".to_string()).with_detail(
            "rerun with upstream setup enabled, or `git push -u <remote> <branch>`".to_string(),
        );
    }
    if upstream.is_none() && !state.remotes.iter().any(|r| r.name == remote_name) {
        return base(
            OutcomeKind::Failed,
            format!("no '{remote_name}' remote to push to"),
        )
        .with_details(
            state
                .remotes
                .iter()
                .map(|r| format!("configured remote: {}", r.name))
                .collect::<Vec<_>>(),
        );
    }

    let commit_count = ahead.max(if upstream.is_none() { 1 } else { 0 });
    if options.dry_run {
        let remote_url = state
            .remotes
            .iter()
            .find(|r| r.name == remote_name)
            .and_then(|r| r.fetch_url())
            .unwrap_or("(unknown url)");
        return base(
            OutcomeKind::Success,
            if upstream.is_none() {
                format!("would push '{branch}' and set upstream on {remote_name} ({remote_url})")
            } else {
                format!(
                    "would push {commit_count} commit(s) of '{branch}' to {remote_name} ({remote_url})"
                )
            },
        );
    }

    let mut args: Vec<String> = vec!["push".to_string()];
    if options.dry_run {
        args.push("--dry-run".to_string());
    }
    if upstream.is_none() {
        args.push("--set-upstream".to_string());
        args.push(remote_name.clone());
        args.push(branch.to_string());
    }

    let args_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let target = upstream_ref(upstream.as_deref(), &branch);
    match git.run(&args_refs) {
        Ok(out) if out.success() => {
            let mut outcome = base(
                OutcomeKind::Success,
                if upstream.is_none() {
                    format!("pushed '{branch}' and set upstream on {remote_name}")
                } else {
                    format!("pushed {commit_count} commit(s) to {target}")
                },
            );
            let summary = out.stderr.trim();
            if !summary.is_empty() {
                outcome.details.extend(
                    summary
                        .lines()
                        .filter(|l| !l.contains("remote:") || l.len() < 200)
                        .map(|l| l.trim().to_string())
                        .take(10),
                );
            }
            outcome
        }
        Ok(out) => {
            let stderr = out.stderr.clone();
            let lower = stderr.to_ascii_lowercase();
            let (kind, summary) = if lower.contains("non-fast-forward")
                || lower.contains("rejected")
                || lower.contains("fetch first")
            {
                (
                    OutcomeKind::Failed,
                    "rejected: the remote has commits this repository does not have".to_string(),
                )
            } else if lower.contains("authentication")
                || lower.contains("permission denied")
                || lower.contains("access denied")
            {
                (OutcomeKind::Failed, "authentication failed".to_string())
            } else if lower.contains("not found") || lower.contains("does not appear") {
                (
                    OutcomeKind::Failed,
                    "remote repository not found".to_string(),
                )
            } else if lower.contains("could not resolve host") || lower.contains("unable to access")
            {
                (
                    OutcomeKind::Failed,
                    "network failure while contacting the remote".to_string(),
                )
            } else if lower.contains("no upstream branch") {
                (
                    OutcomeKind::Failed,
                    "no upstream branch configured".to_string(),
                )
            } else {
                (OutcomeKind::Failed, "push failed".to_string())
            };
            let mut outcome = base(kind, summary);
            outcome.details.push(util::concise_git_error(&stderr));
            outcome.details.push(classify_transport_hint(&stderr));
            if lower.contains("non-fast-forward") || lower.contains("rejected") {
                outcome.details.push(
                    "run `gitmesh pull` first (or resolve the divergence manually); nothing local was changed"
                        .to_string(),
                );
            }
            outcome
        }
        Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
            .with_detail(err.to_string()),
    }
}

fn upstream_ref(upstream: Option<&str>, branch: &str) -> String {
    upstream.unwrap_or(branch).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    fn options() -> PushOptions {
        PushOptions::default()
    }

    #[test]
    fn pushes_only_repositories_with_work_and_reports_nothing_to_push() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "remotes/root.git");
        fixture.publish("engine", "remotes/engine.git");

        fixture.write_and_commit("src/main.rs", "root change");

        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        assert!(report.is_success());
        let root = report.outcomes.iter().find(|o| o.id == "root").unwrap();
        assert_eq!(root.kind, OutcomeKind::Success);
        assert!(root.summary.contains("pushed 1 commit"));
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Skipped);
        assert_eq!(engine.summary, "nothing to push");

        // The commit really arrived at the remote.
        let bare = fixture.runner().repo(fixture.bare_path("remotes/root.git"));
        let log = bare.run_checked(&["log", "-1", "--pretty=%s"]).unwrap();
        assert!(log.contains("update src/main.rs"));
    }

    #[test]
    fn pushes_several_repositories_at_once() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        for repo in [".", "engine", "renderer"] {
            fixture.publish(
                repo,
                &format!("remotes/{}.git", if repo == "." { "root" } else { repo }),
            );
        }
        fixture.write_and_commit("src/a.rs", "a");
        fixture.write_and_commit("engine/src/b.rs", "b");
        fixture.write_and_commit("renderer/c.js", "c");

        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        assert_eq!(report.counts().0, 3);
        for (bare, msg) in [
            ("remotes/root.git", "src/a.rs"),
            ("remotes/engine.git", "src/b.rs"),
            ("remotes/renderer.git", "c.js"),
        ] {
            let bare_repo = fixture.runner().repo(fixture.bare_path(bare));
            let log = bare_repo
                .run_checked(&["log", "-1", "--pretty=%s"])
                .unwrap();
            assert!(log.contains(msg), "{bare}: {log}");
        }
    }

    #[test]
    fn sets_upstream_when_missing() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.create_bare("remotes/root.git");
        fixture.git_ok(
            ".",
            &[
                "remote",
                "add",
                "origin",
                fixture
                    .bare_path("remotes/root.git")
                    .to_string_lossy()
                    .as_ref(),
            ],
        );
        fixture.write_and_commit("src/main.rs", "x");

        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Success);
        assert!(root.summary.contains("set upstream"));
        let upstream = fixture.git_ok(".", &["rev-parse", "--abbrev-ref", "origin/main"]);
        assert_eq!(upstream.trim(), "origin/main");
    }

    #[test]
    fn a_repository_without_a_remote_is_skipped_not_failed() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let root = &report.outcomes[0];
        // Local-only is a supported layout, not a failure: the reason and the fix are
        // reported, and the operation as a whole stays successful.
        assert_eq!(root.kind, OutcomeKind::Skipped);
        assert!(root.summary.contains("no remote"));
        assert!(
            root.details
                .iter()
                .any(|line| line.contains("git remote add")),
            "{:?}",
            root.details
        );
        assert!(report.is_success(), "nothing failed");
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_failing_repository_still_fails_while_a_local_one_is_skipped() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // The root keeps its commits local; engine has a remote that does not exist, which
        // is a real failure and must not be softened by the local-only case.
        fixture.publish(".", "remotes/root.git");
        fixture
            .runner()
            .repo(fixture.path().join("engine"))
            .run_checked(&[
                "remote",
                "add",
                "origin",
                fixture.bare_path("missing.git").to_str().unwrap(),
            ])
            .unwrap();
        fixture.write("engine/lib.rs", "x");
        fixture.commit("engine", "engine work");
        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let by_id: std::collections::HashMap<&str, &crate::ops::RepoOutcome> = report
            .outcomes
            .iter()
            .map(|outcome| (outcome.id.as_str(), outcome))
            .collect();
        assert_eq!(
            by_id["engine"].kind,
            OutcomeKind::Failed,
            "{:?}",
            by_id["engine"]
        );
        assert_eq!(report.exit_code(), 1, "the real failure is still reported");
    }

    #[test]
    fn rejects_non_fast_forward_and_keeps_local_commits() {
        let fixture = RepoFixture::new();
        let bare = fixture.publish(".", "remotes/root.git");
        let project = fixture.project_with(&[("root", ".")]);

        // Another developer pushes a commit we do not have.
        let other = fixture.clone_outside(&bare, "other/root");
        std::fs::write(other.join("their.txt"), "x").unwrap();
        let other_repo = fixture.runner().repo(&other);
        other_repo.run_checked(&["add", "-A"]).unwrap();
        other_repo
            .run_checked(&["commit", "-q", "-m", "theirs"])
            .unwrap();
        other_repo
            .run_checked(&["push", "-q", "origin", "main"])
            .unwrap();

        // We commit and try to push without pulling.
        fixture.write_and_commit("mine.txt", "y");
        let oid = fixture.git_ok(".", &["rev-parse", "HEAD"]);

        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Failed);
        assert!(root.summary.contains("rejected"), "{}", root.summary);
        assert!(root.details.iter().any(|d| d.contains("gitmesh pull")));
        // Our commit is still there.
        assert_eq!(fixture.git_ok(".", &["rev-parse", "HEAD"]), oid);
    }

    #[test]
    fn detects_detached_head_and_missing_upstream() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "remotes/root.git");
        let oid = fixture.git_ok("engine", &["rev-parse", "HEAD"]);
        fixture.git_ok("engine", &["checkout", "-q", oid.trim()]);

        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Failed);
        assert!(engine.summary.contains("detached HEAD"));

        // A repository with a remote but no upstream gets one set up automatically.
        fixture.git_ok("engine", &["checkout", "-q", "main"]);
        fixture.create_bare("remotes/engine.git");
        fixture.git_ok(
            "engine",
            &[
                "remote",
                "add",
                "origin",
                fixture
                    .bare_path("remotes/engine.git")
                    .to_string_lossy()
                    .as_ref(),
            ],
        );
        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Success, "{:#?}", engine);
        assert!(engine.summary.contains("set upstream"));
    }

    #[test]
    fn partial_failure_is_reported_honestly() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        fixture.publish(".", "remotes/root.git");
        fixture.publish("renderer", "remotes/renderer.git");
        // engine has a remote that cannot be reached.
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", "/nonexistent/engine.git"],
        );
        fixture.write_and_commit("src/a.rs", "a");
        fixture.write_and_commit("engine/b.rs", "b");
        fixture.write_and_commit("renderer/c.js", "c");

        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        assert!(!report.is_success());
        assert!(report.is_partial());
        assert_eq!(report.exit_code(), 1);
        let by_id = |id: &str| report.outcomes.iter().find(|o| o.id == id).unwrap().kind;
        assert_eq!(by_id("root"), OutcomeKind::Success);
        assert_eq!(by_id("renderer"), OutcomeKind::Success);
        assert_eq!(by_id("engine"), OutcomeKind::Failed);
    }

    #[test]
    fn dry_run_does_not_modify_the_remote() {
        let fixture = RepoFixture::new();
        let bare = fixture.publish(".", "remotes/root.git");
        let project = fixture.project_with(&[("root", ".")]);
        fixture.write_and_commit("src/main.rs", "x");

        let options = PushOptions {
            dry_run: true,
            ..PushOptions::default()
        };
        let report = push_project(&project, fixture.runner(), &options).unwrap();
        let root = &report.outcomes[0];
        assert_eq!(root.kind, OutcomeKind::Success);
        assert!(root.summary.contains("would push"), "{}", root.summary);
        assert!(report.dry_run);

        let bare_repo = fixture.runner().repo(&bare);
        let log = bare_repo.run_checked(&["log", "--oneline"]).unwrap();
        assert_eq!(
            log.lines().count(),
            1,
            "remote must not have received the commit"
        );
    }

    #[test]
    fn nothing_to_push_anywhere_is_a_success() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "remotes/root.git");
        fixture.publish("engine", "remotes/engine.git");
        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        assert_eq!(report.counts(), (0, 2, 0, 0));
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn a_repository_without_commits_is_skipped() {
        let fixture = RepoFixture::new();
        let project = fixture.project_without_commits(&[("root", "."), ("fresh", "fresh")]);
        fixture.publish(".", "remotes/root.git");
        let report = push_project(&project, fixture.runner(), &options()).unwrap();
        let fresh = report.outcomes.iter().find(|o| o.id == "fresh").unwrap();
        assert_eq!(fresh.kind, OutcomeKind::Skipped);
        assert!(fresh.summary.contains("no commits"));
    }

    #[test]
    fn selection_limits_the_push() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "remotes/root.git");
        fixture.create_bare("remotes/engine.git");
        fixture.git_ok(
            "engine",
            &[
                "remote",
                "add",
                "origin",
                fixture
                    .bare_path("remotes/engine.git")
                    .to_string_lossy()
                    .as_ref(),
            ],
        );
        fixture.write_and_commit("src/a.rs", "a");
        fixture.write_and_commit("engine/b.rs", "b");

        let options = PushOptions {
            selection: RepositorySelection::from_ids(vec!["engine".into()]),
            ..PushOptions::default()
        };
        let report = push_project(&project, fixture.runner(), &options).unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].id, "engine");
        let bare_root = fixture.runner().repo(fixture.bare_path("remotes/root.git"));
        let log = bare_root.run_checked(&["log", "--oneline"]).unwrap();
        assert!(
            !log.contains("src/a.rs"),
            "root must not have been pushed: {log}"
        );
    }
}
