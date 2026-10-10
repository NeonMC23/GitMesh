//! Local graphical interface for GitMesh.
//!
//! ```text
//!   gitmesh gui [--port 7345] [--host 127.0.0.1] [--allow-host NAME] [--open] [--dry-run]
//! ```
//!
//! The GUI is a small HTML application served by GitMesh itself: no cloud service, no
//! telemetry, no accounts, no external assets, no network access beyond the address the
//! user asked for. It exists to make the logical-project workflow the CLI offers
//! reachable without thinking about the physical repositories underneath.
//!
//! Design rules that keep it a front end rather than a second implementation:
//!
//! * **One source of truth.** Every operation goes through [`crate::service`], which
//!   goes through [`crate::ops`]. The GUI contains no Git command, no ownership rule and
//!   no outcome classification of its own.
//! * **One project.** Opening a project, listing repositories, resolving which
//!   repository owns a file and classifying a result are all done by the core; the GUI
//!   renders what [`editor`] puts in the model.
//! * **Progress is an abstraction, not a widget.** Logical operations report their
//!   per-repository progress through [`crate::ops::OperationObserver`], which this
//!   module turns into server-sent events. Any other front end can use the same seam.
//! * **Nothing is hidden.** Conflicts, partial failures and unavailable repositories are
//!   first-class states, and the interface says explicitly when something has to be
//!   resolved with Git in one repository.
//!
//! The module is split so the interesting parts are testable without a browser:
//!
//! * [`editor`] — the model the interface renders (pure, JSON, tested).
//! * [`server`] — HTTP/SSE transport (thin, no project logic).
//! * [`asset`] — embedded HTML/CSS/JS, plus tests for the client-side logic.

pub mod asset;
pub mod editor;
mod server;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::error::{Error, Result};
use crate::git::GitRunner;
use crate::json::Json;
use crate::manage::{self, RepositoryManagementRequest, RepositoryPlan};
use crate::model::{PhysicalRepository, RepositoryRole};
use crate::ops::RepositorySelection;
use crate::ops::{
    BranchAction, BranchOptions, CommitOptions, OperationObserver, OperationReport, OutcomeKind,
    PullStrategy, PushOptions, RepoOutcome, SyncOptions,
};
use crate::service::{self, ProjectSession};
use crate::setup::{self, SetupObserver, SetupPlan, SetupRequest, SetupStepKind};

/// Default port of the local interface.
pub const DEFAULT_PORT: u16 = 7345;

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct GuiOptions {
    /// Directory to open the project from (`None` = the current directory).
    pub start: Option<PathBuf>,
    /// Address to bind (`127.0.0.1` by default; `0.0.0.0` exposes it on the network).
    pub host: String,
    /// Extra host names the interface accepts in the `Host` header, for access through a
    /// proxy or a port forward (`--allow-host`). Empty means "only the bind address".
    pub allow_hosts: Vec<String>,
    /// Port to bind (`0` picks a free one).
    pub port: u16,
    /// Try to open the interface in the system browser.
    pub open: bool,
    /// Start with every mutating operation simulated (`--dry-run`).
    pub dry_run: bool,
}

impl Default for GuiOptions {
    fn default() -> Self {
        GuiOptions {
            start: None,
            host: "127.0.0.1".to_string(),
            allow_hosts: Vec::new(),
            port: DEFAULT_PORT,
            open: false,
            dry_run: false,
        }
    }
}

/// Run the graphical interface until the process is stopped.
pub fn run(options: GuiOptions) -> Result<()> {
    let start = match &options.start {
        Some(path) => crate::paths::absolute(path)?,
        None => std::env::current_dir().map_err(|e| Error::io(PathBuf::from("."), e))?,
    };
    let gui = Arc::new(Gui::new(start, options.dry_run));

    let listener = server::bind(&options.host, options.port)?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Other(format!("could not read the listening port: {e}")))?
        .port();
    let url = format!("http://{}:{port}", display_host(&options.host));

    println!("gitmesh gui");
    println!();
    println!("  interface   {url}");
    match gui.opened_project() {
        Some((name, root)) => {
            println!("  project     {name}");
            println!("  root        {}", root.display());
        }
        None => {
            println!("  project     (none open yet)");
            if let Some(error) = gui.open_error() {
                println!("  note        {error}");
            }
        }
    }
    if gui.dry_run() {
        println!("  mode        dry run: nothing will be changed");
    }
    if options.host != "127.0.0.1" && options.host != "localhost" {
        println!(
            "  warning     bound to {} and reachable from the network; GitMesh has no \
             authentication",
            options.host
        );
    }
    if !options.allow_hosts.is_empty() {
        println!(
            "  note        also answering requests addressed to {}",
            options.allow_hosts.join(", ")
        );
    }
    println!();
    println!("  Open the address above in a browser. Press Ctrl+C to stop.");
    println!("  (GitMesh runs locally: no cloud service, no telemetry, no account.)");

    if options.open {
        open_browser(&url);
    }

    server::serve(listener, gui, &options.host, &options.allow_hosts)
}

/// Host shown in the printed URL (a wildcard bind is not a usable address).
fn display_host(host: &str) -> String {
    match host {
        "0.0.0.0" | "::" | "[::]" => "127.0.0.1".to_string(),
        other => other.to_string(),
    }
}

/// Best-effort "open this in the browser" on the three desktop platforms.
fn open_browser(url: &str) {
    let candidates: [(&str, Vec<&str>); 3] = [
        ("xdg-open", vec![url]),
        ("open", vec![url]),
        ("cmd", vec!["/C", "start", "", url]),
    ];
    for (program, args) in candidates {
        if std::process::Command::new(program)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}

// ------------------------------------------------------------------ operations --

/// One operation requested by the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuiOperation {
    /// One logical commit with one message.
    Commit { message: String },
    /// Create a branch in every repository.
    BranchCreate { name: String },
    /// Switch every repository to an existing branch.
    BranchCheckout { name: String },
    /// Create the branch where it is missing and switch every repository to it.
    BranchStart { name: String },
    /// Merge a branch into the current branch in every repository.
    BranchMerge { name: String },
    /// Delete a branch from every repository that can delete it safely.
    BranchDelete { name: String, force: bool },
    /// Fetch every repository.
    Fetch,
    /// Pull every repository.
    Pull { strategy: PullStrategy },
    /// Push every repository.
    Push,
}

impl GuiOperation {
    /// Name used in progress events and result summaries (the core's operation name).
    pub fn label(&self) -> &'static str {
        match self {
            GuiOperation::Commit { .. } => "commit",
            GuiOperation::BranchCreate { .. } => "branch",
            GuiOperation::BranchCheckout { .. } => "checkout",
            GuiOperation::BranchStart { .. } => "checkout",
            GuiOperation::BranchMerge { .. } => "merge",
            GuiOperation::BranchDelete { .. } => "branch-delete",
            GuiOperation::Fetch => "fetch",
            GuiOperation::Pull { .. } => "pull",
            GuiOperation::Push => "push",
        }
    }

    /// Human sentence shown while the operation runs.
    pub fn sentence(&self) -> String {
        match self {
            GuiOperation::Commit { .. } => "Committing the project".to_string(),
            GuiOperation::BranchCreate { name } => format!("Creating branch '{name}'"),
            GuiOperation::BranchCheckout { name } => format!("Switching to '{name}'"),
            GuiOperation::BranchStart { name } => format!("Starting branch '{name}'"),
            GuiOperation::BranchMerge { name } => format!("Merging '{name}'"),
            GuiOperation::BranchDelete { name, .. } => format!("Deleting branch '{name}'"),
            GuiOperation::Fetch => "Fetching the project".to_string(),
            GuiOperation::Pull { .. } => "Pulling the project".to_string(),
            GuiOperation::Push => "Pushing the project".to_string(),
        }
    }
}

/// Server-sent event buffer of one operation.
#[derive(Debug, Default)]
pub struct EventLog {
    events: Mutex<Vec<String>>,
    finished: AtomicBool,
}

impl EventLog {
    fn push(&self, event: Json) {
        self.events
            .lock()
            .expect("event log lock")
            .push(event.compact());
    }

    /// Hand over everything buffered so far.
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut self.events.lock().expect("event log lock"))
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
    }

    /// True once the operation has produced its final event.
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

