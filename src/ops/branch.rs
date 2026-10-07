//! Unified branch operations.
//!
//! A logical branch exists in every physical repository: `gitmesh branch feature/x`
//! creates `feature/x` everywhere, `gitmesh checkout feature/x` switches every
//! repository to it. GitMesh does not reimplement branching or merging — it runs
//! `git branch`, `git checkout` and `git merge` per repository and reports what
//! happened.
//!
//! Safety rules:
//!
//! * repository identity is verified before anything is changed;
//! * a repository with a merge/rebase in progress is left alone;
//! * `checkout` never discards work: Git itself refuses a checkout that would
//!   overwrite local modifications, and GitMesh reports that refusal verbatim;
//! * `delete` only ever uses `git branch -d` (Git refuses to delete a branch that is
//!   not fully merged); force deletion is explicit through [`BranchOptions::force`];
//! * the summary always reports the repositories that did *not* end up on the
//!   requested branch, so the project can never appear to be on one branch when the
//!   physical repositories disagree.

use crate::analyzer::Analyzer;
use crate::error::Result;
use crate::git::GitRepo;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryState};
use crate::ops::util::{self, RepositorySelection};
use crate::ops::{OperationReport, OutcomeKind, RepoOutcome};

/// The logical branch operation to perform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchAction {
    /// Create a branch without switching to it.
    Create { name: String },
    /// Switch to an existing branch (or create it when missing if `create` is set).
    Checkout { name: String, create: bool },
    /// Delete a branch (locally, in every repository that has it).
    Delete { name: String },
    /// Merge a branch into the current one.
    Merge { name: String },
    /// Report the logical branch state without changing anything.
    Show,
}

impl BranchAction {
    /// Name of the branch the action works with.
    pub fn branch_name(&self) -> Option<&str> {
        match self {
            BranchAction::Create { name }
            | BranchAction::Checkout { name, .. }
            | BranchAction::Delete { name }
            | BranchAction::Merge { name } => Some(name),
            BranchAction::Show => None,
        }
    }

    /// Operation label used in reports.
    pub fn label(&self) -> &'static str {
        match self {
            BranchAction::Create { .. } => "branch",
            BranchAction::Checkout { .. } => "checkout",
            BranchAction::Delete { .. } => "branch-delete",
            BranchAction::Merge { .. } => "merge",
            BranchAction::Show => "branch-status",
        }
    }
}

/// Options for branch operations.
#[derive(Debug, Clone, Default)]
pub struct BranchOptions {
    /// Which repositories to consider.
    pub selection: RepositorySelection,
    /// Allow deleting a branch that is not merged, and re-creating an existing branch.
    pub force: bool,
    /// Report what would happen without changing anything.
    pub dry_run: bool,
    /// Repository ids that must not be touched even if selected (used by the UI to
    /// protect repositories the user marked as read-only).
    pub excluded: Vec<String>,
}

/// Run a logical branch operation across the project.
pub fn branch_operation(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    action: &BranchAction,
    options: &BranchOptions,
) -> Result<OperationReport> {
    branch_operation_observed(
        project,
        runner,
        action,
        options,
        &mut crate::ops::OperationObserver::silent(),
    )
}

/// Same as [`branch_operation`], reporting each repository as the loop reaches it.
pub fn branch_operation_observed(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    action: &BranchAction,
    options: &BranchOptions,
    observer: &mut crate::ops::OperationObserver<'_>,
) -> Result<OperationReport> {
    options.selection.validate(project)?;
    if let Some(name) = action.branch_name() {
        validate_branch_name(name)?;
    }

    if matches!(action, BranchAction::Show) {
        return Ok(branch_status_report(project, runner, options));
    }

    let analyzer = Analyzer::new(project, runner);
    let outcomes = util::each_repository_observed(
        project,
        &analyzer,
        runner,
        &options.selection,
        observer,
        |repo, state, git| {
            if options.excluded.iter().any(|id| id == &repo.id) {
                return RepoOutcome::new(
                    &repo.id,
                    repo.role,
                    repo.relative_slash(),
                    OutcomeKind::Skipped,
                    "excluded from this operation",
                );
            }
            branch_one(project, repo, state, git, action, options)
        },
    );

    Ok(OperationReport::new(
        action.label(),
        options.dry_run,
        outcomes,
    ))
}

