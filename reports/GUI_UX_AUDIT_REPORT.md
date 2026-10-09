# GUI and UX audit — findings and improvements

> Status: **implemented and validated; not committed.** Section 1 (the audit) was written
> before any code changed. Sections 7–12 record what was implemented and what was measured.

## 1. Starting point

* Repository: `/home/user/gitmesh`, branch `main`, HEAD `b4198d8`
  (`manage: change a project's repositories after creation, on one reviewed plan`).
* Working tree at audit time: clean except five files whose **file mode** had been reset by
  the sandbox (`100755` in the index, `100644` on disk; content identical). They were restored
  with `chmod +x`, which makes `git status` clean again. No `.gitignore` change was needed:
  `target/`, `.gitmesh/` and editor noise are already ignored, and nothing ignored exists.
* Baseline before any change (all green on HEAD): `cargo fmt --check` clean;
  `cargo clippy --all-targets -D warnings` clean; `cargo test` **324 lib / 15 cli / 2 client /
  14 workflows, 0 failed**; `cargo build --release` clean; `tools/gui-workflow.py` **74/0**;
  `tools/setup-workflow.py` **115/0**; `tools/repository-workflow.py` **179/0**.

## 2. Audit methodology

1. Read the reports (`REPOSITORY_REPORT.md`, `GUI_REPORT.md`, `SETUP_REPORT.md`) and compared
   each claim with the code.
2. Read the whole browser surface: `index.html` (every panel and control), `app.js` (the pure
   `clientLogic` region and the DOM wiring), `app.css`.
3. Traced each screen to its endpoint and to the service function behind it
   (`editor::project_model` → `service::*_view_json`; `GuiOperation` → `dispatch` → `session.*`).
4. Exercised the **release binary** against scratch projects with real Git repositories:
   CLI-versus-GUI consistency, a live merge conflict, a missing directory, and a commit issued
   while a merge was in progress.
5. Checked the test suite for coverage of each suspected defect.

## 3. Verified findings (by severity)

Finding IDs used in the implementation and the commit-level notes: C1 = **F0**, H1 = **F13**,
H2 = **F1/F2**, H3 = **F8**, H4 = **F16**, M1/M2 = **F6**, M3/M4 = **F7**, L1 = **F14**,
L2/L3 = **F15**, L4 = management failure text (fixed with F1/F2). Two defects were found
while validating (**N1**, **N2** below) and are listed in their own section.


### Critical

