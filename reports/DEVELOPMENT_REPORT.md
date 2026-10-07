# GitMesh development report

Milestone-by-milestone record of what was found, what was built, why, and what remains
open. The final, consolidated assessment (limitations, technical debt, security, next
steps, readiness) is in [`FINAL_REPORT.md`](FINAL_REPORT.md).

Origin: the ten milestone prompts were executed in sequence in one working session. Each
milestone below records the audit it started with, the implementation, the design
decisions, and the verification that was actually run.

The work that followed milestone 10 — the first real **graphical interface** — is recorded
separately in [`GUI_REPORT.md`](GUI_REPORT.md), with its own audit, architecture, tests,
manual validation and readiness assessment.

---

## Milestone 1 — Audit, architecture and project foundation

### Audit findings

The workspace (`/home/user`) was **empty**: no GitMesh repository, no source tree, no
build files, no manifest, no Git state, no documentation. Nothing had to be preserved or
adapted, and no assumption from earlier descriptions applied.

The environment was audited before writing code:

| Item | Finding | Consequence |
| --- | --- | --- |
| Git | `git 2.47.3` at `/usr/bin/git` | The Git-CLI approach is viable and testable |
| Rust | **not installed** | Installed the stable toolchain (`rustc 1.99.0`) outside the workspace |
| rustfmt / clippy | not installed | Added as components (required by the milestone's validation list) |
| Network | crates.io reachable (`index.crates.io` HTTP 200) | Dependencies can be resolved |
| Workspace policy | only `/home/user` persists; dependency/build directories are excluded | Toolchain installed outside the workspace; no `Cargo.lock`-independent caches inside it |

Because there was no prior implementation, the "detect inconsistencies / partial
implementations / regressions" part of the audit had no findings. The audit instead
produced the **architectural baseline** that later milestones were checked against.

### Implemented

* Crate `gitmesh` (library + binary `gitmesh`), edition 2021, MSRV 1.74.
* Module skeleton: `git`, `model`, `manifest`, `discovery`, `analyzer`, `ops`, `paths`,
  `error`, and a CLI foundation able to run and be tested.
* `git::command` — `GitRunner` / `GitRepo`: the only place a Git process is spawned.
  Working directory always explicit (`-C`), stable output configuration
  (`core.quotepath=false`, `color.ui=false`, `--no-pager`, `LC_ALL=C`), prompts disabled
  by default (`GIT_TERMINAL_PROMPT=0`), and `verify_identity()` which refuses to operate
  unless the directory really is the configured repository's root.
* `error.rs` — contextual errors (repository, path, command, Git stderr) and an
  `is_configuration_error()` classification that drives the exit codes.
* `paths.rs` — lexical normalisation, project-relative conversion, manifest path
  validation, slash rendering.
* Unit tests for path handling and the Git abstraction, including the identity guard.

### Design decisions

* **Git CLI only.** No libgit2, no Git reimplementation; behaviour must be identical to
  the user's own terminal.
* **Repository-scoped commands are only reachable through a bound handle.** There is no
  API that runs a repository command "in the current directory", which makes "operating
  on the wrong repository" a compile-time impossibility rather than a discipline.
* **Foundations before features.** Only abstractions needed by the next milestone were
  added (manifest, discovery, ops came later with their own milestones).

### Verification

`cargo fmt`, `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build
--release` — all clean at the end of every subsequent milestone as well.

---

## Milestone 2 — Project manifest and repository layout model

### Audit findings

Reviewed the foundation for inconsistencies: no duplicated path logic (one `paths`
module), no Git knowledge outside `git/`, and no concrete project model yet beyond
`GitMeshProject`/`PhysicalRepository` placeholders. Adaptations made during this
milestone: `physical repository role/absolute path` were formalised, and validation was
centralised so that *every* construction path (parse, UI, tests) runs the same rules.

### Implemented

* `manifest/schema.rs` — TOML schema (`version`, `name`, `[root]`, `[[repositories]]`).
* `manifest/validation.rs` — structural validation returning **all** problems at once:
  exactly one root, id format/uniqueness, path uniqueness/non-overlap, escaping paths,
  remote URL shape, duplicate remotes, branch-name validity, root/path agreement.
* `manifest/mod.rs` — load/save (atomic write), upward project discovery, render/parse
  round-trip, conversion between the file and the internal model.
* `model.rs` — the central internal model plus **longest-match ownership**.
* Tests for valid and invalid manifests (duplicates, overlaps, escaping paths, bad URLs,
  bad branches, unsupported version, round-trips, discovery from nested directories).

### Design decisions

* `[root]` is optional; the root repository always exists conceptually, so a project can
  be configured before `git init` has run.
* Root-as-ancestor-of-externals is explicitly allowed (that is GitMesh's whole model);
  any other nesting is rejected as ambiguous.
* Absolute paths never appear in the manifest; they are derived from the project root, so
  a project can be cloned elsewhere.
* The manifest is plain TOML and belongs in the root repository — it is configuration, not
  a hidden database.

The exact format, semantics and error catalogue are specified in `docs/MANIFEST.md`.

---

## Milestone 3 — Repository discovery and project configuration workflow

### Audit findings

The milestone-2 model and manifest behaved as intended, but three things were missing for
real usage: a read-only scanner, explicit assignment operations and a guarded "is this
directory a repository root?" check. The discovery module was added to fill exactly those
gaps and nothing else.

### Implemented (all read-only; no user file is ever moved or modified)

* `discovery::scan_project` — bounded-depth directory tree with repository markers,
  `.git`/metadata/`node_modules` awareness, nested-repository detection, notices.
* `discovery::find_repository_root` — asks Git for the top level of a path.
* `discovery::check_assignment` — reports blockers (missing directory, already assigned,
  nested inside an assigned repository, containing an assigned repository) versus
  warnings (not a Git repository yet, `--git-init` needed).
* `discovery::assign_repository` / `unassign_repository` / `rename_repository` /
  `set_repository_remote` — pure configuration operations returning a re-validated
  project, then persisted by the caller. They perform filesystem changes only when
  explicitly asked (`--git-init`, `--set-git-remote`).
* `discovery::verified_repo` — the single way to obtain a Git handle for a configured
  repository, with identity verification built in.
* Tests: one root, root + one nested, root + several, several levels deep, overlapping
  definitions, missing repositories, non-Git directories, empty directories, plus a test
  asserting a scan changes nothing on disk.

### Design decisions

* **Facts vs decisions.** Discovery reports what exists; only explicit user actions change
  the configuration. Automatic detection never decides the user's architecture.
* Directory ids default to the directory name with `-2`, `-3` suffixes on collision.
* `unassign` never touches files or history — it is a configuration-only operation, said
  so in its output.

---

## Milestone 4 — Real Git status, diff and change ownership

### Audit findings

The first genuine bug of the project was found here: the parser for
`git status --porcelain=v2` assumed newline-separated headers inside NUL-separated
records. An empirical check against Git 2.47 proved that `-z` NUL-terminates **every**
record (headers included), that a nested repository is reported as a single `sub/`
untracked entry, and that `git add -A` **fails** when a nested repository has no commits
("does not have a commit checked out"). All three findings changed the implementation:

* header parsing became NUL-based (and tolerates newlines for non-`-z` output),
* `--untracked-files=all` is used so ownership is computed per file,
* the root repository stages with explicit exclusions (`:(exclude)<dir>`) and audits the
  index afterwards, unstaging anything that belongs to another repository.

A second real bug surfaced in the tests: `branch.oid (initial)` (an unborn branch) was
being parsed as a normal branch with a commit. GitMesh now reports `Head::Unborn`, which
push/pull/branch logic depends on.

### Implemented

* `git/status.rs` — full porcelain v2 model: staged/unstaged/untracked, added, modified,
  deleted, renamed (with original path), copied, type changes, all seven conflict codes,
  branch/upstream/ahead/behind, detached HEAD, unborn branches, remotes with classification.
* `analyzer.rs` — one `ProjectStatus` over all repositories (root first), `OwnedChange`
  list with both repository-relative and logical paths, ownership resolution with
  no-escape checks, notices for missing repositories, in-progress operations and
  "the root repository still tracks files inside an external repository", and a complete
  JSON representation (`Analyzer::status_json`).
* Root-status filtering: entries that live inside a configured external repository are
  attributed to that repository only, never to the root.
* CLI: `gitmesh status [--changes] [--short] [--json]`.
* Tests against real repositories for clean/modified/multiple/untracked/deleted/
  staged+unstaged/conflicts/missing/wrong-remote/detached-state scenarios.

### Design decisions

* **Ownership from configuration, not from the filesystem.** The root's view of a nested
  repository directory is filtered out; that is why a fresh project reads as *clean*
  instead of showing `engine/` as an untracked gitlink.
* **Machine-readable results are the internal types themselves**, plus a JSON rendering
  for scripting — no parallel "output-only" model that could drift.
* Conflicts are a first-class state, not a failure: they are counted and reported as `!`.

---

## Milestone 5 — Unified staging and logical commit orchestration

### Audit findings

The analyzer's ownership model was sufficient, but the root-repository staging rule
(exclusions) and the "root status must not report external content" rule had to be reused
by `commit` to guarantee that a repository only ever stages its own files. Both were moved
into `ops::util` and shared, rather than duplicated.

### Implemented

* `gitmesh commit -m MSG` — analyse → stage (`git add -A -- .` with exclusions) → real
  `git commit` per affected repository → summary. Clean repositories are skipped; a
  failing repository (hook, missing directory) never stops the others; conflicts block the
  commit in that repository only.
* Per-repository safety: identity verification before any command; a post-staging audit
  that unstages files belonging to another repository; failures leave changes **staged**,
  never discarded, and say so.
* Selection: `--repo <id>` (repeatable) and `--path <subtree>`, both validated so a typo
  produces a clear error instead of a silent no-op.
* `--dry-run` reports what would be committed without touching anything.
* Tests: one/multiple repositories, untracked, deleted, empty repository (no commits yet),
  failure in one repository while others succeed, no changes anywhere, message handling
  (empty message rejected, special characters preserved).

### Design decisions

* **No fake global commit.** One logical message, N real commits in N real histories.
* **Never stage across boundaries**, including the gitlink case.
* Exit code 0 only when no repository failed or conflicted; partial success is reported as
  such and exits 1.

---

## Milestone 6 — Unified branch, checkout and merge

### Audit findings

Re-checked ownership, status and commit against the branch layer: all three still matched
the logical-single-repository model. One gap: `App`/`ops` had no notion of "the project's
branch" — the reference branch concept (the root repository decides; outliers are
reported) was added so the project can never silently look consistent while repositories
disagree.

### Implemented

* `gitmesh branch` — reports the logical branch state, plus an explicit conflict outcome
  when repositories are on different branches.
* `gitmesh branch create NAME` / `delete NAME [--force]` — create in every repository;
  delete only where Git can delete safely (`git branch -d`), never the current branch.
* `gitmesh checkout NAME [--create]` — switch every repository; Git itself refuses to
  overwrite local modifications, and GitMesh reports that refusal verbatim together with
  the advice to commit or stash.
* `gitmesh merge NAME` — Git's own merge per repository; conflicts are reported as
  conflicts with the file list, left in place, and never auto-resolved or aborted.
* Safety: repositories with a merge/rebase/cherry-pick in progress are left alone;
  invalid branch names are rejected before any repository is touched; unborn repositories
  report "no commits yet" and are skipped for creation.
* Tests: creation/checkout everywhere, dirty repositories (change kept, split state
  reported), missing branches, merge conflicts, safe-delete refusals, partial failure,
  detached HEAD, dry-run, selection/exclusions.

### Design decisions

* **A logical branch is uniform.** Where uniformity cannot be achieved, the report says
  which repositories did not end up on the branch — no silent partial states.
* **Merge across repositories is not "all-or-nothing".** Repositories that can merge do;
  the conflicted one is reported.
* Delete stays safe by default (`-d`); `--force` is explicit and never implied.

---

## Milestone 7 — Unified pull, fetch and synchronisation

### Audit findings

A real correctness problem was found: `ahead`/`behind` from `git status` are only as fresh
as the last fetch, so divergence detection on stale data is unreliable (a repository that
had actually diverged looked merely "behind", and `pull --ff-only` then failed with a raw
Git error instead of an explanation). Pull now fetches first, then decides. Transport
error classification was also reordered after a real case showed that a missing local
repository path produces both "does not appear to be a git repository" *and* "could not
read from remote repository" — the "not found" case must win, otherwise the user gets a
misleading authentication hint.

### Implemented

* `gitmesh fetch` — fetch/prune per repository; no remote → skipped with an explanation.
* `gitmesh pull [--strategy ff-only|merge|rebase]` — fetch, then:
  * already up to date → success;
  * diverged (ff-only) → failure with a clear "GitMesh will not merge or rebase
    automatically" explanation;
  * uncommitted changes → refusal that explicitly states local work is never discarded;
  * conflicts → conflict outcome with the file list, left in place;
  * unreachable remote → failure with an actionable hint (network, authentication,
    repository not found, credentials prompting disabled).
* Tests: all up to date, different repositories needing different updates, one repository
  failing, conflicting changes, diverged branches, local modifications, missing upstream,
  skipped repositories without commits, dry-run, transport-hint classification.

### Design decisions

* **Fetch before judging.** Correctness beats saving one network round trip.
* **`--ff-only` is the default.** A routine pull must never create a surprise merge
  commit; `--merge`/`--rebase` are explicit opt-ins.
* **Conflicts are left visible.** Aborting automatically would hide the state; GitMesh
  reports the files and the exact recovery commands.

---

## Milestone 8 — Unified push and remote synchronisation

### Audit findings

Reviewed commit/branch/sync for regressions. Two behaviours were corrected while
implementing push: (a) a repository with nothing to push must be skipped *before* its
remote is contacted, so a broken remote does not pollute an otherwise clean result; (b) a
repository without upstream must not be reported as "nothing to push" — GitMesh sets the
upstream (`git push --set-upstream origin <branch>`) by default, because that is what the
user means by "push my project".

### Implemented

* `gitmesh push [--dry-run] [--no-set-upstream]` — push only repositories that are ahead;
  skip the rest; continue after failures; never claim success when one repository failed.
* Handled and classified: no remote, no upstream (auto-set), detached HEAD, no commits yet,
  non-fast-forward rejection (with "run `gitmesh pull` first, nothing local changed"),
  authentication failure, network failure, remote repository not found.
* `--dry-run` maps to `git push --dry-run` and prints remote name, URL and commit count
  for troubleshooting.
* Detail output: Git's own messages plus the classified hint, per repository.
* Tests with local **bare repositories**: single/multiple pushes, upstream setup, rejection,
  detached HEAD, missing remote, partial failure, dry-run, nothing to push, selection.

### Design decisions

* **Sequential pushes.** A handful of repositories; ordered, attributable output is worth
  more than concurrency, and it avoids concurrent pushes to the same remote.
* **Remote state is never faked:** `--dry-run` uses Git's own dry-run.
* Only `upstream` (or, when unset, the configured remote) is pushed — GitMesh never pushes
  to unrelated remotes.

---

## Milestone 9 — GUI/TUI and GitHub provider foundation

### Audit findings

Before building the UI, the CLI/core surface was reviewed: every operation returns an
`OperationReport`, configuration changes flow through `discovery` + `manifest`, and status
is available as structured data — enough to expose through a UI **without any new Git
logic**. One addition was required for honesty in the interface: a deterministic project
branch (`ProjectStatus::reference_branch`, root repository decides; outliers listed), so
the UI can show `main (engine on side)` instead of picking a majority branch.

### Implemented

* `gitmesh ui` (`gitmesh tui`) — full-screen terminal interface:
  * **Setup screen:** choose a directory, inspect the tree, mark/unmark directories as
    independent repositories, rename ids, set remote URLs, save `.gitmesh/project.toml`.
  * **Project screen:** one tree (with repository ownership marked), one unified status
    table, one change list showing which repository owns each change, an activity log, and
    single-key operations: commit (`c`), pull (`p`), push (`P`), fetch (`f`), refresh (`s`),
    new branch/checkout (`n`/`b`), merge (`m`), unassign (`a`), rename (`i`), remote (`u`),
    dry-run toggle (`d`), reload (`r`), help (`?`), quit (`q`).
  * Modal text input for messages, branches, ids, URLs and directories.
  * Without a TTY (pipes, CI) the command prints the equivalent CLI commands instead of
    failing or hanging.
* `providers/` — `Provider` trait + `GitHubProvider`: matching, owner/repository
  extraction from scp-like and URL forms, web/SSH/HTTPS URLs, `RemoteRef`/`GitHubRepo`.
* `gitmesh remotes [--json]` — per-repository remotes with provider coordinates.

### Design decisions

* **UI contains no Git logic.** It calls the exact functions the CLI calls; the state
  machine (`ui::app`) is terminal-independent, which is what makes the workflows testable
  and prevents divergence between front ends.
* **Physical boundaries visible, not dominant.** The tree marks repository roots and the
  status lists per-repository state, but the primary objects on screen are the project
  tree, the project status and the project branch.
* **GitHub stays optional.** No HTTP client, no tokens, no network dependency in the local
  core; provider code is pure URL understanding.

---

## Milestone 10 — Complete audit, hardening, integration and release readiness

### Audit findings and fixes

The full tree was re-read module by module against the twenty core principles. Findings:

1. **Correctness**
   * `Head::Unborn` detection (fixed in milestone 4) was re-verified end to end.
   * Pull freshness (milestone 7) re-verified with a real divergence scenario.
   * Transport-hint ordering (milestone 7) re-verified.
   * `to_slash` rendered absolute paths as `//tmp/...` (double slash) — found while reading
     `status --json` output and fixed, with tests for absolute, trailing-slash, `./` and
     root paths.
   * Unborn repositories are now also skipped (not failed) for branch creation and push.
2. **Duplicated logic**
   * The "root must exclude external repositories" rule existed in two places (commit
     staging and root status filtering) with different implementations; the staging
     exclusions and the ownership filter now live together in `ops::util` and are used by
     the analyzer, commit and tests.
   * The test fixture previously used a plain `git add -A` that could stage nested
     repositories as gitlinks; it now uses the product's exclusion rule.
3. **Unsafe behaviour** — none found beyond the fixed issues: no destructive Git command
   is issued anywhere; every mutating operation was re-read to confirm it can only be
   run after identity verification.
4. **Incomplete implementations** — none found; every CLI command is implemented (no
   stubs, no `todo!()`), and `--dry-run` exists for every mutating operation.
5. **Unnecessary complexity** — removed the unused `error::io_context` and manifest
   `read_to_string` helpers, the `tempfile` dev-dependency (replaced by a 40-line
   `testkit::TempDir`), and the `--json`-only JSON dependency (replaced by `src/json.rs`).
   Net dependency count: 6 (`clap`, `serde`, `toml`, `thiserror`, `ratatui`, `crossterm`).

### Integration suite

Added `tests/workflows.rs` (14 scenarios) and `tests/cli.rs` (10 process-level scenarios)
covering the requested list: initialise, configure root + externals, modify files across
repositories, unified status, commit, branch create, checkout, merge, pull, conflict
reporting, push, partial network failure, partial Git failure (failing hook), missing
repository, incorrect remote, dirty repository, untracked files, repository with no
changes, multiple repositories modified at once, adding a new external repository, invalid
project configuration, and restarting GitMesh against an existing project. Idempotency is
asserted for commit, branch creation, fetch and push; a test asserts that an unrelated
sibling repository is never touched.

### Documentation and usability

* `README.md` (overview, principles, command table, guarantees), `docs/ARCHITECTURE.md`
  (layering, ten safety invariants, design decisions), `docs/MANIFEST.md` (schema and the
  complete validation catalogue), `docs/DEVELOPMENT.md` (test layers, how to add a command,
  troubleshooting).
* `examples/demo.sh` — a runnable end-to-end walkthrough on real repositories and local
  bare remotes: configure a three-repository project, commit across it, branch and check
  out everywhere, push, provoke a genuine cross-repository conflict with a second clone,
  then resolve it. Verified against the release binary; it is the fastest way for a new
  developer to see the intended workflow, and it doubles as a manual smoke test.
* The exit-code contract was made explicit in the README after the audit found the wording
  ambiguous: inspection commands (`status`, `branch`, `remotes`, `discover`) report
  conflicts in their output and still exit `0`, while mutating operations exit `1` when any
  repository failed or conflicted. Verified by hand: `commit` with nothing to do → `0`,
  invalid configuration → `2`, usage error → `2`.

### Verification actually run

```console
$ cargo fmt --all -- --check                        # clean
$ cargo test                                        # 209 tests: 185 + 10 + 14, all pass
$ cargo clippy --all-targets -- -D warnings         # clean, no warnings
$ cargo build --release                             # 2.0 MB binary
```

Every CLI test and every hand-run command is a **new process**, so "restart GitMesh and
reopen the project" is exercised on every invocation; it was additionally re-verified by
hand from a nested directory (`cd shop/engine && gitmesh status`) and with the global
`-C` flag. The isolation property was re-verified after a full project-wide commit: an
unrelated sibling repository next to the project kept its exact `HEAD` and a clean working
tree.

Beyond the test suite, the release binary was exercised by hand on a temporary
three-repository project with bare remotes: `init`, `discover`, `configure add/list`,
`status --changes`, `commit`, `branch create`, `checkout`, `merge` (including a conflict
left in place), `pull` (up-to-date, diverged, and conflicting), `push` (success, nothing to
push, dry-run, genuine SSH authentication failure of one repository while the others
pushed), `status --json`, and `ui` without a TTY. Partial failures were reproduced and
reported exactly as specified.
