# Repository lifecycle: audit, implementation and verification

Status: implemented in the working tree, uncommitted, untagged. Baseline commit `b4198d8`.
The final audit pass (CLI guidance, GUI clone journey, upstream diagnostics, docs) is recorded in
section 10. Browser and E2E suites were run on the final tree and passed; see section 5.
Everything in "Verified" was run in this sandbox; everything in "Not verified" was not.

## 1. Audit: what was wrong, and how each state is classified

Each state was traced through the code and then reproduced with the real binary in a scratch
project (pre-change results recorded in the session log).

| State as reported | Classification | Finding |
|---|---|---|
| "remote has commits this repository does not have" | Legitimate Git state (remote ahead), **incorrect classification** when histories are unrelated or diverged | Push matched the English words `rejected` / `fetch first`. Unrelated histories and diverged histories both produced this text, and a hook refusal was reported as a divergence. |
| "no upstream branch configured" | Legitimate state; **defect** in pull | Pull returned this before probing the remote, so an unreachable remote was hidden behind it. Skipped, not failed, which is right for the state itself. |
| "repository has no commits yet" | Legitimate state; skipped by pull and push | Correct. Commit-dependent steps are not run. |

Further defects found:

- **Silent empty repository on import.** `add` with a remote and a non-existent directory was
  refused, but an *empty existing* directory with a remote that had history was `git init`-ed,
  producing an unrelated repository that pull and push then skipped. (Experiments 1c/1d.)
- **No clone at all.** The only way to get a remote into the project was to create the empty
  directory and init it, so cloning was missing as a workflow.
- **Push to a differently named branch without asking** (`push --set-upstream` created a branch
  on the remote with the local name). Retained as the documented default: same-name push. No
  guessing among several remote branches was added.
- **False success after remote branch deletion.** Pull reported "already up to date" when the
  tracked branch no longer existed on the remote. Fixed: reported as failed.
- **Misleading CLI flags in messages.** The diverged-branch messages told users to run
  `--merge` / `--rebase`. Those flags do not exist; the real flags are `pull --strategy merge|rebase`.
  Corrected, and two existing assertions that encoded the wrong flag were updated (section 6).

## 2. Decisions

- Git is driven through the CLI only. No new dependencies. No hosting API, tokens or credentials.
- Remote probing uses `git ls-remote` (read-only). A remote starting with `-` is refused before Git runs.
- A rejected push is classified from `git push --porcelain` ref status, then divergence is
  measured with `rev-list --left-right --count` and `merge-base` after a read-only fetch of the
  remote (a rejected push does not update the remote-tracking refs).
- **Never force.** No code path adds `--force` or `--force-with-lease`.
- Clone refuses before Git runs when the destination exists and is non-empty, or is a repository.
  Clone also refuses when the remote cannot be read, so nothing is created.
- If a clone fails at run time, the `update-manifest` action is skipped so no repository the
  clone did not create is recorded.
- The TUI does **not** get clone. It stays the daily workflow (status, commit, pull, push). Clone is
  GUI and CLI only (`docs/REPOSITORIES.md`).
- The manifest is still written only by GitMesh.

## 3. Implementation

- `src/git/remote.rs` (new): `probe_remote`, `parse_ls_remote`, `condense_git_error`,
  `transport_hint`, `parse_push_porcelain` / `PushRefusal`, `divergence` / `Divergence`,
  `ref_exists`, `upstream_is_gone`. Seven unit tests.
- `src/manage.rs`: `RepositoryIntent::Clone`; `plan_clone` (validates the path, id, conflicts,
  destination, and probes the remote); `CloneRepository` change and action kinds; run-time
  execution through `discovery::clone_repository`; verification of the clone (`.git` exists and
  the manifest lists it); the manifest-skip guard after a failed clone; `plan_add` probes the
  remote (unreachable → warning, empty remote → notice, empty directory with a remote that has
  history → refused, with a pointer to clone); the missing-directory refusal names the clone path.
- `src/discovery.rs`: `is_empty_directory`, `clone_repository` (refuses non-empty destinations,
  runs `git clone -- <url> <dir>`, tracks `origin/<default>` when the remote has commits).
