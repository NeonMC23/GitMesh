# GitMesh architecture

This document describes how GitMesh is put together, where the boundaries are, and why.
It is the reference for anyone extending the project.

## 1. The model

GitMesh distinguishes two things that other tools tend to blur together:

| Concept | Meaning |
| --- | --- |
| **Logical project** | What the user has: one directory, one tree, one branch, one status. |
| **Physical repository** | Where Git records history: a real `.git` directory with its own branches, remotes and commits. |

A logical project has **exactly one root repository** (the repository that contains the
project root, `gitmesh` metadata aside) plus **zero or more external repositories**, each
at a user-chosen directory inside the project.

The root repository conceptually owns the whole project — that is what makes
`project/src/file` meaningful — while ownership of any *concrete* path is decided by the
**longest matching repository path**:

```text
project/src/file          -> root        (no external repository matches)
project/engine/src/file   -> engine      (longest match: engine)
project/engine/sub/file   -> engine      (sub is not a repository of its own)
project/vendor/lib/x      -> vendor-lib  (longest match: vendor/lib)
```

Longest-match is what keeps ownership unambiguous even though the root repository is an
ancestor of every external repository. It is implemented once, in
[`GitMeshProject::repository_for_relative`](../src/model.rs), and every layer uses it —
the analyzer, commit, branch operations and the UI all ask the same function.

### Why not submodules?

Submodules record a *pin* (a specific commit of another repository) rather than a
directory that belongs to the project. That makes everyday work a two-step commit and
forces the user to reason about repository boundaries — exactly the experience GitMesh
removes. GitMesh stores no pins: external repositories are independent, and GitMesh
orchestrates them at operation time.

## 2. Layering

```text
┌────────────────────────────────────────────────────────────────┐
│ cli (src/cli.rs, src/main.rs)   ui (src/ui)   gui (src/gui)     │
│ argument parsing / rendering    terminal      browser interface │
│ no Git logic, no filesystem policy, no ownership rules          │
└───────────────┬────────────────────────────────┬───────────────┘
                │                                │
┌───────────────▼────────────────────────────────▼───────────────┐
│ service (src/service.rs)     application layer                  │
│ open a project · status/changes · operations · view models      │
│ progress events · presentation vocabulary shared by front ends  │
└───────────────┬────────────────────────────────────────────────┘
                │
┌───────────────▼────────────────────────────────────────────────┐
│ ops (src/ops)            one logical operation → many repos    │
│ commit · branch · sync (fetch/pull) · push · util               │
│ per-repository resilience, outcome classification               │
│ OperationObserver: reusable per-repository progress reporting   │
└───────────────┬────────────────────────────────────────────────┘
                │
┌───────────────▼────────────────────────────────────────────────┐
│ analyzer (src/analyzer.rs)   unified status, change ownership   │
└───────────────┬────────────────────────────────────────────────┘
                │
┌───────────────▼────────────────────────────────────────────────┐
│ model (src/model.rs) · manifest · discovery · paths             │
│ project configuration, physical repository model, scans         │
└───────────────┬────────────────────────────────────────────────┘
                │
┌───────────────▼────────────────────────────────────────────────┐
│ git (src/git)   safe Git CLI execution + stable output parsing  │
└────────────────────────────────────────────────────────────────┘
```

Rules that keep the layering honest:

* **`git` is the only module that knows Git's command line.** Nothing above it builds a
  Git invocation.
* **`model` is pure data.** No I/O, no Git: it is the vocabulary shared by every layer.
* **`ops` is the only module that changes repositories** (apart from `discovery`, which
  can `git init` when explicitly asked). Everything else is read-only.
* **`cli`, `ui` and `gui` contain no Git logic.** They call `service` (and, for
  configuration editing, `discovery`), so the front ends cannot drift apart. The GUI
  additionally goes through `service` exclusively: it has no direct `ops` call for
  anything but the observer-aware operation methods, and no direct `git` call at all.
* **`service` is front-end neutral.** It holds the vocabulary shared by every front end
  (`ChangeState`, `RepositoryStateKind`, `PendingWork`, the JSON view models) and the
  `*_observed` operation entry points. It adds no Git behaviour of its own.
