//! Minimal HTTP/1.1 server for the local interface.
//!
//! Deliberately small and dependency-free:
//!
//! * one thread per connection (the interface serves a browser on one machine),
//! * GET for the embedded assets and read-only endpoints,
//! * POST for operations, with `application/x-www-form-urlencoded` bodies (the browser
//!   sends those natively through `URLSearchParams`, and parsing them needs no JSON
//!   parser on the Rust side),
//! * server-sent events for operation progress, so a long Git operation never blocks
//!   the interface and never requires polling.
//!
//! Two protections matter for a local tool that can commit and push:
//!
//! * if a request carries an `Origin` header, it must match the `Host` header — a web
//!   page on another site cannot drive the interface (cross-site request forgery);
//! * responses carry `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`,
//!   because they describe the state of a working tree.
//!
//! Nothing here knows about projects, repositories or Git: it routes to [`Gui`].

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::gui::{asset, Gui, GuiOperation, ManagementRefusal, SetupRefusal};
use crate::manage::{RepositoryIntent, RepositoryManagementRequest};
use crate::setup::{RepositoryRequest, SetupRequest};

/// Maximum size of a request head, in bytes.
const MAX_HEAD: usize = 16 * 1024;
/// Maximum size of a request body, in bytes (a path and a commit message).
const MAX_BODY: usize = 256 * 1024;
/// How long an idle event stream is kept open before it is closed.
const STREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Bind the listening socket.
pub fn bind(host: &str, port: u16) -> Result<TcpListener> {
    let address = format!("{host}:{port}");
    let listener = TcpListener::bind(&address).map_err(|e| {
        Error::Other(format!(
            "could not bind {address}: {e}. Use --port to choose another port."
        ))
    })?;
    Ok(listener)
}

/// Serve until the process is stopped.
pub fn serve(
    listener: TcpListener,
    gui: Arc<Gui>,
    host: &str,
    extra_hosts: &[String],
) -> Result<()> {
    let allowed_hosts = allowed_hosts(host, extra_hosts);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let gui = Arc::clone(&gui);
                let allowed = allowed_hosts.clone();
                std::thread::spawn(move || {
                    // A connection that fails is not fatal: the interface reloads.
                    let _ = handle(stream, &gui, &allowed);
                });
            }
            Err(err) => {
                eprintln!("gitmesh gui: connection error: {err}");
            }
        }
    }
    Ok(())
}

/// Host names that may appear in the `Host` header for this server.
///
/// `extra` holds hosts the user explicitly allowed (`--allow-host`), for the cases where
/// the interface is reached through a proxy or a port forward: the browser then sends
/// that name in `Host`, and refusing it would make the page unusable. Nothing is allowed
/// implicitly — an unexpected host is still answered with 421.
fn allowed_hosts(host: &str, extra: &[String]) -> Vec<String> {
    let mut hosts = vec![
        "127.0.0.1".to_string(),
        "localhost".to_string(),
        "[::1]".to_string(),
    ];
    if host != "0.0.0.0" && host != "::" && host != "[::]" && !hosts.iter().any(|k| k == host) {
        hosts.push(host.to_string());
    }
    for name in extra {
        let name = name.trim();
        if !name.is_empty() && !hosts.iter().any(|known| known == name) {
            hosts.push(name.to_string());
        }
    }
    hosts
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Path without the query string.
    fn path(&self) -> &str {
        match self.target.split_once('?') {
            Some((path, _)) => path,
            None => &self.target,
        }
    }
}

struct Response {
    status: u16,
    reason: &'static str,
    content_type: &'static str,
    body: Vec<u8>,
    /// Extra headers (used by the event stream).
    headers: Vec<(String, String)>,
}

impl Response {
    fn new(status: u16, reason: &'static str, content_type: &'static str, body: String) -> Self {
        Response {
            status,
            reason,
            content_type,
            body: body.into_bytes(),
            headers: Vec::new(),
        }
    }

    fn json(body: String) -> Self {
        Response::new(200, "OK", "application/json; charset=utf-8", body)
    }

    fn html(body: &'static str) -> Self {
        Response::new(200, "OK", "text/html; charset=utf-8", body.to_string())
    }

    fn css(body: &'static str) -> Self {
        Response::new(200, "OK", "text/css; charset=utf-8", body.to_string())
    }

    fn js(body: &'static str) -> Self {
        Response::new(
            200,
            "OK",
            "application/javascript; charset=utf-8",
            body.to_string(),
        )
    }

    fn error(status: u16, reason: &'static str, message: &str) -> Self {
        Response::json(
            crate::json::Json::object([
                ("error", crate::json::Json::from(message.to_string())),
                ("status", crate::json::Json::from(status as i64)),
            ])
            .to_pretty_string(),
        )
        .with_status(status, reason)
    }

    fn with_status(mut self, status: u16, reason: &'static str) -> Self {
        self.status = status;
        self.reason = reason;
        self
    }

    fn empty(status: u16, reason: &'static str) -> Self {
        Response::new(status, reason, "text/plain; charset=utf-8", String::new())
    }
}

fn handle(mut stream: TcpStream, gui: &Arc<Gui>, allowed_hosts: &[String]) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(30)))?;

    // Requests are small; one read of the head is enough to route them.
    let mut reader = BufReader::new(stream.try_clone()?);
    let request = match read_request(&mut reader)? {
        Some(request) => request,
        None => return Ok(()), // client hung up
    };

    // Cross-site guard: an `Origin` that does not match the `Host` is not the interface.
    if let (Some(origin), Some(host)) = (request.header("Origin"), request.header("Host")) {
        if !origin_matches_host(origin, host) {
            let response = Response::error(
                403,
                "Forbidden",
                "cross-site requests are not allowed: this interface can commit and push, so it \
                 only accepts requests from its own page",
            );
            return write_response(&mut stream, response);
        }
    }

    // The `Host` header must be plausible, which closes the DNS-rebinding hole.
    if let Some(host) = request.header("Host") {
        let host_name = host_name(host);
        if !allowed_hosts.iter().any(|allowed| allowed == &host_name) {
            let response = Response::error(
                421,
                "Misdirected Request",
                "this interface only answers requests addressed to the host it was started with",
            );
            return write_response(&mut stream, response);
        }
    }

    match (request.method.as_str(), request.path()) {
        ("GET", "/") | ("GET", "/index.html") => {
            write_response(&mut stream, Response::html(asset::INDEX_HTML))
        }
        ("GET", "/app.css") => write_response(&mut stream, Response::css(asset::APP_CSS)),
        ("GET", "/app.js") => write_response(&mut stream, Response::js(asset::APP_JS)),
        ("GET", "/favicon.ico") => write_response(&mut stream, Response::empty(204, "No Content")),
        ("GET", "/api/health") => write_response(&mut stream, Response::json(health_json())),
        ("GET", "/api/model") => write_response(&mut stream, Response::json(gui.model())),
        ("POST", "/api/refresh") => write_response(&mut stream, Response::json(gui.model())),
        ("GET", "/api/setup/status") => write_response(&mut stream, setup_status_response(gui)),
        ("POST", "/api/setup/inspect") => {
            write_response(&mut stream, setup_inspect_response(gui, &request))
        }
        ("POST", "/api/setup/plan") => {
            write_response(&mut stream, setup_plan_response(gui, &request))
        }
        ("POST", "/api/setup/apply") => {
            write_response(&mut stream, setup_apply_response(gui, &request))
        }
        ("GET", "/api/repositories") => write_response(&mut stream, repositories_response(gui)),
        ("POST", "/api/repository/inspect") => {
            write_response(&mut stream, repository_inspect_response(gui, &request))
        }
        ("POST", "/api/repository/plan") => {
            write_response(&mut stream, repository_plan_response(gui, &request))
        }
        ("POST", "/api/repository/apply") => {
            write_response(&mut stream, repository_apply_response(gui, &request))
        }
        ("POST", "/api/open") => write_response(&mut stream, open_response(gui, &request)),
        ("POST", "/api/dry-run") => write_response(&mut stream, dry_run_response(gui, &request)),
        ("POST", "/api/commit") => write_response(&mut stream, commit_response(gui, &request)),
        ("POST", "/api/branch") => write_response(&mut stream, branch_response(gui, &request)),
        ("POST", "/api/sync") => write_response(&mut stream, sync_response(gui, &request)),
        ("POST", "/api/push") => write_response(&mut stream, push_response(gui)),
        (method, path) if method == "GET" && path.starts_with("/api/events/") => {
            match path.trim_start_matches("/api/events/").parse::<u64>() {
                Ok(id) => stream_events(&mut stream, gui, id),
                Err(_) => write_response(
                    &mut stream,
                    Response::error(400, "Bad Request", "the event stream id must be a number"),
                ),
            }
        }
        (method, path) if method == "GET" && path.starts_with("/api/report/") => {
            match path.trim_start_matches("/api/report/").parse::<u64>() {
                Ok(id) => match gui.stored_report(id) {
                    Some(report) => write_response(&mut stream, Response::json(report)),
                    None => write_response(
                        &mut stream,
                        Response::error(404, "Not Found", "no such operation result"),
                    ),
                },
                Err(_) => write_response(
                    &mut stream,
                    Response::error(400, "Bad Request", "the operation id must be a number"),
                ),
            }
        }
        (method, _) if method != "GET" && method != "POST" => write_response(
            &mut stream,
            Response::error(405, "Method Not Allowed", "only GET and POST are supported"),
        ),
        _ => write_response(&mut stream, not_found()),
    }
}

