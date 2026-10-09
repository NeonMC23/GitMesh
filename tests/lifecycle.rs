//! Repository lifecycle: onboarding, cloning, remotes, upstreams and synchronisation errors.
//!
//! Every test uses real Git repositories and local bare remotes, so the suite needs no
//! network, no account and no credentials. Each test states the situation it builds and the
//! outcome the product promises for it.

use std::path::{Path, PathBuf};

use gitmesh::discovery;
use gitmesh::manage::{
    self, RepositoryIntent, RepositoryManagementRequest, RepositoryManagementResult,
    RepositoryObserver, RepositoryPlan,
};
use gitmesh::ops::{pull_project, push_project, OutcomeKind, PushOptions, SyncOptions};
use gitmesh::testkit::RepoFixture;

// ------------------------------------------------------------------ helpers --

fn url(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn plan(fixture: &RepoFixture, intent: RepositoryIntent) -> RepositoryPlan {
    let project = fixture.load_project();
    manage::plan(
        &project,
        &RepositoryManagementRequest::one(intent),
        fixture.runner(),
    )
    .expect("plan")
}

fn apply(fixture: &RepoFixture, plan: &RepositoryPlan) -> RepositoryManagementResult {
    let mut observer = RepositoryObserver::silent();
    manage::apply(plan, false, fixture.runner(), &mut observer)
}

/// Plan and apply one intent, asserting that the plan is ready.
fn run(fixture: &RepoFixture, intent: RepositoryIntent) -> RepositoryManagementResult {
    let plan = plan(fixture, intent);
    assert!(plan.is_ready(), "unexpected blockers: {:?}", plan.blockers);
    apply(fixture, &plan)
}

fn clone_intent(path: &str, id: &str, remote: &str) -> RepositoryIntent {
    RepositoryIntent::Clone {
        path: path.to_string(),
        id: id.to_string(),
        remote: remote.to_string(),
        branch: None,
    }
}

fn add_intent(path: &str, id: &str, remote: Option<&str>, initialize: bool) -> RepositoryIntent {
    RepositoryIntent::Add {
        path: path.to_string(),
        id: id.to_string(),
        remote: remote.map(str::to_string),
        branch: None,
        initialize,
        configure_remote: remote.is_some(),
        untrack_from_root: false,
    }
}

/// A bare remote whose `main` has one commit containing `file`.
fn seeded_remote(fixture: &RepoFixture, name: &str, file: &str) -> PathBuf {
    let bare = fixture.create_bare(name);
    let work = fixture.clone_outside(&bare, &format!("seed-{name}"));
    commit_file(fixture, &work, file, &format!("{file} content"), "seed");
    fixture
        .runner()
        .repo(&work)
        .run_checked(&["push", "-q", "origin", "HEAD:main"])
        .expect("push seed");
    bare
}

fn commit_file(fixture: &RepoFixture, repo: &Path, file: &str, content: &str, message: &str) {
    std::fs::write(repo.join(file), content).expect("write file");
    let git = fixture.runner().repo(repo);
    git.run_checked(&["add", "-A"]).expect("add");
    git.run_checked(&["commit", "-q", "-m", message])
        .expect("commit");
}

fn head(fixture: &RepoFixture, repo: &Path) -> String {
    fixture
        .runner()
        .repo(repo)
        .run_checked(&["rev-parse", "HEAD"])
        .expect("head")
        .trim()
        .to_string()
}

/// The commit a branch of a bare remote points at.
fn remote_branch(fixture: &RepoFixture, bare: &Path, branch: &str) -> Option<String> {
    fixture
        .runner()
        .repo(bare)
        .run_optional(&[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .expect("rev-parse")
}

/// A repository inside the project with one commit and no remote.
fn local_repo(fixture: &RepoFixture, rel: &str) -> PathBuf {
    let path = fixture.init_repo(rel);
    commit_file(fixture, &path, "README.md", "local work\n", "local commit");
    path
}

fn outcome<'a>(
    report: &'a gitmesh::ops::OperationReport,
    id: &str,
) -> &'a gitmesh::ops::RepoOutcome {
    report
        .outcomes
        .iter()
        .find(|o| o.id == id)
        .unwrap_or_else(|| panic!("no outcome for {id}"))
}

fn all_text(outcome: &gitmesh::ops::RepoOutcome) -> String {
    let mut text = outcome.summary.clone();
    for detail in &outcome.details {
        text.push('\n');
        text.push_str(detail);
    }
    text
}

// ------------------------------------------------------------ cloning --

#[test]
fn cloning_a_remote_into_a_new_directory_tracks_its_default_branch_and_records_it() {
    let f = RepoFixture::named("lifecycle-clone-new");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "core.git", "core.txt");
    let remote = url(&bare);

    let result = run(&f, clone_intent("libs/core", "core", &remote));

    assert!(result.is_success(), "{:?}", result.refused);
    let clone = f.path().join("libs/core");
    assert!(
        clone.join("core.txt").exists(),
        "the clone has the remote's files"
    );
    let git = f.runner().repo(&clone);
    assert_eq!(
        git.run_checked(&["remote", "get-url", "origin"])
            .unwrap()
            .trim(),
        remote
    );
    assert_eq!(
        git.upstream().unwrap().as_deref(),
        Some("origin/main"),
        "the default branch tracks the remote"
    );

    // The manifest records the repository and its remote; the validation re-opens everything.
    let project = f.load_project();
    let core = project.repository("core").expect("core is recorded");
    assert_eq!(core.remote_url.as_deref(), Some(remote.as_str()));
    assert!(result.validation.as_ref().is_some_and(|v| v.ok));
}

