//! Command line interface tests.
//!
//! These run the real `gitmesh` binary as a child process, so they cover argument
//! parsing, exit codes and rendered output — not just the library API.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gitmesh::testkit::RepoFixture;

/// Absolute path of the compiled binary under test.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_gitmesh"))
}

struct Cli {
    dir: PathBuf,
}

impl Cli {
    fn new(dir: &Path) -> Cli {
        Cli {
            dir: dir.to_path_buf(),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(binary())
            .args(args)
            .current_dir(&self.dir)
            .env("GIT_AUTHOR_NAME", "GitMesh Test")
            .env("GIT_AUTHOR_EMAIL", "test@gitmesh.test")
            .env("GIT_COMMITTER_NAME", "GitMesh Test")
            .env("GIT_COMMITTER_EMAIL", "test@gitmesh.test")
            .output()
            .expect("run gitmesh")
    }

    fn out(&self, args: &[&str]) -> (String, String, i32) {
        let output = self.run(args);
        (
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.code().unwrap_or(-1),
        )
    }

    fn ok(&self, args: &[&str]) -> String {
        let (stdout, stderr, code) = self.out(args);
        assert_eq!(
            code, 0,
            "gitmesh {args:?} exited {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        stdout
    }
}

#[test]
fn version_and_help_are_available() {
    let fixture = RepoFixture::new();
    let cli = Cli::new(fixture.path());
    let (stdout, _, code) = cli.out(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("gitmesh"), "{stdout}");

    let (stdout, _, code) = cli.out(&["--help"]);
    assert_eq!(code, 0);
    for expected in [
        "init",
        "status",
        "commit",
        "pull",
        "push",
        "configure",
        "ui",
    ] {
        assert!(
            stdout.contains(expected),
            "help is missing '{expected}':\n{stdout}"
        );
    }
}

#[test]
fn init_discover_configure_status_commit_round_trip() {
    let fixture = RepoFixture::new();
    fixture.init_repo("engine");
    fixture.write("engine/README.md", "# engine\n");
    fixture.commit("engine", "engine init");

    let cli = Cli::new(fixture.path());
    let stdout = cli.ok(&["init", ".", "--name", "demo"]);
    assert!(
        stdout.contains("Created GitMesh project 'demo'"),
        "{stdout}"
    );

    let stdout = cli.ok(&["discover"]);
    assert!(stdout.contains("engine"), "{stdout}");
    assert!(stdout.contains("git repository - unassigned"), "{stdout}");

    let stdout = cli.ok(&[
        "configure",
        "add",
        "engine",
        "--remote",
        "git@github.com:acme/engine.git",
    ]);
    assert!(stdout.contains("Added repository 'engine'"), "{stdout}");

    let stdout = cli.ok(&["configure", "list"]);
    assert!(stdout.contains("external"), "{stdout}");
    assert!(
        stdout.contains("git@github.com:acme/engine.git"),
        "{stdout}"
    );

    // A change in each repository, then one logical commit.
    std::fs::write(fixture.path().join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(fixture.path().join("engine/lib.rs"), "pub fn go() {}").unwrap();
    let stdout = cli.ok(&["status", "--changes"]);
    assert!(stdout.contains("engine/lib.rs"), "{stdout}");
    assert!(stdout.contains("main.rs"), "{stdout}");

    let stdout = cli.ok(&["commit", "-m", "cli commit"]);
    assert!(stdout.contains("root"), "{stdout}");
    assert!(stdout.contains("2 succeeded"), "{stdout}");
    assert_eq!(
        fixture
            .git_ok("engine", &["log", "-1", "--pretty=%s"])
            .trim(),
        "cli commit"
    );

    // Status is clean afterwards.
    let stdout = cli.ok(&["status"]);
    assert!(stdout.contains("clean"), "{stdout}");
}

#[test]
fn status_json_is_machine_readable() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", "."), ("engine", "engine")]);
    std::fs::write(fixture.path().join("engine/new.txt"), "x").unwrap();

    let cli = Cli::new(fixture.path());
    let stdout = cli.ok(&["status", "--json"]);
    let parsed = stdout.trim();
    assert!(parsed.starts_with('{') && parsed.ends_with('}'), "{parsed}");
    assert!(parsed.contains("\"project\""));
    assert!(
        parsed.contains("\"logical_path\": \"engine/new.txt\""),
        "{parsed}"
    );
    assert!(parsed.contains("\"role\": \"external\""), "{parsed}");
}

#[test]
fn commit_reports_partial_failure_with_a_non_zero_exit_code() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", "."), ("engine", "engine")]);
    std::fs::write(fixture.path().join("a.rs"), "root").unwrap();
    std::fs::write(fixture.path().join("engine/b.rs"), "engine").unwrap();

    // A failing pre-commit hook in engine.
    let hooks = fixture.path().join("engine/.git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
    }

    let cli = Cli::new(fixture.path());
    let (stdout, stderr, code) = cli.out(&["commit", "-m", "partial"]);
    assert_eq!(code, 1, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("1 succeeded"), "{stdout}");
    assert!(stdout.contains("1 failed"), "{stdout}");
    assert!(stdout.contains("✗"), "{stdout}");
    // The root repository still committed.
    assert_eq!(
        fixture.git_ok(".", &["log", "-1", "--pretty=%s"]).trim(),
        "partial"
    );
}

#[test]
fn missing_project_exits_with_the_usage_code() {
    let fixture = RepoFixture::new();
    let cli = Cli::new(fixture.path());
    let (_, stderr, code) = cli.out(&["status"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("no GitMesh project found"), "{stderr}");
    assert!(stderr.contains("gitmesh init"), "{stderr}");
}

#[test]
fn branch_checkout_and_push_work_through_the_cli() {
    let fixture = RepoFixture::new();
    let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
    assert_eq!(project.len(), 2);
    fixture.publish(".", "remotes/root.git");
    fixture.publish("engine", "remotes/engine.git");

    let cli = Cli::new(fixture.path());
    let stdout = cli.ok(&["branch", "create", "feature/cli"]);
    assert!(stdout.contains("created branch 'feature/cli'"), "{stdout}");
    let stdout = cli.ok(&["checkout", "feature/cli"]);
    assert!(
        stdout.contains("switched to branch 'feature/cli'"),
        "{stdout}"
    );
    let stdout = cli.ok(&["branch"]);
    assert!(stdout.contains("feature/cli"), "{stdout}");

    std::fs::write(fixture.path().join("work.rs"), "x").unwrap();
    cli.ok(&["commit", "-m", "on the branch"]);
    let stdout = cli.ok(&["push"]);
    assert!(stdout.contains("2 succeeded"), "{stdout}");

    // Dry run does not push anything new.
    std::fs::write(fixture.path().join("more.rs"), "y").unwrap();
    cli.ok(&["commit", "-m", "more"]);
    let stdout = cli.ok(&["push", "--dry-run"]);
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(stdout.contains("would push"), "{stdout}");
}

#[test]
fn pull_reports_a_conflict_without_resolving_it() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let bare = fixture.publish(".", "remotes/root.git");

    let other = fixture.clone_outside(&bare, "dev/root");
    std::fs::write(other.join("README.md"), "their version\n").unwrap();
    let other_repo = fixture.runner().repo(&other);
    other_repo.run_checked(&["add", "-A"]).unwrap();
    other_repo
        .run_checked(&["commit", "-q", "-m", "theirs"])
        .unwrap();
    other_repo
        .run_checked(&["push", "-q", "origin", "main"])
        .unwrap();

    // Our own committed change to the same file.
    fixture.append("README.md", "our version\n");
    let cli = Cli::new(fixture.path());
    cli.ok(&["commit", "-m", "ours"]);

    let (stdout, _, code) = cli.out(&["pull", "--strategy", "merge"]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.contains("conflict"), "{stdout}");

    let content = std::fs::read_to_string(fixture.path().join("README.md")).unwrap();
    assert!(content.contains("<<<<<<<"), "{content}");
}

#[test]
fn remotes_command_reports_github_coordinates() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", "."), ("engine", "engine")]);
    fixture.git_ok(
        "engine",
        &["remote", "add", "origin", "git@github.com:acme/engine.git"],
    );

