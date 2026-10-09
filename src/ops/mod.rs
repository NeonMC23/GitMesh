//! Orchestration: one logical operation translated into many physical Git operations.
//!
//! Every GitMesh operation follows the same contract:
//!
//! 1. the project configuration decides which repositories are involved;
//! 2. each repository is inspected (and its identity verified) before being touched;
//! 3. the operation runs **independently** in each repository;
//! 4. a failing repository never prevents the others from completing;
//! 5. the result is a [`OperationReport`] that distinguishes success, skipped,
//!    conflict and failure for every single repository.
//!
//! Nothing is ever silently discarded: GitMesh only runs Git commands that Git itself
//! refuses to execute destructively (`checkout` with local modifications, `branch -d`
//! on unmerged branches, `pull` that would overwrite local work, ...).

pub mod branch;
pub mod commit;
pub mod push;
pub mod stage;
pub mod sync;
pub mod util;

use crate::json::Json;
use crate::model::{PhysicalRepository, RepositoryRole};

pub use branch::{branch_operation, branch_operation_observed, BranchAction, BranchOptions};
pub use commit::{commit_project, commit_project_observed, CommitOptions};
pub use push::{push_project, push_project_observed, PushOptions};
pub use stage::{stage_project, StageOptions};
pub use sync::{
    fetch_project, fetch_project_observed, pull_project, pull_project_observed, PullStrategy,
    SyncOptions,
};
pub use util::RepositorySelection;

/// Progress hooks for one logical operation.
///
/// A logical operation is a loop over physical repositories. Front ends that can be
/// busy for a while (a GUI showing "engine ... running") need to know where the loop
/// is, without reimplementing it and without the orchestrator knowing anything about
/// widgets, terminals or sockets. This type is that seam: it is deliberately tiny,
/// front-end agnostic, and silent by default.
#[derive(Default)]
pub struct OperationObserver<'a> {
    on_repository_start: Option<&'a mut dyn FnMut(&PhysicalRepository)>,
    on_repository_end: Option<&'a mut dyn FnMut(&RepoOutcome)>,
}

impl<'a> OperationObserver<'a> {
    /// An observer that does nothing (used by front ends that do not show progress).
    pub fn silent() -> Self {
        OperationObserver::default()
    }

    /// Observer notified before a repository is worked on.
    pub fn on_start<F>(mut self, f: &'a mut F) -> Self
    where
        F: FnMut(&PhysicalRepository),
    {
        self.on_repository_start = Some(f);
        self
    }

    /// Observer notified after a repository has produced its outcome.
    pub fn on_end<F>(mut self, f: &'a mut F) -> Self
    where
        F: FnMut(&RepoOutcome),
    {
        self.on_repository_end = Some(f);
        self
    }

    /// Called by the orchestration loop before touching a repository.
    pub fn repository_started(&mut self, repo: &PhysicalRepository) {
        if let Some(f) = self.on_repository_start.as_mut() {
            f(repo);
        }
    }

    /// Called by the orchestration loop once a repository has an outcome.
    pub fn repository_finished(&mut self, outcome: &RepoOutcome) {
        if let Some(f) = self.on_repository_end.as_mut() {
            f(outcome);
        }
    }
}

/// Result of one repository's part of a logical operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeKind {
    /// The operation did something meaningful in this repository.
    Success,
    /// The repository had nothing to do.
    Skipped,
    /// The operation stopped because of a merge conflict.
    Conflict,
    /// The operation could not be completed in this repository.
    Failed,
}

impl OutcomeKind {
    /// Symbol used in the CLI summary.
    pub fn symbol(self) -> &'static str {
        match self {
            OutcomeKind::Success => "✓",
            OutcomeKind::Skipped => "-",
            OutcomeKind::Conflict => "!",
            OutcomeKind::Failed => "✗",
        }
    }

    /// Stable machine-readable label.
    pub fn label(self) -> &'static str {
        match self {
            OutcomeKind::Success => "success",
            OutcomeKind::Skipped => "skipped",
            OutcomeKind::Conflict => "conflict",
            OutcomeKind::Failed => "failed",
        }
    }

    pub fn is_problem(self) -> bool {
        matches!(self, OutcomeKind::Conflict | OutcomeKind::Failed)
    }
}