* **`ops::OperationObserver` is the only progress seam.** Logical operations report
  "repository X started / finished with outcome Y" through it; the CLI passes a silent
  observer, the GUI turns it into server-sent events. No widget, terminal or socket is
  referenced below `service`.
* Every dependency points downwards. There is no cycle.

## 3. Module map

| File | Responsibility |
| --- | --- |
| `src/git/command.rs` | `GitRunner` / `GitRepo`: process execution, working-directory binding, environment policy, identity verification, read helpers (`head`, `remotes`, `ahead_behind`, `operation_in_progress`, ...) |
| `src/git/status.rs` | Parsing of `status --porcelain=v2 -z` and `remote -v`; `RepoStatus`, `StatusEntry`, `Head`, `Remote` |
| `src/model.rs` | `GitMeshProject`, `PhysicalRepository`, `RepositoryRole`, `RepositoryState`, ownership lookup, change counts |
| `src/manifest/` | TOML schema, semantic validation, load/save, project discovery (`find_project_root`) |
| `src/discovery.rs` | Directory scanning, repository detection, assignment checks, `assign_repository` / `unassign_repository` / `rename_repository` / `set_repository_remote`, verified handles |
| `src/analyzer.rs` | Unified project status, change ownership, ownership warnings, JSON status |
| `src/ops/` | `commit`, `branch` (create/checkout/delete/merge/show), `sync` (fetch/pull), `push`, shared `util` (selection, per-repository loop, exclusions) |
| `src/paths.rs` | Lexical normalisation, project-relative conversion, manifest path validation |
| `src/json.rs` | Minimal JSON writer used by `--json` output (no serialisation dependency) |
| `src/providers/` | Hosting provider abstraction; `github.rs` parses GitHub URLs and coordinates |
| `src/service.rs` | Application layer: `ProjectSession` (open, status, changes, operations), shared presentation vocabulary (`ChangeState`, `RepositoryStateKind`, `ProjectStateKind`, `PendingWork`, `ProjectBranch`), JSON view models (`status_view_json`, `operation_view_json`), `*_observed` operation entry points |
| `src/ui/` | `app.rs` state machine (terminal-independent), `render.rs` ratatui drawing, `mod.rs` event loop |
| `src/gui/` | `editor.rs` builds the model the interface renders from the service layer, `server.rs` is a minimal HTTP/SSE transport, `asset.rs` embeds the three front-end files, `static/` holds them (page, stylesheet, script, and the script's pure-logic tests) |
| `src/testkit.rs` | Temporary project/repository fixtures used by unit and integration tests |

## 4. Safety invariants

These are the properties the design must not lose. Each is enforced in one place and
covered by tests.

| # | Invariant | Enforcement |
| --- | --- | --- |
| 1 | A Git command never runs in the wrong repository | Repository-scoped commands only exist on `GitRepo`, which always passes `-C <dir>`; `verify_identity()` compares `rev-parse --show-toplevel` with the configured path and refuses mismatches |
| 2 | A repository never stages another repository's files | `git add -A -- . ':(exclude)<external-dir>'` plus a post-staging audit that unstages stray paths |
| 3 | The root repository does not report external content | The analyzer drops root status entries that live inside a configured external repository |
| 4 | Local work is never discarded | Only non-destructive Git commands are used; `pull` defaults to `--ff-only`; `checkout` relies on Git's own refusal to overwrite; `branch -d` (not `-D`) unless `--force` is explicit |
| 5 | One failing repository does not stop the rest | `ops::util::each_repository` iterates every repository and converts failures into outcomes |
| 6 | Results never claim success on failure | `OperationReport::is_success` requires that no outcome is `Failed` or `Conflict`; the exit code follows the same rule |
| 7 | Configuration is validated before any Git command | `manifest::validation::validate_project` runs on load, on save and on every programmatic change |
| 8 | Paths cannot escape the project | `paths::normalize_relative` rejects absolute paths, `..`, and `.git` targets |
| 9 | GitMesh never prompts invisibly | `GIT_TERMINAL_PROMPT=0` unless explicitly enabled, so failures surface as messages instead of hangs |
| 10 | Output parsing does not depend on user configuration | `core.quotepath=false`, `color.ui=false`, `--no-pager`, `LC_ALL=C` |

## 5. How an operation works

Every logical operation follows the same shape (see `src/ops/util.rs`):

```text
1. validate the selection against the project       (unknown ids → clear error)
2. for each configured repository, in project order:
     a. inspect it (analyzer)            → missing / not a repo / error → outcome Failed
     b. verify its identity              → mismatch → outcome Failed, command not run
     c. run the operation                → Success | Skipped | Conflict | Failed
3. aggregate into an OperationReport    → summary + details + JSON + exit code
```

Classification used everywhere in the CLI and the UI:

| Symbol | Kind | Meaning |
| --- | --- | --- |
| `✓` | success | The repository did something, or was already in the desired state |
| `-` | skipped | There was nothing to do (clean, nothing to push, no remote) |
| `!` | conflict | The operation produced or found a merge conflict |
| `✗` | failed | The operation could not be completed in this repository |

## 6. Design decisions

**Git CLI, not libgit2 or a Rust Git implementation.** Driving `git` guarantees identical
behaviour to what the user gets in a terminal — hooks, configuration, credential helpers,
`.gitignore` semantics, merge strategies — and cannot drift from Git. The cost is process
spawning per operation, which is irrelevant at project scale.

**Explicit configuration.** GitMesh refuses to guess boundaries because guessing is
irreversible from the user's point of view. Discovery only *reports*; assignment is an
explicit action (`gitmesh configure add`, or `A` in the UI), and the result is persisted
in the manifest.

**Manifest as the single source of truth.** Once configured, ownership never depends on
the filesystem again. The manifest is plain TOML so it can be reviewed, diffed and
committed with the project.

**Longest-match ownership.** Chosen over "the root owns everything except what the
external repositories own" because it also handles nesting-free multi-level paths
(`vendor/lib`) without special cases, and because it is trivially testable.

**No global commit.** Every physical repository gets a real commit with the logical
message. This keeps each repository independently cloneable, reviewable and revertible —
the property that makes hosting them separately worthwhile in the first place.

**Sequential, not parallel, operations.** A project has a handful of repositories; the
dominant cost is the user reading the result. Sequential execution keeps output ordered,
avoids concurrent index locks on shared filesystems and makes failures attributable.

**No automatic merge strategy change.** A diverged branch is reported, not merged or
rebased, unless the user asks for `--strategy merge|rebase`. This keeps surprise merges
(and their conflicts) out of a routine `gitmesh pull`.

**GitHub is a provider, not a dependency.** GitMesh works with any Git remote, including
local paths and bare repositories (which is exactly how its own tests work). Provider
code is limited to parsing URLs and exposing coordinates; no HTTP client exists, and
nothing in the local core can fail because GitHub is unreachable.

**No framework bloat.** Dependencies: `clap` (CLI), `serde` + `toml` (manifest),
`thiserror` (errors), `ratatui` + `crossterm` (terminal UI). JSON output is written by
~130 lines of code in `src/json.rs` rather than adding a serialisation dependency.

## 7. GitHub integration boundary

`src/providers/` defines a `Provider` trait (id, display name, URL matching, remote
parsing) with one implementation for GitHub. It answers questions such as "which
organisation and repository does this remote point at?" and produces the URLs GitMesh
would configure. A future API client (creating repositories, checking permissions,
opening pull requests) plugs in behind this trait, and must remain optional: local
operations must keep working with no network at all.

## 8. Known architectural limitations

* A directory cannot be split into two repositories, and repositories cannot nest inside
  one another. Validation rejects overlapping boundaries; the constraint is what makes
  ownership unambiguous.
* GitMesh does not move files between repositories. If the root repository already tracks
  files inside an external repository's directory, GitMesh reports the situation (status
  notices) rather than rewriting history; fixing it is a deliberate, manual
  `git rm -r --cached <dir>` in the root repository.
* `.gitmesh/project.toml` is a normal project file and shows up as a change until it is
  committed; GitMesh does not commit it implicitly.
* Repository-specific history operations (rebase, cherry-pick, submodule handling) are
  intentionally left to Git inside the individual repository.