/// One operation, running or recently finished.
struct OperationEntry {
    id: u64,
    label: String,
    events: Arc<EventLog>,
    /// Pretty JSON result, stored once the operation ends.
    result: String,
}

#[derive(Default)]
struct Operations {
    sequence: u64,
    current: Option<OperationEntry>,
}

// ------------------------------------------------------------------------ gui --

/// Application state of the interface.
pub struct Gui {
    state: Mutex<GuiState>,
    operations: Mutex<Operations>,
}

struct GuiState {
    start: PathBuf,
    session: Option<ProjectSession>,
    error: Option<String>,
    configuration: bool,
    dry_run: bool,
    /// Runner used by the setup wizard, which works *before* a project exists.
    runner: GitRunner,
    /// Set when `git` itself is unavailable, so the wizard can say so.
    runner_error: Option<String>,
}

impl Gui {
    /// Create the interface, opening the project at (or above) `start` when there is
    /// one. A missing or invalid project is not fatal: the interface opens and offers to
    /// open another directory, which is what a GUI should do.
    pub fn new(start: PathBuf, dry_run: bool) -> Gui {
        let (session, error, configuration) = match ProjectSession::open(&start) {
            Ok(session) => (Some(session), None, false),
            Err(err) => (
                None,
                Some(err.to_string()),
                err.is_configuration_error() || matches!(err, Error::ProjectNotFound { .. }),
            ),
        };
        let (runner, runner_error) = match GitRunner::detect() {
            Ok(runner) => (runner, None),
            Err(err) => (GitRunner::default(), Some(err.to_string())),
        };
        Gui {
            state: Mutex::new(GuiState {
                start,
                session,
                error,
                configuration,
                dry_run,
                runner,
                runner_error,
            }),
            operations: Mutex::new(Operations::default()),
        }
    }

    /// The opened project, if any.
    pub fn opened_project(&self) -> Option<(String, PathBuf)> {
        let state = self.state.lock().expect("state lock");
        state
            .session
            .as_ref()
            .map(|session| (session.name().to_string(), session.root().to_path_buf()))
    }

    /// Why the project could not be opened, if it could not.
    pub fn open_error(&self) -> Option<String> {
        self.state.lock().expect("state lock").error.clone()
    }

    /// True when every mutating operation is simulated.
    pub fn dry_run(&self) -> bool {
        self.state.lock().expect("state lock").dry_run
    }

    /// Turn dry-run mode on or off.
    pub fn set_dry_run(&self, value: bool) {
        self.state.lock().expect("state lock").dry_run = value;
    }

    /// The model currently shown by the interface.
    pub fn model(&self) -> String {
        self.model_json().to_pretty_string()
    }

    /// The current interface model as structured data.
    fn model_json(&self) -> Json {
        let state = self.state.lock().expect("state lock");
        match &state.session {
            Some(session) => editor::project_model(session, state.dry_run),
            None => editor::empty_model(&state.start, state.error.as_deref(), state.configuration),
        }
    }

    /// Open a project from a directory (or any directory inside it).
    ///
    /// Fails while an operation is running: the interface must not switch projects
    /// underneath a commit that is still executing.
    pub fn open(&self, path: &Path) -> std::result::Result<(), String> {
        if self.is_busy() {
            return Err(
                "an operation is still running; wait for it to finish before opening another \
                 project"
                    .to_string(),
            );
        }
        self.adopt(path)
    }

    /// Re-read the project from disk, from the directory the interface last opened.
    ///
    /// A session keeps the manifest it was opened with, so a repository added or removed
    /// by the CLI or the TUI would stay invisible until the interface restarted. A reload
    /// reopens the session; a project that has become invalid shows its error instead.
    /// While an operation runs this does nothing: the operation works on the configuration
    /// it started with, and the busy guard keeps the session from changing under it.
    pub fn reload(&self) {
        if self.is_busy() {
            return;
        }
        let start = self.start_directory();
        // The outcome is already recorded in the state (session or error), so the
        // returned message is not needed here.
        let _ = self.adopt(&start);
    }

    /// Open a project from inside a running operation.
    ///
    /// Only the setup operation uses this, and only for the project it just created: the
    /// busy check exists so that a *user* cannot switch projects under a running commit,
    /// while the setup is the operation and finishes by adopting its own result.
    fn adopt(&self, path: &Path) -> std::result::Result<(), String> {
        let normalized = crate::paths::lexical_normalize(path);
        match ProjectSession::open(&normalized) {
            Ok(session) => {
                let mut state = self.state.lock().expect("state lock");
                state.start = normalized;
                state.session = Some(session);
                state.error = None;
                state.configuration = false;
                Ok(())
            }
            Err(err) => {
                let configuration =
                    err.is_configuration_error() || matches!(err, Error::ProjectNotFound { .. });
                let message = err.to_string();
                let mut state = self.state.lock().expect("state lock");
                state.start = normalized;
                state.session = None;
                state.error = Some(message.clone());
                state.configuration = configuration;
                Err(message)
            }
        }
    }

    /// The directory the interface was started in (and last looked at).
    pub fn start_directory(&self) -> PathBuf {
        self.state.lock().expect("state lock").start.clone()
    }

    /// Git runner, or the reason there is none.
    fn runner(&self) -> std::result::Result<GitRunner, String> {
        let state = self.state.lock().expect("state lock");
        match &state.runner_error {
            Some(error) => Err(error.clone()),
            None => Ok(state.runner.clone()),
        }
    }

    /// Inspect a directory as a candidate project root. Read-only.
    pub fn inspect_directory(&self, path: &Path) -> std::result::Result<setup::Inspection, String> {
        let runner = self.runner()?;
        let path = crate::paths::absolute(path).map_err(|err| err.to_string())?;
        let inspection = setup::inspect(&path, &runner).map_err(|err| err.to_string())?;
        let mut state = self.state.lock().expect("state lock");
        state.start = path;
        Ok(inspection)
    }

    /// Turn a setup request into a plan. Read-only: nothing is created.
    pub fn plan_setup(&self, request: &SetupRequest) -> std::result::Result<SetupPlan, String> {
        let runner = self.runner()?;
        setup::plan(request, &runner).map_err(|err| err.to_string())
    }

    /// Inspect the repositories of the open project. Read-only.
    ///
    /// This is the row source of the repositories panel and of the settings screen: what is
    /// configured, what is on disk, and what deserves attention.
    pub fn inspect_repositories(
        &self,
    ) -> std::result::Result<manage::RepositoryInspection, String> {
        let session = self
            .session()
            .ok_or_else(|| "no GitMesh project is open".to_string())?;
        let runner = self.runner()?;
        manage::inspect(session.project(), &runner).map_err(|err| err.to_string())
    }

    /// Inspect one directory as a candidate repository. Read-only.
    pub fn inspect_repository_candidate(
        &self,
        path: &str,
    ) -> std::result::Result<manage::CandidateInspection, String> {
        let session = self
            .session()
            .ok_or_else(|| "no GitMesh project is open".to_string())?;
        let runner = self.runner()?;
        manage::inspect_candidate(session.project(), path, &runner).map_err(|err| err.to_string())
    }

    /// Turn a management request into a plan. Read-only: nothing is configured yet.
    ///
    /// The open project is the configuration the plan is made from: this is the same kind
    /// of plan the CLI builds, including the manifest preview it will write.
    pub fn plan_management(
        &self,
        request: &RepositoryManagementRequest,
    ) -> std::result::Result<RepositoryPlan, String> {
        let session = self
            .session()
            .ok_or_else(|| "no GitMesh project is open".to_string())?;
        let runner = self.runner()?;
        manage::plan(session.project(), request, &runner).map_err(|err| err.to_string())
    }

