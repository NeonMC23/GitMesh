//! GitMesh command line entry point.
//!
//! The CLI resolves the project, calls the orchestration layer and renders the result.
//! It never constructs a Git command itself.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use gitmesh::analyzer::Analyzer;
use gitmesh::cli::{Cli, Command, ConfigureCommand, GlobalOptions};
use gitmesh::discovery::{self, ScanOptions};
use gitmesh::git::GitRunner;
use gitmesh::json::Json;
use gitmesh::manifest;
use gitmesh::model::{GitMeshProject, RepositoryState};
use gitmesh::ops::{
    self, BranchAction, BranchOptions, CommitOptions, OperationReport, OutcomeKind, PushOptions,
    RepositorySelection,
};
use gitmesh::paths::to_slash;
use gitmesh::{providers, Error, Result};

/// Usage/configuration errors exit with 2, operational problems with 1.
const EXIT_OK: u8 = 0;
const EXIT_OPERATION_FAILED: u8 = 1;
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("gitmesh: {err}");
            let code =
                if err.is_configuration_error() || matches!(err, Error::ProjectNotFound { .. }) {
                    EXIT_USAGE
                } else {
                    EXIT_OPERATION_FAILED
                };
            ExitCode::from(code)
        }
    }
}

/// Run the CLI and return the process exit code.
fn run(cli: Cli) -> Result<u8> {
    let global = cli.global();
    let command = cli
        .command
        .unwrap_or(Command::Status(gitmesh::cli::StatusArgs {
            short: false,
            changes: false,
        }));
    let runner = GitRunner::detect()?;

    match command {
        Command::Init(args) => cmd_init(&global, &runner, &args),
        Command::Discover(args) => cmd_discover(&global, &runner, &args),
        Command::Configure(command) => cmd_configure(&global, &runner, command),
        Command::Status(args) => cmd_status(&global, &runner, &args),
        Command::Commit(args) => {
            let project = load_project(&global, &runner)?;
            let options = CommitOptions {
                message: args.message,
                selection: selection(&args.repositories, args.path),
                dry_run: args.dry_run,
                include_untracked: true,
                quiet_clean: false,
            };
            let report = ops::commit_project(&project, &runner, &options)?;
            render_report(&global, &report)
        }
        Command::Branch(args) => {
            let project = load_project(&global, &runner)?;
            if global.json {
                // Branch reports are operations; reuse the standard report rendering.
            }
            let options = BranchOptions {
                selection: RepositorySelection::All,
                force: args.force(),
                dry_run: args.dry_run,
                excluded: Vec::new(),
            };
            let report = ops::branch_operation(&project, &runner, &args.action(), &options)?;
            render_report(&global, &report)
        }
        Command::Checkout(args) => {
            let project = load_project(&global, &runner)?;
            let options = BranchOptions {
                selection: RepositorySelection::All,
                force: false,
                dry_run: args.dry_run,
                excluded: Vec::new(),
            };
            let action = BranchAction::Checkout {
                name: args.name,
                create: args.create,
            };
            let report = ops::branch_operation(&project, &runner, &action, &options)?;
            render_report(&global, &report)
        }
        Command::Merge(args) => {
            let project = load_project(&global, &runner)?;
            let options = BranchOptions {
                selection: RepositorySelection::All,
                force: false,
                dry_run: args.dry_run,
                excluded: Vec::new(),
            };
            let report = ops::branch_operation(
                &project,
                &runner,
                &BranchAction::Merge { name: args.name },
                &options,
            )?;
            render_report(&global, &report)
        }
        Command::Fetch(args) => {
            let project = load_project(&global, &runner)?;
            let report = ops::fetch_project(&project, &runner, &args.sync_options())?;
            render_report(&global, &report)
        }
        Command::Pull(args) => {
            let project = load_project(&global, &runner)?;
            let report = ops::pull_project(&project, &runner, &args.sync_options())?;
            render_report(&global, &report)
        }
        Command::Push(args) => {
            let project = load_project(&global, &runner)?;
            let options = PushOptions {
                selection: RepositorySelection::from_ids(args.repositories.clone()),
                dry_run: args.dry_run,
                set_upstream: !args.no_set_upstream,
                default_remote: "origin".to_string(),
            };
            let report = ops::push_project(&project, &runner, &options)?;
            render_report(&global, &report)
        }
        Command::Remotes => cmd_remotes(&global, &runner),
        Command::Gui(args) => cmd_gui(&global, &args),
        Command::Ui(args) => {
            let path = resolve_start_path(&global, &args.path);
            match gitmesh::ui::run(&path, args.dry_run) {
                Ok(()) => Ok(EXIT_OK),
                Err(Error::Unsupported(message)) => {
                    println!("{message}");
                    Ok(EXIT_OK)
                }
                Err(err) => Err(err),
            }
        }
    }
}

