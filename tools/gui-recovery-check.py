#!/usr/bin/env python3
"""Browser checks for the "Clone repository" action offered by a refused add.

Test tooling only; GitMesh itself has no browser dependency. Requirements:

    pip install playwright && python3 -m playwright install chromium

    python3 tools/gui-recovery-check.py [path/to/gitmesh]

The script builds a throw-away project with the real `git` CLI and a local bare remote that
already has history. An empty directory is added in the *Repositories* tab: the add is refused
because the remote already has history, and the refusal offers "Clone repository". The script
checks, in Chromium, that the offer fills the clone form, that the core plans it, that Cancel
changes nothing, and that Apply clones the remote history. Each outcome is checked on disk and
in `.gitmesh/project.toml`, not only in the text on screen.

Exit status 0 means every check passed.
"""

import os
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request

from playwright.sync_api import sync_playwright

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "release", "gitmesh")
PORT = 8795

failures = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f" — {detail}"))
    if not ok:
        failures.append(name)


def git(cwd, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="T", GIT_AUTHOR_EMAIL="t@example.test",
               GIT_COMMITTER_NAME="T", GIT_COMMITTER_EMAIL="t@example.test")
    return subprocess.run(["git", "-C", cwd, *args], check=True, capture_output=True,
                          text=True, env=env).stdout.strip()


def gitmesh(project, *args):
    return subprocess.run([BIN, "-C", project, *args], capture_output=True, text=True,
                          stdin=subprocess.DEVNULL)


def build(base):
    """A project with a root repository, a remote with history, and an empty directory."""
    project = os.path.join(base, "MyProject")
    os.makedirs(project)
    git(project, "init", "-q", "-b", "main")
    open(os.path.join(project, "README.md"), "w").write("# Demo\n")
    git(project, "add", ".")
    git(project, "commit", "-q", "-m", "init")
    gitmesh(project, "init", "--name", "MyProject")

    seed = os.path.join(base, "ram-src")
    git(base, "init", "-q", "-b", "main", "ram-src")
    open(os.path.join(seed, "src.rs"), "w").write("fn main() {}\n")
    git(seed, "add", ".")
    git(seed, "commit", "-q", "-m", "RAMforge seed")
    remote = os.path.join(base, "ramforge.git")
    subprocess.run(["git", "clone", "-q", "--bare", seed, remote], check=True,
                   capture_output=True)
    os.makedirs(os.path.join(project, "libs", "ram"))
    os.makedirs(os.path.join(project, "libs", "other"))
    return project, remote


def manifest(project):
    with open(os.path.join(project, ".gitmesh", "project.toml"), "rb") as handle:
        return tomllib.load(handle)


def start(directory, port):
    proc = subprocess.Popen([BIN, "-C", directory, "gui", "--port", str(port)],
                            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, text=True)
    url = f"http://127.0.0.1:{port}/"
    for _ in range(100):
        try:
            urllib.request.urlopen(url, timeout=1)
            return proc, url
        except Exception:
            time.sleep(0.1)
    proc.kill()
    raise RuntimeError("gitmesh gui did not start")


def refused_add(page, path, remote):
    """Check the directory and ask to add it with the remote; the refusal is shown."""
    page.click('button.tab[data-tab="repos"]')
    page.fill("#repo-path", path)
    page.click("#btn-repo-check")
    page.wait_for_function(
        "() => { const c = document.getElementById('repo-candidate'); return c && !c.hidden; }",
        timeout=15000)
    page.wait_for_function(
        "() => !document.getElementById('repo-add-form').hidden", timeout=15000)
    page.fill("#repo-remote-url", remote)
    page.check("#repo-configure-remote")
    page.click("#btn-repo-review")
    page.wait_for_function(
        "() => !document.getElementById('repo-review').hidden", timeout=15000)
    page.wait_for_timeout(200)


