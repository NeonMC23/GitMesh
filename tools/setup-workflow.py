#!/usr/bin/env python3
"""End-to-end validation of the GitMesh setup wizard.

Starts from an ordinary directory, drives the same HTTP interface the browser uses
(`/api/setup/inspect`, `/api/setup/plan`, `/api/setup/apply`), and then verifies the
result on disk: real `.git` directories, the generated `.gitmesh/project.toml`, the
configured remotes, the first publish, a partial failure, and a rerun on the project
that is now already configured.

    cargo build --release && ./tools/setup-workflow.py

Requires python3 (standard library only), git and a free TCP port. The project it
builds is left in /tmp/gitmesh-setup-e2e for inspection.
"""
import atexit, json, os, shutil, subprocess, sys, time, http.client
from urllib.parse import quote

BIN = os.environ.get(
    "GITMESH",
    os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                 "target", "release", "gitmesh"),
)
WORK = "/tmp/gitmesh-setup-e2e"
PROJECT = os.path.join(WORK, "MyProject")
REMOTES = os.path.join(WORK, "remotes")
OUTSIDE = os.path.join(WORK, "Untouched")
PORT = 7412
SECOND_PORT = 7413

ok_count = 0
fail_count = 0
STARTED = []


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


ENV = dict(os.environ)
ENV.update({
    "GIT_AUTHOR_NAME": "Mesh Dev",
    "GIT_AUTHOR_EMAIL": "dev@gitmesh.test",
    "GIT_COMMITTER_NAME": "Mesh Dev",
    "GIT_COMMITTER_EMAIL": "dev@gitmesh.test",
})


def start_gui(cwd, port):
    """Start an interface and remember it: a leftover process would hold the port and
    poison the next run, so every interface this script starts is stopped on exit."""
    process = subprocess.Popen([BIN, "gui", "--port", str(port)], cwd=cwd,
                               stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, env=ENV)
    STARTED.append(process)
    atexit.register(process.terminate)
    time.sleep(1.5)
    return process


def port_is_free(port):
    import socket
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        return sock.connect_ex(("127.0.0.1", port)) != 0


def run(args, cwd=None):
    return subprocess.run(args, cwd=cwd, env=ENV, capture_output=True, text=True)


def git(repo, *args):
    return run(["git", "-C", repo, *args]).stdout.strip()


def write(path, text):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as fh:
        fh.write(text)


def cli(*args, cwd=PROJECT):
    return run([BIN, *args], cwd=cwd)


def api(method, path, body=None, port=PORT):
    conn = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
    headers = {"Content-Type": "application/x-www-form-urlencoded"} if body is not None else {}
    conn.request(method, path, body or "", headers)
    response = conn.getresponse()
    payload = response.read().decode()
    status = response.status
    conn.close()
    return status, payload


def record(**fields):
    """One `path=engine;id=engine` repository record, exactly as the wizard sends it.

    The separators inside a record stay literal (`;` between fields, `=` after a key) and
    only the values are percent-encoded; `&` separates records from each other and from the
    rest of the form.
    """
    return ";".join(f"{key}={quote(str(value), safe='')}"
                    for key, value in fields.items() if value is not None)


def setup_body(path, name="MyProject", root="yes", configure_remotes="yes", untrack="yes",
               repositories=(), root_remote=None, publish=None, plan_id=None, **extra):
    fields = [("path", path), ("name", name), ("root", root),
              ("configureRemotes", configure_remotes), ("untrack", untrack)]
    if root_remote:
        fields.append(("rootRemote", root_remote))
    if publish:
        fields.append(("publish", "yes"))
        fields.append(("firstCommit", publish))
    if plan_id:
        fields.append(("planId", plan_id))
    fields.extend(extra.items())
    body = "&".join(f"{key}={quote(str(value), safe='')}" for key, value in fields)
    for item in repositories:
        body += "&repositories=" + record(**item)
    return body