    let cli = Cli::new(fixture.path());
    let stdout = cli.ok(&["remotes"]);
    assert!(stdout.contains("github"), "{stdout}");
    assert!(stdout.contains("acme/engine"), "{stdout}");
    assert!(
        stdout.contains("https://github.com/acme/engine"),
        "{stdout}"
    );

    // The same information is available as JSON.
    let stdout = cli.ok(&["remotes", "--json"]);
    assert!(stdout.contains("\"provider\": \"github\""), "{stdout}");
}

#[test]
fn ui_without_a_terminal_explains_the_cli_alternative() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let cli = Cli::new(fixture.path());
    let (stdout, _, code) = cli.out(&["ui"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("no interactive terminal"), "{stdout}");
    assert!(stdout.contains("gitmesh status"), "{stdout}");
}

#[test]
fn no_unrelated_repository_is_touched() {
    // A GitMesh project must not reach outside its own root: a sibling repository is
    // left completely alone.
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let bare = fixture.create_bare("sibling.git");
    let sibling = fixture.clone_outside(&bare, "sibling");

    let cli = Cli::new(fixture.path());
    cli.ok(&["init", ".", "--name", "demo", "--force"]);
    std::fs::write(fixture.path().join("a.txt"), "x").unwrap();
    cli.ok(&["commit", "-m", "root only"]);

    let repo = fixture.runner().repo(&sibling);
    let status = repo.status().unwrap();
    assert!(
        status.is_fully_clean(),
        "sibling repository must be untouched"
    );
}

