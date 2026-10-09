# GitMesh UX/UI redesign — initial audit

Status: written before any implementation change. Scope: the local web GUI
(`src/gui/static/`, `src/gui/*.rs`) and the terminal UI (`src/ui/`). Core services
(`src/service.rs`, `src/ops/`, `src/git/`) were read for the points that the front ends depend on.

## 1. Repository state at audit time

- Base commit `b4198d8` on `main`. Nothing staged, nothing committed in this task.
- 20 modified files and one untracked report from the previous GUI/UX audit task
  (`reports/GUI_UX_AUDIT_REPORT.md`) are still present and are preserved. This redesign does
  not overwrite that report; it writes `reports/UX_REDESIGN_AUDIT.md` and later
  `reports/UX_REDESIGN_REPORT.md`.
- Executable bits on five tooling scripts were restored (644 → 755). `git diff --summary` is clean.
- `.gitignore` was not modified during the audit.

## 2. Baseline gates (before redesign edits)

| Gate | Result |
|---|---|
| `cargo fmt --check` | exit 0 |
| `cargo clippy --all-targets -- -D warnings` | exit 0 |
| `cargo test` | lib 326, cli 15, client 2, workflows 15 — all passing |
| `cargo build --release` | exit 0 (49 s) |

## 3. Evidence captured

Fixture (rebuilt under `/tmp/ux/proj`, not in the repository): a root repository with
`README.md`; `app` (external, with a real bare-repo `origin`); `lib` (external, **local only**, no
remote). Five changes across three repositories.

- **GUI at 390 px (before):** page scroll width is **432 px** — horizontal overflow. The header
  wraps its tagline into four lines, `Open…` and `Set up…` are squeezed and clipped at the right
  edge, the seven tabs wrap onto three rows, and the repository-status table has four columns in a
  ~300 px card (details wrap one word per line). Screenshot: `/tmp/ux/gui-before-390.png`.
- **TUI at 100×30 (before):** three stacked panels plus a "one project, one status" title, a
  footer with eight keys, and a "changes by owning repository" panel. Useful information is
  present, but the footer hides the rest of the key set and the activity panel is mostly empty.
- **TUI at 50×14 (before):** the status table collapses to three-letter columns (`RE P BR`), the
  change list is empty because its panel has no height, and the footer is cut at `s: refr`. There
  is no fallback for small terminals.

## 4. Findings

### 4.1 GUI — information architecture

1. **Seven tabs, with commit separated from changes.** Changes (staging list) and Commit (message
   and targets) are two tabs, so the user has to move between them to commit what they just
   reviewed. The Status tab duplicates the tree and repository table.
2. **"Pull / Push" and "Repositories" carry configuration and sync in one place.** Repository
   assignment (configuration) lives beside pull and push (daily sync) on different tabs with no
   visual difference in risk.
3. **Concept not explained where it is needed.** The root-vs-nested concept appears as a hint
   paragraph inside the tree card. There is no short, collapsible explanation at the top of the
   project view.
4. **Primary action is not context-aware.** The Overview never says what to do next (commit N
   changes, push N repositories, nothing pending).
5. **Stage semantics are invisible.** The commit tab says the project is committed "with one
   message", but not that every change is staged first. Users cannot tell what will be included
   before pressing the button (the core's `commit_project` stages all owned changes).

### 4.2 GUI — layout, density and responsiveness

6. **Horizontal overflow at phone width** (432 px at a 390 px viewport; see §3).
7. **Header actions** (Refresh, Open…, Set up…) are not grouped by purpose and collapse badly.
8. **Tables** are fixed-column and do not reflow; paths and error text break inside narrow cells.
9. **Density:** the Repositories tab shows a long explanatory paragraph, a full table and two
   management forms at once.

### 4.3 GUI — design system

10. **No tokens.** Spacing, type sizes and radii are set per component. Buttons have `primary`,
    `ghost` and `danger ghost`, but the visual difference between a neutral and a destructive
    action is weak.
11. **Status is conveyed by colour badges.** `badge-warn` and similar are coloured text with
    little or no text difference, which is not enough for colour-blind users.
12. **Empty, loading and result states** are ad hoc strings in the summary line rather than
    components.

### 4.4 TUI

13. **Mixed purpose.** The TUI has two screens (Setup with a tree and manual assignment; Project)
    and exposes commit, fetch, pull, push, branch checkout, new branch, merge, rename, remote URL,
    assign/unassign and takeover. The everyday workflow is buried among management actions.
14. **No change list.** The main panel is a directory tree, not the changed files grouped by
    owning repository with status letters and counts.
15. **Commit is a one-line prompt.** The TUI commit path calls the same stage-all commit as the
    CLI and GUI. Nothing is shown before it runs, and an empty message is possible in the prompt
    path.
16. **Footer and help cannot fit.** The footer lists keys in one line and is truncated below about
    80 columns. The advertised keys (`c p P f s n b m …`) are not all the keys that exist (`a`,
    `r`, `A`, `d`, `?`), so the footer does not match the full binding set.
17. **Synchronous operations with no "working" state.** Commit, pull and push run inside the
    key handler; the screen is not redrawn to show that an operation is in progress, and other
    keys are not explicitly blocked during it.
18. **No small-terminal fallback.** Below a usable size the layout degrades silently (§3).
19. **Documentation is out of date for the TUI.** `docs/ARCHITECTURE.md` says "or `A` in the UI"
    for assignment, and README says the terminal "can also edit the configuration". Both have to
    change when the TUI scope changes.

### 4.5 Staging and conflicts (core behaviour, verified in source)

- `ops/commit.rs::commit_project` filters owned entries, returns **Skipped** for "nothing to
  commit", then runs `stage_all` (`git add -A -- .` in the repository, with root exclusions for
  external repositories), a stray-file guard that unstages files owned by another repository, and
  finally `git commit -m`.
