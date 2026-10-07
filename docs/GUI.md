# The GitMesh graphical interface

```console
$ cd my-project
$ gitmesh gui
gitmesh gui

  interface   http://127.0.0.1:7345
  project     my-project
  root        /home/dev/my-project

  Open the address above in a browser. Press Ctrl+C to stop.
  (GitMesh runs locally: no cloud service, no telemetry, no account.)
```

`gitmesh gui` starts a small web server **on your machine** and serves one page. Open the
address it prints in any browser; there is nothing to install, no account, and nothing is
fetched from the internet. Stop it with `Ctrl+C`.

Options:

| Flag | Meaning |
| --- | --- |
| `-C <path>` / `<path>` | Project directory (or any directory inside it). Default: the current directory |
| `--port N` | Port to bind (default `7345`; `0` picks a free one) |
| `--host A` | Address to bind (default `127.0.0.1`; `0.0.0.0` exposes it on the network — see *Safety* below) |
| `--allow-host NAME` | Also accept requests addressed to `NAME`, for access through a proxy or a port forward. Repeatable |
| `--open` | Try to open the interface in your browser |
| `--dry-run` | Start in dry-run mode: every operation is simulated |

## What it is

The GUI is a **front end**, not a second implementation:

* every read comes from `gitmesh::service` and `gitmesh::analyzer`;
* every write goes through `gitmesh::service` → `gitmesh::ops` → `git` — the same code
  the CLI and the terminal interface run;
* the interface renders one JSON model that the Rust side builds, so ownership,
  classification and outcomes cannot drift from the CLI.

There is no JavaScript build step, no framework and no dependency: the page, its
stylesheet and its script are compiled into the `gitmesh` binary.

## One project

The interface presents the whole project:

```text
MyProject                          branch  main        modified
/home/dev/MyProject · 4 repositories · 5 changes

  MyProject                    [root]
    src/               3 files
    engine/           1 file     [repo]
    renderer/         2 files    [repo]
    tools/            1 file     [repo]
```

* **Status** — the project tree with the repository boundaries marked, and a table with
  one row per repository: id, branch, state, and what is going on in it.
* **Changes** — one list of changed files with their *logical* path (`engine/src/lib.rs`),
  their change type, and the physical repository that owns them. Optional grouping by
  repository makes the mapping explicit without making it the default.
* **Commit** — the repositories that will receive a commit, the ones that cannot (with
  the reason), one message box, one button.
* **Branches** — the logical branch, every branch across the project (which repositories
  have it, which have it checked out), and create / switch / merge / delete actions.
* **Pull / Push** — the pull strategy selector plus fetch, pull and push, with the
  ahead/behind state of every repository.
* **Project** — read-only project information: name, root, manifest, repositories, paths,
  remotes, and any notices GitMesh produced.

## Opening a project

GitMesh looks for `.gitmesh/project.toml` in the directory you start from and above it.
You can start the interface anywhere:

* from the project root, from a nested directory (the project is found upwards), or
* with no project open at all — the interface then shows what it looked for and why it
  failed, and lets you type a directory to open.

A directory that is not a GitMesh project produces a clear message, never an empty
screen; opening another project is a single action and does not require restarting.

## Creating a project (the setup wizard)

Starting from an ordinary folder — no manifest and no repositories, or some of each — the
interface can build the GitMesh project for you. Press **New project** (or **Create a
GitMesh project here** on the welcome screen) and walk the twelve steps:

```text
1  choose the project root            any local directory, absolute path
2  scan the structure                 facts only: sub-directories, files, existing .git
3  select repository boundaries       one checkbox per directory; nested or overlapping
                                      selections are refused, with the reason
4  name the root repository           name, branch, optional remote, optional provider
5  configure each selected directory  id, "create a Git repository here if there is
                                      none", remote, and "stop tracking it in the root"
6  see what already exists            existing repositories are adopted, explained, and
                                      never re-initialised
7  review the remotes                 local paths, existing remotes, or a GitHub target
8  the manifest                       `.gitmesh/project.toml` is generated from the
                                      answers; an existing one is kept unless you confirm
9  review the plan                    every step, every refusal, the exact manifest text
10 confirm                            the plan id you reviewed is what will run
11 execute                            live per-step progress
12 result                             validation, and the project opens with no restart
```

What the wizard is:

* **plan-driven** — the step-9 review and the execution consume the *same* typed plan
  (`SetupRequest` → `SetupPlan` → `apply` → `SetupResult` → `verify`). Changing any answer
  invalidates the reviewed plan, and its fingerprint travels back with the confirmation,
  so a plan that changed on disk in between is refused instead of run.
