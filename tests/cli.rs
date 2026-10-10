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
#[test]
fn init_records_a_remote_and_only_configures_git_when_asked() {
    let fixture = RepoFixture::new();

    // `--remote` records the URL in the manifest: no Git configuration is touched, which
    // is what the flag has always promised.
    let cli = Cli::new(fixture.path());
    let stdout = cli.ok(&[
        "init",
        ".",
        "--name",
        "demo",
        "--remote",
        "git@github.com:acme/demo.git",
    ]);
    assert!(stdout.contains("demo"), "{stdout}");
    let manifest = std::fs::read_to_string(fixture.path().join(".gitmesh/project.toml")).unwrap();
    assert!(
        manifest.contains("git@github.com:acme/demo.git"),
        "{manifest}"
    );
    assert_eq!(
        fixture.git_ok(".", &["remote"]).trim(),
        "",
        "the repository itself has no remote yet"
    );

    // ...and the project still opens, because a recorded remote is a complete answer.
    let stdout = cli.ok(&["status"]);
    assert!(stdout.contains("demo"), "{stdout}");

    // `--add-git-remote` is the explicit request to configure `origin`.
    cli.ok(&[
        "init",
        ".",
        "--name",
        "demo",
        "--remote",
        "git@github.com:acme/demo.git",
        "--add-git-remote",
        "--force",
    ]);
    assert_eq!(
        fixture.git_ok(".", &["remote", "get-url", "origin"]).trim(),
        "git@github.com:acme/demo.git"
    );
}

#[test]
fn init_refuses_to_run_a_plan_it_cannot_satisfy() {
    let fixture = RepoFixture::new();
    fixture.init_repo("engine");

    // A repository inside a repository is refused, with the reason, and nothing is written.
    let cli = Cli::new(fixture.path());
    let (_, stderr, code) = cli.out(&["init", ".", "--name", "demo"]);
    assert_eq!(code, 0, "{stderr}");

    // A remote that disagrees with `origin` on disk, recorded without configuring Git, is
    // refused: the manifest would otherwise promise a remote that pushes do not use.
    fixture
        .runner()
        .repo(fixture.path())
        .run_checked(&["remote", "add", "origin", "git@github.com:acme/other.git"])
        .unwrap();
    let (_, stderr, code) = cli.out(&[
        "init",
        ".",
        "--name",
        "demo",
        "--remote",
        "git@github.com:acme/demo.git",
        "--force",
    ]);
    assert_eq!(code, 2, "stderr:\n{stderr}");
    assert!(
        stderr.contains("without configuring Git"),
        "the refusal says what to do: {stderr}"
    );
    assert_eq!(
        fixture.git_ok(".", &["remote", "get-url", "origin"]).trim(),
        "git@github.com:acme/other.git",
        "the existing remote is untouched"
    );

    // With the explicit confirmation the same call replaces `origin`, and succeeds.
    cli.ok(&[
        "init",
        ".",
        "--name",
        "demo",
        "--remote",
        "git@github.com:acme/demo.git",
        "--add-git-remote",
        "--force",
    ]);
    assert_eq!(
        fixture.git_ok(".", &["remote", "get-url", "origin"]).trim(),
        "git@github.com:acme/demo.git"
    );
}

