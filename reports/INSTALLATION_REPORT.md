# Installation report

Scope: a per-user installation workflow for GitMesh (`install.sh`, `tools/install-check.sh`,
`docs/INSTALLATION.md`, and the README link), plus the documentation and hygiene corrections made in the same
pass. Nothing was committed or pushed. Platform scope: **Linux x86_64, Debian 13 only** (Git 2.47.3, Rust
stable 1.99.0). Every other platform is untested.

## 1. Documentation inconsistencies and hygiene issues found

Installation (new work):
* `install.sh --force` with a destination that is a symbolic link to a directory moved the new executable
  into that directory, left the link in place, and then failed its version check (exit 1). A real defect in
  the installer written earlier in this task.
* The installer hid the real error text when it could not create the destination directory (`2>/dev/null`).
* `docs/INSTALLATION.md` described the symlink case as "already exists and is not a GitMesh executable", which
  was not the message printed, and claimed `(permission denied)` text the script does not print.

Documentation (existing guides, checked against the code):
* `docs/GUI.md`: wizard headings did not match the twelve real steps (`index.html`); the labels "New project",
  "Create a GitMesh project here", "Commit the project", "Create & switch", "Merge", "Delete" did not exist;
  the sample output and the "One project" tree contained invented counts; the `/tmp/gitmesh-repositories-e2e`
  claim was wrong (the code uses `mkdtemp`); dry run was described as a status-bar item (it is a header badge
  toggled by `D`); `gitmesh ui` was said to edit configuration (it does not).
* `docs/DEVELOPMENT.md`: stale terminal-interface screens, hard-coded `/tmp` paths and step counts, a garbled
  `GITMESH_TEST_KEEP` sentence, an unverified "Git 2.25+" claim, an outdated layout tree, and unverified
  statements about internals. Rewritten from verified facts.
* `docs/MANIFEST.md`: the "Written by" line omitted the setup wizard and the GUI Repositories tab, and said the
  terminal UI writes the manifest (it does not). Unknown keys were said to be rejected; they are accepted and
  dropped on the next save. The overlap example used text the product does not print for that command.
* `docs/REPOSITORIES.md`: the example used a private project name (`RAMforge`), which is replaced by `engine`.
  The "pull first" row did not match the product text. The "diverged" wording was approximate.
* `docs/ARCHITECTURE.md`: `src/json.rs` was said to be about 130 lines (it is 271); `src/git/history.rs` and
  `src/git/remote.rs` were missing from the module map; the claim that the GUI makes no direct Git call was
  false (it holds a `GitRunner` and passes it to the core); a blank line at end of file.
* `reports/`: the index did not mention this report.

## 2. Documentation updated

| File | Change |
| --- | --- |
| `install.sh` | Symlink refusal at the destination, even with `--force`; uninstall refuses a symlink; `mkdir` error text is shown. |
| `tools/install-check.sh` | Regression checks for `--force` with a symlink to a directory (refused; target receives nothing; link kept). 49 checks. |
| `docs/INSTALLATION.md` | Symlink behaviour in Safeguards, Troubleshooting and Uninstall; exact installer messages; PATH advice wording for bash. |
| `docs/GUI.md` | Real startup output; real wizard headings; real button and tab labels; dry-run badge and `D`; configuration-editing claim corrected; test-path claims corrected. |
| `docs/DEVELOPMENT.md` | Rewritten: prerequisites, validation sequence, tool table, test layers, fixtures, terminal and provider structure, layout, troubleshooting. |
| `docs/MANIFEST.md` | Writers corrected; unknown-key behaviour corrected; real overlap message. |
| `docs/REPOSITORIES.md` | `RAMforge` replaced by `engine`; push-refusal texts corrected. |
| `docs/ARCHITECTURE.md` | `json.rs` line count; module map rows for `history.rs` and `remote.rs`; GUI-to-Git wording; EOF newline. |
| `README.md` | Prominent link to `docs/INSTALLATION.md` (done earlier in this task). |
| `reports/README.md` | Index row for this report. |

Installation guide platform statement: Linux x86_64 Debian 13 is the only tested platform. macOS, Windows,
other architectures, package managers, desktop integration and system-wide installs are **not** supported.

## 3. Cleanup and `.gitignore`

* Scratch removed (exact paths): `/var/tmp/gm-guidoc.A76vau` (test project used for the GUI startup check),
  `/var/tmp/gm-demo.75tAe9`, `/var/tmp/gm-symlink.*` (from the symlink reproduction), and the temporary
  logs and helper scripts this session created in `/var/tmp`. No GitMesh process was running.
* Left alone: `/tmp/arena-workspace` (not ours), the user's checkout and any real `~/.local/bin/gitmesh`
  (not present; the isolated test used a temporary directory).
* **`.gitignore`: no change.** The only ignored item in the working tree is `target/` (already ignored). No
  generated, cache or scratch file is untracked. No broad rule was added; no source, test, fixture, lockfile
  or report is ignored.

Test tooling installed at user level (not part of the repository): a Python virtual environment at
`~/.venvs/gitmesh-tests` (playwright 1.63.0, pyte 0.8.2), the Playwright Chromium build under
`~/.cache/ms-playwright`, and Debian packages extracted without root into `~/.local/gmbrowserlibs`, used only
through `LD_LIBRARY_PATH` for the browser check. No system configuration was changed.

## 4. Code and script fixes, with justification