**C1. In-progress Git operations are never detected outside the process directory.**
`GitRepo::operation_in_progress` runs `git rev-parse --git-path MERGE_HEAD` *in the repository*,
which prints a path relative to that repository (`.git/MERGE_HEAD`), then calls
`Path::new(&path).exists()` **from the GitMesh process directory**. For any repository that is
not the process directory, the check is always false. Consequences, reproduced:
* `gitmesh status` reported `tools … 1 conflicted` and never said that a merge was in progress;
* the guards in `ops/{commit,sync,push,branch}.rs` ("finish or abort it before running GitMesh
  operations") were inert;
* after the conflict was resolved by hand, **`gitmesh commit -m "unified commit during merge"`
  completed the merge in `tools` with the message typed for the whole project**. Git history
  changed without the user asking for it.
No test covered in-progress detection (`grep` finds no test for `MERGE_HEAD`).

### High

**H1. "Refresh" never re-reads the manifest.** `ProjectSession` caches the manifest at open
time, and `/api/refresh` returns the model of the same session. A repository added with
`gitmesh configure add tools` in a terminal was still absent from the GUI after Refresh and
from the Repositories tab (reproduced). The button's tooltip ("Re-read the project") and the
docs ("Refresh … re-reads everything") are therefore false. The session's own doc comment
claims "reopen and refresh are the same operation", which is not true for the manifest.
Side effect: a plan made in the GUI is built from the stale view and is then refused by
`manage::apply` when the manifest on disk differs. The refusal is safe, but it is a dead end.

**H2. A failed operation can be presented as "completed".** On `Err` the server emits
`failed` and never emits `finished`. The client's fold (`progressRows`) only converts
`running` rows to `skipped` when `finished` arrives, so after a `failed` event rows stay
`running` ("…", "working…"), and `resultTitle` counts only conflict/failed rows as problems.
Result: `progressRows([started, repository(a), failed])` gives title **"Commit completed"** with
a red error line under it. The existing test only checks that `failed.failed === 'boom'`.
The same fold labels a row that never reported as **"nothing to do"** after `finished`, which
hides a missing result.

**H3. A refused plan is a dead end in the Repositories tab.** When `/api/repository/apply`
returns 409 (configuration changed) or 422 (blocked), the body carries the fresh plan.
`startOperation` ignores it: the error becomes "Operation refused", the reviewed plan stays on
screen, and **Apply stays enabled**, so every retry is refused the same way.

**H4. The TUI removes a repository without a plan or a consent step.** In the project screen,
`a` on an external repository calls `discovery::unassign_repository` and saves the manifest
directly. If the root repository tracks files inside it, those files go back to the root, so
the next unified commit stages them in the root repository. The CLI and the GUI both require
explicit consent (`--confirm-takeover`, the checkbox) for that case. The previous report said
the TUI "already contains its own confirmations" — **that was wrong**.

### Medium

**M1. Operation-in-progress is in the model but never rendered.** `operationInProgress` is in
every repository view (`service::repository_view_json`), and `app.js` never reads it (grep:
0 matches). The overview cannot say that a repository is in the middle of a merge.

**M2. Overview does not separate local-only repositories or show ownership overlap.** The
Status table has no remote column. The Repositories table shows "local only" but no change
count, clean/dirty flag, or "the root repository also tracks N file(s) here" — all of which the
inspection already provides (`clean`, `changes`, `ahead`, `trackedByRoot`).

**M3. Connection loss during an operation is silent.** `source.onerror` clears
`activeOperation` and refreshes, with no message. The operation may still be running on the
server; the next action is then refused with "still running" and the user is not told why.

**M4. The setup wizard opens the project after a fixed 400 ms delay** (`setTimeout`), instead of
waiting for the open request. On a slow disk the wizard closes early or not at all.

**M5. TUI key hints do not match the key bindings.** The footer and help advertise `A`, `C`,
`D`, `F`, `N`, `B`, `M`, `R`, `S` (uppercase) and `P: pull / Shift-P: push`, but the handler
binds lowercase letters and `p` = pull, `P` = push. Shift-letter presses are ignored.

### Low

**L1. Accessibility.** No `:focus-visible` style (keyboard focus depends on the browser default
on a dark background). Tabs have `role="tab"` but no `aria-selected`, so the active tab is
not announced. Wide tables overflow the page on narrow screens (no overflow container).

**L2. The operation panel is at the bottom of the workspace**, below the whole panel stack.
Starting a commit from the Commit tab can leave its progress off-screen.

**L3. Keyboard `d` toggles dry-run silently** (outside inputs). Turning dry-run *off* is the
risky direction; the status line does not say so.

**L4. Hard-coded text.** The management route reports `git is not available` in its result
even when the event carries the real runner error. (Fixed: both routes now report the runner
error as an `{message, configuration}` object.)

**L5. Documentation errors in the previous report.** `REPOSITORY_REPORT.md` states that the
wizard still reads a dead element `setup-root-remote-select` — **it does not** (no reference
exists in `src/`). It also describes the TUI keys as `A`/`R`; the real key is `a`, and `r`
reloads.

## 4. What was verified as correct (no change needed)

* Unavailable repositories carry their error and are never shown as clean (a deleted directory
  shows "directory does not exist" in the details).
* Local-only repositories are labelled "local only" in the Repositories tab.
* Setup and repository-management refusals are shown with their blockers; stale plan ids in
  the setup wizard are handled (`error.data.plan` is used there).
* The conflict guidance never claims to resolve anything.
* Request-origin, host and busy protections are unchanged by this audit (not weakened).

## 5. Capability trace (screen → endpoint → service)

| Screen / action | Endpoint(s) | Service |
| --- | --- | --- |
| Open project (welcome, dialog) | `POST /api/open`, `GET /api/model` | `Gui::open` → `ProjectSession::open` |
| Refresh / 15-second refresh | `POST /api/refresh` | returns cached session model (**H1**) |
| Status, tree, changes, branches, sync, settings | from `GET /api/model` | `editor::project_model` → `service::*_view_json` |
| Commit | `POST /api/commit` → `GET /api/events/{id}` | `dispatch` → `session.commit_observed` |
| Branch | `POST /api/branch` | `dispatch` → `session.branch_observed` |
| Pull / fetch / push | `POST /api/sync`, `POST /api/push` | `session.sync_observed`, `session.push_observed` |
| Setup wizard | `/api/setup/{inspect,plan,apply}` | `setup::{inspect,plan,apply}` |
| Repositories tab | `GET /api/repositories`, `POST /api/repository/{inspect,plan,apply}` | `manage::{inspect,inspect_candidate,plan,apply}` |
| Dry-run | `POST /api/dry-run` | `Gui::set_dry_run` |
| Project state for the TUI | (not served) | `ui::app` → `discovery`/`manage` directly (**H4**) |

Operations that still need the terminal: resolving conflicts (by design, with Git), aborting a
merge, inspecting diffs/history (no such views exist), and the setup/configuration commands the
GUI also offers (kept in the CLI for scripting). Each is either by design or listed in §8 as
deferred.

### Found during validation (not in the first draft)

**N1 (High). The setup wizard never drew its per-directory editors in the browser.** `app.js`
declared `renderRepositories` twice in the same closure: once for the wizard and once for the
Repositories panel. In JavaScript the later declaration replaces the earlier one, so
`renderWizard()` drew the Repositories panel instead. The wizard's `#setup-repositories`
stayed empty: no name, create or untrack controls for selected directories. The Rust tests
and the Node client tests did not run the DOM, so they missed it. Reproduced and confirmed
fixed in a real browser (see §9).

**N2 (Regression risk, fixed before merge). Conflicts were reported as "merge in progress".**
Once `operation_in_progress` read the repository's own Git directory (F0), an unresolved
conflict was reported first as an in-progress merge (`failed`) instead of as a conflict.
Three existing tests (`conflicts_are_reported…`, `conflict_prevents_commit…`,
`pull_conflict_is_reported…`) caught it. The fix orders the guards: unresolved conflicts are
reported first in commit and pull; a merge with no conflicts is refused as in progress.

**N3. A management apply that fails part-way left the interface on the old configuration.**
`run_management` reloaded the session only when the whole apply succeeded, although some
actions write the manifest before a later one fails. It now reloads after every real apply.

**N4. A timed-out or closed operation stream left the interface locked on "running".** The
server closes long streams with `closed {reason: timeout}`; the client only closed the
`EventSource` and kept `activeOperation`, so the busy state stayed until a reload. Connection
loss and closed streams now end the operation with a message and a refresh (F7).

**N5. A GUI workflow step relied on the F0 bug.** `tools/gui-workflow.py` step 12 resolved a
merge by calling `gitmesh commit`, which only completed the merge because of C1. The step now
concludes the merge with `git commit` (its title already says "with Git"), and keeps every
previous check.

## 6. Prioritized plan

Implemented in this order, each with tests: **C1** (Git detection, with a regression test that
runs from a different process directory), **H1** (refresh reloads the session, with tests),
**H2** (truthful failed/unreported statuses, client tests), **H3** (refused plan replaces the
review, client-side logic), **H4** (TUI unassign/assign through the shared management service
with the same consent rule), **M1/M2** (overview: operation in progress, remote, change and
overlap columns), **M3/M4** (connection loss message, awaited open), **M5** (TUI key hints),
**L1–L4** (focus, tabs, layout, dry-run feedback, management failure text). **L5** is corrected
in the previous report.


## 7. Implemented changes

| Finding | Change | Where |
| --- | --- | --- |
| C1 / F0 | `operation_in_progress` reads markers from the repository's own Git directory (`git_dir()`), not the process directory. Pull, push and branch refuse an open merge too. Unresolved conflicts are reported before in-progress (N2). | `src/git/command.rs`, `src/ops/commit.rs`, `src/ops/sync.rs`, `tests/workflows.rs` |
| H1 / F13 | `Gui::reload()` re-opens the session from the last opened directory; `POST /api/refresh` calls it. Busy protection: a reload is a no-op while an operation runs. | `src/gui/mod.rs`, `src/gui/server.rs` |
| H2 / F1–F2 | Failed runs are never "completed". New `unreported` status (`?`) for work that started or was never reached; `resultTitle(…, failed)`; "stopped before finishing" after partial results; no "nothing to do" for unreported work. | `src/gui/static/app.js`, `app.css`, `client.test.js` |
| H2 (server) | Management failure payload is an object like the other routes (L4). Management reloads after every real apply (N3). | `src/gui/mod.rs` |
| H3 / F8 | A refused plan (409/422) replaces the reviewed plan with the one the service returns; Apply is re-armed only after a new review. The server test asserts the returned plan carries the current id. | `src/gui/static/app.js`, `src/gui/server.rs` |
| H4 / F16 | TUI removal goes through `manage::plan`/`manage::apply`. It refuses when files would go back to the root and names `gitmesh configure remove <id> --confirm-takeover`. Dry-run is honoured. | `src/ui/app.rs` |
| M5 / F16 | Footer and help use the real bindings (`p` pull, `P` push, lowercase keys). | `src/ui/render.rs` |
| M1, M2 / F6 | Overview Details show an open Git operation (`merge in progress…`) and `local only (no remote)` for verified repositories. Clean repositories keep "nothing to record". | `src/gui/static/app.js` |
| M3 / F7 | Lost connection or a closed stream ends the operation with a message, clears the elapsed timer, and refreshes (N4). | `src/gui/static/app.js` |
| M4 / F7 | The wizard closes when the open request succeeds (callback), not after a fixed 400 ms timer. | `src/gui/static/app.js` |
| L1 / F14 | `aria-selected` on tabs, kept in sync; `:focus-visible` outline; wide tables scroll inside their panel; `role="status"` on the operation panel and status line. | `index.html`, `app.js`, `app.css` |
| L2 / F15 | The operation panel scrolls into view when work starts. | `app.js` |
| L3 / F15 | Dry-run toggled by keyboard or badge says the new state; turning it off says that operations will change repositories. | `app.js` |
| N1 | The wizard's editors renderer is `renderSetupRepositories`. A Rust test fails on two same-named declarations in one block. | `app.js`, `src/gui/asset.rs` |

**Not changed:** the Git engine, GitHub integration, manifest format, CLI commands and output,
exit codes, `--json`, `--dry-run`, the stale-plan fingerprint, Host/Origin checks, the
localhost binding and the busy guard. No dependency was added. The GUI still computes no plan.

## 8. Architecture and files changed

Layers are unchanged: Rust core (`ops`, `manage`, `setup`, `service`) decides; the GUI server
turns operations into events; `app.js` only presents. The only new shared pieces are the
`reload` method on `Gui` and the ordering of existing guards.

Files changed (`git status` at the end, nothing committed): `README.md`, `docs/GUI.md`,
`src/git/command.rs`, `src/gui/asset.rs`, `src/gui/mod.rs`, `src/gui/server.rs`,
`src/gui/static/{app.css,app.js,client.test.js,index.html}`, `src/ops/commit.rs`,
`src/ops/sync.rs`, `src/ui/app.rs`, `src/ui/render.rs`, `tests/workflows.rs`,
`tools/gui-workflow.py`, and this report. `.gitignore` was not changed.

## 9. UX and safety decisions

* **Refused is not failed, and failed is not completed.** A stale or blocked plan shows the
  current plan with its reasons. Apply is disabled until the user reviews it again.
* **Local-only repositories are skipped for remote operations, not failed** (existing
  behaviour, unchanged and re-verified by the E2E scripts).
* **No blind retries.** The interface never re-sends a commit, push or apply after a lost
  connection; it asks the user to check the state.
* **TUI consent.** The terminal has no confirmation step for handing files back to the root,
  so it refuses and names the explicit command. It does not add a prompt.
* **Setup keeps its rule.** A partial setup stays in the wizard and is not adopted (unchanged).
* **Merges are concluded only by Git.** GitMesh's unified commit refuses while a merge is open.
* **Browser-verified facts only.** The overview shows "local only" from the remote list, and an
  open operation from the marker files. Nothing is inferred.

## 10. Commands run and actual outcomes

All on the working tree after the changes above (Rust toolchain via `tools/rust-env.sh`).

| Command | Outcome |
| --- | --- |
| `cargo fmt --check` | exit 0 |
| `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| `cargo test` | **lib 326 passed, 0 failed; cli 15 passed; client 2 passed (the JS suite, all assertions); workflows 15 passed; 0 failed** |
| `cargo build --release` | exit 0 (rebuilt after the final static asset edits) |
| `tools/gui-workflow.py` | **75 passed, 0 failed** (baseline 74/0; one check added, step 12 changed as N5) |
| `tools/setup-workflow.py` | **115 passed, 0 failed** (baseline 115/0) |
| `tools/repository-workflow.py` | **179 passed, 0 failed** (baseline 179/0) |
| Regression check: F0 test against the original `command.rs` | FAILED as intended (`left == right`), then restored and passed |
| Browser run `check1` (welcome, wizard, tab state) | welcome PASS, wizard editors PASS (2 editors), aria-selected PASS; a later step timed out because that server had no project open (test-script limitation, not an app result) |
| Browser run `check2` (project overview) | **9 passed, 0 failed**: aria-selected, local-only text, focus outline, dry-run feedback on/off, Repositories panel lists 3 rows, no page or console errors |

The browser checks used Playwright with Chromium in the sandbox (installed outside the
repository, not a project dependency). They are not committed. Setup in the sandbox needed
`sudo` to install Chromium's system libraries.

## 11. Test coverage added

* Rust: `a_merge_in_progress_in_an_external_repository_is_seen_and_protected` (commit, pull,
  push, branch refuse; HEAD unchanged; `MERGE_HEAD` kept); `refresh_rereads_the_project…`
  (external repository appears after refresh; invalid manifest shown as error);
  `unassigning_refuses_when_files_would_go_back…` (replaces the old removal test that
  expected the unsafe outcome); stale-plan test asserts the returned plan id;
  `no_function_is_declared_twice_in_one_block` (fails on the N1 collision, verified).
* JavaScript: failed/unreported folds, failure titles, dry-run failure, no "skipped" after a
  failure, repository details for in-progress and local-only.

## 12. Limitations, risks, and what was not done

**Limitations**

* The partial-apply reload (N3) has no dedicated test: forcing a management action to fail
  after the manifest write needs a fault injection the service does not have.
* The stale-plan refresh in the Repositories panel (H3) is covered on the server side and by
  reading the code; the browser path was not exercised.
* The wizard browser check covered one scenario (scan a directory with two repositories, suggest,
  see editors). The full wizard apply was not run in a browser.
* Overlap ownership (M2) was not given a new column. The existing warning ("N file(s) belong to
  two repositories") in the Repositories panel already reports it.
* The operation panel is still below the workspace (L2 partly fixed by scrolling into view).
* Browser verification covered Chromium only.

**Risks**

* A refresh after a failed *setup* will open the partially built project, because refresh
  reads what is on disk. This is consistent with "refresh reads disk", but it differs from
  the wizard's "do not adopt a partial project" rule. Recommended: decide whether refresh should
  show a partial setup as an error state.
* The F0 guard now refuses operations in repositories with an open merge that a user had
  resolved by hand. This is the intended behaviour; users must conclude the merge with Git.

**Not implemented (recommended only)**

* A confirmation prompt for takeover in the terminal (the terminal refuses instead).
* A dedicated overview column for root-tracked overlap.
* Keyboard navigation of the project tree and a full mobile layout review.
* Persisted operation history or a result log.

## 13. Manual verification steps

1. `cargo build --release`, then `gitmesh gui /path/to/project`.
2. Add a repository in a terminal (`gitmesh configure add <dir>`), then press Refresh in the
   browser. The new repository must appear in the Repositories tab.
3. Create a merge in one repository and leave it open. The overview must say
   `merge in progress: finish or abort it in Git first`, and Commit must refuse that repository.
4. Open the Repositories tab, choose a remove of a repository whose directory the root tracks,
   review the plan, then change the configuration in a terminal and apply. The old plan must
   be replaced, and Apply must be disabled until reviewed again.
5. Press `d` twice and read the status line each time.
6. Tab through the page: every control must show a visible outline. Use a screen reader to
   check that the active tab is announced.
7. In `gitmesh ui`, press `a` on an external repository whose directory the root tracks. The
   log must refuse and show `gitmesh configure remove <id> --confirm-takeover`.

## 14. Readiness

Implemented and validated for the listed findings, with the limitations in §12. Nothing is
committed. The report file, the source changes, and the doc changes are in the working tree
for review.