// --------------------------------------------------------------------- gui --

fn cmd_gui(global: &GlobalOptions, args: &gitmesh::cli::GuiArgs) -> Result<u8> {
    // `gui` takes the project directory the same way every other command does: the
    // path argument, or the global `-C` when it is set.
    let start = match &global.project {
        Some(path) => path.clone(),
        None => args.path.clone(),
    };
    let options = gitmesh::gui::GuiOptions {
        start: Some(start),
        host: args.host.clone(),
        allow_hosts: args.allow_hosts.clone(),
        port: args.port,
        open: args.open,
        dry_run: args.dry_run,
    };
    gitmesh::gui::run(options)?;
    Ok(EXIT_OK)
}

// ----------------------------------------------------------------- utilities --

/// Resolve the project for commands that require an existing GitMesh project.
fn load_project(global: &GlobalOptions, runner: &GitRunner) -> Result<GitMeshProject> {
    let start = global.project.clone().unwrap_or_else(|| PathBuf::from("."));
    let start = absolute(&start)?;
    let root = manifest::find_project_root(&start).ok_or_else(|| Error::ProjectNotFound {
        root: start.clone(),
    })?;
    let project = manifest::load_from_root(&root)?;
    // A missing `git` must be reported before anything else.
    runner.version()?;
    Ok(project)
}

/// Resolve a path argument to an absolute path (without requiring existence).
fn absolute(path: &Path) -> Result<PathBuf> {
    gitmesh::paths::absolute(path)
}

/// Where a command that works on a *directory* (rather than a project) starts.
fn resolve_start_path(global: &GlobalOptions, fallback: &Path) -> PathBuf {
    match &global.project {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.clone()),
        None => {
            if fallback.is_absolute() {
                fallback.to_path_buf()
            } else {
                std::env::current_dir()
                    .map(|cwd| cwd.join(fallback))
                    .unwrap_or_else(|_| fallback.to_path_buf())
            }
        }
    }
}

fn selection(repositories: &[String], path: Option<PathBuf>) -> RepositorySelection {
    if !repositories.is_empty() {
        RepositorySelection::from_ids(repositories.to_vec())
    } else if let Some(path) = path {
        RepositorySelection::Subtree(path)
    } else {
        RepositorySelection::All
    }
}

// -------------------------------------------------------------------- init --