fn branch_one(
    _project: &GitMeshProject,
    repo: &PhysicalRepository,
    state: &RepositoryState,
    git: &GitRepo<'_>,
    action: &BranchAction,
    options: &BranchOptions,
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

    match action {
        BranchAction::Show => unreachable!("handled by branch_status_report"),
        BranchAction::Create { name } => {
            if state.head().is_unborn() {
                return base(
                    OutcomeKind::Skipped,
                    "repository has no commits yet, so it has no branch to create from".to_string(),
                )
                .with_detail(
                    "the branch will exist as soon as this repository has its first commit"
                        .to_string(),
                );
            }
            let exists = git.branch_exists(name).unwrap_or(false);
            if exists && !options.force {
                return base(
                    OutcomeKind::Skipped,
                    format!("branch '{name}' already exists"),
                );
            }
            if options.dry_run {
                return base(
                    OutcomeKind::Success,
                    format!("would create branch '{name}'"),
                );
            }
            let args: Vec<&str> = if exists {
                vec!["branch", "--force", name]
            } else {
                vec!["branch", name]
            };
            match git.run(&args) {
                Ok(out) if out.success() => {
                    base(OutcomeKind::Success, format!("created branch '{name}'"))
                }
                Ok(out) => base(
                    OutcomeKind::Failed,
                    format!("could not create branch '{name}'"),
                )
                .with_detail(util::concise_git_error(&out.stderr)),
                Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
                    .with_detail(err.to_string()),
            }
        }
        BranchAction::Checkout { name, create } => {
            let exists = git.branch_exists(name).unwrap_or(false);
            if !exists && !create {
                return base(
                    OutcomeKind::Failed,
                    format!("branch '{name}' does not exist in this repository"),
                )
                .with_detail(
                    "use `gitmesh checkout --create <branch>` to create it everywhere".to_string(),
                );
            }
            if state.head().branch() == Some(name) {
                return base(OutcomeKind::Skipped, format!("already on branch '{name}'"));
            }
            if options.dry_run {
                return base(
                    OutcomeKind::Success,
                    format!("would switch to branch '{name}'"),
                );
            }
            // Plain `git checkout` (or `checkout -b`): Git refuses to overwrite local
            // modifications, which is exactly the behaviour GitMesh wants.
            let args: Vec<&str> = if exists {
                vec!["checkout", name]
            } else {
                vec!["checkout", "-b", name]
            };
            match git.run(&args) {
                Ok(out) if out.success() => {
                    let mut outcome =
                        base(OutcomeKind::Success, format!("switched to branch '{name}'"));
                    if !out.stderr.trim().is_empty() {
                        outcome.details.push(out.stderr.trim().to_string());
                    }
                    outcome
                }
                Ok(out) => {
                    let mut outcome = base(
                        OutcomeKind::Failed,
                        format!("could not switch to branch '{name}'"),
                    );
                    outcome.details.push(util::concise_git_error(&out.stderr));
                    if state.has_tracked_changes() {
                        outcome.details.push(
                            "this repository has uncommitted changes; commit or stash them first \
                             (GitMesh never discards local work)"
                                .to_string(),
                        );
                    }
                    outcome
                }
                Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
                    .with_detail(err.to_string()),
            }
        }
        BranchAction::Delete { name } => {
            if state.head().branch() == Some(name) {
                return base(
                    OutcomeKind::Failed,
                    format!("'{name}' is the current branch of this repository"),
                );
            }
            if !git.branch_exists(name).unwrap_or(false) {
                return base(
                    OutcomeKind::Skipped,
                    format!("branch '{name}' does not exist here"),
                );
            }
            if options.dry_run {
                return base(
                    OutcomeKind::Success,
                    format!("would delete branch '{name}'"),
                );
            }
            let args: Vec<&str> = if options.force {
                vec!["branch", "-D", name]
            } else {
                vec!["branch", "-d", name]
            };
            match git.run(&args) {
                Ok(out) if out.success() => {
                    base(OutcomeKind::Success, format!("deleted branch '{name}'"))
                }
                Ok(out) => base(
                    OutcomeKind::Failed,
                    format!("could not delete branch '{name}'"),
                )
                .with_detail(util::concise_git_error(&out.stderr)),
                Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
                    .with_detail(err.to_string()),
            }
        }
        BranchAction::Merge { name } => {
            if state.has_conflicts() {
                return base(
                    OutcomeKind::Conflict,
                    "unresolved conflicts already present".to_string(),
                );
            }
            let exists = git.branch_exists(name).unwrap_or(false);
            let remote_exists = git
                .run_optional(&[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/remotes/origin/{name}"),
                ])
                .unwrap_or(None)
                .is_some();
            if !exists && !remote_exists {
                return base(
                    OutcomeKind::Failed,
                    format!("branch '{name}' does not exist in this repository"),
                );
            }
            if options.dry_run {
                return base(OutcomeKind::Success, format!("would merge '{name}'"));
            }
            match git.run(&["merge", "--no-edit", name]) {
                Ok(out) if out.success() => {
                    let summary = if out.stdout.contains("Already up to date") {
                        format!("already up to date with '{name}'")
                    } else {
                        format!("merged '{name}'")
                    };
                    base(OutcomeKind::Success, summary)
                }
                Ok(out) => {
                    let conflicted = git.status().map(|s| s.has_conflicts()).unwrap_or(false);
                    let mut outcome = if conflicted {
                        base(
                            OutcomeKind::Conflict,
                            format!("merge of '{name}' produced conflicts"),
                        )
                    } else {
                        base(OutcomeKind::Failed, format!("merge of '{name}' failed"))
                    };
                    outcome.details.push(util::concise_git_error(&out.stderr));
                    if conflicted {
                        if let Ok(status) = git.status() {
                            let files: Vec<String> =
                                status.conflicts().map(|c| c.path.clone()).collect();
                            outcome.details.push(format!(
                                "resolve these files and run `gitmesh commit -m ...` (or \
                                 `git merge --abort` inside {}):",
                                state.path.display()
                            ));
                            outcome.details.extend(files);
                        }
                    }
                    outcome
                }
                Err(err) => base(OutcomeKind::Failed, "git could not be run".to_string())
                    .with_detail(err.to_string()),
            }
        }
    }
}

