//! Remote-first lifecycle: GitMesh must never create a history that is unrelated to the
//! remote's, and must never push across one.
//!
//! The recurring failure this file guards against: a project is set up against a remote that
//! already has commits, GitMesh makes its own first commit, and the push is rejected with
//! "unrelated histories". The tests reproduce each situation with real Git repositories and
//! local bare remotes (no network, no account), and check the outcome in Git itself: refs,
//! ancestry, working-tree files, and exit status. Every failure test also checks that no local
//! file was lost, no commit discarded, no remote history rewritten, and nothing force-pushed.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use gitmesh::git::GitRunner;
use gitmesh::manage::{self, RepositoryIntent, RepositoryManagementRequest, RepositoryObserver};
use gitmesh::manifest;
use gitmesh::model::GitMeshProject;
use gitmesh::ops::{commit_project, pull_project, push_project, CommitOptions, OutcomeKind};
use gitmesh::ops::{OperationReport, PullStrategy, PushOptions, SyncOptions};
use gitmesh::setup::{self, SetupObserver, SetupRequest, SetupResult};
use gitmesh::testkit::TempDir;

// ------------------------------------------------------------------ helpers --

fn git_raw(dir: &Path, args: &[&str]) -> Output {
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

/// Run git and return its trimmed stdout, failing the test when git fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_raw(dir, args);
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Commit identity for a repository, so GitMesh's own commits work without a global config.
fn identity(dir: &Path) {
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.com"]);
}

fn write(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, content).expect("write");
}

/// The commit a ref names, or `None` when the ref does not exist.
fn rev(dir: &Path, name: &str) -> Option<String> {
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

fn head(dir: &Path) -> Option<String> {
    rev(dir, "HEAD")
}

fn commit_count(dir: &Path, name: &str) -> usize {
    git(dir, &["rev-list", "--count", name])
        .parse()
        .unwrap_or(0)
}

fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    git_raw(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
        .status
        .success()
}

/// A bare remote whose `branch` has one commit per `(file, content)` pair.
fn seeded_remote(tmp: &TempDir, name: &str, branch: &str, files: &[(&str, &str)]) -> PathBuf {
    let bare = tmp.join(name);
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", branch, bare.to_str().unwrap()],
    );
    let work = tmp.join(&format!("{name}-seed"));
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            bare.to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    identity(&work);
    for (file, content) in files {
        write(&work.join(file), content);
    }
    git(&work, &["add", "-A"]);
    git(&work, &["commit", "-q", "-m", &format!("seed {name}")]);
    git(&work, &["push", "-q", "origin", &format!("HEAD:{branch}")]);
    bare
}

