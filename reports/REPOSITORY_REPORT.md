# Repository management after creation

**Milestone report: managing the physical repositories of an existing GitMesh project.**
This document describes what was audited, what was built, how it was validated and what is
deliberately not there yet. It is written against the code in the working tree, not against
intentions.

---

## 1. What was audited, and what it found

The audit started from the committed state (`234bb60`, milestone 12 — project creation) and
looked at the real tree: `src/setup.rs`, `src/manage.rs` (absent at that point),
`src/manifest/`, `src/discovery.rs`, `src/ops/`, `src/service.rs`, `src/gui/*`,
`src/ui/app.rs`, `src/cli.rs` + `src/main.rs`, `tests/`, `tools/`, `docs/`, `reports/`.

Findings that shaped the milestone:

1. **Creation was plan-driven, management was not.** `setup.rs` already had the shape this
   milestone needs — `inspect` → typed `SetupPlan` (every step, every change, the exact
   manifest, the safety statements, a fingerprint) → `apply` (replays that plan and nothing
   else) → `verify_expecting` (re-reads the result through the normal discovery and manifest
   mechanisms). Nothing comparable existed for changing a project *after* it was created.
2. **`gitmesh configure` was the only real management path, and it bypassed all of that.**
   `cmd_configure` called `discovery::assign_repository` / `unassign_repository` /
   `rename_repository` / `set_repository_remote` and `manifest::save_project` directly, with
   no plan, no preview, no post-operation verification, and no protection beyond the checks
   inside `discovery`. The GUI had no management surface at all: `Gui` could open a project,
   run the wizard and run the unified operations, and nothing else. The terminal UI could
   assign and unassign, and nothing else.
3. **The safety rules were real but unevenly applied.** `discovery` already refused to
   shadow an existing `origin` unless explicitly asked (`ensure_origin_remote`), refused
   overlapping boundaries (`assignment_conflicts`), refused a directory that is not a
   repository root (`verify_identity`), and refused absolute/`..`/`.git` paths
   (`paths::normalize_relative`). It could **not** stop tracking files in the parent
   repository: the one implementation of that idea lived inside the setup engine, with no
   `discovery` entry point. `configure add` had no way to express it, and a user who added a
   directory the root repository was tracking got two repositories owning the same files,
   with a warning printed only after the fact.
4. **Removal had a consequence nobody could see.** `unassign_repository` is manifest-only
   and harmless — but if the root repository tracks files inside the removed directory,
   removing the entry silently hands those files back to the root repository. That is a real
   ownership change and it was announced nowhere.
5. **The GUI's JavaScript had no management vocabulary, and no Git logic to lose.** The
   client is pure presentation (`renderStatus`, `renderSettings`, `startOperation`); the
   model is built by `service::*_view_json` on the Rust side. Extending it meant adding a
   panel that posts intent and renders JSON, not re-implementing anything.
6. **`--json` and the setup wizard share the same view-model layer.** `service.rs` already
   carried `setup_plan_view_json`, `setup_result_view_json` and `validation_view_json`; a
   management flow would reuse them rather than invent a second serialisation.
7. **Regressions were not present, but the invariants were enforced in several places.**
   The same rules (no silent remote replacement, no overlapping ownership, no path escaping
   the root) were implemented twice — once in `setup.rs`, once inside `discovery` callers.
   The audit concluded that the *rules* belonged in `discovery` (one implementation) and the
   *planning* belonged in a new service that sits beside `setup`.

The audit decided the architecture before a line was written: **a second plan-driven
service for an existing project, reusing `setup`'s shape and `discovery`'s primitives, and
no new configuration system, no new manifest handling, and no Git logic in the front ends.**

---

## 2. Architecture

```text
CLI (configure …)     GUI (Repositories tab)     TUI (A / R keys)
        │                       │                        │
        └───────────┬───────────┴────────────────────────┘
                    ▼
        manage.rs   inspect → RepositoryPlan → apply → verify
                    │
        discovery.rs + manifest + paths + git   (the only code that touches Git)
```