#[test]
fn configure_add_plans_before_it_changes_anything() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    fixture.write("new-module/main.rs", "fn main() {}\n");
    let manifest_path = fixture.path().join(".gitmesh/project.toml");
    let manifest_before = std::fs::read_to_string(&manifest_path).unwrap();
    let cli = Cli::new(fixture.path());

    // A dry run prints the plan the same service builds for the interface, and writes
    // nothing at all.
    let stdout = cli.ok(&["configure", "add", "new-module", "--git-init", "--dry-run"]);
    assert!(stdout.contains("changes to the configuration"), "{stdout}");
    assert!(
        stdout.contains("create a Git repository in 'new-module'"),
        "{stdout}"
    );
    assert!(stdout.contains("add 'new-module' to GitMesh"), "{stdout}");
    assert!(stdout.contains("Dry run: nothing was changed"), "{stdout}");
    assert!(!fixture.path().join("new-module/.git").exists());
    assert_eq!(
        std::fs::read_to_string(&manifest_path).unwrap(),
        manifest_before
    );

    // A directory that is not a repository and was not asked to become one is refused
    // before anything runs, with the reasons the plan collected.
    let (_, stderr, code) = cli.out(&["configure", "add", "new-module"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("is not a Git repository"), "{stderr}");
    assert!(!fixture.path().join("new-module/.git").exists());
    assert_eq!(
        std::fs::read_to_string(&manifest_path).unwrap(),
        manifest_before
    );

    // The real thing: initialised, added, and tracked by exactly one repository.
    let stdout = cli.ok(&[
        "configure",
        "add",
        "new-module",
        "--git-init",
        "--untrack-from-root",
    ]);
    assert!(stdout.contains("Added repository 'new-module'"), "{stdout}");
    assert!(fixture.path().join("new-module/.git").is_dir());
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(manifest.contains("id = \"new-module\""), "{manifest}");

    // Repeating the same command changes nothing and succeeds: an operation that is
    // already done is not an error, and the command line does not claim it added a
    // repository a second time either.
    let stdout = cli.ok(&[
        "configure",
        "add",
        "new-module",
        "--git-init",
        "--untrack-from-root",
    ]);
    assert!(stdout.contains("Nothing to do"), "{stdout}");
    assert!(!stdout.contains("Added repository"), "{stdout}");
    assert_eq!(std::fs::read_to_string(&manifest_path).unwrap(), manifest);
}

#[test]
fn configure_remove_and_remote_go_through_the_same_plan() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    // The root repository tracks the files of the new directory, and the directory becomes
    // a repository of its own *without* untracking: the files then belong to two
    // repositories, which is exactly the state a removal has to warn about.
    fixture.write("engine/lib.rs", "pub fn go() {}\n");
    fixture.add_all(".");
    fixture.commit(".", "root tracks engine/lib.rs");
    let cli = Cli::new(fixture.path());
    cli.ok(&["configure", "add", "engine", "--git-init"]);
    assert!(fixture.path().join("engine/.git").is_dir());
    assert_eq!(
        fixture.git_ok(".", &["ls-files", "--", "engine"]).trim(),
        "engine/lib.rs"
    );

    // Renaming changes the name and never the directory.
    let stdout = cli.ok(&["configure", "rename", "root", "root-repository"]);
    assert!(
        stdout.contains("Renamed repository 'root' to 'root-repository'"),
        "{stdout}"
    );
    assert!(fixture
        .load_project()
        .repository("root-repository")
        .is_some());
    assert_eq!(
        fixture.load_project().root_repository().relative_slash(),
        "."
    );
}

#[test]
fn configure_clone_clones_into_a_new_directory_and_refuses_a_non_empty_one() {
    let f = RepoFixture::named("cli-configure-clone");
    f.project_with(&[(".", ".")]);
    let bare = f.create_bare("cli-core.git");
    let seed = f.clone_outside(&bare, "cli-seed");
    std::fs::write(seed.join("core.txt"), "core").unwrap();
    let git = f.runner().repo(&seed);
    git.run_checked(&["add", "-A"]).unwrap();
    git.run_checked(&["commit", "-q", "-m", "seed"]).unwrap();
    git.run_checked(&["push", "-q", "origin", "HEAD:main"])
        .unwrap();

    let cli = Cli::new(f.path());
    let remote = bare.to_string_lossy().to_string();
    let (stdout, stderr, code) = cli.out(&[
        "configure",
        "clone",
        "libs/core",
        "--remote",
        &remote,
        "--id",
        "core",
    ]);
    assert_eq!(code, 0, "stdout: {stdout}\nstderr: {stderr}");
    assert!(f.path().join("libs/core/core.txt").exists());

    // A directory with files is refused, and the files are left exactly as they were.
    std::fs::create_dir_all(f.path().join("libs/busy")).unwrap();
    std::fs::write(f.path().join("libs/busy/notes.txt"), "mine").unwrap();
    let (_, stderr, code) = cli.out(&["configure", "clone", "libs/busy", "--remote", &remote]);
    assert_eq!(
        code, 2,
        "a refused clone exits with the refusal code: {stderr}"
    );
    assert!(stderr.contains("not empty"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(f.path().join("libs/busy/notes.txt")).unwrap(),
        "mine"
    );
    assert!(!f.path().join("libs/busy/.git").exists());
}

// ------------------------------------------------------- ui directory handling --
//
// `gitmesh ui` opens an existing project. The directory comes from the positional
// PATH or the global `-C`, never both; relative paths resolve from the current
// directory; nothing is created. These tests run only in temporary projects.

#[test]
fn ui_explicit_path_opens_the_project_from_a_cwd_outside_it() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let outside = Cli::new(fixture.outside_path());
    let abs = fixture.path().to_string_lossy().to_string();

    let (stdout, stderr, code) = outside.out(&["ui", &abs]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("is available at"), "{stdout}");
    assert!(stdout.contains("gitmesh-"), "{stdout}");
    // Nothing is written next to the caller.
    assert!(!fixture.outside_path().join(".gitmesh").exists());
}

