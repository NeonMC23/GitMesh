# GitMesh graphical interface — development report

Report for the introduction of the **first real graphical interface** in GitMesh: audit,
architecture, technology choice, implementation, refactors, tests, manual validation and
readiness. Milestone numbering is maintained by the project owner; this document is
referred to as the *graphical interface* work in
[`DEVELOPMENT_REPORT.md`](DEVELOPMENT_REPORT.md) and is the graphical counterpart of the
readiness assessment in [`FINAL_REPORT.md`](FINAL_REPORT.md).

---

## 1. Audit findings (before writing any code)

The repository was audited first, as required, rather than assumed:

| Item | Finding | Consequence for the GUI |
| --- | --- | --- |
| Repository state | `main`, clean tree, 39 tracked files, all milestones 1–10 delivered | The GUI was added on top of a working product, not a prototype |
| Front ends | `cli` (`src/cli.rs`, `src/main.rs`) and `ui` (`src/ui/`, ratatui) | Two front ends already existed and shared `ops` — the GUI had to join them, not fork them |
| Orchestration | `ops::{commit,branch,sync,push}` with per-repository resilience and `OutcomeKind` classification | Already the correct seam; no Git behaviour had to be re-implemented |
| Analysis | `analyzer::ProjectStatus` with `OwnedChange` carrying `repository_path` | `OwnedChange` lacked the owning repository's path, which the changes view needs |
| Shared vocabulary | Outcome classification, state classification and the `status` column labels lived in `cli.rs` / `model.rs` | A GUI importing `cli` would have been an architectural lie; the vocabulary needed one home |
| Progress | Nothing: operations ran to completion and returned a report | A front end that must show `repo → ✓/…/pending` needs a seam that `ops` does not already have |
| Rendering | ratatui/crossterm only | No GUI rendering machinery existed |
| Dependencies | 5 direct dependencies (clap, serde, toml, thiserror, ratatui+crossterm) | Adding none keeps GitMesh in its "single binary, no runtime dependencies" class |
| Environment | no display server (`DISPLAY` unset), no GUI toolkit libraries | A native toolkit could be written but **not run or verified** here |

Auditing also settled what the GUI must **not** become: a mirror of `git status` per
repository ("a dashboard of unrelated repo cards"). The milestone's premise is that the
user thinks in one project, so the interface has to be organised around the project — with
repository detail available, not foregrounded.

## 2. Architecture before implementation

```text
cli ─┐
ui  ─┼─→ ops ─→ analyzer ─→ model/manifest/discovery ─→ git
     ┘
```

`cli`, `ui` and `ops` were the only layers that existed. Three gaps blocked a GUI:

1. **No front-end-neutral application layer.** Useful logic that both `cli` and the future
   GUI need (open a project, classify a repository state, build a status view, run an
   operation and describe the result) was either in `cli.rs` or duplicated per command.
2. **No shared vocabulary.** `RepositoryStateKind`, `ChangeState` and the human wording of
   the status column lived with the CLI, so a second front end would inevitably drift.
3. **No progress seam.** `ops` returned a finished report; nothing reported "repository X
   is being worked on now".

The GUI was therefore built **on the smallest possible new layer**: one application module
(`src/service.rs`) plus one presentation module (`src/gui/`). No existing behaviour was
moved into the GUI, and no Git invocation, ownership rule or outcome classification was
written twice.

## 3. Technology decision and rationale

**Local web interface**: `gitmesh gui` binds `127.0.0.1:7345` by default, serves one
self-contained HTML page from the binary itself, and talks to the Rust core over a tiny
HTTP API with server-sent events for progress.

| Option | Verdict | Why |
| --- | --- | --- |
| **Local web UI (chosen)** | Implemented | Zero new dependencies (`std::net` + `include_str!`), works over SSH and in containers, browser renders everything, nothing leaves the machine, and it is the only option that is **verifiable in this environment** |
| egui / eframe | Rejected | ~100 transitive crates and a graphics stack that cannot be validated here (no display). "Lightweight and maintainable" is a stated constraint |
| GTK / gtk-rs | Rejected | System libraries are not installed and could not be installed or tested reliably; runtime dependency on a toolkit version |
| Tauri / Electron-like | Rejected | Node/webview toolchain plus per-platform runtime bundling — a heavyweight answer to a lightweight problem |

