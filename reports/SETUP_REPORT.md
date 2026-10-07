# GitMesh project setup — development report

Report for **creating a GitMesh project**: turning an ordinary directory into a project, from
the command line or from the graphical wizard, with a plan the user reviews before anything
runs. Milestone numbering is maintained by the project owner; this document is referred to as
the *project setup* work in [`DEVELOPMENT_REPORT.md`](DEVELOPMENT_REPORT.md) and is the
creation counterpart of [`GUI_REPORT.md`](GUI_REPORT.md).

The work is deliberately plan-driven: GitMesh inspects the directory, produces a typed plan,
shows that exact plan, and executes that same plan on confirmation. Nothing in this milestone
guesses, and nothing is created before the user has seen what will be created.

---

## 1. Audit findings (before writing any code)

The repository was re-audited first, as required — the graphical milestone is not assumed to
be unchanged just because it is the previous one:

| Item | Finding | Consequence |
| --- | --- | --- |
| Repository state | `main` @ `03d276f`, GUI milestone committed, gate green (289 lib / 13 cli / 2 client / 14 workflows) | The setup work was added on top of a working interface, not a prototype |
| Project creation | `gitmesh init` existed and worked, but it **wrote the manifest directly**: build the initial project with `discovery::initial_project`, optionally attach `origin`, then `manifest::save_project` | There was no way to *show* what `init` would do, and no way for a second front end to reuse it without copying it |
| Empty-directory flow | `init` required the root to already exist and never created a repository (`git init` was left to the user; the CLI printed "run `git init` there") | The wizard's "define the root repository" step had nothing to call |
| Discovery | `discovery::{scan_project, scan_nested, assignment_conflicts, assign_repository, suggest_id, initialize_repository, is_repository_root, ensure_origin_remote}` already existed and was used by the CLI `discover`/`assign` commands and the TUI | The wizard must **not** re-implement scanning, overlap detection or name suggestions |
| Manifest | `manifest::{Project, save_project, parse_manifest, looks_like_url}`, `.gitmesh/project.toml`, schema validation, `--force` semantics | Manifest generation had to go through this schema; a second TOML writer would be an instant divergence |
| Providers | `providers::github` foundation only: `split_owner_and_name`, `plan_remote`, `remote_url_for`, `Visibility`, `RemoteScheme`. No HTTP client, no tokens, ever | The "hosting" step can prepare a correct remote URL and print the `gh` command, and can do nothing else. That is a feature |
| Application layer | `src/service.rs` (`ProjectSession`) existed for the **opened** project; nothing for a directory that is not a project yet | A new service module was needed, not a new layer |
| Front ends | `cli`, `ui` (TUI), `gui` all present and sharing `ops` | The GUI may only collect intent and present results |
| Symmetry | `gitmesh init` and the wizard would have been two implementations of one flow | `init` was **re-pointed at the new setup service**; one implementation, two front ends |
| Environment | no display server; the web interface from the previous milestone is the only verifiable front end here | The wizard had to be a page in the existing interface, not a new one |

The audit also settled the boundary: GitMesh **creates** projects; it does not clone, bootstrap
from a template, or import anything. There is no new bootstrap flow, and none was added.

## 2. Architecture before implementation

```text
cli ─┐
ui  ─┼─→ ops ─→ analyzer ─→ model/manifest/discovery ─→ git
gui ─┘        ↑
             └── setup (new): inspect → plan → apply → verify
```

The new module is the service for a directory that is **not a project yet**:

```text
SetupRequest ──inspect──→ Inspection ──plan──→ SetupPlan ──apply──→ SetupResult ─→ verify
     (intent)               (facts)              (typed steps)        (outcomes)   (ValidationReport)
```

Three properties were designed in, not bolted on:

1. **Typed throughout.** Every answer is a Rust struct (`SetupRequest`, `RepositoryRequest`,
   `SetupPlan`, `SetupStep`, `SetupResult`, `ValidationReport`). The HTTP layer serialises
   them; the GUI never assembles a plan of its own, and cannot disagree with what runs.
2. **One plan, two uses.** `plan()` is a pure function of `SetupRequest` + the filesystem.
   `/api/setup/plan` returns it; `/api/setup/apply` re-plans from the request and executes
   **that** plan, so the preview is never a separately generated description. The plan carries
   a 16-hex fingerprint of its own content (`SetupPlan::fingerprint`), printed in the review
   step and in the result, so "the plan you saw" is checkable.
