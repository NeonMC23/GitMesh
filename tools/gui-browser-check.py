#!/usr/bin/env python3
"""Browser checks for the GitMesh graphical interface (Chromium through Playwright).

Test tooling only; GitMesh itself has no browser dependency. Requirements:

    pip install playwright && python3 -m playwright install chromium

    python3 tools/gui-browser-check.py [path/to/gitmesh]

The script creates its own throw-away projects with the real `git` CLI, starts
`gitmesh gui` on two local ports, and checks in Chromium:

* responsive layout: no horizontal page overflow at 360, 768 and 1280 px;
* first-use onboarding in a directory that is not a project;
* navigation through every tab, and the overview's next-step action;
* keyboard focus is visible on a focused button;
* no JavaScript errors while navigating.

Exit status 0 means every check passed.
"""

import os
import subprocess
import sys
import tempfile
import time
import urllib.request

from playwright.sync_api import sync_playwright

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.join(ROOT, "target", "release", "gitmesh")
PORT_PROJECT = 8791
PORT_EMPTY = 8792

failures = []


def check(name, ok, detail=""):
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f" — {detail}"))
    if not ok:
        failures.append(name)


def git(cwd, *args):
    env = dict(os.environ, GIT_AUTHOR_NAME="T", GIT_AUTHOR_EMAIL="t@example.test",
               GIT_COMMITTER_NAME="T", GIT_COMMITTER_EMAIL="t@example.test")
    subprocess.run(["git", "-C", cwd, *args], check=True, capture_output=True, env=env)


def make_project(base):
    project = os.path.join(base, "MyProject")
    os.makedirs(project)
    git(project, "init", "-q", "-b", "main")
    open(os.path.join(project, "README.md"), "w").write("# Demo\n")
    git(project, "add", ".")
    git(project, "commit", "-q", "-m", "init")
    engine = os.path.join(project, "engine_with_a_deliberately_long_directory_name_for_layout")
    os.makedirs(engine)
    git(engine, "init", "-q", "-b", "main")
    open(os.path.join(engine, "lib.rs"), "w").write("fn a() {}\n")
    git(engine, "add", ".")
    git(engine, "commit", "-q", "-m", "engine init")
    open(os.path.join(engine, "lib.rs"), "a").write("fn b() {}\n")
    subprocess.run([BIN, "-C", project, "init", "--name", "MyProject"], check=True,
                   capture_output=True, stdin=subprocess.DEVNULL)
    subprocess.run([BIN, "-C", project, "configure", "add",
                    "engine_with_a_deliberately_long_directory_name_for_layout"],
                   check=True, capture_output=True, stdin=subprocess.DEVNULL)
    return project


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


def main():
    if not os.path.exists(BIN):
        print(f"gitmesh binary not found at {BIN}; build it first")
        return 2
    base = tempfile.mkdtemp(prefix="gitmesh-browser-")
    project = make_project(base)
    empty = os.path.join(base, "not-a-project")
    os.makedirs(empty)

    proc_project, url_project = start(project, PORT_PROJECT)
    proc_empty, url_empty = start(empty, PORT_EMPTY)
    try:
        with sync_playwright() as p:
            browser = p.chromium.launch()

            # ---- responsive layout -------------------------------------------
            for width in (360, 768, 1280):
                page = browser.new_page(viewport={"width": width, "height": 800})
                errors = []
                page.on("pageerror", lambda e: errors.append(str(e)))
                page.goto(url_project)
                page.wait_for_selector("#workspace:not([hidden])")
                overflow = page.evaluate("document.documentElement.scrollWidth")
                check(f"no page overflow at {width}px", overflow <= width + 1,
                      f"scrollWidth {overflow}")
                check(f"no script errors at {width}px", not errors, str(errors))
                page.close()

            # ---- navigation, next step, focus --------------------------------
            page = browser.new_page(viewport={"width": 1280, "height": 900})
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(url_project)
            page.wait_for_selector("#workspace:not([hidden])")
            page.wait_for_timeout(300)

            tabs = page.locator("button.tab")
            check("six navigation views", tabs.count() == 6, f"found {tabs.count()}")

            for name in ["changes", "branches", "sync", "repos", "settings", "status"]:
                page.click(f'button.tab[data-tab="{name}"]')
                visible = page.is_visible(f"#panel-{name}")
                check(f"tab '{name}' shows its panel", visible)
                others = [n for n in ["status", "changes", "branches", "sync", "repos", "settings"]
                          if n != name and page.is_visible(f"#panel-{n}")]
                check(f"tab '{name}' hides the others", not others, str(others))

            page.click('button.tab[data-tab="status"]')
            page.wait_for_timeout(200)
            next_text = page.inner_text("#next-step")
            check("overview states the next step", "not committed yet" in next_text, next_text)
            page.click('#next-step button[data-go="changes"]')
            check("next-step action opens Changes",
                  page.is_visible("#panel-changes") and page.is_visible("#commit-card"))

            page.click('button.tab[data-tab="status"]')
            page.click("#btn-refresh")
            page.focus("#btn-refresh")
            page.keyboard.press("Shift+Tab")
            page.keyboard.press("Tab")
            ring = page.evaluate(
                "getComputedStyle(document.activeElement).boxShadow")
            check("keyboard focus is visible", ring not in ("none", ""), ring)

            # Concept note is collapsed by default and opens with one click.
            check("concept note collapsed by default",
                  not page.evaluate("document.querySelector('details.concept').open"))
            page.click("details.concept summary")
            check("concept note opens",
                  page.evaluate("document.querySelector('details.concept').open"))

            check("no script errors while navigating", not errors, str(errors))
            page.close()

            # ---- first use: a directory that is not a project -----------------
            page = browser.new_page(viewport={"width": 390, "height": 800})
            errors = []
            page.on("pageerror", lambda e: errors.append(str(e)))
            page.goto(url_empty)
            page.wait_for_selector("#welcome:not([hidden])")
            check("first use explains that no project is open",
                  "No GitMesh project is open" in page.inner_text("#welcome"))
            check("first use offers setup", page.is_visible("#btn-welcome-setup"))
            overflow = page.evaluate("document.documentElement.scrollWidth")
            check("first-use screen fits a phone", overflow <= 391, f"scrollWidth {overflow}")
            check("no script errors on first use", not errors, str(errors))
            page.close()

            browser.close()
    finally:
        for proc in (proc_project, proc_empty):
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()

    print()
    if failures:
        print(f"{len(failures)} check(s) failed: {', '.join(failures)}")
        return 1
    print("all browser checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
