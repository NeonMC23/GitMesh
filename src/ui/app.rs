//! Terminal UI state machine.
//!
//! The UI exists so that the user perceives **one project**: one tree, one status, one
//! commit, one pull, one push. Physical repository boundaries are visible where they
//! help (the tree shows which repository owns a directory, the status lists changes per
//! repository) but they never dominate the experience.
//!
//! This module contains no Git logic and no rendering: it is a state machine over the
//! core API ([`crate::analyzer`], [`crate::discovery`], [`crate::ops`]). Keeping it
//! free of terminal I/O means the workflows can be tested without a terminal, and it
//! guarantees the UI cannot drift away from the CLI.

use std::path::{Path, PathBuf};

use crate::analyzer::{Analyzer, ProjectStatus};
use crate::discovery::{self, DirectoryNode, ProjectScan, ScanOptions};
use crate::error::Result;
use crate::git::GitRunner;
use crate::manage;
use crate::manifest;
use crate::model::{GitMeshProject, RepositoryRole};
use crate::ops::sync::SyncOptions;
use crate::ops::{
    self, BranchAction, BranchOptions, CommitOptions, OperationReport, PushOptions,
    RepositorySelection,
};
use crate::paths::to_slash;

/// Which screen is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// No project yet: choose a directory, inspect the tree, mark repositories.
    Setup,
    /// A project is configured: tree, status and operations.
    Project,
}

/// A modal text input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Directory,
    RepositoryId,
    RemoteUrl,
    CommitMessage,
    CheckoutBranch,
    NewBranch,
    MergeBranch,
}

impl InputKind {
    /// Prompt shown above the input line.
    pub fn prompt(self) -> &'static str {
        match self {
            InputKind::Directory => "Project directory",
            InputKind::RepositoryId => "Logical repository id",
            InputKind::RemoteUrl => "Remote URL (empty clears)",
            InputKind::CommitMessage => "Commit message (applies to every affected repository)",
            InputKind::CheckoutBranch => "Branch to switch every repository to",
            InputKind::NewBranch => "New branch name",
            InputKind::MergeBranch => "Branch to merge into the current branch",
        }
    }
}

/// One visible row of the project tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeRow {
    pub depth: usize,
    pub name: String,
    pub relative_path: PathBuf,
    pub is_repository_root: bool,
    /// Configured logical repository that owns this directory.
    pub repository_id: Option<String>,
    /// True when this directory is configured as an external repository.
    pub is_external: bool,
    pub file_count: usize,
    pub truncated: bool,
}

/// What the user is looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Repository,
}

/// The application state.
pub struct App {
    pub runner: GitRunner,
    /// Directory being configured (Setup) or the project root (Project).
    pub start_dir: PathBuf,
    pub screen: Screen,
    pub project: Option<GitMeshProject>,
    pub scan: Option<ProjectScan>,
    pub rows: Vec<TreeRow>,
    pub selected: usize,
    pub status: Option<ProjectStatus>,
    /// Result of the most recent operation.
    pub report: Option<OperationReport>,
    /// Rolling message log.
    pub log: Vec<String>,
    pub mode: Option<InputKind>,
    pub input: String,
    pub dry_run: bool,
    pub show_help: bool,
    pub quit: bool,
    /// Repositories the user excluded from operations in this session.
    pub excluded: Vec<String>,
}

impl App {
    /// Create the application, opening a project when one is found at or above `start`.
    pub fn new(start: &Path, dry_run: bool) -> Result<App> {
        let runner = GitRunner::detect()?;
        let start = crate::paths::lexical_normalize(start);
        let mut app = App {
            runner,
            start_dir: start.clone(),
            screen: Screen::Setup,
            project: None,
            scan: None,
            rows: Vec::new(),
            selected: 0,
            status: None,
            report: None,
            log: Vec::new(),
            mode: None,
            input: String::new(),
            dry_run,
            show_help: false,
            quit: false,
            excluded: Vec::new(),
        };

        match manifest::find_project_root(&start)
            .and_then(|root| manifest::load_from_root(&root).ok())
        {
            Some(project) => {
                app.log(format!("opened project '{}'", project.name));
                app.set_project(project)?;
            }
            None => {
                app.log(format!(
                    "no GitMesh project found at or above {}",
                    start.display()
                ));
                app.log("choose a directory and press Enter, or press 's' to scan the current one");
                app.scan_directory(&start)?;
            }
        }
        Ok(app)
    }

