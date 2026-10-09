//! Repository-scoped "stage all": stage the changes each repository owns, nothing else.
//!
//! This is the explicit counterpart of the automatic staging inside
//! [`commit_project`](super::commit_project). It is used by front ends that want the user
//! to see the staged set before committing (the terminal interface does).
//!
//! Guarantees:
//!
//! * a repository only stages files it owns. In the root repository, directories that
//!   belong to external repositories are excluded with the same pathspecs commit uses;
//! * ignore rules are respected (`git add -A` never adds ignored files);
//! * a repository with conflicted paths or an open merge, rebase or cherry-pick is
//!   **refused**, because `git add` would silently mark conflicted files as resolved;
//! * nothing is committed, pushed, pulled or otherwise changed in any repository.

use crate::error::Result;
use crate::git::GitRepo;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryState};
use crate::ops::util::{self, RepositorySelection};
use crate::ops::{OperationReport, OutcomeKind, RepoOutcome};

/// Options for [`stage_project`].
#[derive(Debug, Clone)]
pub struct StageOptions {
    /// Which repositories to consider.
    pub selection: RepositorySelection,
    /// Report what would be staged without changing anything.
    pub dry_run: bool,
}

impl Default for StageOptions {
    fn default() -> Self {
        StageOptions {
            selection: RepositorySelection::All,
            dry_run: false,
        }
    }
}

/// Stage the changes of every selected repository, one repository at a time.
///
/// Like the other project operations this never fails as a whole because one repository
/// failed; the report carries one outcome per considered repository.
pub fn stage_project(
    project: &GitMeshProject,
    runner: &crate::git::GitRunner,
    options: &StageOptions,
) -> Result<OperationReport> {
    options.selection.validate(project)?;
    let analyzer = crate::analyzer::Analyzer::new(project, runner);
    let outcomes = util::each_repository_observed(
        project,
        &analyzer,
        runner,
        &options.selection,
        &mut crate::ops::OperationObserver::silent(),
        |repo, state, git| stage_one(project, repo, state, git, options.dry_run),
    );
    Ok(OperationReport::new("stage", options.dry_run, outcomes))
}

