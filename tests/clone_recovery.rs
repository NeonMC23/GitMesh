//! A refused "add" offers the clone that resolves it, with the exact inputs, and that clone is
//! the same reviewed, confirmed operation as any other clone.
//!
//! Everything is offline: temporary repositories and local bare remotes. Each failure test
//! checks that the directory, the existing repositories, the manifest and the remote are as
//! they were before.

mod common;

use std::process::Command;

use common::*;
use gitmesh::manage::PlanRecovery;
use gitmesh::testkit::TempDir;

// ------------------------------------------------------------------- tests --

#[test]
fn a_refused_add_offers_the_clone_with_its_exact_inputs_and_changes_nothing() {
    let tmp = TempDir::new("clone-offer").unwrap();
    let bare = ramforge_remote(&tmp);
    let root = project(&tmp);
    std::fs::create_dir_all(root.join("RAMforge")).unwrap();
    let manifest_before = manifest_text(&root);
    let root_head = rev(&root, "HEAD");
    let remote_before = rev(&bare, "refs/heads/main");

    let refused = plan(&root, add("RAMforge", "RAMforge", &url(&bare)));
    assert!(!refused.is_ready());
    assert!(
        refused
            .blockers
            .iter()
            .any(|b| b.contains("already has history")),
        "{:?}",
        refused.blockers
    );
    assert_eq!(
        refused.recovery,
        vec![PlanRecovery::CloneRemote {
            path: "RAMforge".into(),
            id: "RAMforge".into(),
            remote: url(&bare),
        }],
        "the way forward is offered"
    );
    assert_eq!(
        refused.recovery[0].command(&root),
        format!(
            "gitmesh configure clone RAMforge --remote {} --id RAMforge -C {}",
            url(&bare),
            url(&root)
        ),
        "with the exact command"
    );

    // Cancel: the refused plan is simply not applied. Nothing has changed.
    assert_eq!(manifest_text(&root), manifest_before, "manifest unchanged");
    assert_eq!(rev(&root, "HEAD"), root_head, "root history unchanged");
    assert!(
        dir_entries(&root.join("RAMforge")).is_empty(),
        "the empty directory is still empty"
    );
    assert_eq!(
        rev(&bare, "refs/heads/main"),
        remote_before,
        "remote unchanged"
    );
}

#[test]
fn applying_the_offered_clone_brings_the_remote_history_in_and_keeps_the_root_separate() {
    let tmp = TempDir::new("clone-apply").unwrap();
    let bare = ramforge_remote(&tmp);
    let remote_head = rev(&bare, "refs/heads/main");
    let root = project(&tmp);
    std::fs::create_dir_all(root.join("RAMforge")).unwrap();
    let root_head = rev(&root, "HEAD");

    let refused = plan(&root, add("RAMforge", "RAMforge", &url(&bare)));
    let recovery = refused.recovery.first().expect("offered").clone();

    // The offer is a new, reviewed request: plan it, then apply the plan.
    let reviewed = plan(&root, intent_of(&recovery));
    assert!(reviewed.is_ready(), "{:?}", reviewed.blockers);
    let result = apply(&reviewed);
    assert!(result.is_success(), "the clone is applied");

    let child = root.join("RAMforge");
    assert_eq!(
        rev(&child, "HEAD"),
        remote_head,
        "the remote's history is the clone's history"
    );
    assert_eq!(
        std::fs::read_to_string(child.join("src/main.rs")).unwrap(),
        "fn main() {}\n",
        "the source is in the RAMforge directory"
    );

    // Separation: the root repository did not take the child's files or history.
    assert_eq!(rev(&root, "HEAD"), root_head, "root history unchanged");
    let root_files = git(&root, &["ls-files"]);
    assert!(!root_files.contains("src/main.rs"), "{root_files}");
    assert!(
        !root.join("src").exists(),
        "no copy of the sources in the root"
    );

    // The manifest lists the child and stays valid.
    let project = load(&root);
    let repo = project.repository("RAMforge").expect("listed");
    assert_eq!(repo.relative_slash(), "RAMforge");
    assert_eq!(repo.remote_url.as_deref(), Some(url(&bare).as_str()));
    assert_eq!(repo.absolute_path, root.join("RAMforge"));
}

