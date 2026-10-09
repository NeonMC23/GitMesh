# GitMesh UX/UI redesign — implementation report

Status: implemented and verified in the working tree. **Nothing is committed and no tag is
created.** No version or milestone number is assigned. The previous session's uncommitted work
(the GUI/UX audit fixes, `reports/GUI_UX_AUDIT_REPORT.md`) is still present and is included in
`git diff` against `b4198d8`. It was not reverted by this task.

Companion documents: `reports/UX_REDESIGN_AUDIT.md` (the audit written before any code change),
`docs/TUI.md` (terminal keys and semantics), `docs/GUI.md` (updated views).

## 1. Audit findings (summary)

Full evidence is in `reports/UX_REDESIGN_AUDIT.md`. The key findings, with measurements:

* **GUI at 390 px:** the page was 432 px wide at baseline (horizontal overflow). The header
  clipped its buttons, seven tabs wrapped onto three rows, and the repository table was
  cramped. After the redesign the page does not overflow at 390 px; tables scroll inside their
  cards.
* **TUI at 50×14:** the status table collapsed to three-letter columns, the change list was
  empty, and the footer was cut. There was no small-terminal fallback.
* **TUI scope:** the terminal mixed the everyday workflow with management (assign, rename,
  remote, branch checkout/create/merge, fetch, takeover) and had no changed-file view.
* **Staging:** the commit path staged every owned change automatically and never showed what
  would be committed. Its `git add -A` does not refuse repositories with conflicts or an open
  merge, so a "Stage all" built on it would have silently resolved conflicts.
* **Documentation drift:** `docs/ARCHITECTURE.md` described an `A` key for repository
  assignment in the terminal, and the README said the terminal could edit the configuration.

## 2. Usability problems addressed

| Problem | Change |
|---|---|
| Commit separated from the changes it commits | Changes view now holds the list and the Commit card below it |
| No "what next" guidance | Overview's next-step line, derived from the model (conflicts → changes → commits to push → nothing to do) |
| Concept unexplained | Collapsed "How a GitMesh project works" note on the Overview |
| Seven tabs, unclear labels | Six views: Overview, Changes, Branches, Sync, Repositories, Project |
| Header buttons clipped at phone width | Grouped toolbar that wraps; tagline hidden on narrow windows |
| Tables crush or overflow the page | Cards scroll their own wide content; `main` pins a single shrinkable column |
| Colour-only status | Every state has a glyph and text (`✓`, `●`, `!`, `–` with words) |
| Terminal unusable when small | Minimum 56×14 with a resize notice; everything else reflows |
| Terminal footer did not match bindings | Footer and help come from the same key tables; tests check them |
| Terminal commit was one prompt with no preview | Message field, explicit Stage all, confirmation that names the file and repository count |
| Terminal result could hide repository outcomes | Result panel lists every repository and says how many lines are hidden if it cannot fit |

## 3. Design decisions

1. **Repo-scoped Stage all is a new core operation** (`src/ops/stage.rs`). It reuses the same
   exclusion rules as commit (the root never takes files owned by a nested repository),
   respects ignore rules, and **refuses** a repository with conflicted paths or an open merge,
   rebase or cherry-pick, reporting it as `!` or `✗` without running `git add` there. After
   staging it unstages anything that belongs to another repository.
2. **Terminal Commit commits only what is staged** (`CommitOptions::staged`, `staged_only`).
   The CLI and GUI keep stage-all-then-commit, so their behaviour is unchanged. This difference
   is deliberate and documented in `docs/TUI.md` and `docs/GUI.md`: in the terminal the staged
   set is visible before it becomes a commit.
3. **The terminal is reduced to the everyday workflow.** It no longer offers setup, assignment,
   rename, remote configuration, manifest editing, branch checkout/create/merge, fetch, or
   takeover. These remain in the CLI (`configure`, `set-remote`, `branch`, `checkout`, `merge`,
   `fetch`) and in the GUI. This is a documented reduction, not a silent removal, and it is
   listed under limitations.
4. **Commit has two steps.** `c` (or Enter in the message field) states what will be committed
   and asks for confirmation. `c` or Enter again confirms; Esc or any other key cancels
   (`cancelling_or_any_other_key_drops_the_pending_commit`).
5. **Refusals explain the next step.** An empty message moves focus to the message field;
   committing with nothing staged points to Stage all; no project explains `gitmesh init`.
6. **Busy protection.** While an operation runs, every key is refused and a working frame is
   shown (`keys_are_refused_while_an_operation_is_running`).
