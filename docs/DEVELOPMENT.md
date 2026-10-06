# Developing GitMesh

## 1. Prerequisites

* Rust 1.74 or newer (`rustup toolchain install stable`)
* Git 2.25+ on `PATH` (GitMesh spawns the real `git`)
* `rustfmt` and `clippy` components (used by the validation commands below)

```console
$ rustup component add rustfmt clippy
```

## 2. Commands

```console
$ cargo build                     # debug build
$ cargo test                      # unit + integration tests (uses real Git)
$ cargo fmt --all                 # format
$ cargo fmt --all -- --check      # verify formatting (CI)
$ cargo clippy --all-targets -- -D warnings   # lint, warnings are errors
$ cargo build --release           # optimised binary: target/release/gitmesh
```

The full validation sequence used for every milestone:

```console
$ cargo fmt --all -- --check \
  && cargo clippy --all-targets -- -D warnings \
  && cargo test \
  && cargo build --release
```

## 3. Test strategy

GitMesh's behaviour *is* Git behaviour, so the tests use real Git repositories created in
temporary directories — no mocks of the Git layer. Local **bare repositories** stand in
for hosted remotes, which makes push/pull/conflict/divergence scenarios fully testable
offline and deterministically.

| Layer | Where | What it covers |
| --- | --- | --- |
| Unit | `#[cfg(test)]` in each module | Parsing (`status --porcelain=v2`, remotes), path normalisation, manifest validation, ownership, outcome classification, JSON encoding |
| Library integration | `tests/workflows.rs` | Complete multi-repository workflows: init-free configuration, status/ownership, commit, branch/checkout/merge, pull/push, conflicts, partial failures, idempotency, restart |
| CLI process | `tests/cli.rs` | The real binary: argument parsing, exit codes, rendered output, JSON output, `ui` fallback without a TTY |
| UI state machine | `src/ui/app.rs`, `src/ui/mod.rs` | Key-driven flows (commit, branch, setup/assignment, dry-run, input handling) without a terminal |

Fixtures live in `src/testkit.rs` (public, dependency-free):
`RepoFixture` creates a project root that is a real Git repository, initialises external
repositories with an initial commit, publishes repositories to bare remotes
(`fixture.publish(".", "remotes/root.git")`), and creates clones that play "another
developer" (`fixture.clone_outside(...)`). Bare remotes and other clones are created
**outside** the project directory so the project is never accidentally dirty.

`fixture.add_all(repo)` mirrors the product's staging rule by excluding nested
repositories — a nested repository must never be staged as a gitlink, neither by GitMesh
nor by a test helper.

A runnable end-to-end demonstration (creates its own workspace, bare remotes and a real
conflict, then cleans nothing up so you can inspect it):

```console
$ GITMESH=./target/release/gitmesh ./examples/demo.sh /tmp/gitmesh-demo
```

Running a single test:

```console
$ cargo test --lib manifest::tests::detects_overlapping_paths
$ cargo test --test workflows partial_network_failure
```

Debugging a failing scenario: set `GITMESH_TEST_KEEP=1`-style caching is not needed —
`TempDir` removes its directory on drop, so add a `println!("{}", fixture.path().display())`
plus a `std::mem::forget(fixture)` when you want to inspect the state by hand.

## 4. Adding a command

1. **Core first.** Add the operation to `src/ops/` as a function returning
   `Result<OperationReport>`. Use `ops::util::each_repository`, which gives you the
   inspection, identity verification, per-repository resilience and outcome model.
2. **Classify outcomes.** Use `Success` only when the repository is in the desired state
   because of the operation; `Skipped` when there was nothing to do; `Conflict` for merge
   conflicts; `Failed` for anything else — never make a failure look like a success.
3. **Never discard work.** Only run Git commands that Git refuses to run destructively.
4. **Test it.** Add tests in the `ops` module using `RepoFixture`, plus a workflow test in
   `tests/workflows.rs` if the command takes part in a larger scenario.
5. **Expose it.** Add the subcommand to `src/cli.rs` and a branch in `run()` in
   `src/main.rs` (render with `render_report`), then add a key binding in
   `src/ui/mod.rs` if it belongs in the terminal UI.

## 5. Terminal UI