* **`src/manage.rs`** (new, ~4600 lines including its 25 tests) is the counterpart of
  `setup.rs` for a project that already exists. It contains no Git invocation: it reads
  through `discovery`, builds the manifest with `manifest::render_manifest`, and acts
  through `discovery::*` helpers.
* **`setup.rs` was not duplicated.** The two services share `setup::SetupKind`
  (complete/partial/failed → exit 0/1/2), `setup::StepState`, `setup::ValidationReport`,
  `setup::verify_expecting` and the `*_view_json` view models. `manage` reuses
  `verify_expecting(root, runner, &expected_origins)` for its post-operation validation, so
  "does the project still open" has exactly one implementation.
* **`discovery.rs` gained the two primitives it was missing** — `untrack_from_root` and
  `count_files_tracked_under` — and `untrack_from_root` is now the single implementation
  used by both the setup wizard and repository management.
* **`service.rs` gained the view models** (`management_inspection_view_json`,
  `managed_repository_view_json`, `management_candidate_view_json`,
  `management_plan_view_json`, `management_result_view_json`) so the CLI, the GUI and any
  future front end render the same object.
* **`gui/mod.rs` gained the application-facing operations**
  (`inspect_repositories`, `inspect_repository_candidate`, `plan_management`,
  `start_management`) and `run_management`, which streams the plan's steps over the existing
  SSE channel. The GUI keeps no state about the plan: it holds a `RepositoryPlan`, runs it,
  and hands the `RepositoryManagementResult` to `service` to serialise.
* **No front end can run a plan the user did not see.** The reviewed plan id is checked
  again before execution, and a plan whose configuration no longer matches the project is
  refused with a fresh plan attached.

### The management model

| Type | Role |
| --- | --- |
| `RepositoryInspection` | read-only: every configured repository with its manifest data, its state on disk, its `origin`, whether the root tracks files inside it, issues and warnings; plus the directories that could still be added |
| `CandidateInspection` | read-only: what one directory would mean — exists / inside the root / already a repository / has commits / has an origin / number of files the root tracks inside / nested repositories found inside / suggested id / whether it can be added / the consequence in words / blockers / warnings |
| `RepositoryIntent` | `Add { path, id, remote, branch, initialize, configure_remote, untrack_from_root }`, `Remove { id, confirm_takeover }`, `Rename { id, new_id }`, `SetRemote { id, remote, configure }`, wrapped in `RepositoryManagementRequest` |
| `RepositoryChangeKind` | `AddRepository`, `AdoptRepository`, `InitializeRepository`, `RemoveRepositoryFromManifest`, `RenameRepository`, `ConfigureRemote`, `UpdateRemote`, `KeepExistingRemote`, `RecordRemote`, `ClearRemote`, `UntrackFromRoot`, `UpdateManifest` — each with a heading and a `changes_configuration` flag |
| `RepositoryActionKind` | `AdoptRepository`, `InitializeRepository`, `ConfigureRemote`, `UpdateRemote`, `UntrackFromRoot`, `UpdateManifest`, `VerifyProject` — the steps, in execution order |
| `RepositoryPlan` | the reviewed object: `changes`, `actions`, `removals`, `manifest_before`/`manifest_after`, the rendered `manifest_after` text, `safety`, `warnings`, `blockers`, `notices`, `expected_origins`, and a 16-hex FNV-1a fingerprint |
| `RepositoryManagementResult` | one outcome per action and per change, with evidence; `SetupKind`; the written manifest path; the reopened project; the validation report; the warnings; `exit_code()` |
| `RepositoryObserver` | the progress seam: `on_change` / `on_outcome`, so the CLI prints and the GUI streams, and neither is called by the service |

`plan.changes` is what changes in the configuration; `plan.actions` is what will *run*. That
split is what makes the review honest: "add 'engine' to GitMesh" is a configuration change
that may require no Git step at all, while "untrack … from the root repository" is a step
with a visible effect, and both appear as separate lines with their own symbols
(`…` planned, `–` already satisfied, `✗` blocked).

---

## 3. What each capability does