def plan(path, **kwargs):
    status, payload = api("POST", "/api/setup/plan", setup_body(path, **kwargs))
    body = json.loads(payload) if payload.strip().startswith("{") else {}
    return status, body.get("plan", {}), payload


def run_operation(path, body):
    """POST an operation and collect its whole event stream."""
    status, payload = api("POST", path, body)
    assert status == 202, (status, payload)
    op_id = json.loads(payload)["id"]
    events = []
    conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
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


def finished_of(events):
    for event in events:
        if event.get("type") == "finished":
            return event
    return None


def outcome(events, row_id):
    for event in events:
        if event.get("type") == "outcome" and event.get("id") == row_id:
            return event
    return None


# ------------------------------------------------------------------ scenario --
step(0, f"Prepare an ordinary directory at {PROJECT}")
for port in (PORT, SECOND_PORT):
    if not port_is_free(port):
        print(f"port {port} is already in use: stop whatever holds it (a leftover "
              f"`gitmesh gui` is the usual reason)")
        sys.exit(2)
shutil.rmtree(WORK, ignore_errors=True)
os.makedirs(REMOTES)
os.makedirs(OUTSIDE)
write(os.path.join(PROJECT, "src/main.rs"), "fn main() {}\n")
write(os.path.join(PROJECT, "engine/lib.rs"), "pub fn tick() {}\n")
write(os.path.join(PROJECT, "tools/build.sh"), "#!/bin/sh\n")
write(os.path.join(PROJECT, "renderer/index.js"), "export {};\n")

# An external repository that already exists, with its own history and its own remote:
# the setup must adopt it, never re-initialise it.
renderer = os.path.join(PROJECT, "renderer")
run(["git", "init", "-q", "-b", "main", renderer])
run(["git", "-C", renderer, "add", "-A"])
run(["git", "-C", renderer, "commit", "-q", "-m", "renderer work"])
run(["git", "init", "-q", "--bare", "-b", "main", os.path.join(REMOTES, "renderer.git")])
renderer_head_before = git(renderer, "rev-parse", "HEAD")
git(renderer, "remote", "add", "origin", os.path.join(REMOTES, "renderer.git"))

# Bare remotes for the root repository and for engine; tools stays local-only.
for name in ["root", "engine"]:
    run(["git", "init", "-q", "--bare", "-b", "main", os.path.join(REMOTES, f"{name}.git")])

# A repository outside the project that no setup may touch.
run(["git", "init", "-q", "-b", "main", OUTSIDE])
write(os.path.join(OUTSIDE, "keep.txt"), "untouched\n")
run(["git", "-C", OUTSIDE, "add", "-A"])
run(["git", "-C", OUTSIDE, "commit", "-q", "-m", "outside"])
outside_head_before = git(OUTSIDE, "rev-parse", "HEAD")
outside_files_before = sorted(os.listdir(OUTSIDE))
check(not os.path.exists(os.path.join(PROJECT, ".gitmesh")), "the directory is not a project yet")
check(not os.path.exists(os.path.join(PROJECT, ".git")), "and holds no repository at the root")

gui = start_gui(WORK, PORT)
health = api("GET", "/api/health")
check(health[0] == 200, "the interface answers on the given port", health[1][:120])

step(1, "Scan the directory: facts only, nothing is created")
status, payload = api("GET", "/api/setup/status")
status_view = json.loads(payload)["inspection"]
check(status == 200, "the read-only status tells what the directory holds", payload[:200])
check(status_view["root"] == WORK, "the directory it was started in is inspected",
      status_view["root"])
candidates_here = {node["path"]: node for node in status_view["candidates"]}
check("MyProject" in candidates_here, "and its sub-directories are candidates",
      str(sorted(candidates_here)))
status, payload = api("POST", "/api/setup/inspect", f"path={quote(PROJECT)}&name=MyProject")
inspection = json.loads(payload)["inspection"]
candidates = {node["path"]: node for node in inspection["candidates"]}
check(status == 200, "scanning succeeds", payload[:200])
check({"src", "engine", "renderer", "tools"} <= set(candidates),
      "every directory of the project is a candidate", str(sorted(candidates)))