def main():
    base = tempfile.mkdtemp(prefix="gitmesh-recovery-")
    project, remote = build(base)
    head_before = git(project, "rev-parse", "HEAD")
    manifest_before = open(os.path.join(project, ".gitmesh", "project.toml")).read()
    proc, url = start(project, PORT)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page()
            errors = []
            page.on("pageerror", lambda error: errors.append(str(error)))

            # 1. The refusal offers the clone, with its inputs, and does not apply anything.
            page.goto(url)
            refused_add(page, "libs/ram", remote)
            button = page.locator('button[data-recovery="0"]')
            check("the refused add offers Clone repository",
                  button.count() == 1 and button.is_visible(),
                  "no Clone repository button in the refusal")
            plan_text = page.inner_text("#repo-plan")
            check("the refusal says why, in plain words",
                  "already has history" in plan_text, plan_text[:200])
            check("the refusal names the exact command",
                  "gitmesh configure clone libs/ram --remote" in plan_text, plan_text[:300])
            check("Apply is disabled while the add is refused",
                  page.is_disabled("#btn-repo-apply"))
            check("nothing is written while the refusal is shown",
                  open(os.path.join(project, ".gitmesh", "project.toml")).read()
                  == manifest_before)

            # 2. Cancel changes nothing and hides the review.
            page.click("#btn-repo-discard")
            page.wait_for_timeout(200)
            check("Cancel hides the review",
                  page.is_hidden("#repo-review"), "the review is still visible")
            check("Cancel says that nothing was changed",
                  "Nothing was changed" in page.inner_text("#repos-message"),
                  page.inner_text("#repos-message"))
            check("Cancel leaves the directory empty",
                  os.listdir(os.path.join(project, "libs", "ram")) == [])

            # 3. Clone repository fills the clone form and reviews the clone (core plan).
            refused_add(page, "libs/ram", remote)
            page.click('button[data-recovery="0"]')
            page.wait_for_function(
                "() => !document.getElementById('repo-review').hidden", timeout=15000)
            page.wait_for_timeout(300)
            check("the clone form is filled with the destination",
                  page.input_value("#repo-clone-path") == "libs/ram",
                  page.input_value("#repo-clone-path"))
            check("the clone form is filled with the remote",
                  page.input_value("#repo-clone-remote") == remote)
            check("the intent is set to clone",
                  page.input_value("#repo-intent") == "clone")
            plan_text = page.inner_text("#repo-plan")
            check("the clone plan shows the git clone step",
                  "git clone" in plan_text, plan_text[:300])
            check("the ready clone plan can be applied",
                  page.is_enabled("#btn-repo-apply"))

            # 4. Apply needs the explicit confirmation: without it nothing runs.
            page.click("#btn-repo-apply")
            page.wait_for_timeout(300)
            check("Apply without confirmation is refused with a reason",
                  "reviewed" in page.inner_text("#repos-message"),
                  page.inner_text("#repos-message"))
            check("nothing is cloned before confirmation",
                  not os.path.exists(os.path.join(project, "libs", "ram", ".git")))

            # Confirm and apply: the directory is a clone, the manifest lists it.
            page.check("#repo-confirm")
            page.click("#btn-repo-apply")
            page.wait_for_function(
                "() => document.getElementById('repo-result-card') && "
                "!document.getElementById('repo-result-card').hidden", timeout=60000)
            page.wait_for_timeout(300)
            clone = os.path.join(project, "libs", "ram")
            check("the directory is now a clone",
                  os.path.isdir(os.path.join(clone, ".git")))
            check("the clone has the remote's history",
                  git(clone, "rev-parse", "HEAD") == git(remote, "rev-parse", "main"))
            check("the sources are in the clone directory",
                  os.path.isfile(os.path.join(clone, "src.rs")))
            repos = {r["id"]: r for r in manifest(project).get("repositories", [])}
            check("the manifest lists the clone with its remote",
                  repos.get("ram", {}).get("remote") == remote, str(repos))
            check("the root repository is unchanged",
                  git(project, "rev-parse", "HEAD") == head_before)
            check("the root does not track the clone's files",
                  "libs/ram/" not in git(project, "ls-files"))
            check("the other empty sibling directory is untouched",
                  os.listdir(os.path.join(project, "libs", "other")) == [])
            check("no script errors during the recovery journey", not errors, str(errors))
            browser.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    if failures:
        print(f"\n{len(failures)} recovery browser check(s) failed")
        return 1
    print("\nall recovery browser checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
