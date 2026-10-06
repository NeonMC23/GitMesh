//! End-to-end workflows against real Git repositories.
//!
//! These tests drive the public GitMesh API the way a user does: configure a project,
//! change files across several repositories, read one status, commit once, branch and
//! check out once, pull and push everywhere. Everything runs against real `git` in a
//! temporary directory, with local bare repositories standing in for hosted remotes.

use gitmesh::analyzer::Analyzer;
use gitmesh::manifest;
use gitmesh::model::RepositoryRole;
use gitmesh::ops::{
    commit_project, fetch_project, pull_project, push_project, BranchAction, BranchOptions,
    CommitOptions, OperationReport, OutcomeKind, PullStrategy, PushOptions, SyncOptions,
};
use gitmesh::testkit::RepoFixture;

fn branch_options() -> BranchOptions {
    BranchOptions::default()
}

fn sync_options() -> SyncOptions {
    SyncOptions::new()
}

/// A three-repository project, each repository published to its own bare remote.
struct Mesh {
    fixture: RepoFixture,
    project: gitmesh::model::GitMeshProject,
}

impl Mesh {
    fn new() -> Mesh {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[
            ("root", "."),
            ("engine", "engine"),
            ("renderer", "renderer"),
        ]);
        for repo in [".", "engine", "renderer"] {
            let bare = match repo {
                "." => "remotes/root.git",
                other => Box::leak(format!("remotes/{other}.git").into_boxed_str()),
            };
            fixture.publish(repo, bare);
        }
        Mesh { fixture, project }
    }

    fn reload(&self) -> gitmesh::model::GitMeshProject {
        self.fixture.load_project()
    }

    fn commit_all(&self, message: &str) -> OperationReport {
        let project = self.reload();
        commit_project(
            &project,
            self.fixture.runner(),
            &CommitOptions::new(message),
        )
        .unwrap()
    }
}

#[test]
fn full_workflow_from_init_to_push() {
    let mesh = Mesh::new();

    // 1. Modify files in every repository of the project.
    mesh.fixture.write("src/main.rs", "fn main() {}");
    mesh.fixture.write("engine/src/lib.rs", "pub fn run() {}");
    mesh.fixture
        .write("renderer/src/index.js", "export const render = () => {};");

    // 2. One unified status sees everything and attributes it correctly.
    let project = mesh.reload();
    let analyzer = Analyzer::new(&project, mesh.fixture.runner());
    let status = analyzer.analyze();
    assert!(status.has_changes());
    assert_eq!(status.changed().count(), 3);
    let owned = analyzer.owned_changes(&status);
    let mut paths: Vec<&str> = owned.iter().map(|c| c.logical_path.as_str()).collect();
    paths.sort();
    assert_eq!(
        paths,
        vec!["engine/src/lib.rs", "renderer/src/index.js", "src/main.rs"]
    );

    // 3. One logical commit creates a real commit in each repository.
    let report = mesh.commit_all("add first implementation");
    assert!(report.is_success());
    assert_eq!(report.counts().0, 3);
    for repo in [".", "engine", "renderer"] {
        let subject = mesh.fixture.git_ok(repo, &["log", "-1", "--pretty=%s"]);
        assert_eq!(subject.trim(), "add first implementation");
    }

    // 4. One logical branch in every repository.
    let project = mesh.reload();
    let report = gitmesh::ops::branch_operation(
        &project,
        mesh.fixture.runner(),
        &BranchAction::Create {
            name: "feature/next".into(),
        },
        &branch_options(),
    )
    .unwrap();
    assert_eq!(report.counts().0, 3);

    // 5. One logical checkout.
    let report = gitmesh::ops::branch_operation(
        &project,
        mesh.fixture.runner(),
        &BranchAction::Checkout {
            name: "feature/next".into(),
            create: false,
        },
        &branch_options(),
    )
    .unwrap();
    assert!(report.is_success());
    for repo in [".", "engine", "renderer"] {
        assert_eq!(
            mesh.fixture
                .git_ok(repo, &["symbolic-ref", "--short", "HEAD"])
                .trim(),
            "feature/next"
        );
    }

    // 6. Push everything; each repository sets its own upstream.
    let report = push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
    assert!(report.is_success(), "{:#?}", report.outcomes);
    assert_eq!(report.counts().0, 3);
    for (bare, name) in [
        ("remotes/root.git", "root"),
        ("remotes/engine.git", "engine"),
        ("remotes/renderer.git", "renderer"),
    ] {
        let bare_repo = mesh.fixture.runner().repo(mesh.fixture.bare_path(bare));
        let branches = bare_repo
            .run_checked(&["branch", "--list", "feature/next"])
            .unwrap();
        assert!(branches.contains("feature/next"), "{name}: {branches}");
    }

    // 7. Pulling again changes nothing (idempotent).
    let report = pull_project(&project, mesh.fixture.runner(), &sync_options()).unwrap();
    assert!(report.is_success(), "{:#?}", report.outcomes);
    let report = push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
    assert_eq!(report.counts().1, 3, "everything should be up to date now");

    // 8. Fetch is idempotent too.
    let report = fetch_project(&project, mesh.fixture.runner(), &sync_options()).unwrap();
    assert!(report.is_success());
    let report = fetch_project(&project, mesh.fixture.runner(), &sync_options()).unwrap();
    assert!(report.is_success());
}

