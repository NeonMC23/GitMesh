#!/usr/bin/env bash
#
# GitMesh demo: build a three-repository logical project with local bare remotes and
# walk through the whole workflow (status, commit, branch, checkout, push, pull, conflict).
#
#   ./examples/demo.sh [work-directory]
#
# Everything happens inside the work directory (default: a fresh temporary directory);
# nothing outside it is touched. Requires `gitmesh` on PATH or GITMESH=<path to binary>.

set -euo pipefail

GITMESH="${GITMESH:-gitmesh}"
WORK="${1:-$(mktemp -d -t gitmesh-demo-XXXXXX)}"
PROJECT="$WORK/shop"
REMOTES="$WORK/remotes"

step() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }
run() { printf '\033[0;90m$ %s\033[0m\n' "$*"; "$@"; }

# ---------------------------------------------------------------- environment --
export GIT_AUTHOR_NAME="${GIT_AUTHOR_NAME:-GitMesh Demo}"
export GIT_AUTHOR_EMAIL="${GIT_AUTHOR_EMAIL:-demo@gitmesh.test}"
export GIT_COMMITTER_NAME="$GIT_AUTHOR_NAME"
export GIT_COMMITTER_EMAIL="$GIT_AUTHOR_EMAIL"

mkdir -p "$REMOTES"
printf 'demo workspace: %s\n' "$WORK"

# ------------------------------------------------------- an existing project --
step "1. An existing project: one directory, three real Git repositories"
mkdir -p "$PROJECT/src" "$PROJECT/engine/src" "$PROJECT/renderer/src"
git -C "$PROJECT" init -q -b main
printf '# shop\n' > "$PROJECT/README.md"
git -C "$PROJECT" add -A && git -C "$PROJECT" commit -qm "initial root commit"

for part in engine renderer; do
  git -C "$PROJECT/$part" init -q -b main
  printf '# %s\n' "$part" > "$PROJECT/$part/README.md"
  git -C "$PROJECT/$part" add -A
  git -C "$PROJECT/$part" commit -qm "initial $part commit"
done

# ------------------------------------------------------------ initialise it --
step "2. Tell GitMesh about the project"
cd "$PROJECT"
run "$GITMESH" init . --name shop
run "$GITMESH" discover --depth 2

step "3. Mark the two directories as independent physical repositories"
run "$GITMESH" configure add engine --remote "git@github.com:acme/shop-engine.git"
run "$GITMESH" configure add renderer --remote "git@github.com:acme/shop-renderer.git"
run "$GITMESH" configure list

# ------------------------------------------------------------ normal work --
step "4. Do normal work: change files in all three repositories at once"
printf 'fn main() {}\n' > src/main.rs
printf 'pub fn tick() {}\n' >> engine/src/lib.rs
printf 'export const draw = () => {};\n' > renderer/src/index.js
run "$GITMESH" status --changes

step "5. One logical commit, three real Git commits"
run "$GITMESH" commit -m "add the first end-to-end slice"
for dir in . engine renderer; do
  printf '%-10s %s\n' "$dir" "$(git -C "$PROJECT/$dir" log -1 --pretty='%h %s')"
done

# ----------------------------------------------------------- remotes & sync --
# Step 3 already configured `origin` in engine and renderer with their GitHub URLs.
# For an offline demo we now point every repository at a local bare remote instead.
step "6. Publish each repository to its own remote (bare repositories here)"
for part in root engine renderer; do
  git init -q --bare -b main "$REMOTES/$part.git"
done
run "$GITMESH" configure remote root --url "$REMOTES/root.git" --set-git-remote
git -C "$PROJECT/engine" remote set-url origin "$REMOTES/engine.git"
git -C "$PROJECT/renderer" remote set-url origin "$REMOTES/renderer.git"
run "$GITMESH" push

step "7. One logical branch, switch everywhere"
run "$GITMESH" branch create feature/gpu
run "$GITMESH" checkout feature/gpu
run "$GITMESH" branch
# Publish the new branch in every repository (upstreams are set automatically).
run "$GITMESH" push

# -------------------------------------------------------------- a conflict --
step "8. Another developer changes the same file: pull reports, never hides"
git clone -q "$REMOTES/renderer.git" "$WORK/other-renderer"
git -C "$WORK/other-renderer" checkout -q feature/gpu
printf '// theirs\n' >> "$WORK/other-renderer/src/index.js"
git -C "$WORK/other-renderer" add -A
git -C "$WORK/other-renderer" commit -qm "their change"
git -C "$WORK/other-renderer" push -q origin feature/gpu

printf '// ours\n' >> renderer/src/index.js
run "$GITMESH" commit -m "our change"
run "$GITMESH" pull --strategy merge || true

step "9. The conflict is left visible in the affected repository only"
git -C renderer status --short || true

step "10. Resolve it (plain Git in one repository) and finish the logical operation"
printf '// merged\n' > renderer/src/index.js
git -C renderer add -A
run "$GITMESH" commit -m "resolve the renderer conflict"
run "$GITMESH" push
run "$GITMESH" status

printf '\n\033[1;32mDemo complete.\033[0m Project: %s\n' "$PROJECT"
printf 'Try the terminal interface:   cd %s && %s ui\n' "$PROJECT" "$GITMESH"