**Add a directory as a repository.** `Add` inspects the directory, refuses a path that
escapes the root, a directory that does not exist, the project root itself, a directory
already configured under another id, a directory already covered by another repository, and
a directory the root repository has committed (that last one is a *warning*, not a refusal:
the files can legitimately belong to two repositories, and the plan says so). If the
directory is not a Git repository and `initialize` is set, the plan creates one — `git init`
only ever runs for `create && !is_repository_root`. If it *is* a Git repository, the plan
**adopts** it: no `init`, no commit, no reset, no remote change.

**Ownership.** With `untrack_from_root`, the plan stops the root repository from tracking the
new directory's files (`git rm -r --cached`, which deletes nothing) so a file belongs to
exactly one repository. Without it, the plan warns that the same files would belong to two
repositories. Either way the change is a named, visible line in the review, with an
`UntrackFromRoot` action if it has to run.

**Remotes.** `SetRemote` follows the existing safety model exactly: the same URL as the
manifest already records is *already satisfied*; recording a URL and configuring it in Git
are two different things (`configure`); an existing `origin` pointing somewhere else is
**refused** unless that box is ticked, and the refusal says what to do; clearing the
recorded URL never touches Git; a repository with no remote is a perfectly valid repository.
No credential is read, stored or logged, and no provider API is called — the provider layer
is only used to *label* a remote (GitHub/GitLab/other) in the inspection view.

**Logical ids.** `Rename` changes the id in the manifest and nothing else. The directory
keeps its name, the Git repositories keep their names, the history keeps its object ids.
Duplicate ids are refused before anything is written.

**Removal.** `Remove` deletes the manifest entry, and only the manifest entry: the
directory, its `.git`, its history, its remote and its files are kept, and the plan states
that in words ("its directory, its .git, its history and its remote are kept"). If the root
repository tracks files inside it, the plan is **blocked** until the takeover is confirmed,
because those files go back to the root repository — and the warning then says how many.

**Partial failure.** Actions run in order; a failing step does not stop the others, and the
steps that depended on it are skipped. The result is `Partial` (exit `1`), every failure
carries Git's own message, the applied changes stay applied, and the validation reports the
resulting project honestly — a repository whose `git init` failed is *named* by `status`
afterwards, with what to do about it.

**Idempotency.** Adding what is already configured, removing what is already gone, setting
the remote that is already recorded, untracking a directory the root does not track and
rewriting a manifest that is already byte-identical are all reported as *nothing to do* —
`AlreadySatisfied`/`Skipped`, never a change that did not happen and never an error. Running
the same plan twice can never re-initialise, overwrite, delete or duplicate anything.

---

## 4. CLI, TUI, GUI, HTTP

**`gitmesh configure`** now goes through the service (`run_repository_intent`):
`manage::plan` → warnings on stderr → a blocked plan becomes
`Error::InvalidConfiguration(blockers)` (exit `2`) → `--dry-run` prints the same summary,
changes and steps the review shows, then "Dry run: nothing was changed." → `manage::apply`
→ `report_management`, which prints failures with Git's messages and returns
`result.exit_code()`. Two flags were added for the two consequences that only exist here:
`configure add --untrack-from-root` and `configure remove --confirm-takeover`. A repeated
command now prints the plan's own sentence ("Nothing to do: the project is already
configured as requested.") instead of claiming an addition that never happened.

**The TUI** keeps its own key-driven flow (`A` to assign, `R` to remove) on the same
`discovery` primitives — the milestone did not force a terminal redesign, and the TUI's
flows already contain their own confirmations. The one real gap the audit found was fixed:
removing a repository whose directory the root repository still tracks now logs how many
files go back to the root repository, so the consequence is stated in the terminal too.

**The GUI** gained a *Repositories* tab, in the existing visual language:

* a read-only inspection table (id, role, path, recorded remote, `origin` Git really uses,
  branch, state, whether the root also tracks files inside it, issues) and the directories
  that could still be added;
* *Check the directory* — the candidate inspection, before any button that changes anything;
* *Review the change* — the plan: every change, every step, the exact
  `.gitmesh/project.toml` that would be written, the safety statements, the warnings, the
  blockers, the fingerprint;
* *Apply the change* — only after the confirmation box, with per-step progress over the
  existing SSE stream and the evidence per change in the result; the project is re-read from
  disk when the operation ends, so every other tab shows the new layout without a restart.

