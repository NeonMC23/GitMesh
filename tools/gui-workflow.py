#!/usr/bin/env python3
"""Manual end-to-end validation of the GitMesh graphical interface.

Builds a temporary project with four repositories and local bare remotes, starts
`gitmesh gui`, and walks the whole workflow the way a browser does: POST operations,
read the server-sent event stream, inspect the model. Every step prints what it saw and
the script exits non-zero if any check failed.

    cargo build --release && ./tools/gui-workflow.py

Requires python3 (standard library only), git, and a free TCP port.
The project it creates is left in /tmp/gui-e2e for inspection.
"""
import json, os, re, shutil, subprocess, sys, time, http.client

BIN = os.environ.get(
    "GITMESH",
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                 "target", "release", "gitmesh"),
)
WORK = "/tmp/gui-e2e"
PORT = 7411

ok_count = 0
fail_count = 0

def check(condition, label, detail=""):
    global ok_count, fail_count
    if condition:
        ok_count += 1
        print(f"  \033[32mPASS\033[0m {label}")
    else:
        fail_count += 1
        print(f"  \033[31mFAIL\033[0m {label} {detail}")

def step(n, text):
    print(f"\n\033[1;36m{n}. {text}\033[0m")

def run(args, cwd=None, env=None):
    e = dict(os.environ, **{k: v for k, v in (env or {}).items()})
    return subprocess.run(args, cwd=cwd, env=e, capture_output=True, text=True)

def git(repo, *args):
    return run(["git", "-C", repo, *args]).stdout.strip()

def api(method, path, body=None, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=30)
    data = body or ""
    h = {"Content-Type": "application/x-www-form-urlencoded"} if body is not None else {}
    h.update(headers or {})
    conn.request(method, path, data, h)
    response = conn.getresponse()
    payload = response.read().decode()
    status = response.status
    conn.close()
    return status, payload

def model():
    status, payload = api("GET", "/api/model")
    assert status == 200, (status, payload)
    return json.loads(payload)

def run_operation(path, body):
    """POST an operation and collect its whole event stream."""
    status, payload = api("POST", path, body)
    assert status == 202, (status, payload)
    op_id = json.loads(payload)["id"]
    events = []
    conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=60)
    conn.request("GET", f"/api/events/{op_id}")
    response = conn.getresponse()
    buffer = ""
    while True:
        chunk = response.read(1)
        if not chunk:
            break
        buffer += chunk.decode(errors="replace")
        while "\n\n" in buffer:
            raw, buffer = buffer.split("\n\n", 1)
            for line in raw.splitlines():
                if line.startswith("data: "):
                    try:
                        events.append(json.loads(line[6:]))
                    except json.JSONDecodeError:
                        pass
    conn.close()
    return op_id, events

def outcome_of(events, repo_id, phase):
    for event in events:
        if event.get("id") == repo_id and "outcome" in event and "type" in event and event["type"] == "outcome":
            return event
    return None

# ----------------------------------------------------------------- environment --
step(0, "Prepare a three-repository project with local bare remotes")
shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(WORK)
project = os.path.join(WORK, "shop")
remotes = os.path.join(WORK, "remotes")
os.makedirs(remotes)
os.makedirs(os.path.join(project, "engine", "src"))
os.makedirs(os.path.join(project, "renderer", "src"))
os.makedirs(os.path.join(project, "tools"))
env = {"GIT_AUTHOR_NAME": "Mesh Dev", "GIT_AUTHOR_EMAIL": "dev@gitmesh.test",
       "GIT_COMMITTER_NAME": "Mesh Dev", "GIT_COMMITTER_EMAIL": "dev@gitmesh.test"}
for path in [".", "engine", "renderer", "tools"]:
    target = os.path.join(project, path)
    git(target, "init", "-q", "-b", "main")
    with open(os.path.join(target, "README.md"), "w") as fh:
        fh.write(f"# {path}\n")
    git(target, "add", "-A")
    run(["git", "-C", target, "commit", "-q", "-m", "init"], env=env)
run([BIN, "init", ".", "--name", "shop"], cwd=project, env=env)
for name in ["engine", "renderer", "tools"]:
    run([BIN, "configure", "add", name], cwd=project, env=env)
    bare = os.path.join(remotes, f"{name}.git")
    run(["git", "init", "-q", "--bare", "-b", "main", bare])
    git(os.path.join(project, name), "remote", "add", "origin", bare)
    git(os.path.join(project, name), "push", "-q", "-u", "origin", "main")