// ------------------------------------------------------------------- endpoints --

fn not_found() -> Response {
    Response::error(404, "Not Found", "no such endpoint")
}

fn health_json() -> String {
    crate::json::Json::object([
        ("ok", crate::json::Json::from(true)),
        ("name", crate::json::Json::from("gitmesh")),
        ("version", crate::json::Json::from(crate::VERSION)),
    ])
    .compact()
}

fn open_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let path = form_value(&request.body, "path").unwrap_or_default();
    if path.trim().is_empty() {
        return Response::error(400, "Bad Request", "the 'path' field is required");
    }
    // On failure the interface reloads the model itself, so the error response only
    // has to carry the reason: the server never has to merge two JSON documents.
    match gui.open(std::path::Path::new(path.trim())) {
        Ok(()) => Response::json(gui.model()),
        Err(message) => Response::json(
            crate::json::Json::object([("error", crate::json::Json::from(message))])
                .to_pretty_string(),
        )
        .with_status(422, "Unprocessable Entity"),
    }
}

fn dry_run_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let value = form_value(&request.body, "value")
        .map(|value| value == "true" || value == "1" || value == "on")
        .unwrap_or(false);
    gui.set_dry_run(value);
    Response::json(gui.model())
}

/// Inspect the directory the interface runs in, without changing anything.
fn setup_status_response(gui: &Arc<Gui>) -> Response {
    let start = gui.start_directory();
    inspect_response(gui, &start, None)
}

/// Inspect the directory given in the body, or the current one when the body is empty.
fn setup_inspect_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let path = match form_value(&request.body, "path") {
        Some(path) if !path.trim().is_empty() => std::path::PathBuf::from(path.trim()),
        _ => gui.start_directory(),
    };
    let name = optional_form(&request.body, "name");
    inspect_response(gui, &path, name.as_deref())
}

fn inspect_response(gui: &Arc<Gui>, path: &std::path::Path, name_hint: Option<&str>) -> Response {
    match gui.inspect_directory(path) {
        Ok(inspection) => Response::json(
            crate::json::Json::object([(
                "inspection",
                crate::service::inspection_view_json(&inspection, name_hint),
            )])
            .to_pretty_string(),
        ),
        Err(message) => Response::error(422, "Unprocessable Entity", &message),
    }
}

/// Turn the wizard's answers into a plan. Read-only: a plan creates nothing.
fn setup_plan_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let setup = match setup_request_from_form(&request.body) {
        Ok(setup) => setup,
        Err(message) => return Response::error(400, "Bad Request", &message),
    };
    match gui.plan_setup(&setup) {
        Ok(plan) => Response::json(
            crate::json::Json::object([("plan", crate::service::setup_plan_view_json(&plan))])
                .to_pretty_string(),
        ),
        Err(message) => Response::error(422, "Unprocessable Entity", &message),
    }
}

/// Apply a reviewed plan, in the background, and stream its progress.
///
/// The plan id the interface reviewed is required: without it the request would be a
/// setup nobody approved, and the server refuses rather than assuming consent.
fn setup_apply_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let reviewed = form_value(&request.body, "planId").unwrap_or_default();
    let reviewed = reviewed.trim().to_string();
    if reviewed.is_empty() {
        return Response::error(
            400,
            "Bad Request",
            "the 'planId' field is required: the plan has to be reviewed and confirmed first",
        );
    }
    let setup = match setup_request_from_form(&request.body) {
        Ok(setup) => setup,
        Err(message) => return Response::error(400, "Bad Request", &message),
    };
    match gui.start_setup(setup, Some(&reviewed)) {
        Ok(id) => {
            let body = crate::json::Json::object([
                ("id", crate::json::Json::from(id as i64)),
                (
                    "events",
                    crate::json::Json::from(format!("/api/events/{id}")),
                ),
            ])
            .compact();
            Response::json(body).with_status(202, "Accepted")
        }
        Err(SetupRefusal::Busy(message)) => Response::error(409, "Conflict", &message),
        Err(SetupRefusal::Refused(message)) => {
            Response::error(422, "Unprocessable Entity", &message)
        }
        Err(SetupRefusal::PlanChanged(plan)) => plan_refusal_response(
            409,
            "Conflict",
            "the directory changed since it was reviewed; check the plan again",
            crate::service::setup_plan_view_json(&plan),
        ),
        Err(SetupRefusal::Blocked(plan)) => plan_refusal_response(
            422,
            "Unprocessable Entity",
            "this plan cannot be applied as it is",
            crate::service::setup_plan_view_json(&plan),
        ),
    }
}

fn plan_refusal_response(
    status: u16,
    reason: &'static str,
    message: &str,
    plan: crate::json::Json,
) -> Response {
    Response::json(
        crate::json::Json::object([("error", crate::json::Json::from(message)), ("plan", plan)])
            .to_pretty_string(),
    )
    .with_status(status, reason)
}

// ------------------------------------------------------- repository endpoints --