* **read-only until you confirm** — scanning and planning create nothing at all.
* **honest about adoption** — a directory that already is a Git repository is used as it
  is (its history, branch and remote untouched); the plan says so instead of planning a
  `git init` there.
* **local-first** — a remote is optional everywhere. Unchecking "configure remotes" means
  the URL is recorded in the manifest and Git is not touched at all; a remote that would
  contradict an `origin` already on disk is refused rather than recorded silently.
* **GitHub-optional** — picking a GitHub target only prepares the URL and shows the exact
  `gh repo create` command, from the provider layer. GitMesh never creates the repository,
  never asks for a token and stores no credential (see `docs/MANIFEST.md`).
* **re-runnable** — running the wizard again on the created project reports every step as
  already satisfied, leaves the manifest byte-for-byte unchanged, and simply opens the
  project.

### First publish

If you tick *after the setup, make one first commit and push it*, the plan carries that
promise (`FirstPublish`) and shows the message and the repositories it will touch. The
setup engine itself never commits and never pushes: after a successful setup the interface
runs the **ordinary** commit and push operations, restricted to the repositories that
received a remote here. Concretely:

* one commit message, one real commit per affected repository — never a global commit;
* repositories without a remote stay local, are not pushed, and are not failures;
* a failing commit or push does not stop the others and is reported per repository with
  Git's own message; a partial result is never shown as success.

### When something fails

* A refusal at step 9 (overlapping boundaries, an unconfirmed remote replacement, a
  missing directory, a path outside the project, a duplicate id, a first commit without a
  message) blocks the whole plan: nothing is written, and each refusal says what to fix.
* During execution a failing step does not stop the unrelated ones; a repository whose
  creation failed has its own follow-up steps skipped, and the manifest is still written
  when it can be, so the remaining project is honest about itself.
* A partial setup stays in the wizard with the failing steps on screen — a half-built
  project is not opened, because adopting it would hide what still has to be fixed. A
  complete setup (including a re-run where everything already exists) opens the project
  immediately, in the same interface, with no restart.

### What stays manual

* Creating the hosted repository itself (GitHub, GitLab, …): GitMesh prints the command
  and records the URL; it never talks to a provider API.
* Resolving conflicts: normal Git tooling, as everywhere else in GitMesh.
* Editing the configuration afterwards (assigning, renaming, changing remotes): still
  `gitmesh configure` or `gitmesh ui`, exactly as before.

## How status and changes work

Status is the same unified status the CLI prints: repository, branch, staged/unstaged/
untracked/deleted/renamed/conflicted entries, ahead/behind, upstream and remotes. Change
ownership comes from the manifest, not from the filesystem, so `engine/foo.rs` is simply
a file of the project that happens to live in the `engine` repository — and the
repository root is never reported as "containing" another repository's files.

Refresh (`R`, or the refresh button) re-reads everything. The view also refreshes
automatically every 15 seconds while it is visible and no operation is running.

## How the unified commit works

Pressing *Commit the project*:

1. analyses every repository,
2. stages the changes of each affected repository (never the files of another
   repository),
3. runs one real `git commit` per affected repository with the message you typed,
4. reports the outcome per repository: `✓ committed`, `– nothing to commit`,
   `✗ failed`, and lists repositories that cannot be committed (conflicts, missing
   directories) with the reason.

There is **no synthetic global commit**: your message is used verbatim in every
repository that receives a commit, and the interface shows you which repositories those
are before you press the button.

## How branch operations work

A branch is a project branch: *Create & switch* creates it in every repository (or
switches where it already exists), *Switch* moves every repository onto an existing
branch, *Merge* merges into the current branch in each repository, and *Delete* removes
it from the repositories that can delete it safely.

Repositories that cannot follow are reported explicitly — the interface never pretends
the project is on one branch when it is not. If the physical repositories are already on
different branches (for example because someone checked out a branch by hand), the branch
view says so and names them.

## How pull and push work

* **Pull** fetches first, then updates. The default strategy is fast-forward only: if a
  repository has diverged, GitMesh refuses and says so instead of silently creating a
  merge commit. *Merge* and *Rebase* are explicit choices.
* **Push** pushes only the repositories that have commits to push and sets the upstream
  on the first push. Repositories with nothing to push are reported as "nothing to do",
  not as failures.
* A failing repository never stops the others. The result panel lists every repository
  with its own outcome and keeps the details Git returned.

## Conflicts and failures

Conflicts are shown as conflicts — a distinct colour, a distinct symbol (`!`), the
conflicted files listed, the project state marked *conflicted*, and the affected
repositories marked as blocked in the commit view.