#[test]
fn cloning_does_not_change_the_ownership_of_the_root_repository() {
    let f = RepoFixture::named("lifecycle-clone-ownership");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "owned.git", "owned.txt");
    let remote = url(&bare);

    run(&f, clone_intent("libs/owned", "owned", &remote));

    // The root repository tracks nothing inside the clone: one file, one owner.
    let root = f.runner().repo(f.path());
    assert_eq!(
        discovery::count_files_tracked_under(f.path(), Path::new("libs/owned"), f.runner()),
        0
    );
    assert!(root
        .tracked_files_under(Path::new("libs/owned"))
        .unwrap()
        .is_empty());
    // The root repository keeps its own history and manifest.
    assert!(f.path().join(".gitmesh/project.toml").exists());
}

#[test]
fn cloning_refuses_a_non_empty_directory_and_leaves_its_files_alone() {
    let f = RepoFixture::named("lifecycle-clone-nonempty");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "busy.git", "busy.txt");
    f.write("libs/busy/notes.txt", "my unsaved work");

    let plan = plan(&f, clone_intent("libs/busy", "busy", &url(&bare)));

    assert!(
        !plan.is_ready(),
        "a directory with files must refuse the clone"
    );
    assert!(
        plan.blockers.iter().any(|b| b.contains("not empty")),
        "{:?}",
        plan.blockers
    );
    assert_eq!(
        std::fs::read_to_string(f.path().join("libs/busy/notes.txt")).unwrap(),
        "my unsaved work",
        "the existing file is untouched"
    );
    assert!(!f.path().join("libs/busy/.git").exists());
}

#[test]
fn cloning_refuses_a_directory_that_already_is_a_repository() {
    let f = RepoFixture::named("lifecycle-clone-existing-repo");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "again.git", "again.txt");
    local_repo(&f, "libs/again");

    let plan = plan(&f, clone_intent("libs/again", "again", &url(&bare)));

    assert!(!plan.is_ready());
    assert!(
        plan.blockers
            .iter()
            .any(|b| b.contains("already a Git repository")),
        "{:?}",
        plan.blockers
    );
    assert!(f.path().join("libs/again/README.md").exists());
}

#[test]
fn an_unreachable_remote_refuses_the_clone_with_the_reason() {
    let f = RepoFixture::named("lifecycle-clone-unreachable");
    f.project_with(&[(".", ".")]);
    let missing = f.outside_path().join("no-such-remote.git");

    let plan = plan(&f, clone_intent("libs/gone", "gone", &url(&missing)));

    assert!(!plan.is_ready());
    let text = plan.blockers.join("\n");
    assert!(text.contains("could not be found"), "{text}");
    assert!(!f.path().join("libs/gone").exists(), "nothing was created");
}