The client is hand-written HTML/CSS/JS with **no framework, no build step, no CDN**: three
files compiled into the binary by `include_str!`. The page makes requests only to its own
origin (`/api/*`), which is asserted by a test. The server is ~950 lines of `std::net`
HTTP/1.1 + SSE with a thread per connection and a 30-second read timeout, and refuses
cross-site and misdirected requests (see §6).

## 4. Implemented

### 4.1 Application layer — `src/service.rs` (1 514 lines, 23 tests)

`ProjectSession` is the API every front end can use:

* **Project opening**: `open(path)`, `with_project(...)`, `project()`, `name()`, `root()`,
  `manifest_path()`, `runner()`, `analyzer()` — discovery and validation stay in
  `manifest`/`discovery`.
* **Reading**: `status()`, `status_with_analyzer(...)`, `changes()`, `conflicts()`,
  `pending_work()`, `project_branches()`, plus the JSON view models
  (`status_view_json`, `project_view_json`, `repository_view_json`, `change_view_json`,
  `branches_view_json`, `operation_view_json`).
* **Writing**: `commit_observed(...)`, `branch_observed(...)`, `sync_observed(...)`,
  `push_observed(...)` — thin, typed wrappers over `ops`, each returning the existing
  `OperationReport`. The plain methods (`commit`, `branch`, `sync`, `push`) delegate with
  a silent observer, so the CLI is untouched.
* **Vocabulary** (previously CLI-local): `ChangeState`, `status_column_label(&StatusEntry)`,
  `branch_cell`, `repository_state_kind`, `project_state_kind`, `PendingWork`,
  `ProjectBranch`, `report_kind`.

### 4.2 Progress seam — `src/ops` (`OperationObserver`, `each_repository_observed`)

`OperationObserver` has four methods (`on_start`, `repository_started`,
`repository_finished`, `on_end`) and a `silent()` implementation. `ops::each_repository`
keeps its old body and forwards to `each_repository_observed` with a silent observer, and
the four operations gained `*_observed` entry points; their original signatures still
exist and behave identically. This is deliberately *not* a callback into a widget: the
observer takes `RepoOutcome`/summary data, and the GUI decides on its own that a
repository-start event becomes an SSE frame.

### 4.3 Presentation — `src/gui/`

| File | Role |
| --- | --- |
| `mod.rs` (989) | `Gui` state (open project, busy flag, current operation, stored reports, event log), `GuiOperation` (Commit, BranchCreate/Checkout/Start/Merge/Delete, Fetch, Pull, Push → label + sentence), the background worker emitting `started`/`repository`/`outcome`/`finished`/`failed`, `run()` banner and browser opening, `--dry-run` |
| `editor.rs` (434) | Builds the model the interface renders: `project`, `repositories`, `changes`, `pending`, `conflicts`, one `tree`, `branches`, commit/push/branch readiness with reasons, and the sentences/badges for states. Pure: no I/O, no Git |
| `server.rs` (957) | `std::net` HTTP/1.1 + SSE; routes, guards, form decoding, operation dispatch; thread per connection |
| `asset.rs` (110) | Embeds `index.html`, `app.css`, `app.js`, `client.test.js`; slices the client-logic region of `app.js`; asserts the assets are complete and network-free |
| `static/index.html` (204) | One page: topbar (project, branch, dry-run, Refresh, Open), notices, six tabs, operation panel, welcome panel, open dialog, status bar |
| `static/app.css` (166) | Dark theme, no external font or image |
| `static/app.js` (910) | Pure client logic (between markers) + DOM layer (fetch, `EventSource`, tab switching, keyboard `R`/`D`/`1`-`6`, idle auto-refresh) |
| `static/client.test.js` (367) | 88 assertions over the extracted logic, executed by `tests/client.rs` under Node |

### 4.4 The requested scope, point by point

1. **Project opening** — the directory is detected on startup (`.gitmesh/project.toml` at
   or above the start directory). No project: the welcome panel shows the directory that
   was searched and the exact reason, with an input to open another directory
   (`POST /api/open`). Nothing new was invented for creating or cloning a project: the
   message points at `gitmesh init`.
2. **Project overview** — name, root, manifest path, configured repositories with their
   paths and roles, current branch across the project, overall state. Read from the
   service layer; the interface never looks at a `.git` directory.