/// The repositories of the open project: configuration, state on disk, and what needs
/// attention. Read-only, and the same view the CLI and the terminal interface read.
fn repositories_response(gui: &Arc<Gui>) -> Response {
    match gui.inspect_repositories() {
        Ok(inspection) => Response::json(
            crate::json::Json::object([(
                "inspection",
                crate::service::management_inspection_view_json(&inspection),
            )])
            .to_pretty_string(),
        ),
        Err(message) => Response::error(409, "Conflict", &message),
    }
}

/// What would happen to one directory that could become a repository. Read-only.
fn repository_inspect_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let path = form_value(&request.body, "path").unwrap_or_default();
    let path = path.trim().to_string();
    if path.is_empty() {
        return Response::error(400, "Bad Request", "the 'path' field is required");
    }
    match gui.inspect_repository_candidate(&path) {
        Ok(candidate) => Response::json(
            crate::json::Json::object([(
                "candidate",
                crate::service::management_candidate_view_json(&candidate),
            )])
            .to_pretty_string(),
        ),
        Err(message) => Response::error(422, "Unprocessable Entity", &message),
    }
}

/// Turn one repository action into a plan. Read-only: a plan configures nothing.
fn repository_plan_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let management = match repository_request_from_form(&request.body) {
        Ok(management) => management,
        Err(message) => return Response::error(400, "Bad Request", &message),
    };
    match gui.plan_management(&management) {
        Ok(plan) => Response::json(
            crate::json::Json::object([("plan", crate::service::management_plan_view_json(&plan))])
                .to_pretty_string(),
        ),
        Err(message) => Response::error(422, "Unprocessable Entity", &message),
    }
}

/// Apply a reviewed plan, in the background, and stream its progress.
///
/// The plan id is required for the same reason as in the setup: without it the request
/// would be a configuration change nobody approved.
fn repository_apply_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let reviewed = form_value(&request.body, "planId").unwrap_or_default();
    let reviewed = reviewed.trim().to_string();
    if reviewed.is_empty() {
        return Response::error(
            400,
            "Bad Request",
            "the 'planId' field is required: the plan has to be reviewed and confirmed first",
        );
    }
    let management = match repository_request_from_form(&request.body) {
        Ok(management) => management,
        Err(message) => return Response::error(400, "Bad Request", &message),
    };
    match gui.start_management(management, Some(&reviewed)) {
        Ok(id) => {
            let body = crate::json::Json::object([
                ("id", crate::json::Json::from(id as i64)),
                (
                    "events",
                    crate::json::Json::from(format!("/api/events/{id}")),
                ),
            ])
            .compact();
            Response::json(body).with_status(202, "Accepted")
        }
        Err(ManagementRefusal::Busy(message)) => Response::error(409, "Conflict", &message),
        Err(ManagementRefusal::Refused(message)) => {
            Response::error(422, "Unprocessable Entity", &message)
        }
        Err(ManagementRefusal::PlanChanged(plan)) => plan_refusal_response(
            409,
            "Conflict",
            "the configuration changed since it was reviewed; check the plan again",
            crate::service::management_plan_view_json(&plan),
        ),
        Err(ManagementRefusal::Blocked(plan)) => plan_refusal_response(
            422,
            "Unprocessable Entity",
            "this plan cannot be applied as it is",
            crate::service::management_plan_view_json(&plan),
        ),
    }
}

/// Read one repository action out of the panel's form body.
///
/// One request is one action, because that is what the interface offers: the review panel
/// shows exactly one change, and the same request planned elsewhere (the command line, a
/// script) goes through the same planning and execution code.
fn repository_request_from_form(
    body: &str,
) -> std::result::Result<RepositoryManagementRequest, String> {
    let intent = form_value(body, "intent").unwrap_or_default();
    let intent = intent.trim().to_string();
    let required = |key: &str| -> std::result::Result<String, String> {
        let value = form_value(body, key).unwrap_or_default().trim().to_string();
        if value.is_empty() {
            Err(format!("the '{key}' field is required"))
        } else {
            Ok(value)
        }
    };
    let one = match intent.as_str() {
        "add" => RepositoryIntent::Add {
            path: required("path")?,
            id: optional_form(body, "id").unwrap_or_default(),
            remote: optional_form(body, "remote"),
            branch: optional_form(body, "branch"),
            initialize: form_flag(body, "initialize"),
            configure_remote: form_flag(body, "configureRemote"),
            untrack_from_root: form_flag(body, "untrack"),
        },
        "remove" => RepositoryIntent::Remove {
            id: required("id")?,
            confirm_takeover: form_flag(body, "confirmTakeover"),
        },
        "rename" => RepositoryIntent::Rename {
            id: required("id")?,
            new_id: required("newId")?,
        },
        "set-remote" => RepositoryIntent::SetRemote {
            id: required("id")?,
            remote: optional_form(body, "remote"),
            configure: form_flag(body, "configure"),
        },
        other => return Err(format!("unknown repository action '{other}'")),
    };
    Ok(RepositoryManagementRequest::one(one))
}

fn operation_response(gui: &Arc<Gui>, operation: GuiOperation) -> Response {
    match gui.start_operation(operation) {
        Ok(id) => Response::json(
            crate::json::Json::object([
                ("id", crate::json::Json::from(id as i64)),
                (
                    "events",
                    crate::json::Json::from(format!("/api/events/{id}")),
                ),
            ])
            .compact(),
        )
        .with_status(202, "Accepted"),
        Err(message) => Response::error(409, "Conflict", &message),
    }
}

fn commit_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let message = form_value(&request.body, "message").unwrap_or_default();
    let message = message.trim().to_string();
    if message.is_empty() {
        return Response::error(
            400,
            "Bad Request",
            "a commit message is required; GitMesh uses it for every repository it commits",
        );
    }
    operation_response(gui, GuiOperation::Commit { message })
}

fn branch_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let action = form_value(&request.body, "action").unwrap_or_default();
    let name = form_value(&request.body, "name").unwrap_or_default();
    let name = name.trim().to_string();
    let force = form_value(&request.body, "force").as_deref() == Some("true");
    if name.is_empty() {
        return Response::error(400, "Bad Request", "a branch name is required");
    }
    let operation = match action.as_str() {
        "create" => GuiOperation::BranchCreate { name },
        "checkout" => GuiOperation::BranchCheckout { name },
        "start" => GuiOperation::BranchStart { name },
        "merge" => GuiOperation::BranchMerge { name },
        "delete" => GuiOperation::BranchDelete { name, force },
        other => {
            return Response::error(
                400,
                "Bad Request",
                &format!("unknown branch action '{other}'"),
            )
        }
    };
    operation_response(gui, operation)
}

fn sync_response(gui: &Arc<Gui>, request: &Request) -> Response {
    let action = form_value(&request.body, "action").unwrap_or_else(|| "pull".to_string());
    match action.as_str() {
        "fetch" => operation_response(gui, GuiOperation::Fetch),
        "pull" => {
            let strategy = form_value(&request.body, "strategy").unwrap_or_default();
            match crate::gui::parse_strategy(&strategy) {
                Ok(strategy) => operation_response(gui, GuiOperation::Pull { strategy }),
                Err(message) => Response::error(400, "Bad Request", &message),
            }
        }
        other => Response::error(
            400,
            "Bad Request",
            &format!("unknown sync action '{other}'"),
        ),
    }
}

fn push_response(gui: &Arc<Gui>) -> Response {
    operation_response(gui, GuiOperation::Push)
}

