# GitMesh

**One logical project. Many physical Git repositories. No new version-control system.**

GitMesh lets you work in one local project directory while the project is stored in several
Git repositories — usually because parts of it are hosted separately (for example on GitHub)
so that access, review and CI can be scoped to each part.

You keep working normally: one directory, one status, one commit, one pull, one push. GitMesh
runs real `git` commands in each physical repository and reports, per repository, what happened.
It does not reimplement Git.

```text
my-project/                      physical repositories         hosting
├── .gitmesh/project.toml        (the manifest: the source of truth)
├── .git/                        root repository          ──▶ github.com/acme/my-project
├── src/                         │  (files not owned by a child repository)
├── engine/                      engine repository        ──▶ github.com/acme/engine
│   └── .git/
└── renderer/                    renderer repository      ──▶ github.com/acme/renderer
    └── .git/
```

```console
$ gitmesh status
Project 'my-project' at /home/dev/my-project (3 repositories)

REPOSITORY  ROLE      PATH      BRANCH  STATE
root        root      .         main    main, 1 modified
engine      external  engine    main    main, clean
renderer    external  renderer  main    main, 2 modified, 1 untracked

$ gitmesh commit -m "add GPU backend"
gitmesh commit

root         ✓ committed 1 file(s) [4f1a9c2]
engine       - nothing to commit
renderer     ✓ committed 3 file(s) [91b0d47]

commit: 2 succeeded, 1 skipped, 0 conflicted, 0 failed
```

(Output abridged; the hashes shown are examples.)

## Core concepts

| Concept | Meaning |
| --- | --- |
| **Project** | The directory that contains `.gitmesh/project.toml`. Its name and configuration live there. |
| **Root repository** | The Git repository at the project root, if there is one. It owns every file that no child repository claims. |
| **Child (external) repository** | A Git repository in its own directory below the root, registered in the manifest with an id and a path. Its files belong to it alone. |
| **Manifest** | `.gitmesh/project.toml`, written by GitMesh. Repository boundaries come only from the manifest — GitMesh never guesses them from the presence of a `.git` directory. See [docs/MANIFEST.md](docs/MANIFEST.md). |
| **Ownership** | A path belongs to the repository with the longest matching path. Child content never appears in the root's commits. See [docs/REPOSITORIES.md](docs/REPOSITORIES.md). |
| **Logical operation** | A command such as `commit`, `pull` or `push` is run in each affected repository, one after another. Each repository gets its own real commit, pull or push. |
| **Outcome** | Every repository reports `✓` success, `-` skipped, `!` conflict or `✗` failed. A failure in one repository never stops the others, and the result is never reported as success when something failed. |

GitMesh never creates a synthetic global commit, never force-pushes, never resolves conflicts for
you, and never discards local work. Configuration changes are planned and shown before they run.

## Quick start

You need **Git** on your `PATH` (GitMesh runs it) and, to build, **Rust 1.88 or newer** with a
**C linker** (`cc`). You do not need to know Cargo.

```console
$ cd gitmesh               # the source tree
$ ./build.sh               # builds the release executable
$ target/release/gitmesh --help
```

`build.sh` works from any directory, takes no configuration, does not start GitMesh, and prints the
path of the executable when it finishes. Nothing is installed system-wide.

* **Rust.** If a suitable Rust is already installed (system packages or rustup), `build.sh` uses it.
  If none is installed, it stops and asks before installing. In a script, use
  `./build.sh --install-rust`. The installer is the official rustup-init, verified against its
  published SHA-256 checksum; Rust is installed for your user only, and shell start-up files are not
  edited. A Rust that is installed but too old or broken is reported and never overwritten.
* **C linker.** If `cc` is missing, `build.sh` stops before any download and prints the command for
  your system (for example `sudo dnf install gcc` or `sudo apt install build-essential`). It never runs
  package managers itself.
* **Network and disk.** The first build downloads crates from crates.io (and rustup from
  static.rust-lang.org if Rust is installed by `build.sh`). Measured here: the stable Rust toolchain
  with the minimal profile plus `rustfmt`/`clippy` took about 0.6 GB, the crate cache about 0.1 GB, and
  the release build about 0.17 GB. Running the full test suite builds a debug copy, about 1.1 GB more.
* **Permission denied.** If `./build.sh` reports `Permission denied`, the executable bit was lost
  (some archives and copies drop it). Run `bash build.sh`, or restore it with `chmod +x build.sh`.

`cargo build --release --locked` produces the same executable if you prefer to run Cargo yourself.

To make `gitmesh` available on your `PATH` for your user account (optional):

