#!/usr/bin/env python3
"""End-to-end validation of post-creation repository management.

Creates a GitMesh project with a root repository, an existing repository, plain
directories and local bare remotes, then drives `gitmesh gui` the way a browser does:
it inspects candidate directories, reviews plans, confirms them, streams the progress,
and checks the result against the filesystem and the Git history. The command line is
exercised on the same project, because both front ends share one service.

    cargo build --release && ./tools/repository-workflow.py

Requires python3 (standard library only) and git. The project it creates is left in
/tmp/gitmesh-repositories-e2e for inspection. Ports 7414 and 7415 are used, so this
script never collides with the setup and interface workflows (7411-7413).
"""
import atexit, hashlib, json, os, shutil, socket, stat, subprocess, sys, time, http.client
from urllib.parse import urlencode

BIN = os.environ.get(
    "GITMESH",
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                 "target", "release", "gitmesh"),
)
WORK = "/tmp/gitmesh-repositories-e2e"
PORT = 7414
SPARE_PORT = 7415

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
    return subprocess.run(args, cwd=cwd, env=dict(os.environ, **(env or {})),
                          capture_output=True, text=True)


def git(repo, *args):
    return run(["git", "-C", repo, *args]).stdout.strip()


def port_is_free(port):
    # Connect rather than bind: a socket left in TIME_WAIT by a previous run must not
    # stop a new one.
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        return probe.connect_ex(("127.0.0.1", port)) != 0


def start_gui(port, cwd):
    process = subprocess.Popen([BIN, "gui", "--port", str(port)], cwd=cwd,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, env=os.environ.copy())
    atexit.register(process.terminate)
    for _ in range(50):
        time.sleep(0.1)
        try:
            if api(port, "GET", "/api/health")[0] == 200:
                return process
        except OSError:
            continue
    raise RuntimeError(f"the interface did not answer on port {port}")


def api(port, method, path, body=None, headers=None):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    data = body or ""
    head = {"Content-Type": "application/x-www-form-urlencoded"} if body is not None else {}
    head.update(headers or {})
    conn.request(method, path, data, head)
    response = conn.getresponse()
    payload = response.read().decode()
    status = response.status
    conn.close()
    return status, payload


def post(port, path, fields):
    return api(port, "POST", path, urlencode(fields))


def plan_of(port, fields):
    """Ask for a plan and return (status, plan). The plan is read-only."""
    status, payload = post(port, "/api/repository/plan", fields)
    try:
        body = json.loads(payload)
    except json.JSONDecodeError:
        return status, {}
    return status, body.get("plan", {})


def apply_plan(port, fields, plan_id):
    """Confirm a reviewed plan and collect its whole event stream."""
    body = dict(fields)
    body["planId"] = plan_id
    status, payload = post(port, "/api/repository/apply", body)
    if status != 202:
        return status, [], payload
    op_id = json.loads(payload)["id"]
    events = []
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
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
    return status, events, payload


def collect_events(port, op_id):
    """Read an event stream that a previous call already started."""
    events = []
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=60)
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
    return events


def finished_of(events):
    for event in events:
        if event.get("type") == "finished":
            return event
    return {}


def inspection_of(port):
    status, payload = api(port, "GET", "/api/repositories")
    assert status == 200, (status, payload)
    return json.loads(payload)["inspection"]


def manifest_text(project):
    with open(os.path.join(project, ".gitmesh", "project.toml")) as fh:
        return fh.read()


def tree_digest(path):
    digest = hashlib.sha256()
    for root, dirs, files in os.walk(path):
        dirs.sort()
        for name in sorted(files):
            full = os.path.join(root, name)
            digest.update(os.path.relpath(full, path).encode())
            with open(full, "rb") as fh:
                digest.update(fh.read())
    return digest.hexdigest()


def set_mode(path, mode):
    os.chmod(path, mode)


def force_remove(path):
    """Delete a directory tree even if a test made part of it read-only."""
    if not os.path.exists(path):
        return
    for root, dirs, _ in os.walk(path):
        for name in dirs:
            try:
                os.chmod(os.path.join(root, name), 0o700)
            except OSError:
                pass
    shutil.rmtree(path, ignore_errors=True)


# ----------------------------------------------------------------- environment --
if not port_is_free(PORT) or not port_is_free(SPARE_PORT):
    print(f"ports {PORT}/{SPARE_PORT} are busy; stop the process using them first")
    sys.exit(2)

