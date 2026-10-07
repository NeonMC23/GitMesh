# GitMesh

**One logical project. Many physical Git repositories. No new version-control system.**

GitMesh lets you keep working in a single local project directory while the project is
physically stored in several Git repositories — typically because parts of it are hosted
separately on GitHub so that teams, permissions and CI pipelines can be scoped to them.

You work normally: one directory, one tree, one status, one commit, one pull, one push,
one branch. Underneath, GitMesh runs real `git` commands in each physical repository and
reports exactly what happened in each of them.

```text
my-project/                     physical repositories      hosting
├── .gitmesh/project.toml       (the manifest: source of truth)
├── .git/                       ── root repository      ──▶ github.com/acme/my-project
├── src/                        │   (project files not assigned elsewhere)
├── docs/
├── engine/                     ── engine repository    ──▶ github.com/acme/engine
│   └── .git/
└── renderer/                   ── renderer repository  ──▶ github.com/acme/renderer
    └── .git/
```

```console
$ gitmesh status
Project 'my-project' at /home/dev/my-project (3 repositories)

REPOSITORY     ROLE      PATH        BRANCH   STATE
root           root      .           main     main, 1 modified
engine         external  engine      main     main, clean
renderer       external  renderer    main     main, 2 modified, 1 untracked

$ gitmesh commit -m "add GPU backend"
gitmesh commit

root         ✓ committed 1 file(s) [4f1a9c2]
engine       - nothing to commit
renderer     ✓ committed 3 file(s) [91b0d47]

commit: 2 succeeded, 1 skipped, 0 conflicted, 0 failed
```

## Why GitMesh exists

Splitting a project into several GitHub repositories is often required by *hosting*
concerns — access control, review workflows, release cycles, CI. But development is not
split: the developer changes several parts at once, and suddenly every task means
repeating `git status`, `git add`, `git commit`, `git pull` and `git push` in five
directories, plus remembering which branch each of them is on.

Existing solutions do not fit that need:

| Approach | Why it is not the answer here |
| --- | --- |
| Git submodules | The outer repository pins an exact commit; a change in a part is a two-step commit in two repositories, and submodules are a well-known source of confusion. |
| Monorepo tooling | Assumes the opposite direction: one repository, many projects. |
| Git subtree | Copies history into one repository, so the hosted repositories stop being independent. |
| Custom VCS | Reinvents Git, losing Git's ecosystem, hosting and tooling. |

GitMesh takes the opposite approach: keep real, independent Git repositories, and add a
thin orchestration layer that makes them behave like one project for everyday work.

## Core principles

1. GitMesh represents **one logical project**.
2. The user works from **one local project root**.
3. Users should not need to think about physical repository boundaries during normal work.
4. Physical repositories exist for hosting, collaboration and scale.
5. **Git remains the version-control engine.**
6. GitMesh does **not** reimplement Git: it drives the `git` CLI.
7. Repository boundaries are **explicitly configured** by the user, never guessed.
8. GitMesh generates and maintains the TOML manifest.
9. Every logical operation is translated into physical repository operations.
10. Only the repositories actually affected by an operation are modified.
11. Commit stages the relevant changes automatically.
12. Push is automatic across repositories.
13. Pull operates across repositories and reports conflicts instead of hiding them.
14. Branches behave as one logical branch.
15. A failure in one repository does not prevent independent repositories from finishing.
16. Every result distinguishes success, failure, skipped and conflict.
17. No user data or local modification is silently discarded.
18. GitHub is optional; the local core never requires it.
19. The implementation stays lightweight and maintainable.
20. The architecture stays extensible without becoming a Git replacement.

## Installation

```console
$ cargo build --release
$ install -m755 target/release/gitmesh ~/.local/bin/gitmesh   # or copy it anywhere on PATH
```

Requirements: Rust (edition 2021, Rust 1.74+) to build, and Git on `PATH` at runtime.
GitMesh is a single binary with no runtime dependencies beyond Git itself.

## Getting started

```console
# 1. In an existing project directory
$ cd my-project
$ gitmesh init --name my-project
$ gitmesh discover                     # which directories are Git repositories?

# 2. Mark the directories that are separate repositories
$ gitmesh configure add engine --remote git@github.com:acme/engine.git
$ gitmesh configure add renderer --git-init --remote git@github.com:acme/renderer.git
$ gitmesh configure list

# 3. Work normally
$ gitmesh status --changes
$ gitmesh commit -m "implement the new backend"
$ gitmesh pull
$ gitmesh push
```

Prefer an interface? GitMesh ships two, and both drive the same core as the CLI:

```console
$ gitmesh gui                    # graphical interface in your browser (local only)
$ gitmesh ui                     # full-screen terminal interface (alias: tui)
```