/// Outcome of a logical operation in one physical repository.
#[derive(Debug, Clone)]
pub struct RepoOutcome {
    /// Logical repository id.
    pub id: String,
    /// Root or external.
    pub role: RepositoryRole,
    /// Project-relative path of the repository.
    pub path: String,
    /// Classification of the outcome.
    pub kind: OutcomeKind,
    /// One-line explanation, shown in the summary.
    pub summary: String,
    /// Extra lines shown when the user asks for details or when something failed.
    pub details: Vec<String>,
}

impl RepoOutcome {
    pub fn new(
        id: impl Into<String>,
        role: RepositoryRole,
        path: impl Into<String>,
        kind: OutcomeKind,
        summary: impl Into<String>,
    ) -> Self {
        RepoOutcome {
            id: id.into(),
            role,
            path: path.into(),
            kind,
            summary: summary.into(),
            details: Vec::new(),
        }
    }

    /// Add a detail line.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.details.push(detail.into());
        self
    }

    /// Add several detail lines.
    pub fn with_details<I, S>(mut self, details: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.details.extend(details.into_iter().map(Into::into));
        self
    }

    pub fn is_problem(&self) -> bool {
        self.kind.is_problem()
    }

    /// One summary line, e.g. `root        ✓ committed 3 file(s) [a1b2c3d]`.
    pub fn line(&self) -> String {
        format!("{:<12} {} {}", self.id, self.kind.symbol(), self.summary)
    }

    fn to_json(&self) -> Json {
        Json::object([
            ("id", Json::from(self.id.clone())),
            ("role", Json::from(self.role.label())),
            ("path", Json::from(self.path.clone())),
            ("outcome", Json::from(self.kind.label())),
            ("summary", Json::from(self.summary.clone())),
            (
                "details",
                Json::array(self.details.iter().map(|d| Json::from(d.as_str()))),
            ),
        ])
    }
}

/// Report of a logical operation across the whole project.
#[derive(Debug, Clone)]
pub struct OperationReport {
    /// Operation name (`commit`, `push`, `pull`, `fetch`, `branch`, `checkout`, ...).
    pub operation: String,
    /// True when nothing was actually changed (`--dry-run`).
    pub dry_run: bool,
    /// One outcome per repository, in project order.
    pub outcomes: Vec<RepoOutcome>,
}

impl OperationReport {
    pub fn new(operation: impl Into<String>, dry_run: bool, outcomes: Vec<RepoOutcome>) -> Self {
        OperationReport {
            operation: operation.into(),
            dry_run,
            outcomes,
        }
    }

