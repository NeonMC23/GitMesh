# The GitMesh manifest

The manifest is the single source of truth for a GitMesh project: it describes the
logical project, its root repository and its external repositories.

* **Location:** `<project root>/.gitmesh/project.toml`
* **Format version:** `1`
* **Written by:** GitMesh (`gitmesh init`, `gitmesh configure ...`, the terminal UI)
* **Read by:** every operation that needs to know repository boundaries

The manifest is an *internal persistent representation*: users are not expected to
hand-edit it as their primary workflow, but it is plain readable TOML so that it can be
reviewed, diffed and committed alongside the project.

## 1. Format

```toml
version = 1
name = "my-project"

[root]
id = "root"                                          # optional, defaults to "root"
path = "."                                           # optional, must be "." when present
remote = "git@github.com:acme/my-project.git"        # optional
branch = "main"                                      # optional hint

[[repositories]]
id = "engine"
path = "engine"
remote = "git@github.com:acme/engine.git"
branch = "main"

[[repositories]]
id = "renderer"
path = "renderer"
remote = "https://github.com/acme/renderer.git"

[[repositories]]
id = "vendor-lib"
path = "vendor/lib"
```

### Fields

| Key | Required | Meaning |
| --- | --- | --- |
| `version` | yes | Manifest format version. A build refuses versions it does not know (currently `1`). |
| `name` | no | Logical project name. Defaults to the project directory name. |
| `[root]` | no | Root repository configuration. Defaults to `id = "root"`, `path = "."`, no remote. |
| `root.id` | no | Logical id of the root repository. |
| `root.path` | no | Must be `.` (or absent): the root repository owns the project root. |
| `root.remote` | no | Remote URL of the root repository. |
| `root.branch` | no | Branch hint (advisory: Git remains authoritative for the current branch). |
| `[[repositories]]` | no | Zero or more external repositories. |
| `repositories[].id` | yes | Logical identifier, unique in the project. |
| `repositories[].path` | yes | Directory relative to the project root. |
| `repositories[].remote` | no | Remote URL (recorded; configure it in the repository with `--set-git-remote`). |
| `repositories[].branch` | no | Branch hint. |

Unknown keys are rejected by the parser (a typo must not be silently ignored).

## 2. Validation rules

Validation runs when the manifest is loaded, when it is saved and when a project is
assembled programmatically. **All problems are reported at once**, so a broken
configuration can be fixed in one pass.

| Rule | Error |
| --- | --- |
| `version` must be supported | `manifest version 99 is not supported by this build (expected 1)` |
| Exactly one root repository | `the project must define exactly one root repository` |
| The root repository lives at the project root | `the root repository path must be the project root (got 'src')` |
| Repository ids: non-empty, `[A-Za-z0-9._-]` | `repository id 'my repo' contains unsupported characters` |
| Repository ids unique | `duplicate repository id 'engine'` |
| Repository paths unique | `repositories 'a' and 'b' are assigned the same path 'shared'` |
| Repository paths must not overlap | `repository paths overlap: 'engine' ('engine') and 'nested' ('engine/deep') - a directory cannot belong to two physical repositories` |
| (The root repository being the ancestor of externals is *allowed* — that is the point of GitMesh) | — |
| Paths must be relative, non-escaping, not `.git` | `repository path '../outside' must not contain '..' components` |
| External repositories must not use the project root | `external repository 'engine' must not use the project root as its path` |
| Remote URLs must look like Git URLs | `repository 'engine' remote URL 'not-a-url' is not a supported Git URL (expected https://, ssh://, git@host:path, file:// or a local path)` |
| Two repositories must not share a remote URL | `repositories 'a' and 'b' use the same remote URL 'git@github.com:acme/x.git'` |
| Branch names must be valid Git branch names | `repository 'engine' branch 'feature/..bad' is not a valid Git branch name` |

Errors are printed as a list:

```console
$ gitmesh configure add engine/deep
gitmesh: project configuration is invalid:
  - repository paths overlap: 'engine' ('engine') and 'nested' ('engine/deep') - a
    directory cannot belong to two physical repositories
```

Exit code `2` for configuration errors, `1` for operational failures.

## 3. Semantics

* **Ownership.** A path relative to the project root belongs to the *longest* matching
  repository path. `.` matches everything, so the root repository owns all content that
  no external repository claims.
* **Root repository.** Always present in the model, even when `[root]` is absent from the
  file: a project whose root is not a Git repository yet can still be configured and
  later `git init`ed.
* **External repository boundaries are explicit.** GitMesh never infers them from the
  presence of a `.git` directory. Discovery reports candidates; only a `configure add`
  (or the equivalent UI action) makes a directory an external repository.
* **Missing repositories are tolerated.** If a configured directory is missing or is no
  longer a repository root, operations report that repository as *failed* with the reason,
  and the rest of the project keeps working. Nothing is repaired automatically.
* **Remote URLs are documentation plus configuration.** GitMesh records them in the
  manifest and can write them into the repository (`configure remote <id> --url ... --set-git-remote`).
  It never contacts them.
* **Branch hints are advisory.** GitMesh's `checkout`/`branch` operations act on branches
  the user names; `branch` in the manifest is a default hint for future automation.

## 4. Manifest files and Git

`gitmesh init` creates `.gitmesh/project.toml`. Because the manifest describes the project
as a whole, it normally belongs in the **root repository** and should be committed:

```console
$ gitmesh init
$ gitmesh commit -m "add GitMesh project manifest"
```

The metadata directory is skipped by `gitmesh discover` (like `.git`, `node_modules`,
`target` and `__pycache__`) and is never presented as user content.

## 5. Complete example

Project layout:

```text
shop/
├── .gitmesh/project.toml
├── src/                   (root repository: shop)
├── shared/theme/          (external repository: theme)
└── services/api/          (external repository: api)
```

Manifest:

```toml
version = 1
name = "shop"

[root]
remote = "git@github.com:acme/shop.git"

[[repositories]]
id = "theme"
path = "shared/theme"
remote = "git@github.com:acme/shop-theme.git"
branch = "main"

[[repositories]]
id = "api"
path = "services/api"
remote = "git@github.com:acme/shop-api.git"
```

Ownership resulting from this manifest:

| Path | Owning repository |
| --- | --- |
| `src/checkout.rs` | `root` |
| `shared/theme/button.scss` | `theme` |
| `services/api/handler.rs` | `api` |
| `README.md` | `root` |

## 6. Versioning policy

The manifest carries an explicit `version`. Any incompatible change to the format must
bump it, and the loader must keep rejecting unknown versions with a clear message
(never a partial guess). Adding optional keys does not require a bump; adding required
keys does.

Future fields that are anticipated but deliberately not implemented yet:

* per-repository `readonly` / `excluded` flags,
* provider metadata (`provider = "github"`, `owner`, `visibility`) once GitHub API
  support lands,
* commit/status options per repository (for example `stage = "tracked-only"`).