7. **Honest result titles.** A result says "Push: NOT everything succeeded" with "N of M
   repositories need attention" whenever a repository conflicted or failed. "done in N
   repositories" appears only when no repository needs attention. Local-only repositories are
   `-` (skipped) and do not count as problems.
8. **No new dependencies.** Browser and terminal tests are standalone Python scripts under
   `tools/` that use Playwright and pyte, which are installed outside the repository. Nothing was
   added to `Cargo.toml`. The GUI remains a self-contained, std-only server with embedded
   assets. Its pure client logic is tested by `tests/client.rs` under Node (v20 was installed for
   the final run; without Node the test prints a skip notice instead of running).
9. **Design system.** `app.css` is rewritten around tokens (colour, type scale, spacing, radius,
   focus ring), with primary, ghost and danger buttons, one card surface, and responsive rules.
   The GUI's decision logic (`GitMesh.nextStep`) lives in the tested client-logic region.

## 4. Changes

### Core
* `src/ops/stage.rs` (new): `stage_project`, `StageOptions`, with unit tests.
* `src/ops/commit.rs`: `CommitOptions::staged_only` and `CommitOptions::staged()`. Staged-only
  commits skip repositories with nothing staged ("nothing staged to commit"), never call
  `git add`, and say "would commit N staged file(s)" in dry run. Tests added.
* `src/ops/mod.rs`: exports `stage_project` and `StageOptions`.
* `src/main.rs`: the one struct literal of `CommitOptions` gets `staged_only: false`. CLI
  behaviour is unchanged.

### Terminal (`src/ui/`)
* `app.rs` (rewritten): terminal-independent state machine (`Key`, `Action`, `Focus`,
  `BUTTONS`, `SHORTCUTS`, `ChangeLine`, `ResultView`, `App::handle_key`, `dispatch`,
  `result_view`). Tests: key-driven workflows over real repositories.
* `render.rs` (rewritten): header, grouped changes, message field, action bar, result panel,
  footer, help overlay, resize notice, no-project screen. Tests: ratatui `TestBackend` at sizes
  from 1×1 to 200×60.
* `mod.rs` (rewritten): event loop, key translation (`translate`, `handle_key`), non-TTY
  fallback. Tests: key translation.

### GUI (`src/gui/`)
* `static/app.css` (rewritten): design system and responsive rules.
* `static/index.html`: six views; the commit card moved under the changes list; concept note and
  next-step line on the Overview; header toolbar.
* `static/app.js`: `GitMesh.nextStep` (pure, exported, tested), `renderNextStep`, next-step
  button delegation to the existing tab wiring; "Status tab" wording updated to Overview.
* `static/client.test.js`: next-step priority tests.

### Tests and tools
* `tools/tui-pty-check.py` (new): drives the release binary in a pseudo-terminal (pyte).
* `tools/gui-browser-check.py` (new): drives Chromium through Playwright.

### Documentation
* `docs/TUI.md` (new): keys, commit and staging rules, pull/push outcome symbols, size behaviour,
  what the terminal does not do.
* `docs/GUI.md`: view list and commit description updated.
* `README.md`: terminal description and the status paragraph.
* `docs/ARCHITECTURE.md`: removed the stale `A` key reference; module table updated.
* `docs/DEVELOPMENT.md`: test layers for the terminal and browser checks.

### Key files
`src/ops/stage.rs`, `src/ops/commit.rs`, `src/ui/app.rs`, `src/ui/render.rs`, `src/ui/mod.rs`,
`src/gui/static/{app.css,index.html,app.js,client.test.js}`, `tools/tui-pty-check.py`,
`tools/gui-browser-check.py`, `docs/TUI.md`, `docs/GUI.md`.

## 5. Test results (actual runs, final tree)