/// An empty bare remote: no branch, no commit.
fn empty_remote(tmp: &TempDir, name: &str) -> PathBuf {
    let bare = tmp.join(name);
    git(
        tmp.path(),
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    bare
}

/// A new repository with one commit of its own, on `main`, with no remote.
fn own_history(dir: &Path, file: &str, message: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    git(dir, &["init", "-q", "-b", "main"]);
    identity(dir);
    write(&dir.join(file), message);
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", message]);
}

fn url(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn runner() -> GitRunner {
    GitRunner::detect().expect("git is installed")
}

/// A setup request for a project whose root is the directory, with `origin` pointing at
/// `remote` (when given).
fn root_request(root: &Path, remote: Option<&str>, create: bool) -> SetupRequest {
    SetupRequest {
        root: root.to_path_buf(),
        name: "demo".to_string(),
        root_remote: remote.map(str::to_string),
        create_root_repository: create,
        set_git_remote: true,
        ..SetupRequest::default()
    }
}

fn setup_plan(req: &SetupRequest) -> setup::SetupPlan {
    setup::plan(req, &runner()).expect("the plan can be made")
}

fn setup_apply(plan: &setup::SetupPlan) -> SetupResult {
    let mut observer = SetupObserver::silent();
    setup::apply(plan, false, &runner(), &mut observer)
}

/// Plan and apply a setup, asserting that the plan is ready.
fn setup_ready(req: &SetupRequest) -> SetupResult {
    let plan = setup_plan(req);
    assert!(plan.is_ready(), "unexpected blockers: {:?}", plan.blockers);
    let result = setup_apply(&plan);
    let problems: Vec<String> = result
        .outcomes
        .iter()
        .map(|o| {
            format!(
                "{:?}/{:?}: {} {:?}",
                o.kind, o.outcome, o.summary, o.details
            )
        })
        .collect();
    assert!(
        result.is_success(),
        "setup failed: kind={:?} steps={} refused={:?} {problems:?}",
        result.kind,
        plan.steps.len(),
        result.refused
    );
    result
}

fn project(root: &Path) -> GitMeshProject {
    manifest::load_from_root(root).expect("the manifest loads")
}

fn commit_all(root: &Path, message: &str) -> OperationReport {
    let project = project(root);
    commit_project(&project, &runner(), &CommitOptions::new(message)).expect("commit runs")
}

fn push(root: &Path) -> OperationReport {
    let project = project(root);
    let options = PushOptions {
        set_upstream: true,
        ..PushOptions::default()
    };
    push_project(&project, &runner(), &options).expect("push runs")
}

/// The text of the root repository's outcome.
fn root_text(report: &OperationReport) -> String {
    report
        .outcomes
        .iter()
        .flat_map(|o| std::iter::once(o.summary.clone()).chain(o.details.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn root_kind(report: &OperationReport) -> OutcomeKind {
    report
        .outcomes
        .iter()
        .find(|o| o.id == "root")
        .map(|o| o.kind)
        .expect("the root repository is reported")
}

/// Checks that a refused push left the local and remote histories exactly as they were.
fn assert_push_changed_nothing(
    local: &Path,
    bare: &Path,
    branch: &str,
    local_before: Option<String>,
    remote_before: Option<String>,
) {
    assert_eq!(head(local), local_before, "local HEAD moved");
    assert_eq!(
        rev(bare, &format!("refs/heads/{branch}")),
        remote_before,
        "remote {branch} moved"
    );
}

// ------------------------------------------------------------------- tests --

#[test]
fn an_empty_remote_receives_the_first_commit_and_no_history_is_invented() {
    let tmp = TempDir::new("rh-empty").unwrap();
    let bare = empty_remote(&tmp, "empty.git");
    let root = tmp.join("project");
    write(&root.join("main.rs"), "fn main() {}\n");

    let plan = setup_plan(&root_request(&root, Some(&url(&bare)), true));
    assert!(plan.is_ready(), "{:?}", plan.blockers);
    assert!(
        plan.steps
            .iter()
            .all(|s| s.kind != setup::SetupStepKind::AdoptRemoteHistory),
        "an empty remote has no history to adopt"
    );
    assert!(setup_apply(&plan).is_success());
    assert_eq!(head(&root), None, "nothing is committed by setup itself");

    let commit = commit_all(&root, "First commit");
    assert!(commit.is_success(), "{}", root_text(&commit));
    let pushed = push(&root);
    assert!(pushed.is_success(), "{}", root_text(&pushed));

    assert_eq!(
        rev(&bare, "refs/heads/main"),
        head(&root),
        "the remote receives our branch"
    );
    assert_eq!(
        commit_count(&root, "HEAD"),
        1,
        "one commit, nothing invented"
    );
    assert!(root.join("main.rs").exists());
}

#[test]
fn a_remote_with_an_initial_commit_is_adopted_before_the_first_commit() {
    // The recurring bug, end to end: the remote has a README, the project has files and no
    // history. The first commit must build on the remote, so the push is a fast-forward.
    let tmp = TempDir::new("rh-adopt").unwrap();
    let bare = seeded_remote(&tmp, "engine.git", "main", &[("README.md", "# Engine\n")]);
    let remote_head = rev(&bare, "refs/heads/main");
    let root = tmp.join("project");
    write(&root.join("main.rs"), "fn main() {}\n");

    let result = setup_ready(&root_request(&root, Some(&url(&bare)), true));
    assert!(
        result
            .outcomes
            .iter()
            .any(|o| o.kind == setup::SetupStepKind::AdoptRemoteHistory
                && o.outcome == OutcomeKind::Success),
        "the history is adopted: {:?}",
        result
            .outcomes
            .iter()
            .map(|o| o.summary.clone())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        head(&root),
        remote_head,
        "the local branch is the remote's history"
    );
    assert!(
        root.join("README.md").exists(),
        "the remote's files are checked out"
    );
    assert!(
        root.join("main.rs").exists(),
        "the local file is kept, untracked"
    );

    let commit = commit_all(&root, "Add the program");
    assert!(commit.is_success(), "{}", root_text(&commit));
    let pushed = push(&root);
    assert!(pushed.is_success(), "{}", root_text(&pushed));
    assert!(
        !root_text(&pushed).contains("unrelated"),
        "no unrelated history was created: {}",
        root_text(&pushed)
    );

    let local = head(&root).unwrap();
    assert_eq!(
        rev(&bare, "refs/heads/main").as_deref(),
        Some(local.as_str())
    );
    assert!(
        is_ancestor(&root, remote_head.as_deref().unwrap(), &local),
        "the remote's commit is an ancestor"
    );
    assert_eq!(
        commit_count(&root, "HEAD"),
        2,
        "the seed commit and ours, nothing else"
    );
}

#[test]
fn unrelated_local_history_blocks_setup_and_changes_nothing() {
    let tmp = TempDir::new("rh-unrelated-setup").unwrap();
    let bare = seeded_remote(&tmp, "other.git", "main", &[("README.md", "theirs\n")]);
    let remote_before = rev(&bare, "refs/heads/main");
    let root = tmp.join("project");
    own_history(&root, "mine.rs", "my own history");
    let local_before = head(&root);

    let plan = setup_plan(&root_request(&root, Some(&url(&bare)), false));
    assert!(!plan.is_ready(), "the plan must refuse");
    assert!(
        plan.blockers.iter().any(|b| b.contains("shares no commit")),
        "{:?}",
        plan.blockers
    );
    let result = setup_apply(&plan);
    assert!(!result.is_success(), "nothing is applied");

    assert_eq!(head(&root), local_before, "local history is untouched");
    assert!(root.join("mine.rs").exists(), "local files are kept");
    assert_eq!(
        rev(&bare, "refs/heads/main"),
        remote_before,
        "remote history is untouched"
    );
    assert!(git(&root, &["remote"]).is_empty(), "no origin was added");
    assert!(!root.join(".gitmesh").exists(), "no manifest was written");
}

#[test]
fn local_history_that_descends_from_the_remote_is_kept_and_pushes_fast_forward() {
    let tmp = TempDir::new("rh-descends").unwrap();
    let bare = seeded_remote(&tmp, "shared.git", "main", &[("README.md", "shared\n")]);
    let root = tmp.join("project");
    git(
        tmp.path(),
        &["clone", "-q", &url(&bare), root.to_str().unwrap()],
    );
    identity(&root);
    let before = head(&root);

    let plan = setup_plan(&root_request(&root, Some(&url(&bare)), false));
    assert!(plan.is_ready(), "{:?}", plan.blockers);
    assert!(
        plan.steps
            .iter()
            .all(|s| s.kind != setup::SetupStepKind::AdoptRemoteHistory),
        "existing history is never re-adopted"
    );
    assert!(setup_apply(&plan).is_success());
    assert_eq!(head(&root), before, "the history is kept as it is");

    write(&root.join("notes.txt"), "more\n");
    assert!(commit_all(&root, "Add notes").is_success());
    let pushed = push(&root);
    assert!(pushed.is_success(), "{}", root_text(&pushed));
    assert_eq!(rev(&bare, "refs/heads/main"), head(&root));
    assert!(is_ancestor(
        &root,
        before.as_deref().unwrap(),
        &head(&root).unwrap()
    ));
}

#[test]
fn a_remote_advanced_after_the_clone_refuses_the_push_and_changes_nothing() {
    let tmp = TempDir::new("rh-advanced").unwrap();
    let bare = seeded_remote(&tmp, "moving.git", "main", &[("README.md", "v1\n")]);
    let root = tmp.join("project");
    git(
        tmp.path(),
        &["clone", "-q", &url(&bare), root.to_str().unwrap()],
    );
    identity(&root);
    setup_ready(&root_request(&root, None, false));

    // Someone else pushes after our clone.
    let other = tmp.join("other");
    git(
        tmp.path(),
        &["clone", "-q", &url(&bare), other.to_str().unwrap()],
    );
    identity(&other);
    write(&other.join("README.md"), "v2 by someone else\n");
    git(&other, &["commit", "-q", "-am", "their change"]);
    git(&other, &["push", "-q", "origin", "main"]);
    let remote_before = rev(&bare, "refs/heads/main");

    // We commit locally, then try to push.
    write(&root.join("local.txt"), "ours\n");
    assert!(commit_all(&root, "Our change").is_success());
    let local_before = head(&root);
    let pushed = push(&root);
    assert_eq!(
        root_kind(&pushed),
        OutcomeKind::Failed,
        "{}",
        root_text(&pushed)
    );
    let text = root_text(&pushed);
    assert!(
        text.contains("commit(s) this repository does not have") || text.contains("diverged"),
        "{text}"
    );
    assert!(text.contains("pull"), "the way out is named: {text}");
    assert_push_changed_nothing(
        &root,
        &bare,
        "main",
        local_before.clone(),
        remote_before.clone(),
    );

    // The explicit way out: pull, then push.
    // Without a strategy the pull refuses (ff-only), and says so.
    let refused_pull = pull_project(&project(&root), &runner(), &SyncOptions::new()).unwrap();
    assert_eq!(
        root_kind(&refused_pull),
        OutcomeKind::Failed,
        "{}",
        root_text(&refused_pull)
    );
    assert!(
        root_text(&refused_pull).contains("--strategy merge"),
        "{}",
        root_text(&refused_pull)
    );
    // The explicit merge, chosen by the user.
    let options = SyncOptions {
        strategy: PullStrategy::Merge,
        ..SyncOptions::new()
    };
    let pulled = pull_project(&project(&root), &runner(), &options).unwrap();
    assert!(
        pulled
            .outcomes
            .iter()
            .all(|o| o.kind != OutcomeKind::Failed),
        "{}",
        root_text(&pulled)
    );
    let pushed_again = push(&root);
    assert!(pushed_again.is_success(), "{}", root_text(&pushed_again));
    assert_eq!(rev(&bare, "refs/heads/main"), head(&root));
}

#[test]
fn a_remote_whose_default_branch_is_not_main_is_adopted_under_its_own_name() {
    let tmp = TempDir::new("rh-master").unwrap();
    let bare = seeded_remote(&tmp, "legacy.git", "master", &[("README.md", "legacy\n")]);
    let remote_master = rev(&bare, "refs/heads/master");
    let root = tmp.join("project");
    write(&root.join("tool.sh"), "echo hi\n");

    setup_ready(&root_request(&root, Some(&url(&bare)), true));
    assert_eq!(git(&root, &["symbolic-ref", "--short", "HEAD"]), "master");
    assert_eq!(head(&root), remote_master);

    assert!(commit_all(&root, "Add tool").is_success());
    let pushed = push(&root);
    assert!(pushed.is_success(), "{}", root_text(&pushed));
    assert_eq!(
        rev(&bare, "refs/heads/master"),
        head(&root),
        "master advances"
    );
    assert!(
        rev(&bare, "refs/heads/main").is_none(),
        "no 'main' branch is invented on a remote that uses master"
    );
}

#[test]
fn an_unreachable_remote_is_a_warning_at_setup_and_a_refusal_at_push() {
    let tmp = TempDir::new("rh-unreachable").unwrap();
    let missing = tmp.join("does-not-exist.git");
    let root = tmp.join("project");
    write(&root.join("main.rs"), "fn main() {}\n");

    let plan = setup_plan(&root_request(&root, Some(&url(&missing)), true));
    assert!(
        plan.is_ready(),
        "an unreachable remote does not stop the setup: {:?}",
        plan.blockers
    );
    assert!(
        plan.warnings
            .iter()
            .any(|w| w.contains("could not be read")),
        "{:?}",
        plan.warnings
    );
    assert!(setup_apply(&plan).is_success());
    assert!(commit_all(&root, "First commit").is_success());
    let local_before = head(&root);

    let pushed = push(&root);
    assert_eq!(root_kind(&pushed), OutcomeKind::Failed);
    assert!(
        root_text(&pushed).contains("cannot read 'origin'"),
        "{}",
        root_text(&pushed)
    );
    assert_eq!(head(&root), local_before, "nothing local changed");
}

#[test]
fn a_conflicting_local_file_stops_the_adoption_and_is_kept() {
    let tmp = TempDir::new("rh-conflict").unwrap();
    let bare = seeded_remote(
        &tmp,
        "conflict.git",
        "main",
        &[("README.md", "from remote\n")],
    );
    let remote_before = rev(&bare, "refs/heads/main");
    let root = tmp.join("project");
    write(&root.join("README.md"), "my own readme, not committed\n");

    let plan = setup_plan(&root_request(&root, Some(&url(&bare)), true));
    assert!(
        plan.is_ready(),
        "the plan cannot know the file conflicts: {:?}",
        plan.blockers
    );
    let result = setup_apply(&plan);
    let adoption = result
        .outcomes
        .iter()
        .find(|o| o.kind == setup::SetupStepKind::AdoptRemoteHistory)
        .expect("the adoption step ran");
    assert_eq!(
        adoption.outcome,
        OutcomeKind::Failed,
        "{}",
        adoption.summary
    );
    assert!(
        adoption.summary.contains("Nothing was changed")
            || adoption.summary.contains("local files"),
        "{}",
        adoption.summary
    );

    assert_eq!(
        std::fs::read_to_string(root.join("README.md")).unwrap(),
        "my own readme, not committed\n",
        "the local file is not overwritten"
    );
    assert_eq!(head(&root), None, "no history was created or adopted");
    assert_eq!(
        rev(&bare, "refs/heads/main"),
        remote_before,
        "the remote is untouched"
    );
}

#[test]
fn repeating_the_setup_is_idempotent() {
    let tmp = TempDir::new("rh-idempotent").unwrap();
    let bare = seeded_remote(&tmp, "again.git", "main", &[("README.md", "once\n")]);
    let root = tmp.join("project");
    write(&root.join("main.rs"), "fn main() {}\n");
    let req = root_request(&root, Some(&url(&bare)), true);

    setup_ready(&req);
    let after_first = head(&root);
    let remote_after_first = rev(&bare, "refs/heads/main");

    // A second run plans nothing that creates history or a remote.
    let second = setup_plan(&req);
    assert!(second.is_ready(), "{:?}", second.blockers);
    assert!(
        second
            .steps
            .iter()
            .all(|s| s.kind != setup::SetupStepKind::AdoptRemoteHistory),
        "nothing left to adopt"
    );
    assert!(
        second
            .steps
            .iter()
            .filter(|s| s.kind == setup::SetupStepKind::ConfigureRemote)
            .all(|s| !s.planned()),
        "origin is not added twice"
    );
    assert!(setup_apply(&second).is_success());

    assert_eq!(head(&root), after_first, "no commit was added");
    assert_eq!(
        rev(&bare, "refs/heads/main"),
        remote_after_first,
        "the remote is unchanged"
    );
    assert_eq!(
        git(&root, &["remote"]).lines().count(),
        1,
        "exactly one origin"
    );
    assert_eq!(commit_count(&root, "HEAD"), 1, "no new unrelated history");
}

#[test]
fn repeating_a_remote_configuration_does_not_duplicate_or_rewrite_anything() {
    let tmp = TempDir::new("rh-repeat-remote").unwrap();
    // An empty remote: configuring it is allowed, and a second configuration is a no-op.
    let bare = empty_remote(&tmp, "repeat.git");
    let root = tmp.join("project");
    own_history(&root, "app.rs", "app");
    setup_ready(&root_request(&root, None, false));
    let intent = || RepositoryIntent::SetRemote {
        id: "root".into(),
        remote: Some(url(&bare)),
        configure: true,
    };

    let first = manage::plan(
        &project(&root),
        &RepositoryManagementRequest::one(intent()),
        &runner(),
    )
    .unwrap();
    assert!(first.is_ready(), "{:?}", first.blockers);
    let mut observer = RepositoryObserver::silent();
    assert!(manage::apply(&first, false, &runner(), &mut observer).is_success());
    let head_after_first = head(&root);
    assert_eq!(
        git(&root, &["remote"]).lines().count(),
        1,
        "exactly one origin after the first run"
    );

    let second = manage::plan(
        &project(&root),
        &RepositoryManagementRequest::one(intent()),
        &runner(),
    )
    .unwrap();
    assert!(second.is_ready(), "{:?}", second.blockers);
    assert!(
        second.is_noop(),
        "the second configuration changes nothing: {}",
        second.summary()
    );
    let mut observer = RepositoryObserver::silent();
    manage::apply(&second, false, &runner(), &mut observer);

    assert_eq!(
        git(&root, &["remote"]).lines().count(),
        1,
        "still exactly one origin"
    );
    assert_eq!(head(&root), head_after_first, "no commit was made");
    assert_eq!(git(&root, &["remote", "get-url", "origin"]), url(&bare));
}

#[test]
fn a_push_refuses_unrelated_histories_and_sets_no_upstream() {
    let tmp = TempDir::new("rh-push-unrelated").unwrap();
    let bare = seeded_remote(&tmp, "push-other.git", "main", &[("README.md", "theirs\n")]);
    let remote_before = rev(&bare, "refs/heads/main");
    let root = tmp.join("project");
    own_history(&root, "mine.rs", "mine");
    let local_before = head(&root);
    git(&root, &["remote", "add", "origin", &url(&bare)]);
    setup_ready(&root_request(&root, None, false));

    let pushed = push(&root);
    assert_eq!(
        root_kind(&pushed),
        OutcomeKind::Failed,
        "{}",
        root_text(&pushed)
    );
    let text = root_text(&pushed);
    assert!(
        text.contains("unrelated histories (no common commit)"),
        "{text}"
    );
    assert!(text.contains("never force-pushes"), "{text}");
    assert_push_changed_nothing(&root, &bare, "main", local_before, remote_before);
    assert!(
        !git_raw(
            &root,
            &[
                "rev-parse",
                "--abbrev-ref",
                "--symbolic-full-name",
                "@{upstream}"
            ]
        )
        .status
        .success(),
        "no upstream was configured by the refused push"
    );
}

#[test]
fn a_branch_the_remote_does_not_have_is_refused_when_it_would_be_a_parallel_history() {
    let tmp = TempDir::new("rh-mismatch").unwrap();
    // The remote's only branch is master; the local branch is main with unrelated history.
    let bare = seeded_remote(&tmp, "mismatch.git", "master", &[("README.md", "master\n")]);
    let remote_master = rev(&bare, "refs/heads/master");
    let root = tmp.join("project");
    own_history(&root, "mine.rs", "main history");
    let local_before = head(&root);
    git(&root, &["remote", "add", "origin", &url(&bare)]);
    setup_ready(&root_request(&root, None, false));

    let pushed = push(&root);
    assert_eq!(
        root_kind(&pushed),
        OutcomeKind::Failed,
        "{}",
        root_text(&pushed)
    );
    let text = root_text(&pushed);
    assert!(text.contains("no commits in common"), "{text}");
    assert!(
        text.contains("branch-name mismatch") || text.contains("git push -u"),
        "{text}"
    );
    assert!(
        rev(&bare, "refs/heads/main").is_none(),
        "no parallel 'main' was created"
    );
    assert_eq!(rev(&bare, "refs/heads/master"), remote_master);
    assert_eq!(head(&root), local_before);
}

#[test]
fn a_new_branch_built_on_the_remote_history_is_pushed() {
    let tmp = TempDir::new("rh-new-branch").unwrap();
    let bare = seeded_remote(&tmp, "feature.git", "main", &[("README.md", "base\n")]);
    let root = tmp.join("project");
    git(
        tmp.path(),
        &["clone", "-q", &url(&bare), root.to_str().unwrap()],
    );
    identity(&root);
    setup_ready(&root_request(&root, None, false));
    git(&root, &["checkout", "-q", "-b", "feature/x"]);
    write(&root.join("feature.txt"), "x\n");
    assert!(commit_all(&root, "Feature work").is_success());

    let pushed = push(&root);
    assert!(pushed.is_success(), "{}", root_text(&pushed));
    assert_eq!(
        rev(&bare, "refs/heads/feature/x"),
        head(&root),
        "the new branch is created"
    );
    assert!(
        rev(&bare, "refs/heads/main").is_some(),
        "the remote's main is still there"
    );
}

#[test]
fn an_empty_repository_is_adopted_by_add_and_an_unrelated_one_is_refused() {
    // The same rules through the repository-management path (configure add).
    let tmp = TempDir::new("rh-manage").unwrap();
    let bare = seeded_remote(&tmp, "manage.git", "main", &[("README.md", "m\n")]);
    let remote_head = rev(&bare, "refs/heads/main");
    let root = tmp.join("project");
    std::fs::create_dir_all(&root).unwrap();
    setup_ready(&root_request(&root, None, true));

    // An empty repository that is not yet a GitMesh repository.
    let engine = root.join("engine");
    std::fs::create_dir_all(&engine).unwrap();
    git(&engine, &["init", "-q", "-b", "main"]);
    identity(&engine);
    let add = |path: &str, id: &str| RepositoryIntent::Add {
        path: path.into(),
        id: id.into(),
        remote: Some(url(&bare)),
        branch: None,
        initialize: false,
        configure_remote: true,
        untrack_from_root: false,
    };
    let plan = manage::plan(
        &project(&root),
        &RepositoryManagementRequest::one(add("engine", "engine")),
        &runner(),
    )
    .unwrap();
    assert!(plan.is_ready(), "{:?}", plan.blockers);
    let mut observer = RepositoryObserver::silent();
    let result = manage::apply(&plan, false, &runner(), &mut observer);
    assert!(result.is_success(), "{:?}", result.refused);
    assert_eq!(
        head(&engine),
        remote_head,
        "the empty repository adopts the remote's history"
    );

    // An existing repository with its own, unrelated history is refused by the same rules.
    let other = root.join("other");
    own_history(&other, "x.rs", "other history");
    let before = head(&other);
    let plan = manage::plan(
        &project(&root),
        &RepositoryManagementRequest::one(add("other", "other")),
        &runner(),
    )
    .unwrap();
    assert!(!plan.is_ready(), "the unrelated repository must be refused");
    assert!(
        plan.blockers.iter().any(|b| b.contains("shares no commit")),
        "{:?}",
        plan.blockers
    );
    assert_eq!(head(&other), before, "its history is untouched");
    assert_eq!(
        rev(&bare, "refs/heads/main"),
        remote_head,
        "the remote is untouched"
    );
}

#[test]
fn the_command_line_reproduces_the_fix_and_the_refusal() {
    let bin = env!("CARGO_BIN_EXE_gitmesh");

    // Remote-first: adopt, then commit and push, all from the command line.
    let tmp = TempDir::new("rh-cli").unwrap();
    let bare = seeded_remote(&tmp, "cli.git", "main", &[("README.md", "# CLI\n")]);
    let root = tmp.join("project");
    write(&root.join("main.rs"), "fn main() {}\n");
    let init = Command::new(bin)
        .args(["init", "--git-init", "--add-git-remote", "--remote"])
        .arg(&bare)
        .args(["--name", "demo", "-C"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(
        init.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let stdout = String::from_utf8_lossy(&init.stdout);
    assert!(
        stdout.contains("history:"),
        "the adoption is reported: {stdout}"
    );
    assert!(root.join("README.md").exists());

    let commit = Command::new(bin)
        .args(["commit", "-m", "First commit", "-C"])
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(
        commit.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&commit.stdout)
    );
    let push_out = Command::new(bin)
        .arg("push")
        .arg("-C")
        .arg(&root)
        .output()
        .unwrap();
    assert_eq!(
        push_out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&push_out.stdout)
    );
    assert_eq!(rev(&bare, "refs/heads/main"), head(&root));

    // Unrelated: refused with exit code 2, and the directory is untouched.
    let tmp2 = TempDir::new("rh-cli-refuse").unwrap();
    let other = seeded_remote(&tmp2, "cli-other.git", "main", &[("README.md", "theirs\n")]);
    let mine = tmp2.join("project");
    own_history(&mine, "mine.rs", "mine");
    let before = head(&mine);
    let refused = Command::new(bin)
        .args(["init", "--git-init", "--add-git-remote", "--remote"])
        .arg(&other)
        .args(["--name", "demo", "-C"])
        .arg(&mine)
        .output()
        .unwrap();
    assert_eq!(
        refused.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert!(String::from_utf8_lossy(&refused.stderr).contains("shares no commit"));
    assert_eq!(head(&mine), before);
    assert!(!mine.join(".gitmesh").exists());
}

#[test]
fn no_operation_merges_unrelated_histories_so_there_is_no_merge_to_confirm() {
    // GitMesh offers no way to merge unrelated histories: a pull reports them and changes
    // nothing. Recovery is a deliberate choice made outside GitMesh (see docs/REPOSITORIES.md).
    let tmp = TempDir::new("rh-pull").unwrap();
    let bare = seeded_remote(&tmp, "pull-other.git", "main", &[("README.md", "theirs\n")]);
    let root = tmp.join("project");
    own_history(&root, "mine.rs", "mine");
    git(&root, &["remote", "add", "origin", &url(&bare)]);
    git(&root, &["fetch", "-q", "origin"]);
    git(&root, &["branch", "--set-upstream-to=origin/main"]);
    setup_ready(&root_request(&root, None, false));
    let local_before = head(&root);
    let remote_before = rev(&bare, "refs/heads/main");

    let pulled = pull_project(&project(&root), &runner(), &SyncOptions::new()).unwrap();
    assert!(
        root_text(&pulled).contains("unrelated"),
        "{}",
        root_text(&pulled)
    );
    assert_eq!(head(&root), local_before);
    assert_eq!(rev(&bare, "refs/heads/main"), remote_before);
    assert!(root.join("mine.rs").exists());
}