1. **`install.sh`: refuse any symlink at the destination, even with `--force`.** Reproduced: with
   `--bin-dir D --force` and `D/gitmesh` a symlink to a directory, the old code exited 1 with
   "the installed file did not report a GitMesh version", leaving a `.gitmesh-install.*` file inside the
   link's target. The installer now stops before changing anything:
   `…/gitmesh is a symbolic link, so it was NOT changed. Remove the link yourself, or choose another --bin-dir.`
   Uninstall refuses symlinks the same way (`… so it was NOT removed`). Regression checks added.
2. **`install.sh`: `mkdir` errors are shown** instead of discarded, so the user sees the real cause.

No Rust source was changed in this pass.

## 5. Validation: commands and results

Commands were run in `/home/user/gitmesh` on Linux x86_64 Debian 13, unless noted.

| Command | Result |
| --- | --- |
| `bash -n build.sh && bash -n install.sh && bash -n tools/install-check.sh && bash -n tools/build-script-check.sh && bash -n tools/toolchain.sh && bash -n tools/rust-env.sh` | exit 0 (all syntax OK) |
| `tools/install-check.sh` | exit 0: **49 PASS, 0 FAIL** (earlier runs had 46; three regression checks added for the symlink case) |
| `tools/build-script-check.sh` | exit 0: **81 passed, 0 failed, 0 skipped** |
| `./build.sh` | exit 0; builds `target/release/gitmesh` |
| `./tools/rust-env.sh cargo fmt --all -- --check` | exit 0 |
| `./tools/rust-env.sh cargo clippy --all-targets --all-features --locked -- -D warnings` | exit 0 |
| `./tools/rust-env.sh cargo test --locked` | exit 0: **471 passed, 0 failed, 0 ignored** (unit 367; `cli` 25; `client` 2, Node present so the test ran; `clone_recovery` 10; `lifecycle` 27; `remote_history` 16; `repository_separation` 9; `workflows` 15) |
| `python3 tools/gui-browser-check.py $PWD/target/release/gitmesh` (Chromium via Playwright, with user-level libraries) | exit 0: **41 PASS**, "all browser checks passed" |
| `python3 tools/tui-pty-check.py $PWD/target/release/gitmesh` | exit 0: **24 PASS**, "all terminal checks passed" |

Isolated installation (temporary directory, `--no-build`, not the real `~/.local/bin`):

```text
$ ./install.sh --no-build --bin-dir /var/tmp/gm-inst.XXXX/bin
installed: /var/tmp/gm-inst.XXXX/bin/gitmesh
version: gitmesh 0.1.0
/var/tmp/gm-inst.XXXX/bin is not in your PATH, so 'gitmesh' will not be found yet.
...
$ <dir>/bin/gitmesh --version      -> gitmesh 0.1.0
$ <dir>/bin/gitmesh --help         -> usage text
$ ./install.sh --no-build --bin-dir <dir>/bin      -> re-run; also checked as "already up to date" by install-check
$ ./install.sh --uninstall --bin-dir <dir>/bin     -> removed the executable only; directory kept
```

Also verified by hand in this pass: `gitmesh ui` outside a terminal inside a project prints
`gitmesh: no interactive terminal available.` and exits 0; the TUI minimum is 56×14 (`src/ui/app.rs`);
`gitmesh status` outside a project exits 2. The GUI startup output quoted in `docs/GUI.md` was captured in an earlier round of this task, not in this pass.

Not run in this pass: `cargo build` in debug (covered by `cargo test`); `shellcheck` (not installed); the
workflow scripts `tools/gui-workflow.py`, `tools/setup-workflow.py`, `tools/repository-workflow.py`, the GUI
clone and recovery checks, and `examples/demo.sh` (not re-run in this session); a fresh-machine install.

### Git status

* Staged: `A build.sh` (mode 100755, blob `b4ca978…`). Restored in this pass. It had disappeared from the index
  between tool calls; the cause was not determined.
* Nothing else is staged.
* Unstaged modifications: 37 tracked files (documentation, `src/`, `tests/`, `tools/`, `examples/demo.sh`,
  `Cargo.toml`, `README.md`). Earlier work by the user; not reverted.
* Untracked, from this task and earlier ones: `install.sh`, `tools/install-check.sh`, `tools/build-script-check.sh`,
  `docs/INSTALLATION.md`, `docs/TUI.md`, `docs/REPOSITORIES.md`, `reports/*` (including this report), new modules
  and tests, and browser and terminal check tools.
* Executable bits: `build.sh`, `install.sh`, `tools/install-check.sh` are 755 in the working tree.
  Snapshots may drop executable bits; re-check with `ls -l` before relying on them.
* No commit, tag, push, reset, stash or clean was performed.

## Remaining issues and recommended manual checks

* **Unchanged defect:** `gitmesh init` without `--git-init` in a non-repository directory writes the manifest,
  reports completed-with-errors, and exits 1. `init --git-init` exits 0. Documented, not fixed.
* **Unknown manifest keys** are accepted and dropped on save. The docs now say so. Changing this to reject them
  is a user decision; the parser was not changed.
* **Version floors:** Rust 1.88 is declared and not tested; the minimum Git version is not established.
* **Binary contents:** `target/release/gitmesh` embeds build-machine source paths in panic messages. Do not
  describe the binary as path-free.
* **Manual checks recommended:** run `./install.sh` (default `~/.local/bin`) on a clean Debian 13 account and
  follow the PATH steps for bash, zsh and fish; open `gitmesh gui` in a real browser on the machine; run the
  workflow scripts listed above; review `docs/INSTALLATION.md` once more after any change to `install.sh`.
* **Test tooling:** the browser and terminal checks need Chromium's system libraries. They were supplied at user
  level here; a fresh machine needs them installed through its package manager.