check(candidates["renderer"]["hasGitDir"] and candidates["renderer"]["hasCommits"],
      "an existing repository is detected, with its commits")
check(candidates["engine"]["hasGitDir"] is False, "a plain directory is not mistaken for one")
check(candidates["renderer"]["suggestedId"] == "myproject-renderer",
      "name suggestions come from the Rust side", candidates["renderer"]["suggestedId"])
check(not os.path.exists(os.path.join(PROJECT, ".gitmesh")), "scanning created nothing")

step(2, "Review the plan: what will be created, adopted and configured")
status, view, raw = plan(
    PROJECT,
    root_remote=os.path.join(REMOTES, "root.git"),
    repositories=[
        {"path": "engine", "id": "myproject-engine", "create": "yes",
         "remote": os.path.join(REMOTES, "engine.git")},
        {"path": "renderer", "id": "myproject-renderer"},
        {"path": "tools", "id": "myproject-tools", "create": "yes"},
    ],
    publish="Initial commit",
)
check(status == 200, "the plan is generated", raw[:200])
check(view["ready"] is True, "and it is ready to run", str(view.get("blockers")))
check(view["id"] and len(view["id"]) == 16, "the plan carries a fingerprint", view.get("id"))
repos = {repo["id"]: repo for repo in view["repositories"]}
check(set(repos) == {"root", "myproject-engine", "myproject-renderer", "myproject-tools"},
      "all four repositories are in the plan", str(sorted(repos)))
check(repos["myproject-engine"]["create"] is True, "engine is created")
check(repos["myproject-renderer"]["create"] is False
      and repos["myproject-renderer"]["isRepository"],
      "the existing renderer repository is adopted, not re-created")
check(repos["myproject-renderer"]["remoteAction"] == "keep",
      "its existing remote is kept", repos["myproject-renderer"]["remoteAction"])
check(repos["myproject-tools"]["remoteAction"] == "none",
      "tools stays local-only", repos["myproject-tools"]["remoteAction"])
check(repos["root"]["remoteAction"] == "add", "the root remote is added")
steps = {step["kind"] + ":" + step["target"]: step for step in view["steps"]}
check(steps["create-metadata:manifest"]["state"] == "planned", "the metadata directory is created")
check(steps["create-repository:root"]["state"] == "planned"
      and steps["create-repository:myproject-tools"]["state"] == "planned",
      "the root and tools repositories are created by the setup")
check(steps["create-repository:myproject-renderer"]["state"] == "already",
      "the existing repository is reported as already there")
check(steps["write-manifest:manifest"]["state"] == "planned", "the manifest is written")
check(all(step["state"] == "planned" for step in view["steps"] if step["state"] == "planned"),
      "nothing in the plan is blocked")
check(any("no existing .git directory is deleted" in line for line in view["safety"]),
      "the plan states its guarantees", str(view["safety"]))
check(any("no file is moved" in line for line in view["safety"]),
      "including that no file is touched")
check(view["publish"] and "Initial commit" in view["publish"]["message"],
      "the first publish is part of the plan, not of the executor", str(view["publish"]))
check(set(view["publish"]["repositories"]) == {"root", "myproject-engine"},
      "only repositories with a configured remote will be pushed",
      str(view["publish"]["repositories"]))
check(not os.path.exists(os.path.join(PROJECT, ".gitmesh")), "planning created nothing")

step(3, "The plan refuses layouts it cannot own, instead of guessing")
status, view, raw = plan(PROJECT, repositories=[
    {"path": "engine", "id": "engine", "create": "yes"},
    {"path": "engine/src", "id": "deep", "create": "yes"},
])
check(status == 200 and view["ready"] is False, "a repository inside a repository is refused",
      str(view.get("blockers")))
check(any("nested repository boundaries" in line for line in view["blockers"]),
      "and the refusal says why", str(view["blockers"]))
