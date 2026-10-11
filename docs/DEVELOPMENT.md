# Developing GitMesh

This guide is for people who change GitMesh's code. To install GitMesh as a tool, read
[INSTALLATION.md](INSTALLATION.md). For the design, read [ARCHITECTURE.md](ARCHITECTURE.md).

## 1. Prerequisites

| Need | Required for | Notes |
| --- | --- | --- |
| Rust 1.88 or newer | building | The declared minimum is `rust-version` in `Cargo.toml`. The project was built and tested with stable 1.99.0; 1.88 itself is not tested. `./build.sh` finds or installs Rust (see the README). |
| `rustfmt` and `clippy` | `cargo fmt` and `cargo clippy` | `rustup component add rustfmt clippy` |
| Git | everything (GitMesh runs the real `git`) | Tested with Git 2.47.3. The minimum Git version has not been established. |
| A C linker (`cc`) | linking | `build.sh` checks for it. |
| Node.js | the GUI client-logic test (`tests/client.rs`) | Optional. Without Node the test is skipped with a notice. |
| Python 3 (standard library) | the end-to-end workflow scripts | Optional, see section 3. |
| `playwright` and Chromium | browser checks | Optional, test tooling only. |
| `pyte` | terminal checks | Optional, test tooling only. |

GitMesh itself has no runtime dependency beyond Git. Playwright and pyte are test tools and are never
imported by GitMesh.

## 2. Build and fast checks

```console
$ cargo build                               # debug build: target/debug/gitmesh
$ cargo build --release --locked            # optimised build: target/release/gitmesh
$ ./build.sh                                # the release build, with toolchain checks
$ ./tools/build-script-check.sh             # offline checks of build.sh and toolchain discovery
$ ./tools/install-check.sh                  # install.sh in temporary directories (needs a release build)
```

`tools/install-check.sh` never touches `~/.local/bin`. It uses the existing release build (`--no-build`),
so run `./build.sh` first.

## 3. The validation sequence

Run these before a change is considered validated. Each one must exit 0.

```console
$ cargo fmt --all -- --check
$ cargo clippy --all-targets --all-features --locked -- -D warnings
$ cargo test --locked
$ ./build.sh
```

`cargo test --locked` runs unit tests (in the modules), the library workflow tests (`tests/workflows.rs`),
the process-level CLI tests (`tests/cli.rs`), the lifecycle, clone-recovery, remote-history and
repository-separation tests (`tests/lifecycle.rs`, `tests/clone_recovery.rs`, `tests/remote_history.rs`,
`tests/repository_separation.rs`), and the GUI client-logic test (`tests/client.rs`, which needs Node to run).
The shared helpers are in `tests/common/mod.rs` and `src/testkit.rs`.

The end-to-end scripts need a release build (`cargo build --release --locked`) and a free TCP port range
(section 4).

## 4. Validation tools

| Tool | Command | What it checks | Needs |
| --- | --- | --- | --- |
| Workflow: interface | `./tools/gui-workflow.py` | The interface over real HTTP on a multi-repository project, checked against the CLI's view of the same project | python3, git, port 7411 |
| Workflow: setup | `./tools/setup-workflow.py` | Project setup from an ordinary directory through the interface's endpoints, with the result checked on disk | python3, git, ports 7412–7413 |
| Workflow: repositories | `./tools/repository-workflow.py` | Repository management after creation (add, adopt, rename, remote, remove), checked on disk and through a second interface on port 7415 | python3, git, ports 7414–7415 |
| Demonstration | `./examples/demo.sh [work-dir]` | Three repositories, local bare remotes, a real conflict, the CLI end to end | git; `GITMESH=<binary>` or `gitmesh` on `PATH` |
| Browser | `python3 tools/gui-browser-check.py [binary]` | Chromium: responsive layout (no horizontal overflow at 360, 768 and 1280 px) and the main interface flows | playwright + Chromium |
| Browser: clone | `python3 tools/gui-clone-check.py [binary]` | Cloning a remote from the Repositories tab; outcomes checked on disk and in the manifest | playwright + Chromium |
| Browser: recovery | `python3 tools/gui-recovery-check.py [binary]` | A refused add offers *Clone repository*; cancel changes nothing; apply clones the history | playwright + Chromium |
| Terminal | `python3 tools/tui-pty-check.py [binary]` | The terminal interface run in a pseudo-terminal and read back through a terminal emulator | pyte |
| Installer | `./tools/install-check.sh` | `install.sh`: install, update, custom and `~` destinations, refusals, `--force`, symlinks, uninstall safety | release build |
| Build script | `./tools/build-script-check.sh` | `build.sh` and `toolchain.sh` with fake toolchains; no real build | bash, coreutils |

Each workflow and browser script creates its project in a new temporary directory and prints the path.
Existing directories are never removed. The scripts do not delete the directories they create, so remove
them yourself when you have finished with them.