**HTTP** (only these four routes were added):

| Route | Behaviour |
| --- | --- |
| `GET /api/repositories` | the inspection; `409` when no project is open |
| `POST /api/repository/inspect` | `path` (required): `400` when missing, `409` without a project |
| `POST /api/repository/plan` | `intent` = `add` / `remove` / `rename` / `set-remote`; `400` on an unknown action or a missing required field, `422` when the plan cannot be built |
| `POST /api/repository/apply` | the reviewed plan id is required (`400` otherwise), `409` when the configuration moved on (with the fresh plan), `422` for a blocked plan (with the plan), `202` with `{id, events}` for a run |

All the existing protections are inherited unchanged: loopback binding by default, `Host`
and `Origin` validation, the single-operation busy guard, the asynchronous operation model
and the SSE stream. Paths are validated **server-side** through
`paths::normalize_relative`; the browser is never trusted with the project-root boundary.

---

## 5. Safety decisions

* Nothing deletes a directory, a `.git`, a commit or a remote. The only destructive-looking
  Git command anywhere in the flow is `git rm -r --cached`, which changes the index and no
  file.
* The manifest is written last, atomically, and only through `manifest::save_project` (which
  validates before writing). A byte-identical manifest is not rewritten at all.
* An existing `origin` is never replaced silently; an existing manifest is never replaced
  silently; a directory is never adopted into a configuration by guessing — the user names
  the path.
* A plan that was not reviewed (or was reviewed against a different configuration) is
  refused, and a fresh plan is shown instead.
* The plan's own `expected_origins` names exactly the repositories whose remote the plan
  configures, so validation never turns "this repository has no remote, as configured" into
  a failure of an operation that never touched it.
* The pre-existing refusals (overlap, path escaping, non-repository directory, nested
  repository identity) are unchanged and were re-used, not re-implemented.

---

## 6. Tests

| Suite | Count | Notes |
| --- | --- | --- |
| `cargo test --lib` | **324** (0 failed) | `manage::` **25**, `gui::` **49**, `setup::` **33**, `service::` **28**, `ui::` 68 |
| `cargo test --test cli` | **15** (0 failed) | two new tests drive `configure add/remove/remote/rename` through the real binary: dry run, refusal, apply, idempotency, manifest content, Git untouched |
| `cargo test --test client` | **2** (0 failed) | the interface's own pure logic under Node |
| `cargo test --test workflows` | **14** (0 failed) | unchanged |
| `cargo fmt --all -- --check` | clean | |
| `cargo clippy --all-targets -- -D warnings` | clean | |
| `cargo build --release` | clean | |

`src/manage.rs` covers: inspection and candidate detection, adoption of an existing
repository, initialisation, duplicate ids, duplicate paths, nested repositories, overlaps,
path traversal, invalid paths, remote Add/Keep/Update/Block semantics, removal, logical id
change, manifest update, parent untracking, idempotent add, idempotent remote configuration,
idempotent removal, partial failure and the post-operation evidence. `src/service.rs` covers
the five view models against real fixtures. `src/gui/server.rs` and `src/gui/mod.rs` cover
the four endpoints, their refusals (`400`/`409`/`422`), the busy guard, the plan-id check
and the progress/result payloads. The GUI asset tests still hold the page to its rules
(every used element exists, every wired handler is defined, only local endpoints are called,
the pure client logic contains no DOM, no `window` and no `fetch`).

**No existing test was weakened.** Two CLI expectations changed *because the behaviour
changed on purpose*: a repeated `configure add` now reports "nothing to do" instead of
"Added repository …", and the test asserts exactly that.

---

## 7. Real end-to-end validation

`tools/repository-workflow.py` (ports **7414/7415**, so it can run beside the other two
scripts) drives the **release binary** and its real HTTP interface against real Git
repositories and local bare remotes. It builds `/tmp/gitmesh-repositories-e2e`: a root
repository with a first commit, an existing `renderer` repository with its own history and
its own bare remote, plain directories, an `untouched` directory, and a directory that is
deliberately made unwritable to force a failure. The 20 steps cover the whole lifecycle:

| # | Step |
| --- | --- |
| 0–1 | a real project is created (`gitmesh init`), opened through the interface |
| 2 | a candidate directory is inspected: not a repository, can be added, the root tracks 2 files inside it, "git init" spelled out; a directory the root already committed in reports the double ownership |
| 3–5 | the plan is reviewed (creation + configuration + untracking + manifest write, safety statements, the exact manifest), applying without a plan id is `400`, a stale plan id is `409` with the reason, then the confirmed plan runs with per-step progress and evidence |
| 6 | on disk: `.git` exists, the branch is the project's, the files are still there, the root stopped tracking them, the manifest lists the repository, the unrelated directory is byte-identical, the root's history did not move |
| 7 | the same request again is a no-op: "nothing to do", nothing written, no second `git init` |
| 8 | an existing repository is adopted: history, `reflog` and `origin` are proven unchanged afterwards |
| 9–10 | impossible directories are refused (missing, already owned, outside the project, the root itself, empty path), a duplicate name is blocked with the reason and still cannot be applied |
| 11 | a remote is recorded (Git untouched), then configured in Git, then replaced only with explicit intent — the silent replacement is refused |
| 12 | the new repository takes part in the ordinary workflow immediately: one unified commit gives it a real commit of its own with the same message, its own HEAD, and a push that reaches the bare remote |
| 13 | renaming changes the manifest and nothing else: the directory, the history, the CLI and the GUI all follow the new id |
| 14 | a repository is removed: the directory, `.git`, the history and the remote are verified still there, the CLI now sees it as an unassigned repository |
| 15 | a removal that hands files back to the root repository is blocked until the takeover is confirmed, then applied — the files are still there and the root owns them again |
| 16 | a deliberately failing repository produces a **partial** result with exit `1`, Git's own "Permission denied", the failing repository named, and the validation saying the project is not what the plan promised; `status` fails and names it; removing it from the configuration makes the project healthy again; a later operation on another repository still works |
| 17–20 | a second interface with no project open refuses instead of guessing; the project view exposes the same configuration; `configure list --json`, `status --json`, `status` from a nested directory, a dry run and the missing-project exit code `2` all still behave; nothing was deleted anywhere |

**Result: 179 checks passed, 0 failed.**

The two pre-existing suites were re-run against the same release binary and were not
regressed:

* `tools/gui-workflow.py` — **74 checks passed, 0 failed**;
* `tools/setup-workflow.py` — **115 checks passed, 0 failed**.

Total: **368 end-to-end checks over real repositories, real Git commands and local bare
remotes.**

---

## 8. Documentation

* **`docs/GUI.md`** — a new *Managing repositories after creation* section: the four steps
  every change goes through, the four actions, what happens when a step fails, and what the
  tab never does; the HTTP table; the safety section now that the GUI *can* change the
  configuration (but never silently); the unsupported list updated.
* **`docs/ARCHITECTURE.md`** — the layering diagram names `manage` beside `service` and
  `setup`; the rules list explains that `manage` is the `setup` counterpart for an existing
  project, that the same `discovery` primitives serve both, and that one plan drives both
  the preview and the execution; the module map gained the `manage.rs` row; the
  "GitMesh does not move files" limitation now describes the untracking it *can* do.
* **`docs/DEVELOPMENT.md`** — the test-strategy table gained a `src/manage.rs` row, and
  *Validating repository management by hand* documents `tools/repository-workflow.py`; the
  project layout lists both new files.
* **`README.md`** — the ongoing-development step in *Getting started*, the repositories
  paragraph under the interfaces, the updated `configure` command rows, the new report, and
  the status section.
* **`reports/REPOSITORY_REPORT.md`** — this document.

---

## 9. Remaining limitations

* **One intent per confirmation.** The panel and the command line plan a single change at a
  time (`RepositoryManagementRequest` is a *list* of intents, and the service handles
  several — `RepositoryManagementRequest { intents }` — but no front end offers a batch
  review yet). Adding five directories is five reviews.
* **No physical move or rename of a directory.** The milestone's rule that the physical path
  and the logical id are separate concepts is respected; moving files between directories is
  not implemented anywhere in the architecture, and remains out of scope.