    /// Apply a management plan in the background and return its id.
    ///
    /// The request is planned again here and the identifier is compared with the plan the
    /// user reviewed, exactly like the setup: if the configuration changed in between, the
    /// plan differs and the operation is refused instead of touching something nobody saw.
    pub fn start_management(
        self: &Arc<Self>,
        request: RepositoryManagementRequest,
        reviewed_plan: Option<&str>,
    ) -> std::result::Result<u64, ManagementRefusal> {
        if self.is_busy() {
            let running = self.running_label().unwrap_or_default();
            return Err(ManagementRefusal::Busy(format!(
                "'{running}' is still running; wait for it to finish"
            )));
        }
        let session = self
            .session()
            .ok_or_else(|| ManagementRefusal::Refused("no GitMesh project is open".to_string()))?;
        let runner = self.runner().map_err(ManagementRefusal::Refused)?;
        let plan = manage::plan(session.project(), &request, &runner)
            .map_err(|err| ManagementRefusal::Refused(err.to_string()))?;

        if let Some(reviewed) = reviewed_plan {
            if reviewed != plan.id {
                return Err(ManagementRefusal::PlanChanged(Box::new(plan)));
            }
        }
        if !plan.is_ready() {
            return Err(ManagementRefusal::Blocked(Box::new(plan)));
        }

        let dry_run = self.dry_run();
        let (id, events) = {
            let mut operations = self.operations.lock().expect("operations lock");
            operations.sequence += 1;
            let id = operations.sequence;
            let events = Arc::new(EventLog::default());
            operations.current = Some(OperationEntry {
                id,
                label: "Repositories".to_string(),
                events: Arc::clone(&events),
                result: String::new(),
            });
            (id, events)
        };

        let gui = Arc::clone(self);
        std::thread::spawn(move || {
            let result = run_management(&gui, &plan, dry_run, id, &events);
            events.finish();
            let mut operations = gui.operations.lock().expect("operations lock");
            if let Some(current) = operations.current.as_mut().filter(|entry| entry.id == id) {
                current.result = result;
            }
        });
        Ok(id)
    }

    /// Apply a setup in the background and return its id.
    ///
    /// The request is planned again here and the identifier is compared with the plan
    /// the user reviewed: if the directory changed in between, the plan differs and the
    /// setup is refused instead of executing something nobody saw.
    pub fn start_setup(
        self: &Arc<Self>,
        request: SetupRequest,
        reviewed_plan: Option<&str>,
    ) -> std::result::Result<u64, SetupRefusal> {
        if self.is_busy() {
            let running = self.running_label().unwrap_or_default();
            return Err(SetupRefusal::Busy(format!(
                "'{running}' is still running; wait for it to finish"
            )));
        }
        let runner = self.runner().map_err(SetupRefusal::Refused)?;
        let plan =
            setup::plan(&request, &runner).map_err(|err| SetupRefusal::Refused(err.to_string()))?;

        if let Some(reviewed) = reviewed_plan {
            if reviewed != plan.id {
                return Err(SetupRefusal::PlanChanged(Box::new(plan)));
            }
        }
        if !plan.is_ready() {
            return Err(SetupRefusal::Blocked(Box::new(plan)));
        }

        let dry_run = self.dry_run();
        let (id, events) = {
            let mut operations = self.operations.lock().expect("operations lock");
            operations.sequence += 1;
            let id = operations.sequence;
            let events = Arc::new(EventLog::default());
            operations.current = Some(OperationEntry {
                id,
                label: "Project setup".to_string(),
                events: Arc::clone(&events),
                result: String::new(),
            });
            (id, events)
        };

        let gui = Arc::clone(self);
        std::thread::spawn(move || {
            let result = run_setup(&gui, &plan, dry_run, id, &events);
            events.finish();
            let mut operations = gui.operations.lock().expect("operations lock");
            if let Some(current) = operations.current.as_mut().filter(|entry| entry.id == id) {
                current.result = result;
            }
        });
        Ok(id)
    }

    /// Start an operation in the background and return its id.
    ///
    /// Only one logical operation runs at a time: they touch the same working trees, and
    /// queueing a commit behind a push would be confusing rather than useful.
    pub fn start_operation(
        self: &Arc<Self>,
        operation: GuiOperation,
    ) -> std::result::Result<u64, String> {
        let (session, dry_run) = {
            let state = self.state.lock().expect("state lock");
            (
                state.session.clone().ok_or_else(|| {
                    state
                        .error
                        .clone()
                        .unwrap_or_else(|| "no GitMesh project is open".to_string())
                })?,
                state.dry_run,
            )
        };
        if self.is_busy() {
            let running = self.running_label().unwrap_or_default();
            return Err(format!(
                "'{running}' is still running; wait for it to finish"
            ));
        }

        let (id, events) = {
            let mut operations = self.operations.lock().expect("operations lock");
            operations.sequence += 1;
            let id = operations.sequence;
            let events = Arc::new(EventLog::default());
            operations.current = Some(OperationEntry {
                id,
                label: operation.label().to_string(),
                events: Arc::clone(&events),
                result: String::new(),
            });
            (id, events)
        };

        let gui = Arc::clone(self);
        std::thread::spawn(move || {
            let result = run_operation(&session, &operation, dry_run, id, &events);
            events.finish();
            let mut operations = gui.operations.lock().expect("operations lock");
            if let Some(current) = operations.current.as_mut().filter(|entry| entry.id == id) {
                current.result = result;
            }
        });
        Ok(id)
    }

    /// The open project session, if any.
    fn session(&self) -> Option<ProjectSession> {
        self.state.lock().expect("state lock").session.clone()
    }

    /// The event stream of an operation, while it is still retained.
    pub fn events_for(&self, id: u64) -> Option<Arc<EventLog>> {
        self.operations
            .lock()
            .expect("operations lock")
            .current
            .as_ref()
            .filter(|entry| entry.id == id)
            .map(|entry| Arc::clone(&entry.events))
    }

    /// True while an operation is running.
    pub fn is_busy(&self) -> bool {
        self.operations
            .lock()
            .expect("operations lock")
            .current
            .as_ref()
            .map(|entry| !entry.events.is_finished())
            .unwrap_or(false)
    }

    /// Name of the running operation, if any.
    pub fn running_label(&self) -> Option<String> {
        let operations = self.operations.lock().expect("operations lock");
        operations
            .current
            .as_ref()
            .filter(|entry| !entry.events.is_finished())
            .map(|entry| entry.label.clone())
    }

    /// Stored result of a finished (or failed) operation.
    pub fn stored_report(&self, id: u64) -> Option<String> {
        let operations = self.operations.lock().expect("operations lock");
        operations
            .current
            .as_ref()
            .filter(|entry| entry.id == id && !entry.result.is_empty())
            .map(|entry| entry.result.clone())
    }
}

/// Run one operation, streaming progress events, and return the stored result.
///
/// This is the only place that knows how a GUI action maps onto the core API: it builds
/// the same option structs the CLI builds and calls the same functions. It performs no
/// Git work of its own.
fn run_operation(
    session: &ProjectSession,
    operation: &GuiOperation,
    dry_run: bool,
    id: u64,
    events: &Arc<EventLog>,
) -> String {
    let started = Instant::now();
    let repositories: Vec<Json> = session
        .project()
        .sorted_repositories()
        .iter()
        .map(|repo| {
            Json::object([
                ("id", Json::from(repo.id.clone())),
                ("path", Json::from(repo.relative_slash())),
                ("role", Json::from(repo.role.label())),
            ])
        })
        .collect();
    events.push(Json::object([
        ("type", Json::from("started")),
        ("id", Json::from(id as i64)),
        ("operation", Json::from(operation.label())),
        ("sentence", Json::from(operation.sentence())),
        ("dryRun", Json::from(dry_run)),
        ("total", Json::from(repositories.len())),
        ("repositories", Json::array(repositories)),
        ("at", Json::from(elapsed_ms(started))),
    ]));

    // Progress hooks: two closures over the same event log, one per phase.
    let start_sink = Arc::clone(events);
    let end_sink = Arc::clone(events);
    let mut on_start = move |repo: &PhysicalRepository| {
        start_sink.push(Json::object([
            ("type", Json::from("repository")),
            ("phase", Json::from("running")),
            ("id", Json::from(repo.id.clone())),
            ("path", Json::from(repo.relative_slash())),
            ("role", Json::from(repo.role.label())),
            ("at", Json::from(elapsed_ms(started))),
        ]));
    };
    let mut on_end = move |outcome: &RepoOutcome| {
        end_sink.push(Json::object([
            ("type", Json::from("outcome")),
            ("id", Json::from(outcome.id.clone())),
            ("path", Json::from(outcome.path.clone())),
            ("outcome", Json::from(outcome.kind.label())),
            ("symbol", Json::from(outcome.kind.symbol())),
            ("summary", Json::from(outcome.summary.clone())),
            (
                "details",
                Json::array(outcome.details.iter().map(|d| Json::from(d.as_str()))),
            ),
            ("at", Json::from(elapsed_ms(started))),
        ]));
    };
    let mut observer = OperationObserver::silent()
        .on_start(&mut on_start)
        .on_end(&mut on_end);

    match dispatch(session, operation, dry_run, &mut observer) {
        Ok(report) => {
            let model = editor::project_model(session, dry_run);
            let summary = service::operation_view_json(&report, &session.status());
            events.push(Json::object([
                ("type", Json::from("finished")),
                ("id", Json::from(id as i64)),
                ("operation", Json::from(report.operation.clone())),
                ("kind", Json::from(service::report_kind(&report))),
                ("dryRun", Json::from(report.dry_run)),
                ("exitCode", Json::from(report.exit_code() as i64)),
                ("summary", summary.clone()),
                ("model", model.clone()),
                ("at", Json::from(elapsed_ms(started))),
            ]));
            Json::object([
                ("status", Json::from("finished")),
                ("report", summary),
                ("model", model),
            ])
            .to_pretty_string()
        }
        Err(err) => {
            let message = err.to_string();
            events.push(Json::object([
                ("type", Json::from("failed")),
                ("id", Json::from(id as i64)),
                ("message", Json::from(message.clone())),
                (
                    "configuration",
                    Json::from(
                        err.is_configuration_error()
                            || matches!(err, Error::ProjectNotFound { .. }),
                    ),
                ),
                ("at", Json::from(elapsed_ms(started))),
            ]));
            // The model is still refreshed: a failed operation may have changed part of
            // the project, and the interface must show the real state.
            Json::object([
                ("status", Json::from("failed")),
                (
                    "error",
                    Json::object([
                        ("message", Json::from(message)),
                        (
                            "configuration",
                            Json::from(
                                err.is_configuration_error()
                                    || matches!(err, Error::ProjectNotFound { .. }),
                            ),
                        ),
                    ]),
                ),
                ("model", editor::project_model(session, dry_run)),
            ])
            .to_pretty_string()
        }
    }
}