#[test]
fn ui_global_project_flag_selects_the_project_from_outside() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let outside = Cli::new(fixture.outside_path());
    let abs = fixture.path().to_string_lossy().to_string();

    for args in [vec!["-C", &abs, "ui"], vec!["ui", "-C", &abs]] {
        let (stdout, stderr, code) = outside.out(&args);
        assert_eq!(code, 0, "{args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("is available at"), "{args:?}: {stdout}");
    }
}

#[test]
fn ui_relative_path_resolves_from_the_current_directory() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    fixture.mkdir("sub/deep");
    let cli = Cli::new(fixture.path());

    // A sub-directory of the project opens the project that contains it.
    for args in [
        vec!["ui", "sub/deep"],
        vec!["ui", "./sub/../sub"],
        vec!["-C", "sub", "ui"],
    ] {
        let (stdout, stderr, code) = cli.out(&args);
        assert_eq!(code, 0, "{args:?}\nstdout:\n{stdout}\nstderr:\n{stderr}");
        assert!(stdout.contains("is available at"), "{args:?}: {stdout}");
    }
}

#[test]
fn ui_missing_path_is_reported_with_the_base_directory() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let cli = Cli::new(fixture.path());

    let (stdout, stderr, code) = cli.out(&["ui", "no-such-dir"]);
    assert_eq!(code, 2, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("directory does not exist"), "{stderr}");
    assert!(stderr.contains("no-such-dir"), "{stderr}");
    assert!(stderr.contains("current directory"), "{stderr}");
}

#[test]
fn ui_file_path_is_rejected() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let cli = Cli::new(fixture.path());

    let (_, stderr, code) = cli.out(&["ui", "README.md"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("not a directory"), "{stderr}");
}

#[test]
fn ui_directory_without_a_project_fails_and_creates_nothing() {
    let fixture = RepoFixture::new();
    // The fixture's remote directory is not a GitMesh project.
    let plain = fixture.outside_path();
    assert!(!plain.join(".gitmesh").exists());
    let cli = Cli::new(fixture.path());
    let abs = plain.to_string_lossy().to_string();

    let (stdout, stderr, code) = cli.out(&["ui", &abs]);
    assert_eq!(code, 2, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("no GitMesh project found"), "{stderr}");
    assert!(stderr.contains("gitmesh init"), "{stderr}");
    assert!(
        !plain.join(".gitmesh").exists(),
        "ui must never init a project"
    );
}

#[test]
fn ui_path_and_project_flag_together_are_rejected() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let outside = Cli::new(fixture.outside_path());
    let abs = fixture.path().to_string_lossy().to_string();

    let (_, stderr, code) = outside.out(&["-C", &abs, "ui", &abs]);
    assert_eq!(code, 2);
    assert!(stderr.contains("not both"), "{stderr}");
    // The same rule applies to `gui`, which is rejected before any server starts.
    let (_, stderr, code) = outside.out(&["-C", &abs, "gui", &abs]);
    assert_eq!(code, 2);
    assert!(stderr.contains("not both"), "{stderr}");
}

#[test]
fn ui_broken_manifest_is_reported_before_the_terminal_starts() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    fixture.write(".gitmesh/project.toml", "this is = not [valid toml\n");
    let cli = Cli::new(fixture.path());

    let (stdout, stderr, code) = cli.out(&["ui"]);
    assert_eq!(code, 2, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stderr.contains("gitmesh:"), "{stderr}");
}

#[test]
fn ui_tui_alias_accepts_the_same_directory_forms() {
    let fixture = RepoFixture::new();
    fixture.project_with(&[("root", ".")]);
    let outside = Cli::new(fixture.outside_path());
    let abs = fixture.path().to_string_lossy().to_string();
    let (stdout, stderr, code) = outside.out(&["tui", &abs]);
    assert_eq!(code, 0, "stdout:\n{stdout}\nstderr:\n{stderr}");
    assert!(stdout.contains("is available at"), "{stdout}");
}
