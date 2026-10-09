//! Terminal UI state machine.
//!
//! The terminal interface is a minimal everyday workflow for one GitMesh project:
//!
//! * see the project, its branch and every changed file grouped by the repository that
//!   owns it;
//! * stage all changes (repository-scoped, see [`crate::ops::stage_project`]);
//! * write one commit message and commit it to every repository that has staged work;
//! * pull and push, with a per-repository result.
//!
//! Repository creation, renaming, remote configuration, manifest editing and branch
//! management are deliberately **not** here: the CLI and the GUI own those.
//!
//! This module contains no Git logic and no rendering. Keys go in through [`App::handle_key`],
//! which returns the [`Action`] (if any) to run; [`App::run_action`] executes it through
//! the same core operations the CLI uses. Keeping rendering out lets the tests drive whole
//! workflows without a terminal.

use std::path::{Path, PathBuf};

use crate::analyzer::{Analyzer, ProjectStatus};
use crate::error::Result;
use crate::git::{ChangeKind, GitRunner};
use crate::manifest;
use crate::model::{GitMeshProject, RepositoryRole};
use crate::ops::sync::SyncOptions;
use crate::ops::{
    self, CommitOptions, OperationReport, PushOptions, RepositorySelection, StageOptions,
};

/// Smallest terminal the interface draws in. Below this it shows a resize notice.
pub const MIN_WIDTH: u16 = 56;
/// Smallest terminal height the interface draws in.
pub const MIN_HEIGHT: u16 = 14;

/// A key, independent of the terminal library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    CtrlC,
}

/// An operation the user asked for. Each one maps to one core operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    StageAll,
    Commit,
    Pull,
    Push,
    Refresh,
}

impl Action {
    /// Short name shown while the operation runs and in its result title.
    pub fn label(self) -> &'static str {
        match self {
            Action::StageAll => "Stage all",
            Action::Commit => "Commit",
            Action::Pull => "Pull",
            Action::Push => "Push",
            Action::Refresh => "Refresh",
        }
    }
}

/// Where keyboard focus is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// Typing the commit message.
    Message,
    /// Choosing one of the action buttons (←/→, Enter) or using the letter shortcuts.
    Actions,
}

/// One visible action button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Button {
    pub action: Action,
    /// The key that does the same thing from the action bar.
    pub key: char,
}

/// Action buttons, in display order. The footer and help are built from the same table.
pub const BUTTONS: [Button; 4] = [
    Button {
        action: Action::StageAll,
        key: 's',
    },
    Button {
        action: Action::Commit,
        key: 'c',
    },
    Button {
        action: Action::Pull,
        key: 'p',
    },
    Button {
        action: Action::Push,
        key: 'P',
    },
];

/// Every keyboard shortcut the interface advertises, as (keys, description).
pub const SHORTCUTS: [(&str, &str); 9] = [
    ("s", "stage all"),
    ("c", "commit"),
    ("p / P", "pull / push"),
    ("Tab", "message ⇄ actions"),
    ("↑ ↓ PgUp PgDn", "scroll changes"),
    ("r", "refresh"),
    ("d", "dry run on/off"),
    ("?", "this help"),
    ("q", "quit"),
];

/// One line of the grouped changes list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeLine {
    /// A repository heading with its counts.
    Repository {
        id: String,
        role: RepositoryRole,
        /// Project-relative path ("." for the root).
        path: String,
        changes: usize,
        staged: usize,
        /// The repository's problem, if any (conflict, operation in progress, ...).
        note: Option<String>,
    },
    /// One changed file.
    File {
        /// Two-character git-style code: index then work tree (`M `, ` M`, `A `, `??`, ...).
        code: String,
        path: String,
        staged: bool,
    },
}

/// Result of the last operation, shown in the result panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultView {
    pub title: String,
    /// One line per repository, plus detail lines.
    pub lines: Vec<String>,
    /// True when at least one repository needs attention.
    pub problems: bool,
}

