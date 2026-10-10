//! Root and child repositories stay separate: their files, their Git histories, their status,
//! their staging and their commits. Each test uses temporary repositories and local bare
//! remotes only.
//!
//! The rules under test, in plain words:
//! * the project root is the directory that holds `.gitmesh/project.toml`; the root repository
//!   is the Git repository at that directory. A child is a Git repository in its own directory
//!   below the root, listed in the manifest. There is no `.root` directory.
//! * a child's files live only in the child's directory and are committed only in the child's
//!   history; the root never stages, commits, moves or overwrites them;
//! * a nested Git repository that is not in the manifest is not part of the root either: it is
//!   never recorded in the root as a gitlink.

mod common;

use std::path::Path;

use common::*;
use gitmesh::manage::RepositoryIntent;
use gitmesh::model::RepositoryRole;
use gitmesh::ops::{
    commit_project, stage_project, CommitOptions, RepositorySelection, StageOptions,
};
use gitmesh::testkit::TempDir;

/// The manifest: the root's own, designated metadata file.
const MANIFEST: &str = ".gitmesh/project.toml";

/// A project with a RAMforge child cloned from the seeded remote, and a root commit.
fn project_with_child(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let bare = ramforge_remote(tmp);
    let root = project(tmp);
    let child_remote_head = rev(&bare, "refs/heads/main").expect("the remote has history");
    let result = apply(&plan(
        &root,
        RepositoryIntent::Clone {
            path: "RAMforge".into(),
            id: "RAMforge".into(),
            remote: url(&bare),
            branch: None,
        },
    ));
    assert!(result.is_success(), "the child is cloned");
    (bare, root, child_remote_head)
}

fn ls_tree(dir: &Path, rev: &str) -> String {
    git(dir, &["ls-tree", "-r", rev])
}