step(0, "Prepare a project: root repository, one existing repository, plain directories")
force_remove(WORK)
os.makedirs(WORK)
project = os.path.join(WORK, "shop")
remotes = os.path.join(WORK, "remotes")
os.makedirs(remotes)
os.makedirs(os.path.join(project, "engine", "src"))
os.makedirs(os.path.join(project, "renderer", "src"))
os.makedirs(os.path.join(project, "untouched"))
env = {"GIT_AUTHOR_NAME": "Mesh Dev", "GIT_AUTHOR_EMAIL": "dev@gitmesh.test",
       "GIT_COMMITTER_NAME": "Mesh Dev", "GIT_COMMITTER_EMAIL": "dev@gitmesh.test"}

# The root repository, committed before anything else exists.
git(project, "init", "-q", "-b", "main")
for rel, text in [("README.md", "# shop\n"), ("src/main.rs", "fn main() {}\n"),
                  ("engine/src/tick.rs", "pub fn tick() {}\n"),
                  ("engine/src/clock.rs", "pub fn now() -> u64 { 0 }\n"),
                  ("untouched/keep.txt", "do not touch\n")]:
    full = os.path.join(project, rel)
    os.makedirs(os.path.dirname(full), exist_ok=True)
    with open(full, "w") as fh:
        fh.write(text)
run(["git", "-C", project, "add", "-A"], env=env)
run(["git", "-C", project, "commit", "-q", "-m", "first slice"], env=env)
# `renderer` is a repository of its own with a remote and a history of its own.
renderer = os.path.join(project, "renderer")
git(renderer, "init", "-q", "-b", "main")
with open(os.path.join(renderer, "src", "index.js"), "w") as fh:
    fh.write("export {};\n")
run(["git", "-C", renderer, "add", "-A"], env=env)
run(["git", "-C", renderer, "commit", "-q", "-m", "renderer: initial"], env=env)
bare_renderer = os.path.join(remotes, "renderer.git")
run(["git", "init", "-q", "--bare", "-b", "main", bare_renderer])
git(renderer, "remote", "add", "origin", bare_renderer)
run(["git", "-C", renderer, "push", "-q", "-u", "origin", "main"], env=env)

created = run([BIN, "init", ".", "--name", "shop"], cwd=project, env=env)
check(created.returncode == 0, "gitmesh init created the project", created.stderr[:200])
untouched_before = tree_digest(os.path.join(project, "untouched"))
root_head_before = git(project, "rev-parse", "HEAD")

gui = start_gui(PORT, "/tmp")
check(api(PORT, "GET", "/api/health")[0] == 200, "the interface answers on port 7414")

# ------------------------------------------------------------------- 1 .. 4 --
step(1, "Open the project; the interface lists the configuration the service read")
status, payload = api(PORT, "POST", "/api/open", urlencode({"path": project}))
check(status == 200, "opening the project succeeds", payload[:200])
inspection = inspection_of(PORT)
check(inspection["project"]["name"] == "shop", "the project name is shown")
check(inspection["project"]["manifest"].endswith(".gitmesh/project.toml"),
      "the manifest path is shown")
check([repo["id"] for repo in inspection["repositories"]] == ["root"],
      "only the root repository is configured yet",
      str([repo["id"] for repo in inspection["repositories"]]))
check(inspection["repositories"][0]["state"]["key"] == "ready", "and it is usable")
check(inspection["counts"]["repositories"] == 1, "the panel counts the repositories")
check("untouched" in [c["path"] for c in inspection["candidates"]],
      "directories that could become repositories are offered")

step(2, "Check a directory before adding it: the interface answers every question")
status, payload = post(PORT, "/api/repository/inspect", {"path": "engine"})
check(status == 200, "checking a directory succeeds", payload[:200])
candidate = json.loads(payload)["candidate"]
check(candidate["exists"] and not candidate["isRepository"], "it exists and is not a repository")
check(candidate["canAdd"], "it can be added")
check(candidate["suggestedId"] == "engine", "the interface suggests a name", str(candidate["suggestedId"]))
check(candidate["trackedByRoot"] == 2, "the files the root repository tracks are reported",
      str(candidate["trackedByRoot"]))
check("git init" in candidate["consequence"], "and what would happen is spelled out",
      candidate["consequence"])