/// Logical branch state: does every repository sit on the same branch?
fn branch_status_report(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &BranchOptions,
) -> OperationReport {
    let analyzer = Analyzer::new(project, runner);
    let status = analyzer.analyze();
    let inconsistent: Vec<String> = status
        .inconsistent_branches()
        .iter()
        .map(|r| r.id.clone())
        .collect();

    let outcomes: Vec<RepoOutcome> = status
        .repositories
        .iter()
        .map(|state| {
            let kind = if !state.is_usable() {
                OutcomeKind::Failed
            } else if inconsistent.contains(&state.id) {
                OutcomeKind::Conflict
            } else {
                OutcomeKind::Success
            };
            let branch = state.branch().unwrap_or_else(|| state.head().label());
            let mut outcome = RepoOutcome::new(
                &state.id,
                state.role,
                state.relative_path.clone(),
                kind,
                format!("on branch '{branch}'"),
            );
            if !state.is_usable() {
                outcome.details.push(
                    state
                        .error
                        .clone()
                        .unwrap_or_else(|| "repository is unavailable".to_string()),
                );
            }
            outcome
        })
        .collect();

    let mut report = OperationReport::new("branch-status", options.dry_run, outcomes);
    if !inconsistent.is_empty() {
        report.outcomes.push(
            RepoOutcome::new(
                "(project)",
                crate::model::RepositoryRole::Root,
                ".",
                OutcomeKind::Conflict,
                format!(
                    "physical repositories are on different branches: {}",
                    inconsistent.join(", ")
                ),
            )
            .with_detail(
                "run `gitmesh checkout <branch>` to bring every repository onto one branch",
            )
            .with_detail("run `gitmesh branch` after that to confirm"),
        );
    }
    report
}

