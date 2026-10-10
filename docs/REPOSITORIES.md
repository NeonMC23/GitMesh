# Repositories: onboarding, cloning, remotes, upstreams and sync errors

This page describes how a repository gets into a GitMesh project, how it is connected to a
remote, and what GitMesh reports when pull or push cannot proceed. Everything here uses the
Git CLI on your machine. No account, token or hosting API is needed. A remote can be any
URL or path that `git` itself accepts, including a local bare repository.

The project manifest (`.gitmesh/project.toml`) is written by GitMesh. Change repositories
with the commands and interfaces below, not by editing the file.

## Where you can do each thing

| Task | GUI (`gitmesh gui`) | CLI | TUI (`gitmesh ui`) |
|---|---|---|---|
| Register a directory that is already a repository | Change one repository → candidate check | `gitmesh configure add <dir>` | not offered |
| Create a repository in a directory | Candidate check → initialise | `gitmesh configure add <dir> --git-init` | not offered |
| **Clone a remote into a new directory** | Change one repository → *clone a remote into a new directory* | `gitmesh configure clone <dir> --remote <url>` | not offered |
| Record or change a remote | Change one repository → *record another remote* | `gitmesh configure remote <id> --url <url> [--set-git-remote]` | not offered |
| Commit, pull, push, see status | yes | yes | yes (daily workflow) |

The TUI stays deliberately small: it does not create, clone, rename or reconfigure
repositories. Use the GUI or the CLI for those.

## Checking a directory before adding it

The GUI's candidate check and `configure add --dry-run` report what GitMesh sees. The
states it distinguishes are:

- **Local only**: a repository with no remote. It is valid. Pull and push skip it with
  "no remote configured" and this is not a failure.
- **Missing directory**: GitMesh does not create it. The check points to clone, which creates
  the directory from a remote.
- **Empty directory**: a folder with no entries. GitMesh can `git init` it, but a remote that
  already has history is never merged into it (see below). To bring that history in, clone.
- **Uninitialised directory**: a folder with files. It can be initialised as a new repository.
- **Initialised, no commits**: a repository with no commit yet. Commit-dependent operations
  (pull, push) are skipped with "repository has no commits yet". When its remote already has
  history, the review says the history is not brought in: make the first commit here, or clone.
- **Valid remote**: the remote answers and has commits.
- **Empty remote**: the remote answers but has no commits. Pushing publishes the repository.
- **Missing or unreachable remote**: the real Git error is shown, with what to check.
- **No upstream**: the repository has a remote but no tracking branch.
- **Divergent or unrelated histories**: see "Sync errors" below.

## Cloning a remote into the project

```sh
gitmesh configure clone libs/core --remote git@example.com:team/core.git --id core
gitmesh configure clone libs/core --remote /srv/git/core.git --dry-run   # report only
```

Rules:

- The destination must be **missing or an empty directory**. A directory with files is
  refused and its files are left untouched. A directory that is already a Git repository is
  refused: use `configure add` to adopt it, which keeps its history as it is.
- The remote is read **before** anything is planned. An unreachable remote or a wrong URL is
  refused with Git's own reason, and nothing is created. You can correct the URL and run the
  command again.
- The clone tracks the remote's default branch when the remote has commits. An empty remote
  gives a repository with no commits, which is valid; the first push publishes it.
- The new repository is recorded in the manifest only after the clone succeeded. If the clone
  fails when it runs, the manifest is not written, so no repository is listed that does not
  exist.
- The root repository never takes ownership of the cloned files; the clone is its own
  repository with its own `.git`.
- Running the same command twice is refused with "already the GitMesh repository", and changes
  nothing.

## Connecting a directory that already is a repository

`gitmesh configure add <dir> --remote <url>` records the remote and, with the option the
interface offers, configures `origin` in Git. The existing history is kept. An existing
`origin` is never replaced silently: the review shows it, and `configure remote --set-git-remote`
is the explicit way to change it.

If the remote is unreachable at that point, the directory is still added with a warning, so
that a temporary network problem does not block the setup. Check the remote before the next
sync.

If the remote is empty, the plan says so. Pushing the first commits publishes the repository.

**Do not connect an empty folder to a remote that has history.** GitMesh refuses to `git init`
an empty folder whose remote already has commits, because the result would be an unrelated
history. Clone the remote into a new or empty directory instead (`configure clone`).

**A repository without commits does not receive its remote's history.** Connecting it records
the remote, and the review says that the history is not merged in. Pull skips the repository
until it has a commit. To work from the remote's history, clone it into a new directory.

