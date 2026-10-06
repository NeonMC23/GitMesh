# GitMesh — final development report

Scope: the complete implementation of GitMesh from an empty workspace, audited and
hardened at the end of the work. This report is the deliverable of milestone 10: current
architecture, implemented capabilities, known limitations, remaining technical debt,
security and safety considerations, recommended next steps, and an explicit assessment of
whether GitMesh is ready for two developers on a real multi-repository project.

---

## 1. Current architecture

```text
cli (clap)        ui (ratatui state machine + terminal loop)
      \                     /
       \                   /
        ops  — one logical operation → many physical Git operations
         |        commit · branch (create/checkout/delete/merge/show)
         |        sync (fetch/pull) · push · util (selection, exclusions, loop)
         |
      analyzer — unified project status, change ownership, JSON status
         |
      model · manifest · discovery · paths — project configuration & repository model
         |
      git — the only place a Git process is spawned (GitRunner / GitRepo)
```

* **Logical project:** one root repository + N external repositories, all described by
  `.gitmesh/project.toml`.
* **Ownership:** longest matching configured path; the root repository owns everything no
  external repository claims.
* **Isolation guarantee:** a repository-scoped Git command only exists on a handle that is
  bound to a working directory and whose identity has been verified against the manifest.
* **Resilience:** every project-wide operation inspects each repository, runs
  independently, and classifies its result as `✓ success`, `- skipped`, `! conflict` or
  `✗ failed`; the exit code is 0 only when no repository failed or conflicted.
* **Front ends:** the CLI and the terminal UI call the same core functions; neither
  contains Git logic.

Size: ~13.7k lines of Rust (including in-source tests) across 29 files — 24 library/CLI
modules, 2 integration test files, 3 UI modules; 6 dependencies; 2.0 MB release binary.

## 2. Implemented capabilities

| Area | Status |
| --- | --- |
| Project manifest (TOML, versioned, validated, atomic writes, upward discovery) | complete |
| Repository discovery (tree scan, nested detection, repository-root query, no modification of user files) | complete |
| Explicit configuration (assign/unassign/rename/remote, blockers for overlaps and nesting) | complete |
| Unified status (per repository: branch, detached/unborn, staged, unstaged, untracked, deleted, renamed, ahead/behind, upstream, conflicts, remotes) | complete |
| Change ownership (logical path ↔ owning repository, both directions) | complete |
| Unified commit (`gitmesh commit -m`) with automatic staged-once staging, per-repository resilience | complete |
| Unified branches (create, checkout/switch, safe delete, merge, logical branch state) | complete |
| Unified fetch/pull (fetch-then-decide, `--ff-only` default, divergence and conflict reporting, `--merge`/`--rebase` opt-in) | complete |
| Unified push (ahead-only, automatic upstream setup, rejection/auth/network classification, `--dry-run`) | complete |
| Terminal UI (project setup, tree, status, commit/pull/push/branch in one screen) | complete |
| GitHub provider foundation (URL understanding, coordinates, optional by construction) | complete |
| Machine-readable output (`--json` for status, reports, discovery, configuration, remotes) | complete |
| Safety (no destructive commands, no silent discard, no implicit commits, no repair of user files) | complete |
| Tests (unit, library integration, CLI process, UI state machine) | 209 tests, all passing |
| Validation (`fmt --check`, `clippy -D warnings`, release build) | clean |

## 3. Known limitations

Deliberate, documented boundaries rather than bugs:

1. **No repository splitting.** GitMesh configures boundaries; it does not move files
   between repositories or rewrite history. If the root repository already tracks files
   inside an external repository, GitMesh reports it and expects the user to run
   `git rm -r --cached <dir>` deliberately.
2. **No nested or partially overlapping boundaries.** A directory belongs to exactly one
   repository; two repositories cannot nest.
3. **Merges must be uniform enough to run per repository.** `gitmesh merge X` fails in
   repositories where `X` does not exist (reported explicitly) instead of inventing a
   merge source.
4. **One branch name for all repositories.** Workflows that need genuinely different
   branch names per repository are outside GitMesh's model; per-repository Git still works
   for such cases, and `gitmesh status`/`gitmesh branch` will then report the split.
5. **No clone/first-time bootstrap.** Pointing GitMesh at a project on a new machine
   requires the repositories to exist (cloned or initialised); GitMesh does not yet clone
   configured remotes for you.
6. **No sub-repository file history** (git log/blame per path across repositories) and no
   history view in the UI.
7. **Untracked-file enumeration is exhaustive** (`--untracked-files=all`). On very large
   un-ignored trees this is slower than a collapsed view; a `--no-untracked` option is a
   natural follow-up.
8. **The manifest must be committed by the user.** GitMesh does not commit
   `.gitmesh/project.toml` implicitly (it shows up as a change until committed).
9. **GitHub API features are absent on purpose** (no repository creation, no pull
   requests, no permission checks).
10. **Windows support is untested.** Path handling is written portably (no hard-coded
    separators, prefix-aware rendering, no shell interpolation), but no Windows CI exists.

## 4. Remaining technical debt