    /// Switch to the project screen with a loaded project.
    pub fn set_project(&mut self, project: GitMeshProject) -> Result<()> {
        self.start_dir = project.root.clone();
        self.screen = Screen::Project;
        self.project = Some(project);
        self.refresh_scan()?;
        self.refresh_status()?;
        Ok(())
    }

    // ------------------------------------------------------------- data load --

    /// (Re)scan the project tree.
    pub fn refresh_scan(&mut self) -> Result<()> {
        let options = ScanOptions::default();
        let scan = discovery::scan_project(&self.start_dir, &options, &self.runner)?;
        self.scan = Some(scan);
        self.rebuild_rows();
        Ok(())
    }

    fn scan_directory(&mut self, path: &Path) -> Result<()> {
        self.start_dir = crate::paths::lexical_normalize(path);
        self.refresh_scan()
    }

    /// (Re)load the project from disk.
    pub fn reload_project(&mut self) -> Result<()> {
        match manifest::find_project_root(&self.start_dir) {
            Some(root) => {
                let project = manifest::load_from_root(&root)?;
                self.set_project(project)
            }
            None => {
                self.screen = Screen::Setup;
                self.project = None;
                self.refresh_scan()
            }
        }
    }

    /// (Re)read the unified status.
    pub fn refresh_status(&mut self) -> Result<()> {
        if let Some(project) = &self.project {
            let analyzer = Analyzer::new(project, &self.runner);
            self.status = Some(analyzer.analyze());
        }
        Ok(())
    }

    fn rebuild_rows(&mut self) {
        self.rows.clear();
        let Some(scan) = &self.scan else { return };
        collect_rows(&scan.tree, 0, self.project.as_ref(), &mut self.rows);
        if self.selected >= self.rows.len() {
            self.selected = self.rows.len().saturating_sub(1);
        }
    }

    /// The currently selected tree row.
    pub fn selected_row(&self) -> Option<&TreeRow> {
        self.rows.get(self.selected)
    }

    /// The configured repository that owns the selected directory.
    pub fn selected_repository(&self) -> Option<&crate::model::PhysicalRepository> {
        let row = self.selected_row()?;
        self.project
            .as_ref()?
            .repository_for_relative(&row.relative_path)
            .filter(|repo| !repo.is_root() || row.relative_path == Path::new("."))
    }

