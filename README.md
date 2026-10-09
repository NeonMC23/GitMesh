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
# 0. Create the project from an ordinary folder (or use the graphical wizard below)
$ gitmesh init . --name my-project
#    ...or, with a remote: recorded in the manifest, and configured in Git when asked
$ gitmesh init . --name my-project --remote git@github.com:acme/my-project.git --add-git-remote

# 1. In an existing project directory
$ cd my-project
$ gitmesh init --name my-project
$ gitmesh discover                     # which directories are Git repositories?

# 2. Mark the directories that are separate repositories
$ gitmesh configure add engine --remote git@github.com:acme/engine.git
$ gitmesh configure add renderer --git-init --remote git@github.com:acme/renderer.git
$ gitmesh configure list

# 3. Work normally — and keep the layout alive while you develop
$ mkdir new-module                          # an ordinary directory appears
$ gitmesh configure add new-module --git-init --untrack-from-root --dry-run
$ gitmesh configure add new-module --git-init --untrack-from-root
$ gitmesh status --changes
$ gitmesh commit -m "implement the new backend"
$ gitmesh pull
$ gitmesh push
```

Every configuration change is planned before it is made: `--dry-run` prints exactly the
changes and the steps that would run, and nothing is written. `--untrack-from-root` stops
the root repository from tracking the files that now belong to the new repository (the
files stay on disk — nothing is ever deleted or moved).

Prefer an interface? GitMesh ships two, and both drive the same core as the CLI:

```console
$ gitmesh gui                    # graphical interface in your browser (local only)
$ gitmesh ui                     # full-screen terminal interface (alias: tui)
```

The graphical interface shows the project as **one** project — one tree, one status, one
changes list, one commit message, one branch, one pull, one push — while GitMesh keeps
managing the physical repositories underneath. It starts a small local server
(`http://127.0.0.1:7345` by default) and calls nothing outside your machine: no account,
no telemetry, no CDN.

It also **creates** a project: point it at an ordinary folder, let it scan the structure,
tick the directories that should be separate repositories, configure their names and
optional remotes, review the complete plan *and* the exact `.gitmesh/project.toml` it will
write, confirm it, and watch the setup run step by step — then keep working in the same
interface, with an optional first commit and push through the ordinary GitMesh operations.
Nothing is created before you confirm, an existing repository or manifest is never
replaced silently, and remotes are optional: a fully local project is a first-class
project. See [docs/GUI.md](docs/GUI.md).

The same interface **manages** the project afterwards, in the *Repositories* tab: check a
directory, review the complete plan (including the exact `.gitmesh/project.toml` it would
write), confirm it, watch the steps run, and read the evidence for every change. A new
directory can be turned into a managed repository while the project is open; an existing
Git repository is *adopted* with its history and remote left exactly as they are; a
repository's logical id or recorded remote can be changed; and a repository can be
**removed from GitMesh without its directory, its `.git`, its history or its remote being
touched** — a removal that hands files back to the root repository says so and asks for
confirmation first. Repository management is done with `gitmesh configure` on the command
line or in the graphical interface — both drive one service and one plan. The terminal
interface (`gitmesh ui`) is deliberately limited to the everyday workflow.

To see the whole workflow run end to end against real repositories and local bare remotes
(including a genuine cross-repository conflict), use the demo script:

```console
$ ./examples/demo.sh /tmp/gitmesh-demo
```

## Command reference