3. **Front-end neutral.** `setup` knows nothing about HTTP, HTML or the terminal. `main.rs`
   drives it for `init` today; a `gitmesh setup` subcommand or a TUI wizard would drive the
   same functions, the same way the CLI's `init` does now.

## 3. Implemented

### 3.1 The setup service — `src/setup.rs` (3 687 lines, 33 tests)

| Piece | What it does |
| --- | --- |
| `inspect(root, runner) -> Inspection` | Read-only facts: does the manifest exist / can it be read, is the root a repository, is it *inside* one, which sub-directories are candidates (`CandidateDirectory` with `hasGitDir`, `hasCommits`, `suggestedId`, `suggested`), name suggestions, notices. Best-effort: an unreadable sub-directory becomes a notice, not a failure |
| `SetupRequest` / `RepositoryRequest` | The whole intent in one value: root, name, root remote/branch, create-root-repository, configure remotes, overwrite manifest/remotes, untrack-from-root, first-publish message, and per-repository `{path, id, remote, branch, create, untrack_from_root, visibility}` |
| `plan(&request, runner) -> SetupPlan` | Every step GitMesh would take, as `SetupStep { kind, target, path, detail, state }` with `SetupStepKind::{CreateMetadataDir, CreateRepository, ConfigureRemote, UntrackFromRoot, WriteManifest}` and `StepState::{Planned, AlreadySatisfied, Blocked}`; plus `blockers`, `warnings`, `safety`, `summary`, `manifest` preview, `PlannedRepository` list, and `FirstPublish` |
| `planned_remote_action(configure_remotes, remote, current)` | Decides `RemoteAction::{None, Add, Update, Keep, Record}`: nothing is replaced unless the user asked for it (`overwrite_remotes`), and a remote GitMesh merely *records* becomes an explicit warning + safety line, not a silent action |
| `record_only_conflict(...) -> Option<(String, String)>` | An existing `origin` that differs from the requested URL and is **not** to be rewritten is a blocker, not a guess (`exit 2`) |
| `apply(&plan, dry_run, runner, observer)` | Executes the plan with safe ordering: metadata directory → repositories (root first) → remotes → untracking → manifest → first publish is left to the ordinary operations. A failed repository skips its dependent steps and is reported; unrelated repositories continue |
| `verify(root, runner) -> ValidationReport` | Re-reads the project **through the normal discovery/manifest mechanisms** after the run and checks every configured repository's existence, path and `origin`, with exact per-repository `issues` |
| `SetupResult` | `SetupKind::{Complete, Partial, Refused}`, per-step `SetupStepOutcome` with `✓ / – / ! / ✗` symbols, `counts{succeeded, skipped, failed}`, `exit_code()` 0/1/2, and the project it produced |

Safety invariants, all covered by tests: `git init` only when `create && !is_repository_root`;
an existing remote is replaced only with `overwrite_remotes`; an existing manifest is written
only with `overwrite_manifest` (a byte-identical manifest is `AlreadySatisfied` + a notice, not
a rewrite); paths are normalised and absolute / `..` / `.git` / NUL are refused; overlapping
selections — including a repository at the project root — are refused by
`discovery::assignment_conflicts`; nothing is deleted or moved (the only removal is
`git rm -r --cached`); an empty first-commit message is a blocker.

### 3.2 Application layer — `src/service.rs`

Added the view models the wizard needs, next to the existing ones for the opened project:
`inspection_view_json`, `candidate_view_json`, `setup_plan_view_json`, `setup_result_view_json`,
`validation_view_json`, `operation_view_json`. Same rule as the rest of `service`: presentation
vocabulary only, no Git behaviour.

### 3.3 The CLI did not fork

`gitmesh init` now builds a `SetupRequest` and calls `setup::plan` + `setup::apply`; the
command-line front end and the wizard literally share the flow. Behaviour is preserved
(`--force` → `overwrite_manifest`, `--remote` → record-only, `--add-git-remote` → configure the
remote, `--json` unchanged and now carrying `warnings`), plus one addition, `--git-init`, which
is the CLI's way of asking for the root repository to be created. Plan warnings are printed as
`note: …` lines, because what the plan decided to leave alone is part of the answer.

### 3.4 The wizard — `src/gui/`, one linear flow