/// Why a setup was not started.
///
/// Each variant carries what the interface needs to react: a message, the plan that
/// changed, or the plan that has blockers.
#[derive(Debug)]
pub enum SetupRefusal {
    /// Another operation is running.
    Busy(String),
    /// Something went wrong before planning (no Git, unreadable directory, ...).
    Refused(String),
    /// The plan differs from the one the user reviewed: review it again.
    PlanChanged(Box<SetupPlan>),
    /// The plan cannot be applied as it is.
    Blocked(Box<SetupPlan>),
}

/// Why a repository-management plan was not started.
///
/// The same four answers as [`SetupRefusal`], for the same reasons: another operation is
/// running, the plan cannot be built, the plan changed under the user's eyes, or the plan
/// has blockers.
#[derive(Debug)]
pub enum ManagementRefusal {
    /// Another operation is running.
    Busy(String),
    /// Something went wrong before planning (no project open, no Git, ...).
    Refused(String),
    /// The plan differs from the one the user reviewed: review it again.
    PlanChanged(Box<RepositoryPlan>),
    /// The plan cannot be applied as it is.
    Blocked(Box<RepositoryPlan>),
}

/// Run a repository-management plan, streaming one event per unit of work.
///
/// Like [`run_setup`], this holds no project logic: it calls [`manage::apply`], which
/// verifies every change afterwards, and reopens the project so the interface shows the
/// configuration that was just written.
fn run_management(
    gui: &Arc<Gui>,
    plan: &RepositoryPlan,
    dry_run: bool,
    id: u64,
    events: &Arc<EventLog>,
) -> String {
    let started = Instant::now();
    let runner = match gui.runner() {
        Ok(runner) => runner,
        Err(error) => {
            events.push(Json::object([
                ("type", Json::from("failed")),
                ("id", Json::from(id as i64)),
                ("message", Json::from(error)),
                ("configuration", Json::from(true)),
                ("at", Json::from(elapsed_ms(started))),
            ]));
            return Json::object([
                ("status", Json::from("failed")),
                (
                    "error",
                    Json::object([
                        ("message", Json::from("git is not available")),
                        ("configuration", Json::from(true)),
                    ]),
                ),
            ])
            .to_pretty_string();
        }
    };

    // The rows the interface shows before the first action starts: the planned units of
    // work, in execution order, so progress is visible from the beginning.
    let rows: Vec<Json> = plan
        .planned_actions()
        .map(|action| {
            Json::object([
                ("id", Json::from(action.row_id())),
                ("path", Json::from(action.path.clone())),
                ("role", Json::from(action.kind.role())),
                ("detail", Json::from(action.detail.clone())),
            ])
        })
        .collect();
    events.push(Json::object([
        ("type", Json::from("started")),
        ("id", Json::from(id as i64)),
        ("operation", Json::from("Repositories")),
        ("sentence", Json::from(plan.summary())),
        ("dryRun", Json::from(dry_run)),
        ("total", Json::from(rows.len())),
        ("repositories", Json::array(rows)),
        ("plan", service::management_plan_view_json(plan)),
        ("at", Json::from(elapsed_ms(started))),
    ]));

    let start_sink = Arc::clone(events);
    let end_sink = Arc::clone(events);
    let mut on_action = move |action: &crate::manage::RepositoryAction| {
        start_sink.push(Json::object([
            ("type", Json::from("repository")),
            ("phase", Json::from("running")),
            ("id", Json::from(action.row_id())),
            ("path", Json::from(action.path.clone())),
            ("role", Json::from(action.kind.role())),
            ("detail", Json::from(action.detail.clone())),
            ("at", Json::from(elapsed_ms(started))),
        ]));
    };
    let mut on_outcome = move |outcome: &crate::manage::RepositoryActionOutcome| {
        end_sink.push(Json::object([
            ("type", Json::from("outcome")),
            ("id", Json::from(outcome.row_id())),
            ("path", Json::from(outcome.path.clone())),
            ("role", Json::from(outcome.kind.role())),
            ("outcome", Json::from(outcome.outcome.label())),
            ("symbol", Json::from(outcome.symbol())),
            ("summary", Json::from(outcome.summary.clone())),
            (
                "details",
                Json::array(outcome.details.iter().map(|d| Json::from(d.as_str()))),
            ),
            ("at", Json::from(elapsed_ms(started))),
        ]));
    };
    let mut observer = manage::RepositoryObserver::silent()
        .on_action(&mut on_action)
        .on_outcome(&mut on_outcome);

    let result = manage::apply(plan, dry_run, &runner, &mut observer);
    let result_json = service::management_result_view_json(&result);

    // The configuration may have changed even when the apply did not fully succeed: some
    // actions write the manifest before a later one fails. The open project is therefore
    // reloaded after every real apply, so the interface shows what is on disk now, not
    // what it was before the run.
    let mut opened = false;
    if !dry_run {
        opened = gui.adopt(&plan.root).is_ok();
    }
    let model = gui.model_json();
    events.push(Json::object([
        ("type", Json::from("finished")),
        ("id", Json::from(id as i64)),
        ("operation", Json::from("Repositories")),
        ("kind", Json::from(result.kind.label())),
        ("dryRun", Json::from(result.dry_run)),
        ("exitCode", Json::from(result.exit_code() as i64)),
        ("repository", result_json.clone()),
        (
            "summary",
            Json::object([(
                "outcome",
                Json::object([
                    ("kind", Json::from(result.kind.label())),
                    ("success", Json::from(result.is_success())),
                    ("exitCode", Json::from(result.exit_code() as i64)),
                ]),
            )]),
        ),
        ("opened", Json::from(opened)),
        ("model", model.clone()),
        ("at", Json::from(elapsed_ms(started))),
    ]));
    Json::object([
        ("status", Json::from("finished")),
        ("repository", result_json),
        ("opened", Json::from(opened)),
        ("model", model),
    ])
    .to_pretty_string()
}

