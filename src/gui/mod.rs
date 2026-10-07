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
use crate::json::Json;
use crate::model::{PhysicalRepository, RepositoryRole};
use crate::ops::{
    BranchAction, BranchOptions, CommitOptions, OperationObserver, OperationReport, OutcomeKind,
    PullStrategy, PushOptions, RepoOutcome, SyncOptions,
};
use crate::service::{self, ProjectSession};

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
        Gui {
            state: Mutex::new(GuiState {
                start,
                session,
                error,
                configuration,
                dry_run,
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
        let state = self.state.lock().expect("state lock");
        match &state.session {
            Some(session) => editor::project_model(session, state.dry_run).to_pretty_string(),
            None => editor::empty_model(&state.start, state.error.as_deref(), state.configuration)
                .to_pretty_string(),
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
}