status, view, raw = plan(PROJECT, repositories=[{"path": "../outside", "id": "up", "create": "yes"}])
check(view["ready"] is False, "a path escaping the project root is refused", str(view.get("blockers")))
status, view, raw = plan(PROJECT, repositories=[{"path": "missing", "id": "gone", "create": "yes"}])
check(view["ready"] is False, "a directory that does not exist is refused", str(view.get("blockers")))
status, view, raw = plan(PROJECT, name="MyProject", root="yes", repositories=[
    {"path": "engine", "id": "engine", "create": "yes"},
    {"path": "tools", "id": "engine", "create": "yes"},
])
check(view["ready"] is False, "two repositories cannot share one name", str(view.get("blockers")))
status, payload = api("POST", "/api/setup/plan", "path=/tmp/x&repositories=path%3Dengine%3Bid")
check(status == 400, "a malformed repository record is refused, not guessed", payload[:200])
check(not os.path.exists(os.path.join(PROJECT, ".git")), "nothing was created by any refusal")

step(4, "Execute the reviewed plan: the project is created and opened, with no restart")
status, payload = api("POST", "/api/setup/plan", setup_body(
    PROJECT,
    root_remote=os.path.join(REMOTES, "root.git"),
    repositories=[
        {"path": "engine", "id": "myproject-engine", "create": "yes",
         "remote": os.path.join(REMOTES, "engine.git")},
        {"path": "renderer", "id": "myproject-renderer"},
        {"path": "tools", "id": "myproject-tools", "create": "yes"},
    ],
    publish="Initial commit",
))
reviewed = json.loads(payload)["plan"]
empty_plan = api("POST", "/api/setup/apply", setup_body(PROJECT))
check(empty_plan[0] == 400, "a setup without a reviewed plan id is refused")
stale = api("POST", "/api/setup/apply",
            setup_body(PROJECT, plan_id="0" * 16,
                       repositories=[{"path": "engine", "id": "myproject-engine",
                                      "create": "yes"}]))
check(stale[0] == 409, "a plan that no longer matches is refused", stale[1][:160])

op_id, events = run_operation("/api/setup/apply", setup_body(
    PROJECT,
    root_remote=os.path.join(REMOTES, "root.git"),
    repositories=[
        {"path": "engine", "id": "myproject-engine", "create": "yes",
         "remote": os.path.join(REMOTES, "engine.git")},
        {"path": "renderer", "id": "myproject-renderer"},
        {"path": "tools", "id": "myproject-tools", "create": "yes"},
    ],
    publish="Initial commit",
    plan_id=reviewed["id"],
))
finished = finished_of(events)
check(finished is not None, "the setup finished", str(events[-1])[:200])
check(finished["kind"] == "complete", "as a complete success", finished["kind"])
check(finished["exitCode"] == 0, "exit code 0", str(finished["exitCode"]))
check(finished["opened"] is True, "and the interface adopted the project")
check(finished["setup"]["validation"]["ok"] is True,
      "the resulting project validates through the normal opening path",
      str(finished["setup"]["validation"]["issues"]))
rows = [event["id"] for event in events if event.get("type") == "repository"]
check("create-repository:myproject-engine" in rows, "progress named every step", str(rows))
check(outcome(events, "create-repository:myproject-renderer")["outcome"] == "skipped",
      "the adopted repository is reported as skipped, not re-created")

step(5, "The filesystem is exactly what the plan promised")
check(os.path.isfile(os.path.join(PROJECT, ".git/HEAD")), "the root repository was created")
for name in ["engine", "tools"]:
    check(os.path.isfile(os.path.join(PROJECT, name, ".git/HEAD")), f"{name} was created")
check(git(renderer, "rev-parse", "HEAD") == renderer_head_before,
      "the existing repository keeps its history")
manifest_path = os.path.join(PROJECT, ".gitmesh/project.toml")
check(os.path.isfile(manifest_path), "the manifest was generated")
manifest = open(manifest_path).read()
check('name = "MyProject"' in manifest, "with the requested name", manifest[:200])
check('path = "engine"' in manifest and 'path = "renderer"' in manifest
      and 'path = "tools"' in manifest,
      "and one entry per external repository", manifest)