#[test]
fn a_clone_that_fails_at_run_time_is_not_recorded_in_the_manifest() {
    let f = RepoFixture::named("lifecycle-clone-late-failure");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "vanishing.git", "v.txt");
    let plan = plan(&f, clone_intent("libs/vanish", "vanish", &url(&bare)));
    assert!(plan.is_ready(), "the remote answered at review time");

    // The remote disappears between the review and the run.
    std::fs::remove_dir_all(&bare).unwrap();
    let result = apply(&f, &plan);

    assert!(!result.is_success());
    assert!(
        f.load_project().repository("vanish").is_none(),
        "a repository that was not cloned must not be listed in the manifest"
    );
    assert!(!f.path().join("libs/vanish/.git").exists());
}

#[test]
fn cloning_an_empty_remote_produces_a_repository_without_commits() {
    let f = RepoFixture::named("lifecycle-clone-empty-remote");
    f.project_with(&[(".", ".")]);
    let bare = f.create_bare("empty.git");

    let plan = plan(&f, clone_intent("libs/empty", "empty", &url(&bare)));
    assert!(plan.is_ready(), "an empty remote is a valid clone source");
    assert!(
        plan.notices
            .iter()
            .chain(plan.warnings.iter())
            .any(|n| n.contains("empty")),
        "the plan says the remote has no commits: {:?} {:?}",
        plan.notices,
        plan.warnings
    );

    let result = apply(&f, &plan);
    assert!(result.is_success(), "{:?}", result.refused);
    assert!(!f
        .runner()
        .repo(f.path().join("libs/empty"))
        .head_oid()
        .unwrap()
        .is_some());
}

// ------------------------------------------------------- importing and connecting --

#[test]
fn a_missing_directory_is_pointed_at_the_clone_command_instead_of_a_dead_end() {
    let f = RepoFixture::named("lifecycle-missing-add");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "lib.git", "lib.txt");

    let plan = plan(&f, add_intent("libs/lib", "lib", Some(&url(&bare)), true));

    assert!(!plan.is_ready());
    assert!(
        plan.blockers.iter().any(|b| b.contains("configure clone")),
        "the refusal names the clone command: {:?}",
        plan.blockers
    );
}

#[test]
fn connecting_an_existing_local_repository_keeps_its_history() {
    let f = RepoFixture::named("lifecycle-connect");
    f.project_with(&[(".", ".")]);
    let local = local_repo(&f, "libs/app");
    let before = head(&f, &local);
    let bare = f.create_bare("app.git");

    let result = run(&f, add_intent("libs/app", "app", Some(&url(&bare)), false));

    assert!(result.is_success(), "{:?}", result.refused);
    assert_eq!(head(&f, &local), before, "the history is kept as it is");
    assert_eq!(
        f.runner()
            .repo(&local)
            .run_checked(&["remote", "get-url", "origin"])
            .unwrap()
            .trim(),
        url(&bare)
    );
}

#[test]
fn the_first_push_publishes_a_connected_repository_to_an_empty_remote() {
    let f = RepoFixture::named("lifecycle-publish");
    f.project_with(&[(".", ".")]);
    let local = local_repo(&f, "libs/app");
    let bare = f.create_bare("publish.git");
    let before = head(&f, &local);

    let plan = plan(&f, add_intent("libs/app", "app", Some(&url(&bare)), false));
    assert!(plan
        .notices
        .iter()
        .chain(plan.warnings.iter())
        .any(|n| n.contains("empty")));
    assert!(apply(&f, &plan).is_success());

    let project = f.load_project();
    let report = push_project(&project, f.runner(), &PushOptions::default()).unwrap();
    assert_eq!(outcome(&report, "app").kind, OutcomeKind::Success);
    assert_eq!(
        remote_branch(&f, &bare, "main").as_deref(),
        Some(before.as_str())
    );
}