/// Thread one operation's progress to the browser as server-sent events.
fn stream_events(stream: &mut TcpStream, gui: &Arc<Gui>, id: u64) -> std::io::Result<()> {
    let Some(log) = gui.events_for(id) else {
        // The operation is gone (restarted server, or an unknown id): answer with the
        // stored result if there is one, so a reconnecting interface still recovers.
        return match gui.stored_report(id) {
            Some(report) => {
                let event = format!("event: result\ndata: {}\n\n", report.replace('\n', ""));
                write_sse_head(stream)?;
                stream.write_all(event.as_bytes())?;
                stream.flush()
            }
            None => write_response(
                stream,
                Response::error(404, "Not Found", "no such operation"),
            ),
        };
    };

    write_sse_head(stream)?;
    let started = Instant::now();
    loop {
        for event in log.drain() {
            if stream
                .write_all(format!("data: {event}\n\n").as_bytes())
                .is_err()
            {
                return Ok(()); // the browser went away
            }
        }
        if log.is_finished() {
            // One last drain, then close the stream: the interface reconnects for the
            // next operation.
            for event in log.drain() {
                let _ = stream.write_all(format!("data: {event}\n\n").as_bytes());
            }
            let _ = stream.write_all(b"event: closed\ndata: {}\n\n");
            let _ = stream.flush();
            return Ok(());
        }
        if stream.flush().is_err() {
            return Ok(());
        }
        if started.elapsed() > STREAM_TIMEOUT {
            let _ = stream.write_all(b"event: closed\ndata: {\"reason\":\"timeout\"}\n\n");
            let _ = stream.flush();
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

fn write_sse_head(stream: &mut TcpStream) -> std::io::Result<()> {
    let head = "HTTP/1.1 200 OK\r\n\
                Content-Type: text/event-stream; charset=utf-8\r\n\
                Cache-Control: no-store\r\n\
                Connection: close\r\n\
                X-Content-Type-Options: nosniff\r\n\r\n";
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

// ----------------------------------------------------------------- HTTP plumbing --

/// Read one request. Returns `None` when the client closed the connection first.
fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        head.push_str(&line);
        if head.len() > MAX_HEAD {
            return Ok(Some(Request {
                method: "GET".into(),
                target: "/too-large".into(),
                headers: Vec::new(),
                body: String::new(),
            }));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect();

    let length: usize = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let body = if length == 0 {
        String::new()
    } else {
        let length = length.min(MAX_BODY);
        let mut buffer = vec![0u8; length];
        reader.read_exact(&mut buffer)?;
        String::from_utf8_lossy(&buffer).to_string()
    };

    Ok(Some(Request {
        method,
        target,
        headers,
        body,
    }))
}

fn write_response(stream: &mut TcpStream, response: Response) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n",
        response.status,
        response.reason,
        response.content_type,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

// ------------------------------------------------------------ setup request --

/// Read a setup request out of the wizard's form body.
/// Read a setup request out of the wizard's form body.
///
/// The body is `application/x-www-form-urlencoded`. Field values are percent-encoded by
/// the caller, and a repository record keeps its `;` and `=` separators literal while each
/// *component* is escaped, so the server can split first and decode after. That is what
/// makes an awkward path — one containing `&`, `;` or `=` — survive the trip. A record
/// that cannot be read is refused instead of guessed.
fn setup_request_from_form(body: &str) -> std::result::Result<SetupRequest, String> {
    let path = form_value(body, "path").unwrap_or_default();
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err("the project root path is required".to_string());
    }
    let mut request = SetupRequest {
        root: std::path::PathBuf::from(path),
        name: form_value(body, "name")
            .unwrap_or_default()
            .trim()
            .to_string(),
        root_branch: optional_form(body, "rootBranch"),
        create_root_repository: form_flag(body, "root"),
        set_git_remote: form_flag(body, "configureRemotes"),
        overwrite_manifest: form_flag(body, "overwriteManifest"),
        overwrite_remotes: form_flag(body, "confirmRemotes"),
        untrack_from_root: form_flag(body, "untrack"),
        ..SetupRequest::default()
    };
    request.root_remote = root_remote_from_form(body)?;
    if form_flag(body, "publish") {
        request.publish_first_commit = Some(form_value(body, "firstCommit").unwrap_or_default());
    }
    for record in form_raw_values(body, "repositories") {
        request
            .repositories
            .push(repository_request_from_record(&record)?);
    }
    Ok(request)
}

/// The root repository's remote, built by the provider layer when the wizard picked a
/// provider instead of typing a URL.
fn root_remote_from_form(body: &str) -> std::result::Result<Option<String>, String> {
    if form_value(body, "rootProvider").unwrap_or_default().trim() == "github" {
        let plan = hosted_plan(
            &form_value(body, "rootOwner").unwrap_or_default(),
            &form_value(body, "rootName").unwrap_or_default(),
            &form_value(body, "rootScheme").unwrap_or_default(),
            &form_value(body, "rootVisibility").unwrap_or_default(),
        )?;
        return Ok(Some(plan.url));
    }
    Ok(optional_form(body, "rootRemote"))
}

/// One `path=engine;id=engine;...` record from the repository list.
fn repository_request_from_record(record: &str) -> std::result::Result<RepositoryRequest, String> {
    let mut fields: Vec<(String, String)> = Vec::new();
    for part in record.split(';') {
        if part.trim().is_empty() {
            continue;
        }
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| format!("'{part}' is not a 'key=value' pair"))?;
        fields.push((
            percent_decode(key).trim().to_string(),
            percent_decode(value),
        ));
    }
    if fields.is_empty() {
        return Err("a selected directory record is empty".to_string());
    }
    let get = |key: &str| -> String {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.trim().to_string())
            .unwrap_or_default()
    };

    let path = get("path");
    if path.is_empty() {
        return Err("a selected directory has no path".to_string());
    }
    let provider = get("provider");
    let (remote, visibility) = if provider == "github" {
        let plan = hosted_plan(
            &get("owner"),
            &get("name"),
            &get("scheme"),
            &get("visibility"),
        )?;
        (Some(plan.url), Some(plan.visibility.label().to_string()))
    } else {
        (non_empty(get("remote")), non_empty(get("visibility")))
    };

    Ok(RepositoryRequest {
        path,
        id: get("id"),
        remote,
        branch: non_empty(get("branch")),
        create: flag_value(&get("create")),
        untrack_from_root: flag_value(&get("untrack")),
        visibility,
    })
}

/// Ask the provider layer for the remote of a hosted repository.
///
/// GitMesh never creates a GitHub repository and never stores a credential: this only
/// turns `owner/name/visibility` into the URL that will be configured, exactly as the
/// review step will show it.
fn hosted_plan(
    owner: &str,
    name: &str,
    scheme: &str,
    visibility: &str,
) -> std::result::Result<crate::providers::github::GitHubRemotePlan, String> {
    let scheme = crate::providers::github::RemoteScheme::parse(scheme)
        .unwrap_or(crate::providers::github::RemoteScheme::Ssh);
    let visibility = crate::providers::github::Visibility::parse(visibility);
    crate::providers::github::plan_remote(owner, name, scheme, visibility)
        .map_err(|err| err.to_string())
}

/// A form value that must be present and non-empty.
fn optional_form(body: &str, key: &str) -> Option<String> {
    form_value(body, key).and_then(non_empty)
}

/// A trimmed string, or nothing when it is empty.
fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// True for the values a browser sends for a checked box (`yes` is the wizard's own).
fn form_flag(body: &str, key: &str) -> bool {
    flag_value(&form_value(body, key).unwrap_or_default())
}

fn flag_value(value: &str) -> bool {
    matches!(value.trim(), "yes" | "true" | "1" | "on")
}

/// Every `repositories` value, still percent-encoded, in the order the browser sent them.
fn form_raw_values(body: &str, key: &str) -> Vec<String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter(|(name, _)| *name == key)
        .map(|(_, value)| value.to_string())
        .collect()
}