fn stage_one(
    project: &GitMeshProject,
    repo: &PhysicalRepository,
    state: &RepositoryState,
    git: &GitRepo<'_>,
    dry_run: bool,
) -> RepoOutcome {
    let base = |kind: OutcomeKind, summary: String| {
        RepoOutcome::new(&repo.id, repo.role, repo.relative_slash(), kind, summary)
    };

    if state.has_conflicts() {
        let conflicts: Vec<String> = state
            .status
            .as_ref()
            .map(|s| s.conflicts().map(|e| e.path.clone()).collect())
            .unwrap_or_default();
        return base(
            OutcomeKind::Conflict,
            format!(
                "{} conflicted file(s): resolve before staging",
                conflicts.len()
            ),
        )
        .with_details(conflicts);
    }
    if let Some(operation) = util::has_operation_in_progress(state) {
        return base(
            OutcomeKind::Failed,
            format!("{operation} in progress: finish or abort it before staging"),
        )
        .with_detail(format!("git -C {} status", state.path.display()));
    }
    let Some(status) = state.status.as_ref() else {
        return base(OutcomeKind::Failed, "status could not be read".to_string());
    };

    let owned: Vec<&str> = status
        .entries
        .iter()
        .filter(|entry| !entry.ignored)
        .filter(|entry| !entry.staged || entry.unstaged || entry.untracked)
        .filter(|entry| {
            !util::needs_exclusions(project, repo)
                || util::relative_owned_by_external(project, &entry.path).is_none()
        })
        .map(|entry| entry.path.as_str())
        .collect();

    if owned.is_empty() {
        return base(OutcomeKind::Skipped, "nothing to stage".to_string());
    }
    if dry_run {
        return base(
            OutcomeKind::Success,
            format!("would stage {} file(s)", owned.len()),
        )
        .with_details(
            owned
                .iter()
                .take(20)
                .map(|p| format!("would stage {p}"))
                .collect::<Vec<_>>(),
        );
    }

    if let Err(err) = stage_owned(project, repo, git) {
        return base(OutcomeKind::Failed, "staging failed".to_string())
            .with_detail(util::concise_git_error(&err.to_string()));
    }

    // Count what is staged now (not what we asked for) and make sure nothing that
    // belongs to another repository ended up in this one's index.
    let staged: Vec<String> = git
        .run_optional(&["diff", "--cached", "--name-only", "-z"])
        .ok()
        .flatten()
        .map(|out| {
            out.split('\0')
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let stray: Vec<String> = staged
        .iter()
        .filter(|p| util::relative_owned_by_external(project, p).is_some())
        .cloned()
        .collect();
    if !stray.is_empty() {
        for path in &stray {
            let _ = git.run(&["reset", "-q", "--", path.as_str()]);
        }
        return base(
            OutcomeKind::Failed,
            "refused: staging would have included another repository's files".to_string(),
        )
        .with_details(stray);
    }

    base(
        OutcomeKind::Success,
        format!("staged {} file(s)", staged.len()),
    )
}

/// `git add -A` limited to this repository, with external-repository exclusions in the root.
fn stage_owned(
    project: &GitMeshProject,
    repo: &PhysicalRepository,
    git: &GitRepo<'_>,
) -> Result<()> {
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
    use crate::ops::{commit_project, CommitOptions};
    use crate::testkit::RepoFixture;

    fn stage(fixture: &RepoFixture, dry_run: bool) -> OperationReport {
        let project = fixture.load_project();
        let options = StageOptions {
            selection: RepositorySelection::All,
            dry_run,
        };
        stage_project(&project, fixture.runner(), &options).unwrap()
    }

    fn outcome<'a>(report: &'a OperationReport, id: &str) -> &'a RepoOutcome {
        report
            .outcomes
            .iter()
            .find(|o| o.id == id)
            .expect("outcome")
    }

    fn staged_paths(fixture: &RepoFixture, repo: &str) -> Vec<String> {
        fixture
            .git_ok(repo, &["diff", "--cached", "--name-only"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn stages_each_repository_only_its_own_files() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        fixture.write("README.md", "root\n");
        fixture.write("engine/src/lib.rs", "engine\n");
        fixture.write("renderer/index.js", "renderer\n");

        let report = stage(&fixture, false);
        assert!(report.is_success(), "{report:?}");
        assert_eq!(outcome(&report, "root").kind, OutcomeKind::Success);
        assert_eq!(outcome(&report, "engine").kind, OutcomeKind::Success);
        assert_eq!(outcome(&report, "renderer").kind, OutcomeKind::Success);

        // The root must not have staged the files of the nested repositories.
        assert_eq!(staged_paths(&fixture, "."), vec!["README.md"]);
        assert_eq!(staged_paths(&fixture, "engine"), vec!["src/lib.rs"]);
        assert_eq!(staged_paths(&fixture, "renderer"), vec!["index.js"]);
    }

    #[test]
    fn respects_ignore_rules() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/.gitignore", "target/\n*.log\n");
        fixture.write("engine/target/out.bin", "build\n");
        fixture.write("engine/debug.log", "noise\n");
        fixture.write("engine/src/lib.rs", "real\n");

        let report = stage(&fixture, false);
        assert!(report.is_success(), "{report:?}");
        let staged = staged_paths(&fixture, "engine");
        assert!(staged.contains(&"src/lib.rs".to_string()), "{staged:?}");
        assert!(
            !staged
                .iter()
                .any(|p| p.contains("target") || p.ends_with(".log")),
            "{staged:?}"
        );
    }

    #[test]
    fn stage_all_then_commit_is_one_real_commit_per_repository() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("README.md", "root\n");
        fixture.write("engine/src/lib.rs", "engine\n");

        stage(&fixture, false);
        let commit = commit_project(
            &project,
            fixture.runner(),
            &CommitOptions::staged("one message"),
        )
        .unwrap();
        assert!(commit.is_success(), "{commit:?}");
        assert_eq!(
            fixture.git_ok(".", &["log", "-1", "--pretty=%s"]).trim(),
            "one message"
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["log", "-1", "--pretty=%s"])
                .trim(),
            "one message"
        );
        // Nothing staged or modified is left in either repository. (The root's manifest
        // `.gitmesh/` and the nested repository directory are not leftover changes.)
        let root_left: Vec<String> = fixture
            .git_ok(".", &["status", "--porcelain"])
            .lines()
            // Nested repositories appear as `?? engine/` in the root; they are not root changes.
            .filter(|l| !l.contains(".gitmesh") && !l.contains("engine/"))
            .map(str::to_string)
            .collect();
        assert!(root_left.is_empty(), "{root_left:?}");
        assert!(fixture
            .git_ok("engine", &["status", "--porcelain"])
            .trim()
            .is_empty());
    }

    #[test]
    fn a_repository_without_changes_is_skipped_not_failed() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("README.md", "root\n");

        let report = stage(&fixture, false);
        assert!(report.is_success());
        assert_eq!(outcome(&report, "engine").kind, OutcomeKind::Skipped);
        assert_eq!(outcome(&report, "engine").summary, "nothing to stage");
    }

    #[test]
    fn dry_run_stages_nothing() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/src/lib.rs", "engine\n");

        let report = stage(&fixture, true);
        assert!(report.dry_run);
        assert_eq!(outcome(&report, "engine").kind, OutcomeKind::Success);
        assert!(staged_paths(&fixture, "engine").is_empty());
    }

    #[test]
    fn refuses_a_repository_with_conflicts_without_resolving_them() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "src/shared.txt");
        fixture.write("README.md", "root\n");

        let report = stage(&fixture, false);
        let engine = outcome(&report, "engine");
        assert_eq!(engine.kind, OutcomeKind::Conflict, "{report:?}");
        assert!(!report.is_success());
        // The conflicted file is still conflicted: `git add` was never run there.
        let status = fixture.git_ok("engine", &["status", "--porcelain"]);
        assert!(status.contains("UU src/shared.txt"), "{status}");
        // The unrelated root repository was still staged.
        assert_eq!(staged_paths(&fixture, "."), vec!["README.md"]);
    }

    #[test]
    fn refuses_a_repository_in_the_middle_of_a_merge() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "src/shared.txt");
        // Resolve the conflict and mark it resolved in the index, but keep the merge open.
        // Staging now would silently finish the merge (a merge commit later), which the
        // stage operation must never do on the user's behalf.
        fixture.write("engine/src/shared.txt", "resolved\n");
        fixture.git_ok("engine", &["add", "src/shared.txt"]);

        let report = stage(&fixture, false);
        let engine = outcome(&report, "engine");
        assert_eq!(engine.kind, OutcomeKind::Failed, "{report:?}");
        assert!(engine.summary.contains("in progress"), "{}", engine.summary);
        assert!(fixture.path().join("engine/.git/MERGE_HEAD").exists());
    }
}