/// Reject branch names Git would reject, before touching any repository.
fn validate_branch_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    let invalid = trimmed.is_empty()
        || trimmed != name
        || trimmed.starts_with('-')
        || trimmed.starts_with('/')
        || trimmed.ends_with('/')
        || trimmed.ends_with(".lock")
        || trimmed.contains("..")
        || trimmed.contains("//")
        || trimmed.contains("@{")
        || trimmed == "@"
        || trimmed.contains(|c: char| {
            c.is_whitespace() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        });
    if invalid {
        return Err(crate::error::Error::Other(format!(
            "'{name}' is not a valid Git branch name"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    fn options() -> BranchOptions {
        BranchOptions::default()
    }

    fn run(fixture: &RepoFixture, action: BranchAction) -> OperationReport {
        let project = fixture.load_project();
        branch_operation(&project, fixture.runner(), &action, &options()).unwrap()
    }

    #[test]
    fn creates_a_branch_in_every_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let report = run(
            &fixture,
            BranchAction::Create {
                name: "feature/vulkan".into(),
            },
        );
        assert!(report.is_success());
        assert_eq!(report.counts().0, 2);
        for repo in [".", "engine"] {
            let branches = fixture.git_ok(repo, &["branch", "--list", "feature/vulkan"]);
            assert!(branches.contains("feature/vulkan"), "{repo}: {branches}");
        }
    }

    #[test]
    fn checkout_switches_every_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        for repo in [".", "engine"] {
            fixture.git_ok(repo, &["branch", "feature/x"]);
        }
        let report = run(
            &fixture,
            BranchAction::Checkout {
                name: "feature/x".into(),
                create: false,
            },
        );
        assert!(report.is_success());
        for repo in [".", "engine"] {
            let head = fixture.git_ok(repo, &["symbolic-ref", "--short", "HEAD"]);
            assert_eq!(head.trim(), "feature/x");
        }
    }

    #[test]
    fn checkout_create_creates_missing_branches() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let report = run(
            &fixture,
            BranchAction::Checkout {
                name: "feature/new".into(),
                create: true,
            },
        );
        assert!(report.is_success());
        assert_eq!(
            fixture
                .git_ok("engine", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "feature/new"
        );
    }

    #[test]
    fn missing_branch_is_reported_and_nothing_changes() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let report = run(
            &fixture,
            BranchAction::Checkout {
                name: "nope".into(),
                create: false,
            },
        );
        assert!(!report.is_success());
        assert_eq!(report.counts().3, 2, "both repositories must fail");
        assert_eq!(
            fixture
                .git_ok(".", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "main"
        );
    }

    #[test]
    fn dirty_repository_is_reported_and_its_changes_are_kept() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // engine is on main and has uncommitted work to a file that differs on the
        // target branch, so Git refuses to switch.
        fixture.write_and_commit("engine/shared.txt", "main content");
        fixture.git_ok("engine", &["branch", "other"]);
        fixture.git_ok("engine", &["checkout", "-q", "other"]);
        fixture.write_and_commit("engine/shared.txt", "other content");
        fixture.git_ok("engine", &["checkout", "-q", "main"]);
        fixture.write("engine/shared.txt", "local uncommitted work");

        fixture.git_ok(".", &["branch", "other"]);
        let report = run(
            &fixture,
            BranchAction::Checkout {
                name: "other".into(),
                create: false,
            },
        );
        assert!(!report.is_success());
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Failed);
        assert!(engine
            .details
            .iter()
            .any(|d| d.contains("never discards local work")));
        // The root did switch, engine did not: the report says so explicitly.
        assert_eq!(
            fixture
                .git_ok(".", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "other"
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "main"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join("engine/shared.txt")).unwrap(),
            "local uncommitted work"
        );
    }

    #[test]
    fn branch_status_detects_inconsistent_branches() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.git_ok("engine", &["checkout", "-q", "-b", "side"]);
        let report = run(&fixture, BranchAction::Show);
        assert!(!report.is_success());
        let project_line = report
            .outcomes
            .iter()
            .find(|o| o.id == "(project)")
            .unwrap();
        assert_eq!(project_line.kind, OutcomeKind::Conflict);
        assert!(project_line.summary.contains("different branches"));
    }

    #[test]
    fn merge_reports_conflicts_without_discarding_anything() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // `create_conflict` leaves engine on main with a `feature` branch that
        // conflicts; abort it so GitMesh performs the merge itself.
        fixture.create_conflict("engine", "file.txt");
        fixture.git_ok("engine", &["merge", "--abort"]);

        let report = run(
            &fixture,
            BranchAction::Merge {
                name: "feature".into(),
            },
        );
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert_eq!(engine.kind, OutcomeKind::Conflict);
        assert!(engine
            .details
            .iter()
            .any(|d| d.contains("resolve these files")));
        // The conflicting content is untouched and the merge is still in progress.
        let content = std::fs::read_to_string(fixture.path().join("engine/file.txt")).unwrap();
        assert!(content.contains("<<<<<<<"), "{content}");
    }

    #[test]
    fn merge_up_to_date_is_a_success() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        for repo in [".", "engine"] {
            fixture.git_ok(repo, &["branch", "copy"]);
        }
        let report = run(
            &fixture,
            BranchAction::Merge {
                name: "copy".into(),
            },
        );
        assert!(report.is_success());
        assert!(report
            .outcomes
            .iter()
            .all(|o| o.summary.contains("already up to date")));
    }

    #[test]
    fn delete_only_uses_safe_deletion_by_default() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        // Unmerged branch: safe deletion must fail.
        for repo in [".", "engine"] {
            fixture.git_ok(repo, &["checkout", "-q", "-b", "wip"]);
            fixture.write_and_commit(
                &format!("{}/wip.txt", if repo == "." { "." } else { repo }),
                "wip",
            );
            fixture.git_ok(repo, &["checkout", "-q", "main"]);
        }
        let report = run(&fixture, BranchAction::Delete { name: "wip".into() });
        assert!(!report.is_success());
        assert_eq!(report.counts().3, 2);
        // Still there.
        assert!(fixture
            .git_ok(".", &["branch", "--list", "wip"])
            .contains("wip"));

        // Merged branch can be deleted safely.
        for repo in [".", "engine"] {
            fixture.git_ok(repo, &["branch", "merged"]);
        }
        let report = run(
            &fixture,
            BranchAction::Delete {
                name: "merged".into(),
            },
        );
        assert!(report.is_success());
        assert!(!fixture
            .git_ok(".", &["branch", "--list", "merged"])
            .contains("merged"));
    }

    #[test]
    fn deleting_the_current_branch_is_refused() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let report = run(
            &fixture,
            BranchAction::Delete {
                name: "main".into(),
            },
        );
        assert!(!report.is_success());
        assert!(report
            .outcomes
            .iter()
            .all(|o| o.summary.contains("is the current branch")));
    }

    #[test]
    fn partial_failure_does_not_stop_the_other_repositories() {
        let fixture = RepoFixture::new();
        let mut project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        project.repositories.retain(|r| r.id != "engine");
        project.repositories.push(crate::model::PhysicalRepository {
            id: "engine".into(),
            role: crate::model::RepositoryRole::External,
            relative_path: std::path::PathBuf::from("engine"),
            remote_url: None,
            branch: None,
            absolute_path: fixture.path().join("engine"),
        });
        std::fs::remove_dir_all(fixture.path().join("engine")).unwrap();

        let report = branch_operation(
            &project,
            fixture.runner(),
            &BranchAction::Create {
                name: "feature/ok".into(),
            },
            &options(),
        )
        .unwrap();
        assert!(report.is_partial());
        assert!(fixture
            .git_ok(".", &["branch", "--list", "feature/ok"])
            .contains("feature/ok"));
    }

    #[test]
    fn invalid_branch_names_are_rejected_before_any_repository_is_touched() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        for name in ["", "-bad", "bad..name", "bad name", "bad~name", "feature/"] {
            let err = branch_operation(
                &project,
                fixture.runner(),
                &BranchAction::Create { name: name.into() },
                &options(),
            );
            assert!(err.is_err(), "expected '{name}' to be rejected");
        }
    }

    #[test]
    fn detached_head_is_visible_and_checkout_still_works() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let oid = fixture.git_ok("engine", &["rev-parse", "HEAD"]);
        fixture.git_ok("engine", &["checkout", "-q", oid.trim()]);

        let report = run(&fixture, BranchAction::Show);
        let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
        assert!(engine.summary.contains("detached at"));

        // Checking out an existing branch brings it back on a branch.
        fixture.git_ok(".", &["branch", "main2"]);
        fixture.git_ok("engine", &["branch", "main2"]);
        let report = run(
            &fixture,
            BranchAction::Checkout {
                name: "main2".into(),
                create: false,
            },
        );
        assert!(report.is_success());
        assert_eq!(
            fixture
                .git_ok("engine", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "main2"
        );
    }

    #[test]
    fn dry_run_does_not_create_branches() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", ".")]);
        let options = BranchOptions {
            dry_run: true,
            ..Default::default()
        };
        let report = branch_operation(
            &project,
            fixture.runner(),
            &BranchAction::Create {
                name: "feature/dry".into(),
            },
            &options,
        )
        .unwrap();
        assert!(report.is_success());
        assert!(!fixture
            .git_ok(".", &["branch", "--list", "feature/dry"])
            .contains("feature/dry"));
    }

    #[test]
    fn selection_and_exclusions_are_respected() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        let options = BranchOptions {
            selection: RepositorySelection::All,
            excluded: vec!["engine".into()],
            ..Default::default()
        };
        let report = branch_operation(
            &project,
            fixture.runner(),
            &BranchAction::Create {
                name: "feature/partial".into(),
            },
            &options,
        )
        .unwrap();
        assert!(report.is_success());
        assert!(!fixture
            .git_ok("engine", &["branch", "--list", "feature/partial"])
            .contains("feature/partial"));
        assert!(fixture
            .git_ok("renderer", &["branch", "--list", "feature/partial"])
            .contains("feature/partial"));
    }
}
