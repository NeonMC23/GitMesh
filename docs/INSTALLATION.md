# Installing GitMesh

This guide covers installing GitMesh as a command you can run from any directory: getting the
source, building, installing, updating, and uninstalling. It is for people who use GitMesh.
Developers who change the code should read [DEVELOPMENT.md](DEVELOPMENT.md).

Installing GitMesh and creating a GitMesh **project** are different things. Installation puts one
executable on your computer. A project is a directory with a `.gitmesh/project.toml` manifest that
you create with `gitmesh init` (see [Your first project](../README.md#your-first-project)). Installing
never creates, changes, or deletes projects, repositories, or history.

## Supported platforms

| Platform | Status |
|---|---|
| Linux x86_64 (Debian 13 "trixie") | **Tested.** The build, the test suite, and this installation workflow were run here (Git 2.47.3, Rust 1.99.0). |
| Other Linux distributions or architectures | Not tested. `build.sh` knows the Linux C-compiler packages for Fedora, Debian/Ubuntu, Arch and openSUSE, but nothing else here is verified. |
| macOS, Windows | **Not supported and not tested.** `install.sh` is a POSIX shell script and has not been run on these systems. |

No package manager, desktop integration, or system-wide (root) installation is provided.

## Prerequisites

* **Git** on your `PATH`. GitMesh runs the real `git` command for every operation.
* **A C linker** (`cc`). Rust needs it to link programs. `build.sh` checks for it and prints the
  package to install if it is missing (for example `sudo apt install build-essential` or
  `sudo dnf install gcc`). GitMesh never runs these package-manager commands itself.
* **Rust 1.88 or newer** (the minimum is `rust-version` in `Cargo.toml`). `build.sh` uses a Rust
  already installed on your system if it works and is new enough. If none is found, it asks before
  installing stable Rust for your user account only (see the README, "Quick start").

## 1. Get the source

Use a checkout of the GitMesh repository. This repository currently has no release tags, so choose a
revision yourself, for example:

```console
$ cd ~/src/gitmesh                  # the checkout you have
$ git status                        # should be clean if you intend to install this exact revision
$ git log --oneline -1              # the revision you are about to build
```

To install a particular revision, check it out first with `git checkout <commit-or-branch>`.
The installed executable reflects the source that was built, not the checkout's current state.

## 2. Build

```console
$ ./build.sh
```

This builds `target/release/gitmesh` (or `$CARGO_TARGET_DIR/release/gitmesh` if you set that
variable) with `cargo build --release --locked`. It does not install anything. If `build.sh` reports
`Permission denied`, run `bash build.sh` or `chmod +x build.sh`.

## 3. Install

```console
$ ./install.sh
```

`install.sh` runs `build.sh` first, then copies the executable to **`$HOME/.local/bin/gitmesh`**
and checks the copy by running `gitmesh --version`. The result is a plain copy: it does not depend
on the checkout or on `target/`. Its output shows the exact installed path and, if the directory is not on your
`PATH`, the line to add (step 4).

What `install.sh` does and does not do:

* It writes **one file**, `<destination>/gitmesh`, with mode 755, and creates the destination
  directory if it does not exist. The file is copied next to its destination and then renamed into
  place, so a failed copy never leaves a half-written executable.
* It runs no `sudo`, makes no network request, installs no toolchain (that is `build.sh`'s
  separate, consent-based step), and does not edit `~/.bashrc`, `~/.zshrc`, or any other shell file.
* It never deletes or overwrites a file that is not GitMesh (see "Safeguards" below).

Options:

| Option | Meaning |
|---|---|
| `--bin-dir DIR` | Install to `DIR/gitmesh` instead. `DIR` must be an absolute path (or start with `~/`). |
| `--no-build` | Do not run `build.sh`; install the release build that already exists. Use this only if you know it is current. |
| `--force` | Replace a file at the destination that is **not** a GitMesh executable. Without it, that case is refused. |
| `--uninstall` | Remove the installed GitMesh executable. See step 7. |
| `-h`, `--help` | Show the usage summary. |

Examples:

```console
$ ./install.sh --bin-dir "$HOME/bin"          # a different per-user directory
$ ./install.sh --no-build                      # install the existing build, without rebuilding
```

### Safeguards

* **Identity check.** A file counts as a GitMesh executable only if it is a regular file that contains
  GitMesh's own help text. The file is searched as data, never run, before it is replaced or removed.
  A symbolic link at the destination is never followed or treated as GitMesh.
* **No silent overwrite.** An existing non-GitMesh file, or a directory, at the destination stops the
  install with a message and changes nothing. `--force` is the explicit choice to replace a
  non-GitMesh file.
* **Idempotent.** Running `./install.sh` again updates a GitMesh executable at the same destination.
  If the build is byte-identical to what is installed, it reports `already up to date`.
* **Permission errors fail clearly.** If the destination cannot be created or written, the installer
  stops and suggests another `--bin-dir`. It never retries with `sudo`.
* **Invalid paths are refused** before anything is created: relative paths, empty values, and paths
  that are files.

## 4. Make `gitmesh` available in your shell

`install.sh` does not change your shell, so you may need to add the destination to `PATH` once.

Check first:

```console
$ command -v gitmesh || echo "gitmesh is not on PATH"
$ gitmesh --version                        # should print: gitmesh 0.1.0
```

If `command -v` shows nothing, check whether the directory is on `PATH`:

```console
$ echo "$PATH" | tr ':' '\n' | grep -x "$HOME/.local/bin" || echo "not on PATH"
```

To use it in the current shell only:

```console
$ export PATH="$HOME/.local/bin:$PATH"
```

To make it permanent, add the same line to your start-up file:

| Shell | File | Line to add |
|---|---|---|
| bash | `~/.bashrc` (login shells also read `~/.profile`) | `export PATH="$HOME/.local/bin:$PATH"` |
| zsh | `~/.zshrc` | `export PATH="$HOME/.local/bin:$PATH"` |
| fish | (run once) | `fish_add_path ~/.local/bin` |

Open a new terminal (or run `source ~/.bashrc` / `source ~/.zshrc`) and repeat `command -v gitmesh`.
`install.sh` prints the exact path and this advice when the destination is missing from `PATH`.

Use `$HOME` in these examples rather than a literal home directory, so the same line works for any
user.

## 5. Verify

```console
$ gitmesh --version         # gitmesh 0.1.0
$ gitmesh --help            # the list of commands
$ cd / && gitmesh --help    # works from any directory
```

The installed executable needs only `libc`, `libgcc_s`, and Git at runtime. The web interface's
files are built into the executable, so it does not read the source tree. To confirm which copy
runs when several exist:

```console
$ command -v -a gitmesh     # every gitmesh on PATH, in order
```

## 6. Start GitMesh

GitMesh has three interfaces. All three read the same project; select it with a path or with
`-C/--project`, and it defaults to the current directory. Each command accepts a directory inside
the project, and it finds the project at or above that directory.

```console
$ cd ~/work/demo
$ gitmesh status                     # command line, in this project
$ gitmesh -C ~/work/demo status      # the same project, chosen explicitly
```

Terminal interface (needs an interactive terminal of at least 56 × 14 characters):

```console
$ gitmesh ui ~/work/demo
```

Web interface, served on your own machine (`127.0.0.1:7345` by default):

```console
$ gitmesh gui ~/work/demo            # prints the address; open it in your browser
$ gitmesh gui ~/work/demo --port 8080 --open
```

`gitmesh gui` binds to `127.0.0.1` unless you pass `--host`. Binding to `0.0.0.0` exposes the
interface on your network **with no authentication**; do not do that on a shared network. Stop the
server with Ctrl+C. See [GUI.md](GUI.md) and [TUI.md](TUI.md) for what each interface does.

Use `--dry-run` with `ui` or `gui` to explore operations without changing repositories.

## 7. Update

Update when you change the source or move to a new revision. Installed copies do not update themselves.

```console
$ cd ~/src/gitmesh
$ git pull                           # or: git checkout <new-revision>
$ ./install.sh                       # rebuilds, then replaces the installed executable
$ gitmesh --version
```

You must rebuild and reinstall after any source change, because the installed file is a copy of the
build. `./install.sh --no-build` installs whatever is in `target/release/` without checking that it
matches your source, so use it only after `./build.sh` has run for the revision you want.

Before updating, make a copy of your project manifests. Updating GitMesh does not change them, but a
copy costs nothing:

```console
$ cp ~/work/demo/.gitmesh/project.toml ~/backups/demo-project.toml
```

Running `gitmesh gui` or `gitmesh ui` keeps the old executable in memory until it exits. Stop and
restart them after updating.

## 8. Back up and preserve your data

GitMesh's data lives in your projects, not in the installation:

| What | Where | Touched by install, update, or uninstall? |
|---|---|---|
| The project manifest | `<project root>/.gitmesh/project.toml` | No |
| Each child repository, with its files and history | its own directory under the project root | No |
| The root repository, with its files and history | the project root (Git's own `.git`) | No |
| The GitMesh executable | `~/.local/bin/gitmesh` | Yes (replaced or removed) |
| The source checkout and `target/` build output | wherever you cloned it | No (`cargo clean` removes build output only if you run it) |

Back up a project the same way you back up any Git work: keep copies of the repositories (or push them
to a remote) and of `.gitmesh/project.toml`. The manifest is generated by GitMesh; edit it through
`gitmesh configure` rather than by hand.

## 9. Uninstall

```console
$ ./install.sh --uninstall                 # removes ~/.local/bin/gitmesh only
$ ./install.sh --uninstall --bin-dir DIR   # if you installed to a different directory
```

Uninstall removes the executable and nothing else. It refuses to remove a file that is not a GitMesh
executable. It keeps:

* the destination directory itself (it may contain other files);
* every project, its `.gitmesh/project.toml`, every repository and its history, and every remote;
* the source checkout and its build output;
* any `PATH` line you added to a shell start-up file (remove it yourself if you no longer want it).

To remove the build output too, run `cargo clean` in the checkout. To remove the checkout, delete the
directory yourself once you are sure you do not need it. Neither step is automatic.

## Troubleshooting

**`gitmesh: command not found`.** The destination is not on `PATH`. Follow step 4. Check with
`ls -l ~/.local/bin/gitmesh`; if that file is missing, the install did not complete (re-run
`./install.sh`).

**`Permission denied` when running `./build.sh` or `./install.sh`.** The script lost its executable
bit, which some archive tools and copies do. Run `bash ./install.sh` or `chmod +x install.sh build.sh`.

**`cannot create ... (permission denied)` or `is not writable`.** The destination belongs to another
user or is read-only. Choose a directory you own, for example `./install.sh --bin-dir "$HOME/bin"`.
The installer never uses `sudo`.

**`--bin-dir must be an absolute path`.** Use `--bin-dir "$HOME/bin"` or `--bin-dir ~/bin`, not a relative
path such as `bin`.

**`already exists and is not a GitMesh executable`.** Something else is at the destination. Look at it
with `file ~/.local/bin/gitmesh`. If you are sure it can be replaced, re-run with `--force`; otherwise
install to a different `--bin-dir`. Nothing was changed.

**`is a directory`.** Remove or rename the directory yourself, or choose another `--bin-dir`. The installer
never deletes it. A symbolic link at the destination is reported as "already exists and is not a GitMesh
executable" and is also left alone.

**Build fails: no Rust.** Run `./build.sh` and answer the prompt, or `./build.sh --install-rust` in a
script. Or install Rust yourself from https://rustup.rs or your system packages, then re-run.

**Build fails: Rust too old.** `rustc --version` must report 1.88 or newer. Update your Rust
(`rustup update stable`) or let `build.sh` install a new one for your user account.

**Build fails: no C linker.** Install the compiler package named in the `build.sh` message, for example
`sudo apt install build-essential`. This step is yours; the installer will not run it.

**`no release executable at ...`.** You used `--no-build` before building. Run `./build.sh`, or drop
`--no-build`.

**A stale binary.** Symptoms: `gitmesh --version` or behaviour does not match the source. Check
`command -v -a gitmesh` to see whether an older copy shadows the new one, then run `./install.sh` for
the current source. Remember the installed copy never updates by itself.

**`gitmesh gui` does not start, or the page does not load.** Another program may use the port: pass
`--port 8080` (or any free port) and check the printed address. Use `127.0.0.1` in the browser when the
server runs on your machine. If you reach it through a proxy, add the host name with `--allow-host NAME`.

**`gitmesh ui` prints "no interactive terminal available" and exits 0.** The terminal interface needs an
interactive terminal: run it directly in a terminal, not piped or from a script. The terminal must also be
at least 56 × 14 characters.

**`gitmesh status` says "no GitMesh project found at or above" and exits 2.** You are not inside a project.
Run `gitmesh init` in the project root (see the README), or pass `-C <path>`.

For problems with a project itself (conflicts, child repositories, manifest errors), see the README's
"Safety" section and [REPOSITORIES.md](REPOSITORIES.md).