| Gate | Result |
|---|---|
| `cargo fmt --check` | exit 0 |
| `cargo clippy --all-targets -- -D warnings` | exit 0 |
| `cargo test` | lib **345** passed (326 at the start of this task's baseline), CLI **15**, client JS **2**, workflows **15**, 0 failed |
| `cargo build --release` | exit 0 |
| `tools/gui-workflow.py` (GUI E2E) | **75** checks, 0 failed |
| `tools/setup-workflow.py` (setup E2E, `/tmp/gitmesh-setup-e2e/MyProject`) | **115** checks, 0 failed |
| `tools/repository-workflow.py` (repository E2E) | **179** checks, 0 failed |
| `tools/tui-pty-check.py` (terminal, real PTY) | **24** checks, all passed |
| `tools/gui-browser-check.py` (Chromium) | **29** checks, all passed |

Terminal checks (`tools/tui-pty-check.py`): main screen layout, grouped changes, `??`
indicators, the four actions and footer keys; resize to 40×10 (notice) and back to 100×30;
60×16 usable; help open and close; commit refused before staging; Stage all staging the
engine's changed and new files and none of the root's; confirmed commit creating a real commit
in the engine with the typed message; field cleared; pull results per repository with the
local-only repository skipped; partial push failure reported as "NOT everything succeeded"
with `✗ engine`; `q` and Ctrl-C quit.

Browser checks (`tools/gui-browser-check.py`): no page overflow at 360, 768 and 1280 px; no
script errors at each width; six views shown and exclusive; the next-step action opens Changes
and the commit card is visible; keyboard focus ring visible; concept note collapsed, then open;
first-use screen explains and offers setup and fits a phone.

Defects found and fixed during verification (all covered by the final runs):
* The terminal result panel clipped the last repository line of a pull. Fixed by sizing the
  panel to its content, and a visible "N more line(s) hidden" note when lines cannot fit.
* `main` grid column sized to content, so the page overflowed at 390 px (a 574 px section during
  the fix; 432 px page width at baseline). Fixed with a shrinkable single column.
* Table header words broke mid-word ("REPOSIT ORY"). Fixed with normal header wrapping.

## 6. Limitations and what was not done

* **Browser coverage is Chromium headless on Linux only**, through Playwright. The stale-plan and
  error-state flows were **not** exercised in the browser. They are covered by the Rust server
  test `a_reviewed_plan_is_refused_when_the_configuration_moved_on`, by
  `a_blocked_setup_is_refused_with_the_plan_that_explains_why`, and by the setup and repository
  E2E scripts over HTTP.
* **Visual review** was done with screenshots at 390 px (before and after) and overflow
  measurement at 1280 px. 768 px was verified for overflow, not inspected visually. The final
  390 px screenshot after the last table-header fix was not re-taken.
* **Contrast** was chosen by palette and was **not measured** with any tool. Treat contrast as
  unverified.
* **Screen readers** were not tested. Focus rings and labels were checked by inspection and by
  the keyboard-focus assertion.
* **Terminal tests use a terminal emulator library (pyte)**, not a physical terminal, and run on
  Unix only. Column counts use character counts, so wide (CJK) characters may misalign.
* **Terminal scope reduction is breaking for existing terminal users**: branch, fetch and
  configuration keys are gone from `gitmesh ui`. The CLI and GUI keep all of them, and
  `docs/TUI.md` lists the replacements.
* **Terminal and GUI commit differ on staging** by design (see §3.2). Terminal users who expect
  `git commit -a` behaviour must press Stage all first.
* **Not redesigned:** the setup wizard's internal steps and the Repositories management forms
  keep their content; only their surrounding layout changed.
* **No `gitmesh stage` CLI command** was added; Stage all is a core operation used by the
  terminal. Exposing it on the CLI is a possible follow-up.
* The previous session's uncommitted work is still in the tree, so `git diff` mixes it with this
  task's changes.

## 7. Recommendations

1. Review the terminal reduction with users before committing. If branch switching is used
   daily, add a single "switch branch" action rather than restoring the old screen.
2. Consider a `gitmesh stage` command so the staging step is scriptable and matches the terminal.
3. Run the browser checks on Firefox and WebKit in CI, and add the stale-plan and error-state
   flows to `tools/gui-browser-check.py`.
4. Measure contrast with a tool (for example a WCAG checker) before a release.
5. Consider a width-aware text measure in the terminal if CJK paths matter, and a re-check of
   the 768 px layout visually.

## 8. Diff review

* Reviewed every file changed by this task. Generated artefacts were removed
  (`tools/__pycache__/`). No build output or fixtures are in the repository, and `.gitignore`
  was not modified.
* Nothing under RAMforge or forgeCORE was touched.
* No dependency was added to `Cargo.toml`.
* `git status` lists only intended files: modified source, docs and tests; new
  `src/ops/stage.rs`, `docs/TUI.md`, `tools/tui-pty-check.py`, `tools/gui-browser-check.py`,
  `reports/UX_REDESIGN_AUDIT.md`, and this report.
* Existing tests were not weakened. Test expectations were corrected only where the test itself
  was wrong (for example, the fixture root id and fixture changes).

## 9. Readiness

**Ready for review, not ready to merge without that review.** Every automated gate passes on
the final tree, and the terminal and browser flows were exercised end to end on real
repositories. Before merging, a human should review the terminal scope reduction (§6), the
staging difference between terminal and other front ends (§3.2), and the layout at
intermediate widths. The limitations in §6 are real and are not covered by the results above.