/// Run a setup, streaming one event per step, and open the project when it worked.
///
/// Like [`run_operation`], this contains no project logic of its own: it calls
/// [`setup::apply`] and turns the steps into events.
fn run_setup(
    gui: &Arc<Gui>,
    plan: &SetupPlan,
    dry_run: bool,
    id: u64,
    events: &Arc<EventLog>,
) -> String {
    let started = Instant::now();
    let runner = match gui.runner() {
        Ok(runner) => runner,
        Err(error) => {
            events.push(Json::object([
                ("type", Json::from("failed")),
                ("id", Json::from(id as i64)),
                ("message", Json::from(error)),
                ("configuration", Json::from(true)),
                ("at", Json::from(elapsed_ms(started))),
            ]));
            return Json::object([
                ("status", Json::from("failed")),
                (
                    "error",
                    Json::object([
                        ("message", Json::from("git is not available")),
                        ("configuration", Json::from(true)),
                    ]),
                ),
            ])
            .to_pretty_string();
        }
    };

    // The rows the interface shows before the first step starts: the planned steps, in
    // execution order, so progress is visible from the beginning.
    let rows: Vec<Json> = plan
        .planned_steps()
        .map(|step| {
            Json::object([
                ("id", Json::from(setup_row_id(step.kind, &step.target))),
                ("path", Json::from(step.path.clone())),
                ("role", Json::from(setup_step_role(step.kind))),
                ("detail", Json::from(step.detail.clone())),
            ])
        })
        .collect();
    events.push(Json::object([
        ("type", Json::from("started")),
        ("id", Json::from(id as i64)),
        ("operation", Json::from("Project setup")),
        (
            "sentence",
            Json::from(format!("Creating the project '{}'", plan.name)),
        ),
        ("dryRun", Json::from(dry_run)),
        ("total", Json::from(rows.len())),
        ("repositories", Json::array(rows)),
        ("plan", service::setup_plan_view_json(plan)),
        ("at", Json::from(elapsed_ms(started))),
    ]));

    let start_sink = Arc::clone(events);
    let end_sink = Arc::clone(events);
    let mut on_step = move |step: &crate::setup::SetupStep| {
        start_sink.push(Json::object([
            ("type", Json::from("repository")),
            ("phase", Json::from("running")),
            ("id", Json::from(setup_row_id(step.kind, &step.target))),
            ("path", Json::from(step.path.clone())),
            ("role", Json::from(setup_step_role(step.kind))),
            ("detail", Json::from(step.detail.clone())),
            ("at", Json::from(elapsed_ms(started))),
        ]));
    };
    let mut on_outcome = move |outcome: &crate::setup::SetupStepOutcome| {
        end_sink.push(Json::object([
            ("type", Json::from("outcome")),
            (
                "id",
                Json::from(setup_row_id(outcome.kind, &outcome.target)),
            ),
            ("path", Json::from(outcome.path.clone())),
            ("role", Json::from(setup_step_role(outcome.kind))),
            ("outcome", Json::from(outcome.outcome.label())),
            ("symbol", Json::from(outcome.symbol())),
            ("summary", Json::from(outcome.summary.clone())),
            (
                "details",
                Json::array(outcome.details.iter().map(|d| Json::from(d.as_str()))),
            ),
            ("at", Json::from(elapsed_ms(started))),
        ]));
    };
    let mut observer = SetupObserver::silent()
        .on_step(&mut on_step)
        .on_outcome(&mut on_outcome);

    let result = setup::apply(plan, dry_run, &runner, &mut observer);
    let setup_json = service::setup_result_view_json(&result);

    // A complete setup hands the interface to the project it just created, with no restart.
    // "Complete" means the project validates: a run where every step was already in place
    // opens the existing project just as well, and a partial one stays in the wizard with
    // the failing steps on screen, because adopting a half-built project would hide what
    // still has to be fixed.
    let adoptable = result.is_success()
        && !dry_run
        && result
            .validation
            .as_ref()
            .is_some_and(|validation| validation.ok);
    let opened = if adoptable {
        gui.adopt(&plan.root).is_ok()
    } else {
        false
    };

    // The plan's follow-up, if it has one: one commit, one push, through the ordinary
    // operations and only in the repositories the plan listed.
    let mut publish_json = Json::Null;
    if opened {
        if let Some(publish) = plan.first_publish() {
            if let Some(session) = gui.session() {
                publish_json = run_first_publish(&session, &publish, dry_run, events, started);
            }
        }
    }

    let model = gui.model_json();
    events.push(Json::object([
        ("type", Json::from("finished")),
        ("id", Json::from(id as i64)),
        ("operation", Json::from("Project setup")),
        ("kind", Json::from(result.kind.label())),
        ("dryRun", Json::from(result.dry_run)),
        ("exitCode", Json::from(result.exit_code() as i64)),
        ("setup", setup_json.clone()),
        (
            "summary",
            Json::object([(
                "outcome",
                Json::object([
                    ("kind", Json::from(result.kind.label())),
                    ("success", Json::from(result.is_success())),
                    ("exitCode", Json::from(result.exit_code() as i64)),
                ]),
            )]),
        ),
        ("opened", Json::from(opened)),
        ("publish", publish_json.clone()),
        ("model", model.clone()),
        ("at", Json::from(elapsed_ms(started))),
    ]));
    Json::object([
        ("status", Json::from("finished")),
        ("setup", setup_json),
        ("publish", publish_json),
        ("opened", Json::from(opened)),
        ("model", model),
    ])
    .to_pretty_string()
}

/// Row id of one setup step.
///
/// Two steps can concern the same target — the metadata directory and the manifest are
/// both `manifest` — so the kind is part of the id and every row stays its own row.
fn setup_row_id(kind: SetupStepKind, target: &str) -> String {
    format!("{}:{target}", kind.label())
}

/// Short role label of a setup step, the way the interface names the row.
fn setup_step_role(kind: SetupStepKind) -> &'static str {
    match kind {
        SetupStepKind::CreateMetadataDir => "Metadata",
        SetupStepKind::CreateRepository => "Repository",
        SetupStepKind::ConfigureRemote => "Remote",
        SetupStepKind::AdoptRemoteHistory => "History",
        SetupStepKind::UntrackFromRoot => "Root index",
        SetupStepKind::WriteManifest => "Manifest",
    }
}

/// Run the plan's first publish: one ordinary commit, then one ordinary push.
///
/// Nothing is invented here. The commit and the push go through the same core operations
/// the buttons in the project view use, restricted to the repositories the plan listed, so
/// staging rules, the single message, upstream handling, conflict reporting and partial
/// failures behave exactly as they do everywhere else in GitMesh.
fn run_first_publish(
    session: &ProjectSession,
    publish: &crate::setup::FirstPublish,
    dry_run: bool,
    events: &Arc<EventLog>,
    started: Instant,
) -> Json {
    /// Which of the two ordinary operations a round is running.
    enum Stage {
        Commit,
        Push,
    }

    let selection = RepositorySelection::Ids(publish.repositories.clone());
    let mut commit_options = CommitOptions::new(publish.message.clone());
    commit_options.selection = selection.clone();
    commit_options.dry_run = dry_run;
    // A repository the first commit does not touch is not reported: after a setup most
    // repositories are simply clean, and listing them all would bury the useful rows.
    commit_options.quiet_clean = true;
    let push_options = PushOptions {
        selection,
        dry_run,
        ..PushOptions::default()
    };

    let mut sections: Vec<Json> = Vec::new();
    for (label, prefix, stage) in [
        ("First commit", "commit", Stage::Commit),
        ("First push", "push", Stage::Push),
    ] {
        let mut on_start = publish_start(events, label, prefix, started);
        let mut on_end = publish_finish(events, label, prefix, started);
        let mut observer = OperationObserver::silent()
            .on_start(&mut on_start)
            .on_end(&mut on_end);
        let result = match stage {
            Stage::Commit => session.commit_observed(&commit_options, &mut observer),
            Stage::Push => session.push_observed(&push_options, &mut observer),
        };
        // One flat section per operation: the interface counts outcomes and looks for
        // problems without having to know how an operation report is nested, and the same
        // shape is what `/api/commit` and `/api/push` return.
        sections.push(match result {
            Ok(report) => Json::object([
                ("operation", Json::from(label)),
                (
                    "outcome",
                    Json::object([
                        ("kind", Json::from(service::report_kind(&report))),
                        ("success", Json::from(report.is_success())),
                        ("exitCode", Json::from(report.exit_code() as i64)),
                    ]),
                ),
                (
                    "counts",
                    Json::object([
                        ("succeeded", Json::from(report.success().count() as i64)),
                        ("skipped", Json::from(report.skipped().count() as i64)),
                        ("conflicted", Json::from(report.conflicts().count() as i64)),
                        ("failed", Json::from(report.failures().count() as i64)),
                    ]),
                ),
                (
                    "outcomes",
                    Json::array(report.outcomes.iter().map(|outcome| {
                        Json::object([
                            ("id", Json::from(outcome.id.clone())),
                            ("path", Json::from(outcome.path.clone())),
                            ("outcome", Json::from(outcome.kind.label())),
                            ("symbol", Json::from(outcome.kind.symbol())),
                            ("summary", Json::from(outcome.summary.clone())),
                            (
                                "details",
                                Json::array(
                                    outcome.details.iter().map(|line| Json::from(line.as_str())),
                                ),
                            ),
                        ])
                    })),
                ),
            ]),
            Err(err) => Json::object([
                ("operation", Json::from(label)),
                ("error", Json::from(err.to_string())),
            ]),
        });
    }
    Json::array(sections)
}

