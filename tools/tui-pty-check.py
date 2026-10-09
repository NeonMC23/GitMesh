#!/usr/bin/env python3
"""Terminal-level checks for `gitmesh ui`, driven through a real pseudo-terminal.

Requires the `pyte` terminal emulator (`pip install pyte`); it is test tooling only and
is not a GitMesh dependency. Builds throw-away Git repositories under a temporary
directory with the real `git` CLI, runs the release binary, sends keys, and reads the
rendered screen back.

    python3 tools/tui-pty-check.py [path/to/gitmesh]

Exit status 0 means every check passed. Each check prints PASS or FAIL with the screen
on failure.
"""

import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

import pyte

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "release", "gitmesh")
ENTER = "\r"
TAB = "\t"
ESC = "\x1b"
RIGHT = "\x1b[C"
LEFT = "\x1b[D"
BACKSPACE = "\x7f"


class Session:
    """One `gitmesh ui` process attached to a pyte screen."""

    def __init__(self, cwd, cols, rows):
        self.cols, self.rows = cols, rows
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(cwd)
            os.environ["TERM"] = "xterm-256color"
            os.environ["GIT_TERMINAL_PROMPT"] = "0"
            os.execv(BIN, [BIN, "ui"])
        self._resize(cols, rows)
        self.pump(1.2)

    def _resize(self, cols, rows):
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def resize(self, cols, rows):
        self.cols, self.rows = cols, rows
        self.screen.resize(rows, cols)
        self._resize(cols, rows)
        self.pump(0.8)

    def pump(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            ready, _, _ = select.select([self.fd], [], [], 0.05)
            if ready:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    return
                self.stream.feed(data)

    def send(self, keys, wait=0.7):
        os.write(self.fd, keys.encode())
        self.pump(wait)

    def text(self):
        return "\n".join(line.rstrip() for line in self.screen.display)

    def wait_for(self, needle, timeout=8.0):
        end = time.time() + timeout
        while time.time() < end:
            if needle in self.text():
                return True
            self.pump(0.2)
        return needle in self.text()

    def exited(self, timeout=5.0):
        end = time.time() + timeout
        while time.time() < end:
            pid, _ = os.waitpid(self.pid, os.WNOHANG)
            if pid == self.pid:
                return True
            self.pump(0.1)
        return False

    def close(self):
        try:
            os.kill(self.pid, 9)
            os.waitpid(self.pid, 0)
        except OSError:
            pass


def git(cwd, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="T", GIT_AUTHOR_EMAIL="t@example.test",
               GIT_COMMITTER_NAME="T", GIT_COMMITTER_EMAIL="t@example.test")
    out = subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True, env=env)
    if out.returncode != 0:
        raise RuntimeError(f"git {args} in {cwd}: {out.stderr}")
    return out.stdout


def gitmesh(cwd, *args):
    return subprocess.run([BIN, *args], cwd=cwd, capture_output=True, text=True,
                          stdin=subprocess.DEVNULL)


def build_fixture(base):
    """root (with README), engine (nested, pushed to a bare remote), lib (local only)."""
    project = os.path.join(base, "project")
    remotes = os.path.join(base, "remotes")
    os.makedirs(project)
    os.makedirs(remotes)
    git(project, "init", "-q", "-b", "main")
    open(os.path.join(project, "README.md"), "w").write("# Demo\n")
    git(project, "add", ".")
    git(project, "commit", "-q", "-m", "init")

    engine = os.path.join(project, "engine")
    os.makedirs(engine)
    git(engine, "init", "-q", "-b", "main")
    open(os.path.join(engine, "lib.rs"), "w").write("pub fn a() {}\n")
    git(engine, "add", ".")
    git(engine, "commit", "-q", "-m", "engine init")

    lib = os.path.join(project, "lib")
    os.makedirs(lib)
    git(lib, "init", "-q", "-b", "main")
    open(os.path.join(lib, "lib.txt"), "w").write("lib\n")
    git(lib, "add", ".")
    git(lib, "commit", "-q", "-m", "lib init")

    bare_root = os.path.join(remotes, "root.git")
    bare_engine = os.path.join(remotes, "engine.git")
    for bare in (bare_root, bare_engine):
        subprocess.run(["git", "init", "-q", "--bare", "-b", "main", bare], check=True)
    git(project, "remote", "add", "origin", bare_root)
    git(project, "push", "-q", "-u", "origin", "main")
    git(engine, "remote", "add", "origin", bare_engine)
    git(engine, "push", "-q", "-u", "origin", "main")

    # Real changes: a modified tracked file in the root and in the engine, and a new
    # untracked file in the engine.
    open(os.path.join(project, "README.md"), "a").write("more\n")
    open(os.path.join(engine, "lib.rs"), "a").write("pub fn b() {}\n")
    open(os.path.join(engine, "extra.rs"), "w").write("pub fn c() {}\n")

    out = gitmesh(project, "init", "--name", "Demo")
    assert out.returncode == 0, out.stderr
    out = gitmesh(project, "configure", "add", "engine")
    assert out.returncode == 0, out.stderr
    out = gitmesh(project, "configure", "add", "lib")
    assert out.returncode == 0, out.stderr
    return project, bare_engine


