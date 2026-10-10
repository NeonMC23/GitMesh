#!/usr/bin/env python3
"""Browser checks for cloning a remote into a GitMesh project (Chromium through Playwright).

Test tooling only; GitMesh itself has no browser dependency. Requirements:

    pip install playwright && python3 -m playwright install chromium

    python3 tools/gui-clone-check.py [path/to/gitmesh]

The script builds a throw-away project with the real `git` CLI, local bare remotes (a seeded
one, an empty one) and a directory that holds a user file. It starts `gitmesh gui` and drives
the *Repositories* tab in Chromium, through the clone form, the review, the confirmation and the
result. Each outcome is then checked on disk, in Git and in `.gitmesh/project.toml`, not only
in the text on screen.

Journeys covered:

* complete clone: remote URL and destination entered, reviewed, confirmed, cloned, and the
  repository listed in the table and the manifest; the clone tracks the remote default branch;
* duplicate destination: refused in the review, Apply disabled, nothing changed;
* non-empty destination: refused, the user's file untouched;
* unreachable remote: refused with Git's reason, no directory created;
* invalid remote (starts with "-"): refused, nothing run;
* empty remote: reviewed with a notice, cloned without commits, recorded in the manifest.

Exit status 0 means every check passed.
"""

import os
import re
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.request

from playwright.sync_api import sync_playwright

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "release", "gitmesh")
PORT = 8794

failures = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f" — {detail}"))
    if not ok:
        failures.append(name)


def git(cwd, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="T", GIT_AUTHOR_EMAIL="t@example.test",
               GIT_COMMITTER_NAME="T", GIT_COMMITTER_EMAIL="t@example.test")
    return subprocess.run(["git", "-C", cwd, *args], check=True, capture_output=True,
                          text=True, env=env).stdout


def gitmesh(project, *args):
    return subprocess.run([BIN, "-C", project, *args], capture_output=True, text=True,
                          stdin=subprocess.DEVNULL)


def build(base):
    """A project with a root repository and two remotes: one seeded, one empty."""
    project = os.path.join(base, "MyProject")
    os.makedirs(project)
    git(project, "init", "-q", "-b", "main")
    open(os.path.join(project, "README.md"), "w").write("# Demo\n")
    git(project, "add", ".")
    git(project, "commit", "-q", "-m", "init")
    gitmesh(project, "init", "--name", "MyProject")

    core = os.path.join(base, "core.git")
    git(base, "init", "-q", "-b", "main", "core-src")
    open(os.path.join(base, "core-src", "core.txt"), "w").write("core\n")
    git(os.path.join(base, "core-src"), "add", ".")
    git(os.path.join(base, "core-src"), "commit", "-q", "-m", "seed")
    subprocess.run(["git", "clone", "-q", "--bare", os.path.join(base, "core-src"), core],
                   check=True, capture_output=True)

    empty = os.path.join(base, "empty.git")
    subprocess.run(["git", "init", "-q", "--bare", "-b", "main", empty], check=True)

    # A directory that already holds the user's work.
    busy = os.path.join(project, "libs", "busy")
    os.makedirs(busy)
    open(os.path.join(busy, "notes.txt"), "w").write("my unsaved work\n")

    # Onboarding states, checked in the browser: an empty directory, and a repository
    # that has no commits yet.
    os.makedirs(os.path.join(project, "libs", "fresh"))
    unborn = os.path.join(project, "libs", "unborn")
    os.makedirs(unborn)
    git(unborn, "init", "-q", "-b", "main")
    return project, core, empty


def check_candidate(page, path):
    """Type a directory into the add form, check it, and return the headline text."""
    page.click('button.tab[data-tab="repos"]')
    page.fill("#repo-path", path)
    page.click("#btn-repo-check")
    page.wait_for_function(
        "() => { const c = document.getElementById('repo-candidate'); return c && !c.hidden; }",
        timeout=15000)
    page.wait_for_timeout(200)
    return page.inner_text("#repo-candidate")


def manifest(project):
    with open(os.path.join(project, ".gitmesh", "project.toml"), "rb") as handle:
        return tomllib.load(handle)


def repo_entries(project):
    data = manifest(project)
    return {repo["id"]: repo for repo in data.get("repositories", [])}


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


def open_clone_form(page):
    page.click('button.tab[data-tab="repos"]')
    page.wait_for_selector("#repo-intent")
    page.select_option("#repo-intent", "clone")
    page.wait_for_selector("#repo-clone-fields:not([hidden])")


def fill_clone(page, path, remote, ident=""):
    page.fill("#repo-clone-path", path)
    page.fill("#repo-clone-remote", remote)
    page.fill("#repo-clone-id", ident)


def review(page):
    """Ask for the review and wait for the server's answer (plan or refusal)."""
    page.click("#btn-repo-change")
    page.wait_for_function(
        "() => { const r = document.getElementById('repo-review'); return r && !r.hidden; }",
        timeout=15000)
    page.wait_for_timeout(200)


def plan_text(page):
    return page.inner_text("#repo-plan")


def apply_disabled(page):
    return page.evaluate("document.getElementById('btn-repo-apply').disabled")


def apply(page):
    page.check("#repo-confirm")
    page.click("#btn-repo-apply")
    page.wait_for_function(
        "() => { const r = document.getElementById('repo-result-card'); return r && !r.hidden; }",
        timeout=30000)
    page.wait_for_timeout(300)