#[test]
fn an_empty_directory_is_not_attached_to_remote_history_by_init() {
    let f = RepoFixture::named("lifecycle-empty-dir-history");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "history.git", "history.txt");
    std::fs::create_dir_all(f.path().join("libs/fresh")).unwrap();

    let plan = plan(
        &f,
        add_intent("libs/fresh", "fresh", Some(&url(&bare)), true),
    );

    assert!(
        !plan.is_ready(),
        "an empty directory must not be turned into a fake local repository"
    );
    assert!(
        plan.blockers.iter().any(|b| b.contains("clone")),
        "{:?}",
        plan.blockers
    );
    assert!(!f.path().join("libs/fresh/.git").exists());
}

#[test]
fn an_unreachable_remote_is_recorded_with_a_warning_not_silently_accepted() {
    let f = RepoFixture::named("lifecycle-unreachable-connect");
    f.project_with(&[(".", ".")]);
    local_repo(&f, "libs/app");
    let missing = f.outside_path().join("typo.git");

    let plan = plan(
        &f,
        add_intent("libs/app", "app", Some(&url(&missing)), false),
    );

    assert!(
        plan.is_ready(),
        "the URL may be right while the network is down"
    );
    assert!(
        plan.warnings.iter().any(|w| w.contains("cannot reach")),
        "{:?}",
        plan.warnings
    );
}

#[test]
fn a_local_only_repository_is_valid_and_skipped_by_synchronisation() {
    let f = RepoFixture::named("lifecycle-local-only");
    f.project_with(&[(".", ".")]);
    local_repo(&f, "libs/solo");

    let result = run(&f, add_intent("libs/solo", "solo", None, false));
    assert!(result.is_success(), "{:?}", result.refused);

    let project = f.load_project();
    let pull = pull_project(&project, f.runner(), &SyncOptions::new()).unwrap();
    let push = push_project(&project, f.runner(), &PushOptions::default()).unwrap();
    for report in [&pull, &push] {
        let solo = outcome(report, "solo");
        assert_eq!(solo.kind, OutcomeKind::Skipped, "{}", all_text(solo));
        assert!(solo.summary.contains("no remote"), "{}", solo.summary);
        assert!(
            report.is_success(),
            "a local-only repository is not a failure"
        );
    }
}

// ------------------------------------------------------ upstream and synchronisation --

#[test]
fn a_repository_with_a_remote_but_no_upstream_is_reported_as_such() {
    let f = RepoFixture::named("lifecycle-no-upstream");
    f.project_with(&[(".", ".")]);
    let bare = f.create_bare("noup.git");
    let local = local_repo(&f, "libs/noup");
    f.runner()
        .repo(&local)
        .run_checked(&["remote", "add", "origin", &url(&bare)])
        .unwrap();
    let project = f.project_with(&[(".", "."), ("noup", "libs/noup")]);

    let report = pull_project(&project, f.runner(), &SyncOptions::new()).unwrap();
    let noup = outcome(&report, "noup");
    assert_eq!(noup.kind, OutcomeKind::Skipped);
    assert_eq!(noup.summary, "no upstream branch configured");
    assert!(all_text(noup).contains("push -u"), "the fix is named");
}

#[test]
fn a_repository_without_commits_is_skipped_without_running_commit_dependent_operations() {
    let f = RepoFixture::named("lifecycle-no-commits");
    let project = f.project_without_commits(&[(".", "."), ("fresh", "fresh")]);
    let bare = seeded_remote(&f, "fresh-remote.git", "x.txt");
    f.runner()
        .repo(f.path().join("fresh"))
        .run_checked(&["remote", "add", "origin", &url(&bare)])
        .unwrap();

    let pull = pull_project(&project, f.runner(), &SyncOptions::new()).unwrap();
    let push = push_project(&project, f.runner(), &PushOptions::default()).unwrap();

    let pulled = outcome(&pull, "fresh");
    assert_eq!(pulled.kind, OutcomeKind::Skipped);
    assert!(pulled.summary.contains("no commits"), "{}", pulled.summary);
    let pushed = outcome(&push, "fresh");
    assert_eq!(pushed.kind, OutcomeKind::Skipped);
    assert!(pushed.summary.contains("no commits"), "{}", pushed.summary);
}