check(not os.path.isdir(os.path.join(project, "engine", ".git")), "checking created nothing")
status, payload = post(PORT, "/api/repository/inspect", {"path": "engine/src"})
check(status == 200, "checking a directory inside the root repository succeeds")
candidate = json.loads(payload)["candidate"]
check(candidate["canAdd"], "a subdirectory of the root can become a repository of its own")
check(candidate["trackedByRoot"] == 2,
      "and the files the root commits there are counted", str(candidate["trackedByRoot"]))
check(any("belong to two repositories" in line for line in candidate["warnings"]),
      "with the double ownership pointed out", str(candidate["warnings"]))

step(3, "Review the plan for adding the directory as a repository")
add_engine = {"intent": "add", "path": "engine", "id": "engine", "remote": "",
              "branch": "", "initialize": "true", "configureRemote": "false",
              "untrack": "true"}
status, plan = plan_of(PORT, add_engine)
check(status == 200, "the plan is built", str(plan)[:200])
check(plan["id"], "the plan has a fingerprint the interface reviews", str(plan))
check(plan["ready"], "and it can be applied", str(plan.get("blockers")))
kinds = [change["kind"] for change in plan["changes"]]
check("initialize-repository" in kinds, "the review lists the repository creation", str(kinds))
check("add-repository" in kinds, "and the configuration change", str(kinds))
check("untrack-from-root" in kinds, "and the ownership change", str(kinds))
check(any(action["kind"] == "update-manifest" for action in plan["actions"]),
      "the manifest write is a step of its own", str(plan["actions"]))
check(plan["manifest"]["changes"],
      "the manifest preview shows a change")
check("id = \"engine\"" in plan["manifest"]["after"], "with the repository in it")
check(any("deleted" in line for line in plan["safety"]), "the safety guarantees are listed",
      str(plan["safety"]))
check(any("two repositories" in line for line in plan["warnings"])
      or candidate["trackedByRoot"] == 2,
      "the shared ownership is pointed out before the change")
check(not os.path.isdir(os.path.join(project, "engine", ".git")), "planning created nothing")
check(manifest_text(project) == plan["manifest"]["before"], "and wrote nothing")

step(4, "A plan that was not reviewed cannot be applied")
status, payload = post(PORT, "/api/repository/apply", add_engine)
check(status == 400, "applying without a plan id is refused", f"{status} {payload[:160]}")
status, payload = post(PORT, "/api/repository/apply",
                       dict(add_engine, planId="0000000000000000"))
check(status == 409, "a stale plan id is refused", f"{status} {payload[:160]}")
check("changed since it was reviewed" in payload, "with the reason", payload[:160])
check("plan" in json.loads(payload), "and the fresh plan to review")
check(not os.path.isdir(os.path.join(project, "engine", ".git")), "nothing was created")

# ------------------------------------------------------------------ 5 .. 6 ---
step(5, "Apply the reviewed plan: progress per step, then the result")
status, events, _ = apply_plan(PORT, add_engine, plan["id"])
check(status == 202, "the confirmed plan is accepted", str(status))
started = [event for event in events if event.get("type") == "started"]
check(started and started[0]["operation"] == "Repositories", "the operation is named",
      str(started)[:200])
check(len(started[0]["repositories"]) >= 3, "every step is a row from the start",
      str(started[0]["repositories"]))
check(any(event.get("type") == "repository" for event in events),
      "progress is streamed while Git runs")
outcomes = [event for event in events if event.get("type") == "outcome"]
check(outcomes and all(event.get("symbol") for event in outcomes),
      "every step reports its outcome with a symbol",
      str([(event.get("outcome"), event.get("symbol")) for event in outcomes]))
finished = finished_of(events)
check(finished.get("kind") == "complete", "the operation completed", str(finished.get("kind")))
check(finished.get("exitCode") == 0, "exit code 0")
check(finished.get("opened") is True, "the project is reloaded without a restart")
check(finished["repository"]["counts"]["applied"] >= 2, "the changes are reported as applied",
      str(finished["repository"]["counts"]))
evidence = [line for change in finished["repository"]["changes"] for line in change["evidence"]]
check(any("the manifest now lists 'engine'" in line for line in evidence),
      "each change carries the proof it happened", str(evidence)[:200])

step(6, "The change is real, and it touched exactly what it promised")
check(os.path.isdir(os.path.join(project, "engine", ".git")), "the repository exists")
check(git(os.path.join(project, "engine"), "symbolic-ref", "--short", "HEAD") == "main",
      "on the branch GitMesh uses")