3. **Unified status** — one tree of the logical project with repository boundaries marked,
   plus a table of repository/branch/state/activity and drill-down per repository
   (branch, upstream, ahead/behind, remotes, what changed, what is blocked and why).
4. **Changes view** — one list of changes with the path relative to the logical root, the
   change type, the staging state and the **owning repository**; grouping by repository is
   available but not the default, and counts ("5 changes") are project-level.
5. **Commit workflow** — review of the affected repositories (and of the ones that cannot
   be committed, with the reason), one message box, one button; then real per-repository
   `git commit` with the same message, and a result per repository (`✓ committed`,
   `– nothing to commit`, `✗ failed`) with a clear partial-failure summary. The interface
   states explicitly that a commit is made **in every affected repository**, not globally.
6. **Branch management** — the logical branch, consistency across repositories, a
   per-repository branch table (has it / is it checked out), create & switch, switch,
   merge, delete; inconsistent repositories are named, never hidden.
7. **Pull / push** — fetch, pull (strategy: fast-forward only / merge / rebase) and push,
   reusing `ops::sync` and `ops::push`, with the same partial-failure semantics; the
   summary panel lists one outcome per repository (`✓`, `–`, `!`, `✗`).
8. **Conflicts and errors** — a distinct visual state, the project marked *conflicted*, the
   conflicted files named, the affected repositories blocked in the commit view, the
   repository and Git's own message shown for failures, and an explicit statement that
   resolution is done with normal Git tooling (with the exact commands).
9. **Operation progress** — the operation runs in a background thread; `repository` events
   stream to the page as SSE so rows move `… → ✓/–/!/✗` live and the panel ends with the
   aggregate result. The page never blocks and the model is re-read when the operation
   finishes; a reload during an operation re-attaches to it.
10. **Project information / settings** — read-only: name, root, manifest, repositories,
    paths, roles, remotes, upstreams, plus any notices GitMesh produced. Editing stays in
    `gitmesh configure` and `gitmesh ui`, as agreed.

### 4.5 HTTP surface

```text
GET  /  /index.html  /app.css  /app.js  /favicon.ico
GET  /api/health  /api/model  /api/refresh  /api/events/<id>  /api/report/<id>
POST /api/open  /api/dry-run  /api/commit  /api/branch  /api/sync  /api/push
```

`/api/events/<id>` is an SSE stream; `/api/report/<id>` returns the stored result of an
operation so a page that reconnects still sees what happened. Operation identifiers are
per-request, and the model is the single description of the project.

## 5. Refactors

Small and deliberate; no feature was rebuilt.

| Change | Reason |
| --- | --- |
| `analyzer::OwnedChange` gained `repository_path` | The changes view must name the owning repository without re-deriving ownership (which would be duplicated logic) |
| `cli.rs` lost its local outcome/state vocabulary to `service` | One source of truth for wording shared by CLI, TUI and GUI |
| `status_column_label` became a free function over `StatusEntry` | The GUI needs the same "3 modified, 1 untracked" wording as `gitmesh status` |
| `Json::with_field(key, value)` | The GUI model decorates existing JSON objects; no serialisation dependency needed |
| `paths::absolute()` extracted from `main.rs` | The GUI resolves directories the same way the CLI does (lexical normalisation, no symlink surprises) |
| `each_repository` → `each_repository_observed` (+ `OperationObserver`) | Progress without duplicating the per-repository loop |
| `cli.rs`: `GuiArgs` + `main.rs::cmd_gui` | The interface is a normal GitMesh command, sharing `-C`, `--json` and the exit-code contract |

Explicitly **not** refactored: `ops` semantics, `model`, `manifest`, `discovery`,
`providers`, `ui`. No GUI-specific code was added to any of them, and no code was moved
merely to make it "look like" the GUI.

## 6. Safety and robustness of the local server

* Binds `127.0.0.1` by default; `--host 0.0.0.0` prints a warning about the absence of
  authentication.
* **Cross-site**: a `POST` is refused with `403` unless the `Origin` header (when present)
  matches the `Host` header of that request.
* **DNS rebinding**: a request whose `Host` is not the bind address, `localhost`,
  `127.0.0.1`, `[::1]`, or a host explicitly allowed with `--allow-host NAME`, is refused
  with `421`. Nothing is allowed implicitly.
* One thread per connection with a 30 s read and write timeout; a malformed or empty
  request closes the connection instead of panicking a thread (this includes health probes
  that connect and disconnect immediately).