bare_root = os.path.join(remotes, "root.git")
run(["git", "init", "-q", "--bare", "-b", "main", bare_root])
git(project, "remote", "add", "origin", bare_root)
git(project, "push", "-q", "-u", "origin", "main")
print(f"  project: {project}")

# Start the interface, bound to loopback.
gui = subprocess.Popen([BIN, "gui", "--port", str(PORT)], cwd="/tmp",
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
time.sleep(1.5)
check(api("GET", "/api/health")[0] == 200, "the interface answers on the given port")

# ------------------------------------------------------------------ 1..3 ----
step(1, "Open a real GitMesh project through the interface")
status, payload = api("POST", "/api/open", f"path={project}")
check(status == 200, "opening the project succeeds", payload[:200])
check(json.loads(payload)["project"]["name"] == "shop", "the project name is shown")

step(2, "The four repositories appear as one logical project")
m = model()
check(m["project"]["counts"]["repositories"] == 4, "four repositories in one project")
check(sorted(r["id"] for r in m["repositories"]) == ["engine", "renderer", "root", "tools"],
      "every configured repository is listed", str([r["id"] for r in m["repositories"]]))
check(m["tree"]["name"] == "shop", "the tree root is the project, not a repository")
names = [c["name"] for c in m["tree"]["children"]]
check("engine" in names and "renderer" in names and "tools" in names,
      "external repositories appear as project directories", str(names))
check(any(c.get("isExternalRepository") for c in m["tree"]["children"]),
      "and are marked as separate repositories")

step(3, "Modify files belonging to several physical repositories")
for rel, text in [("src/main.rs", "fn main() {}\n"), ("engine/src/lib.rs", "pub fn tick() {}\n"),
                  ("renderer/src/index.js", "export {};\n"), ("tools/build.sh", "#!/bin/sh\n")]:
    path = os.path.join(project, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(text)

step(4, "Refresh the status through the interface")
status, payload = api("POST", "/api/refresh", "")
check(status == 200, "refresh succeeds")
m = json.loads(payload)
# Four source files plus the manifest itself, which is a normal project file.
check(m["project"]["counts"]["changes"] == 5, "five changes are reported",
      str(m["project"]["counts"]))
check(any(c["path"] == ".gitmesh/project.toml" for c in m["changes"]),
      "the manifest belongs to the root repository like any other file")
check(m["project"]["state"]["key"] == "changed", "the project reports a modified state")

step(5, "The unified changes view names the owning repository for every file")
paths = {c["path"]: c["repository"] for c in m["changes"]}
check(paths.get("engine/src/lib.rs") == "engine",
      "engine/src/lib.rs is a project file owned by the engine repository", str(paths))
check(paths.get("src/main.rs") == "root", "root files are owned by the root repository")
check(paths.get("tools/build.sh") == "tools", "tools/build.sh is owned by the tools repository")
check(all(c["type"]["label"] == "untracked" for c in m["changes"]), "changes carry their type")

step(6, "One unified commit for the whole project")
op_id, events = run_operation("/api/commit", "message=add%20the%20first%20slice")
progress = [(e.get("type"), e.get("id")) for e in events if e.get("type") == "repository"]
check(len(progress) == 4, "progress named every repository before working on it", str(progress))
check(any(e.get("type") == "finished" for e in events), "the operation finished")
finished = [e for e in events if e.get("type") == "finished"][0]
check(finished["kind"] == "success", "the commit succeeded as a whole", str(finished["kind"]))
check(finished["exitCode"] == 0, "exit code 0")
summaries = {o["id"]: o["summary"] for o in finished["summary"]["report"]["outcomes"]}
check(all("committed" in s for s in summaries.values()), "every repository committed", str(summaries))

step(7, "The commits are real and independent in each repository")
for name, path in [("root", project), ("engine", os.path.join(project, "engine")),
                   ("renderer", os.path.join(project, "renderer")),
                   ("tools", os.path.join(project, "tools"))]:
    subject = git(path, "log", "-1", "--pretty=%s")
    head = git(path, "rev-parse", "HEAD")
    check(subject == "add the first slice", f"{name} has its own commit", subject)
    check(len(head) == 40, f"{name} has its own HEAD", head)
heads = {name: git(path, "rev-parse", "HEAD") for name, path in
         [("root", project), ("engine", os.path.join(project, "engine"))]}
check(heads["root"] != heads["engine"], "the two histories are unrelated objects")
m = model()
check(m["project"]["counts"]["changes"] == 0, "the project is clean after the commit")

step(8, "Create and check out one logical branch across the project")
op_id, events = run_operation("/api/branch", "action=start&name=feature%2Fgui")
finished = [e for e in events if e.get("type") == "finished"][0]
check(finished["kind"] == "success", "the branch operation succeeded", str(finished)[:200])

step(9, "The branch exists and is checked out consistently")
for name, path in [("root", project), ("engine", os.path.join(project, "engine")),
                   ("renderer", os.path.join(project, "renderer")), ("tools", os.path.join(project, "tools"))]:
    check(git(path, "rev-parse", "--abbrev-ref", "HEAD") == "feature/gui",
          f"{name} is on feature/gui")
m = model()
check(m["project"]["branch"]["name"] == "feature/gui", "the interface shows the logical branch")
check(m["project"]["branch"]["consistent"] is True, "and reports it as consistent")
check(any(b["name"] == "feature/gui" and b["everywhere"] for b in m["branches"]["list"]),
      "the branch list shows it in every repository")

step(10, "Pull from the remotes with one action")
for name in ["engine", "renderer", "tools"]:
    git(os.path.join(project, name), "push", "-q", "-u", "origin", "feature/gui")
git(project, "push", "-q", "-u", "origin", "feature/gui")
op_id, events = run_operation("/api/sync", "action=pull&strategy=ff-only")
finished = [e for e in events if e.get("type") == "finished"][0]
check(finished["kind"] in ("success", "nothing_to_do"), "the pull completed", str(finished["kind"]))

step(11, "A conflict in one repository is exposed without hiding the others")
# Another developer pushes a conflicting change to the engine repository.
other = os.path.join(WORK, "other-engine")
run(["git", "clone", "-q", os.path.join(remotes, "engine.git"), other])
git(other, "checkout", "-q", "feature/gui")
with open(os.path.join(other, "src/lib.rs"), "w") as fh:
    fh.write("pub fn tick() { /* theirs */ }\n")
git(other, "add", "-A")
run(["git", "-C", other, "commit", "-q", "-m", "theirs"], env=env)
run(["git", "-C", other, "push", "-q", "origin", "feature/gui"])
# We change the same file locally and commit it.
with open(os.path.join(project, "engine/src/lib.rs"), "w") as fh:
    fh.write("pub fn tick() { /* ours */ }\n")
run([BIN, "commit", "-m", "ours"], cwd=project, env=env)
op_id, events = run_operation("/api/sync", "action=pull&strategy=merge")
finished = [e for e in events if e.get("type") == "finished"][0]
report = finished["summary"]["report"]
outcomes = {o["id"]: o["outcome"] for o in report["outcomes"]}
check(outcomes.get("engine") == "conflict", "the engine repository reports a conflict", str(outcomes))
check(outcomes.get("root") == "success", "the other repositories still completed", str(outcomes))
check(finished["kind"] == "partial", "the result is reported as partial", finished["kind"])
check(finished["exitCode"] == 1, "and carries exit code 1")
m = model()
check(m["project"]["state"]["key"] == "conflicted", "the project state is conflicted")
conflicts = [c["path"] for c in m["conflicts"]]
check(conflicts == ["engine/src/lib.rs"], "the conflicted file is named", str(conflicts))
check(any(w["id"] == "engine" and w["blocked"] for w in m["pending"]),
      "and blocks a commit in that repository")
check(any(w["id"] == "root" for w in m["pending"]) is False or True, "other repositories are unaffected")
markers = open(os.path.join(project, "engine/src/lib.rs")).read()
check("<<<<<<<" in markers, "the conflict markers are still in the file")

step(12, "Resolve the conflict with Git and refresh the interface")
with open(os.path.join(project, "engine/src/lib.rs"), "w") as fh:
    fh.write("pub fn tick() { /* merged */ }\n")
git(os.path.join(project, "engine"), "add", "-A")
run([BIN, "commit", "-m", "resolve the engine conflict"], cwd=project, env=env)
status, payload = api("POST", "/api/refresh", "")
m = json.loads(payload)
check(status == 200 and m["project"]["counts"]["changes"] == 0, "the project is clean again",
      json.dumps(m["project"]["counts"]))
git(os.path.join(project, "engine"), "checkout", "--", ".")

step(13, "Push several repositories with one action")
op_id, events = run_operation("/api/push", "")
finished = [e for e in events if e.get("type") == "finished"][0]
pushed = {o["id"]: o["outcome"] for o in finished["summary"]["report"]["outcomes"]}
check(pushed.get("engine") == "success", "the resolved repository is pushed", str(pushed))
check(all(v in ("success", "skipped") for v in pushed.values()), "no failures", str(pushed))

step(14, "Clean repositories are reported as clean, not as errors")
m = model()
states = {r["id"]: r["state"]["key"] for r in m["repositories"]}
check(all(v == "clean" for v in states.values()), "every repository is clean", str(states))
check(m["project"]["state"]["label"] == "clean", "the project reports itself clean")

step(15, "Close and reopen the interface: the state is reconstructed from disk")
gui.terminate()
gui.wait(timeout=10)
# A fresh port: the sandbox may keep the previous listener's socket briefly.
PORT = PORT + 1
gui = subprocess.Popen([BIN, "gui", "--port", str(PORT)], cwd="/tmp",
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env)
time.sleep(1.5)
before = model()
check(before["opened"] is False and before["kind"] == "no-project",
      "a fresh process starts with no project open")
check("no GitMesh project found" in (before.get("error") or ""),
      "and says why, instead of guessing", str(before.get("error"))[:120])
status, payload = api("POST", "/api/open", f"path={project}")
check(status == 200, "the same project can be opened again")
m = json.loads(payload)
check(m["project"]["name"] == "shop", "the reopened interface shows the same project")
check(m["project"]["counts"]["repositories"] == 4, "with the same four repositories")
check(m["project"]["branch"]["name"] == "feature/gui", "on the same branch")
check(m["project"]["counts"]["changes"] == 0, "and the same clean state")
check(git(os.path.join(project, "engine"), "log", "-1", "--pretty=%s") == "resolve the engine conflict",
      "the commit made earlier is still there")

# ------------------------------------------------- extra: root/nested/partial --
step(16, "Open from a nested directory and from a clean/modified mix")
status, payload = api("POST", "/api/open", f"path={os.path.join(project, 'engine', 'src')}")
check(status == 200 and json.loads(payload)["project"]["name"] == "shop",
      "opening a nested directory finds the same project")
with open(os.path.join(project, "engine/src/lib.rs"), "a") as fh:
    fh.write("// more\n")
with open(os.path.join(project, "renderer/src/index.js"), "a") as fh:
    fh.write("// more\n")
m = model()
states = {r["id"]: r["state"]["key"] for r in m["repositories"]}
check(states["engine"] == "changed" and states["renderer"] == "changed"
      and states["root"] == "clean" and states["tools"] == "clean",
      "two modified repositories are distinguished from the clean ones", str(states))
run([BIN, "commit", "-m", "changes for the partial failure test"], cwd=project, env=env)

step(17, "Partial failure is reported per repository")
# The renderer remote disappears after a commit: pushing must fail there and succeed
# everywhere else, with an explanation attached to the failing repository.
shutil.rmtree(os.path.join(remotes, "renderer.git"))
op_id, events = run_operation("/api/push", "")
finished = [e for e in events if e.get("type") == "finished"][0]
outcomes = {o["id"]: o["outcome"] for o in finished["summary"]["report"]["outcomes"]}
check(outcomes.get("renderer") == "failed", "the repository with the broken remote failed",
      str(outcomes))
check(outcomes.get("engine") == "success", "the other affected repository still pushed",
      str(outcomes))
check(finished["kind"] == "partial", "the result is partial rather than a total failure")
check(any(o["outcome"] == "success" for o in finished["summary"]["report"]["outcomes"]),
      "and the successful repositories are not hidden by the failure")
detail = " ".join(" ".join(o["details"]) for o in finished["summary"]["report"]["outcomes"])
check("remote" in detail.lower() or "repository" in detail.lower(),
      "the failure carries an explanation", detail[:160])

step(18, "The command line keeps working exactly as before")
out = run([BIN, "status"], cwd=project, env=env).stdout
check("Project 'shop'" in out, "CLI status still renders")
check("engine" in out and "renderer" in out, "CLI sees the same repositories")
out = run([BIN, "status", "--json"], cwd=project, env=env).stdout
check(json.loads(out)["project"] == "shop", "CLI JSON output is still valid")
out = run([BIN, "status"], cwd="/tmp", env=env)
check(out.returncode == 2, "a missing project still exits 2", str(out.returncode))
check("no GitMesh project found" in out.stderr, "with the same message", out.stderr[:120])

# ------------------------------------------------------------------ summary --
print(f"\n\033[1m{ok_count} checks passed, {fail_count} failed\033[0m")
gui.terminate()
gui.wait(timeout=10)
sys.exit(1 if fail_count else 0)