check(os.path.isfile(os.path.join(project, "engine", "src", "tick.rs")), "the files are still there")
check(git(project, "ls-files", "--", "engine") == "", "the root repository stopped tracking them")
check("id = \"engine\"" in manifest_text(project), "the manifest lists the new repository")
check(tree_digest(os.path.join(project, "untouched")) == untouched_before,
      "the directory nothing asked about is untouched")
check(git(project, "rev-parse", "HEAD") == root_head_before,
      "the root repository's history did not move")
inspection = inspection_of(PORT)
ids = [repo["id"] for repo in inspection["repositories"]]
check(ids == ["root", "engine"], "the panel now lists both repositories", str(ids))
check(inspection["repositories"][1]["state"]["key"] == "ready",
      "and reports the new one as usable")

# ------------------------------------------------------------------ 7 .. 8 ---
step(7, "Repeating the same request is a no-op, never a second initialisation")
status, repeat = plan_of(PORT, add_engine)
check(status == 200 and repeat["noop"] is True, "the new plan says there is nothing to do",
      str(repeat.get("summary")))
check("nothing to do" in repeat["summary"], "in those words", repeat.get("summary"))
engine_head = git(os.path.join(project, "engine"), "rev-parse", "HEAD")
manifest_before = manifest_text(project)
status, events, _ = apply_plan(PORT, add_engine, repeat["id"])
check(status == 202 and finished_of(events).get("kind") == "complete",
      "applying it again still succeeds", str(status))
check(manifest_text(project) == manifest_before, "the manifest was not rewritten")
check(not os.path.exists(os.path.join(project, "engine", ".git", "MERGE_HEAD")),
      "no second git init was attempted")

step(8, "An existing repository is adopted, never re-initialised")
before = {"head": git(renderer, "rev-parse", "HEAD"),
          "reflog": git(renderer, "reflog", "--format=%gD"),
          "origin": git(renderer, "remote", "get-url", "origin")}
status, payload = post(PORT, "/api/repository/inspect", {"path": "renderer"})
candidate = json.loads(payload)["candidate"]
check(candidate["isRepository"] and candidate["hasCommits"],
      "the interface sees the existing repository")
check(candidate["origin"] == bare_renderer, "and its remote", str(candidate["origin"]))
adopt = {"intent": "add", "path": "renderer", "id": "renderer", "remote": bare_renderer,
         "branch": "", "initialize": "false", "configureRemote": "false", "untrack": "true"}
status, plan = plan_of(PORT, adopt)
check(status == 200 and plan["ready"], "the plan adopts it", str(plan.get("blockers")))
adopt_row = [change for change in plan["changes"]
             if change["kind"] in ("adopt-repository", "initialize-repository")]
check(adopt_row and adopt_row[0]["kind"] == "adopt-repository",
      "the review says it is adopted, not created", str(adopt_row))
status, events, _ = apply_plan(PORT, adopt, plan["id"])
check(finished_of(events).get("kind") == "complete", "the adoption completed")
check(git(renderer, "rev-parse", "HEAD") == before["head"], "the history is untouched")
check(git(renderer, "reflog", "--format=%gD") == before["reflog"],
      "no commit, no reset, no re-initialisation")
check(git(renderer, "remote", "get-url", "origin") == before["origin"],
      "the remote is untouched")
check("id = \"renderer\"" in manifest_text(project), "and it is in the manifest")
check(bare_renderer in manifest_text(project), "with the remote it already had")

# ----------------------------------------------------------------- 9 .. 11 ---
step(9, "Directories that cannot be added are refused, before anything is created")
for path, why in [("missing-dir", "it does not exist"),
                  ("engine/src", "the root repository already commits it"),
                  ("../outside", "it is outside the project"),
                  (".", "it is the project root")]:
    status, payload = post(PORT, "/api/repository/inspect", {"path": path})
    refused = status != 200 or not json.loads(payload)["candidate"]["canAdd"]
    check(refused, f"'{path}' is refused ({why})", f"{status} {payload[:120]}")
check(not os.path.exists(os.path.join(WORK, "outside")), "nothing was created outside the project")
status, payload = post(PORT, "/api/repository/inspect", {"path": "   "})
check(status == 400, "an empty path is a request error", str(status))