    pub fn success(&self) -> impl Iterator<Item = &RepoOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.kind == OutcomeKind::Success)
    }

    pub fn skipped(&self) -> impl Iterator<Item = &RepoOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.kind == OutcomeKind::Skipped)
    }

    pub fn conflicts(&self) -> impl Iterator<Item = &RepoOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.kind == OutcomeKind::Conflict)
    }

    pub fn failures(&self) -> impl Iterator<Item = &RepoOutcome> {
        self.outcomes
            .iter()
            .filter(|o| o.kind == OutcomeKind::Failed)
    }

    pub fn problems(&self) -> impl Iterator<Item = &RepoOutcome> {
        self.outcomes.iter().filter(|o| o.is_problem())
    }

    /// True when every repository either succeeded or had nothing to do.
    pub fn is_success(&self) -> bool {
        self.outcomes.iter().all(|o| !o.is_problem())
    }

    /// True when the operation is only partly successful.
    pub fn is_partial(&self) -> bool {
        let mut problems = self.problems();
        let some_problem = problems.next().is_some();
        some_problem && self.outcomes.iter().any(|o| o.kind == OutcomeKind::Success)
    }

    /// Count of repositories in each state.
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        (
            self.success().count(),
            self.skipped().count(),
            self.conflicts().count(),
            self.failures().count(),
        )
    }

    /// Exit code the CLI should use: 0 when everything worked, 1 when there were
    /// problems.
    pub fn exit_code(&self) -> i32 {
        if self.is_success() {
            0
        } else {
            1
        }
    }

    /// Human-readable summary lines (without a trailing header).
    pub fn summary_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self.outcomes.iter().map(RepoOutcome::line).collect();
        let (ok, skipped, conflicts, failed) = self.counts();
        lines.push(String::new());
        lines.push(format!(
            "{}: {ok} succeeded, {skipped} skipped, {conflicts} conflicted, {failed} failed",
            self.operation
        ));
        if self.is_partial() {
            lines.push(
                "some repositories completed and some did not: the project is in an \
                 intentionally partial state, review the lines marked ! and ✗"
                    .to_string(),
            );
        }
        lines
    }

    /// Detail lines for every outcome that carries them.
    pub fn detail_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for outcome in &self.outcomes {
            if outcome.details.is_empty() {
                continue;
            }
            lines.push(format!("{} ({}):", outcome.id, outcome.path));
            for detail in &outcome.details {
                lines.push(format!("  {detail}"));
            }
        }
        lines
    }

    /// Machine-readable representation.
    pub fn to_json(&self) -> Json {
        Json::object([
            ("operation", Json::from(self.operation.clone())),
            ("dry_run", Json::from(self.dry_run)),
            (
                "outcomes",
                Json::array(self.outcomes.iter().map(RepoOutcome::to_json)),
            ),
            (
                "summary",
                Json::object([
                    ("succeeded", Json::from(self.success().count())),
                    ("skipped", Json::from(self.skipped().count())),
                    ("conflicted", Json::from(self.conflicts().count())),
                    ("failed", Json::from(self.failures().count())),
                ]),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_classifies_outcomes() {
        let report = OperationReport::new(
            "commit",
            false,
            vec![
                RepoOutcome::new(
                    "root",
                    RepositoryRole::Root,
                    ".",
                    OutcomeKind::Success,
                    "committed 3 file(s)",
                ),
                RepoOutcome::new(
                    "engine",
                    RepositoryRole::External,
                    "engine",
                    OutcomeKind::Skipped,
                    "nothing to commit",
                ),
                RepoOutcome::new(
                    "renderer",
                    RepositoryRole::External,
                    "renderer",
                    OutcomeKind::Conflict,
                    "2 conflicted file(s)",
                ),
                RepoOutcome::new(
                    "tools",
                    RepositoryRole::External,
                    "tools",
                    OutcomeKind::Failed,
                    "push rejected",
                ),
            ],
        );
        assert_eq!(report.counts(), (1, 1, 1, 1));
        assert!(!report.is_success());
        assert!(report.is_partial());
        assert_eq!(report.exit_code(), 1);
        assert_eq!(report.problems().count(), 2);
    }

    #[test]
    fn fully_successful_report_is_success() {
        let report = OperationReport::new(
            "push",
            false,
            vec![
                RepoOutcome::new(
                    "root",
                    RepositoryRole::Root,
                    ".",
                    OutcomeKind::Success,
                    "pushed",
                ),
                RepoOutcome::new(
                    "engine",
                    RepositoryRole::External,
                    "engine",
                    OutcomeKind::Skipped,
                    "nothing to push",
                ),
            ],
        );
        assert!(report.is_success());
        assert!(!report.is_partial());
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn summary_lines_use_the_documented_symbols() {
        let report = OperationReport::new(
            "push",
            false,
            vec![
                RepoOutcome::new(
                    "root",
                    RepositoryRole::Root,
                    ".",
                    OutcomeKind::Success,
                    "pushed",
                ),
                RepoOutcome::new(
                    "renderer",
                    RepositoryRole::External,
                    "renderer",
                    OutcomeKind::Failed,
                    "rejected",
                ),
            ],
        );
        let lines = report.summary_lines();
        assert!(lines[0].starts_with("root"), "{}", lines[0]);
        assert!(lines[0].contains("✓ pushed"));
        assert!(lines[1].contains("✗ rejected"));
        assert!(lines
            .iter()
            .any(|l| l.contains("1 succeeded, 0 skipped, 0 conflicted, 1 failed")));
    }

    #[test]
    fn details_are_grouped_per_repository() {
        let report = OperationReport::new(
            "commit",
            false,
            vec![RepoOutcome::new(
                "root",
                RepositoryRole::Root,
                ".",
                OutcomeKind::Success,
                "committed",
            )
            .with_detail("a1b2c3d commit message")],
        );
        let details = report.detail_lines();
        assert_eq!(details[0], "root (.):");
        assert_eq!(details[1], "  a1b2c3d commit message");
    }

    #[test]
    fn json_report_contains_each_outcome() {
        let report = OperationReport::new(
            "commit",
            true,
            vec![RepoOutcome::new(
                "root",
                RepositoryRole::Root,
                ".",
                OutcomeKind::Success,
                "committed",
            )],
        );
        let json = report.to_json().to_string();
        assert!(json.contains("\"operation\":\"commit\""));
        assert!(json.contains("\"dry_run\":true"));
        assert!(json.contains("\"outcome\":\"success\""));
    }
}