#[test]
fn merge_across_repositories_uses_git_and_reports_conflicts() {
    let mesh = Mesh::new();
    mesh.fixture.write_and_commit("engine/feature.txt", "base");
    mesh.commit_all("base change");

    // Create a feature branch everywhere, make a conflicting change in engine only.
    let project = mesh.reload();
    gitmesh::ops::branch_operation(
        &project,
        mesh.fixture.runner(),
        &BranchAction::Checkout {
            name: "feature/conflict".into(),
            create: true,
        },
        &branch_options(),
    )
    .unwrap();
    mesh.fixture
        .write_and_commit("engine/feature.txt", "feature side");
    mesh.commit_all("feature change");

    // Meanwhile main moves on in engine.
    mesh.fixture.git_ok("engine", &["checkout", "-q", "main"]);
    mesh.fixture
        .write_and_commit("engine/feature.txt", "main side");
    mesh.fixture
        .git_ok("engine", &["checkout", "-q", "feature/conflict"]);

    // Merge the other way round: main into the feature branch, in every repository.
    // The root and renderer have nothing to merge (their main is an ancestor), so only
    // engine can produce a conflict.
    let project = mesh.reload();
    let report = gitmesh::ops::branch_operation(
        &project,
        mesh.fixture.runner(),
        &BranchAction::Merge {
            name: "main".into(),
        },
        &branch_options(),
    )
    .unwrap();

    // The root and renderer merged (their main is an ancestor), engine conflicts.
    let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
    assert_eq!(engine.kind, OutcomeKind::Conflict);
    assert!(engine.details.iter().any(|d| d.contains("feature.txt")));
    let root = report.outcomes.iter().find(|o| o.id == "root").unwrap();
    assert!(root.kind != OutcomeKind::Failed, "{root:#?}");

    // Nothing was lost: the conflict markers are in the file, the branches intact.
    let content = std::fs::read_to_string(mesh.fixture.path().join("engine/feature.txt")).unwrap();
    assert!(content.contains("<<<<<<<"), "{content}");
}

#[test]
fn pull_updates_different_repositories_and_reports_conflicts() {
    let mesh = Mesh::new();
    mesh.fixture
        .write_and_commit("README.md", "# project\nline\n");
    mesh.commit_all("root content");
    mesh.fixture
        .write_and_commit("engine/README.md", "# engine\nline\n");
    mesh.commit_all("engine content");
    mesh.fixture.git_ok(".", &["push", "-q", "origin", "main"]);
    mesh.fixture
        .git_ok("engine", &["push", "-q", "origin", "main"]);

    // Another developer pushes a clean change to the root and a conflicting one to
    // the engine.
    let other_root = mesh
        .fixture
        .clone_outside(&mesh.fixture.bare_path("remotes/root.git"), "dev/root");
    std::fs::write(other_root.join("their-file.txt"), "new\n").unwrap();
    let repo = mesh.fixture.runner().repo(&other_root);
    repo.run_checked(&["add", "-A"]).unwrap();
    repo.run_checked(&["commit", "-q", "-m", "their root change"])
        .unwrap();
    repo.run_checked(&["push", "-q", "origin", "main"]).unwrap();

    let other_engine = mesh
        .fixture
        .clone_outside(&mesh.fixture.bare_path("remotes/engine.git"), "dev/engine");
    std::fs::write(
        other_engine.join("README.md"),
        "# engine\nchanged upstream\n",
    )
    .unwrap();
    let repo = mesh.fixture.runner().repo(&other_engine);
    repo.run_checked(&["add", "-A"]).unwrap();
    repo.run_checked(&["commit", "-q", "-m", "their engine change"])
        .unwrap();
    repo.run_checked(&["push", "-q", "origin", "main"]).unwrap();

    // Our own conflicting change in the engine.
    mesh.fixture
        .write_and_commit("engine/README.md", "# engine\nchanged locally\n");
    mesh.commit_all("our engine change");

    let project = mesh.reload();
    let options = SyncOptions {
        strategy: PullStrategy::Merge,
        ..SyncOptions::new()
    };
    let report = pull_project(&project, mesh.fixture.runner(), &options).unwrap();

    let root = report.outcomes.iter().find(|o| o.id == "root").unwrap();
    assert_eq!(root.kind, OutcomeKind::Success, "{root:#?}");
    let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
    assert_eq!(engine.kind, OutcomeKind::Conflict);
    assert!(engine.details.iter().any(|d| d.contains("README.md")));
    // The conflict is left in place for the user to resolve; nothing was discarded.
    let content = std::fs::read_to_string(mesh.fixture.path().join("engine/README.md")).unwrap();
    assert!(content.contains("<<<<<<<"));
}