step(10, "A name that is already taken is refused, with the reason on screen")
clash = dict(add_engine, path="untouched", id="engine")
status, plan = plan_of(PORT, clash)
check(status == 200, "the plan is still built so the reasons can be shown", str(status))
check(plan["ready"] is False, "but it cannot be applied")
check(any("already used" in blocker for blocker in plan["blockers"]),
      "the blocker names the clash", str(plan["blockers"]))
status, payload = post(PORT, "/api/repository/apply", dict(clash, planId=plan["id"]))
check(status == 422, "applying a blocked plan is refused", f"{status} {payload[:160]}")
check(json.loads(payload)["plan"]["ready"] is False, "and the plan comes back with it")
check(not os.path.isdir(os.path.join(project, "untouched", ".git")),
      "nothing was created in the refused directory")

step(11, "Remotes: recorded, then configured, then replaced only when asked")
add_tools = {"intent": "add", "path": "tools", "id": "tools", "remote": "",
             "branch": "", "initialize": "true", "configureRemote": "false", "untrack": "true"}
os.makedirs(os.path.join(project, "tools"))
with open(os.path.join(project, "tools", "build.sh"), "w") as fh:
    fh.write("#!/bin/sh\n")
status, plan = plan_of(PORT, add_tools)
check(plan["ready"], "a plain new directory can be added", str(plan.get("blockers")))
status, events, _ = apply_plan(PORT, add_tools, plan["id"])
check(finished_of(events).get("kind") == "complete", "the add completed")
bare_tools = os.path.join(remotes, "tools.git")
run(["git", "init", "-q", "--bare", "-b", "main", bare_tools])

status, plan = plan_of(PORT, {"intent": "set-remote", "id": "tools", "remote": bare_tools,
                              "configure": "false"})
check(plan["ready"], "recording a remote is a valid plan", str(plan.get("blockers")))
status, events, _ = apply_plan(PORT, {"intent": "set-remote", "id": "tools",
                                      "remote": bare_tools, "configure": "false"}, plan["id"])
check(finished_of(events).get("kind") == "complete", "it applies")
check(run(["git", "-C", os.path.join(project, "tools"), "remote", "get-url", "origin"]).returncode != 0,
      "and Git was not touched")
check(bare_tools in manifest_text(project), "the remote is recorded in the manifest")

status, plan = plan_of(PORT, {"intent": "set-remote", "id": "tools", "remote": bare_tools,
                              "configure": "true"})
status, events, _ = apply_plan(PORT, {"intent": "set-remote", "id": "tools",
                                      "remote": bare_tools, "configure": "true"}, plan["id"])
check(finished_of(events).get("kind") == "complete", "configuring it in Git applies")
check(git(os.path.join(project, "tools"), "remote", "get-url", "origin") == bare_tools,
      "and origin is there")

other = os.path.join(remotes, "other.git")
run(["git", "init", "-q", "--bare", "-b", "main", other])
status, plan = plan_of(PORT, {"intent": "set-remote", "id": "tools", "remote": other,
                              "configure": "false"})
check(plan["ready"] is False, "replacing an existing origin without asking is refused")
check(any("without configuring Git" in blocker for blocker in plan["blockers"]),
      "with the reason", str(plan["blockers"]))
status, plan = plan_of(PORT, {"intent": "set-remote", "id": "tools", "remote": other,
                              "configure": "true"})
status, events, _ = apply_plan(PORT, {"intent": "set-remote", "id": "tools", "remote": other,
                                     "configure": "true"}, plan["id"])
check(finished_of(events).get("kind") == "complete", "replacing it explicitly applies")
check(git(os.path.join(project, "tools"), "remote", "get-url", "origin") == other,
      "and origin was replaced")

# ---------------------------------------------------------------- 12 .. 13 ---
step(12, "The new repository takes part in the ordinary workflow straight away")
with open(os.path.join(project, "tools", "extra.sh"), "w") as fh:
    fh.write("#!/bin/sh\necho extra\n")
status, payload = post(PORT, "/api/commit", {"message": "add the tools module"})
check(status == 202, "a unified commit is accepted", f"{status} {payload[:160]}")
events = collect_events(PORT, json.loads(payload)["id"])
finished = finished_of(events)
check(finished.get("kind") == "success", "the commit succeeded", str(finished.get("kind")))
check(git(os.path.join(project, "tools"), "log", "-1", "--pretty=%s") == "add the tools module",
      "the new repository has a real commit of its own",
      git(os.path.join(project, "tools"), "log", "-1", "--pretty=%s"))