```console
$ install -m755 target/release/gitmesh ~/.local/bin/gitmesh
```

GitMesh is a single binary; at runtime it needs only Git.

## Your first project

```console
$ cd my-project
$ gitmesh init --name my-project --git-init  # creates .gitmesh/project.toml and the root repository
$ gitmesh discover                            # which directories are Git repositories?
$ gitmesh configure add engine --remote git@github.com:acme/engine.git
$ gitmesh configure add renderer --git-init --remote git@github.com:acme/renderer.git
$ gitmesh configure list

$ gitmesh status --changes                    # every change, with its owning repository
$ gitmesh commit -m "implement the new backend"
$ gitmesh pull
$ gitmesh push
```

Two things to know:

* **`gitmesh init` creates a Git repository only with `--git-init`.** Add `--remote URL --add-git-remote`
  to also configure `origin` there. Without `--git-init`, `init` writes the manifest but then exits 1
  with "Project setup completed with errors", because the root is not yet a repository. The manifest
  is still on disk, so run `git init` in the root (or re-run with `--git-init`); this is a known issue
  and is reported in the final audit.
* **Commit the manifest.** The first `gitmesh commit` in the root includes `.gitmesh/project.toml`,
  because the manifest describes the project as a whole.

Every configuration change is planned first: `--dry-run` prints the exact changes and steps, and
nothing is written. `--untrack-from-root` stops the root repository from tracking files that now belong
to a child; the files stay on disk, and nothing is deleted or moved. To see the workflow end to end on
real repositories and local bare remotes, including a real cross-repository conflict:

```console
$ ./examples/demo.sh /tmp/gitmesh-demo      # creates everything under the directory you give
```

## The three interfaces

GitMesh has one core and three front ends. All of them call the same services, so the same project
gives the same results in each. Use whichever suits the task.

| | Command line (`gitmesh <command>`) | Terminal (`gitmesh ui`, alias `tui`) | Web (`gitmesh gui`) |
| --- | --- | --- | --- |
| **For** | scripts, CI, everything | the everyday loop in a terminal | the whole project in a browser |
| **Covers** | every command below | status, stage all, commit (staged files only), pull, push, dry run | overview, changes and commit, branches, sync, repository management, project creation (setup wizard), history checks |
| **Repository management** (add, remove, rename, remotes, clone) | yes, `gitmesh configure …` | no | yes, *Repositories* tab and the setup wizard |
| **Needs** | a terminal | an interactive terminal, at least 56×14 | a browser, on the same machine |
| **Machine output** | `--json` | no | no |
| **Documentation** | this page, and the command help | [docs/TUI.md](docs/TUI.md) | [docs/GUI.md](docs/GUI.md) |

The web interface listens only on `127.0.0.1` by default and has no authentication. Its safety rules,
confirmation steps and layout are described in [docs/GUI.md](docs/GUI.md).

## Command reference