/// The application state.
pub struct App {
    pub runner: GitRunner,
    /// Directory the project was opened from (for refresh).
    pub start_dir: PathBuf,
    pub project: Option<GitMeshProject>,
    pub status: Option<ProjectStatus>,
    /// The grouped change list, derived from `status`.
    pub changes: Vec<ChangeLine>,
    /// Index of the first visible change line.
    pub scroll: usize,
    /// Commit message being written.
    pub message: String,
    pub focus: Focus,
    /// Selected action button (index into [`BUTTONS`]).
    pub button: usize,
    pub dry_run: bool,
    pub show_help: bool,
    pub quit: bool,
    /// Last operation's result.
    pub result: Option<ResultView>,
    /// True after Commit was pressed once: the next Commit/Enter confirms it.
    pub confirm_commit: bool,
    /// Operation currently running (set only while [`run_action`](Self::run_action) runs).
    pub busy: Option<Action>,
    /// Message shown in the header when the project cannot be opened.
    pub open_error: Option<String>,
}

impl App {
    /// Create the application, opening the project at or above `start`.
    pub fn new(start: &Path, dry_run: bool) -> Result<App> {
        let runner = GitRunner::detect()?;
        let start = crate::paths::lexical_normalize(start);
        let mut app = App {
            runner,
            start_dir: start.clone(),
            project: None,
            status: None,
            changes: Vec::new(),
            scroll: 0,
            message: String::new(),
            focus: Focus::Actions,
            button: 0,
            dry_run,
            show_help: false,
            quit: false,
            result: None,
            confirm_commit: false,
            busy: None,
            open_error: None,
        };
        match manifest::find_project_root(&start) {
            Some(root) => match manifest::load_from_root(&root) {
                Ok(project) => {
                    app.start_dir = project.root.clone();
                    app.project = Some(project);
                    app.refresh()?;
                }
                Err(err) => app.open_error = Some(err.to_string()),
            },
            None => {
                app.open_error = Some(format!(
                    "no GitMesh project found at or above {}",
                    start.display()
                ));
            }
        }
        Ok(app)
    }

    /// Re-read the project manifest and the status of every repository.
    pub fn refresh(&mut self) -> Result<()> {
        if let Some(root) = self.project.as_ref().map(|p| p.root.clone()) {
            if let Ok(project) = manifest::load_from_root(&root) {
                self.project = Some(project);
            }
        }
        if let Some(project) = &self.project {
            let analyzer = Analyzer::new(project, &self.runner);
            self.status = Some(analyzer.analyze());
        }
        self.rebuild_changes();
        Ok(())
    }

    fn rebuild_changes(&mut self) {
        self.changes.clear();
        let Some(status) = &self.status else {
            return;
        };
        for repo in &status.repositories {
            let entries: Vec<_> = repo
                .status
                .as_ref()
                .map(|s| s.entries.iter().filter(|e| !e.ignored).collect())
                .unwrap_or_default();
            // The root never lists files owned by an external repository.
            let external_excluded = |path: &str| -> bool {
                repo.role == RepositoryRole::Root
                    && self.project.as_ref().is_some_and(|p| {
                        crate::ops::util::relative_owned_by_external(p, path).is_some()
                    })
            };
            let files: Vec<_> = entries
                .into_iter()
                .filter(|e| !external_excluded(&e.path))
                .collect();
            let staged = files.iter().filter(|e| e.staged).count();
            let note = if repo.has_conflicts() {
                Some("has conflicts".to_string())
            } else if let Some(op) = repo.in_progress {
                Some(format!("{} in progress", op.label()))
            } else if repo.status.is_none() {
                Some("status unavailable".to_string())
            } else {
                None
            };
            self.changes.push(ChangeLine::Repository {
                id: repo.id.clone(),
                role: repo.role,
                path: repo.relative_path.clone(),
                changes: files.len(),
                staged,
                note,
            });
            for entry in files {
                self.changes.push(ChangeLine::File {
                    code: two_letter_code(entry),
                    path: entry.path.clone(),
                    staged: entry.staged,
                });
            }
        }
        if self.scroll >= self.changes.len() {
            self.scroll = self.changes.len().saturating_sub(1);
        }
    }

    /// Total number of changed files across the project.
    pub fn change_count(&self) -> usize {
        self.changes
            .iter()
            .filter(|l| matches!(l, ChangeLine::File { .. }))
            .count()
    }