check(git(project, "log", "-1", "--pretty=%s") == "add the tools module",
      "and so does the root repository", git(project, "log", "-1", "--pretty=%s"))
check(git(os.path.join(project, "tools"), "rev-parse", "HEAD")
      != git(project, "rev-parse", "HEAD"),
      "the two histories are unrelated objects")
out = run([BIN, "status"], cwd=project, env=env)
check(out.returncode == 0, "normal GitMesh status still works", out.stderr[:200])
check("tools" in out.stdout, "and includes the new repository")

status, payload = post(PORT, "/api/push", {})
check(status == 202, "a unified push is accepted", f"{status} {payload[:160]}")
finished = finished_of(collect_events(PORT, json.loads(payload)["id"]))
pushed = {outcome["id"]: outcome["outcome"] for outcome in finished["summary"]["report"]["outcomes"]}
check(pushed.get("tools") == "success",
      "the new repository pushed to the remote it was given", str(pushed))
check(git(other, "log", "-1", "--pretty=%s") == "add the tools module",
      "the commit is really on the remote", git(other, "log", "-1", "--pretty=%s"))

step(13, "Renaming changes the configuration and never the directory")
status, plan = plan_of(PORT, {"intent": "rename", "id": "tools", "newId": "tools-build"})
check(plan["ready"], "the rename is planned", str(plan.get("blockers")))
check(any(change["kind"] == "rename-repository" for change in plan["changes"]),
      "the review lists it", str([c["kind"] for c in plan["changes"]]))
tools_head = git(os.path.join(project, "tools"), "rev-parse", "HEAD")
status, events, _ = apply_plan(PORT, {"intent": "rename", "id": "tools",
                                      "newId": "tools-build"}, plan["id"])
check(finished_of(events).get("kind") == "complete", "the rename completed")
check(os.path.isdir(os.path.join(project, "tools")), "the directory keeps its name")
check(git(os.path.join(project, "tools"), "rev-parse", "HEAD") == tools_head,
      "and its history")
check("id = \"tools-build\"" in manifest_text(project), "the manifest has the new name")
out = run([BIN, "status", "--json"], cwd=project, env=env).stdout
check("tools-build" in out, "the command line sees the new name")
inspection = inspection_of(PORT)
check("tools-build" in [repo["id"] for repo in inspection["repositories"]],
      "and so does the interface")

step(14, "Removing a repository keeps its directory, its Git and its remote")
renderer_head = git(renderer, "rev-parse", "HEAD")
status, plan = plan_of(PORT, {"intent": "remove", "id": "renderer"})
check(plan["ready"], "the removal is planned", str(plan.get("blockers")))
check(any(change["kind"] == "remove-repository" for change in plan["changes"]),
      "the review says a repository leaves the configuration")
check(plan["removals"] and plan["removals"][0]["head"] == renderer_head,
      "with what is there at that moment", str(plan["removals"]))
check(any("no file is moved, renamed or deleted" in line for line in plan["safety"]),
      "and the guarantees", str(plan["safety"]))
status, events, _ = apply_plan(PORT, {"intent": "remove", "id": "renderer"}, plan["id"])
finished = finished_of(events)
check(finished.get("kind") == "complete", "the removal completed", str(finished.get("kind")))
removal_evidence = [line for change in finished["repository"]["changes"]
                    if change["kind"] == "remove-repository" for line in change["evidence"]]
check(any("no longer in the manifest" in line for line in removal_evidence),
      "the removal is proven", str(removal_evidence))
check(any("is still there" in line for line in removal_evidence), "the directory is still there")
check(os.path.isdir(renderer) and os.path.isdir(os.path.join(renderer, ".git")),
      "with its .git")
check(git(renderer, "rev-parse", "HEAD") == renderer_head, "and its history")
check(git(renderer, "remote", "get-url", "origin") == bare_renderer, "and its remote")
check("renderer" not in manifest_text(project), "only the configuration lost it")
check(os.path.isfile(os.path.join(renderer, "src", "index.js")), "the files are untouched")
check(run([BIN, "discover"], cwd=project, env=env).stdout.count("renderer") >= 1,
      "the command line now sees it as an unassigned repository")

# ---------------------------------------------------------------- 15 .. 16 ---
step(15, "A removal that changes ownership needs the consequence confirmed")
os.makedirs(os.path.join(project, "shared"))
with open(os.path.join(project, "shared", "note.txt"), "w") as fh:
    fh.write("shared\n")