check(".." not in manifest and PROJECT not in manifest,
      "manifest paths are relative, normalised and inside the project", manifest)
check(f"remote = \"{os.path.join(REMOTES, 'engine.git')}\"" in manifest,
      "the configured remote is recorded")
check("myproject-tools" in manifest and manifest.count("[[repositories]]") == 3,
      "three external repositories, in one manifest", str(manifest.count("[[repositories]]")))
check(open(os.path.join(PROJECT, ".gitmesh/project.toml")).read() == manifest,
      "the file on disk is exactly the preview")

step(6, "Remotes are configured where the plan said they would be")
for name in ["root", "engine"]:
    repo = PROJECT if name == "root" else os.path.join(PROJECT, name)
    check(git(repo, "remote", "get-url", "origin") == os.path.join(REMOTES, f"{name}.git"),
          f"{name} has the planned origin", git(repo, "remote", "get-url", "origin"))
check(git(os.path.join(PROJECT, "tools"), "remote") == "",
      "tools has no remote: it is local-only on purpose")
check(git(renderer, "remote", "get-url", "origin") == os.path.join(REMOTES, "renderer.git"),
      "renderer keeps the remote it already had")

step(7, "The first publish ran through the ordinary commit and push operations")
publish = finished.get("publish")
check(publish, "the result reports the first publish", str(publish)[:200])
commits = [section for section in publish if section["operation"] == "First commit"]
pushes = [section for section in publish if section["operation"] == "First push"]
check(commits and commits[0]["outcome"]["exitCode"] == 0, "the first commit succeeded", str(commits))
check(pushes and pushes[0]["outcome"]["exitCode"] == 0, "the first push succeeded", str(pushes))
check({outcome["id"] for outcome in commits[0]["outcomes"]} == {"root", "myproject-engine"},
      "the first commit named the repositories the plan listed",
      str(commits[0]["outcomes"]))
for name in ["root", "engine"]:
    repo = PROJECT if name == "root" else os.path.join(PROJECT, name)
    check(git(repo, "log", "-1", "--pretty=%s") == "Initial commit",
          f"{name} has a real commit with the logical message", git(repo, "log", "-1", "--pretty=%s"))
    check(git(repo, "rev-parse", "HEAD") == git(os.path.join(REMOTES, f"{name}.git"), "rev-parse", "main"),
          f"{name} was pushed to its remote")
tools_head = run(["git", "-C", os.path.join(PROJECT, "tools"), "rev-parse", "--verify", "HEAD"])
check(tools_head.returncode != 0,
      "tools was not committed in the first publish: it has no remote and nothing to send",
      tools_head.stdout[:120])

step(8, "The project opens normally, from the interface and from the command line")
status, payload = api("POST", "/api/open", f"path={quote(PROJECT)}")
check(status == 200, "the interface opens it", payload[:200])
model = json.loads(payload)
check(model["kind"] == "project" and model["project"]["name"] == "MyProject",
      "as one logical project", str(model.get("project", {}).get("name")))
check(model["project"]["counts"]["repositories"] == 4, "with its four repositories",
      str(model["project"]["counts"]))
status_json = cli("status", "--json")
check(status_json.returncode == 0, "the command line sees it too", status_json.stderr[:200])
check(json.loads(status_json.stdout)["project"] == "MyProject",
      "with the same name", status_json.stdout[:200])
nested = cli("-C", os.path.join(PROJECT, "engine"), "status", "--json")
check(nested.returncode == 0, "and from a nested directory (-C)", nested.stderr[:200])
status, payload = api("POST", "/api/open", f"path={quote(os.path.join(WORK, 'nowhere'))}")
check(status == 422, "a directory that is not a project is refused", payload[:160])
refused = json.loads(api("GET", "/api/model")[1])
check(refused["kind"] == "no-project" and refused["error"],
      "and the interface explains why, with nothing half-open", str(refused.get("error"))[:120])