The graphical interface shows the project as **one** project — one tree, one status, one
changes list, one commit message, one branch, one pull, one push — while GitMesh keeps
managing the physical repositories underneath. It starts a small local server
(`http://127.0.0.1:7345` by default) and calls nothing outside your machine: no account,
no telemetry, no CDN. See [docs/GUI.md](docs/GUI.md).

`gitmesh ui` is the terminal equivalent and additionally supports editing the project
configuration (marking directories as repositories, renaming ids, setting remote URLs)
from inside the interface.

To see the whole workflow run end to end against real repositories and local bare remotes
(including a genuine cross-repository conflict), use the demo script:

```console
$ ./examples/demo.sh /tmp/gitmesh-demo
```

## Command reference

| Command | What it does |
| --- | --- |
| `gitmesh init [path] [--name N] [--remote URL] [--branch B]` | Create the manifest for a project (does not touch your files) |
| `gitmesh discover [--depth N] [--hidden] [--deep]` | Scan the tree, list the Git repositories in it |
| `gitmesh configure add <dir> [--id I] [--remote URL] [--git-init]` | Make a directory an independent repository |
| `gitmesh configure remove <id>` / `rename <id> <new>` / `remote <id> --url U [--set-git-remote]` | Edit the configuration |
| `gitmesh configure list` | List configured repositories |
| `gitmesh status [--changes] [--short] [--json]` | Unified project status with change ownership |
| `gitmesh commit -m MSG [--repo ID] [--path PATH] [--dry-run]` | Stage and commit in every affected repository |
| `gitmesh branch [create NAME \| delete NAME]` | Show or change the logical branch |
| `gitmesh checkout NAME [--create]` | Switch every repository to a branch |
| `gitmesh merge NAME` | Merge a branch (Git's own merge) in every repository |
| `gitmesh fetch` / `gitmesh pull [--strategy ff-only\|merge\|rebase]` | Synchronise the project |
| `gitmesh push [--dry-run] [--no-set-upstream]` | Push every repository that has commits to push |
| `gitmesh remotes` | Remotes per repository, provider and GitHub coordinates |
| `gitmesh ui` | Interactive terminal interface (can also edit the configuration) |
| `gitmesh gui [--port N] [--host A] [--allow-host NAME] [--open] [--dry-run]` | Graphical interface served locally in a browser |

Global flags: `-C <path>` (project directory), `--json` (machine-readable output),
`-v/--verbose` (include Git output and details).

Exit codes: `0` everything succeeded, `1` an operation had failures or conflicts (or a
repository of an inspection command is unavailable), `2` usage or configuration error.
Inspection commands (`status`, `branch`, `remotes`, `discover`) report conflicts and
problems in their output but exit `0` as long as the project itself could be read.

## What GitMesh guarantees

* **Nothing is discarded.** GitMesh only runs Git commands that Git itself refuses to run
  destructively: `checkout` that would overwrite local work is refused, `pull` defaults
  to `--ff-only`, branch deletion defaults to `git branch -d`, conflicts are reported and
  left in place, never resolved behind your back.
* **Nothing is guessed.** Repository boundaries come from the manifest. Discovery reports
  facts and asks; it never moves, copies or deletes files, and it never decides on its own
  that a directory should become a repository.
* **Nothing is faked.** Every physical repository keeps its own real history. There is no
  synthetic global commit and no shadow repository.
* **Nothing is hidden.** A failing repository does not stop the others, and the result
  always lists success, skipped, conflict and failure per repository.

## Documentation

| Document | Contents |
| --- | --- |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Layering, module boundaries, ownership model, safety invariants, design decisions |
| [`docs/MANIFEST.md`](docs/MANIFEST.md) | The exact manifest format, semantics and validation rules |
| [`docs/GUI.md`](docs/GUI.md) | The graphical interface: running it, the views, commit/branch/pull/push semantics, conflict presentation, safety, what is not supported |
| [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) | Build, test, lint, extend; test strategy; UI architecture |
| [`reports/DEVELOPMENT_REPORT.md`](reports/DEVELOPMENT_REPORT.md) | What was built, milestone by milestone, and why |
| [`reports/FINAL_REPORT.md`](reports/FINAL_REPORT.md) | Hardening audit, limitations, technical debt, readiness assessment |
| [`reports/GUI_REPORT.md`](reports/GUI_REPORT.md) | The graphical interface: audit, architecture, technology choice, tests, manual end-to-end validation, readiness |

## Status

GitMesh implements the complete local multi-repository workflow — discovery,
configuration, status/ownership, commit, branch/checkout/merge, fetch/pull/push — with a
command line, a full-screen terminal interface and a local graphical interface, over a
GitHub-aware (but GitHub-independent) provider foundation. It is usable
by a small team on a real multi-repository project today, with the limitations listed in
[`reports/FINAL_REPORT.md`](reports/FINAL_REPORT.md).

## Licence

MIT OR Apache-2.0.