* **No provider API.** A remote is a URL the user supplies. GitMesh never creates the hosted
  repository, never reads a token, and never contacts GitHub.
* **Removal is manifest-only by design.** If a user expects the directory to disappear, the
  plan says otherwise in words, in every front end — but it will not do it.
* **Conflicts are still resolved with Git.** Repository management never resolves anything:
  it reports what Git says.
* **The TUI did not gain the plan/confirm flow.** It keeps its own key-driven one, on the
  same primitives; only the removal consequence was added there.
* **`git add -A` inside a project that contains repositories without commits is refused by
  Git itself** ("'engine/' does not have a commit checked out"). This is Git's own behaviour,
  not GitMesh's, and it can surprise a user who runs Git by hand in the project root; it is
  documented here because the E2E script had to work around it.

## 10. Technical debt

* `src/manage.rs` is large (≈4600 lines, 25 tests). The split between `plan_*` functions per
  intent is clear, but `plan()` itself is a long dispatcher; a future refactor could move
  each intent's planner into its own module without changing the public types.
* `src/gui/server.rs` (≈1880 lines) now holds the setup wizard, the management panel and the
  operation endpoints in one file. The routing table and the form helpers are shared, so
  splitting is a matter of taste rather than of correctness.
* A dead element (`setup-root-remote-select`) is still read by the wizard's script but
  never rendered. Harmless, and stored here so it is not mistaken for a bug.
* `docs/GUI.md` duplicates part of what `reports/REPOSITORY_REPORT.md` says. The manual is
  the source of truth for behaviour; the report is a snapshot of this milestone.

## 11. Intentionally not implemented

* Diff, history/log views, per-file staging and operation cancellation — out of scope for
  this milestone, as instructed.
* Creating a hosted repository (GitHub/GitLab API), tokens, OAuth, accounts, telemetry —
  permanently out of scope.
* Moving, splitting or merging physical repositories, and rewriting history.
* Automatic conflict resolution.
* A GUI editor for anything other than the repository set (project name, root branch, …):
  those stay in `gitmesh init` / the manifest, exactly as before.

---

## 12. Final validation

```console
$ cargo fmt --all -- --check                 # clean
$ cargo clippy --all-targets -- -D warnings  # clean
$ cargo test                                 # 324 lib / 15 cli / 2 client / 14 workflows, 0 failed
$ cargo build --release                      # clean
$ ./tools/repository-workflow.py             # 179 checks passed, 0 failed   (release binary)
$ ./tools/gui-workflow.py                    #  74 checks passed, 0 failed   (release binary)
$ ./tools/setup-workflow.py                  # 115 checks passed, 0 failed   (release binary)
```

The working tree contains only this milestone's changes: `src/manage.rs`,
`tools/repository-workflow.py`, and modifications to `src/{cli,discovery,lib,main,service,setup}.rs`,
`src/gui/{asset,mod,server}.rs` + `static/{app.css,app.js,client.test.js,index.html}`,
`src/ui/app.rs`, `tests/cli.rs`, `README.md`, `docs/{ARCHITECTURE,DEVELOPMENT,GUI}.md`. No
debug code, no `TODO`/`FIXME`, no temporary file, no credential and no generated artifact is
included; file modes were restored so no unrelated mode change is part of the diff.

## 13. Readiness

The development lifecycle this milestone set out to support works end to end, on the
release binary, through the interface and the command line, against real Git:

> create a project → work normally → create a new subdirectory → turn it into a managed
> repository through the GUI (inspect → plan → review → confirm → apply → verify) → keep
> developing → adopt another existing repository later → configure or change its remote →
> rename its logical identity → remove a repository from GitMesh without deleting it →
> keep using the unified GitMesh operations.

**This milestone is complete and ready for validation / increment.** The specific
capabilities requested — add, adopt, initialise, configure/change a remote, change a logical
id, remove from the configuration without deleting anything, review the configuration and
state, detect conflicts and unsafe configurations before applying, reopen/refresh after a
change, and continue with the normal GitMesh operations immediately — are implemented,
tested at the service, HTTP, GUI-asset and process level, and validated end to end.