status, payload = api("POST", "/api/open", f"path={quote(PROJECT)}")
check(status == 200 and json.loads(payload)["kind"] == "project",
      "the project opens again right after the refusal", payload[:120])

step(9, "Real changes in several repositories, then one logical commit")
for rel, text in [("src/main.rs", "fn main() { println!(\"hello\"); }\n"),
                  ("engine/lib.rs", "pub fn tick() {}\npub fn tock() {}\n"),
                  ("renderer/index.js", "export const ready = true;\n"),
                  ("tools/build.sh", "#!/bin/sh\nset -e\n")]:
    write(os.path.join(PROJECT, rel), text)
status, payload = api("POST", "/api/commit", "message=one%20logical%20commit")
check(status == 202, "the commit starts", payload[:160])
op_id = json.loads(payload)["id"]
events = []
conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
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
finished = finished_of(events)
check(finished["kind"] == "success", "the unified commit succeeded", str(finished["kind"]))
for name in ["root", "engine", "renderer", "tools"]:
    repo = PROJECT if name == "root" else os.path.join(PROJECT, name)
    subject = git(repo, "log", "-1", "--pretty=%s")
    check(subject == "one logical commit", f"{name} has its own commit with the one message", subject)

step(10, "Push everything: remotes are used, local-only repositories are not failures")
status, payload = api("POST", "/api/push", "")
check(status == 202, "the push starts", payload[:160])
op_id = json.loads(payload)["id"]
events = []
conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
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
finished = finished_of(events)
push_outcomes = {outcome["id"]: outcome for outcome in
                 finished["summary"]["report"]["outcomes"]}
check(push_outcomes["root"]["outcome"] == "success", "the root repository was pushed",
      str(push_outcomes["root"]))
check(push_outcomes["myproject-renderer"]["outcome"] == "success", "renderer was pushed",
      str(push_outcomes["myproject-renderer"]))
check(push_outcomes["myproject-engine"]["outcome"] == "success", "engine was pushed",
      str(push_outcomes["myproject-engine"]))
check(push_outcomes["myproject-tools"]["outcome"] == "skipped",
      "the local-only repository is skipped, never a failure", str(push_outcomes["myproject-tools"]))
check(finished["exitCode"] == 0, "and the whole operation reports success", str(finished["exitCode"]))

step(11, "A partial failure is reported precisely, and does not hide the others")
shutil.rmtree(os.path.join(REMOTES, "engine.git"))
write(os.path.join(PROJECT, "engine/lib.rs"), "pub fn tick() {}\npub fn tock() {}\npub fn boom() {}\n")
write(os.path.join(PROJECT, "src/main.rs"), "fn main() { println!(\"again\"); }\n")
status, payload = api("POST", "/api/commit", "message=second%20slice")
op_id = json.loads(payload)["id"]
conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
conn.request("GET", f"/api/events/{op_id}")
response = conn.getresponse()
while response.read(4096):
    pass
conn.close()
status, payload = api("POST", "/api/push", "")
check(status == 202, "the push starts again", payload[:160])
op_id = json.loads(payload)["id"]
events = []
conn = http.client.HTTPConnection("127.0.0.1", PORT, timeout=120)
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
finished = finished_of(events)
push_outcomes = {outcome["id"]: outcome for outcome in finished["summary"]["report"]["outcomes"]}
check(push_outcomes["myproject-engine"]["outcome"] == "failed",
      "the repository whose remote disappeared fails", str(push_outcomes["myproject-engine"]))
check(push_outcomes["root"]["outcome"] == "success",
      "the others are pushed anyway", str(push_outcomes["root"]))
check(finished["kind"] in ("partial", "failed") and finished["exitCode"] == 1,
      "the project does not claim success", str(finished["kind"]))
model = json.loads(api("GET", "/api/model")[1])
check(model["kind"] == "project", "the project is still open and usable")
check(json.loads(api("GET", "/api/model")[1])["project"]["counts"]["repositories"] == 4,
      "with all four repositories")