- Staging is already **repository-scoped and respects ignore rules** (`git add -A` skips ignored
  files). Another repository's files are not staged.
- It does **not** check for unmerged paths or an in-progress merge/rebase/cherry-pick before
  staging. `git add` would silently mark conflicted files resolved. This is the one real safety gap
  found for a repo-scoped "Stage All".
- `status.rs` already exposes `unmerged` codes, `staged`/`unstaged`/`untracked` flags and
  `has_conflicts()`. These are sufficient for the new operation; no new Git logic is needed.
- Local-only repositories (no remote) already produce `Skipped` with "no remote configured" in
  `ops/sync.rs` and `ops/push.rs`. The TUI must present these as skipped, not failed.

## 5. Usability problems ranked

| # | Problem | Severity | Area |
|---|---|---|---|
| 1 | Commit does not show what will be staged/committed; TUI stage-all is implicit | High (safety/trust) | Core + TUI + GUI |
| 2 | Stage-all does not refuse repositories with conflicts or an open merge | High (safety) | Core |
| 3 | GUI horizontal overflow and clipped header on phones | High | GUI |
| 4 | TUI unusable below ~60 columns, footer keys not matching bindings | High | TUI |
| 5 | TUI mixes management actions with the daily workflow | High | TUI |
| 6 | Changes and commit split across tabs | Medium | GUI |
| 7 | No next-step guidance on the project view | Medium | GUI |
| 8 | Colour-only status indicators | Medium | GUI |
| 9 | Long paths, errors and messages break cramped layouts | Medium | GUI + TUI |
| 10 | Docs describe the old TUI (`A` key, configuration editing) | Medium | Docs |

## 6. Decisions (planned before implementation)

1. **Repo-scoped Stage All** is a new core operation (`ops::stage`). It reuses the private stage
   logic (`git add -A -- .` with root exclusions), refuses any repository that has unmerged paths
   or an open merge/rebase/cherry-pick, and reports per-repository outcomes. It never touches
   another repository.
2. **TUI Commit commits what is staged.** The TUI stages explicitly with Stage All so the user
   sees the staged set before committing. The CLI and GUI keep their current stage-all commit
   semantics, which are unchanged (no CLI regression). This difference is documented in
   `docs/TUI.md` and the report.
3. **TUI reduced to the everyday workflow:** project and branch header; grouped changes panel with
   M/A/D/R/?? and counts; commit message field; actions Stage All, Commit, Pull, Push; a result
   panel; a footer with only the keys that are bound. Setup, tree navigation, assignment, rename,
   remote configuration, branch checkout/create/merge and fetch are removed from the TUI. They
   remain in the CLI (`gitmesh configure`, `branch`, `checkout`, `merge`, `fetch`, `set-remote`) and
   in the GUI. This is a deliberate, documented reduction, not a silent removal.
4. **Commit requires a non-empty message and a second confirmation.** Pull and Push show a "working"
   frame before running, and keys are blocked during the operation.
5. **GUI keeps every existing function** (Repositories management, branches, merge, sync, the
   setup wizard, Settings) but reorganises it: fewer top-level views (Overview, Changes and
   commit, Sync, Branches, Repositories, Project), a compact header, a design-system stylesheet with
   tokens, icons and text for every state, reflowing tables, and a collapsible concept note.
6. **No new dependencies.** Browser tests use Playwright outside the repository (documented
   tooling); the TUI PTY test uses `pyte` outside the repository. Both are test tooling only.

## 7. Verification plan

- Rust unit tests for the TUI state machine (key handling, commit confirmation, empty message,
  busy blocking, footer/binding consistency) and render tests with ratatui `TestBackend` at sizes
  from 20×6 to 200×60.
- Integration tests with real temporary repositories for stage-all (multi-repo, ignore rules,
  conflicts, open merge), commit (per-repo, skipped repos, no fake global commit), pull and push
  (partial failure, local-only repos skipped).
- Chromium (Playwright) tests for the GUI at 360, 768 and 1280 px: overflow, navigation,
  onboarding, stale plan, error states, keyboard focus.
- PTY tests (pyte) for the TUI at constrained sizes: navigation, staging, commit, push/pull
  outcomes, partial failures.
- Existing GUI, setup and repository E2E workflows; fmt, clippy `-D warnings`, the full test
  suite and a release build.

## 8. Not in scope

Repository creation, renaming, remote configuration, manifest editing and branch management in the
TUI; any remote service, analytics or external asset; changes to RAMforge or forgeCORE; commits
or tags; version or milestone numbers.
