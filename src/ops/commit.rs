//! Unified commit: one logical message, one real Git commit per physical repository.
//!
//! `gitmesh commit -m "message"` is deliberately close to `git commit -a -m` from the
//! user's point of view, with three differences that matter:
//!
//! * the same commit message is used in every affected repository, so one logical
//!   change is recorded consistently everywhere;
//! * staging is automatic but **bounded**: a repository only ever stages its own
//!   files, and the root repository explicitly excludes directories that belong to
//!   external repositories;
//! * a repository that fails (a rejected pre-commit hook, a missing directory) does
//!   not stop the others, and the summary says exactly what happened.
//!
//! There is no fake global commit: each physical repository gets a real commit in its
//! own history.

use crate::analyzer::Analyzer;
use crate::error::Result;
use crate::git::GitRepo;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryState};
use crate::ops::util::{self, RepositorySelection};
use crate::ops::{OperationReport, OutcomeKind, RepoOutcome};

/// Options for [`commit_project`].
#[derive(Debug, Clone)]
pub struct CommitOptions {
    /// Logical commit message used in every affected repository.
    pub message: String,
    /// Which repositories to consider.
    pub selection: RepositorySelection,
    /// Report what would be committed without changing anything.
    pub dry_run: bool,
    /// Stage untracked files as well (default: yes).
    pub include_untracked: bool,
    /// Skip the "nothing to commit" repositories entirely instead of listing them.
    pub quiet_clean: bool,
    /// Commit only what is already staged; never stage automatically.
    ///
    /// Used by front ends that stage explicitly (see [`stage_project`](super::stage_project)),
    /// so the user sees the staged set before committing.
    pub staged_only: bool,
}

impl CommitOptions {
    /// Options with automatic staging of every change in the selected repositories.
    pub fn new(message: impl Into<String>) -> Self {
        CommitOptions {
            message: message.into(),
            selection: RepositorySelection::All,
            dry_run: false,
            include_untracked: true,
            quiet_clean: false,
            staged_only: false,
        }
    }

    /// Options that commit only the changes already staged in each repository.
    pub fn staged(message: impl Into<String>) -> Self {
        CommitOptions {
            staged_only: true,
            ..CommitOptions::new(message)
        }
    }
}

/// Commit all changes across the project.
///
/// Never fails as a whole because one repository failed: the returned report always
/// contains one outcome per considered repository, and the caller decides the exit
/// code ([`OperationReport::exit_code`] does the right thing).
pub fn commit_project(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &CommitOptions,
) -> Result<OperationReport> {
    commit_project_observed(
        project,
        runner,
        options,
        &mut crate::ops::OperationObserver::silent(),
    )
}

/// Same as [`commit_project`], reporting each repository as the loop reaches it.
pub fn commit_project_observed(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &CommitOptions,
    observer: &mut crate::ops::OperationObserver<'_>,
) -> Result<OperationReport> {
    if options.message.trim().is_empty() {
        return Err(crate::error::Error::Other(
            "a commit message is required (use -m/--message)".into(),
        ));
    }
    options.selection.validate(project)?;

    let analyzer = Analyzer::new(project, runner);
    let outcomes = util::each_repository_observed(
        project,
        &analyzer,
        runner,
        &options.selection,
        observer,
        |repo, state, git| commit_one(project, repo, state, git, options),
    );
    Ok(OperationReport::new("commit", options.dry_run, outcomes))
}