failures = []


def check(name, condition, session=None):
    if condition:
        print(f"PASS {name}")
    else:
        print(f"FAIL {name}")
        if session is not None:
            print(session.text())
        failures.append(name)


def main():
    if not os.path.exists(BIN):
        print(f"gitmesh binary not found at {BIN}; build it first")
        return 2
    base = tempfile.mkdtemp(prefix="gitmesh-pty-")
    project, bare_engine = build_fixture(base)

    # 1. Normal size: layout, grouping, actions, footer.
    s = Session(project, 100, 30)
    check("main screen renders the project and its branch", s.wait_for("Demo") and "branch main" in s.text(), s)
    check("changes are grouped by repository", "Changes by repository" in s.text() and "engine" in s.text(), s)
    check("untracked files show ??", "??" in s.text(), s)
    check("action bar offers the four actions", all(x in s.text() for x in ["Stage all", "Commit", "Pull", "Push"]), s)
    check("footer lists only real shortcuts", "s stage all" in s.text() and "P push" in s.text(), s)
    s.close()

    # 2. Constrained size: resize notice, then recovery on resize back.
    s = Session(project, 100, 30)
    s.resize(40, 10)
    check("too-small terminal shows a resize notice", "Terminal too small" in s.text(), s)
    s.resize(100, 30)
    check("growing the terminal restores the screen", "Changes by repository" in s.text(), s)
    s.resize(60, 16)
    check("a 60x16 terminal still shows the action bar", "Stage all" in s.text(), s)
    s.close()

    # 3. Help overlay and closing it.
    s = Session(project, 100, 30)
    s.send("?")
    check("? opens the key reference", "Keys" in s.text() and "stage all" in s.text(), s)
    s.send("x")
    check("any key closes the key reference", "Changes by repository" in s.text(), s)

    # 4. Commit before staging is refused with a useful reason.
    s.send(TAB)
    s.send("early")
    s.send(TAB)
    s.send("c")
    check("commit before staging explains what to do", "nothing is staged" in s.text(), s)
    # Clear the text typed above so the real commit below has exactly its own message.
    s.send(TAB)
    s.send(BACKSPACE * len("early"))
    s.send(TAB)

    # 5. Stage all, then commit with confirmation, against the real repositories.
    s.send("s", wait=1.5)
    check("stage all reports the staged repositories", "staged" in s.text() and "Stage all:" in s.text(), s)
    engine_staged = git(os.path.join(project, "engine"), "diff", "--cached", "--name-only").strip()
    check("stage all staged both engine files, and only those",
          set(engine_staged.split()) == {"lib.rs", "extra.rs"}, s)
    root_cached = git(project, "diff", "--cached", "--name-only").strip()
    check("stage all did not stage the nested repository's files in the root", "engine/" not in root_cached and "lib.rs" not in root_cached, s)

    s.send(TAB)
    s.send("tidy up")
    s.send(TAB)
    s.send("c")
    check("commit asks for confirmation", "press Enter or c to commit" in s.text(), s)
    s.send(ENTER, wait=1.5)
    check("confirmed commit reports the repositories", "Commit: done in" in s.text(), s)
    subject = git(os.path.join(project, "engine"), "log", "-1", "--pretty=%s").strip()
    check("engine received a real commit with the message", subject == "tidy up", s)
    check("the message field was cleared", "write the commit message" in s.text(), s)

    # 6. Pull reports per repository; local-only lib is skipped, not failed.
    s.send(RIGHT)
    s.send(RIGHT)
    s.send(ENTER, wait=1.5)
    check("pull result shows per-repository lines", "Pull:" in s.text(), s)
    check("local-only repository is reported as skipped", "- lib" in s.text() and "no remote" in s.text(), s)
    s.close()

    # 7. Partial failure on push: both repositories have a commit ahead of their remote,
    # then the engine's remote disappears, so its push must fail.
    open(os.path.join(project, "README.md"), "a").write("root work\n")
    open(os.path.join(project, "engine", "lib.rs"), "a").write("// engine work\n")
    git(project, "commit", "-q", "-am", "root work")
    git(os.path.join(project, "engine"), "commit", "-q", "-am", "engine work")
    git(os.path.join(project, "engine"), "remote", "set-url", "origin", "/nonexistent/gone.git")
    s = Session(project, 100, 30)
    s.send("P", wait=2.0)
    check("partial push failure is never reported as success",
          "NOT everything succeeded" in s.text(), s)
    check("the failing repository is marked as failed", "✗ engine" in s.text(), s)
    s.close()

    # 8. Quit leaves the program cleanly.
    s = Session(project, 100, 30)
    s.send("q")
    check("q quits", s.exited(), s)
    s.close()

    # 9. Ctrl-C quits too.
    s = Session(project, 100, 30)
    s.send("\x03")
    check("Ctrl-C quits", s.exited(), s)
    s.close()

    print()
    if failures:
        print(f"{len(failures)} check(s) failed: {', '.join(failures)}")
        return 1
    print("all terminal checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