engine = [repo for repo in model["repositories"] if repo["id"] == "myproject-engine"][0]
readiness = model["readiness"]["push"]
check("myproject-engine" in readiness["aheadIn"],
      "the repository that failed still has unpublished commits", json.dumps(readiness)[:200])
check(engine["sync"]["ahead"] > 0, "and the model shows exactly that", str(engine["sync"]))

step(12, "Rerunning the setup on the configured project changes nothing")
status, payload = api("POST", "/api/setup/inspect", f"path={quote(PROJECT)}")
inspection = json.loads(payload)["inspection"]
check(inspection["isGitMeshProject"] is True, "the directory is now a GitMesh project")
heads_before = {name: git(PROJECT if name == "root" else os.path.join(PROJECT, name),
                          "rev-parse", "HEAD")
                for name in ["root", "engine", "renderer", "tools"]}
status, payload = api("POST", "/api/setup/plan", setup_body(
    PROJECT,
    root_remote=os.path.join(REMOTES, "root.git"),
    repositories=[
        {"path": "engine", "id": "myproject-engine", "create": "yes",
         "remote": os.path.join(REMOTES, "engine.git")},
        {"path": "renderer", "id": "myproject-renderer"},
        {"path": "tools", "id": "myproject-tools", "create": "yes"},
    ],
))
again = json.loads(payload)["plan"]
check(again["ready"] is True, "the request is still satisfiable", str(again.get("blockers")))
check(again["noop"] is True, "and nothing is left to do", again["summary"])
check(all(step["state"] == "already" for step in again["steps"]),
      "every step is reported as already in place",
      str([s["state"] for s in again["steps"]]))
op_id, events = run_operation("/api/setup/apply", setup_body(
    PROJECT,
    root_remote=os.path.join(REMOTES, "root.git"),
    repositories=[
        {"path": "engine", "id": "myproject-engine", "create": "yes",
         "remote": os.path.join(REMOTES, "engine.git")},
        {"path": "renderer", "id": "myproject-renderer"},
        {"path": "tools", "id": "myproject-tools", "create": "yes"},
    ],
    plan_id=again["id"],
))
finished = finished_of(events)
check(finished["kind"] == "complete", "the rerun succeeds", finished["kind"])
check(finished["setup"]["validation"]["ok"] is True, "the project still validates")
check(open(manifest_path).read() == manifest, "the manifest is byte-for-byte unchanged")
check(all(git(PROJECT if name == "root" else os.path.join(PROJECT, name), "rev-parse", "HEAD")
          == head for name, head in heads_before.items()),
      "no repository was re-initialised and no history was touched")
check(git(OUTSIDE, "rev-parse", "HEAD") == outside_head_before,
      "a repository outside the project is untouched")
check(sorted(os.listdir(OUTSIDE)) == outside_files_before,
      "and no file of it was moved or deleted")

step(13, "Restart the interface: the project is still there, fully usable")
gui.terminate()
gui.wait(timeout=10)
gui2 = start_gui(PROJECT, SECOND_PORT)
status, payload = api("GET", "/api/model", port=SECOND_PORT)
model = json.loads(payload)
check(status == 200, "a fresh interface answers", payload[:200])
check(model["kind"] == "project" and model["project"]["name"] == "MyProject",
      "and opens the project it was started in", str(model.get("project", {}).get("name")))
check(model["project"]["counts"]["repositories"] == 4, "with all four repositories")
status, payload = api("POST", "/api/open", f"path={quote(PROJECT)}", port=SECOND_PORT)
check(status == 200 and json.loads(payload)["kind"] == "project",
      "and can be reopened explicitly")
status, payload = api("POST", "/api/commit", "message=nothing%20to%20do", port=SECOND_PORT)
check(status == 202, "operations still work after the restart", payload[:160])
gui2.terminate()
gui2.wait(timeout=10)

step(14, "Summary")
print(f"\n{ok_count} checks passed, {fail_count} failed")
print(f"project left in {PROJECT}")
if fail_count:
    sys.exit(1)