# The file is staged by name: `git add -A` in a project that contains repositories
# without commits is refused by Git, and this test is about GitMesh, not about that.
staged = run(["git", "-C", project, "add", "--", "shared/note.txt"], env=env)
check(staged.returncode == 0, "the root repository can track the new files",
      staged.stderr[:200])
committed = run(["git", "-C", project, "commit", "-q", "-m", "shared files"], env=env)
check(committed.returncode == 0, "and commit them", committed.stderr[:200])
check(git(project, "ls-files", "--", "shared").strip() == "shared/note.txt",
      "so the root repository owns them, and the directory is shared with nothing yet")
shared_add = {"intent": "add", "path": "shared", "id": "shared", "remote": "",
              "branch": "", "initialize": "true", "configureRemote": "false",
              "untrack": "false"}
status, plan = plan_of(PORT, shared_add)
check(plan["ready"], "the directory is added without untracking", str(plan.get("blockers")))
status, events, _ = apply_plan(PORT, shared_add, plan["id"])
check(finished_of(events).get("kind") == "complete", "the add completed")

status, plan = plan_of(PORT, {"intent": "remove", "id": "shared"})
check(plan["ready"] is False, "removing it is refused while the root repository owns its files")
check(any("hands those files back" in blocker for blocker in plan["blockers"]),
      "the consequence is stated", str(plan["blockers"]))
status, payload = post(PORT, "/api/repository/apply",
                       {"intent": "remove", "id": "shared", "planId": plan["id"]})
check(status == 422, "and it cannot be applied", f"{status} {payload[:160]}")
check("shared" in manifest_text(project), "the repository is still configured")

status, plan = plan_of(PORT, {"intent": "remove", "id": "shared", "confirmTakeover": "true"})
check(plan["ready"], "with the confirmation it is planned", str(plan.get("blockers")))
check(any("owns the 1 file(s)" in line for line in plan["warnings"]),
      "and the review says who will own them afterwards", str(plan["warnings"]))
status, events, _ = apply_plan(PORT, {"intent": "remove", "id": "shared",
                                      "confirmTakeover": "true"}, plan["id"])
check(finished_of(events).get("kind") == "complete", "and it applies")
check(os.path.isfile(os.path.join(project, "shared", "note.txt")), "the files are still there")
check(git(project, "ls-files", "--", "shared").strip() == "shared/note.txt",
      "and the root repository still owns them")
check("id = \"shared\"" not in manifest_text(project), "only the configuration let it go")

step(16, "A failing repository does not stop the others")
blocked = os.path.join(project, "blocked")
os.makedirs(blocked)
with open(os.path.join(blocked, "note.txt"), "w") as fh:
    fh.write("cannot be written\n")
set_mode(blocked, 0o500)
status, plan = plan_of(PORT, {"intent": "add", "path": "blocked", "id": "blocked",
                              "remote": "", "branch": "", "initialize": "true",
                              "configureRemote": "false", "untrack": "true"})
check(plan["ready"], "the plan itself is accepted", str(plan.get("blockers")))
status, events, _ = apply_plan(PORT, {"intent": "add", "path": "blocked", "id": "blocked",
                                      "remote": "", "branch": "", "initialize": "true",
                                      "configureRemote": "false", "untrack": "true"}, plan["id"])
finished = finished_of(events)
check(finished.get("kind") == "partial", "the result is partial, never a false success",
      str(finished.get("kind")))
check(finished.get("exitCode") == 1, "and it says so in the exit code")
failures = [action for action in finished["repository"]["actions"]
            if action["outcome"] == "failed"]
check(any(action["target"] == "blocked" for action in failures), "the failing repository is named",
      str(failures)[:200])
check(any(any("Permission denied" in detail for detail in action["details"])
          for action in failures),
      "with the real Git error", str(failures)[:200])
check(finished["repository"]["validation"]["ok"] is False,
      "and the validation says the project is not what the plan promised")
out = run([BIN, "status"], cwd=project, env=env)
check(out.returncode != 0, "the command line fails while the project is inconsistent",
      str(out.returncode))
check("blocked" in out.stdout and "unavailable" in out.stdout,
      "and names the repository that could not be created", out.stdout[:200])
set_mode(blocked, 0o700)
out = run([BIN, "configure", "remove", "blocked"], cwd=project, env=env)
check(out.returncode == 0, "the failed repository can be removed from the configuration",
      out.stderr[:200])