/// Progress hook for the first publish, when a repository starts.
///
/// The id is prefixed so these rows stay apart from the setup rows: the same repository
/// appears in both lists, and the interface keys rows by id.
fn publish_start(
    events: &Arc<EventLog>,
    label: &str,
    prefix: &str,
    started: Instant,
) -> impl Fn(&PhysicalRepository) {
    let sink = Arc::clone(events);
    let label = label.to_string();
    let prefix = prefix.to_string();
    move |repo: &PhysicalRepository| {
        sink.push(publish_event(
            "repository",
            &prefix,
            &label,
            &repo.id,
            &repo.relative_slash(),
            started,
            None,
        ));
    }
}

/// Progress hook for the first publish, when a repository is done.
fn publish_finish(
    events: &Arc<EventLog>,
    label: &str,
    prefix: &str,
    started: Instant,
) -> impl Fn(&RepoOutcome) {
    let sink = Arc::clone(events);
    let label = label.to_string();
    let prefix = prefix.to_string();
    move |outcome: &RepoOutcome| {
        sink.push(publish_event(
            "outcome",
            &prefix,
            &label,
            &outcome.id,
            &outcome.path,
            started,
            Some(outcome),
        ));
    }
}

/// One row of the first-publish progress list.
fn publish_event(
    kind: &str,
    prefix: &str,
    label: &str,
    id: &str,
    path: &str,
    started: Instant,
    outcome: Option<&RepoOutcome>,
) -> Json {
    let mut fields: Vec<(String, Json)> = vec![
        ("type".into(), Json::from(kind)),
        ("id".into(), Json::from(format!("{prefix}:{id}"))),
        ("path".into(), Json::from(path.to_string())),
        ("role".into(), Json::from(label.to_string())),
        ("at".into(), Json::from(elapsed_ms(started))),
    ];
    match outcome {
        None => fields.push(("phase".into(), Json::from("running"))),
        Some(outcome) => {
            fields.push(("outcome".into(), Json::from(outcome.kind.label())));
            fields.push(("symbol".into(), Json::from(outcome.kind.symbol())));
            fields.push(("summary".into(), Json::from(outcome.summary.clone())));
            fields.push((
                "details".into(),
                Json::array(outcome.details.iter().map(|d| Json::from(d.as_str()))),
            ));
        }
    }
    Json::object(fields)
}

/// Map one GUI operation onto the existing core operations.
fn dispatch(
    session: &ProjectSession,
    operation: &GuiOperation,
    dry_run: bool,
    observer: &mut OperationObserver<'_>,
) -> Result<OperationReport> {
    match operation {
        GuiOperation::Commit { message } => {
            let mut options = CommitOptions::new(message.clone());
            options.dry_run = dry_run;
            session.commit_observed(&options, observer)
        }
        GuiOperation::BranchCreate { name } => session.branch_observed(
            &BranchAction::Create { name: name.clone() },
            &branch_options(dry_run),
            observer,
        ),
        GuiOperation::BranchCheckout { name } => session.branch_observed(
            &BranchAction::Checkout {
                name: name.clone(),
                create: false,
            },
            &branch_options(dry_run),
            observer,
        ),
        GuiOperation::BranchStart { name } => session.branch_observed(
            &BranchAction::Checkout {
                name: name.clone(),
                create: true,
            },
            &branch_options(dry_run),
            observer,
        ),
        GuiOperation::BranchMerge { name } => session.branch_observed(
            &BranchAction::Merge { name: name.clone() },
            &branch_options(dry_run),
            observer,
        ),
        GuiOperation::BranchDelete { name, force } => session.branch_observed(
            &BranchAction::Delete { name: name.clone() },
            &BranchOptions {
                force: *force,
                ..branch_options(dry_run)
            },
            observer,
        ),
        GuiOperation::Fetch => {
            let mut options = SyncOptions::new();
            options.dry_run = dry_run;
            session.sync_observed(&options, true, observer)
        }
        GuiOperation::Pull { strategy } => {
            let options = SyncOptions {
                strategy: *strategy,
                dry_run,
                ..SyncOptions::new()
            };
            session.sync_observed(&options, false, observer)
        }
        GuiOperation::Push => {
            let options = PushOptions {
                dry_run,
                ..PushOptions::default()
            };
            session.push_observed(&options, observer)
        }
    }
}

fn branch_options(dry_run: bool) -> BranchOptions {
    BranchOptions {
        dry_run,
        ..BranchOptions::default()
    }
}

fn elapsed_ms(start: Instant) -> i64 {
    start.elapsed().as_millis() as i64
}

/// Role of a repository (re-exported so the interface never invents its own labels).
pub fn role_label(role: RepositoryRole) -> &'static str {
    role.label()
}

/// Symbol of an outcome (same symbols as the CLI).
pub fn outcome_symbol(kind: OutcomeKind) -> &'static str {
    kind.symbol()
}