Setup for the browser and terminal tools (test tooling only; GitMesh has no dependency on it):

```console
$ python3 -m venv ~/.venvs/gitmesh-tests
$ ~/.venvs/gitmesh-tests/bin/pip install playwright pyte
$ ~/.venvs/gitmesh-tests/bin/python -m playwright install chromium
```

On Debian-based systems Chromium may also need system libraries (for example `libnss3`, `libnspr4`,
`libatk1.0-0t64`, `libasound2t64`). Install them with your package manager; the tools do not do it for you.

## 5. Test strategy

GitMesh's behaviour is Git's behaviour, so the tests use real Git repositories in temporary directories.
Local **bare repositories** stand in for hosted remotes, so push, pull, conflict and divergence scenarios
run offline and deterministically.

| Layer | Where | What it covers |
| --- | --- | --- |
| Unit | `#[cfg(test)]` in each module | Parsing (`status --porcelain=v2`, remotes), path normalisation, manifest validation, ownership, outcome classification, JSON output |
| Library workflows | `tests/workflows.rs` | Multi-repository workflows: configuration, status and ownership, commit, branch, checkout, merge, pull, push, conflicts, partial failures, idempotency, restart |
| Application layer | `src/service.rs` | Project opening, unified status, change ownership, operation aggregation, JSON view models |
| Repository management | `src/manage.rs` | Plan-driven changes to an existing project: candidate analysis, refusals, add / adopt / initialise / untrack / rename / remote / remove, idempotency, partial failures, evidence |
| Project setup | `src/setup.rs` | Plan-driven project creation: inspection, plan, apply, verification |
| CLI process | `tests/cli.rs` | The real binary: argument parsing, exit codes, text output and `--json` output |
| Terminal state machine | `src/ui/app.rs`, `src/ui/mod.rs` | Key-driven flows over real repositories, without drawing to a terminal |
| Terminal rendering | `src/ui/render.rs` | ratatui test backend at sizes from 1×1 to 200×60: no panics, the resize notice, truncation |
| GUI model and HTTP | `src/gui/{mod,editor,server}.rs` | The model the interface renders, routing, origin and host guards, operation lifecycle, asset completeness |
| GUI client logic | `src/gui/static/app.js` (between its `clientLogic` markers) and `tests/client.rs` | The interface's pure logic, executed under Node. Skipped with a notice when Node is absent |
| Lifecycle and recovery | `tests/lifecycle.rs`, `tests/clone_recovery.rs`, `tests/remote_history.rs`, `tests/repository_separation.rs` | Repository lifecycle, clone recovery, remote-history checks, root and child separation |

Fixtures live in `src/testkit.rs` and `tests/common/mod.rs`. `RepoFixture` creates a project root that is a
real Git repository, initialises external repositories, publishes them to bare remotes
(`fixture.publish(".", "remotes/root.git")`), and creates clones that play "another developer"
(`fixture.clone_outside(...)`). Bare remotes and other clones are created **outside** the project, so the
project is never dirty by accident.

`fixture.add_all(repo)` follows the product's staging rule by excluding nested repositories. A nested
repository must never be staged as a gitlink, neither by GitMesh nor by a test helper.

Running one test:

```console
$ cargo test --lib manifest::tests::detects_overlapping_paths
$ cargo test --test workflows partial_network_failure_leaves_successful_repositories_updated
$ cargo test --test client                # GUI client logic; needs Node
```

Debugging a failing test: temporary directories are removed when a test ends. To inspect one, print its
path and keep the fixture alive (for example `println!("{}", fixture.path().display());` followed by
`std::mem::forget(fixture);`), and remove the change before committing it.

## 6. Adding a command

1. **Core first.** Add the operation to `src/ops/` as a function returning `Result<OperationReport>`. Use
   `ops::util::each_repository`, which provides inspection, identity verification, per-repository
   resilience and outcome classification.
2. **Classify outcomes.** Use `Success` only when the repository is in the desired state because of the
   operation, `Skipped` when there was nothing to do, `Conflict` for merge conflicts, and `Failed` for
   anything else. Never let a failure look like a success.
3. **Never discard work.** Only run Git commands that Git refuses to run destructively.
4. **Test it.** Add tests in the `ops` module using `RepoFixture`, and a workflow test in
   `tests/workflows.rs` if the command is part of a larger scenario.
5. **Expose it.** Add the subcommand to `src/cli.rs` and a branch in `run()` in `src/main.rs`. Add a key
   binding in `src/ui/mod.rs` only if it belongs in the terminal interface (the terminal interface is
   deliberately small; see [TUI.md](TUI.md)).

## 7. Terminal interface

The terminal interface is split so that logic is testable and rendering is simple:

* `src/ui/app.rs`: the `App` state machine. It calls the same `ops::*` functions as the CLI.
* `src/ui/render.rs`: ratatui drawing from `App` state.
* `src/ui/mod.rs`: the event loop, key translation and terminal setup.