    /// Number of staged files and of repositories that have staged files.
    pub fn staged_summary(&self) -> (usize, usize) {
        let mut files = 0;
        let mut repos = 0;
        for line in &self.changes {
            match line {
                ChangeLine::Repository { staged, .. } => {
                    if *staged > 0 {
                        repos += 1;
                    }
                    files += staged;
                }
                ChangeLine::File { .. } => {}
            }
        }
        (files, repos)
    }

    /// Branch of the project (the root's branch), with the outliers spelled out.
    pub fn header_branch(&self) -> String {
        let Some(status) = &self.status else {
            return "-".to_string();
        };
        let Some(reference) = status.reference_branch() else {
            return "-".to_string();
        };
        let outliers = status.inconsistent_branches();
        if outliers.is_empty() {
            return reference;
        }
        let detail = outliers
            .iter()
            .map(|state| format!("{} on {}", state.id, state.head().label()))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{reference} (differs: {detail})")
    }

    // ----------------------------------------------------------- keyboard --

    /// Apply one key. Returns the action to run, if the key asked for one.
    ///
    /// While an operation is running ([`busy`](Self::busy) is set) every key is refused.
    pub fn handle_key(&mut self, key: Key) -> Option<Action> {
        if key == Key::CtrlC {
            self.quit = true;
            return None;
        }
        if self.busy.is_some() {
            return None;
        }
        if self.show_help {
            // Any key closes the help overlay; `q` still quits.
            self.show_help = false;
            if key == Key::Char('q') {
                self.quit = true;
            }
            return None;
        }

        // A pending commit confirmation is answered by Enter/c (confirm) or Esc (cancel);
        // any other key cancels it, so a confirmation can never outlive the user's intent.
        if self.confirm_commit {
            match key {
                Key::Enter | Key::Char('c') => {
                    // `accept` consumes the flag when the commit really proceeds.
                    return Some(Action::Commit);
                }
                Key::Esc => {
                    self.confirm_commit = false;
                    self.set_notice("commit cancelled");
                    return None;
                }
                _ => {
                    self.confirm_commit = false;
                }
            }
        }

        match self.focus {
            Focus::Message => self.handle_message_key(key),
            Focus::Actions => self.handle_action_key(key),
        }
    }