#[test]
fn partial_network_failure_leaves_successful_repositories_updated() {
    let mesh = Mesh::new();
    mesh.fixture.write_and_commit("src/a.rs", "a");
    mesh.fixture
        .write_and_commit("renderer/index.js", "renderer change");
    mesh.commit_all("changes everywhere");

    // The renderer's remote disappears (network/remote failure simulation).
    std::fs::remove_dir_all(mesh.fixture.bare_path("remotes/renderer.git")).unwrap();

    let project = mesh.reload();
    let report = push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
    assert!(!report.is_success());
    assert!(report.is_partial());
    let by_id = |id: &str| report.outcomes.iter().find(|o| o.id == id).unwrap().kind;
    assert_eq!(by_id("root"), OutcomeKind::Success);
    assert_eq!(by_id("engine"), OutcomeKind::Skipped);
    assert_eq!(by_id("renderer"), OutcomeKind::Failed);
    assert_eq!(report.exit_code(), 1);

    // The good repositories really did push.
    let bare_root = mesh
        .fixture
        .runner()
        .repo(mesh.fixture.bare_path("remotes/root.git"));
    let log = bare_root.run_checked(&["log", "--oneline"]).unwrap();
    assert!(log.contains("update src/a.rs"), "{log}");
}

#[test]
fn missing_repository_is_reported_and_does_not_block_the_rest() {
    let mesh = Mesh::new();
    mesh.fixture.write("src/main.rs", "root");
    mesh.fixture.write("renderer/index.js", "renderer");

    // One configured repository disappears from disk.
    std::fs::remove_dir_all(mesh.fixture.path().join("engine")).unwrap();

    let project = mesh.reload();
    let report = commit_project(
        &project,
        mesh.fixture.runner(),
        &CommitOptions::new("partial"),
    )
    .unwrap();
    let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
    assert_eq!(engine.kind, OutcomeKind::Failed);
    assert!(engine.details.iter().any(|d| d.contains("does not exist")));
    assert_eq!(report.counts().0, 2);
    assert!(report.is_partial());

    // The status also says so instead of hiding it.
    let analyzer = Analyzer::new(&project, mesh.fixture.runner());
    let status = analyzer.analyze();
    assert_eq!(status.unavailable().count(), 1);
    assert!(status.notices.iter().any(|n| n.contains("engine")));
}

#[test]
fn incorrect_remote_is_reported_with_an_actionable_hint() {
    let mesh = Mesh::new();
    mesh.fixture.write_and_commit("src/a.rs", "a");
    mesh.commit_all("change");
    mesh.fixture.git_ok(
        ".",
        &["remote", "set-url", "origin", "/nonexistent/root.git"],
    );

    let project = mesh.reload();
    let report = push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
    let root = report.outcomes.iter().find(|o| o.id == "root").unwrap();
    assert_eq!(root.kind, OutcomeKind::Failed);
    assert!(
        root.details
            .iter()
            .any(|d| d.to_lowercase().contains("could not be found")
                || d.to_lowercase().contains("authentication")),
        "{root:#?}"
    );
}