    // ------------------------------------------------------------- navigation --

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let index = self.selected as isize + delta;
        self.selected = index.clamp(0, self.rows.len() as isize - 1) as usize;
    }

    pub fn log(&mut self, message: impl Into<String>) {
        self.log.push(message.into());
        if self.log.len() > 200 {
            self.log.drain(0..100);
        }
    }

    // ------------------------------------------------------------- operations --

    /// Start a modal input.
    pub fn begin_input(&mut self, kind: InputKind) {
        self.mode = Some(kind);
        self.input = match kind {
            InputKind::Directory => self.start_dir.display().to_string(),
            _ => String::new(),
        };
    }

    /// Cancel the modal input.
    pub fn cancel_input(&mut self) {
        self.mode = None;
        self.input.clear();
    }

    /// Submit the modal input.
    pub fn submit_input(&mut self) -> Result<()> {
        let Some(kind) = self.mode.take() else {
            return Ok(());
        };
        let value = self.input.trim().to_string();
        self.input.clear();
        match kind {
            InputKind::Directory => {
                let path = PathBuf::from(&value);
                if !path.is_dir() {
                    self.log(format!("'{value}' is not a directory"));
                    return Ok(());
                }
                match manifest::find_project_root(&path) {
                    Some(root) => {
                        let project = manifest::load_from_root(&root)?;
                        self.log(format!("opened project '{}'", project.name));
                        self.set_project(project)?;
                    }
                    None => {
                        self.log(format!("no GitMesh project at or above {value}; scanning"));
                        self.scan_directory(&path)?;
                    }
                }
            }
            InputKind::RepositoryId => {
                if let Some(repo) = self.selected_repository().map(|r| r.id.clone()) {
                    let project = self.project.clone().unwrap();
                    let updated = discovery::rename_repository(&project, &repo, &value)?;
                    manifest::save_project(&updated)?;
                    self.log(format!("renamed '{repo}' to '{value}'"));
                    self.reload_project()?;
                }
            }
            InputKind::RemoteUrl => {
                if let Some(id) = self.selected_repository().map(|r| r.id.clone()) {
                    let project = self.project.clone().unwrap();
                    let url = if value.is_empty() {
                        None
                    } else {
                        Some(value.clone())
                    };
                    let updated =
                        discovery::set_repository_remote(&project, &id, url, true, &self.runner)?;
                    manifest::save_project(&updated)?;
                    self.log(match value.is_empty() {
                        true => format!("cleared the remote of '{id}'"),
                        false => format!("set the remote of '{id}' to {value}"),
                    });
                    self.reload_project()?;
                }
            }
            InputKind::CommitMessage => {
                if value.is_empty() {
                    self.log("commit cancelled: no message");
                } else {
                    self.run_commit(&value)?;
                }
            }
            InputKind::CheckoutBranch => {
                self.run_branch(BranchAction::Checkout {
                    name: value,
                    create: false,
                })?;
            }
            InputKind::NewBranch => {
                self.run_branch(BranchAction::Checkout {
                    name: value,
                    create: true,
                })?;
            }
            InputKind::MergeBranch => {
                self.run_branch(BranchAction::Merge { name: value })?;
            }
        }
        Ok(())
    }

    /// Mark the selected directory as an external repository.
    pub fn assign_selected(&mut self) -> Result<()> {
        let Some(project) = self.project.clone() else {
            // Setup screen without a saved project: create the project first.
            return self.create_project_from_scan();
        };
        let Some(row) = self.selected_row().cloned() else {
            return Ok(());
        };
        if row.relative_path == Path::new(".") {
            self.log("the project root is always the root repository");
            return Ok(());
        }
        if row.is_external {
            return self.unassign_selected();
        }
        let check = discovery::check_assignment(&project, &row.relative_path, &self.runner)?;
        if !check.can_assign() {
            for blocker in &check.blockers {
                self.log(format!("cannot assign: {blocker}"));
            }
            return Ok(());
        }
        let options = discovery::AssignOptions {
            id: None,
            remote_url: None,
            init_git: true,
            branch: None,
        };
        let updated =
            discovery::assign_repository(&project, &row.relative_path, &options, &self.runner)?;
        manifest::save_project(&updated)?;
        let id = updated
            .repository_for_relative(&row.relative_path)
            .map(|r| r.id.clone())
            .unwrap_or_default();
        self.log(format!(
            "'{}' is now the independent repository '{id}'",
            to_slash(&row.relative_path)
        ));
        self.reload_project()
    }

    /// Remove the selected repository from the configuration.
    ///
    /// Same path as the command line and the graphical interface: the shared management
    /// service plans the change, and the plan is applied only when it is ready. The plan
    /// refuses when the root repository still tracks files inside the directory, because
    /// those files go back to the root repository. The terminal has no step for confirming
    /// that consequence, so it refuses and says where the explicit confirmation lives. The
    /// directory, its `.git`, its history and its remote are never touched.
    pub fn unassign_selected(&mut self) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let Some(repo) = self.selected_repository().map(|r| r.id.clone()) else {
            self.log("select a configured repository to remove it");
            return Ok(());
        };
        let request = manage::RepositoryManagementRequest::one(manage::RepositoryIntent::Remove {
            id: repo.clone(),
            confirm_takeover: false,
        });
        let plan = manage::plan(&project, &request, &self.runner)?;
        if !plan.is_ready() {
            for blocker in &plan.blockers {
                self.log(format!("cannot remove '{repo}': {blocker}"));
            }
            self.log(format!(
                "to confirm that, run `gitmesh configure remove {repo} --confirm-takeover`"
            ));
            return Ok(());
        }
        for warning in &plan.warnings {
            self.log(format!("note: {warning}"));
        }
        let result = manage::apply(
            &plan,
            self.dry_run,
            &self.runner,
            &mut manage::RepositoryObserver::silent(),
        );
        for outcome in &result.actions {
            self.log(format!("{}: {}", outcome.row_id(), outcome.summary));
        }
        if !result.is_success() {
            self.log("the removal did not complete; review the lines above");
        } else if result.dry_run {
            self.log(format!(
                "dry run: '{repo}' would be removed; nothing was changed"
            ));
        }
        self.reload_project()
    }

    /// Create the manifest for the current directory, keeping the root repository only.
    pub fn create_project_from_scan(&mut self) -> Result<()> {
        let root = self.start_dir.clone();
        if root.join(".gitmesh/project.toml").exists() {
            return self.reload_project();
        }
        let project = discovery::initial_project(&root, None, None, None)?;
        let path = manifest::save_project(&project)?;
        self.log(format!("saved configuration to {}", path.display()));
        self.set_project(project)
    }

    /// Commit every change with one logical message.
    pub fn run_commit(&mut self, message: &str) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let options = CommitOptions {
            message: message.to_string(),
            selection: RepositorySelection::All,
            dry_run: self.dry_run,
            include_untracked: true,
            quiet_clean: false,
        };
        let report = ops::commit_project(&project, &self.runner, &options)?;
        self.finish_report(report)
    }

    /// Fetch from every remote.
    pub fn run_fetch(&mut self) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let mut options = SyncOptions::new();
        options.dry_run = self.dry_run;
        let report = ops::fetch_project(&project, &self.runner, &options)?;
        self.finish_report(report)
    }

    /// Pull every repository.
    pub fn run_pull(&mut self) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let mut options = SyncOptions::new();
        options.dry_run = self.dry_run;
        let report = ops::pull_project(&project, &self.runner, &options)?;
        self.finish_report(report)
    }

    /// Push every repository that has work to push.
    pub fn run_push(&mut self) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let options = PushOptions {
            selection: RepositorySelection::All,
            dry_run: self.dry_run,
            set_upstream: true,
            default_remote: "origin".to_string(),
        };
        let report = ops::push_project(&project, &self.runner, &options)?;
        self.finish_report(report)
    }

    /// Run a logical branch operation.
    pub fn run_branch(&mut self, action: BranchAction) -> Result<()> {
        let Some(project) = self.project.clone() else {
            return Ok(());
        };
        let options = BranchOptions {
            selection: RepositorySelection::All,
            force: false,
            dry_run: self.dry_run,
            excluded: self.excluded.clone(),
        };
        let report = ops::branch_operation(&project, &self.runner, &action, &options)?;
        self.finish_report(report)
    }

    /// Store a report and mirror it into the log so the user sees the outcome.
    fn finish_report(&mut self, report: OperationReport) -> Result<()> {
        for line in report.summary_lines() {
            if !line.is_empty() {
                self.log(line);
            }
        }
        for detail in report.detail_lines() {
            self.log(detail);
        }
        self.report = Some(report);
        self.refresh_status()
    }

    /// True when the last operation had failures or conflicts.
    pub fn last_operation_had_problems(&self) -> bool {
        self.report
            .as_ref()
            .is_some_and(|report| report.outcomes.iter().any(|o| o.kind.is_problem()))
    }

    /// Branch label of the project: the branch of the root repository, with the
    /// outliers spelled out so the project can never look consistent when it is not.
    pub fn logical_branch(&self) -> String {
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
        format!("{reference} ({detail})")
    }

    /// Repositories that are not on the same branch as the majority.
    pub fn branch_outliers(&self) -> Vec<String> {
        self.status
            .as_ref()
            .map(|status| {
                status
                    .inconsistent_branches()
                    .iter()
                    .map(|r| format!("{} on {}", r.id, r.head().label()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Flatten the scanned tree into visible rows, annotating configured ownership.
fn collect_rows(
    node: &DirectoryNode,
    depth: usize,
    project: Option<&GitMeshProject>,
    rows: &mut Vec<TreeRow>,
) {
    let owner = project.and_then(|project| {
        project
            .repository_for_relative(&node.relative_path)
            .filter(|repo| !repo.is_root() || node.relative_path == Path::new("."))
    });
    rows.push(TreeRow {
        depth,
        name: node.name.clone(),
        relative_path: node.relative_path.clone(),
        is_repository_root: node.is_repository_root,
        repository_id: owner.map(|repo| repo.id.clone()),
        is_external: owner.is_some_and(|repo| repo.role == RepositoryRole::External),
        file_count: node.file_count,
        truncated: node.truncated,
    });
    for child in &node.children {
        collect_rows(child, depth + 1, project, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    fn app_for(fixture: &RepoFixture) -> App {
        App::new(fixture.path(), false).expect("app")
    }

    #[test]
    fn opens_an_existing_project() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let app = app_for(&fixture);
        assert_eq!(app.screen, Screen::Project);
        assert!(!app.project.as_ref().unwrap().name.is_empty());
        assert_eq!(app.project.as_ref().unwrap().root_repository().id, "root");
        assert!(app.status.is_some());
    }

    #[test]
    fn starts_in_setup_when_no_project_exists() {
        let fixture = RepoFixture::new();
        std::fs::create_dir_all(fixture.path().join("engine")).unwrap();
        assert!(!fixture.path().join(".gitmesh").exists());
        let app = app_for(&fixture);
        assert_eq!(app.screen, Screen::Setup);
        assert!(app.scan.is_some());
        // The tree is visible so the user can inspect it before configuring anything.
        assert!(app
            .rows
            .iter()
            .any(|row| row.relative_path == Path::new("engine")));
    }

    #[test]
    fn setup_creates_a_project_and_marks_repositories() {
        let fixture = RepoFixture::new();
        fixture.init_repo("engine");
        fixture.write("engine/README.md", "# engine\n");
        fixture.commit("engine", "initial commit");
        let mut app = app_for(&fixture);
        assert_eq!(app.screen, Screen::Setup);

        // Saving the configuration creates the manifest and switches to the project.
        app.create_project_from_scan().unwrap();
        assert_eq!(app.screen, Screen::Project);
        assert!(fixture.path().join(".gitmesh/project.toml").exists());

        // Select the engine directory and mark it as an external repository.
        let index = app
            .rows
            .iter()
            .position(|row| row.relative_path == Path::new("engine"))
            .unwrap();
        app.selected = index;
        app.assign_selected().unwrap();

        let project = app.project.as_ref().unwrap();
        let engine = project
            .repository_for_relative(Path::new("engine"))
            .unwrap();
        assert_eq!(engine.id, "engine");
        assert!(!engine.is_root());
        // The decision is persisted.
        let reloaded = manifest::load_from_root(fixture.path()).unwrap();
        assert!(reloaded.repository("engine").is_some());
    }

    #[test]
    fn unassigning_returns_the_directory_to_the_root_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let mut app = app_for(&fixture);
        let index = app
            .rows
            .iter()
            .position(|row| row.relative_path == Path::new("engine"))
            .unwrap();
        app.selected = index;
        app.unassign_selected().unwrap();
        let project = app.project.as_ref().unwrap();
        assert!(project.repository("engine").is_none());
        assert!(
            fixture.path().join("engine/.git").exists(),
            "files untouched"
        );
    }

    #[test]
    fn unassigning_refuses_when_files_would_go_back_to_the_root_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        // The root repository starts by tracking a file that later belongs to the external
        // repository (the order a real project grows in). Removing the external entry would
        // hand that file back to the root repository, which needs an explicit confirmation.
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks engine/lib.rs");
        fixture.init_repo("engine");
        let project = fixture.load_project();
        let options = discovery::AssignOptions {
            id: None,
            remote_url: None,
            init_git: false,
            branch: None,
        };
        let updated =
            discovery::assign_repository(&project, Path::new("engine"), &options, fixture.runner())
                .unwrap();
        manifest::save_project(&updated).unwrap();

        let mut app = app_for(&fixture);
        let index = app
            .rows
            .iter()
            .position(|row| row.relative_path == Path::new("engine"))
            .unwrap();
        app.selected = index;
        app.unassign_selected().unwrap();

        // The terminal refuses: the repository stays configured, and the reason and the
        // explicit way to confirm it are on screen.
        assert!(
            app.project.as_ref().unwrap().repository("engine").is_some(),
            "a refused removal changes nothing"
        );
        assert!(
            app.log
                .iter()
                .any(|line| line.contains("cannot remove 'engine'") && line.contains("1 file(s)")),
            "{:?}",
            app.log
        );
        assert!(
            app.log
                .iter()
                .any(|line| line.contains("--confirm-takeover")),
            "{:?}",
            app.log
        );
        assert!(
            fixture.path().join("engine/.git").exists(),
            "files untouched"
        );
        assert_eq!(
            fixture.git_ok(".", &["ls-files", "--", "engine"]).trim(),
            "engine/lib.rs",
            "and the root repository still owns it"
        );
        let manifest_now = manifest::load_from_root(fixture.path()).unwrap();
        assert!(
            manifest_now.repository("engine").is_some(),
            "the manifest is unchanged"
        );
    }

    #[test]
    fn commit_from_the_ui_commits_every_repository() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "root");
        fixture.write("engine/src/lib.rs", "engine");

        let mut app = app_for(&fixture);
        app.run_commit("ui commit").unwrap();
        assert!(fixture
            .git_ok(".", &["log", "-1", "--pretty=%s"])
            .contains("ui commit"));
        assert!(fixture
            .git_ok("engine", &["log", "-1", "--pretty=%s"])
            .contains("ui commit"));
        assert!(!app.last_operation_had_problems());
    }

    #[test]
    fn dry_run_mode_changes_nothing() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        fixture.write("src/main.rs", "content");
        let before = fixture.git_ok(".", &["rev-parse", "HEAD"]);

        let mut app = App::new(fixture.path(), true).unwrap();
        app.run_commit("dry ui commit").unwrap();
        assert_eq!(fixture.git_ok(".", &["rev-parse", "HEAD"]), before);
        assert!(app.dry_run);
    }

    #[test]
    fn push_and_pull_are_available_from_the_ui() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "remotes/root.git");
        fixture.publish("engine", "remotes/engine.git");
        let mut app = app_for(&fixture);

        fixture.write_and_commit("src/main.rs", "x");
        app.run_push().unwrap();
        assert!(!app.last_operation_had_problems(), "{:?}", app.log);

        app.run_pull().unwrap();
        assert!(!app.last_operation_had_problems(), "{:?}", app.log);
        assert!(project.repository("engine").is_some());
    }

    #[test]
    fn branch_workflow_from_the_ui() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let mut app = app_for(&fixture);
        app.run_branch(BranchAction::Checkout {
            name: "feature/ui".into(),
            create: true,
        })
        .unwrap();
        assert_eq!(
            fixture
                .git_ok(".", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "feature/ui"
        );
        assert_eq!(
            fixture
                .git_ok("engine", &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "feature/ui"
        );
        assert_eq!(app.logical_branch(), "feature/ui");
    }

    #[test]
    fn reports_split_branches_clearly() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.git_ok("engine", &["checkout", "-q", "-b", "side"]);
        let app = app_for(&fixture);
        // The root repository decides the project branch; engine is reported as out of
        // step in the label itself.
        assert_eq!(app.logical_branch(), "main (engine on side)");
        let outliers = app.branch_outliers();
        assert_eq!(outliers.len(), 1, "{outliers:?}");
        assert!(outliers[0].contains("engine"));
    }

    #[test]
    fn status_shows_which_repository_owns_a_change() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/src/lib.rs", "changed");
        let mut app = app_for(&fixture);
        app.refresh_status().unwrap();
        let project = app.project.as_ref().unwrap();
        let analyzer = Analyzer::new(project, &app.runner);
        let status = app.status.as_ref().unwrap();
        let changes = analyzer.owned_changes(status);
        assert!(changes
            .iter()
            .any(|c| c.repository_id == "engine" && c.logical_path == "engine/src/lib.rs"));
    }

    #[test]
    fn input_flow_applies_a_remote_url() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let mut app = app_for(&fixture);
        let index = app
            .rows
            .iter()
            .position(|row| row.relative_path == Path::new("engine"))
            .unwrap();
        app.selected = index;

        app.begin_input(InputKind::RemoteUrl);
        app.input = "git@github.com:acme/engine.git".into();
        app.submit_input().unwrap();

        let reloaded = manifest::load_from_root(fixture.path()).unwrap();
        assert_eq!(
            reloaded.repository("engine").unwrap().remote_url.as_deref(),
            Some("git@github.com:acme/engine.git")
        );
    }

    #[test]
    fn invalid_input_is_reported_without_panicking() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[("root", ".")]);
        let mut app = app_for(&fixture);
        app.begin_input(InputKind::Directory);
        app.input = "/definitely/not/a/directory".into();
        app.submit_input().unwrap();
        assert!(app.log.iter().any(|l| l.contains("not a directory")));
        assert_eq!(app.screen, Screen::Project);
    }

    #[test]
    fn excluded_repositories_are_left_alone() {
        let fixture = RepoFixture::new();
        fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        let mut app = app_for(&fixture);
        app.excluded = vec!["engine".into()];
        app.run_branch(BranchAction::Create {
            name: "feature/skip".into(),
        })
        .unwrap();
        assert!(!fixture
            .git_ok("engine", &["branch", "--list", "feature/skip"])
            .contains("feature/skip"));
        assert!(fixture
            .git_ok("renderer", &["branch", "--list", "feature/skip"])
            .contains("feature/skip"));
    }
}