#[test]
fn gui_command_serves_the_interface_and_can_be_opened_from_the_cli() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", "."), ("engine", "engine")]);
    fixture.write("src/main.rs", "x");
    fixture.write("engine/lib.rs", "y");

    // Start the real binary and wait for it to announce the address it bound.
    let mut child = Command::new(binary())
        .args(["gui", "--port", "0"])
        .current_dir(fixture.path())
        .env("GIT_AUTHOR_NAME", "GitMesh Test")
        .env("GIT_AUTHOR_EMAIL", "test@gitmesh.test")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start gitmesh gui");

    let stdout = child.stdout.take().expect("stdout pipe");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if sender.send(line.clone()).is_err() {
                break;
            }
        }
    });

    let mut announced = None;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        match receiver.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(line) => {
                if let Some(rest) = line.split("http://").nth(1) {
                    let address = rest.trim().to_string();
                    if address.contains(':') {
                        announced = Some(address);
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    let address = announced.expect("the GUI announces its address");

    // The interface and its assets are served.
    let health = http_get(&address, "/api/health");
    assert!(health.contains("200 OK"), "{health}");
    let page = http_get(&address, "/");
    assert!(page.contains("GitMesh"), "{page}");
    assert!(page.contains("id=\"workspace\""), "{page}");
    let model = http_get(&address, "/api/model");
    assert!(model.contains("\"project\""), "{model}");
    assert!(model.contains("\"engine\""), "{model}");
    assert!(model.contains("engine/lib.rs"), "{model}");

    // A cross-site request is refused before it can reach Git.
    let refused = http_request(
        &address,
        "POST /api/commit HTTP/1.1",
        &[
            "Content-Type: application/x-www-form-urlencoded",
            "Origin: http://evil.example.com",
        ],
        "message=evil",
    );
    assert!(refused.contains("403"), "{refused}");

    // A real commit through the interface works (and leaves the CLI untouched).
    let started = http_request(
        &address,
        "POST /api/commit HTTP/1.1",
        &["Content-Type: application/x-www-form-urlencoded"],
        "message=commit%20from%20the%20interface",
    );
    assert!(started.contains("202"), "{started}");
    let id = started
        .split("\"id\":")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .map(str::to_string)
        .expect("operation id");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut finished = String::new();
    while std::time::Instant::now() < deadline {
        finished = http_get(&address, &format!("/api/report/{id}"));
        if finished.contains("\"commit\"") && finished.contains("\"model\"") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert!(finished.contains("commit"), "{finished}");
    assert!(finished.contains("\"success\""), "{finished}");

    child.kill().ok();
    child.wait().ok();

    // Both repositories received their own real commit with the same message.
    assert!(fixture
        .git_ok(".", &["log", "-1", "--pretty=%s"])
        .contains("commit from the interface"));
    assert!(fixture
        .git_ok("engine", &["log", "-1", "--pretty=%s"])
        .contains("commit from the interface"));
    // The CLI keeps working exactly as before.
    let cli = Cli::new(fixture.path());
    let stdout = cli.ok(&["status", "-s"]);
    assert!(stdout.contains("root"), "{stdout}");
    assert!(stdout.contains("clean"), "{stdout}");
}

/// Minimal HTTP GET, used to talk to the interface the way a browser would.
fn http_get(address: &str, path: &str) -> String {
    http_request(address, &format!("GET {path} HTTP/1.1"), &[], "")
}

fn http_request(address: &str, request_line: &str, headers: &[&str], body: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(address).expect("connect to the interface");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut request = format!(
        "{request_line}\r\nHost: {address}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for header in headers {
        request.push_str(&format!("{header}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}