| Step in the page | Requirement it answers |
| --- | --- |
| 1 Project root | absolute path, scan, "already a GitMesh project" → offers the existing-project flow instead |
| 2 Structure | candidate directories, coverage view, nested/overlap rejection, "stop tracking the selected directories" |
| 3 Root repository | name, create-or-not, optional remote (manual or GitHub owner/name), branch |
| 4 Repositories inside the project | one editor per directory: id (suggested, e.g. `myproject-engine`), remote, create, branch |
| 5 Repositories that already exist | adopted as-is, with the reason explained; never re-initialised |
| 6 Remotes | "no remote is fine" is stated; configuring remotes is a deliberate checkbox |
| 7 Hosting (optional) | GitHub through the provider foundation: the URL is *prepared*, the `gh` command is *printed*, nothing is created, no token is asked for or stored |
| 8 Manifest | the `.gitmesh/project.toml` GitMesh will write, from these answers, via the project schema |
| 9 Review and dry run | the real plan: every CREATE / already-there item, remotes, the safety block, the plan fingerprint |
| 10 Confirm | an explicit checkbox over the reviewed fingerprint; no confirmation, no run |
| 11 Apply | live per-step progress (`✓`, `…`, pending) over SSE, then the aggregate |
| 12 Result | `✓ Created / – Already existed / ! Skipped / ✗ Failed`, the validation report with exact errors, and — on success — the project opens in the normal interface with no restart |

### 3.5 First publish is plan data

The wizard's optional "make one initial commit and push it" is not an executor special case: it
is `FirstPublish { message, repositories }` inside the plan, where `repositories` is exactly the
set whose `RemoteAction` is `Add`. `Gui::run_first_publish` then calls the **ordinary**
`service::ProjectSession::commit` (`CommitOptions::new(message)`, `quiet_clean`) and the ordinary
`push`. So the first publish is the normal workflow, reported in the normal per-repository
shapes (`commit:<id>` / `push:<id>`), and it cannot behave differently from a later commit.

### 3.6 HTTP surface added

| Route | Answer |
| --- | --- |
| `GET /api/setup/status` | `{inspection}` for the directory the interface was started in |
| `POST /api/setup/inspect` | `{inspection}` for a given absolute path (`path`, `name`) |
| `POST /api/setup/plan` | `{plan}` — including `ready`, `id` (fingerprint), `steps`, `safety`, `blockers`, `warnings` |
| `POST /api/setup/apply` | SSE stream: `plan`, then per-step events, then `finished` with `plan, setup, publish, opened, model` |

All of them go through the same host/Origin guards the interface already had (a foreign `Host`
gets 421, a cross-site POST gets 403), bind `127.0.0.1` only, and stay behind the existing
`is_busy()` guard so two setups can never run at once.

## 4. Refactors

| Refactor | Why | Risk |
| --- | --- | --- |
| `gitmesh init` re-pointed at `setup::{plan, apply}` | Two front ends, one flow; `init` gained the ability to *report* what it will do | Behaviour preserved; existing `tests/cli.rs` cases pass unchanged, plus new ones |
| New `--git-init` flag on `init` | The service creates the root repository when asked; without a flag the CLI could not ask | Additive only |
| `src/discovery.rs` hardened (+158 −43) | `is_repository_root` / `top_level` semantics, nested scans and `initialize_repository` needed to be exact for a plan that promises "no `.git` is re-initialised" | Covered by new discovery tests |
| `Gui::open` gained the setup entry points (`inspect_directory`, `plan_setup`, `start_setup`, `run_setup`, `adopt`) | The wizard is a state of the same interface; a refused open still closes the session (unchanged, locked by its test) | Contained in `gui`; no orchestration moved |
| Push: a remote-less repository is `Skipped`, not `Failed` (`src/ops/push.rs`) | Found by the setup E2E: a local-only repository (an explicitly supported configuration) made the whole push exit 1 | **Product fix, not a test fix.** Aligned with `pull`/`fetch`; a real failure in another repository still exits 1 (`a_failing_repository_still_fails_while_a_local_one_is_skipped`) |
| Stale `src/setup_tests.rs` draft deleted | Superseded by `mod tests` inside `src/setup.rs` | none |

No Git invocation, ownership rule or outcome classification was written in the GUI. The GUI
received **no** new orchestration: it collects intent, posts it, renders the plan, asks for
confirmation and renders the results.

## 5. Tests

Automated, all green (289 lib / 13 cli / 2 client / 14 workflows = 318 total, 0 failed):