#[test]
fn dirty_and_untracked_repositories_are_handled_explicitly() {
    let mesh = Mesh::new();
    mesh.fixture.write("engine/modified.txt", "content");
    mesh.commit_all("engine baseline");

    // Now: an untracked file in the root, a modification in engine, a deletion.
    mesh.fixture.write("untracked.txt", "new file");
    mesh.fixture.append("engine/modified.txt", "more\n");
    mesh.fixture.remove("README.md");

    let project = mesh.reload();
    let analyzer = Analyzer::new(&project, mesh.fixture.runner());
    let status = analyzer.analyze();

    let root = status.repositories.iter().find(|r| r.id == "root").unwrap();
    assert!(root.has_changes());
    assert!(root.status.as_ref().unwrap().untracked().count() >= 1);
    assert!(root.status.as_ref().unwrap().deleted().count() >= 1);

    // A bare `git checkout`-style operation must not throw away the dirty work.
    let before = std::fs::read_to_string(mesh.fixture.path().join("engine/modified.txt")).unwrap();
    let report = gitmesh::ops::branch_operation(
        &project,
        mesh.fixture.runner(),
        &BranchAction::Checkout {
            name: "other".into(),
            create: true,
        },
        &branch_options(),
    )
    .unwrap();
    assert!(report.is_success());
    let after = std::fs::read_to_string(mesh.fixture.path().join("engine/modified.txt")).unwrap();
    assert_eq!(before, after);

    // And a commit records both the untracked and the deleted file.
    let report = commit_project(
        &project,
        mesh.fixture.runner(),
        &CommitOptions::new("record"),
    )
    .unwrap();
    assert!(report.is_success());
    let tracked = mesh.fixture.git_ok(".", &["ls-files"]);
    assert!(tracked.contains("untracked.txt"));
    assert!(!tracked.contains("README.md"));
}

#[test]
fn restarting_gitmesh_reopens_the_same_project() {
    let mesh = Mesh::new();
    mesh.fixture.write("src/main.rs", "content");
    mesh.commit_all("work");

    // Simulate a restart: everything is reloaded from the manifest on disk.
    let reloaded = manifest::load_from_root(mesh.fixture.path()).unwrap();
    assert_eq!(reloaded, mesh.project);

    let analyzer = Analyzer::new(&reloaded, mesh.fixture.runner());
    let status = analyzer.analyze();
    assert_eq!(status.repositories.len(), 3);
    assert!(!status.has_changes());
    let ids: Vec<&str> = status.repositories.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["root", "engine", "renderer"]);
    assert_eq!(
        status
            .repositories
            .iter()
            .find(|r| r.id == "engine")
            .unwrap()
            .role,
        RepositoryRole::External
    );
}

#[test]
fn new_external_repository_can_be_added_to_a_live_project() {
    let mesh = Mesh::new();
    // A repository that already exists on disk but is not configured yet.
    mesh.fixture.init_repo("tools");
    mesh.fixture.write("tools/README.md", "# tools\n");
    mesh.fixture.commit("tools", "tools init");

    let project = mesh.reload();
    let updated = gitmesh::discovery::assign_repository(
        &project,
        std::path::Path::new("tools"),
        &gitmesh::discovery::AssignOptions::default(),
        mesh.fixture.runner(),
    )
    .unwrap();
    manifest::save_project(&updated).unwrap();

    // The new repository takes part in the very next logical operation.
    mesh.fixture.write("tools/new.rs", "code");
    mesh.fixture.write("src/main.rs", "root code");
    let project = mesh.reload();
    let report = commit_project(
        &project,
        mesh.fixture.runner(),
        &CommitOptions::new("add tools"),
    )
    .unwrap();
    assert_eq!(report.counts().0, 2);
    assert_eq!(
        mesh.fixture
            .git_ok("tools", &["log", "-1", "--pretty=%s"])
            .trim(),
        "add tools"
    );
}

#[test]
fn invalid_project_configuration_is_refused_before_any_git_command() {
    let fixture = RepoFixture::new();
    let mut project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
    // Overlapping repository paths would make ownership ambiguous.
    project
        .repositories
        .push(gitmesh::model::PhysicalRepository {
            id: "nested".into(),
            role: RepositoryRole::External,
            relative_path: std::path::PathBuf::from("engine/deep"),
            remote_url: None,
            branch: None,
            absolute_path: fixture.path().join("engine/deep"),
        });
    let err = manifest::save_project(&project).unwrap_err();
    assert!(err.to_string().contains("overlap"), "{err}");

    // Loading a malformed manifest reports every problem at once.
    let text = std::fs::read_to_string(manifest::manifest_path(fixture.path())).unwrap();
    let broken = format!("{text}\n[[repositories]]\nid = \"engine\"\npath = \"engine\"\n");
    let err = manifest::parse_manifest(
        &broken,
        fixture.path(),
        &manifest::manifest_path(fixture.path()),
    )
    .unwrap_err();
    let message = err.to_string();
    assert!(message.contains("duplicate repository id"), "{message}");
    assert!(message.contains("same path"), "{message}");
}