fn commit_one(
    project: &GitMeshProject,
    repo: &PhysicalRepository,
    state: &RepositoryState,
    git: &GitRepo<'_>,
    options: &CommitOptions,
) -> RepoOutcome {
    let base = |kind: OutcomeKind, summary: String| {
        RepoOutcome::new(&repo.id, repo.role, repo.relative_slash(), kind, summary)
    };

    // ---- safety checks -----------------------------------------------------
    if state.has_conflicts() {
        let conflicts: Vec<String> = state
            .status
            .as_ref()
            .map(|s| s.conflicts().map(|e| e.path.clone()).collect())
            .unwrap_or_default();
        return base(
            OutcomeKind::Conflict,
            format!(
                "{} conflicted file(s): resolve before committing",
                conflicts.len()
            ),
        )
        .with_details(conflicts);
    }
    if let Some(operation) = util::has_operation_in_progress(state) {
        return base(
            OutcomeKind::Failed,
            format!("{operation}: finish or abort it first"),
        )
        .with_detail(format!("git -C {} status", state.path.display()));
    }

    let Some(status) = state.status.as_ref() else {
        return base(OutcomeKind::Failed, "status could not be read".to_string());
    };

    // Changes this repository owns, excluding anything that belongs to an external
    // repository (only possible in the root repository).
    let owned: Vec<String> = status
        .entries
        .iter()
        .filter(|entry| !entry.ignored)
        .filter(|entry| {
            if util::needs_exclusions(project, repo) {
                util::relative_owned_by_external(project, &entry.path).is_none()
            } else {
                true
            }
        })
        .filter(|entry| options.include_untracked || !entry.untracked)
        .filter(|entry| !options.staged_only || entry.staged)
        .map(|entry| entry.path.clone())
        .collect();

    if owned.is_empty() {
        let message = if options.staged_only {
            "nothing staged to commit"
        } else {
            "nothing to commit"
        };
        return base(OutcomeKind::Skipped, message.to_string());
    }

    if options.dry_run {
        return base(
            OutcomeKind::Success,
            if options.staged_only {
                format!("would commit {} staged file(s)", owned.len())
            } else {
                format!("would stage and commit {} file(s)", owned.len())
            },
        )
        .with_details(
            owned
                .iter()
                .take(20)
                .map(|p| format!("would commit {p}"))
                .collect::<Vec<_>>(),
        );
    }

    // ---- stage -------------------------------------------------------------
    if !options.staged_only {
        if let Err(err) = stage_all(project, repo, git) {
            return base(OutcomeKind::Failed, "staging failed".to_string())
                .with_detail(err.to_string());
        }
    }

    let mut details: Vec<String> = Vec::new();
    if let Ok(Some(staged)) = git.run_optional(&["diff", "--cached", "--name-only", "-z"]) {
        let stray: Vec<String> = staged
            .split('\0')
            .filter(|p| !p.is_empty())
            .filter(|p| util::relative_owned_by_external(project, p).is_some())
            .map(str::to_string)
            .collect();
        if !stray.is_empty() {
            // Defence in depth: never leave another repository's files staged.
            for path in &stray {
                let _ = git.run(&["reset", "-q", "--", path.as_str()]);
            }
            details.push(format!(
                "unstaged {} file(s) that belong to another repository: {}",
                stray.len(),
                stray.join(", ")
            ));
        }
    }

    // ---- commit ------------------------------------------------------------
    let staged_files = git
        .run_optional(&["diff", "--cached", "--name-only"])
        .ok()
        .flatten()
        .map(|out| out.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(owned.len());

    let out = match git.run(&["commit", "-m", options.message.as_str()]) {
        Ok(out) => out,
        Err(err) => {
            return base(
                OutcomeKind::Failed,
                "git commit could not be run".to_string(),
            )
            .with_detail(err.to_string());
        }
    };

    if !out.success() {
        let mut outcome = base(
            OutcomeKind::Failed,
            format!(
                "commit failed ({} file(s) left staged, nothing was discarded)",
                staged_files
            ),
        );
        outcome.details.push(util::concise_git_error(&out.stderr));
        outcome.details.extend(details);
        return outcome;
    }

    let oid = git
        .head_oid()
        .ok()
        .flatten()
        .map(|oid| crate::git::short_oid(&oid))
        .unwrap_or_else(|| "?".to_string());

    let mut outcome = base(
        OutcomeKind::Success,
        format!("committed {staged_files} file(s) [{oid}]"),
    );
    if state.head().is_detached() {
        outcome.details.push(
            "this repository is in detached HEAD state: the commit is not on a branch".to_string(),
        );
    }
    outcome.details.extend(details);
    outcome
}

/// Stage every change of a repository, excluding directories owned elsewhere.
fn stage_all(project: &GitMeshProject, repo: &PhysicalRepository, git: &GitRepo<'_>) -> Result<()> {
    let mut args: Vec<String> = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
    if util::needs_exclusions(project, repo) {
        args.extend(util::exclusion_pathspecs(project));
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    git.run_checked(&args)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    fn commit_all(fixture: &RepoFixture, message: &str) -> OperationReport {
        let project = fixture.load_project();
        commit_project(&project, fixture.runner(), &CommitOptions::new(message)).unwrap()
    }

    #[test]
    fn commits_a_single_modified_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "fn main() {}");
        let report = commit_all(&fixture, "add main");
        assert!(report.is_success());
        let root = report.outcomes.iter().find(|o| o.id == "root").unwrap();
        assert_eq!(root.kind, OutcomeKind::Success);
        assert!(root.summary.contains("1 file(s)"));
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Skipped);
    }

    #[test]
    fn commits_multiple_repositories_with_the_same_message() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        fixture.write("src/main.rs", "root");
        fixture.write("engine/src/lib.rs", "engine");
        fixture.write("renderer/index.js", "renderer");

        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::new("tick")).unwrap();
        assert_eq!(report.counts().0, 3);
        for repo in [".", "engine", "renderer"] {
            let log = fixture.git_ok(repo, &["log", "-1", "--pretty=%s"]);
            assert_eq!(log.trim(), "tick");
        }
    }

    #[test]
    fn stages_untracked_files() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.write("brand/new.txt", "hello");
        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::new("new file")).unwrap();
        assert!(report.is_success());
        let tracked = fixture.git_ok(".", &["ls-files"]);
        assert!(tracked.contains("brand/new.txt"));
    }

    #[test]
    fn records_deleted_files() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.write_and_commit("gone.txt", "bye");
        fixture.remove("gone.txt");
        let report = commit_project(
            &project,
            fixture.runner(),
            &CommitOptions::new("remove gone"),
        )
        .unwrap();
        assert!(report.is_success());
        let tracked = fixture.git_ok(".", &["ls-files"]);
        assert!(!tracked.contains("gone.txt"));
    }

    #[test]
    fn works_in_an_empty_repository_without_commits() {
        let fixture = RepoFixture::new();
        // A brand new repository: `git init` only, no commit yet.
        let project = fixture.project_without_commits(&[("root", "."), ("fresh", "fresh")]);
        fixture.write("fresh/hello.txt", "hi");
        let report = commit_project(
            &project,
            fixture.runner(),
            &CommitOptions::new("first commit"),
        )
        .unwrap();
        let fresh = report.outcomes.iter().find(|o| o.id == "fresh").unwrap();
        assert_eq!(fresh.kind, OutcomeKind::Success);
        let log = fixture.git_ok("fresh", &["log", "--oneline"]);
        assert_eq!(log.lines().count(), 1);
    }

    #[test]
    fn repository_with_no_changes_is_reported_as_skipped() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let report = commit_all(&fixture, "nothing");
        assert_eq!(report.counts(), (0, 2, 0, 0));
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn root_repository_never_stages_files_of_an_external_repository() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // A change inside the external repository, and a real change in the root.
        fixture.write("engine/src/lib.rs", "engine change");
        fixture.write("src/main.rs", "root change");

        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::new("mixed")).unwrap();
        assert!(report.is_success());

        // Root committed only its own file.
        let root_files = fixture.git_ok(".", &["show", "--name-only", "--pretty=format:", "HEAD"]);
        assert!(root_files.contains("src/main.rs"));
        assert!(!root_files.contains("engine/src/lib.rs"));
        assert!(!root_files.contains("engine"));

        // Engine committed its own file.
        let engine_files = fixture.git_ok(
            "engine",
            &["show", "--name-only", "--pretty=format:", "HEAD"],
        );
        assert!(engine_files.contains("src/lib.rs"));
    }

    #[test]
    fn root_status_does_not_see_the_external_repository_as_a_change() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let analyzer = Analyzer::new(&project, fixture.runner());
        let status = analyzer.analyze();
        assert!(
            !status.has_changes(),
            "fresh project must be clean: {:#?}",
            status.notices
        );

        let owned = analyzer.owned_changes(&status);
        assert!(
            !owned.iter().any(|c| c.logical_path.starts_with("engine")),
            "root must not report engine content: {owned:#?}"
        );
    }

    #[test]
    fn failing_repository_does_not_stop_the_others() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        fixture.write("src/main.rs", "root");
        fixture.write("renderer/index.js", "renderer");

        // Make commits fail in `engine` with a pre-commit hook.
        let hooks = fixture.mkdir("engine/.git/hooks");
        let hook = hooks.join("pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook, perms).unwrap();
        }
        fixture.write("engine/src/lib.rs", "engine");

        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::new("partial")).unwrap();
        assert!(!report.is_success());
        assert!(report.is_partial());
        assert_eq!(report.counts(), (2, 0, 0, 1));
        assert_eq!(report.exit_code(), 1);

        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Failed);
        assert!(engine.summary.contains("left staged"));
        // The user's change is still there, staged and not lost.
        let staged = fixture.git_ok("engine", &["diff", "--cached", "--name-only"]);
        assert!(staged.contains("src/lib.rs"));
        // Other repositories really did commit.
        assert_eq!(
            fixture.git_ok(".", &["log", "-1", "--pretty=%s"]).trim(),
            "partial"
        );
        assert_eq!(
            fixture
                .git_ok("renderer", &["log", "-1", "--pretty=%s"])
                .trim(),
            "partial"
        );
    }

    #[test]
    fn conflict_prevents_commit_without_touching_anything() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "conflict.txt");
        let head_before = fixture.git_ok("engine", &["rev-parse", "HEAD"]);

        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::new("nope")).unwrap();
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Conflict);
        assert!(engine.summary.contains("conflicted file"));
        assert_eq!(
            fixture.git_ok("engine", &["rev-parse", "HEAD"]),
            head_before
        );
    }

    #[test]
    fn missing_repository_is_reported_as_failed() {
        let fixture = RepoFixture::new();
        let mut project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        std::fs::remove_dir_all(fixture.path().join("engine")).unwrap();
        project.repositories.retain(|r| r.id != "engine");
        project.repositories.push(crate::model::PhysicalRepository {
            id: "engine".into(),
            role: crate::model::RepositoryRole::External,
            relative_path: std::path::PathBuf::from("engine"),
            remote_url: None,
            branch: None,
            absolute_path: fixture.path().join("engine"),
        });
        fixture.write("src/main.rs", "root");

        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::new("msg")).unwrap();
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Failed);
        assert!(engine.summary.contains("missing"));
        // Root still committed.
        assert_eq!(report.counts().0, 1);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.write("src/main.rs", "content");
        let head_before = fixture.git_ok(".", &["rev-parse", "HEAD"]);

        let options = CommitOptions {
            dry_run: true,
            ..CommitOptions::new("dry")
        };
        let report = commit_project(&project, fixture.runner(), &options).unwrap();
        assert!(report.is_success());
        assert!(report.dry_run);
        assert_eq!(report.counts().0, 1);
        assert_eq!(fixture.git_ok(".", &["rev-parse", "HEAD"]), head_before);
        let porcelain = fixture.git_ok(".", &["status", "--porcelain", "-uall"]);
        assert!(porcelain.contains("src/main.rs"));
    }

    #[test]
    fn selection_limits_the_operation() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "root");
        fixture.write("engine/src/lib.rs", "engine");

        let options = CommitOptions {
            selection: RepositorySelection::from_ids(vec!["engine".into()]),
            ..CommitOptions::new("engine only")
        };
        let report = commit_project(&project, fixture.runner(), &options).unwrap();
        assert_eq!(report.outcomes.len(), 1);
        assert_eq!(report.outcomes[0].id, "engine");
        let root_status = fixture.git_ok(".", &["status", "--porcelain", "-uall"]);
        assert!(
            root_status.contains("src/main.rs"),
            "root must be untouched"
        );
    }

    #[test]
    fn commit_message_handling_rejects_empty_messages() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.write("src/main.rs", "x");
        let err =
            commit_project(&project, fixture.runner(), &CommitOptions::new("   ")).unwrap_err();
        assert!(err.to_string().contains("commit message is required"));
    }

    #[test]
    fn messages_with_special_characters_are_preserved() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        fixture.write("src/main.rs", "x");
        let message = "fix: don't break \"quotes\" & $VARS";
        commit_project(&project, fixture.runner(), &CommitOptions::new(message)).unwrap();
        assert_eq!(
            fixture.git_ok(".", &["log", "-1", "--pretty=%s"]).trim(),
            message
        );
    }

    #[test]
    fn commit_requires_a_known_repository_selection() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let options = CommitOptions {
            selection: RepositorySelection::from_ids(vec!["nope".into()]),
            ..CommitOptions::new("msg")
        };
        let err = commit_project(&project, fixture.runner(), &options).unwrap_err();
        assert!(err.to_string().contains("unknown repository id"));
    }

    #[test]
    fn staged_only_commits_what_is_staged_and_leaves_the_rest_alone() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/src/a.rs", "a\n");
        fixture.write("engine/src/b.rs", "b\n");
        fixture.git_ok("engine", &["add", "src/a.rs"]);

        let report =
            commit_project(&project, fixture.runner(), &CommitOptions::staged("only a")).unwrap();
        assert!(report.is_success(), "{report:?}");
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Success);
        assert!(engine.summary.contains("1 file(s)"), "{}", engine.summary);

        // a.rs is committed; b.rs was never staged and stays untracked.
        let files = fixture.git_ok("engine", &["show", "--name-only", "--pretty=", "HEAD"]);
        assert_eq!(files.trim(), "src/a.rs");
        let status = fixture.git_ok("engine", &["status", "--porcelain"]);
        assert!(status.contains("?? src/b.rs"), "{status}");
    }

    #[test]
    fn staged_only_skips_repositories_with_nothing_staged() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/src/a.rs", "a\n");
        fixture.write("README.md", "root\n");

        let report = commit_project(
            &project,
            fixture.runner(),
            &CommitOptions::staged("nothing"),
        )
        .unwrap();
        assert!(report.is_success());
        assert_eq!(report.counts().0, 0, "{report:?}");
        for outcome in &report.outcomes {
            assert_eq!(outcome.kind, OutcomeKind::Skipped);
            assert_eq!(outcome.summary, "nothing staged to commit");
        }
        // Nothing was committed anywhere.
        assert_eq!(
            fixture
                .git_ok("engine", &["rev-list", "--count", "HEAD"])
                .trim(),
            "1"
        );
    }
}