fn cmd_init(
    global: &GlobalOptions,
    runner: &GitRunner,
    args: &gitmesh::cli::InitArgs,
) -> Result<u8> {
    let root = absolute(&resolve_start_path(global, &args.path))?;
    if !root.is_dir() {
        return Err(Error::Other(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let manifest_file = manifest::manifest_path(&root);
    if manifest_file.exists() && !args.force {
        return Err(Error::Manifest(format!(
            "{} already contains a GitMesh project (use --force to replace the manifest)",
            root.display()
        )));
    }

    let project = discovery::initial_project(
        &root,
        args.name.clone(),
        args.remote.clone(),
        args.branch.clone(),
    )?;

    let is_repository = runner.repo(&root).is_repository();
    if args.add_git_remote {
        if let Some(url) = args.remote.as_deref() {
            if !is_repository {
                return Err(Error::NotARepository { path: root.clone() });
            }
            let repo = runner.repo(&root);
            let has_origin = repo.remotes()?.iter().any(|r| r.name == "origin");
            if has_origin {
                repo.run_checked(&["remote", "set-url", "origin", url])?;
            } else {
                repo.run_checked(&["remote", "add", "origin", url])?;
            }
        }
    }

    let path = manifest::save_project(&project)?;

    if global.json {
        println!(
            "{}",
            Json::object([
                ("project", Json::from(project.name.clone())),
                ("root", Json::from(to_slash(&project.root))),
                ("manifest", Json::from(to_slash(&path))),
                ("root_is_repository", Json::from(is_repository)),
            ])
            .to_pretty_string()
        );
        return Ok(EXIT_OK);
    }

    println!("Created GitMesh project '{}'", project.name);
    println!("  root:     {}", project.root.display());
    println!("  manifest: {}", path.display());
    if !is_repository {
        println!();
        println!("Note: the project root is not a Git repository yet.");
        println!("      Run `git init` there to give the root repository a history.");
    }
    println!();
    println!("Next steps:");
    println!("  gitmesh discover                 # inspect the tree and the repositories in it");
    println!("  gitmesh configure add <dir>      # mark a directory as an external repository");
    println!("  gitmesh status                   # unified project status");
    println!("  gitmesh ui                       # interactive terminal interface");
    Ok(EXIT_OK)
}

// ---------------------------------------------------------------- discover --

fn cmd_discover(
    global: &GlobalOptions,
    runner: &GitRunner,
    args: &gitmesh::cli::DiscoverArgs,
) -> Result<u8> {
    let root = absolute(&resolve_start_path(global, &args.path))?;
    let scan_root = manifest::find_project_root(&root).unwrap_or(root);
    let options = ScanOptions {
        max_depth: args.depth,
        include_hidden: args.hidden,
        stop_at_repository_roots: !args.deep,
    };
    let scan = discovery::scan_project(&scan_root, &options, runner)?;

    let configured = manifest::load_from_root(&scan_root).ok();

    if global.json {
        let mut nodes = Vec::new();
        scan.tree.walk(&mut |node, depth| {
            nodes.push(Json::object([
                ("path", Json::from(to_slash(&node.relative_path))),
                ("depth", Json::from(depth)),
                ("is_repository", Json::from(node.is_repository_root)),
                ("files", Json::from(node.file_count)),
                ("truncated", Json::from(node.truncated)),
            ]));
        });
        let repositories: Vec<Json> = scan
            .repositories
            .iter()
            .map(|repo| {
                let configured_id = configured
                    .as_ref()
                    .and_then(|project| project.repository_for_relative(&repo.relative_path))
                    .filter(|owner| !owner.is_root() || repo.is_project_root)
                    .map(|owner| owner.id.clone());
                Json::object([
                    ("path", Json::from(to_slash(&repo.relative_path))),
                    ("is_project_root", Json::from(repo.is_project_root)),
                    ("branch", Json::opt(repo.branch.clone().map(Json::from))),
                    ("has_commits", Json::from(repo.has_commits)),
                    ("tracked_files", Json::from(repo.tracked_files)),
                    ("remote", Json::opt(repo.remote_url.clone().map(Json::from))),
                    ("configured_id", Json::opt(configured_id.map(Json::from))),
                    (
                        "nested_inside",
                        Json::opt(
                            repo.nested_inside
                                .as_ref()
                                .and_then(|p| gitmesh::paths::project_relative(&scan.root, p))
                                .map(|p| Json::from(to_slash(&p))),
                        ),
                    ),
                ])
            })
            .collect();
        println!(
            "{}",
            Json::object([
                ("root", Json::from(to_slash(&scan.root))),
                ("git_available", Json::from(scan.git_available)),
                ("root_is_repository", Json::from(scan.root_is_repository)),
                ("tree", Json::from(nodes)),
                ("repositories", Json::from(repositories)),
                (
                    "notices",
                    Json::array(scan.notices.iter().map(|n| Json::from(n.as_str())))
                ),
            ])
            .to_pretty_string()
        );
        return Ok(EXIT_OK);
    }

    println!("Project tree of {}", scan.root.display());
    println!();
    scan.tree.walk(&mut |node, depth| {
        let indent = "  ".repeat(depth);
        let marker = if node.is_repository_root {
            match configured
                .as_ref()
                .and_then(|project| project.repository_for_relative(&node.relative_path))
                .filter(|owner| !owner.is_root())
            {
                Some(owner) => format!(" [repository: {}]", owner.id),
                None => " [git repository - unassigned]".to_string(),
            }
        } else {
            String::new()
        };
        let files = if node.file_count > 0 {
            format!(" ({} file(s))", node.file_count)
        } else {
            String::new()
        };
        println!("{indent}{}{marker}{files}", node.name);
    });

    println!();
    if scan.repositories.is_empty() {
        println!("No Git repositories found.");
    } else {
        println!("Repositories found:");
        for repo in &scan.repositories {
            let role = if repo.is_project_root {
                "root"
            } else {
                "nested"
            };
            let branch = repo.branch.clone().unwrap_or_else(|| "(no branch)".into());
            let commits = if repo.has_commits {
                ""
            } else {
                ", no commits yet"
            };
            println!(
                "  {:<32} {:<7} {:<20} {} tracked file(s){commits}",
                to_slash(&repo.relative_path),
                role,
                branch,
                repo.tracked_files
            );
        }
    }
    if !scan.notices.is_empty() {
        println!();
        println!("Notes:");
        for notice in &scan.notices {
            println!("  - {notice}");
        }
    }
    println!();
    println!("To make a directory its own physical repository:");
    println!("  gitmesh configure add <directory> [--remote <url>]");
    Ok(EXIT_OK)
}

// --------------------------------------------------------------- configure --

fn cmd_configure(
    global: &GlobalOptions,
    runner: &GitRunner,
    command: ConfigureCommand,
) -> Result<u8> {
    match command {
        ConfigureCommand::List => {
            let project = load_project(global, runner)?;
            if global.json {
                println!("{}", project_json(&project).to_pretty_string());
                return Ok(EXIT_OK);
            }
            println!("Project '{}' at {}", project.name, project.root.display());
            println!();
            println!(
                "{:<16} {:<10} {:<24} {:<16} REMOTE",
                "ID", "ROLE", "PATH", "BRANCH"
            );
            for repo in project.sorted_repositories() {
                println!(
                    "{:<16} {:<10} {:<24} {:<16} {}",
                    repo.id,
                    repo.role.label(),
                    repo.relative_slash(),
                    repo.branch.clone().unwrap_or_else(|| "-".into()),
                    repo.remote_url.clone().unwrap_or_else(|| "-".into())
                );
            }
            Ok(EXIT_OK)
        }
        ConfigureCommand::Add(args) => {
            let project = load_project(global, runner)?;
            let check = discovery::check_assignment(&project, &args.path, runner)?;
            for warning in &check.warnings {
                eprintln!("gitmesh: note: {warning}");
            }
            let options = discovery::AssignOptions {
                id: args.id.clone(),
                remote_url: args.remote.clone(),
                init_git: args.git_init,
                branch: args.branch.clone(),
            };
            if args.dry_run {
                let id = args.id.clone().unwrap_or_else(|| "auto".to_string());
                println!(
                    "would add repository '{id}' at '{}'{}",
                    to_slash(&check.relative_path),
                    if check.is_repository {
                        ""
                    } else if args.git_init {
                        " (creating a Git repository there)"
                    } else {
                        " (not a Git repository yet)"
                    }
                );
                return Ok(EXIT_OK);
            }
            let updated = discovery::assign_repository(&project, &args.path, &options, runner)?;
            let path = manifest::save_project(&updated)?;
            let added = updated
                .repository_for_relative(&check.relative_path)
                .map(|r| r.id.clone())
                .unwrap_or_default();
            println!(
                "Added repository '{added}' at '{}' (saved to {})",
                to_slash(&check.relative_path),
                path.display()
            );
            if !check.is_repository {
                println!();
                println!("Note: '{added}' is not hosted anywhere yet. Add a remote with:");
                println!("  gitmesh configure remote {added} --url <url> --set-git-remote");
            }
            Ok(EXIT_OK)
        }
        ConfigureCommand::Remove(args) => {
            let project = load_project(global, runner)?;
            let repo = project
                .repository(&args.id)
                .ok_or_else(|| Error::UnknownRepository(args.id.clone()))?;
            if args.dry_run {
                println!(
                    "would remove repository '{}' from the configuration (its files and Git history are untouched)",
                    args.id
                );
                return Ok(EXIT_OK);
            }
            let updated = discovery::unassign_repository(&project, &args.id)?;
            manifest::save_project(&updated)?;
            println!(
                "Removed repository '{}' at '{}' from the configuration.",
                args.id,
                repo.relative_slash()
            );
            println!("Its directory and Git history were not touched.");
            Ok(EXIT_OK)
        }
        ConfigureCommand::Rename(args) => {
            let project = load_project(global, runner)?;
            if args.dry_run {
                println!("would rename repository '{}' to '{}'", args.id, args.new_id);
                return Ok(EXIT_OK);
            }
            let updated = discovery::rename_repository(&project, &args.id, &args.new_id)?;
            manifest::save_project(&updated)?;
            println!("Renamed repository '{}' to '{}'", args.id, args.new_id);
            Ok(EXIT_OK)
        }
        ConfigureCommand::Remote(args) => {
            let project = load_project(global, runner)?;
            let url = if args.clear {
                None
            } else if args.url.trim().is_empty() {
                return Err(Error::Other(
                    "provide --url <url> or --clear to remove the remote".into(),
                ));
            } else {
                Some(args.url.trim().to_string())
            };
            if args.dry_run {
                match &url {
                    Some(url) => println!("would set the remote of '{}' to {url}", args.id),
                    None => println!("would clear the remote of '{}'", args.id),
                }
                return Ok(EXIT_OK);
            }
            let updated = discovery::set_repository_remote(
                &project,
                &args.id,
                url.clone(),
                args.set_git_remote,
                runner,
            )?;
            manifest::save_project(&updated)?;
            match url {
                Some(url) => println!("Set the remote of '{}' to {url}", args.id),
                None => println!("Cleared the remote of '{}'", args.id),
            }
            Ok(EXIT_OK)
        }
    }
}

fn project_json(project: &GitMeshProject) -> Json {
    Json::object([
        ("project", Json::from(project.name.clone())),
        ("root", Json::from(to_slash(&project.root))),
        (
            "repositories",
            Json::array(
                project
                    .sorted_repositories()
                    .iter()
                    .map(|repo| {
                        Json::object([
                            ("id", Json::from(repo.id.clone())),
                            ("role", Json::from(repo.role.label())),
                            ("path", Json::from(repo.relative_slash())),
                            ("absolute_path", Json::from(to_slash(&repo.absolute_path))),
                            ("remote", Json::opt(repo.remote_url.clone().map(Json::from))),
                            ("branch", Json::opt(repo.branch.clone().map(Json::from))),
                            (
                                "provider",
                                Json::opt(
                                    repo.remote_url
                                        .as_deref()
                                        .and_then(providers::parse_remote)
                                        .map(|remote| Json::from(remote.provider)),
                                ),
                            ),
                        ])
                    })
                    .collect::<Vec<_>>(),
            ),
        ),
    ])
}

// ------------------------------------------------------------------ status --

fn cmd_status(
    global: &GlobalOptions,
    runner: &GitRunner,
    args: &gitmesh::cli::StatusArgs,
) -> Result<u8> {
    let project = load_project(global, runner)?;
    let analyzer = Analyzer::new(&project, runner);
    let status = analyzer.analyze();

    if global.json {
        println!("{}", analyzer.status_json(&status).to_pretty_string());
        return Ok(if status.unavailable().next().is_some() {
            EXIT_OPERATION_FAILED
        } else {
            EXIT_OK
        });
    }

    let inconsistent: Vec<String> = status
        .inconsistent_branches()
        .iter()
        .map(|r| r.id.clone())
        .collect();

    println!(
        "Project '{}' at {} ({} repositor{})",
        status.name,
        status.root.display(),
        status.repositories.len(),
        if status.repositories.len() == 1 {
            "y"
        } else {
            "ies"
        }
    );
    println!();

    if args.short {
        for state in &status.repositories {
            println!("{:<12} {}", state.id, state.summary());
        }
    } else {
        println!(
            "{:<14} {:<9} {:<20} {:<26} STATE",
            "REPOSITORY", "ROLE", "PATH", "BRANCH"
        );
        for state in &status.repositories {
            println!(
                "{:<14} {:<9} {:<20} {:<26} {}",
                state.id,
                state.role.label(),
                state.relative_path,
                branch_cell(state),
                state.summary()
            );
        }
    }

    let changes = analyzer.owned_changes(&status);
    if args.changes && !changes.is_empty() {
        println!();
        println!("Changes ({}):", changes.len());
        for change in &changes {
            // One classification, shared with every other front end (see
            // `service::status_column_label` for why this stays the legacy wording).
            let flag = gitmesh::service::status_column_label(&change.entry);
            let renamed = change
                .entry
                .original_path
                .as_ref()
                .map(|original| format!(" (was {original})"))
                .unwrap_or_default();
            println!(
                "  {:<12} {:<16} {}{}",
                change.repository_id, flag, change.logical_path, renamed
            );
        }
    } else if !changes.is_empty() && !args.short {
        let mut per_repo: Vec<(String, usize)> = Vec::new();
        for change in &changes {
            match per_repo
                .iter_mut()
                .find(|(id, _)| id == &change.repository_id)
            {
                Some((_, count)) => *count += 1,
                None => per_repo.push((change.repository_id.clone(), 1)),
            }
        }
        println!();
        println!(
            "Changes: {} in {} repository(ies). Use --changes to list them.",
            changes.len(),
            per_repo.len()
        );
    }

    if !inconsistent.is_empty() {
        println!();
        println!(
            "Warning: physical repositories are on different branches: {}",
            inconsistent.join(", ")
        );
        println!("         run `gitmesh checkout <branch>` to bring them onto one branch.");
    }
    if !status.notices.is_empty() {
        println!();
        println!("Notes:");
        for notice in &status.notices {
            println!("  - {notice}");
        }
    }

    let problems: Vec<&RepositoryState> = status.unavailable().collect();
    Ok(if problems.is_empty() {
        EXIT_OK
    } else {
        EXIT_OPERATION_FAILED
    })
}

/// Delegates to the shared presentation helper, so the CLI, the terminal interface and
/// the GUI always describe a repository the same way.
fn branch_cell(state: &RepositoryState) -> String {
    gitmesh::service::branch_cell(state)
}

// ------------------------------------------------------------------ remotes --

fn cmd_remotes(global: &GlobalOptions, runner: &GitRunner) -> Result<u8> {
    let project = load_project(global, runner)?;
    let analyzer = Analyzer::new(&project, runner);
    let status = analyzer.analyze();

    if global.json {
        println!(
            "{}",
            Json::object([
                ("project", Json::from(project.name.clone())),
                (
                    "repositories",
                    Json::array(
                        status
                            .repositories
                            .iter()
                            .map(|state| {
                                Json::object([
                                    ("id", Json::from(state.id.clone())),
                                    ("path", Json::from(state.relative_path.clone())),
                                    (
                                        "configured_remote",
                                        Json::opt(
                                            project
                                                .repository(&state.id)
                                                .and_then(|r| r.remote_url.clone())
                                                .map(Json::from),
                                        ),
                                    ),
                                    (
                                        "remotes",
                                        Json::array(
                                            state
                                                .remotes
                                                .iter()
                                                .map(|remote| {
                                                    let url = remote.fetch_url().unwrap_or("");
                                                    Json::object([
                                                        ("name", Json::from(remote.name.clone())),
                                                        ("url", Json::from(url)),
                                                        ("kind", Json::from(remote.kind().label())),
                                                        (
                                                            "provider",
                                                            Json::opt(
                                                                providers::parse_remote(url).map(
                                                                    |r| Json::from(r.provider),
                                                                ),
                                                            ),
                                                        ),
                                                    ])
                                                })
                                                .collect::<Vec<_>>(),
                                        ),
                                    ),
                                ])
                            })
                            .collect::<Vec<_>>(),
                    ),
                ),
            ])
            .to_pretty_string()
        );
        return Ok(EXIT_OK);
    }

    println!("Remotes for project '{}'", project.name);
    println!();
    for state in &status.repositories {
        let configured = project
            .repository(&state.id)
            .and_then(|r| r.remote_url.clone());
        println!("{} ({})", state.id, state.relative_path);
        if !state.is_usable() {
            println!(
                "  unavailable: {}",
                state.error.clone().unwrap_or_else(|| "unknown".into())
            );
            continue;
        }
        if state.remotes.is_empty() {
            println!("  no remote configured");
        }
        for remote in &state.remotes {
            let url = remote.fetch_url().unwrap_or("(no url)");
            match providers::parse_remote(url) {
                Some(coordinates) => println!(
                    "  {:<8} {}  [{} {} - {}]",
                    remote.name,
                    url,
                    coordinates.provider,
                    coordinates.full_name(),
                    coordinates.web_url()
                ),
                None => println!("  {:<8} {}  [{}]", remote.name, url, remote.kind().label()),
            }
        }
        if let Some(configured) = configured {
            let matches = state
                .remotes
                .iter()
                .any(|r| r.fetch_url() == Some(configured.as_str()));
            if !matches {
                println!("  manifest records: {configured} (not configured in the repository)");
            }
        }
    }
    println!();
    println!("GitHub is optional: local GitMesh operations work without any remote.");
    Ok(EXIT_OK)
}

// ------------------------------------------------------------------ reports --

fn render_report(global: &GlobalOptions, report: &OperationReport) -> Result<u8> {
    if global.json {
        println!("{}", report.to_json().to_pretty_string());
        return Ok(report.exit_code() as u8);
    }

    if report.dry_run {
        println!("[dry run] {} - nothing was changed", report.operation);
    } else {
        println!("gitmesh {}", report.operation);
    }
    println!();
    for line in report.summary_lines() {
        println!("{line}");
    }
    if global.verbose || !report.is_success() {
        let details = report.detail_lines();
        if !details.is_empty() {
            println!();
            println!("Details:");
            for line in details {
                println!("{line}");
            }
        }
    }
    if report
        .outcomes
        .iter()
        .any(|o| o.kind == OutcomeKind::Conflict)
    {
        println!();
        println!("Conflicts must be resolved inside the affected repositories; GitMesh never");
        println!("resolves or discards conflicting content automatically.");
    }
    Ok(report.exit_code() as u8)
}