| Command | What it does |
| --- | --- |
| `gitmesh init [path] [--name N] [--remote URL] [--add-git-remote] [--git-init] [--branch B] [--force]` | Create the manifest for a project (does not touch your files). `--remote` only *records* the URL; `--add-git-remote` also points `origin` at it, and is what allows replacing an existing one |
| `gitmesh discover [--depth N] [--hidden] [--deep]` | Scan the tree, list the Git repositories in it |
| `gitmesh configure add <dir> [--id I] [--remote URL] [--git-init] [--branch B] [--untrack-from-root] [--dry-run]` | Make a directory an independent repository (planned first; `--untrack-from-root` stops the root repository from tracking its files) |
| `gitmesh configure remove <id> [--confirm-takeover] [--dry-run]` | Remove a repository from the configuration: its directory, `.git`, history and remote are kept. `--confirm-takeover` confirms that files tracked by the root repository go back to it |
| `gitmesh configure rename <id> <new>` / `remote <id> --url U [--set-git-remote] [--clear]` | Change the logical identity, or record / configure / clear a remote |
| `gitmesh configure list` | List configured repositories |
| `gitmesh status [--changes] [--short] [--json]` | Unified project status with change ownership |
| `gitmesh commit -m MSG [--repo ID] [--path PATH] [--dry-run]` | Stage and commit in every affected repository |
| `gitmesh branch [create NAME \| delete NAME]` | Show or change the logical branch |
| `gitmesh checkout NAME [--create]` | Switch every repository to a branch |
| `gitmesh merge NAME` | Merge a branch (Git's own merge) in every repository |
| `gitmesh fetch` / `gitmesh pull [--strategy ff-only\|merge\|rebase]` | Synchronise the project |
| `gitmesh push [--dry-run] [--no-set-upstream]` | Push every repository that has commits to push |
| `gitmesh remotes` | Remotes per repository, provider and GitHub coordinates |
| `gitmesh configure clone <dir> --remote <url>` | Clone a remote into a new (missing or empty) directory and record it |
| `gitmesh ui` | Interactive terminal interface for the everyday workflow: stage, commit, pull, push (see [docs/TUI.md](docs/TUI.md)) |
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
| [`docs/REPOSITORIES.md`](docs/REPOSITORIES.md) | Onboarding a directory, cloning a remote, connecting remotes and upstreams, and what the sync errors mean |
| [`docs/GUI.md`](docs/GUI.md) | The graphical interface: running it, the views, commit/branch/pull/push semantics, conflict presentation, safety, what is not supported |
| [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) | Build, test, lint, extend; test strategy; UI architecture |
| [`reports/DEVELOPMENT_REPORT.md`](reports/DEVELOPMENT_REPORT.md) | What was built, milestone by milestone, and why |
| [`reports/FINAL_REPORT.md`](reports/FINAL_REPORT.md) | Hardening audit, limitations, technical debt, readiness assessment |
| [`reports/GUI_REPORT.md`](reports/GUI_REPORT.md) | The graphical interface: audit, architecture, technology choice, tests, manual end-to-end validation, readiness |
| [`reports/REPOSITORY_REPORT.md`](reports/REPOSITORY_REPORT.md) | Managing repositories after creation: audit findings, the reuse of the plan-driven architecture, the management service, safety decisions, tests, end-to-end validation, limitations |
| [`reports/SETUP_REPORT.md`](reports/SETUP_REPORT.md) | Integrated project creation and repository setup: audit findings, the plan-driven setup service, the wizard, first publish, tests, end-to-end validation, limitations |

## Status

GitMesh implements the complete local multi-repository workflow — project creation,
discovery, configuration, status/ownership, commit, branch/checkout/merge,
fetch/pull/push — with a command line, a minimal full-screen terminal interface and a local
graphical interface, over a GitHub-aware (but GitHub-independent) provider foundation.
Creating a project from an ordinary folder is part of the flow now: `gitmesh init` and the
graphical wizard are two front ends of the same plan-driven setup service, which previews
every change, writes the manifest, and can hand over to a first commit and push.

The project's repository layout is no longer fixed at creation time. `gitmesh configure`
and the GUI's *Repositories* tab manage it afterwards through one plan-driven service
(`src/manage.rs`): a directory becomes a repository (initialised or adopted), a remote is
recorded, configured or replaced only when that is explicitly asked for, a logical id is
renamed without touching a directory, and a repository leaves the configuration while its
directory, `.git`, history and remote stay exactly where they are. Everything is
inspected, planned, reviewed and confirmed before it runs, and every applied change is
proven afterwards by evidence and a validation of the reopened project.

It is usable by a small team on a real multi-repository project today, with the
limitations listed in [`reports/FINAL_REPORT.md`](reports/FINAL_REPORT.md),
[`reports/SETUP_REPORT.md`](reports/SETUP_REPORT.md) and
[`reports/REPOSITORY_REPORT.md`](reports/REPOSITORY_REPORT.md).

## Licence

MIT OR Apache-2.0.