out = run([BIN, "status"], cwd=project, env=env)
check(out.returncode == 0, "and the project is healthy again", out.stderr[:200])
os.makedirs(os.path.join(project, "fine"))
out = run([BIN, "configure", "add", "fine", "--git-init"], cwd=project, env=env)
check(out.returncode == 0, "a later operation on another repository works", out.stderr[:200])
check(os.path.isdir(os.path.join(project, "fine", ".git")), "and it created its repository")
set_mode(blocked, 0o700)

step(17, "Without an open project the panel refuses instead of guessing")
spare = start_gui(SPARE_PORT, "/tmp")
check(api(SPARE_PORT, "GET", "/api/health")[0] == 200, "a second interface starts on 7415")
status, payload = api(SPARE_PORT, "GET", "/api/repositories")
check(status == 409, "asking for repositories without a project is refused", str(status))
check("no GitMesh project is open" in payload, "with a clear message", payload[:160])
status, payload = post(SPARE_PORT, "/api/repository/plan",
                       {"intent": "add", "path": "engine"})
check(status == 422, "and planning is refused the same way", str(status))

# ---------------------------------------------------------------- 18 .. 20 ---
step(18, "The project view exposes the same configuration, read-only")
status, payload = api(PORT, "POST", "/api/refresh", "")
check(status == 200, "refreshing succeeds")
m = json.loads(payload)
ids = sorted(repo["id"] for repo in m["repositories"])
check("root" in ids and "engine" in ids and "tools-build" in ids,
      "every configured repository is in the model", str(ids))
check(m["kind"] == "project", "the model is the project view")
check(os.path.basename(m["project"]["root"]) == "shop", "with the project root")
check(not os.path.isdir(os.path.join(project, "renderer", ".gitmesh")),
      "no second manifest was ever written")

step(19, "The command line keeps working on the managed project")
out = run([BIN, "configure", "list", "--json"], cwd=project, env=env)
check(out.returncode == 0, "configure list --json still parses", out.stderr[:200])
configured = json.loads(out.stdout)
check(configured["project"] == "shop", "and reports the project", str(configured)[:160])
before_manifest = manifest_text(project)
out = run([BIN, "configure", "add", "untouched", "--git-init", "--dry-run"],
          cwd=project, env=env)
check(out.returncode == 0, "a dry run succeeds", out.stderr[:200])
check("Dry run: nothing was changed" in out.stdout, "and says so", out.stdout[:200])
check(manifest_text(project) == before_manifest, "with the manifest untouched")
out = run([BIN, "configure", "add", "tools", "--git-init"], cwd=project, env=env)
check(out.returncode == 0, "adding a path that is already configured succeeds quietly",
      out.stderr[:200])
check(manifest_text(project) == before_manifest, "with nothing changed")
check("Added repository" not in out.stdout,
      "and without claiming it added anything", out.stdout[:200])
out = run([BIN, "status", "--json"], cwd=project, env=env)
check(json.loads(out.stdout)["project"] == "shop", "status --json is still valid")
out = run([BIN, "status"], cwd=os.path.join(project, "engine", "src"), env=env)
check(out.returncode == 0 and "'shop'" in out.stdout,
      "a nested directory still finds the project", out.stderr[:200])
out = run([BIN, "status"], cwd="/tmp", env=env)
check(out.returncode == 2 and "no GitMesh project found" in out.stderr,
      "and a missing project still exits 2", f"{out.returncode} {out.stderr[:120]}")

step(20, "Nothing was deleted, anywhere, by any of this")
check(os.path.isdir(renderer) and os.path.isfile(os.path.join(renderer, "src", "index.js")),
      "the removed repository's directory and files are intact")
check(os.path.isdir(os.path.join(renderer, ".git")), "its .git is intact")
check(tree_digest(os.path.join(project, "untouched")) == untouched_before,
      "the untouched directory is byte-identical")
check(os.path.isfile(os.path.join(project, "src", "main.rs")),
      "the project's own files are intact")
check(git(project, "rev-parse", "HEAD") != "", "the root repository still has its history")

# ------------------------------------------------------------------ summary ---
print(f"\n\033[1m{ok_count} checks passed, {fail_count} failed\033[0m")
if fail_count:
    print(f"the project is left in {project} for inspection")
sys.exit(1 if fail_count else 0)