/// `application/x-www-form-urlencoded` field, percent-decoded.
fn form_value(body: &str, key: &str) -> Option<String> {
    body.split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(name, _)| *name == key)
        .map(|(_, value)| percent_decode(value))
}

/// Decode `%XX` and `+` from a form value. Invalid escapes are kept verbatim, because a
/// half-decoded path is worse than a visible one.
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
                match hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    None => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            other => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Host part of a `Host` header (`example.com:8080` -> `example.com`).
fn host_name(host: &str) -> String {
    let host = host.trim();
    if let Some(rest) = host.strip_prefix('[') {
        // IPv6 literal
        return match rest.split_once(']') {
            Some((address, _)) => format!("[{address}]"),
            None => host.to_string(),
        };
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name.to_string(),
        _ => host.to_string(),
    }
}

/// True when an `Origin` header belongs to the same origin as the `Host` header.
fn origin_matches_host(origin: &str, host: &str) -> bool {
    let origin = origin.trim();
    if origin == "null" {
        return false;
    }
    let without_scheme = match origin.split_once("://") {
        Some((_, rest)) => rest,
        None => origin,
    };
    host_name(without_scheme) == host_name(host) && origin_port(without_scheme) == host_port(host)
}

fn origin_port(origin: &str) -> Option<String> {
    match origin.rsplit_once(':') {
        Some((_, port)) if port.chars().all(|c| c.is_ascii_digit()) => Some(port.to_string()),
        _ => None,
    }
}