#[test]
fn a_non_empty_destination_is_refused_and_its_files_are_kept() {
    let tmp = TempDir::new("clone-nonempty").unwrap();
    let bare = ramforge_remote(&tmp);
    let root = project(&tmp);
    write(
        &root.join("RAMforge/notes.txt"),
        "my notes, not in any repository\n",
    );
    let manifest_before = manifest_text(&root);

    let refused = plan(&root, clone("RAMforge", "RAMforge", &url(&bare)));
    assert!(!refused.is_ready());
    assert!(
        refused.blockers.iter().any(|b| b.contains("not empty")),
        "{:?}",
        refused.blockers
    );
    assert!(
        refused.recovery.is_empty(),
        "nothing is offered that would overwrite files"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("RAMforge/notes.txt")).unwrap(),
        "my notes, not in any repository\n",
        "the file is kept"
    );
    assert_eq!(
        dir_entries(&root.join("RAMforge")),
        vec!["notes.txt".to_string()]
    );
    assert_eq!(manifest_text(&root), manifest_before);
    assert!(!root.join("RAMforge/.git").exists());
}

#[test]
fn an_existing_repository_at_the_destination_is_refused_not_cloned_over() {
    let tmp = TempDir::new("clone-existing").unwrap();
    let bare = ramforge_remote(&tmp);
    let root = project(&tmp);
    let child = root.join("RAMforge");
    write(&child.join("mine.txt"), "mine\n");
    git(&child, &["init", "-q", "-b", "main"]);
    identity(&child);
    git(&child, &["add", "-A"]);
    git(&child, &["commit", "-q", "-m", "my own history"]);
    let child_head = rev(&child, "HEAD");

    let refused = plan(&root, clone("RAMforge", "RAMforge", &url(&bare)));
    assert!(!refused.is_ready());
    assert!(
        refused
            .blockers
            .iter()
            .any(|b| b.contains("already a Git repository")),
        "{:?}",
        refused.blockers
    );
    assert_eq!(rev(&child, "HEAD"), child_head, "its history is kept");
    assert_eq!(
        std::fs::read_to_string(child.join("mine.txt")).unwrap(),
        "mine\n"
    );
}

#[test]
fn a_failed_clone_leaves_the_directory_and_the_manifest_as_they_were() {
    let tmp = TempDir::new("clone-fail").unwrap();
    let bare = ramforge_remote(&tmp);
    let root = project(&tmp);
    std::fs::create_dir_all(root.join("RAMforge")).unwrap();
    let manifest_before = manifest_text(&root);
    let root_head = rev(&root, "HEAD");

    // The clone is planned while the remote exists; the remote then disappears.
    let reviewed = plan(&root, clone("RAMforge", "RAMforge", &url(&bare)));
    assert!(reviewed.is_ready(), "{:?}", reviewed.blockers);
    std::fs::remove_dir_all(&bare).unwrap();

    let result = apply(&reviewed);
    assert!(
        !result.is_success(),
        "a clone that cannot run is reported as a failure"
    );
    assert!(
        !root.join("RAMforge/.git").exists(),
        "no partial repository is left"
    );
    assert!(
        dir_entries(&root.join("RAMforge")).is_empty(),
        "the directory is empty again"
    );
    assert_eq!(
        manifest_text(&root),
        manifest_before,
        "the manifest does not list it"
    );
    assert_eq!(rev(&root, "HEAD"), root_head);
}

#[test]
fn an_unreachable_remote_is_refused_before_anything_is_planned() {
    let tmp = TempDir::new("clone-unreachable").unwrap();
    let root = project(&tmp);
    std::fs::create_dir_all(root.join("RAMforge")).unwrap();
    let missing = tmp.join("missing.git");

    let refused = plan(&root, clone("RAMforge", "RAMforge", &url(&missing)));
    assert!(!refused.is_ready());
    assert!(
        refused
            .blockers
            .iter()
            .any(|b| b.contains("could not be read")),
        "{:?}",
        refused.blockers
    );
    assert!(refused
        .actions
        .iter()
        .all(|a| !a.detail.starts_with("git clone")));
    assert!(dir_entries(&root.join("RAMforge")).is_empty());
}

#[test]
fn a_clone_that_would_give_two_repositories_one_remote_is_not_offered() {
    let tmp = TempDir::new("clone-duplicate").unwrap();
    let bare = ramforge_remote(&tmp);
    let root = project(&tmp);
    // A first repository already uses this remote.
    let first = plan(&root, clone("RAMforge", "RAMforge", &url(&bare)));
    assert!(apply(&first).is_success());

    std::fs::create_dir_all(root.join("RAMforge2")).unwrap();
    let refused = plan(&root, add("RAMforge2", "RAMforge2", &url(&bare)));
    assert!(!refused.is_ready());
    assert!(
        refused.recovery.is_empty(),
        "an offer GitMesh would refuse is never shown: {:?}",
        refused.recovery
    );
    assert!(
        refused
            .blockers
            .iter()
            .any(|b| b.contains("same remote URL")),
        "the real reason is given: {:?}",
        refused.blockers
    );
}