#[test]
fn repeated_operations_are_idempotent() {
    let mesh = Mesh::new();
    mesh.fixture.write("src/main.rs", "content");

    // Committing twice: the second one has nothing to do.
    let first = mesh.commit_all("once");
    assert_eq!(first.counts().0, 1);
    let second = mesh.commit_all("once");
    assert_eq!(second.counts(), (0, 3, 0, 0));

    // Branch creation twice: the second run reports "already exists".
    let project = mesh.reload();
    let action = BranchAction::Create { name: "dup".into() };
    let first =
        gitmesh::ops::branch_operation(&project, mesh.fixture.runner(), &action, &branch_options())
            .unwrap();
    assert_eq!(first.counts().0, 3);
    let second =
        gitmesh::ops::branch_operation(&project, mesh.fixture.runner(), &action, &branch_options())
            .unwrap();
    assert!(second.is_success());
    assert_eq!(second.counts().1, 3);

    // Push everything once, then verify that repeating the operation is a no-op.
    let report = push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
    assert!(report.is_success());
    for _ in 0..2 {
        let report = fetch_project(&project, mesh.fixture.runner(), &sync_options()).unwrap();
        assert!(report.is_success());
        let report =
            push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
        assert!(report.is_success());
        assert_eq!(report.counts().1, 3);
    }
}

#[test]
fn detached_head_is_detected_and_operations_stay_safe() {
    let mesh = Mesh::new();
    // Make sure the engine has at least one commit, then detach it.
    mesh.fixture.write_and_commit("engine/x.txt", "x");
    mesh.commit_all("engine commit");
    let oid = mesh.fixture.git_ok("engine", &["rev-parse", "HEAD"]);
    mesh.fixture
        .git_ok("engine", &["checkout", "-q", oid.trim()]);
    mesh.fixture.write_and_commit("src/a.rs", "root");
    mesh.commit_all("root commit");

    let project = mesh.reload();
    let report = push_project(&project, mesh.fixture.runner(), &PushOptions::default()).unwrap();
    let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
    assert_eq!(engine.kind, OutcomeKind::Failed);
    assert!(engine.summary.contains("detached"));

    // A commit in a detached repository is still honoured, and said out loud.
    mesh.fixture.write("engine/detached.txt", "work");
    let report = commit_project(
        &project,
        mesh.fixture.runner(),
        &CommitOptions::new("detached work"),
    )
    .unwrap();
    let engine = report.outcomes.iter().find(|o| o.id == "engine").unwrap();
    assert_eq!(engine.kind, OutcomeKind::Success);
    assert!(engine.details.iter().any(|d| d.contains("detached")));
}

#[test]
fn empty_and_clean_repositories_are_skipped_not_ignored() {
    let fixture = RepoFixture::new();
    // The external repository has no commits at all (`git init` only).
    let project = fixture.project_without_commits(&[("root", "."), ("fresh", "fresh")]);
    fixture.write("fresh/hello.txt", "hi");

    let report = commit_project(&project, fixture.runner(), &CommitOptions::new("first")).unwrap();
    let fresh = report.outcomes.iter().find(|o| o.id == "fresh").unwrap();
    assert_eq!(fresh.kind, OutcomeKind::Success);
    assert_eq!(
        fixture
            .git_ok("fresh", &["log", "--oneline"])
            .lines()
            .count(),
        1
    );

    // Now clean everywhere: nothing to commit, nothing to push, nothing to pull.
    let report = commit_project(&project, fixture.runner(), &CommitOptions::new("again")).unwrap();
    assert_eq!(report.counts(), (0, 2, 0, 0));

    let report = push_project(&project, fixture.runner(), &PushOptions::default()).unwrap();
    assert!(report
        .outcomes
        .iter()
        .all(|o| o.kind != OutcomeKind::Success));
}

#[test]
fn selection_limits_operations_to_the_chosen_repository() {
    let mesh = Mesh::new();
    mesh.fixture.write("src/main.rs", "root");
    mesh.fixture.write("engine/src/lib.rs", "engine");
    mesh.fixture.write("renderer/src/index.js", "renderer");

    let project = mesh.reload();
    let options = CommitOptions {
        selection: gitmesh::ops::RepositorySelection::Subtree(std::path::PathBuf::from("engine")),
        ..CommitOptions::new("only engine")
    };
    let report = commit_project(&project, mesh.fixture.runner(), &options).unwrap();
    assert_eq!(report.outcomes.len(), 1);
    assert_eq!(report.outcomes[0].id, "engine");

    // The other repositories were not touched at all.
    let root_status = mesh
        .fixture
        .git_ok(".", &["status", "--porcelain", "-uall"]);
    assert!(root_status.contains("src/main.rs"), "{root_status}");
    let renderer_status = mesh
        .fixture
        .git_ok("renderer", &["status", "--porcelain", "-uall"]);
    assert!(
        renderer_status.contains("src/index.js"),
        "{renderer_status}"
    );
}