A directory that does not exist yet is refused with a pointer to `configure clone`, not
created silently.

## Upstream (tracking) branches

- **After a clone**, the local default branch tracks `origin/<default>`, where `<default>` is
  the branch the remote's `HEAD` points to. An empty remote has nothing to track yet.
- **After connecting an existing repository**, no upstream is set. The first `gitmesh push`
  sets one (`git push --set-upstream <remote> <branch>`).
- **Same-name rule.** GitMesh tracks and pushes only a branch of the *same name* as the local
  branch. It does not rename branches, and it does not choose between several remote branches.
  If your local branch is `feature-x` and the remote has only `main`, pull reports that there is
  no branch named `feature-x` and lists what the remote has. Push it with `gitmesh push`, or
  choose the upstream yourself with `git branch --set-upstream-to=<remote>/<branch>`.
- **Several remotes.** When more than one remote is configured, pull and push never choose a
  remote for you. Pull reports the situation and names the command to use.
- **Pull without an upstream** first reads the remote. An unreachable remote is a failure,
  because it is broken whatever the upstream is. A reachable remote is reported as "no upstream",
  with the one command that fits (if there is exactly one reading).
- Fetch never changes your branches or working files. Pull is a separate step.

## Sync errors and what to do

Pull and push report one line per repository. "Skipped" means nothing was needed or nothing
could be done, and is not a failure. "Failed" means the repository needs attention, and the
operation as a whole is never reported as a success when any repository failed.

| Message | Meaning | What to do |
|---|---|---|
| `no remote configured` (skipped) | Local-only repository. | Nothing, or `configure remote <id> --url … --set-git-remote`. |
| `no upstream branch configured` (skipped) | Remote exists, no tracking branch. The details say which situation it is (below). | `git push -u <remote> <branch>`, or `gitmesh push`. |
| `… and remote 'origin' could not be read` (failed) | No upstream, and the remote itself is unreachable or gone. | Fix the URL or access; nothing local was changed. |
| `several remotes are configured … no upstream is chosen` (skipped) | More than one remote and no upstream. | Choose one with `git push -u <remote> <branch>`. |
| `remote 'x' has no branch named 'y' (it has: …)` (detail) | Same-name rule: no branch of that name on the remote. | Push it, or set the upstream to a branch you choose. |
| `'origin/y' exists on the remote; to track it, run …` (detail) | Unambiguous: the same-name branch exists. | Run the printed command if you want that tracking. GitMesh does not run it. |
| `repository has no commits yet` (skipped) | No commit to pull or push. | Commit first. |
| `no longer exists on the remote` (failed) | The remote deleted the tracked branch. | Push the branch again, or pick another upstream. |
| `diverged from …` (failed) | Both sides have commits the other lacks. Pull refuses to merge silently. | `gitmesh pull --strategy merge` or `--strategy rebase`, then push. |
| `rejected: … diverged` (push, failed) | Same as above, seen from the push. | Integrate with `gitmesh pull --strategy merge` (or `rebase`), then push again. |
| `… unrelated histories (no common commit)` (failed) | The two repositories share no commit at all, so neither pull nor merge can join them automatically. | Decide which history to keep. Do not force it: GitMesh never force-pushes. |
| `rejected: the remote has commits this repository does not have` (push, failed) | The remote is ahead. | `gitmesh pull` first, then push. |
| `rejected by the remote` (push, failed) | The remote refused the update itself (a hook, a protected branch). The remote's own message is shown. | Follow the remote's policy. Local work is unchanged. |
| `the remote could not be read` / `cannot reach the remote` | Network, path or permission problem. | Check the URL and access. The message names the cause. |

Nothing in these cases changes your local commits or working files. GitMesh never resets,
cleans, force-pushes or overwrites your changes.

## Limitations

- Integrating diverged branches is done by you, with `pull --strategy merge|rebase`, not
  automatically.
- Only same-name branches are tracked and pushed (see "Same-name rule"). Renaming a branch
  to match the remote is not done by GitMesh.
- A repository without commits is not given its remote's history by GitMesh; pull skips it.
- After a clone, tracking follows the remote's `HEAD`. If the remote has no `HEAD` branch
  recorded, the clone may have no upstream; pull then reports it as above.
- A remote that is unreachable at `configure add` time is recorded with a warning; GitMesh
  does not block it.
- The TUI does not clone; this is a GUI and CLI operation.