| Command | What it does |
| --- | --- |
| `gitmesh init [PATH] [--name N] [--remote URL] [--add-git-remote] [--git-init] [--branch B] [--force]` | Create the manifest for a project. Records `--remote`; `--add-git-remote` also configures `origin`. `--force` replaces an existing manifest. |
| `gitmesh discover [--depth N] [--hidden] [--deep]` | List the Git repositories found in the tree. Reports facts; changes nothing. |
| `gitmesh configure add <dir> [--id I] [--remote URL] [--git-init] [--branch B] [--untrack-from-root] [--dry-run]` | Make a directory an independent repository. |
| `gitmesh configure clone <dir> --remote URL [--id I] [--branch B] [--dry-run]` | Clone a remote into a missing or empty directory and record it. |
| `gitmesh configure remove <id> [--confirm-takeover] [--dry-run]` | Remove a repository from the configuration. Its directory, `.git`, history and remote are kept. |
| `gitmesh configure rename <id> <new-id> [--dry-run]` | Change the logical id only. |
| `gitmesh configure remote <id> [--url URL \| --clear] [--set-git-remote] [--dry-run]` | Record, change or clear a repository's remote. `--set-git-remote` also configures it in Git. |
| `gitmesh configure list` | List configured repositories. |
| `gitmesh status [--changes] [--short] [--json]` | Unified status and change ownership. |
| `gitmesh commit -m MSG [--repo ID] [--path PATH] [--dry-run]` | Stage and commit each affected repository with one message. |
| `gitmesh branch [create NAME \| delete NAME] [--dry-run]` | Show the logical branch, or create or delete it in every repository. |
| `gitmesh checkout NAME [--create] [--dry-run]` | Switch every repository to a branch. |
| `gitmesh merge NAME [--dry-run]` | Merge a branch into the current one, in every repository (Git's own merge). |
| `gitmesh fetch [--repo ID] [--dry-run]` | Fetch every repository. Never changes branches or files. |
| `gitmesh pull [--strategy ff-only\|merge\|rebase] [--repo ID] [--dry-run]` | Fetch and update. Defaults to fast-forward only. |
| `gitmesh push [--repo ID] [--no-set-upstream] [--dry-run]` | Push every repository that has commits to push. |
| `gitmesh remotes` | Remotes per repository, with provider and GitHub coordinates. |
| `gitmesh ui [PATH] [--dry-run]` (alias `tui`) | Open the terminal interface. See [docs/TUI.md](docs/TUI.md). |
| `gitmesh gui [PATH] [--port N] [--host A] [--allow-host NAME] [--open] [--dry-run]` | Serve the web interface locally. See [docs/GUI.md](docs/GUI.md). |

Global options: `-C <path>` (or `--project <path>`) selects the project directory; `--json` prints
machine-readable output; `-v` / `--verbose` includes Git's own output. The project is the one at or
above the directory you run the command in, so commands work from any subdirectory.

**Exit codes.** `0` the operation succeeded everywhere. `1` the operation failed or was partly
successful, or a configured repository is unavailable (for example its directory is missing; `status`
reports this and exits `1`). `2` usage or configuration error. A conflict reported by `status` does
not change its exit code; a `commit` that cannot proceed because of a conflict exits `1`.

## Safety

* **Nothing is discarded.** GitMesh runs only Git commands that refuse to destroy work. `pull` defaults
  to `--ff-only`, branch deletion uses `git branch -d`, a checkout that would overwrite local work is
  refused, and conflicts are left in place for you to resolve in the repository.
* **Nothing is guessed.** Repository boundaries come from the manifest. Discovery reports facts and
  never decides on its own that a directory is a repository.
* **Nothing is silently changed.** Configuration changes are planned, reviewed and confirmed before they
  run. An existing `.git`, remote or manifest is never replaced without explicit intent. Removing a
  repository never deletes files.
* **Child content stays out of the root.** Root staging excludes every configured child directory, so a
  root commit never contains child files and never records a child as a gitlink. Plain Git can still do
  this if you run `git add -A` in the root yourself. Use `gitmesh commit` (see
  [docs/REPOSITORIES.md](docs/REPOSITORIES.md)).
* **Nothing is hidden.** A failing repository does not stop the others, and every result lists the
  outcome per repository with Git's own message.

## Platforms and support

| Platform | Status |
| --- | --- |
| Linux x86_64 (Debian 13 "trixie") | Built and tested: `cargo test --locked` (full suite), the build-script checks, the browser and terminal checks, and `build.sh --install-rust` from an empty home directory. Git 2.47.3, Rust stable 1.99.0 |
| Linux aarch64 | Not tested. `build.sh` knows the triple; nothing has been built here |
| macOS (Apple silicon or Intel) | Not tested. `build.sh` contains the rustup triples and the Xcode hint; nothing has been built here |
| Other Linux distributions | Not tested. `build.sh` prints the C-compiler package to install for Fedora, Debian/Ubuntu, Arch and openSUSE |
| Windows | Not supported (`build.sh` needs bash; WSL has not been tested) |

The declared minimum Rust is **1.88** (`rust-version` in `Cargo.toml`). The project was built and tested
with stable 1.99.0; 1.88 has not been tested. The minimum Git version has not been established. Git
2.47.3 is the only version tested.

## Documentation

| Document | Read it for |
| --- | --- |
| [docs/GUI.md](docs/GUI.md) | The web interface: layout, views, the repository overview, operations and progress, confirmations, conflicts, responsive behaviour, safety, its HTTP routes, and its tests |
| [docs/TUI.md](docs/TUI.md) | The terminal interface: opening a project, the keys, committing, staging rules, limits |
| [docs/REPOSITORIES.md](docs/REPOSITORIES.md) | Adding and cloning repositories, connecting remotes, root and child ownership, upstreams, push preflight, unrelated histories, sync errors and their fixes |
| [docs/MANIFEST.md](docs/MANIFEST.md) | The `.gitmesh/project.toml` format, validation rules and semantics |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Layering, module boundaries, the safety invariants, design decisions |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | Building, the test suites and what they need, validation scripts, adding commands, troubleshooting |
| [reports/README.md](reports/README.md) | The dated development reports, and the status of each |

## Licence

MIT OR Apache-2.0 (as declared in `Cargo.toml`). The repository does not currently contain separate
licence files.