Because the key handler is a plain function over `App`, tests drive whole flows by sending key events.

## 8. Hosting providers

`src/providers/mod.rs` defines the `Provider` trait (id, display name, web base URL, URL matching, remote
parsing). `src/providers/github.rs` implements it for GitHub, including the `gh repo create` hint that the
setup wizard prints.

* Provider code must never be required for local operations. Nothing outside `src/providers/` may depend
  on a provider being present.
* No network calls belong in the core. A future API client must sit behind the trait, be optional, and fail
  cleanly when offline.
* `gitmesh remotes` shows the provider view of each remote; `gitmesh --json remotes` gives the JSON form.

## 9. Code conventions

* Errors carry context: which repository, which path, which command failed. Configuration problems are
  collected with `Error::InvalidConfiguration(Vec<String>)`, so every problem is reported at once.
* Git arguments are passed as a vector to the `git` process, never through a shell.
* Keep dependencies few; each new dependency needs a clear reason. `src/json.rs` is GitMesh's own JSON
  writer.
* Document *why* in module headers, and keep comments from restating the code.
* The public modules are listed in `src/lib.rs`.

## 10. Project layout

```text
gitmesh/
├── Cargo.toml  Cargo.lock           crate and locked dependencies
├── build.sh                         release build with toolchain checks
├── install.sh                       per-user installation (see INSTALLATION.md)
├── README.md
├── docs/                            user and developer documentation
├── examples/demo.sh                 runnable end-to-end demonstration
├── reports/                         dated development reports (see reports/README.md)
├── src/
│   ├── main.rs  cli.rs              entry point, argument parsing, rendering
│   ├── lib.rs                       crate root and module layering
│   ├── service.rs                   application layer shared by all front ends
│   ├── setup.rs                     project creation: inspect, plan, apply, verify
│   ├── manage.rs                    repository management after creation
│   ├── analyzer.rs                  unified status and change ownership
│   ├── discovery.rs                 scanning, assignment, verified handles
│   ├── model.rs                     logical project and physical repository model
│   ├── manifest/                    TOML schema, validation, load and save
│   ├── ops/                         commit, branch, sync, push, stage, shared utilities
│   ├── git/                         Git CLI execution, status parsing, history and remote checks
│   ├── providers/                   hosting providers (GitHub)
│   ├── ui/                          terminal interface
│   ├── gui/                         web interface: model, HTTP/SSE server, embedded assets
│   │   └── static/                  index.html, app.css, app.js, client.test.js
│   ├── paths.rs  json.rs  error.rs
│   └── testkit.rs                   test fixtures
├── tools/
│   ├── toolchain.sh                 Rust discovery (sourced by build.sh; not executable)
│   ├── rust-env.sh                  run a command with the discovered toolchain
│   ├── build-script-check.sh        offline checks of build.sh
│   ├── install-check.sh             installer checks
│   ├── gui-workflow.py              interface workflow
│   ├── setup-workflow.py            project-creation workflow
│   ├── repository-workflow.py       repository-management workflow
│   ├── gui-browser-check.py         browser checks
│   ├── gui-clone-check.py           browser checks: cloning
│   ├── gui-recovery-check.py        browser checks: clone recovery
│   └── tui-pty-check.py             terminal checks
└── tests/
    ├── workflows.rs  cli.rs  client.rs
    ├── lifecycle.rs  clone_recovery.rs  remote_history.rs  repository_separation.rs
    └── common/mod.rs                shared test helpers
```

## 11. Troubleshooting

| Symptom | Cause and fix |
| --- | --- |
| `the git executable could not be run (...): ... GitMesh drives Git through the Git CLI and requires it on PATH.` | Git is missing or not on `PATH` for this shell. Check that `git --version` works there. |
| `refusing to operate on <dir>: it is inside the Git repository rooted at <top>` | A configured repository path is inside another repository instead of being its own root. Fix the configuration with `gitmesh configure remove`, then `gitmesh configure add`. |
| `directory does not exist: <path>` | A configured repository directory is missing. Restore it, or remove it from the configuration with `gitmesh configure remove`. GitMesh does not repair this silently. |
| Credentials are requested but GitMesh does not prompt | GitMesh sets `GIT_TERMINAL_PROMPT=0`, so an operation cannot hang. Configure a credential helper or an SSH agent. |
| The browser or terminal check reports a missing module | Install the optional tools in section 4. The check does not pass when its tool is absent. |
| Chromium does not start (missing `lib*.so`) | Install the system libraries Chromium needs (section 4). |
| `cargo test --test client` prints a skip notice | Node is not on `PATH`. The test is skipped, not passed. |
| `Permission denied` running a script | The executable bit is missing. Restore it with `chmod +x <script>` (see INSTALLATION.md). |
