//! Helpers shared by the offline repository tests: temporary repositories, local bare remotes
//! and plain git commands. Nothing here touches the network or any credential.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gitmesh::git::GitRunner;
use gitmesh::manage::{
    self, PlanRecovery, RepositoryIntent, RepositoryManagementRequest, RepositoryManagementResult,
    RepositoryObserver, RepositoryPlan,
};
use gitmesh::manifest;
use gitmesh::model::GitMeshProject;
use gitmesh::setup::{self, SetupObserver, SetupRequest};
use gitmesh::testkit::TempDir;

// ------------------------------------------------------------------ helpers --

pub fn git_raw(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git runs")
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_raw(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn identity(dir: &Path) {
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
}

pub fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

pub fn rev(dir: &Path, name: &str) -> Option<String> {
    let out = git_raw(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn url(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

pub fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

/// A bare remote whose `main` has `README.md` and `src/main.rs`.
pub fn ramforge_remote(tmp: &TempDir) -> PathBuf {
    let bare = tmp.join("ramforge.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let seed = tmp.join("ramforge-seed");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            seed.to_str().unwrap(),
        ],
    );
    identity(&seed);
    write(&seed.join("README.md"), "# RAMforge\n");
    write(&seed.join("src/main.rs"), "fn main() {}\n");
    git(&seed, &["add", "-A"]);
    git(&seed, &["commit", "-q", "-m", "RAMforge initial"]);
    git(&seed, &["push", "-q", "origin", "HEAD:main"]);
    bare
}

/// A project whose root is a repository with one commit, and that has a GitMesh manifest.
pub fn project(tmp: &TempDir) -> PathBuf {
    let root = tmp.join("project");
    write(&root.join("README.md"), "# Project\n");
    git(&root, &["init", "-q", "-b", "main"]);
    identity(&root);
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "project initial"]);
    let request = SetupRequest {
        root: root.clone(),
        name: "demo".to_string(),
        create_root_repository: true,
        set_git_remote: true,
        ..SetupRequest::default()
    };
    let plan = setup::plan(&request, &runner()).expect("the plan can be made");
    let mut observer = SetupObserver::silent();
    assert!(setup::apply(&plan, false, &runner(), &mut observer).is_success());
    // The manifest is committed with the root, as a first root commit would do.
    git(&root, &["add", ".gitmesh"]);
    git(&root, &["commit", "-q", "-m", "gitmesh: manifest"]);
    root
}

pub fn load(root: &Path) -> GitMeshProject {
    manifest::load_from_root(root).expect("the manifest loads")
}

pub fn plan(root: &Path, intent: RepositoryIntent) -> RepositoryPlan {
    manage::plan(
        &load(root),
        &RepositoryManagementRequest::one(intent),
        &runner(),
    )
    .expect("the plan can be made")
}

pub fn apply(plan: &RepositoryPlan) -> RepositoryManagementResult {
    let mut observer = RepositoryObserver::silent();
    manage::apply(plan, false, &runner(), &mut observer)
}

pub fn add(path: &str, id: &str, remote: &str) -> RepositoryIntent {
    RepositoryIntent::Add {
        path: path.into(),
        id: id.into(),
        remote: Some(remote.into()),
        branch: None,
        initialize: true,
        configure_remote: true,
        untrack_from_root: false,
    }
}

pub fn clone(path: &str, id: &str, remote: &str) -> RepositoryIntent {
    RepositoryIntent::Clone {
        path: path.into(),
        id: id.into(),
        remote: remote.into(),
        branch: None,
    }
}

/// The intent a user gets when they press "Clone repository" on a refusal.
pub fn intent_of(recovery: &PlanRecovery) -> RepositoryIntent {
    match recovery {
        PlanRecovery::CloneRemote { path, id, remote } => clone(path, id, remote),
    }
}

pub fn manifest_text(root: &Path) -> String {
    std::fs::read_to_string(manifest::manifest_path(root)).expect("manifest exists")
}

pub fn dir_entries(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}