* No authentication, no persistence, no telemetry, no remote assets, no outbound network
  access: the page's own tests assert that the client never loads or calls anything that is
  not part of the served page.

## 7. Tests

```console
$ cargo fmt --check                                   # clean
$ cargo clippy --all-targets -- -D warnings            # clean
$ cargo test                                           # 268 tests, 0 failures
$ cargo build --release                                # 2.3 MB binary, no new dependencies
```

| Suite | Count | Covers |
| --- | --- | --- |
| `service::` | 23 | Project opening (including malformed/missing manifest), unified status, change ownership, branch consistency, operation aggregation, partial failures, conflict reporting, JSON views |
| `gui::` (model) | 11 | Operation lifecycle and event log, per-repository events, stored report, dry-run toggle, dispatch parsing, strategy parsing |
| `gui::editor` | 8 | Model construction, tree shape, readiness and reasons, staging note, empty model, state wording/badges |
| `gui::asset` | 4 | Assets complete, no remote URL/CDN/`@import`/analytics, page loads only its own files, the client-logic region has no `document.`/`window.`/`fetch(` |
| `gui::server` | 10 | Routing, assets served, model JSON, open (success and failure), commit → SSE → stored report, validation errors, cross-site `403`, misdirected `421`, allowed-host handling |
| `tests/cli.rs` (new case) | +1 (11) | The real binary serves the interface: `/api/health`, page contains the workspace, model contains `engine/lib.rs`, a real cross-repository commit via `POST /api/commit` + SSE + report, both repositories' `git log -1` show the message, then `status -s` is clean, foreign `Origin` → `403` |
| `tests/client.rs` | 2 | Runs the client-logic region extracted from `app.js` under Node (`88 assertions passed`); skips cleanly when Node is absent |
| `tests/workflows.rs` | 14 | Unchanged and still green — the CLI/core behaviour is untouched |

No existing test was weakened, renamed away or skipped; the new CLI test was added to the
existing `tests/cli.rs` with the existing raw-socket helpers.

## 8. Manual validation (real, end to end)

```console
$ cargo build --release && ./tools/gui-workflow.py
74 checks passed, 0 failed
```

`tools/gui-workflow.py` (354 lines, standard library only) creates
`/tmp/gui-e2e` with a four-repository project — `root`, `engine`, `renderer`, `tools` —
each published to its own local **bare** remote, starts the release binary, and drives the
interface over real HTTP + SSE, in 18 steps:

| Step | What was verified |
| --- | --- |
| 1–2 | Start with no project → the page reports the reason; project opens from the directory; the tree root is the project, not a repository; external repositories appear as project directories marked as repositories |
| 3–5 | Files modified in several repositories simultaneously; refresh updates the model; the changes view names the owning repository for every file (`engine/src/lib.rs`, root files, `tools/build.sh`) and the change type |
| 6–7 | One commit from one message: progress named every repository before touching it, the operation finished, exit code 0, every repository committed — and each repository has its **own** commit object and `HEAD` (two histories are unrelated objects), project clean afterwards |
| 8–9 | One logical branch created and checked out in all four repositories, shown as one branch in the interface, reported as consistent, and listed in every repository |
| 10 | One pull across the project |
| 11 | A real conflict induced in `engine` on pull: the conflict is named, the other repositories still completed, the result is reported as partial with exit code 1, the project state is *conflicted*, the conflicted file is named, the repository is blocked for commit, other repositories are unaffected, and the conflict markers are still in the file |
| 12 | The conflict resolved with normal Git (`git add` + `git commit`), then refresh: project clean again |
| 13–14 | Push across repositories: resolved repository pushed, no failures, clean repositories reported as clean rather than as errors |
| 15 | The interface stopped and started again: it opens with no project and says what it looked for, then reopens the same project via the interface; same four repositories, same branch, same clean state, the earlier commit still there |
| 16 | Opening from a **nested directory** finds the same project; two modified repositories are distinguished from the clean ones |
| 17 | **Partial failure**: the renderer's bare remote is deleted after a commit, so the push fails there and succeeds in `engine` — the failing repository is reported with Git's message, the successful one is not hidden, the overall result is *partial*, and the failure carries an explanation |
| 18 | CLI non-regression in the same project: `gitmesh status` renders, sees the same repositories, `--json` is still valid JSON, a missing project still exits `2` with the same message |

