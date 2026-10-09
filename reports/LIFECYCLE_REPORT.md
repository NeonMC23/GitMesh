# Repository lifecycle: audit, implementation and verification

Status: implemented in the working tree, uncommitted, untagged. Baseline commit `b4198d8`.
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
- `tests/lifecycle.rs` (new): 21 integration tests on temp bare repositories, no network.
- `tests/cli.rs`: one test for `configure clone` through the real binary.
- Docs: `docs/REPOSITORIES.md` (new), `docs/GUI.md` (clone section), `docs/ARCHITECTURE.md`
  (section 9), `README.md` (command table and documentation index).

## 4. Changed files (this task)

`src/manage.rs`, `src/discovery.rs`, `src/ops/push.rs`, `src/ops/sync.rs`, `src/cli.rs`,
`src/main.rs`, `src/gui/server.rs`, `src/gui/static/app.js`, `src/gui/static/index.html`,
`src/gui/static/client.test.js`, `src/git/remote.rs` (new), `src/git/mod.rs`, `tests/lifecycle.rs`
(new), `tests/cli.rs`, `docs/REPOSITORIES.md` (new), `docs/GUI.md`, `docs/ARCHITECTURE.md`,
`README.md`, `reports/LIFECYCLE_REPORT.md` (new).

The working tree also contains changes from the earlier GUI/TUI redesign task, which are not
part of this report. `cargo fmt` reported no changes beyond these.

Scratch file deleted: `/home/user/tmpbuild/command.rs.fixed`, after confirming it was byte-identical
to `src/git/command.rs`.

## 5. Test commands and actual results

Run in this sandbox against the final tree.

| Command | Result |
|---|---|
| `cargo fmt --check` | exit 0 |
| `cargo clippy --all-targets -- -D warnings` | exit 0, no warnings |
| `cargo test --no-fail-fast` | exit 0: lib 353 passed; cli 16 passed; client 2 passed; lifecycle 21 passed; workflows 15 passed |
| `cargo build --release` | exit 0 |
| Manual smoke via the binary (`/tmp/audit/smoke_clone.sh`) | clone into a new dir: exit 0, `origin/main` tracked, files present; dry run changes nothing; repeat refused with exit 2 and a clear message; non-empty directory refused with exit 2 and files intact; unreachable remote refused with Git's reason and exit 2 |

The lifecycle tests cover: clone into a new directory (tracking and manifest); clone does not
change root ownership; refusal into a non-empty directory; refusal into an existing repository;
unreachable remote refused, nothing created; late clone failure not recorded; empty remote clone;
missing directory pointed to clone; connecting an existing repository keeps history; first push to
an empty remote; empty directory not initialised onto remote history; unreachable remote recorded
with a warning; local-only repository skipped, not failed; remote without upstream; repository
without commits skipped; rejected diverged push explained and not forced; unrelated histories
reported by pull and push; hook refusal reported as the remote's refusal; deleted upstream not
reported up to date; one failing repository among several, counts correct; fast-forward pull.

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

- **Browser and E2E not re-run.** The GUI clone option and the server change were checked with the
  Rust unit test and the client test (`tests/client.rs`), but not in a browser. The Playwright
  checks (`tools/gui-browser-check.py`, `tools/gui-workflow.py`) and the repository E2E script
  (`tools/repository-workflow.py`) were not re-run after this change.
- **Only same-name upstreams.** Push tracks and pushes the branch with the same name. Differently
  named remote branches are not matched or guessed. Cloning tracks the remote's default branch only.
- **Pull without an upstream does not probe the remote.** It still reports "no upstream" first.
- **Refusal header.** CLI refusals still print the generic `project configuration is invalid:`
  header, which is existing behaviour, not new wording.
- **Unreachable remote at `configure add` is a warning**, by design, so a temporary outage does not
  block setup. It is recorded and reported.
- **Integration of diverged branches is manual** (`pull --strategy merge|rebase`). GitMesh does not
  merge or rebase automatically.
- **Probe cost.** Planning runs `git ls-remote` for any remote it is given. Over HTTP the probe uses
  Git's low-speed limits; other transports have no timeout of GitMesh's own. Not measured on a slow network.
- **Platform.** Tests were run on Linux only. The hook test writes an executable `pre-receive` script
  and relies on Unix permissions; it is not expected to pass unchanged on other platforms.
- **Not committed, not tagged.** No version or milestone number was assigned.
