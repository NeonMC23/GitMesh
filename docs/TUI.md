# The terminal interface

`gitmesh ui` (alias `gitmesh tui`) is a minimal, full-screen workflow for one GitMesh
project. It does the everyday loop — see what changed, stage it, write one message,
commit, pull, push — and nothing else. Repository management (creating, renaming,
assigning, remotes, manifest changes) and branch operations belong to the CLI and the
graphical interface.

```text
GitMesh  MyProject  branch main
/home/dev/MyProject  ·  5 change(s)  ·  2 staged in 2 repositories
┌ Changes by repository ─────────────────────────────────────────────┐
│ root (root, .)  2 changed, 1 staged                                │
│   A  added     README.md                                           │
│   ??  untracked  notes.md                                          │
│ engine (repo, engine)  3 changed, 2 staged                         │
│   M  modified  src/lib.rs                                          │
└────────────────────────────────────────────────────────────────────┘
┌ Commit message (Tab: edit) ────────────────────────────────────────┐
│fix the parser                                                      │
└────────────────────────────────────────────────────────────────────┘
 s Stage all   c Commit   p Pull   P Push
┌ Result ────────────────────────────────────────────────────────────┐
│Commit: done in 2 repositories                                      │
│✓ root  committed 1 file(s) [9f2c1ab]                               │
│✓ engine  committed 2 file(s) [4d0e7aa]                             │
└────────────────────────────────────────────────────────────────────┘
s stage all  c commit  p pull  P push  Tab message/actions  d dry run  ? help  q quit
```

## Keys

The action-bar keys and the single-character shortcuts come from the same tables as the key
handling, and tests check that each advertised one is bound. Arrow and paging keys are
covered by the workflow tests and the terminal checks in `tools/tui-pty-check.py`.

| Key | Action |
|---|---|
| `s` | **Stage all**: stage every repository's own changes |
| `c` | **Commit**: asks for confirmation, then commits (press `c` or Enter again) |
| `p` | **Pull** every repository (fast-forward only, never discards local work) |
| `P` | **Push** every repository that has commits to push |
| `Tab` | Move between the message field and the action bar |
| `←` `→` | Choose an action button (with the action bar focused); `Enter` runs it |
| `Enter` | In the message field: start a commit. On the action bar: run the chosen button |
| `↑` `↓` `PgUp` `PgDn` | Scroll the changes list |
| `Esc` | Leave the message field; cancel a pending commit |
| `r` | Refresh: re-read the project and every repository |
| `d` | Toggle **dry run**: operations report what they would do and change nothing |
| `?` | Show the keys (any key closes it) |
| `q` or `Ctrl-C` | Quit |

In the message field, letters are typed, not interpreted as shortcuts.

## Committing

1. Write one message in the message field. An empty or whitespace-only message is refused.
2. Press `s` (Stage all) to stage the changes. Committing without anything staged is refused
   with a pointer to Stage all.
3. Press `c`. The screen states how many files, in how many repositories, will be
   committed with the message. Press `c` or Enter again to confirm; any other key cancels.
4. Each repository with staged files gets its own real `git commit` with the same message.
   Repositories with nothing staged are skipped.

There is no combined "global" commit. Only what is staged is committed, so you always see
the staged set before it becomes a commit.

**Difference from the other front ends:** `gitmesh commit` and the graphical Commit card
stage everything first and then commit (`git commit -a`-like). The terminal interface
commits only what you staged explicitly. Both use the same repository-scoped rules.

## Staging rules

* Stage all runs `git add` **inside each repository**, for that repository's own files
  only. The root never stages files that belong to a nested repository.
* Ignore rules are respected: ignored files are never staged.
* A repository with **conflicted files**, or in the middle of a **merge, rebase or
  cherry-pick**, is refused: `git add` would silently mark conflicts resolved. The
  repository is reported as `!` (conflict) or `✗` (in progress) and nothing is staged there.
  Other repositories are still staged.

## Pull and push

Each repository gets one line in the result panel:

| Symbol | Meaning |
|---|---|
| `✓` | Done in this repository |
| `-` | Skipped: nothing to do, or no remote configured (a local-only repository is **skipped, not failed**) |
| `!` | Conflict: the operation stopped and nothing was discarded |
| `✗` | Failed, with Git's message under it |

The title of the result is never a success claim when something needs attention:
*NOT everything succeeded: 1 of 3 repositories need attention*. A title of
*done in N repositories* means every repository that had work succeeded.

## Size and layout

* Minimum size: 56 columns × 14 rows. Smaller terminals show a resize notice instead of a
  cramped screen; enlarging the window restores the view immediately.
* Long paths, names and messages are cut with `…`; the message field shows the end of a
  long message.
* A result that does not fit its panel says how many lines are hidden. Enlarge the
  terminal to see them; nothing disappears silently.

## Not in the terminal interface

These are deliberately not in `gitmesh ui`:

* creating a repository, renaming one, assigning a directory, recording a remote, or any
  manifest editing: use `gitmesh configure …`, `gitmesh set-remote`, or the graphical
  Repositories tab;
* branches (create, switch, merge, delete) and fetch: use `gitmesh branch`,
  `gitmesh checkout`, `gitmesh merge`, `gitmesh fetch`, or the graphical Branches and Sync views;
* a second configuration system.

Before this redesign the terminal interface also offered those operations, and a setup
screen. They were removed from the terminal on purpose so that the everyday workflow is
the whole screen.

## Without a terminal

When standard output is not a terminal (a pipe, CI, a script), `gitmesh ui` prints a short
explanation and the equivalent command-line steps instead of starting the interface.