    fn handle_message_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Char(c) if !c.is_control() => {
                self.message.push(c);
                None
            }
            Key::Backspace => {
                self.message.pop();
                None
            }
            Key::Tab | Key::Esc => {
                self.focus = Focus::Actions;
                None
            }
            Key::BackTab => {
                self.focus = Focus::Actions;
                None
            }
            Key::Enter => Some(Action::Commit),
            Key::Down | Key::PageDown => {
                self.scroll_by(1);
                None
            }
            Key::Up | Key::PageUp => {
                self.scroll_by(-1);
                None
            }
            _ => None,
        }
    }

    fn handle_action_key(&mut self, key: Key) -> Option<Action> {
        match key {
            Key::Char('?') => {
                self.show_help = true;
                None
            }
            Key::Char('q') => {
                self.quit = true;
                None
            }
            Key::Char('r') => Some(Action::Refresh),
            Key::Char('d') => {
                self.dry_run = !self.dry_run;
                self.set_notice(if self.dry_run {
                    "dry run on: operations report what they would do and change nothing"
                } else {
                    "dry run off: operations change repositories"
                });
                None
            }
            Key::Char('s') => Some(Action::StageAll),
            Key::Char('c') => Some(Action::Commit),
            Key::Char('p') => Some(Action::Pull),
            Key::Char('P') => Some(Action::Push),
            Key::Tab | Key::BackTab => {
                self.focus = Focus::Message;
                None
            }
            Key::Left => {
                self.button = (self.button + BUTTONS.len() - 1) % BUTTONS.len();
                None
            }
            Key::Right => {
                self.button = (self.button + 1) % BUTTONS.len();
                None
            }
            Key::Enter => Some(BUTTONS[self.button].action),
            Key::Up => {
                self.scroll_by(-1);
                None
            }
            Key::Down => {
                self.scroll_by(1);
                None
            }
            Key::PageUp => {
                self.scroll_by(-10);
                None
            }
            Key::PageDown => {
                self.scroll_by(10);
                None
            }
            _ => None,
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.changes.len().saturating_sub(1) as isize;
        let next = (self.scroll as isize + delta).clamp(0, max.max(0));
        self.scroll = next as usize;
    }

    fn set_notice(&mut self, text: &str) {
        self.result = Some(ResultView {
            title: text.to_string(),
            lines: Vec::new(),
            problems: false,
        });
    }

    /// Decide what happens for an action requested by the user. Returns the action to run
    /// now, or `None` when the request was refused or needs confirmation first.
    ///
    /// This is separate from [`run_action`](Self::run_action) so that the checks are
    /// visible in tests and so the event loop can draw "working…" before the operation.
    pub fn accept(&mut self, action: Action) -> Option<Action> {
        if self.project.is_none() {
            self.set_notice("no project is open: run `gitmesh init` in your project first");
            return None;
        }
        match action {
            Action::Commit => {
                if self.message.trim().is_empty() {
                    self.focus = Focus::Message;
                    self.set_notice("write a commit message first");
                    return None;
                }
                let (files, repos) = self.staged_summary();
                if files == 0 {
                    self.set_notice(
                        "nothing is staged: press s (Stage all) to stage the changes first",
                    );
                    return None;
                }
                if !self.confirm_commit {
                    self.confirm_commit = true;
                    self.set_notice(&format!(
                        "press Enter or c to commit {files} staged file(s) in {repos} repositor{} with this message, Esc to cancel",
                        if repos == 1 { "y" } else { "ies" }
                    ));
                    return None;
                }
                self.confirm_commit = false;
                Some(Action::Commit)
            }
            other => Some(other),
        }
    }

    /// Apply one key and return the action that is ready to run, if any.
    ///
    /// This is the single entry point the event loop uses: [`handle_key`](Self::handle_key)
    /// followed by [`accept`](Self::accept), so the checks (message, staging, confirmation)
    /// always apply.
    pub fn dispatch(&mut self, key: Key) -> Option<Action> {
        let requested = self.handle_key(key)?;
        self.accept(requested)
    }

    /// Run one accepted action through the core and store its result.
    pub fn run_action(&mut self, action: Action) -> Result<()> {
        self.busy = Some(action);
        let outcome = self.execute(action);
        self.busy = None;
        outcome
    }

    fn execute(&mut self, action: Action) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let report = match action {
            Action::Refresh => {
                self.result = None;
                return self.refresh();
            }
            Action::StageAll => ops::stage_project(
                &project,
                &self.runner,
                &StageOptions {
                    selection: RepositorySelection::All,
                    dry_run: self.dry_run,
                },
            )?,
            Action::Commit => {
                let options = CommitOptions {
                    selection: RepositorySelection::All,
                    dry_run: self.dry_run,
                    ..CommitOptions::staged(self.message.clone())
                };
                let report = ops::commit_project(&project, &self.runner, &options)?;
                if !self.dry_run && report.is_success() && report.counts().0 > 0 {
                    self.message.clear();
                }
                report
            }
            Action::Pull => {
                let mut options = SyncOptions::new();
                options.dry_run = self.dry_run;
                ops::pull_project(&project, &self.runner, &options)?
            }
            Action::Push => {
                let options = PushOptions {
                    selection: RepositorySelection::All,
                    dry_run: self.dry_run,
                    set_upstream: true,
                    default_remote: "origin".to_string(),
                };
                ops::push_project(&project, &self.runner, &options)?
            }
        };
        self.result = Some(result_view(action, &report));
        self.refresh()
    }

    /// Convenience for tests and simple callers: accept and run in one step.
    pub fn press(&mut self, action: Action) -> Result<()> {
        if let Some(accepted) = self.accept(action) {
            self.run_action(accepted)?;
        }
        Ok(())
    }

    /// True when the last operation had problems (for the exit state of the UI).
    pub fn last_operation_had_problems(&self) -> bool {
        self.result.as_ref().is_some_and(|r| r.problems)
    }
}