GitMesh does not resolve conflicts, and the interface does not pretend to. When one
occurs, it shows the repository and the files, and the exact Git commands to use
(`git status`, `git add … && git commit`, `git merge --abort`). Resolve in the repository,
press *Refresh*, and continue: the other repositories are unaffected and their results are
still displayed.

Failures are equally visible: the repository, the operation, Git's own message, and
whether the rest of the project succeeded.

## Operation progress

Git operations can take a while (a fetch over the network, a large commit). The interface
never freezes and never blocks the whole page: an operation runs in the background on the
server, and progress streams to the page as server-sent events.

```text
Pulling the project
…  renderer   working…
✓  root       already up to date
✓  engine     pulled 2 commit(s)
–  tools      nothing to do
```

The final panel replaces it with the aggregate result: how many repositories succeeded,
were skipped, conflicted or failed, with details per repository.

## The local HTTP surface

The page talks only to the server it was served from.

| Route | Purpose |
| --- | --- |
| `GET /api/model`, `POST /api/refresh` | the interface model (project, tree, repositories, changes, readiness) |
| `POST /api/open` | open a project from a directory (refused while an operation runs) |
| `POST /api/dry-run` | toggle dry-run mode |
| `POST /api/commit`, `/api/branch`, `/api/sync`, `/api/push` | the logical operations, always asynchronously |
| `GET /api/events/<id>` | server-sent stream of per-repository progress and the final result |
| `GET /api/report/<id>` | the stored result of the last operation |
| `GET /api/setup/status` | read-only: what the interface sees in the directory it was started in |
| `POST /api/setup/inspect` | scan a directory as a candidate project root (`path`, optional `name`) |
| `POST /api/setup/plan` | generate the plan for the wizard's answers — creates nothing |
| `POST /api/setup/apply` | execute a reviewed plan; the reviewed plan id is required, and the work runs in the background on the same event stream |
| `GET /api/health` | liveness (start-up check, workflow scripts) |

## Safety

* **Local by default.** The server binds `127.0.0.1`, has no authentication and is not
  meant to be exposed. `--host 0.0.0.0` is possible for containers and remote
  development, and prints a warning when used; combine it with `--allow-host` when the
  network name is not the bind address.
* **Cross-site requests are refused.** A POST is only accepted when the `Origin` header
  (when present) matches the server's own host, and requests with a foreign `Host` header
  are rejected — a random web page cannot drive your repository.
* **Behind a proxy.** If you reach the interface through something that rewrites the host
  (a reverse proxy, a container port forward, a tunnel), the browser sends that host name
  and GitMesh answers `421 Misdirected Request`. Pass it explicitly,
  `gitmesh gui --allow-host dev.example.com`, and nothing else changes: names that are
  not listed are still refused.
* **Nothing is discarded.** The interface offers exactly the operations the CLI offers,
  with the same safety rules: no forced checkout, no destructive reset, conflicts left in
  place, local work never overwritten.
* **Dry-run** simulates every operation and is visible in the status bar.
* **No silent configuration changes.** The GUI cannot redraw your project layout: marking
  directories as repositories, renaming ids and changing remote URLs stay in
  `gitmesh configure` and `gitmesh ui`.

## What is not supported yet

* Editing an existing project's configuration (assign/rename/remote) — deliberate for this
  version: the wizard *creates* a project, `gitmesh configure` and `gitmesh ui` edit one.
* Cloning a project that does not exist locally yet (create it here, or `git clone` and
  then open it).
* Resolving conflicts inside the interface; they are resolved with Git, as documented.
* A history/log view, per-file diffs, and staging individual files (the commit stages
  everything, as the CLI does).
* Cancelling a running operation (it always runs to completion and reports).
* Remote/resumable sessions: the server is meant for one local user.

## Testing the interface

```console
$ cargo test                       # includes the service layer and the GUI tests
$ cargo test --test client         # runs the interface's client logic under Node (if installed)
$ ./tools/gui-workflow.py          # full manual workflow over the real HTTP interface
$ ./tools/setup-workflow.py        # the setup wizard end to end, from a blank directory
```

`tools/gui-workflow.py` builds a temporary multi-repository project with local bare
remotes and walks the whole workflow — open, status, changes, commit, branch, pull,
conflict, resolve, push, reopen — printing one line per check. It is the script used
during development to validate the interface end to end.

`tools/setup-workflow.py` starts from an ordinary directory (`/tmp/gitmesh-setup-e2e`,
which it leaves behind for inspection), scans it, reviews a plan that creates two
repositories, adopts one that already exists and keeps a fourth local-only, executes it,
and then checks the result on disk: real `.git` directories, the generated manifest, the
configured remotes, the first publish, a real push, a deliberately broken remote (partial
failure), a rerun on the now-configured project, and a restart of the interface. It also
checks that a repository outside the project is never touched.