- `src/ops/push.rs`: `--porcelain` push; `refusal_outcome` classifies unrelated, diverged,
  remote-ahead, remote-refused (with the remote's own `remote:` lines), and other refusals.
  Wording names `gitmesh pull --strategy merge|rebase`.
- `src/ops/sync.rs`: pull reports a deleted upstream as failed; unrelated histories are reported
  before the generic divergence message, whatever the strategy; the strategy wording is corrected.
- `src/cli.rs`, `src/main.rs`: `gitmesh configure clone <dir> --remote <url> [--id] [--branch] [--dry-run]`.
- `src/gui/server.rs`: `clone` intent in the repository form (plus a unit test).
- `src/gui/static/app.js`, `index.html`, `client.test.js`: a "clone a remote into a new directory"
  option with its own fields. The GUI sends the request; the Rust core plans and runs it.
- `tests/lifecycle.rs` (new): 27 integration tests on temp bare repositories, no network. Six were
  added in the final audit: pull diagnostics (unreachable remote, no upstream with one remote, several
  remotes, same-name branch available, no same-name branch, empty remote), the unborn-repository
  notice, and the onboarding candidate state.
- `tests/cli.rs`: one test for `configure clone` through the real binary.
- Docs: `docs/REPOSITORIES.md` (new), `docs/GUI.md` (clone section), `docs/ARCHITECTURE.md`
  (section 9), `README.md` (command table and documentation index).

## 4. Changed files (this task)

`src/manage.rs`, `src/discovery.rs`, `src/ops/push.rs`, `src/ops/sync.rs`, `src/cli.rs`,
`src/main.rs`, `src/gui/server.rs`, `src/gui/static/app.js`, `src/gui/static/index.html`,
`src/gui/static/client.test.js`, `src/git/remote.rs` (new), `src/git/mod.rs`, `tests/lifecycle.rs`
(new), `tests/cli.rs`, `docs/REPOSITORIES.md` (new), `docs/GUI.md`, `docs/ARCHITECTURE.md`,
`README.md`, `reports/LIFECYCLE_REPORT.md` (new).

Final audit pass, additionally: `src/ops/mod.rs` (`guidance_lines`, one unit test), `src/main.rs`
(plan notices printed in dry runs; no-upstream guidance printed for skipped pull/push),
`src/ops/sync.rs` (`no_upstream_outcome`), `src/manage.rs`, `src/service.rs`, `src/gui/static/app.js`,
`src/gui/static/client.test.js`, `tools/gui-clone-check.py` (new), `docs/GUI.md`.
`cargo fmt` (run in the final pass) changed only hunks in `src/ops/mod.rs`, `src/ops/sync.rs` and
`tests/lifecycle.rs`, all written by this task.

The working tree also contains changes from the earlier GUI/TUI redesign task, which are not
part of this report. `cargo fmt` reported no changes beyond these.

Scratch file deleted: `/home/user/tmpbuild/command.rs.fixed`, after confirming it was byte-identical
to `src/git/command.rs`.

## 5. Test commands and actual results

Run in this sandbox against the final tree (after the final audit pass, all in one session, no
later edits).

| Command | Result |
|---|---|
| `bash tools/rust-env.sh cargo fmt --check` | exit 0 (after one `cargo fmt` that changed only this task's hunks) |
| `bash tools/rust-env.sh cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| `bash tools/rust-env.sh cargo test --no-fail-fast` | exit 0: lib 354 passed; cli 16 passed; client 2 passed; lifecycle 27 passed; workflows 15 passed; 0 failed |
| `bash tools/rust-env.sh cargo build --release` | exit 0 |
| `python3 tools/gui-clone-check.py` | exit 0: 32 PASS, 0 FAIL (clone journey and onboarding states in Chromium) |
| `python3 tools/gui-browser-check.py` | exit 0: 29 PASS, 0 FAIL |
| `python3 tools/gui-workflow.py` | exit 0: 75 checks passed, 0 failed |
| `python3 tools/repository-workflow.py` | exit 0: 179 checks passed, 0 failed |
| `python3 tools/setup-workflow.py` | exit 0: 115 checks passed, 0 failed |
| `python3 tools/tui-pty-check.py` | exit 0: 24 PASS (terminal checks) |
| `/tmp/smoke2.sh` (CLI, against the rebuilt release binary) | unborn repository dry run prints the history notice; missing directory exit 2 with the clone hint; empty directory with remote history exit 2; unreachable pull Failed with guidance; reachable pull with no same-name branch Skipped with the branch list; reachable same-name branch Skipped with the exact `git branch --set-upstream-to` command |

The earlier manual smoke script (`/tmp/audit/smoke_clone.sh`, from the first pass) is no longer in
the sandbox and was not re-run. Its earlier results were: clone into a new directory exit 0 with
`origin/main` tracked; dry run changed nothing; repeat refused with exit 2; non-empty directory
refused with files intact; unreachable remote refused with Git's reason.

The `tools/gui-clone-check.py` checks cover, in the browser: duplicate destination, non-empty
destination, unreachable remote, invalid remote (looks like an option), and empty remote, plus the
onboarding states (empty directory, unborn repository, missing directory).

The lifecycle tests cover: clone into a new directory (tracking and manifest); clone does not
change root ownership; refusal into a non-empty directory; refusal into an existing repository;
unreachable remote refused, nothing created; late clone failure not recorded; empty remote clone;
missing directory pointed to clone; connecting an existing repository keeps history; first push to
an empty remote; empty directory not initialised onto remote history; unreachable remote recorded
with a warning; local-only repository skipped, not failed; remote without upstream; repository
without commits skipped; rejected diverged push explained and not forced; unrelated histories
reported by pull and push; hook refusal reported as the remote's refusal; deleted upstream not
reported up to date; one failing repository among several, counts correct; fast-forward pull; and
the six final-audit cases listed in section 3.

The unit test `guidance_shows_the_next_step_for_a_missing_upstream_but_not_for_local_only_repositories`
(in `src/ops/mod.rs`) checks that a local-only skip is left out of the guidance and that a missing
upstream is included, while the full listing still has both.

## 6. Changes to existing tests (disclosed)

- `src/ops/sync.rs` `diverged_branch_is_reported_not_merged`: asserted the detail contains
  `--merge`. The flag does not exist, so the assertion now checks `--strategy merge`.
- `tests/lifecycle.rs` (my new test): the divergence assertion now checks `--strategy merge|rebase`
  instead of `--merge|--rebase`, for the same reason.

No other existing assertion was changed.

## 7. Recovery of previously overwritten files

**Not recovered.** `/tmp/arena-workspace/hydrate.zip` was inspected with `/tmp/audit/recover.py`
(read-only; outputs in `/tmp/audit/recover/`). Its copies of `gitmesh/src/ui/app.rs` and
`gitmesh/src/gui/static/app.css` are byte-identical to the current live files. They therefore do
not show any earlier, pre-overwrite content, and cannot be used as a recovery source. The live files
were not overwritten. No other local history source was found. The loss remains open.

## 8. Report corrections (`reports/UX_REDESIGN_REPORT.md`)

Checked against the report. None of the flagged strings are in the file (`7:1`, `586 px`, the
"two of three" pull-panel claim, the "a second time" commit claim, and the stale-confirmation claim),
so no edits were applied. The correct figures are present: 432 px baseline and 390 px after. The
PTY sizes 40×10 and 60×16 match `tools/tui-pty-check.py`. The cited test
`cancelling_or_any_other_key_drops_the_pending_commit` exists in `src/ui/app.rs`.

## 9. Limitations and not verified

- **Browser and E2E were run** on the final tree (section 5), not only on the earlier one. Their
  results are the counts recorded there. Nothing in this section is claimed beyond those runs.
- **The TUI does not clone.** Clone is a GUI and CLI operation (`gitmesh configure clone`), in line
  with the TUI's one-screen scope. The TUI has no clone path to test.
- **Only same-name upstreams.** Push tracks and pushes the branch with the same name. Differently
  named remote branches are not matched or guessed. Cloning tracks the remote's default branch only.
- **Pull without an upstream does probe the remote.** An unreachable remote is a failure; a reachable
  remote gives the same-name command or the list of remote branches, and nothing is run for you.
- **Refusal header.** CLI refusals still print the generic `project configuration is invalid:`
  header, which is existing behaviour, not new wording.
- **Clone-failure manifest protection** skips the whole manifest update when any clone in a
  multi-intent plan fails, including other repositories' manifest changes. Known limitation.
- **Unreachable remote at `configure add` is a warning**, by design, so a temporary outage does not
  block setup. It is recorded and reported.
- **Integration of diverged branches is manual** (`pull --strategy merge|rebase`). GitMesh does not
  merge, rebase, reset or force-push automatically.
- **Probe cost.** Planning runs `git ls-remote` for any remote it is given. Over HTTP the probe uses
  Git's low-speed limits; other transports have no timeout of GitMesh's own. Not measured on a slow network.
- **Platform.** Tests were run on Linux only. The hook test writes an executable `pre-receive` script
  and relies on Unix permissions; it is not expected to pass unchanged on other platforms.
- **Not verified:** the GUI against a real remote over the network (all remotes are local bare repos);
  Windows and macOS; the sync and push E2E on repositories larger than the fixtures.
- **Not recovered:** the previously overwritten `src/ui/app.rs` and `src/gui/static/app.css` (section 7).
- **Not committed, not tagged.** No version or milestone number was assigned.

## 10. Final audit pass

Focus areas and status:

1. **GUI clone journey:** browser coverage for duplicate, non-empty, unreachable, invalid, and empty
   remote, plus onboarding states. `tools/gui-clone-check.py`: 32 PASS.
2. **Onboarding consistency:** empty directory, unborn repository, missing directory, and existing
   repository are classified distinctly in the GUI (`candidateSummary`) and the CLI (refusal and
   notice text). An unborn repository is never presented as connected to a remote with history.
   The CLI now prints the plan notices in dry runs, so the no-commit notice reaches the terminal.
3. **Pull and upstream diagnostics:** no remote, unreachable remote, deleted upstream, no local commits,
   several remotes, and genuine divergence are distinct. Six lifecycle tests cover the no-upstream
   cases. The CLI shows the no-upstream guidance for skipped repositories; local-only skips stay quiet.
   Nothing is merged, rebased, reset or force-pushed automatically.
4. **Branch docs:** `docs/REPOSITORIES.md` and `docs/GUI.md` describe the same-name rule, the
   no-upstream skip with the command GitMesh does not run, and the default-branch behaviour of clone.
   The push wording in `docs/GUI.md` was corrected from "nothing to do" to "nothing to push" to match
   the code. `README.md` was checked; it has no same-name or default-branch claims beyond its links.

Failing items at the end of this pass: none in the suites listed in section 5.