The UI is split so that logic is testable and rendering is dumb:

* `src/ui/app.rs` — `App`: screen (`Setup` / `Project`), tree rows, selection, status,
  last report, log, modal input, dry-run flag. All operations are methods that call
  `ops::*` / `analyzer` / `discovery` — the same functions the CLI uses.
* `src/ui/render.rs` — pure `ratatui` drawing from `App` state.
* `src/ui/mod.rs` — the event loop, key mapping (`handle_key`), terminal setup and the
  graceful fallback when stdout is not a TTY.

Because `handle_key` is a plain function over `App`, tests drive full flows by sending
key events (`KeyCode::Char('c')` to open the commit prompt, typing, `Enter` to submit).

## 6. Hosting providers (GitHub and others)

`src/providers/mod.rs` defines the `Provider` trait: id, display name, web base URL, URL
matching and remote parsing. `src/providers/github.rs` implements it for GitHub (scp-like
and URL forms, owner/repository extraction, SSH/HTTPS URL construction).

Guidelines:

* Provider code must never be required for local operations. Nothing outside
  `src/providers/` may depend on a provider being present.
* No network calls belong in the core. A future API client should live behind the trait,
  be feature-gated or explicitly invoked (`gitmesh github ...`), and fail cleanly when
  offline.
* `gitmesh remotes` and `gitmesh remotes --json` already expose the provider view of every
  configured remote (`provider`, owner/name, web URL).

## 7. Code conventions

* Errors carry context: which repository, which path, which command failed. Prefer
  `Error::InvalidConfiguration(vec![...])` for configuration problems so users see every
  problem at once.
* No `unwrap()` outside tests and `testkit`; no process-global state; no shell
  interpretation of Git arguments (arguments are always passed as a vector).
* Keep the dependency list small. New dependencies need a clear justification; the JSON
  writer exists precisely to avoid one.
* Document *why* in module headers, and keep comments free of re-statement of the code.
* Public API surface is small on purpose: `gitmesh::{analyzer, discovery, git, manifest,
  model, ops, paths, providers, ui, testkit}`.

## 8. Project layout

```text
gitmesh/
├── Cargo.toml
├── README.md
├── docs/
│   ├── ARCHITECTURE.md
│   ├── MANIFEST.md
│   └── DEVELOPMENT.md
├── examples/demo.sh               runnable end-to-end demonstration
├── reports/
│   ├── DEVELOPMENT_REPORT.md      milestone-by-milestone record
│   └── FINAL_REPORT.md            hardening audit and readiness assessment
├── src/
│   ├── main.rs  cli.rs            entry point, argument parsing, rendering
│   ├── lib.rs                     crate root: module layering
│   ├── analyzer.rs                unified status and change ownership
│   ├── discovery.rs               scanning, assignment, verified handles
│   ├── error.rs  paths.rs  json.rs
│   ├── model.rs                   logical project / physical repository model
│   ├── manifest/                  TOML schema, validation, load/save
│   ├── ops/                       commit, branch, sync, push, shared utilities
│   ├── providers/                 GitHub (optional) provider foundation
│   ├── git/                       Git CLI execution and output parsing
│   ├── ui/                        terminal interface (state machine + rendering)
│   └── testkit.rs                 test fixtures (temporary repositories)
└── tests/
    ├── workflows.rs               library-level multi-repository workflows
    └── cli.rs                     process-level CLI tests
```

## 9. Troubleshooting

| Symptom | Cause / fix |
| --- | --- |
| `the git executable could not be run` | `git` is not on `PATH` for the process running GitMesh. |
| `refusing to operate on <dir>: it is inside the Git repository rooted at <top>` | A configured repository path is a subdirectory of an existing repository instead of its own repository root. Fix the manifest (`gitmesh configure remove` / `add`). |
| `repository 'x' is missing: <path> does not exist` | A configured directory disappeared. Restore it, or remove it from the configuration — GitMesh never repairs this silently. |
| `Credentials are required but GitMesh does not prompt` | GitMesh disables terminal prompts so operations cannot hang. Configure a credential helper or SSH agent. |
| Root status shows files inside an external repository | The root repository still tracks them (legacy state). GitMesh reports it; fix it deliberately with `git rm -r --cached <dir>` in the root repository. |