#[test]
fn unrelated_history_offers_a_clone_beside_the_repository_and_leaves_it_alone() {
    let tmp = TempDir::new("clone-unrelated").unwrap();
    let bare = ramforge_remote(&tmp);
    let remote_head = rev(&bare, "refs/heads/main");
    let root = project(&tmp);
    let mine = root.join("mine");
    write(&mine.join("app.rs"), "fn app() {}\n");
    git(&mine, &["init", "-q", "-b", "main"]);
    identity(&mine);
    git(&mine, &["add", "-A"]);
    git(&mine, &["commit", "-q", "-m", "my own history"]);
    let mine_head = rev(&mine, "HEAD");

    let refused = plan(&root, add("mine", "mine", &url(&bare)));
    assert!(!refused.is_ready());
    assert!(
        refused
            .blockers
            .iter()
            .any(|b| b.contains("shares no commit")),
        "{:?}",
        refused.blockers
    );
    let recovery = refused
        .recovery
        .first()
        .expect("a clone beside it is offered")
        .clone();
    assert!(matches!(&recovery, PlanRecovery::CloneRemote { path, .. } if path == "mine-remote"));

    let reviewed = plan(&root, intent_of(&recovery));
    assert!(reviewed.is_ready(), "{:?}", reviewed.blockers);
    assert!(apply(&reviewed).is_success());

    assert_eq!(rev(&root.join("mine-remote"), "HEAD"), remote_head);
    assert_eq!(
        rev(&mine, "HEAD"),
        mine_head,
        "the existing repository is untouched"
    );
    assert_eq!(
        std::fs::read_to_string(mine.join("app.rs")).unwrap(),
        "fn app() {}\n"
    );
}

#[test]
fn the_plan_the_gui_receives_carries_the_same_recovery_and_command() {
    let tmp = TempDir::new("clone-view").unwrap();
    let bare = ramforge_remote(&tmp);
    let root = project(&tmp);
    std::fs::create_dir_all(root.join("RAMforge")).unwrap();
    let refused = plan(&root, add("RAMforge", "RAMforge", &url(&bare)));

    let view = gitmesh::service::management_plan_view_json(&refused).to_pretty_string();
    assert!(view.contains("\"recovery\""), "{view}");
    assert!(view.contains("\"kind\": \"clone-remote\""), "{view}");
    assert!(view.contains("\"path\": \"RAMforge\""), "{view}");
    assert!(view.contains("configure clone RAMforge"), "{view}");
}

#[test]
fn the_command_line_names_the_exact_clone_and_running_it_works() {
    let bin = env!("CARGO_BIN_EXE_gitmesh");
    let tmp = TempDir::new("clone-cli").unwrap();
    let bare = ramforge_remote(&tmp);
    let remote_head = rev(&bare, "refs/heads/main");
    let root = project(&tmp);
    std::fs::create_dir_all(root.join("RAMforge")).unwrap();
    let manifest_before = manifest_text(&root);

    let refused = Command::new(bin)
        .args([
            "configure",
            "add",
            "RAMforge",
            "--id",
            "RAMforge",
            "--git-init",
            "--remote",
        ])
        .arg(&bare)
        .arg("-C")
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(refused.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&refused.stderr).to_string();
    let line = stderr
        .lines()
        .find(|l| l.contains("instead, run:"))
        .unwrap_or_else(|| panic!("the command is printed: {stderr}"));
    let command = line
        .split("instead, run: ")
        .nth(1)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(
        command,
        format!(
            "gitmesh configure clone RAMforge --remote {} --id RAMforge -C {}",
            url(&bare),
            url(&root)
        )
    );
    assert_eq!(
        manifest_text(&root),
        manifest_before,
        "refusing changed nothing"
    );

    // Run exactly the printed command (the program name is ours, not the one on PATH).
    let args: Vec<&str> = command.split_whitespace().skip(1).collect();
    let ran = Command::new(bin).args(&args).output().unwrap();
    assert_eq!(
        ran.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    assert_eq!(rev(&root.join("RAMforge"), "HEAD"), remote_head);
    let listed = load(&root);
    assert!(listed.repository("RAMforge").is_some());
}