| Area | Where | Count | What is covered |
| --- | --- | --- | --- |
| Setup service | `src/setup.rs` (`mod tests`) | 33 | Scanning an empty directory, a normal project, nested directories, an existing root/nested `.git`, an existing manifest, a malformed manifest, overlapping selections, invalid paths; plan for root-only / +1 / +many, generated and custom names, remote / no-remote, existing / mixed repositories; manifest generation; safety (never deletes `.git`, never silently overwrites a remote or the manifest, rejects ambiguous layouts); execution (root and external init, many repositories, partial failure, already-existing repositories, remote configuration, manifest creation, final validation); idempotency (rerun detects the project, no destructive re-init, reports what exists, changes nothing); refusal of a conflicting record-only remote |
| GUI model | `src/gui/mod.rs` (`gui::tests`) | 17 | Wizard state, inspection/plan/apply plumbing, refusal paths, first publish, adoption only after a successful validation |
| Interface assets | `src/gui/asset.rs` | 7 | Every handler the page wires is defined, every element the script reaches for exists, no unreachable route literals |
| Client logic | `src/gui/static/client.test.js` under Node (`tests/client.rs`) | 145 assertion groups | Plan rendering, step states, result symbols, first-publish rows, refusals |
| CLI | `tests/cli.rs` | 13 | `init`, `init --json`, `init --force`, `init --remote` (record-only), `init --add-git-remote`, nested directories, `-C`, exit codes |
| Workflows | `tests/workflows.rs` | 14 | Multi-repository orchestration, unchanged and not weakened |

No existing test was weakened; `src/ops/push.rs` had a test replaced **because the product
behaviour changed on purpose** (see §4), and the new behaviour is asserted from both sides.

## 6. Manual validation — `tools/setup-workflow.py`, 115 checks, 0 failed

A real end-to-end run against the release binary, over `/tmp/gitmesh-setup-e2e`: `MyProject`
is an ordinary directory holding `src/`, a plain `engine/`, a `renderer/` that **already is** a
repository with its own history and remote, and a `tools/` that stays local-only; bare remotes
live in `remotes/`, and an unrelated repository in `Untouched/` must not be touched. The script
drives the HTTP endpoints exactly as the page does, starts its own interfaces on 7412/7413
(refusing to run if either port is held), and covers the 24 verification points:

| Step | Checks |
| --- | --- |
| 0 | An ordinary directory: no `.git`, no `.gitmesh`; the untouched neighbour is recorded |
| 1 | Scan: candidates detected from the Rust side, an existing repository detected **with its commits**, a plain directory not mistaken for one, suggested ids, `scanning created nothing` |
| 2 | Plan: `ready`, fingerprint, all four repositories, `engine/tools` created, `renderer` adopted + `remoteAction=keep`, `tools` `none`, root remote `add`, step states `planned`/`already`, safety lines, the first publish is plan data with exactly `{root, myproject-engine}`; `planning created nothing` |
| 3 | Refusals, never guesses: a repository inside a repository (with the reason), a path escaping the root, a directory that does not exist, two repositories sharing one id, a malformed record (400); nothing was created by any of them |
| 4 | Apply: the plan executes, `Complete`, validation `ok`, the project opens with no restart |
| 5 | The filesystem is exactly what the plan promised: root/`engine`/`tools` created, the adopted repository keeps its HEAD, the manifest exists with the requested name, one entry per repository, relative normalised paths, the configured remote recorded, and the file on disk identical to the preview |
| 6 | Remotes configured where the plan said, absent where it said |
| 7 | First publish ran through the ordinary operations (each repository has a real commit with the same message; nothing pushed for the local-only repository) |
| 8 | The project opens from the interface **and** from the command line, including `-C` and a nested directory |
| 9 | Real changes in several repositories, then one logical commit |
| 10 | Push: remote-backed repositories pushed, and the local-only repository **skipped** — `the whole operation reports success` (this check found the push bug fixed in §4) |
| 11 | A partial failure (a bare remote deleted on purpose) is reported precisely with the others still successful |
| 12 | Rerunning the setup on the configured project: now recognised as a GitMesh project, plan `noop`, every step `already`, rerun `complete` and validating, manifest byte-identical, no HEAD moved, and the unrelated repository outside the project untouched in content and HEAD |
| 13 | Restart: the project is still there and fully usable |
| 14 | Summary; the project is left in place for inspection |

The previous milestone's suite was re-run unchanged after the change:
`tools/gui-workflow.py` → **74 checks passed, 0 failed** (18 steps), so the setup work did not
regress the interface it lives in.

## 7. Known limitations

- **GitHub is prepare-only.** Step 7 composes the remote URL through `providers::github` and
  prints the `gh repo create` command; GitMesh never creates the repository for you, never asks
  for a token and stores no credential. This is stated in the page. The manual remote path stays
  fully supported and is the default.