fn committed_paths(dir: &Path, rev: &str) -> Vec<String> {
    git(dir, &["show", "--name-only", "--format=", rev])
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

// -------------------------------------------------------- filesystem layout --

#[test]
fn cloning_the_child_puts_its_sources_only_in_its_own_directory() {
    let tmp = TempDir::new("sep-layout").unwrap();
    let (_bare, root, remote_head) = project_with_child(&tmp);

    // The child's sources are in RAMforge/, and nowhere else in the root.
    assert_eq!(
        std::fs::read_to_string(root.join("RAMforge/src/main.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert!(
        !root.join("src").exists(),
        "no copy of the sources in the root"
    );
    assert!(!root.join("Cargo.toml").exists());
    assert!(
        !root.join(".root").exists(),
        "there is no '.root' directory"
    );

    // The child is its own repository, with the remote's history.
    assert_eq!(
        rev(&root.join("RAMforge"), "HEAD").as_deref(),
        Some(remote_head.as_str())
    );

    // The root tracks only its own files; the child is not part of its tree.
    let root_tree = ls_tree(&root, "HEAD");
    assert!(
        !root_tree.contains("RAMforge"),
        "no gitlink or copy: {root_tree}"
    );
    assert!(!root_tree.contains("src/main.rs"), "{root_tree}");
    assert!(
        git(&root, &["status", "--porcelain"])
            .lines()
            .all(|l| !l.contains("src/")),
        "the root status does not list the child's files"
    );

    // The manifest is the one designated place, and it lists both repositories correctly.
    assert!(root.join(".gitmesh/project.toml").is_file());
    let project = load(&root);
    assert_eq!(project.root, root);
    let child = project.repository("RAMforge").expect("listed");
    assert_eq!(child.role, RepositoryRole::External);
    assert_eq!(child.relative_slash(), "RAMforge");
    assert_eq!(child.absolute_path, root.join("RAMforge"));
    assert_eq!(project.root_repository().absolute_path, root);
}

#[test]
fn adding_a_child_with_its_own_history_leaves_both_histories_and_files_alone() {
    let tmp = TempDir::new("sep-add").unwrap();
    let root = project(&tmp);
    // An existing repository with its own history, inside the root's tree.
    let engine = root.join("engine");
    write(&engine.join("lib.rs"), "pub fn engine() {}\n");
    git(&engine, &["init", "-q", "-b", "main"]);
    identity(&engine);
    git(&engine, &["add", "-A"]);
    git(&engine, &["commit", "-q", "-m", "engine: own history"]);
    let engine_head = rev(&engine, "HEAD");
    let root_head = rev(&root, "HEAD");

    let result = apply(&plan(
        &root,
        RepositoryIntent::Add {
            path: "engine".into(),
            id: "engine".into(),
            remote: None,
            branch: None,
            initialize: false,
            configure_remote: false,
            untrack_from_root: false,
        },
    ));
    assert!(result.is_success());

    assert_eq!(
        rev(&engine, "HEAD"),
        engine_head,
        "the child's history is unchanged"
    );
    assert_eq!(
        rev(&root, "HEAD"),
        root_head,
        "the root's history is unchanged"
    );
    assert!(
        git(&root, &["ls-files", "--stage"])
            .lines()
            .all(|l| !l.contains("engine")),
        "the root's index does not take the child's files"
    );
    assert!(load(&root).repository("engine").is_some());
}

// --------------------------------------------------------- commits and status --

#[test]
fn root_and_child_commits_are_independent_histories() {
    let tmp = TempDir::new("sep-commits").unwrap();
    let (_bare, root, _) = project_with_child(&tmp);
    let child = root.join("RAMforge");
    let root_before = rev(&root, "HEAD");
    let child_before = rev(&child, "HEAD");

    write(&root.join("notes.md"), "root notes\n");
    write(&child.join("src/lib.rs"), "pub fn child() {}\n");

    // Commit the root only: the child's file is not part of it.
    let root_report = commit_project(
        &load(&root),
        &runner(),
        &CommitOptions {
            selection: RepositorySelection::Ids(vec!["root".into()]),
            ..CommitOptions::new("root: notes")
        },
    )
    .unwrap();
    assert!(root_report.is_success());
    // The root's own files, and its manifest (which the clone changed): nothing of the child's.
    assert_eq!(
        committed_paths(&root, "HEAD"),
        vec![MANIFEST.to_string(), "notes.md".to_string()]
    );
    assert_eq!(
        rev(&child, "HEAD"),
        child_before,
        "the child is untouched by the root commit"
    );

    // Commit the child only: the root is untouched.
    let root_now = rev(&root, "HEAD");
    let child_report = commit_project(
        &load(&root),
        &runner(),
        &CommitOptions {
            selection: RepositorySelection::Ids(vec!["RAMforge".into()]),
            ..CommitOptions::new("child: lib")
        },
    )
    .unwrap();
    assert!(child_report.is_success());
    assert_eq!(
        committed_paths(&child, "HEAD"),
        vec!["src/lib.rs".to_string()]
    );
    assert_eq!(
        rev(&root, "HEAD"),
        root_now,
        "the root is untouched by the child commit"
    );

    // The two histories share nothing after the separation: each only has its own commits.
    assert_ne!(rev(&root, "HEAD"), root_before);
    assert_ne!(rev(&child, "HEAD"), child_before);
    let root_log = git(&root, &["log", "--format=%s"]);
    let child_log = git(&child, &["log", "--format=%s"]);
    assert!(
        root_log.contains("root: notes") && !root_log.contains("child: lib"),
        "{root_log}"
    );
    assert!(
        child_log.contains("child: lib") && !child_log.contains("root: notes"),
        "{child_log}"
    );
    assert!(!git(&child, &["ls-tree", "-r", "HEAD"]).contains("notes.md"));
    assert!(!git(&root, &["ls-tree", "-r", "HEAD"]).contains("src/lib.rs"));
}

#[test]
fn a_root_commit_never_includes_a_dirty_child() {
    let tmp = TempDir::new("sep-dirty").unwrap();
    let (_bare, root, _) = project_with_child(&tmp);
    write(
        &root.join("RAMforge/src/main.rs"),
        "fn main() { /* child edit */ }\n",
    );
    write(&root.join("README.md"), "# Project, edited\n");

    // The root only: the default selection would also commit the child, which is its own job.
    let report = commit_project(
        &load(&root),
        &runner(),
        &CommitOptions {
            selection: RepositorySelection::Ids(vec!["root".into()]),
            ..CommitOptions::new("root: readme")
        },
    )
    .unwrap();
    assert!(report.is_success());

    let paths = committed_paths(&root, "HEAD");
    assert_eq!(
        paths,
        vec![MANIFEST.to_string(), "README.md".to_string()],
        "{paths:?}"
    );
    // The child's edit is still an edit in the child: not committed by the root.
    let child_status = git(&root.join("RAMforge"), &["status", "--porcelain"]);
    assert!(child_status.contains("src/main.rs"), "{child_status}");
    assert_eq!(
        std::fs::read_to_string(root.join("RAMforge/src/main.rs")).unwrap(),
        "fn main() { /* child edit */ }\n",
        "the child's file is not overwritten"
    );
}

#[test]
fn staging_the_root_never_stages_the_child() {
    let tmp = TempDir::new("sep-stage").unwrap();
    let (_bare, root, _) = project_with_child(&tmp);
    write(
        &root.join("RAMforge/src/main.rs"),
        "fn main() { /* child edit */ }\n",
    );
    write(&root.join("README.md"), "# Project, edited\n");

    let report = stage_project(
        &load(&root),
        &runner(),
        &StageOptions {
            selection: RepositorySelection::Ids(vec!["root".into()]),
            ..StageOptions::default()
        },
    )
    .unwrap();
    assert!(report.is_success(), "{:?}", report);

    let staged = git(&root, &["diff", "--cached", "--name-only"]);
    assert_eq!(
        staged.trim(),
        format!("{MANIFEST}\nREADME.md"),
        "only the root's own files are staged: {staged}"
    );
    assert!(
        git(&root.join("RAMforge"), &["diff", "--cached", "--name-only"])
            .trim()
            .is_empty(),
        "the child's index is untouched by the root"
    );
}

// ----------------------------------------------------- nested, unmanaged repos --

#[test]
fn an_unregistered_nested_repository_is_never_recorded_as_a_gitlink() {
    let tmp = TempDir::new("sep-nested").unwrap();
    let root = project(&tmp);
    // A repository nobody registered, inside the root.
    let scratch = root.join("scratch");
    write(&scratch.join("a.txt"), "scratch\n");
    git(&scratch, &["init", "-q", "-b", "main"]);
    identity(&scratch);
    git(&scratch, &["add", "-A"]);
    git(&scratch, &["commit", "-q", "-m", "scratch: own history"]);
    let scratch_head = rev(&scratch, "HEAD");
    write(&root.join("notes.md"), "root note\n");

    let report =
        commit_project(&load(&root), &runner(), &CommitOptions::new("root: note")).unwrap();
    assert!(report.is_success());

    let tree = ls_tree(&root, "HEAD");
    assert!(!tree.contains("160000"), "no gitlink was created: {tree}");
    assert!(!tree.contains("scratch"), "{tree}");
    assert_eq!(committed_paths(&root, "HEAD"), vec!["notes.md".to_string()]);
    // The nested repository is exactly as it was.
    assert_eq!(rev(&scratch, "HEAD"), scratch_head);
    assert!(git(&scratch, &["status", "--porcelain"]).trim().is_empty());
}

#[test]
fn a_nested_repository_staged_by_hand_is_unstaged_and_not_committed() {
    let tmp = TempDir::new("sep-hand").unwrap();
    let root = project(&tmp);
    let scratch = root.join("scratch");
    write(&scratch.join("a.txt"), "scratch\n");
    git(&scratch, &["init", "-q", "-b", "main"]);
    identity(&scratch);
    git(&scratch, &["add", "-A"]);
    git(&scratch, &["commit", "-q", "-m", "scratch: own history"]);
    write(&root.join("notes.md"), "root note\n");
    // Someone runs `git add` in the root, by hand.
    git(&root, &["add", "scratch"]);
    let scratch_head = rev(&scratch, "HEAD");

    let report =
        commit_project(&load(&root), &runner(), &CommitOptions::new("root: note")).unwrap();
    assert!(report.is_success(), "{report:?}");

    assert!(
        !ls_tree(&root, "HEAD").contains("scratch"),
        "the gitlink is not committed"
    );
    assert_eq!(committed_paths(&root, "HEAD"), vec!["notes.md".to_string()]);
    assert_eq!(rev(&scratch, "HEAD"), scratch_head);
}

// ------------------------------------------------------- validation of targets --

#[test]
fn each_repository_resolves_to_its_own_directory_and_only_that_one_changes() {
    let tmp = TempDir::new("sep-targets").unwrap();
    let (_bare, root, _) = project_with_child(&tmp);
    let project = load(&root);

    // Targets: the root is the root; the child is its directory; nothing else.
    assert_eq!(project.repository("root").unwrap().absolute_path, root);
    assert_eq!(
        project.repository("RAMforge").unwrap().absolute_path,
        root.join("RAMforge")
    );
    assert_eq!(project.sorted_repositories().len(), 2);

    // A change limited to the child touches only the child.
    write(
        &root.join("RAMforge/src/main.rs"),
        "fn main() { /* only the child */ }\n",
    );
    let root_before = rev(&root, "HEAD");
    let report = commit_project(
        &project,
        &runner(),
        &CommitOptions {
            selection: RepositorySelection::Ids(vec!["RAMforge".into()]),
            ..CommitOptions::new("child only")
        },
    )
    .unwrap();
    assert!(report.is_success());
    assert_eq!(rev(&root, "HEAD"), root_before);
    assert_eq!(
        committed_paths(&root.join("RAMforge"), "HEAD"),
        vec!["src/main.rs".to_string()]
    );
}

#[test]
fn a_child_whose_files_the_root_still_tracks_is_flagged_as_shared_ownership() {
    // The root tracks files inside a directory that is then registered. GitMesh copies nothing
    // and deletes nothing; it says plainly that two repositories would own those files, and the
    // untracking is an explicit choice of the user (it is never done silently).
    let tmp = TempDir::new("sep-duplicate").unwrap();
    let root = project(&tmp);
    write(&root.join("engine/lib.rs"), "pub fn engine() {}\n");
    git(&root, &["add", "-A"]);
    git(
        &root,
        &["commit", "-q", "-m", "root tracks the engine files"],
    );
    let root_head = rev(&root, "HEAD");

    let refused = plan(
        &root,
        RepositoryIntent::Add {
            path: "engine".into(),
            id: "engine".into(),
            remote: None,
            branch: None,
            initialize: true,
            configure_remote: false,
            untrack_from_root: false,
        },
    );
    assert!(
        refused
            .warnings
            .iter()
            .any(|w| w.contains("two repositories")),
        "the shared ownership is reported: {:?}",
        refused.warnings
    );
    assert!(
        refused.changes.iter().all(|c| c.kind
            != gitmesh::manage::RepositoryChangeKind::UntrackFromRoot
            || c.after.as_deref() == c.before.as_deref()),
        "nothing is untracked without the explicit choice"
    );
    assert_eq!(rev(&root, "HEAD"), root_head);
    assert!(root.join("engine/lib.rs").is_file(), "the files are kept");
}