| Item | Impact | Suggested treatment |
| --- | --- | --- |
| `testkit` is compiled into the released library (test helpers, ~400 lines) | Small binary/surface cost | Move behind a `testkit` feature enabled by dev builds if the API ever becomes public-facing |
| `Analyzer::status_json` and the operation JSON are hand-written | Low; no dependency is a deliberate trade-off | Revisit only if a real consumer needs more structure |
| UI tests cover the state machine, not rendered output | Rendering regressions would not be caught | Add `ratatui` `TestBackend` snapshot tests for the three screens |
| `ProjectStatus::inconsistent_branches` compares only branch names | Repositories on the same branch name but different commits are not flagged as divergent | Compare upstream/oid per repository and warn when the "same" branch has diverged |
| Selection by subtree takes a single path | `--path a --path b` is not supported | Accept repeated `--path` arguments |
| No cancellation for long fetches in the UI | The UI blocks during `pull`/`push` | Run operations on a worker thread with progress events |
| No CI configuration in-tree | The validation sequence is manual | Add a CI workflow running `fmt`, `clippy -D warnings`, `test`, `build --release` |
| Commit message conventions per repository not supported | Same message everywhere (by design) | Optional per-repository suffix/template if users ask |

## 5. Security and safety considerations

**No destructive Git command is issued anywhere.** The codebase contains no `reset
--hard`, no `clean`, no `checkout --force`, no `branch -D` (except behind the explicit,
documented `--force` flag), no `push --force`, and no history rewriting. This is
verifiable by reading `src/ops/` and `src/git/`.

* **Local work is never silently discarded.** `pull` defaults to `--ff-only`; `checkout`
  relies on Git's refusal to overwrite modifications; a failed commit leaves changes
  staged; conflicts are left in place with the file list and the recovery commands.
* **Wrong-repository operations are structurally impossible.** Commands are bound to a
  working directory, and the directory must equal the configured repository root
  (`rev-parse --show-toplevel`), otherwise the operation fails with an explanation.
* **Boundary violations are prevented, not only detected.** The root repository stages
  with explicit exclusions for external repository directories, and unstages anything that
  slipped in.
* **No credential material.** GitMesh handles no tokens, stores no secrets, and delegates
  authentication entirely to Git; it never logs environment variables or URLs with
  embedded credentials (remote URLs are printed as configured, which is a user-provided
  string — a URL containing an embedded password would be printed; using credential
  helpers avoids that).
* **No prompts, no hangs.** `GIT_TERMINAL_PROMPT=0` unless explicitly enabled, so an
  unavailable credential fails fast with an actionable hint instead of blocking a UI.
* **No network dependency in the core.** Nothing but `git fetch/pull/push` contacts a
  remote; provider code performs no I/O.
* **Path safety.** Manifest paths must be relative, must not contain `..`, must not point
  into `.git`, and are validated on load, on save and on every programmatic change.
* **Process safety.** Git arguments are passed as vectors; no shell is involved, so
  filenames, branch names and commit messages with shell metacharacters are inert (tested
  with a message containing quotes and `$`).

## 6. Recommended next steps

Ordered by value to a team already using GitMesh:

1. **Bootstrap/clone flow** — `gitmesh open` (or `gitmesh clone-project`) that, given a
   manifest, ensures every configured repository exists locally and is on the right
   branch. This is the largest remaining gap for onboarding a second developer.
2. **Manifest commit prompt** — after configuration changes, offer to commit
   `.gitmesh/project.toml` in the root repository (explicit user action, not implicit).
3. **Divergence-aware branch checks** — extend the branch consistency check from names to
   commit state (see technical debt).
4. **UI polish** — `TestBackend` snapshot tests, a repository filter, and a detail pane
   showing the last operation's full Git output.
5. **GitHub provider, opt-in** — `gitmesh github status` (remote reachability, default
   branch, permissions) with a token read from the environment, gated so the local core is
   unaffected; then optionally repository creation for new external repositories.
6. **Splitting guidance** — a documented, semi-automated flow for moving a directory out of
   the root repository (`git subtree split`-style guidance plus the exact commands), never
   automatic.
7. **CI and packaging** — GitHub Actions running the validation sequence on Linux and
   macOS, `cargo dist`/Homebrew tap for binary distribution.

## 7. Is GitMesh ready for two developers on a real multi-repository project?

**Yes, for the local workflow GitMesh covers — with the caveats below.**

What works today, verified against real repositories and bare remotes:

* a project with a root repository and several external repositories can be configured
  once (CLI or UI) and reopened later from the manifest on disk;
* one `gitmesh status` shows the whole project and says which repository owns every change;
* one `gitmesh commit -m` stages and commits in every affected repository with the same
  message, leaving untouched repositories alone, and never losing work when a repository
  fails;
* one `gitmesh branch`/`checkout` brings every repository onto the same branch, refusing
  (rather than discarding) where local changes would be affected, and reporting splits;
* one `gitmesh pull`/`push` synchronises the project, reports conflicts and divergences
  per repository, continues past failures, and never claims success when something failed;
* failures are always attributed to a repository with an actionable message, and partial
  states are visible instead of hidden.

What the two developers must still handle themselves today:

1. **Onboarding requires cloning the repositories** (each repository can be cloned with
   ordinary Git; GitMesh then recognises the layout). The convenience flow is not built.
2. **Repository splitting is manual.** GitMesh does not decide that a directory becomes a
   repository, nor does it migrate history; that decision and the `git rm --cached`
   cleanup on the root repository are deliberate manual steps.
3. **GitHub-side setup is manual.** Creating repositories, setting permissions and
   protecting branches happens on GitHub; GitMesh records and configures remote URLs.
4. **Conflicts must be resolved with Git inside the affected repository** (by design),
   after GitMesh tells you exactly which repository and which files are involved.
5. **Windows is untested**, and there is no CI yet.

For a pair of developers working daily in the same logical project across a handful of
repositories, GitMesh removes the repetitive N-repository bookkeeping that motivated the
project and does so without hiding Git or risking their work. The remaining gaps are
onboarding and GitHub-side automation — both additive, neither blocking.