/// Parse a pull strategy from the interface.
pub fn parse_strategy(value: &str) -> std::result::Result<PullStrategy, String> {
    match value {
        "ff-only" | "" => Ok(PullStrategy::FastForwardOnly),
        "merge" => Ok(PullStrategy::Merge),
        "rebase" => Ok(PullStrategy::Rebase),
        other => Err(format!("unknown pull strategy '{other}'")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;
    use std::time::Duration;

    fn gui(fixture: &RepoFixture, dry_run: bool) -> Arc<Gui> {
        Arc::new(Gui::new(fixture.path().to_path_buf(), dry_run))
    }

    /// Run an operation to completion and return every event it produced.
    fn collect(gui: &Arc<Gui>, id: u64) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut events = Vec::new();
        loop {
            if let Some(log) = gui.events_for(id) {
                events.extend(log.drain());
                if log.is_finished() && events.iter().any(|e| e.contains("\"finished\"")) {
                    events.extend(log.drain());
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "operation {id} did not finish: {events:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        events
    }

    #[test]
    fn gui_opens_a_project_and_builds_a_model() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let gui = gui(&fixture, false);
        assert_eq!(
            gui.opened_project().map(|(name, _)| name),
            Some("demo".into())
        );
        assert!(gui.open_error().is_none());
        let model = gui.model();
        assert!(model.contains("\"kind\": \"project\""));
        assert!(model.contains("\"name\": \"demo\""));
    }

    #[test]
    fn gui_without_a_project_reports_a_clear_error_and_can_open_one() {
        let fixture = RepoFixture::named("demo");
        // Outside the project: a nested directory would still be *inside* the project,
        // which is correct behaviour (the CLI works from any subdirectory too).
        let empty = fixture.outside_path().join("elsewhere");
        std::fs::create_dir_all(&empty).unwrap();
        let gui = Arc::new(Gui::new(empty.clone(), false));
        assert!(gui.opened_project().is_none());
        let error = gui.open_error().expect("error");
        assert!(error.contains("no GitMesh project found"), "{error}");
        assert!(gui.model().contains("\"kind\": \"no-project\""));

        // Opening an existing project from the interface works.
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        gui.open(fixture.path()).expect("open project");
        assert_eq!(
            gui.opened_project().map(|(name, _)| name),
            Some("demo".into())
        );
        assert!(gui.open_error().is_none());
        assert!(gui.model().contains("\"kind\": \"project\""));

        // A non-project directory is reported, and no project stays open.
        let result = gui.open(&empty);
        assert!(result.is_err());
        assert!(gui.model().contains("\"kind\": \"no-project\""));
    }

    #[test]
    fn commit_operation_streams_progress_and_returns_a_model() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let gui = gui(&fixture, false);
        let id = gui
            .start_operation(GuiOperation::Commit {
                message: "one logical commit".into(),
            })
            .expect("operation starts");
        let events = collect(&gui, id).join("\n");

        // Progress named both repositories before touching them.
        assert!(events.contains("\"type\":\"started\""), "{events}");
        assert!(events.contains("\"sentence\":\"Committing the project\""));
        assert!(events.contains("\"type\":\"repository\",\"phase\":\"running\",\"id\":\"root\""));
        assert!(events.contains("\"type\":\"repository\",\"phase\":\"running\",\"id\":\"engine\""));
        assert!(events.contains("\"type\":\"outcome\",\"id\":\"engine\""));
        assert!(events.contains("\"outcome\":\"success\""));
        assert!(events.contains("\"type\":\"finished\""));

        // The result carries the refreshed model and the core's summary lines.
        let stored = gui.stored_report(id).expect("stored report");
        assert!(stored.contains("\"operation\": \"commit\""), "{stored}");
        assert!(stored.contains("\"key\": \"clean\""), "{stored}");
        assert!(gui.model().contains("\"key\": \"clean\""));

        // And the commit is real in both repositories.
        assert!(fixture
            .git_ok(".", &["log", "-1", "--pretty=%s"])
            .contains("one logical commit"));
        assert!(fixture
            .git_ok("engine", &["log", "-1", "--pretty=%s"])
            .contains("one logical commit"));
    }

    /// A plain directory outside the project, with files but no Git: the wizard's input.
    fn plain_directory(fixture: &RepoFixture, label: &str) -> PathBuf {
        let path = fixture.outside_path().join(label);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::create_dir_all(path.join("engine")).unwrap();
        std::fs::write(path.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(path.join("engine/lib.rs"), "pub fn go() {}\n").unwrap();
        path
    }

    fn setup_request(root: &Path) -> SetupRequest {
        SetupRequest {
            root: root.to_path_buf(),
            name: "MyProject".into(),
            create_root_repository: true,
            set_git_remote: true,
            repositories: vec![crate::setup::RepositoryRequest {
                path: "engine".into(),
                create: true,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_wizard_inspects_plans_and_setups_a_project_then_opens_it() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_directory(&fixture, "MyProject");
        let gui = Arc::new(Gui::new(plain.clone(), false));

        // Inspecting a directory that is not a project is read-only and says so.
        let inspection = gui.inspect_directory(&plain).expect("inspect");
        assert!(!inspection.is_gitmesh_project);
        assert_eq!(inspection.suggested_name, "MyProject");
        assert!(
            !plain.join(".gitmesh").exists(),
            "inspecting created nothing"
        );

        // Planning is read-only too, and refuses to run a plan nobody reviewed.
        let request = setup_request(&plain);
        let plan = gui.plan_setup(&request).expect("plan");
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(!plain.join(".gitmesh").exists(), "planning created nothing");
        match gui.start_setup(request.clone(), Some("some-other-plan")) {
            Err(SetupRefusal::PlanChanged(reviewed)) => assert!(reviewed.is_ready()),
            other => panic!("a plan that changed must be refused, got {other:?}"),
        }
        assert!(
            !plain.join(".gitmesh").exists(),
            "a refused plan created nothing"
        );

        // The confirmed plan runs, streams its steps, and opens the project.
        let id = gui
            .start_setup(request.clone(), Some(&plan.id))
            .expect("setup starts");
        let events = collect(&gui, id).join("\n");
        assert!(
            events.contains("\"operation\":\"Project setup\""),
            "{events}"
        );
        assert!(events.contains("\"type\":\"outcome\""), "{events}");
        assert!(
            events.contains("\"id\":\"create-repository:engine\""),
            "{events}"
        );
        assert!(events.contains("\"role\":\"Manifest\""), "{events}");
        assert!(events.contains("\"opened\":true"), "{events}");

        let model = gui.model();
        assert!(model.contains("\"kind\": \"project\""), "{model}");
        assert!(model.contains("\"name\": \"MyProject\""), "{model}");
        assert!(plain.join(".gitmesh/project.toml").is_file());
        assert!(plain.join("engine/.git/HEAD").is_file());

        // A second run reports what already exists instead of redoing it.
        let again = gui.plan_setup(&request).expect("plan again");
        assert!(again.created_repositories().count() == 0);
        assert!(
            again.already_satisfied().count() >= 2,
            "{}",
            again.summary()
        );
    }

    #[test]
    fn a_blocked_setup_is_refused_with_the_plan_that_explains_why() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_directory(&fixture, "Blocked");
        let gui = Arc::new(Gui::new(plain.clone(), false));

        // Two selections that overlap cannot both own the same directory.
        let mut request = setup_request(&plain);
        request.repositories = vec![
            crate::setup::RepositoryRequest {
                path: "engine".into(),
                create: true,
                ..Default::default()
            },
            crate::setup::RepositoryRequest {
                path: "engine/src".into(),
                create: true,
                ..Default::default()
            },
        ];
        match gui.start_setup(request, None) {
            Err(SetupRefusal::Blocked(plan)) => {
                assert!(!plan.is_ready());
                assert!(!plan.blockers.is_empty());
            }
            other => panic!("expected a blocked plan, got {other:?}"),
        }
        assert!(!plain.join(".gitmesh").exists(), "nothing was created");
    }

    #[test]
    fn the_first_publish_goes_through_the_ordinary_commit_and_push() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_directory(&fixture, "Publish");
        let remote = fixture.create_bare("publish-engine.git");
        let gui = Arc::new(Gui::new(plain.clone(), false));

        let mut request = setup_request(&plain);
        request.repositories = vec![crate::setup::RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some(remote.to_string_lossy().to_string()),
            ..Default::default()
        }];
        request.publish_first_commit = Some("Initial commit".into());

        let plan = gui.plan_setup(&request).expect("plan");
        let publish = plan
            .first_publish()
            .expect("the plan promises a first publish");
        assert_eq!(publish.repositories, ["engine"]);
        let id = gui
            .start_setup(request, Some(&plan.id))
            .expect("setup starts");
        let events = collect(&gui, id).join("\n");

        // The follow-up is visible as its own rows, clearly labelled.
        assert!(events.contains("\"role\":\"First commit\""), "{events}");
        assert!(events.contains("\"role\":\"First push\""), "{events}");
        assert!(events.contains("\"id\":\"push:engine\""), "{events}");
        let stored = gui.stored_report(id).expect("stored report");
        assert!(stored.contains("\"publish\""), "{stored}");

        // The commit is a real commit with the message the plan promised, and it is on the
        // remote: the ordinary operations did the work, and they did it from the plan.
        let engine = plain.join("engine");
        assert!(engine.join(".git").is_dir());
        let runner = fixture.runner();
        let local = runner
            .repo(&engine)
            .run_checked(&["log", "-1", "--pretty=%s"])
            .expect("log in the created repository");
        assert!(
            local.contains("Initial commit"),
            "the first commit carries the planned message: {local}"
        );
        let pushed = runner
            .repo(&remote)
            .run_checked(&["log", "-1", "--pretty=%s"])
            .expect("log in the bare remote");
        assert!(
            pushed.contains("Initial commit"),
            "the first commit reached the remote: {pushed}"
        );
    }

    #[test]
    fn dry_run_operations_change_nothing_but_still_report() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/lib.rs", "y");
        let gui = gui(&fixture, true);
        assert!(gui.dry_run());
        let id = gui
            .start_operation(GuiOperation::Commit {
                message: "simulated".into(),
            })
            .expect("operation starts");
        let events = collect(&gui, id).join("\n");
        assert!(events.contains("\"dryRun\":true"), "{events}");
        assert!(!fixture
            .git_ok("engine", &["log", "--oneline"])
            .contains("simulated"));
    }

    #[test]
    fn a_second_operation_is_refused_while_one_is_running() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("engine/lib.rs", "y");
        let gui = gui(&fixture, false);
        let first = gui
            .start_operation(GuiOperation::Commit {
                message: "first".into(),
            })
            .expect("first starts");
        let second = gui.start_operation(GuiOperation::Push);
        match second {
            Err(message) => assert!(message.contains("still running"), "{message}"),
            Ok(id) => {
                // The first operation finished before the second was requested.
                collect(&gui, id);
            }
        }
        collect(&gui, first);
    }

    #[test]
    fn operations_refuse_to_start_without_a_project() {
        let fixture = RepoFixture::named("demo");
        let empty = fixture.outside_path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let gui = Arc::new(Gui::new(empty, false));
        let error = gui
            .start_operation(GuiOperation::Fetch)
            .expect_err("no project open");
        assert!(error.contains("no GitMesh project found"), "{error}");
    }

    #[test]
    fn conflicts_are_reported_as_conflicts_and_do_not_hide_the_other_repositories() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "lib.rs");
        fixture.write("src/main.rs", "root change");
        let gui = gui(&fixture, false);
        let id = gui
            .start_operation(GuiOperation::Commit {
                message: "partial".into(),
            })
            .expect("starts");
        let events = collect(&gui, id).join("\n");
        assert!(events.contains("\"outcome\":\"conflict\""), "{events}");
        assert!(events.contains("\"outcome\":\"success\""));
        assert!(events.contains("\"kind\":\"partial\""));
        assert!(events.contains("\"exitCode\":1"));
        // The root repository still received its commit.
        assert!(fixture
            .git_ok(".", &["log", "-1", "--pretty=%s"])
            .contains("partial"));
    }

    #[test]
    fn branch_operations_run_through_the_interface() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let gui = gui(&fixture, false);
        let id = gui
            .start_operation(GuiOperation::BranchStart {
                name: "feature/gui".into(),
            })
            .expect("starts");
        let events = collect(&gui, id).join("\n");
        assert!(events.contains("\"outcome\":\"success\""), "{events}");
        assert_eq!(
            fixture
                .git_ok("engine", &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "feature/gui"
        );
        assert_eq!(
            fixture
                .git_ok(".", &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "feature/gui"
        );
    }

    #[test]
    fn sync_operations_refuse_divergence_instead_of_merging() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.publish(".", "root.git");
        fixture.publish("engine", "engine.git");
        let other = fixture.clone_outside(&fixture.bare_path("engine.git"), "other");
        std::fs::write(other.join("lib.rs"), "theirs\n").unwrap();
        fixture
            .runner()
            .repo(&other)
            .run_checked(&["add", "-A"])
            .unwrap();
        fixture
            .runner()
            .repo(&other)
            .run_checked(&["commit", "-q", "-m", "theirs"])
            .unwrap();
        fixture
            .runner()
            .repo(&other)
            .run_checked(&["push", "origin", "HEAD"])
            .unwrap();
        fixture.write("engine/lib.rs", "ours\n");
        let gui = gui(&fixture, false);
        let commit = gui
            .start_operation(GuiOperation::Commit {
                message: "ours".into(),
            })
            .expect("starts");
        collect(&gui, commit);

        let id = gui
            .start_operation(GuiOperation::Pull {
                strategy: PullStrategy::FastForwardOnly,
            })
            .expect("starts");
        let events = collect(&gui, id).join("\n");
        // The diverged repository failed with an explanation, the clean one succeeded.
        assert!(events.contains("\"outcome\":\"failed\""), "{events}");
        assert!(events.contains("\"outcome\":\"success\""));
        assert!(
            events.contains("will not merge or rebase automatically") || events.contains("diverg")
        );
    }

    #[test]
    fn pull_strategies_are_parsed_explicitly() {
        assert_eq!(parse_strategy("").unwrap(), PullStrategy::FastForwardOnly);
        assert_eq!(
            parse_strategy("ff-only").unwrap(),
            PullStrategy::FastForwardOnly
        );
        assert_eq!(parse_strategy("merge").unwrap(), PullStrategy::Merge);
        assert_eq!(parse_strategy("rebase").unwrap(), PullStrategy::Rebase);
        assert!(parse_strategy("whatever").is_err());
    }

    #[test]
    fn outcomes_and_symbols_come_from_the_core() {
        assert_eq!(outcome_symbol(OutcomeKind::Success), "✓");
        assert_eq!(outcome_symbol(OutcomeKind::Conflict), "!");
        assert_eq!(role_label(RepositoryRole::Root), "root");
        assert_eq!(role_label(RepositoryRole::External), "external");
    }

    /// The busy guard protects the session, not only the buttons: while an operation runs,
    /// opening another project must be refused, and the interface must stay on the project
    /// it is working on.
    #[test]
    fn another_project_cannot_be_opened_while_an_operation_is_running() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        for index in 0..6 {
            fixture.write(&format!("engine/file{index}.rs"), "// change\n");
        }
        let gui = gui(&fixture, false);
        let elsewhere = plain_directory(&fixture, "Elsewhere");
        let (project, root) = gui.opened_project().expect("a project is open");

        let id = gui
            .start_operation(GuiOperation::Commit {
                message: "while busy".into(),
            })
            .expect("the commit starts");

        // The window is small but real: a commit spawns several Git processes.
        let mut refusals = 0;
        for _ in 0..200 {
            if gui.is_busy() {
                if let Err(message) = gui.open(&elsewhere) {
                    assert!(message.contains("still running"), "{message}");
                    refusals += 1;
                }
                let (current, current_root) = gui.opened_project().expect("still open");
                assert_eq!(current, project, "the open project never changed under us");
                assert_eq!(current_root, root);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(refusals > 0, "the busy guard was never observed");
        collect(&gui, id);

        // Once the operation is done, opening a project works again.
        assert!(
            gui.open(&elsewhere).is_err(),
            "a directory without a manifest is not a project, but it is looked at"
        );
        gui.open(&root).expect("the same project opens again");
    }

    /// A setup whose steps are all already satisfied still ends in the project: the user
    /// asked for this configuration, and that is what is on disk now.
    #[test]
    fn running_the_setup_twice_opens_the_project_without_changing_it() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_directory(&fixture, "MyProject");
        let gui = Arc::new(Gui::new(plain.clone(), false));
        let request = setup_request(&plain);

        let plan = gui.plan_setup(&request).expect("plan");
        let id = gui
            .start_setup(request.clone(), Some(&plan.id))
            .expect("setup starts");
        let events = collect(&gui, id).join("\n");
        assert!(events.contains("\"opened\":true"), "{events}");
        let manifest =
            std::fs::read_to_string(plain.join(".gitmesh/project.toml")).expect("manifest");

        let again = gui.plan_setup(&request).expect("plan again");
        assert!(again.is_noop(), "{}", again.summary());
        let id = gui
            .start_setup(request.clone(), Some(&again.id))
            .expect("the second setup starts");
        let events = collect(&gui, id).join("\n");
        assert!(
            events.contains("\"opened\":true"),
            "a second run stays in the project: {events}"
        );
        assert!(
            events.contains("\"kind\":\"complete\""),
            "every step was already in place: {events}"
        );
        assert_eq!(
            std::fs::read_to_string(plain.join(".gitmesh/project.toml")).expect("manifest"),
            manifest,
            "the manifest is untouched by the second run"
        );
        let model = gui.model();
        assert!(model.contains("\"name\": \"MyProject\""), "{model}");
    }

    /// The wizard's "configure remotes" answer is honoured end to end: the remote ends up
    /// in the manifest, Git is left alone, and the interface does not claim a push it
    /// cannot make.
    #[test]
    fn setup_without_remote_configuration_records_the_remote_only() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_directory(&fixture, "MyProject");
        let gui = Arc::new(Gui::new(plain.clone(), false));
        let mut request = setup_request(&plain);
        request.set_git_remote = false;
        request.repositories[0].remote = Some("git@github.com:acme/engine.git".into());
        request.publish_first_commit = Some("Initial commit".into());

        let plan = gui.plan_setup(&request).expect("plan");
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(plan.first_publish().is_none(), "nothing can be pushed");
        let id = gui
            .start_setup(request.clone(), Some(&plan.id))
            .expect("setup starts");
        let events = collect(&gui, id).join("\n");
        assert!(events.contains("\"opened\":true"), "{events}");
        assert!(
            !events.contains("\"operation\":\"First push\""),
            "no push was attempted: {events}"
        );
        assert!(
            plain.join("engine/.git/HEAD").is_file(),
            "the repository was still created"
        );
        let manifest =
            std::fs::read_to_string(plain.join(".gitmesh/project.toml")).expect("manifest");
        assert!(manifest.contains("acme/engine.git"), "{manifest}");
    }
}