fn host_port(host: &str) -> Option<String> {
    match host.rsplit_once(':') {
        Some((_, port)) if port.chars().all(|c| c.is_ascii_digit()) => Some(port.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;
    use std::net::TcpStream;

    fn get(port: u16, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .write_all(
                format!(
                    "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .unwrap();
        read_all(&mut stream)
    }

    fn post(port: u16, path: &str, body: &str, origin: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let origin = origin
            .map(|origin| format!("Origin: {origin}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}Content-Type: \
             application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: \
             close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).unwrap();
        read_all(&mut stream)
    }

    fn read_all(stream: &mut TcpStream) -> (u16, String) {
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).unwrap();
        let status = raw
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0);
        let body = raw.split_once("\r\n\r\n").map(|(_, body)| body.to_string());
        (status, body.unwrap_or_default())
    }

    /// Start a server on a free port and return (port, join handle).
    fn start_server(gui: Arc<Gui>) -> u16 {
        let listener = bind("127.0.0.1", 0).expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let _ = serve(listener, gui, "127.0.0.1", &[]);
        });
        port
    }

    #[test]
    fn serves_the_interface_and_the_model() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);

        let (status, html) = get(port, "/");
        assert_eq!(status, 200);
        assert!(html.contains("GitMesh"));
        assert!(html.contains("<!doctype html>"));

        let (status, css) = get(port, "/app.css");
        assert_eq!(status, 200);
        assert!(css.contains("--"));

        let (status, js) = get(port, "/app.js");
        assert_eq!(status, 200);
        assert!(js.contains("clientLogic"));

        let (status, health) = get(port, "/api/health");
        assert_eq!(status, 200);
        assert!(health.contains("\"ok\":true"));

        let (status, model) = get(port, "/api/model");
        assert_eq!(status, 200);
        assert!(model.contains("\"name\": \"demo\""));
        assert!(model.contains("\"tree\""));

        let (status, _) = get(port, "/nope");
        assert_eq!(status, 404);
    }

    #[test]
    fn model_reports_a_missing_project_instead_of_failing() {
        let fixture = RepoFixture::named("demo");
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);
        let (status, model) = get(port, "/api/model");
        assert_eq!(status, 200);
        assert!(model.contains("\"kind\": \"no-project\""));
    }

    #[test]
    fn opening_a_directory_through_the_api_works_and_reports_errors() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let empty = fixture.outside_path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        let gui = Arc::new(Gui::new(empty.clone(), false));
        let port = start_server(gui);

        let (status, body) = post(
            port,
            "/api/open",
            &format!("path={}", url_encode(fixture.path().to_str().unwrap())),
            Some(&format!("http://127.0.0.1:{port}")),
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"name\": \"demo\""));

        // Opening a directory that is not a project is a client error with a message.
        let (status, body) = post(
            port,
            "/api/open",
            &format!("path={}", url_encode(empty.to_str().unwrap())),
            None,
        );
        assert_eq!(status, 422, "{body}");
        assert!(body.contains("no GitMesh project found"), "{body}");
    }

    #[test]
    fn commit_endpoint_streams_progress_events() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);

        let (status, body) = post(
            port,
            "/api/commit",
            "message=from%20the%20interface",
            Some(&format!("http://127.0.0.1:{port}")),
        );
        assert_eq!(status, 202, "{body}");
        let id = body
            .split("\"id\":")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap()
            .to_string();

        let (status, events) = get(port, &format!("/api/events/{id}"));
        assert_eq!(status, 200);
        assert!(events.contains("\"type\":\"started\""), "{events}");
        assert!(events.contains("\"type\":\"finished\""), "{events}");
        assert!(events.contains("\"sentence\":\"Committing the project\""));

        // The stored report is available for a late page load.
        let (status, report) = get(port, &format!("/api/report/{id}"));
        assert_eq!(status, 200);
        assert!(report.contains("\"operation\": \"commit\""));

        assert!(fixture
            .git_ok("engine", &["log", "-1", "--pretty=%s"])
            .contains("from the interface"));
    }

    #[test]
    fn commit_without_a_message_is_rejected() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);
        let (status, body) = post(port, "/api/commit", "message=%20%20", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("commit message is required"), "{body}");
    }

    #[test]
    fn branch_and_sync_endpoints_validate_their_input() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);

        let (status, body) = post(port, "/api/branch", "action=nonsense&name=x", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("unknown branch action"));

        let (status, body) = post(port, "/api/branch", "action=create&name=", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("branch name is required"));

        let (status, body) = post(port, "/api/sync", "action=pull&strategy=sideways", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("unknown pull strategy"));

        let (status, body) = post(port, "/api/sync", "action=teleport", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("unknown sync action"));

        let (status, body) = post(port, "/api/branch", "action=start&name=feature%2Fapi", None);
        assert_eq!(status, 202, "{body}");
        let id = body
            .split("\"id\":")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap()
            .to_string();
        let (_, events) = get(port, &format!("/api/events/{id}"));
        assert!(events.contains("feature/api"), "{events}");
        assert_eq!(
            fixture
                .git_ok("engine", &["rev-parse", "--abbrev-ref", "HEAD"])
                .trim(),
            "feature/api"
        );
    }

    /// A plain directory outside the project: what the wizard is pointed at first.
    fn plain_project(fixture: &RepoFixture, label: &str) -> std::path::PathBuf {
        let path = fixture.outside_path().join(label);
        std::fs::create_dir_all(path.join("src")).unwrap();
        std::fs::create_dir_all(path.join("engine")).unwrap();
        std::fs::write(path.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(path.join("engine/lib.rs"), "pub fn go() {}\n").unwrap();
        path
    }

    /// One repository record, the way the browser builds it: escaped components, literal
    /// `;` and `=` separators.
    fn record(fields: &[(&str, &str)]) -> String {
        fields
            .iter()
            .map(|(key, value)| format!("{key}={}", url_encode(value)))
            .collect::<Vec<_>>()
            .join(";")
    }

    fn setup_body(path: &std::path::Path) -> String {
        format!(
            "path={}&name=MyProject&root=yes&configureRemotes=yes&untrack=yes&repositories={}",
            url_encode(&path.to_string_lossy()),
            record(&[("path", "engine"), ("id", "engine"), ("create", "yes")])
        )
    }

    #[test]
    fn setup_endpoints_inspect_plan_apply_and_open_the_project_without_a_restart() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_project(&fixture, "MyProject");
        let gui = Arc::new(Gui::new(plain.clone(), false));
        let port = start_server(Arc::clone(&gui));

        // Status is read-only and works before any project exists.
        let (status, body) = get(port, "/api/setup/status");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"isGitMeshProject\": false"), "{body}");
        assert!(!plain.join(".gitmesh").exists(), "nothing was created");

        // A plan is generated from the answers, and it is still read-only.
        let (status, body) = post(port, "/api/setup/plan", &setup_body(&plain), None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"kind\": \"plan\""), "{body}");
        assert!(body.contains("\"ready\": true"), "{body}");
        assert!(body.contains("[[repositories]]"), "{body}");
        assert!(body.contains("\"safety\""), "{body}");
        let plan_id = plan_id_from(&body);
        assert!(!plain.join(".gitmesh").exists(), "planning created nothing");

        // Applying without the reviewed plan id, or with a stale one, is refused.
        let (status, body) = post(port, "/api/setup/apply", &setup_body(&plain), None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("reviewed"), "{body}");
        let stale = format!("{}&planId=0000", setup_body(&plain));
        let (status, body) = post(port, "/api/setup/apply", &stale, None);
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("changed since it was reviewed"), "{body}");
        assert!(
            !plain.join(".gitmesh").exists(),
            "a refused apply created nothing"
        );

        // The confirmed plan runs in the background and streams its steps.
        let confirmed = format!("{}&planId={}", setup_body(&plain), url_encode(&plan_id));
        let (status, body) = post(port, "/api/setup/apply", &confirmed, None);
        assert_eq!(status, 202, "{body}");
        let id = id_from(&body);
        let (status, events) = get(port, &format!("/api/events/{id}"));
        assert_eq!(status, 200);
        assert!(events.contains("\"type\":\"started\""), "{events}");
        assert!(
            events.contains("\"operation\":\"Project setup\""),
            "{events}"
        );
        assert!(events.contains("\"type\":\"outcome\""), "{events}");
        assert!(events.contains("\"kind\":\"complete\""), "{events}");
        assert!(events.contains("\"type\":\"finished\""), "{events}");
        assert!(events.contains("\"opened\":true"), "{events}");

        // The interface shows the project immediately: no restart, no reopen.
        let (status, model) = get(port, "/api/model");
        assert_eq!(status, 200);
        assert!(model.contains("\"kind\": \"project\""), "{model}");
        assert!(model.contains("\"name\": \"MyProject\""), "{model}");
        assert!(model.contains("\"id\": \"engine\""), "{model}");

        // And the project on disk is a real GitMesh project.
        assert!(plain.join(".gitmesh/project.toml").is_file());
        assert!(plain.join(".git/HEAD").is_file());
        assert!(plain.join("engine/.git/HEAD").is_file());
        let (status, report) = get(port, &format!("/api/report/{id}"));
        assert_eq!(status, 200);
        assert!(report.contains("\"status\": \"finished\""), "{report}");
    }

    #[test]
    fn setup_plan_view_shows_hosted_remotes_without_asking_for_credentials() {
        let fixture = RepoFixture::named("demo");
        let plain = plain_project(&fixture, "Hosted");
        let gui = Arc::new(Gui::new(plain.clone(), false));
        let port = start_server(gui);

        let repository = record(&[
            ("path", "engine"),
            ("id", "engine"),
            ("provider", "github"),
            ("owner", "acme"),
            ("name", "myproject-engine"),
            ("scheme", "ssh"),
            ("visibility", "private"),
            ("create", "yes"),
        ]);
        let body = format!(
            "path={}&name=MyProject&root=yes&repositories={repository}",
            url_encode(&plain.to_string_lossy())
        );
        let (status, body) = post(port, "/api/setup/plan", &body, None);
        assert_eq!(status, 200, "{body}");
        assert!(
            body.contains("git@github.com:acme/myproject-engine.git"),
            "{body}"
        );
        assert!(
            body.contains("\"fullName\": \"acme/myproject-engine\""),
            "{body}"
        );
        assert!(body.contains("\"visibility\": \"private\""), "{body}");
        assert!(
            body.contains("gh repo create acme/myproject-engine --private"),
            "{body}"
        );
        assert!(body.contains("\"createsRepository\": false"), "{body}");

        // A hosted repository that GitHub would reject is refused, with the reason.
        let bad = body.replace("myproject-engine", "..");
        let (status, body) = post(port, "/api/setup/plan", &bad, None);
        assert_eq!(status, 400, "{body}");
    }

    #[test]
    fn setup_requests_survive_awkward_paths_and_refuse_malformed_records() {
        let root = "/tmp/odd;dir=1/&other".to_string();
        let body = format!(
            "path={}&name=Odd&repositories={}",
            url_encode(&root),
            record(&[("path", "engine"), ("id", "engine")])
        );
        let request = setup_request_from_form(&body).expect("request");
        assert_eq!(request.root, std::path::PathBuf::from(&root));
        assert_eq!(request.repositories.len(), 1);
        assert_eq!(request.repositories[0].path, "engine");

        // An awkward directory name inside a record survives, because only the component
        // is escaped while the separators stay literal.
        let body = format!(
            "path=/tmp/x&repositories={}",
            record(&[("path", "odd;dir=1"), ("id", "odd")])
        );
        assert_eq!(
            setup_request_from_form(&body).unwrap().repositories[0].path,
            "odd;dir=1"
        );

        // Records are read in order, and two directories stay two directories.
        let body = format!(
            "path=/tmp/x&repositories={}&repositories={}",
            record(&[("path", "engine"), ("id", "engine"), ("create", "yes")]),
            record(&[
                ("path", "renderer"),
                ("id", "renderer"),
                ("remote", "/tmp/remote.git")
            ])
        );
        let request = setup_request_from_form(&body).expect("request");
        assert_eq!(request.repositories.len(), 2);
        assert!(request.repositories[0].create);
        assert_eq!(
            request.repositories[1].remote.as_deref(),
            Some("/tmp/remote.git")
        );

        // A record that cannot be read is refused instead of guessed.
        // A record without a value is refused, and so is a request without a root.
        let body = "path=/tmp/x&repositories=path=engine;id".to_string();
        assert!(setup_request_from_form(&body).is_err());
        assert!(setup_request_from_form("path=&repositories=path=engine").is_err());
        assert!(setup_request_from_form("name=x").is_err());
    }

    // --------------------------------------------------- repository endpoints --

    /// A project with one repository configured and one directory that is not configured
    /// yet, so the panel has something to list and something to add.
    fn project_with_a_candidate() -> RepoFixture {
        let fixture = RepoFixture::named("panel");
        fixture.project_with(&[("root", ".")]);
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        // The root repository tracks the file, which is the ownership decision the panel
        // has to show before the directory becomes a repository of its own.
        fixture.add_all(".");
        fixture.commit(".", "root tracks the module");
        fixture
    }

    fn open_gui(fixture: &RepoFixture) -> (Arc<Gui>, u16) {
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(Arc::clone(&gui));
        (gui, port)
    }

    fn add_body(fields: &[(&str, &str)]) -> String {
        let mut body = String::from("intent=add");
        for (key, value) in fields {
            body.push_str(&format!("&{key}={}", url_encode(value)));
        }
        body
    }

    #[test]
    fn repository_endpoints_inspect_plan_and_apply_without_a_restart() {
        let fixture = project_with_a_candidate();
        let (_gui, port) = open_gui(&fixture);

        // The panel reads the configuration it is going to change.
        let (status, body) = get(port, "/api/repositories");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"kind\": \"repositories\""), "{body}");
        assert!(body.contains("\"id\": \"root\""), "{body}");
        assert!(body.contains("\"needingAttention\": 0"), "{body}");
        assert!(body.contains("\"path\": \"engine\""), "{body}");

        // Checking a directory answers the questions before the button appears.
        let (status, body) = post(port, "/api/repository/inspect", "path=engine", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"canAdd\": true"), "{body}");
        assert!(body.contains("\"isRepository\": false"), "{body}");
        assert!(body.contains("\"suggestedId\": \"engine\""), "{body}");
        assert!(
            body.contains("\"trackedByRoot\": 1"),
            "the panel is told the root repository owns the file: {body}"
        );
        assert!(
            !fixture.path().join("engine/.git").exists(),
            "checking created nothing"
        );

        // A missing path and an empty one are refused, never guessed.
        let (status, body) = post(port, "/api/repository/inspect", "path=", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("path"), "{body}");
        let (status, body) = post(port, "/api/repository/inspect", "path=nope", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"exists\": false"), "{body}");
        assert!(body.contains("\"canAdd\": false"), "{body}");

        // A plan is built from the request, and it is still read-only.
        let plan_body = add_body(&[
            ("path", "engine"),
            ("id", "engine"),
            ("initialize", "true"),
            ("untrack", "true"),
        ]);
        let (status, body) = post(port, "/api/repository/plan", &plan_body, None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"kind\": \"plan\""), "{body}");
        assert!(body.contains("\"flow\": \"repository\""), "{body}");
        assert!(body.contains("\"ready\": true"), "{body}");
        assert!(
            body.contains("\"kind\": \"initialize-repository\""),
            "{body}"
        );
        assert!(body.contains("\"kind\": \"add-repository\""), "{body}");
        assert!(body.contains("[[repositories]]"), "{body}");
        assert!(body.contains("\"safety\""), "{body}");
        let plan_id = plan_id_from(&body);
        assert!(
            !fixture.path().join("engine/.git").exists(),
            "planning created nothing"
        );
        let manifest_before =
            std::fs::read_to_string(fixture.path().join(".gitmesh/project.toml")).unwrap();

        // Without a reviewed plan id, and with a stale one, nothing runs.
        let (status, body) = post(port, "/api/repository/apply", &plan_body, None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("reviewed"), "{body}");
        let stale = format!("{plan_body}&planId=0000000000000000");
        let (status, body) = post(port, "/api/repository/apply", &stale, None);
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("changed since it was reviewed"), "{body}");
        assert!(
            body.contains("\"kind\": \"plan\""),
            "the fresh plan is sent back: {body}"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.path().join(".gitmesh/project.toml")).unwrap(),
            manifest_before,
            "a refused apply changed nothing"
        );

        // The confirmed plan runs in the background and streams its steps.
        let confirmed = format!("{plan_body}&planId={}", url_encode(&plan_id));
        let (status, body) = post(port, "/api/repository/apply", &confirmed, None);
        assert_eq!(status, 202, "{body}");
        let id = id_from(&body);
        let (status, events) = get(port, &format!("/api/events/{id}"));
        assert_eq!(status, 200);
        assert!(events.contains("\"type\":\"started\""), "{events}");
        assert!(
            events.contains("\"operation\":\"Repositories\""),
            "{events}"
        );
        assert!(events.contains("\"type\":\"outcome\""), "{events}");
        assert!(events.contains("\"kind\":\"complete\""), "{events}");
        assert!(events.contains("\"opened\":true"), "{events}");

        // The project on disk really changed, and the interface shows it: no restart.
        assert!(fixture.path().join("engine/.git/HEAD").is_file());
        let manifest =
            std::fs::read_to_string(fixture.path().join(".gitmesh/project.toml")).unwrap();
        assert!(manifest.contains("id = \"engine\""), "{manifest}");
        let (status, model) = get(port, "/api/model");
        assert_eq!(status, 200);
        assert!(model.contains("\"id\": \"engine\""), "{model}");
        let (status, report) = get(port, &format!("/api/report/{id}"));
        assert_eq!(status, 200);
        assert!(report.contains("\"flow\": \"repository\""), "{report}");
        assert!(
            report.contains("the manifest now lists 'engine' at 'engine'"),
            "the result carries the evidence that the change happened: {report}"
        );
        assert!(
            report.contains("the directory 'engine' exists"),
            "and the proof for the directory itself: {report}"
        );

        // Reading the configuration again reports the repository that is now there.
        let (status, body) = get(port, "/api/repositories");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"id\": \"engine\""), "{body}");
        assert!(body.contains("\"repositories\": 2"), "{body}");
    }

    #[test]
    fn repository_endpoints_refuse_unknown_actions_and_unconfirmed_removals() {
        let fixture = project_with_a_candidate();
        let (_gui, port) = open_gui(&fixture);

        let (status, body) = post(port, "/api/repository/plan", "intent=explode", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("unknown repository action"), "{body}");
        let (status, body) = post(port, "/api/repository/plan", "intent=add", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("'path' field is required"), "{body}");
        let (status, body) = post(port, "/api/repository/plan", "intent=rename&id=root", None);
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("'newId' field is required"), "{body}");

        // Removing the repository that owns the project itself is refused by the service.
        let (status, body) = post(port, "/api/repository/plan", "intent=remove&id=root", None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ready\": false"), "{body}");
        assert!(body.contains("\"kind\": \"plan\""), "{body}");

        // Adopting the directory first, without untracking, leaves the root repository
        // owning its files: removing it then needs the ownership confirmed.
        let plan_body = add_body(&[
            ("path", "engine"),
            ("id", "engine"),
            ("initialize", "true"),
            ("configureRemote", "false"),
            ("untrack", "false"),
        ]);
        let (status, body) = post(port, "/api/repository/plan", &plan_body, None);
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ready\": true"), "{body}");
        assert!(
            body.contains("belong to two repositories"),
            "the ownership warning is in the plan: {body}"
        );
        let plan_id = plan_id_from(&body);
        let confirmed = format!("{plan_body}&planId={}", url_encode(&plan_id));
        let (status, body) = post(port, "/api/repository/apply", &confirmed, None);
        assert_eq!(status, 202, "{body}");
        let id = id_from(&body);
        let (status, _events) = get(port, &format!("/api/events/{id}"));
        assert_eq!(status, 200);

        let (status, body) = post(
            port,
            "/api/repository/plan",
            "intent=remove&id=engine",
            None,
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ready\": false"), "{body}");
        assert!(
            body.contains("hands those files back to the root repository"),
            "the consequence is stated before anything is written: {body}"
        );
        assert!(
            body.contains("has to be confirmed"),
            "and the interface is told which confirmation is missing: {body}"
        );
        assert!(
            body.contains("\"state\": \"blocked\""),
            "the blocked change is a row of the review: {body}"
        );
        let (status, body) = post(
            port,
            "/api/repository/plan",
            "intent=remove&id=engine&confirmTakeover=true",
            None,
        );
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ready\": true"), "{body}");
        assert!(body.contains("\"kind\": \"remove-repository\""), "{body}");
        assert!(
            body.contains("its .git, its history and its remote are kept"),
            "{body}"
        );
        assert!(body.contains("\"removals\""), "{body}");
    }

    #[test]
    fn a_reviewed_plan_is_refused_when_the_configuration_moved_on() {
        let fixture = project_with_a_candidate();
        let (_gui, port) = open_gui(&fixture);

        let plan_body = add_body(&[
            ("path", "engine"),
            ("id", "engine"),
            ("initialize", "true"),
            ("untrack", "true"),
        ]);
        let (status, body) = post(port, "/api/repository/plan", &plan_body, None);
        assert_eq!(status, 200, "{body}");
        let plan_id = plan_id_from(&body);

        // Something else changes the project — another tool, a checkout, a hand edit.
        fixture.write("other.txt", "x\n");
        let manifest_path = fixture.path().join(".gitmesh/project.toml");
        let edited = std::fs::read_to_string(&manifest_path)
            .unwrap()
            .replace("version = 1", "version = 1\n# edited by hand");
        std::fs::write(&manifest_path, edited).unwrap();

        let confirmed = format!("{plan_body}&planId={}", url_encode(&plan_id));
        let (status, body) = post(port, "/api/repository/apply", &confirmed, None);
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("changed since it was reviewed"), "{body}");
        assert!(
            !fixture.path().join("engine/.git").exists(),
            "nothing was created behind the refusal"
        );
    }

    #[test]
    fn repository_endpoints_are_not_available_without_an_open_project() {
        let fixture = RepoFixture::named("empty");
        let (_gui, port) = open_gui(&fixture);
        let (status, body) = get(port, "/api/repositories");
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("no GitMesh project is open"), "{body}");
        let (status, body) = post(port, "/api/repository/plan", "intent=add&path=engine", None);
        assert_eq!(status, 422, "{body}");
    }

    #[test]
    fn dry_run_endpoint_toggles_the_mode() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);
        let (status, model) = post(port, "/api/dry-run", "value=true", None);
        assert_eq!(status, 200);
        assert!(model.contains("\"dryRun\": true"));
        let (_, model) = post(port, "/api/dry-run", "value=false", None);
        assert!(model.contains("\"dryRun\": false"));
    }

    #[test]
    fn cross_site_and_misdirected_requests_are_refused() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        let gui = Arc::new(Gui::new(fixture.path().to_path_buf(), false));
        let port = start_server(gui);

        // A page on another site cannot drive the interface.
        let (status, body) = post(
            port,
            "/api/commit",
            "message=evil",
            Some("http://evil.example.com"),
        );
        assert_eq!(status, 403, "{body}");
        assert!(body.contains("cross-site"), "{body}");
        assert!(!fixture.git_ok(".", &["log", "--oneline"]).contains("evil"));

        // A different port is a different origin too.
        let (status, _) = post(
            port,
            "/api/commit",
            "message=evil",
            Some(&format!("http://127.0.0.1:{}", port + 1)),
        );
        assert_eq!(status, 403);

        // A misdirected Host header (DNS rebinding) is refused.
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(
                b"GET /api/model HTTP/1.1\r\nHost: attacker.example\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let (status, _) = read_all(&mut stream);
        assert_eq!(status, 421);
    }

    #[test]
    fn extra_hosts_are_only_accepted_when_requested() {
        assert_eq!(
            allowed_hosts("127.0.0.1", &[]),
            vec!["127.0.0.1", "localhost", "[::1]"]
        );
        // The bind address is allowed, a wildcard bind is not a usable name.
        assert!(allowed_hosts("10.0.0.5", &[]).contains(&"10.0.0.5".to_string()));
        assert!(!allowed_hosts("0.0.0.0", &[]).contains(&"0.0.0.0".to_string()));
        let extra = allowed_hosts("0.0.0.0", &["preview.example.com".to_string()]);
        assert!(extra.contains(&"preview.example.com".to_string()));
        assert!(!allowed_hosts("0.0.0.0", &[]).contains(&"preview.example.com".to_string()));
        // Blank and duplicate entries do not pile up.
        let messy = allowed_hosts(
            "127.0.0.1",
            &[" ".to_string(), "localhost".to_string(), "".to_string()],
        );
        assert_eq!(messy, vec!["127.0.0.1", "localhost", "[::1]"]);
    }

    #[test]
    fn form_values_and_host_parsing_are_sound() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%2Ftmp%2Fproject"), "/tmp/project");
        assert_eq!(percent_decode("%C3%A9t%C3%A9"), "été");
        assert_eq!(
            form_value("path=%2Ftmp%2Fx&other=1", "path").as_deref(),
            Some("/tmp/x")
        );
        assert_eq!(
            form_value("path=/tmp/x&other=1", "other").as_deref(),
            Some("1")
        );
        assert_eq!(form_value("nothing=1", "path"), None);

        assert_eq!(host_name("example.com:8080"), "example.com");
        assert_eq!(host_name("example.com"), "example.com");
        assert_eq!(host_name("[::1]:80"), "[::1]");
        assert!(origin_matches_host(
            "http://127.0.0.1:7345",
            "127.0.0.1:7345"
        ));
        assert!(!origin_matches_host(
            "http://127.0.0.1:7346",
            "127.0.0.1:7345"
        ));
        assert!(!origin_matches_host(
            "http://evil.test:7345",
            "127.0.0.1:7345"
        ));
        assert!(!origin_matches_host("null", "127.0.0.1:7345"));
    }

    /// The plan id out of a `{"plan": {"id": "..."}}` body.
    fn plan_id_from(body: &str) -> String {
        let after = body.split("\"id\": \"").nth(1).expect("plan id");
        after.split('"').next().unwrap_or_default().to_string()
    }

    /// The operation id out of a `{"id": 3, ...}` body.
    fn id_from(body: &str) -> i64 {
        let compact = body.replace(' ', "");
        let after = compact.split("\"id\":").nth(1).expect("operation id");
        after
            .trim_start_matches('"')
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap_or_default()
            .parse()
            .expect("numeric operation id")
    }

    fn url_encode(value: &str) -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                b' ' => out.push('+'),
                other => out.push_str(&format!("%{other:02X}")),
            }
        }
        out
    }
}