#[test]
fn a_rejected_push_of_divergent_histories_is_explained_and_never_forced() {
    let f = RepoFixture::named("lifecycle-diverged-push");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "diverged.git", "base.txt");
    let clone = f.clone_from(&bare, "libs/div");
    f.project_with(&[(".", "."), ("div", "libs/div")]);
    f.runner()
        .repo(&clone)
        .run_checked(&["branch", "--set-upstream-to=origin/main"])
        .unwrap();
    // Remote moves on, and local moves on too.
    let other = f.clone_outside(&bare, "other-div");
    commit_file(&f, &other, "remote.txt", "remote", "remote change");
    f.runner()
        .repo(&other)
        .run_checked(&["push", "-q", "origin", "main"])
        .unwrap();
    commit_file(&f, &clone, "local.txt", "local", "local change");
    let remote_before = remote_branch(&f, &bare, "main");

    let report = push_project(&f.load_project(), f.runner(), &PushOptions::default()).unwrap();

    let div = outcome(&report, "div");
    assert_eq!(div.kind, OutcomeKind::Failed, "{}", all_text(div));
    assert!(div.summary.contains("diverged"), "{}", div.summary);
    // The message names the integration the CLI really offers: `pull --strategy merge|rebase`.
    assert!(
        all_text(div).contains("--strategy merge") || all_text(div).contains("--strategy rebase"),
        "{}",
        all_text(div)
    );
    assert_eq!(
        remote_branch(&f, &bare, "main"),
        remote_before,
        "the remote history is not overwritten"
    );
}

#[test]
fn unrelated_histories_are_reported_as_unrelated_by_pull_and_push() {
    let f = RepoFixture::named("lifecycle-unrelated");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "other-project.git", "theirs.txt");
    let local = local_repo(&f, "libs/mine");
    let runner = f.runner();
    runner
        .repo(&local)
        .run_checked(&["remote", "add", "origin", &url(&bare)])
        .unwrap();
    runner
        .repo(&local)
        .run_checked(&["fetch", "-q", "origin"])
        .unwrap();
    runner
        .repo(&local)
        .run_checked(&["branch", "--set-upstream-to=origin/main"])
        .unwrap();
    let project = f.project_with(&[(".", "."), ("mine", "libs/mine")]);
    let pull = pull_project(&project, f.runner(), &SyncOptions::new()).unwrap();
    let pulled = all_text(outcome(&pull, "mine"));
    assert!(pulled.contains("unrelated"), "{pulled}");

    let push = push_project(&project, f.runner(), &PushOptions::default()).unwrap();
    let pushed = all_text(outcome(&push, "mine"));
    assert!(pushed.contains("unrelated"), "{pushed}");
    assert!(
        !pushed.contains("pull` first") && !pushed.contains("--merge"),
        "a pull cannot resolve unrelated histories: {pushed}"
    );
}

#[test]
fn a_remote_refusal_by_a_hook_is_not_reported_as_a_history_conflict() {
    let f = RepoFixture::named("lifecycle-hook");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "guarded.git", "g.txt");
    let clone = f.clone_from(&bare, "libs/guarded");
    f.project_with(&[(".", "."), ("guarded", "libs/guarded")]);
    f.runner()
        .repo(&clone)
        .run_checked(&["branch", "--set-upstream-to=origin/main"])
        .unwrap();
    // The policy goes in after the clone, so only later pushes meet it.
    std::fs::write(
        bare.join("hooks/pre-receive"),
        "#!/bin/sh\necho 'policy: direct pushes to main are disabled' >&2\nexit 1\n",
    )
    .unwrap();
    make_executable(&bare.join("hooks/pre-receive"));
    commit_file(&f, &clone, "change.txt", "change", "a change");

    let report = push_project(&f.load_project(), f.runner(), &PushOptions::default()).unwrap();

    let guarded = outcome(&report, "guarded");
    assert_eq!(guarded.kind, OutcomeKind::Failed);
    assert!(
        guarded.summary.contains("rejected by the remote"),
        "{}",
        guarded.summary
    );
    assert!(
        all_text(guarded).contains("policy: direct pushes to main are disabled"),
        "the remote's own reason is shown: {}",
        all_text(guarded)
    );
    assert!(!guarded
        .summary
        .contains("commits this repository does not have"));
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).unwrap();
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