Additionally verified by hand during development: the interface served through the sandbox
preview host with `--allow-host` (host accepted, foreign `Host` → `421`, cross-site `POST`
→ `403`), `--dry-run` toggling from the status bar, and a page reload during an operation
re-attaching to the running operation and its stored report.

## 9. Deviations from the plan

Stated for the record; none of them changes behaviour asked for by the milestone.

* The application layer is **one module** (`src/service.rs`) rather than a new crate or
  `src/app/` directory — the surface is still growing and one module keeps the layering
  visible at a glance. Splitting it later is mechanical and does not affect front ends.
* The presentation module is `src/gui/` (not `web/`), because "gui" is the command name and
  the user-facing word.
* The client-side tests do not live in a separate Rust crate. Instead the pure region of
  `app.js` is sliced out at test time and executed under Node, so **the code that ships is
  the code that is tested**; this keeps the browser build step-free.
* A read-only *Project* view is included (project settings were explicitly out of scope for
  editing, but seeing the configuration is part of "project information").
* `--allow-host` was added while validating the interface behind a proxy: the host guard is
  on by default, and a proxy host has to be named explicitly.

## 10. Known limitations

* One interface for one local user: no authentication, no multi-user sessions, no HTTPS.
* No cancellation of a running operation (it runs to completion and reports).
* No diff view, no history view, and no per-file staging — the commit stages everything, as
  the CLI does.
* Conflicts are shown and explained, but resolved with Git, outside the interface.
* Configuration editing (assign/rename/remote) is CLI/TUI only.
* The idle auto-refresh is a timer (15 s), not a filesystem watcher; anything done outside
  the interface appears at the next refresh or when the user presses `R`.
* Long-running operations stream progress but not partial log output.

## 11. Technical debt

Introduced (small, all deliberate):

| Debt | Why it is acceptable now |
| --- | --- |
| The HTTP server is hand-written (`std::net`), so it supports exactly what the page needs: no chunked encoding, no keep-alive, no TLS | Keeps the product dependency-free; the page is the only client. If the API grows, a small HTTP crate becomes the natural refactor |
| The SSE stream is polled from the event log with a short interval rather than a condition variable | Sub-100 ms latency is irrelevant here, and it avoids a lock-ordering problem; it is a one-function change |
| The GUI model is built as JSON (`Json`) and not as typed structs | Fine for one consumer; the typed shapes live in `service` where the CLI needs them too |
| `app.js` is one file with a marked pure region | The marker is enforced by tests; a bundler would add a build step the project explicitly avoids |

Resolved: the duplicated state/outcome vocabulary between front ends; the missing progress
seam; `OwnedChange` not knowing its repository; two copies of "make this path absolute".

Not introduced: no new dependencies, no async runtime, no build step, no generated code, no
telemetry, no background service, no GitHub dependency.

## 12. Readiness assessment

**Implemented and verified.** The interface is a real front end over the real core: opening
a project, one overview, one status, one changes list with ownership, one commit message
over many real commits, one logical branch, one pull, one push, live progress, conflict and
partial-failure reporting — all exercised against four repositories with local bare remotes
through the HTTP interface the browser actually uses (74 automated checks over 18 steps),
plus 268 Rust tests and the client logic under Node.

**Verified for the CLI too.** No command behaviour, exit code, JSON output, dry-run,
nested-directory, `-C` or partial-failure semantic changed; the CLI suite and the workflow
suite are green, and the manual run re-checks the CLI in the same project at the end.

**Ready for daily use by two developers?** Yes, for the workflow it covers: on a project
configured with `gitmesh init` + `gitmesh configure`, day-to-day work — seeing what changed
and who owns it, committing once, switching branches, pulling, pushing — can be done
entirely in the interface, including on a machine where the developers disagree about which
repository a file belongs to. It is not yet ready for *everything* a developer does in a
day (diffs, history, per-file staging, conflict resolution, configuration editing), and it
is not a server product (one local user, no authentication).

**Missing, in priority order:** per-file diffs and history (the most-requested kind of
"detail" the model does not carry yet), staging selection, an explicit cancel for running
operations, and configuration editing in the interface.

**Complete enough to validate by hand?** Yes. `gitmesh gui` in a project configured by the
CLI, or `./tools/gui-workflow.py` for the full scripted path, reproduce everything claimed
above on a fresh machine with only Rust, Git and a browser.