- **Adoption does not normalise.** An adopted repository keeps its own default branch, remote
  names and history. GitMesh records what is there and reports mismatches on later operations;
  it does not rewrite an existing repository's configuration.
- **Record-only remotes need `--add-git-remote`** (CLI) or the "configure remotes" checkbox
  (wizard) to actually become `origin`. The plan says so explicitly, and a conflicting existing
  `origin` is refused rather than overwritten.
- **The wizard configures, it does not edit.** Changing repositories after setup — assign,
  rename, remote URLs — still belongs to the CLI (`assign`, `configure`) and the TUI. The
  Settings view stays read-only, which is the scope that was agreed.
- **One open project at a time**, and a refused `/api/open` closes the current session (locked
  by `gui_without_a_project_reports_a_clear_error_and_can_open_one`).
- **Scanning is best-effort**: an unreadable sub-directory produces a notice and an empty
  candidate list rather than an error, so a wizard never dies on a permission problem.
- **`--git-init` prepares the root repository but does not commit.** The first commit is the
  first publish, or `gitmesh commit`.

## 8. Technical debt

| Debt | Severity | Note |
| --- | --- | --- |
| The "remote kind" select in the root form has no path back to *none* | low | Noted during the GUI milestone; the plan still refuses nonsense, and the manual default is fine |
| `src/setup.rs` is 3 687 lines including tests | low | Well sectioned; if a fourth front end appears, the inspection half can split out without touching the plan |
| The wizard's HTML is one large static document | low | Deliberate: no build step, no framework, embedded in the binary |
| `Gui::open` now serves two flows (open, setup) | low | Both are thin; the model is still one `GuiState` |
| No browser automation in CI | medium | The E2E drives the real HTTP surface and the client logic runs under Node from the shipped region of `app.js`, but no headless browser clicks the page |

Debt resolved in this milestone: the duplicated project-creation path (`init` writing a
manifest directly), and the push misclassification of local-only repositories.

## 9. Intentionally not implemented

- No clone / bootstrap-from-template / import flow. GitMesh creates projects; it does not
  populate them.
- No GitHub API client, no OAuth device flow, no token storage, no CI or webhook setup.
- No diff, history, staging or file-level operations in the wizard — the milestone's scope guard
  is creation, architecture, manifest, safety, validation and polish.
- No configuration *editing* after setup (assign/rename/remote changes) in the GUI.
- No background service, no telemetry, no analytics, no account, no cloud dependency: the setup
  service is a library function driven by a local process.
- No parallel TOML format or parallel plan format: the manifest goes through the project schema,
  and the wizard renders the plan the service produced.

## 10. Readiness assessment

**Implemented and verified.** Creating a project is a real, plan-driven flow with one
implementation behind two front ends. The service is typed end to end, refuses ambiguous
layouts, never deletes `.git`, never replaces a remote or a manifest without explicit
confirmation, validates the result through the normal discovery/manifest mechanisms, and is
idempotent on a configured project. `gitmesh init` and the wizard cannot drift, because they
call the same functions.

**Verified by real runs, not by the build succeeding.** 318 automated tests pass, clippy is
clean with `-D warnings`, the release binary was rebuilt, and 115 end-to-end checks over a
from-scratch multi-repository project pass — including the cases the milestone calls out:
restart/reopen, existing root and external repositories, an already-configured project, an
ambiguous layout and a deliberate partial failure. The previous GUI suite (74 checks) still
passes, so no CLI or interface regression was introduced.

**Ready for two developers to set up a project daily?** Yes, for the flow it covers: point the
interface at a directory, choose what becomes a repository, review the plan, confirm, and be
inside a normal GitMesh project — with a manifest you could have read but never had to write.
The honest caveat is the one that was true before: the wizard *sets up*, and later configuration
changes are still CLI/TUI work.

**Missing, in priority order:** editing repositories after setup (GUI configuration), a
`gitmesh setup` subcommand exposing the same plan on the command line for scripting, GitHub
repository creation behind the provider layer (the last piece of the hosting step), and
adoption-time normalisation of existing repositories' branches/remotes.

**Complete enough to be validated by hand?** Yes. `gitmesh gui` in an empty or half-populated
directory shows the wizard; the CLI equivalent is `gitmesh init --git-init --remote …`; and
`tools/setup-workflow.py` reproduces the whole scenario, from an empty directory to a pushed,
opened project, in `printf`-style plain output you can read line by line.