#[test]
fn a_branch_deleted_on_the_remote_is_reported_and_never_called_up_to_date() {
    let f = RepoFixture::named("lifecycle-deleted-upstream");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "deleted.git", "d.txt");
    let clone = f.clone_from(&bare, "libs/deleted");
    f.project_with(&[(".", "."), ("deleted", "libs/deleted")]);
    let runner = f.runner();
    runner
        .repo(&clone)
        .run_checked(&["checkout", "-q", "-b", "feature"])
        .unwrap();
    commit_file(&f, &clone, "feature.txt", "f", "feature");
    runner
        .repo(&clone)
        .run_checked(&["push", "-q", "-u", "origin", "feature"])
        .unwrap();
    // Someone deletes the branch on the remote.
    runner
        .repo(&bare)
        .run_checked(&["update-ref", "-d", "refs/heads/feature"])
        .unwrap();

    let report = pull_project(&f.load_project(), f.runner(), &SyncOptions::new()).unwrap();
    let deleted = outcome(&report, "deleted");
    assert_eq!(deleted.kind, OutcomeKind::Failed, "{}", all_text(deleted));
    assert!(
        deleted.summary.contains("no longer exists"),
        "{}",
        deleted.summary
    );
    assert!(!report.is_success());
}

#[test]
fn one_failing_repository_does_not_stop_the_others() {
    let f = RepoFixture::named("lifecycle-partial");
    f.project_with(&[(".", ".")]);
    let good_remote = seeded_remote(&f, "good.git", "good.txt");
    f.clone_from(&good_remote, "libs/good");
    let broken_remote = seeded_remote(&f, "broken.git", "broken.txt");
    f.clone_from(&broken_remote, "libs/broken");
    f.project_with(&[(".", "."), ("good", "libs/good"), ("broken", "libs/broken")]);
    let runner = f.runner();
    // The good repository's remote gets a new commit; the broken one's disappears.
    let writer = f.clone_outside(&good_remote, "writer-good");
    commit_file(&f, &writer, "new.txt", "new", "new commit");
    runner
        .repo(&writer)
        .run_checked(&["push", "-q", "origin", "main"])
        .unwrap();
    std::fs::remove_dir_all(&broken_remote).unwrap();

    let report = pull_project(&f.load_project(), f.runner(), &SyncOptions::new()).unwrap();

    assert_eq!(
        outcome(&report, "good").kind,
        OutcomeKind::Success,
        "{}",
        all_text(outcome(&report, "good"))
    );
    assert!(
        f.path().join("libs/good/new.txt").exists(),
        "the good repository was pulled"
    );
    assert_eq!(outcome(&report, "broken").kind, OutcomeKind::Failed);
    let (ok, _skipped, _conflict, failed) = report.counts();
    assert_eq!((ok, failed), (1, 1));
    assert!(
        !report.is_success(),
        "a failure is never reported as success"
    );
}

#[test]
fn a_fast_forward_pull_still_updates_a_repository_that_tracks_its_remote() {
    let f = RepoFixture::named("lifecycle-ff-pull");
    f.project_with(&[(".", ".")]);
    let bare = seeded_remote(&f, "ff.git", "ff.txt");
    let clone = f.clone_from(&bare, "libs/ff");
    f.project_with(&[(".", "."), ("ff", "libs/ff")]);
    let writer = f.clone_outside(&bare, "writer-ff");
    commit_file(&f, &writer, "later.txt", "later", "later");
    f.runner()
        .repo(&writer)
        .run_checked(&["push", "-q", "origin", "main"])
        .unwrap();

    let report = pull_project(&f.load_project(), f.runner(), &SyncOptions::new()).unwrap();

    assert_eq!(outcome(&report, "ff").kind, OutcomeKind::Success);
    assert!(clone.join("later.txt").exists());
}