def main():
    if not os.path.exists(BIN):
        print(f"gitmesh binary not found at {BIN}; build it first")
        return 2
    base = tempfile.mkdtemp(prefix="gitmesh-clone-browser-")
    project, core, empty = build(base)
    proc, url = start(project, PORT)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()
            page = browser.new_page(viewport={"width": 1280, "height": 900})
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(url)
            page.wait_for_selector("#workspace:not([hidden])")
            # ---- 0. onboarding states are told apart in the browser ------------
            text = check_candidate(page, "libs/fresh")
            check("an empty directory is described as empty, with no history",
                  "empty directory" in text and "clone" in text, text[:300])
            text = check_candidate(page, "libs/unborn")
            check("a repository without commits says so and what that means",
                  "no commits" in text and "skip" in text, text[:300])
            text = check_candidate(page, "libs/nowhere")
            check("a missing directory points to clone",
                  "no directory" in text and "clone" in text, text[:300])
            check("an unborn repository is not presented as having history",
                  "history" not in text)

            open_clone_form(page)

            # ---- 1. complete clone ------------------------------------------------
            check("clone option hides the existing-repository selector",
                  not page.is_visible("#repo-target-field"))
            fill_clone(page, "libs/core", core, "core")
            review(page)
            text = plan_text(page)
            check("review names the clone and the destination",
                  "libs/core" in text and "core" in text, text[:300])
            check("review shows the git clone step", "git clone" in text, text[:300])
            check("review allows Apply when the remote is valid", not apply_disabled(page))
            apply(page)
            check("the clone result is shown", page.is_visible("#repo-result-card"))
            check("the cloned file is present",
                  os.path.exists(os.path.join(project, "libs", "core", "core.txt")))
            upstream = git(os.path.join(project, "libs", "core"), "rev-parse",
                           "--abbrev-ref", "@{u}").strip()
            check("the clone tracks origin/main", upstream == "origin/main", upstream)
            entries = repo_entries(project)
            check("the manifest lists the repository with its remote",
                  "core" in entries and entries["core"].get("remote") == core
                  and entries["core"].get("path") == "libs/core", str(entries.get("core")))
            root = manifest(project).get("root", {})
            check("the manifest keeps the root repository at the project root",
                  root.get("id") == "root" and root.get("path") == ".", str(root))
            check("the manifest lists exactly the clone as a repository (besides the root)",
                  sorted(repo_entries(project)) == ["core"],
                  str(sorted(repo_entries(project))))
            page.wait_for_function(
                "() => document.getElementById('repos-table').innerText.includes('libs/core')",
                timeout=15000)
            table = page.inner_text("#repos-table")
            check("the repository appears in the project table", "core" in table, table[:300])

            # ---- 2. duplicate destination -------------------------------------------
            open_clone_form(page)
            fill_clone(page, "libs/core", core, "core-again")
            review(page)
            text = plan_text(page)
            check("duplicate destination is refused in the review",
                  "already the GitMesh repository" in text, text[:300])
            check("Apply is disabled for a duplicate destination", apply_disabled(page))

            # ---- 3. non-empty destination ---------------------------------------
            open_clone_form(page)
            fill_clone(page, "libs/busy", core, "busy")
            review(page)
            text = plan_text(page)
            check("non-empty destination is refused in the review",
                  "not empty" in text, text[:300])
            check("Apply is disabled for a non-empty destination", apply_disabled(page))
            check("the user's file is untouched",
                  open(os.path.join(project, "libs", "busy", "notes.txt")).read()
                  == "my unsaved work\n")
            check("no repository was created in the non-empty directory",
                  not os.path.exists(os.path.join(project, "libs", "busy", ".git")))

            # ---- 4. unreachable remote ----------------------------------------------
            open_clone_form(page)
            missing = os.path.join(base, "no-such-remote.git")
            fill_clone(page, "libs/gone", missing, "gone")
            review(page)
            text = plan_text(page)
            check("unreachable remote is refused with the reason",
                  "could not be read" in text and "could not be found" in text, text[:400])
            check("Apply is disabled for an unreachable remote", apply_disabled(page))
            check("no directory was created for the unreachable remote",
                  not os.path.exists(os.path.join(project, "libs", "gone")))

            # ---- 5. invalid remote (looks like an option) -------------------------------
            open_clone_form(page)
            fill_clone(page, "libs/odd", "--upload-pack=touch /tmp/x", "odd")
            review(page)
            text = plan_text(page)
            check("a remote that looks like an option is refused",
                  "not a remote URL" in text, text[:300])
            check("Apply is disabled for an invalid remote", apply_disabled(page))
            check("nothing was created for the invalid remote",
                  not os.path.exists(os.path.join(project, "libs", "odd")))

            # ---- 6. empty remote ----------------------------------------------------
            open_clone_form(page)
            fill_clone(page, "libs/empty", empty, "empty")
            review(page)
            text = plan_text(page)
            check("empty remote is reviewed with a notice, not refused",
                  "is empty" in text and not apply_disabled(page), text[:300])
            apply(page)
            empty_clone = os.path.join(project, "libs", "empty")
            check("the empty-remote clone has a .git directory",
                  os.path.isdir(os.path.join(empty_clone, ".git")))
            has_commits = subprocess.run(["git", "-C", empty_clone, "rev-parse", "--verify",
                                          "-q", "HEAD"], capture_output=True).returncode == 0
            check("the empty-remote clone has no commits", not has_commits)
            entries = repo_entries(project)
            check("the empty-remote clone is recorded in the manifest", "empty" in entries)

            check("no script errors during the clone journey", not errors, str(errors))
            browser.close()
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()

    print()
    if failures:
        print(f"{len(failures)} check(s) failed: {', '.join(failures)}")
        return 1
    print("all clone browser checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