/// Two-character git-style code for a status entry.
fn two_letter_code(entry: &crate::git::StatusEntry) -> String {
    if entry.untracked {
        return "??".to_string();
    }
    if entry.unmerged.is_some() {
        return "UU".to_string();
    }
    let letter = |kind: Option<ChangeKind>| match kind {
        Some(ChangeKind::Added) => 'A',
        Some(ChangeKind::Modified) => 'M',
        Some(ChangeKind::Deleted) => 'D',
        Some(ChangeKind::Renamed) => 'R',
        Some(ChangeKind::Copied) => 'C',
        Some(ChangeKind::TypeChanged) => 'T',
        Some(ChangeKind::Unmerged) => 'U',
        _ => ' ',
    };
    format!("{}{}", letter(entry.index), letter(entry.worktree))
}

/// Build the result panel content for one operation's report.
pub fn result_view(action: Action, report: &OperationReport) -> ResultView {
    let (ok, skipped, conflict, failed) = report.counts();
    let problems_count = conflict + failed;
    let total = report.outcomes.len();
    let problems = problems_count > 0;

    let verdict = if report.dry_run {
        "dry run: nothing was changed".to_string()
    } else if problems {
        format!(
            "NOT everything succeeded: {problems_count} of {total} repositor{} need attention",
            if total == 1 { "y" } else { "ies" }
        )
    } else if ok == 0 {
        "nothing to do".to_string()
    } else {
        format!(
            "done in {ok} repositor{}",
            if ok == 1 { "y" } else { "ies" }
        )
    };
    let title = format!("{}: {verdict}", action.label());

    let mut lines = Vec::new();
    for outcome in &report.outcomes {
        lines.push(format!(
            "{} {}  {}",
            outcome.kind.symbol(),
            outcome.id,
            outcome.summary
        ));
        for detail in &outcome.details {
            lines.push(format!("    {detail}"));
        }
    }
    if skipped > 0 && ok + problems_count == 0 {
        lines.push("skipped repositories are not failures: they have no remote to use".to_string());
    }
    ResultView {
        title,
        lines,
        problems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_advertised_button_key_is_bound() {
        // The action bar advertises these keys; each must map to its action.
        for button in BUTTONS {
            let mut app = fake_app();
            let action = app.handle_key(Key::Char(button.key));
            assert_eq!(action, Some(button.action), "key {}", button.key);
        }
    }

    #[test]
    fn every_advertised_shortcut_has_a_binding() {
        for (keys, _) in SHORTCUTS
            .iter()
            .filter(|(k, _)| k.len() == 1 && !k.contains(' '))
        {
            let c = keys.chars().next().unwrap();
            let mut app = fake_app();
            app.show_help = false;
            // `q` quits, `?` opens help; everything else returns an action or changes state.
            let before = (app.dry_run, app.show_help, app.quit);
            let action = app.handle_key(Key::Char(c));
            let after = (app.dry_run, app.show_help, app.quit);
            assert!(
                action.is_some() || before != after,
                "advertised key '{c}' does nothing"
            );
        }
    }

    pub(crate) fn fake_app() -> App {
        App {
            runner: GitRunner::detect().expect("git is installed for tests"),
            start_dir: PathBuf::from("."),
            project: None,
            status: None,
            changes: Vec::new(),
            scroll: 0,
            message: String::new(),
            focus: Focus::Actions,
            button: 0,
            dry_run: false,
            show_help: false,
            quit: false,
            result: None,
            confirm_commit: false,
            busy: None,
            open_error: None,
        }
    }
}

#[cfg(test)]
mod workflow_tests {
    //! Key-driven workflows over real temporary repositories.
    use super::*;
    use crate::testkit::RepoFixture;

    /// Open the fixture project in the state machine.
    fn open(fixture: &RepoFixture, dry_run: bool) -> App {
        App::new(fixture.path(), dry_run).expect("open project")
    }

    /// Press one key and run whatever it asks for, as the event loop does.
    fn press(app: &mut App, key: Key) {
        if let Some(action) = app.dispatch(key) {
            app.run_action(action).expect("operation");
        }
    }

    /// Focus the message field, type, and leave it again (as a user would).
    fn type_text(app: &mut App, text: &str) {
        press(app, Key::Tab);
        for c in text.chars() {
            press(app, Key::Char(c));
        }
        press(app, Key::Tab);
    }

    fn commits(fixture: &RepoFixture, repo: &str) -> usize {
        fixture
            .git_ok(repo, &["rev-list", "--count", "HEAD"])
            .trim()
            .parse()
            .unwrap()
    }

    /// A project with a root and one nested repository, both with uncommitted changes.
    fn two_repo_fixture() -> RepoFixture {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("README.md", "root\n");
        fixture.write("engine/src/lib.rs", "engine\n");
        fixture
    }

    #[test]
    fn stage_all_then_commit_commits_each_repository_with_one_message() {
        let fixture = two_repo_fixture();
        let before_root = commits(&fixture, ".");
        let before_engine = commits(&fixture, "engine");
        let mut app = open(&fixture, false);
        assert_eq!(app.change_count(), 2, "{:?}", app.changes);
        press(&mut app, Key::Char('s'));
        assert_eq!(
            app.staged_summary().1,
            2,
            "both repositories have staged work"
        );

        type_text(&mut app, "tidy");
        press(&mut app, Key::Char('c'));
        assert!(app.confirm_commit, "commit asks for confirmation first");
        assert_eq!(
            commits(&fixture, "engine"),
            before_engine,
            "nothing committed before confirming"
        );

        press(&mut app, Key::Enter);
        assert!(!app.confirm_commit);
        assert_eq!(commits(&fixture, "engine"), before_engine + 1);
        assert_eq!(commits(&fixture, "."), before_root + 1);
        assert_eq!(
            fixture
                .git_ok("engine", &["log", "-1", "--pretty=%s"])
                .trim(),
            "tidy"
        );
        assert_eq!(
            fixture.git_ok(".", &["log", "-1", "--pretty=%s"]).trim(),
            "tidy"
        );
        let result = app.result.as_ref().unwrap();
        assert!(!result.problems, "{result:?}");
        assert!(
            result.title.starts_with("Commit: done in 2 repositories"),
            "{}",
            result.title
        );
        assert!(
            app.message.is_empty(),
            "the message is cleared after a successful commit"
        );
    }

    #[test]
    fn an_empty_message_never_commits() {
        let fixture = two_repo_fixture();
        let before = commits(&fixture, "engine");
        let mut app = open(&fixture, false);
        press(&mut app, Key::Char('s'));
        press(&mut app, Key::Char('c'));
        assert!(!app.confirm_commit);
        assert_eq!(
            app.focus,
            Focus::Message,
            "the empty field is where the user must go"
        );
        press(&mut app, Key::Enter);
        assert_eq!(commits(&fixture, "engine"), before);
        assert!(app
            .result
            .as_ref()
            .unwrap()
            .title
            .contains("write a commit message"));

        // Whitespace only is still empty.
        type_text(&mut app, "   ");
        press(&mut app, Key::Char('c'));
        assert!(!app.confirm_commit);
        assert_eq!(commits(&fixture, "engine"), before);
    }

    #[test]
    fn commit_without_staged_changes_explains_how_to_proceed() {
        let fixture = two_repo_fixture();
        let before = commits(&fixture, "engine");
        let mut app = open(&fixture, false);
        type_text(&mut app, "nothing staged yet");
        press(&mut app, Key::Char('c'));
        assert!(!app.confirm_commit);
        let title = &app.result.as_ref().unwrap().title;
        assert!(title.contains("nothing is staged"), "{title}");
        assert_eq!(commits(&fixture, "engine"), before);
    }

    #[test]
    fn cancelling_or_any_other_key_drops_the_pending_commit() {
        let fixture = two_repo_fixture();
        let before = commits(&fixture, "engine");
        let mut app = open(&fixture, false);
        press(&mut app, Key::Char('s'));
        type_text(&mut app, "maybe");
        press(&mut app, Key::Char('c'));
        assert!(app.confirm_commit);
        press(&mut app, Key::Esc);
        assert!(!app.confirm_commit);
        assert_eq!(commits(&fixture, "engine"), before);

        // Any non-confirming key cancels too, so the confirmation cannot go stale.
        press(&mut app, Key::Char('c'));
        assert!(app.confirm_commit);
        press(&mut app, Key::Right);
        assert!(!app.confirm_commit);
        assert_eq!(commits(&fixture, "engine"), before);
    }

    #[test]
    fn a_conflicted_repository_is_reported_and_never_called_a_success() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "src/shared.txt");
        fixture.write("README.md", "root\n");
        let before_root = commits(&fixture, ".");
        let before_engine = commits(&fixture, "engine");
        let mut app = open(&fixture, false);

        press(&mut app, Key::Char('s'));
        let result = app.result.clone().unwrap();
        assert!(result.problems);
        assert!(
            result.title.contains("NOT everything succeeded"),
            "{}",
            result.title
        );
        assert!(
            result.lines.iter().any(|l| l.starts_with("! engine")),
            "{:?}",
            result.lines
        );

        type_text(&mut app, "partial");
        press(&mut app, Key::Char('c'));
        press(&mut app, Key::Enter);
        let result = app.result.clone().unwrap();
        assert!(result.problems);
        assert!(
            result.title.contains("NOT everything succeeded"),
            "{}",
            result.title
        );
        assert!(
            result.lines.iter().any(|l| l.starts_with("! engine")),
            "{:?}",
            result.lines
        );
        // The root committed its staged file; the conflicted repository did not.
        assert_eq!(commits(&fixture, "."), before_root + 1);
        assert_eq!(commits(&fixture, "engine"), before_engine);
    }

    #[test]
    fn a_local_only_repository_is_skipped_for_push_not_failed() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.write_and_commit("README.md", "root commit\n");
        let mut app = open(&fixture, false);

        press(&mut app, Key::Char('P'));
        let result = app.result.clone().unwrap();
        assert!(!result.problems, "{result:?}");
        assert!(
            result
                .lines
                .iter()
                .any(|l| l.starts_with("- engine") && l.contains("no remote")),
            "{:?}",
            result.lines
        );
    }

    #[test]
    fn a_failed_push_next_to_a_successful_one_is_a_partial_failure() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.publish("engine", "engine.git");
        fixture.write_and_commit("README.md", "root\n");
        fixture.write_and_commit("engine/src/lib.rs", "engine\n");
        // The engine's remote disappears: its push must fail and be reported as such.
        fixture.git_ok(
            "engine",
            &["remote", "set-url", "origin", "/nonexistent/gone.git"],
        );
        let mut app = open(&fixture, false);

        press(&mut app, Key::Char('P'));
        let result = app.result.clone().unwrap();
        assert!(result.problems, "{result:?}");
        assert!(
            result.title.contains("NOT everything succeeded"),
            "{}",
            result.title
        );
        assert!(
            result.lines.iter().any(|l| l.starts_with("✓ root")),
            "{:?}",
            result.lines
        );
        assert!(
            result.lines.iter().any(|l| l.starts_with("✗ engine")),
            "{:?}",
            result.lines
        );
    }

    #[test]
    fn dry_run_commit_changes_nothing() {
        let fixture = two_repo_fixture();
        let before = commits(&fixture, "engine");
        let mut app = open(&fixture, false);
        press(&mut app, Key::Char('s'));
        let staged_before = fixture.git_ok("engine", &["diff", "--cached", "--name-only"]);

        press(&mut app, Key::Char('d'));
        assert!(app.dry_run);
        type_text(&mut app, "dry");
        press(&mut app, Key::Char('c'));
        press(&mut app, Key::Enter);
        let result = app.result.clone().unwrap();
        assert!(
            result.title.starts_with("Commit: dry run"),
            "{}",
            result.title
        );
        assert!(!result.problems);
        assert_eq!(commits(&fixture, "engine"), before);
        assert_eq!(
            fixture.git_ok("engine", &["diff", "--cached", "--name-only"]),
            staged_before
        );
    }

    #[test]
    fn dry_run_stage_all_changes_nothing() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, true);
        press(&mut app, Key::Char('s'));
        let result = app.result.clone().unwrap();
        assert!(
            result.title.starts_with("Stage all: dry run"),
            "{}",
            result.title
        );
        assert_eq!(app.staged_summary().0, 0);
        assert!(fixture
            .git_ok("engine", &["diff", "--cached", "--name-only"])
            .trim()
            .is_empty());
    }

    #[test]
    fn toggling_dry_run_is_visible_state() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, false);
        press(&mut app, Key::Char('d'));
        assert!(app.dry_run);
        press(&mut app, Key::Char('d'));
        assert!(!app.dry_run);
    }

    #[test]
    fn keys_are_refused_while_an_operation_is_running() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, false);
        app.busy = Some(Action::Push);
        assert_eq!(app.dispatch(Key::Char('s')), None);
        assert_eq!(app.dispatch(Key::Char('q')), None);
        assert!(!app.quit);
        assert_eq!(app.staged_summary().0, 0);
    }

    #[test]
    fn typing_in_the_message_never_triggers_shortcuts() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, false);
        press(&mut app, Key::Tab);
        assert_eq!(app.focus, Focus::Message);
        for c in "spscpdq".chars() {
            press(&mut app, Key::Char(c));
        }
        assert_eq!(app.message, "spscpdq");
        assert!(!app.quit && !app.dry_run);
        assert_eq!(
            app.staged_summary().0,
            0,
            "letters typed in the message stage nothing"
        );
        press(&mut app, Key::Esc);
        assert_eq!(app.focus, Focus::Actions);
    }

    #[test]
    fn focus_and_buttons_navigate_without_side_effects() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, false);
        assert_eq!(app.focus, Focus::Actions);
        press(&mut app, Key::Right);
        press(&mut app, Key::Right);
        assert_eq!(BUTTONS[app.button].action, Action::Pull);
        press(&mut app, Key::Left);
        press(&mut app, Key::Left);
        press(&mut app, Key::Left);
        assert_eq!(
            BUTTONS[app.button].action,
            Action::Push,
            "left wraps around"
        );
        assert_eq!(app.staged_summary().0, 0);
    }

    #[test]
    fn enter_on_a_selected_button_runs_that_button() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, false);
        press(&mut app, Key::Right); // Commit
        press(&mut app, Key::Enter);
        assert!(app
            .result
            .as_ref()
            .unwrap()
            .title
            .contains("write a commit message"));
        assert_eq!(app.focus, Focus::Message);
        press(&mut app, Key::Tab);
        press(&mut app, Key::Left); // Stage all
        press(&mut app, Key::Enter);
        assert!(app.staged_summary().0 > 0);
    }

    #[test]
    fn the_change_list_groups_files_by_owning_repository() {
        let fixture = two_repo_fixture();
        let app = open(&fixture, false);
        let headers = app
            .changes
            .iter()
            .filter(|l| matches!(l, ChangeLine::Repository { .. }))
            .count();
        assert_eq!(headers, 2);
        let engine_index = app
            .changes
            .iter()
            .position(|l| matches!(l, ChangeLine::Repository { id, .. } if id == "engine"))
            .unwrap();
        match &app.changes[engine_index + 1] {
            ChangeLine::File { code, path, staged } => {
                assert_eq!(code, "??");
                assert_eq!(path, "src/lib.rs");
                assert!(!staged);
            }
            other => panic!("expected a file, got {other:?}"),
        }
    }

    #[test]
    fn help_closes_on_any_key_and_q_quits_only_from_the_main_screen() {
        let fixture = two_repo_fixture();
        let mut app = open(&fixture, false);
        press(&mut app, Key::Char('?'));
        assert!(app.show_help);
        press(&mut app, Key::Char('s'));
        assert!(!app.show_help);
        assert_eq!(
            app.staged_summary().0,
            0,
            "the key that closed help did nothing else"
        );
        press(&mut app, Key::Char('q'));
        assert!(app.quit);
    }

    #[test]
    fn opening_outside_a_project_explains_instead_of_failing() {
        let fixture = RepoFixture::new();
        let app = App::new(fixture.path(), false).unwrap();
        assert!(app.project.is_none());
        assert!(app
            .open_error
            .as_deref()
            .unwrap()
            .contains("no GitMesh project"));
    }
}
