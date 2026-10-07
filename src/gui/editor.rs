//! Editor model: everything the GUI renders, derived from the core in one place.
//!
//! The GUI never inspects the filesystem, the manifest or Git itself. It renders this
//! model, which is built exclusively from the service layer ([`crate::service`]) and
//! the discovery scan. That keeps the promise that the GUI is a *front end*: if the
//! model says a file belongs to the `engine` repository, that came from the analyzer,
//! not from a second implementation of ownership.
//!
//! The model is intentionally a plain JSON tree, mirroring how the CLI can emit
//! `--json` for scripting: one shape, one place to test it.

use crate::discovery::{self, ScanOptions};
use crate::json::Json;
use crate::model::GitMeshProject;
use crate::paths::to_slash;
use crate::service::{
    self, owned_change_state, project_state_kind, repository_state_kind, ProjectSession,
    RepositoryStateKind,
};

/// Build the complete model for an open project.
pub fn project_model(session: &ProjectSession, dry_run: bool) -> Json {
    let analyzer = session.analyzer();
    let status = analyzer.analyze();
    let changes = analyzer.owned_changes(&status);
    let pending = service::pending_work(session, &status);
    let project = service::project_view_json(session, &status);
    let branches = service::branches_view_json(session);
    let scan_notice = tree_notice(&status);
    let tree =
        match discovery::scan_project(session.root(), &ScanOptions::default(), session.runner()) {
            Ok(scan) => {
                let mut node = tree_json(session.project(), &scan.tree);
                if let Some(notice) = scan_notice {
                    node = node
                        .with_field("notice", Json::from(notice))
                        .with_field("truncated", Json::from(scan.tree.truncated));
                }
                node
            }
            Err(err) => Json::object([
                ("name", Json::from(session.name().to_string())),
                ("path", Json::from(".")),
                ("error", Json::from(err.to_string())),
                ("children", Json::array(Vec::new())),
            ]),
        };

    Json::object([
        ("kind", Json::from("project")),
        ("opened", Json::from(true)),
        ("error", Json::Null),
        ("dryRun", Json::from(dry_run)),
        ("generatedAt", Json::from(now_millis() as i64)),
        ("project", project),
        (
            "repositories",
            Json::array(
                status
                    .repositories
                    .iter()
                    .map(service::repository_view_json),
            ),
        ),
        (
            "changes",
            Json::array(changes.iter().map(service::change_view_json)),
        ),
        (
            "pending",
            Json::array(pending.iter().map(|work| {
                Json::object([
                    ("id", Json::from(work.id.clone())),
                    ("path", Json::from(work.path.clone())),
                    ("files", Json::from(work.files)),
                    ("conflicts", Json::from(work.conflicts)),
                    ("blocked", Json::from(work.blocked)),
                    ("state", Json::from(work.state.key())),
                    ("summary", Json::from(work.summary())),
                ])
            })),
        ),
        (
            "conflicts",
            Json::array(
                changes
                    .iter()
                    .filter(|change| change.is_conflict())
                    .map(service::change_view_json),
            ),
        ),
        ("tree", tree),
        ("branches", branches),
        ("readiness", readiness_json(session, &pending, &status)),
        (
            "staging",
            Json::object([("note", Json::from(staging_note(&changes)))]),
        ),
    ])
}

/// Model shown when no project is open: what the user selected, and why it failed.
pub fn empty_model(start: &std::path::Path, error: Option<&str>, configuration: bool) -> Json {
    Json::object([
        ("kind", Json::from("no-project")),
        ("opened", Json::from(false)),
        ("directory", Json::from(to_slash(start))),
        ("error", Json::opt(error.map(Json::from))),
        ("configuration", Json::from(configuration)),
        ("tree", Json::Null),
    ])
}

/// What the commit/push/branch actions would do right now.
fn readiness_json(
    session: &ProjectSession,
    pending: &[service::PendingWork],
    status: &crate::analyzer::ProjectStatus,
) -> Json {
    let commit_targets: Vec<&service::PendingWork> =
        pending.iter().filter(|work| !work.blocked).collect();
    let blocked: Vec<&service::PendingWork> = pending.iter().filter(|work| work.blocked).collect();
    let committable_files: usize = commit_targets.iter().map(|work| work.files).sum();

    let commit_reason = if pending.is_empty() {
        Some("every repository is clean".to_string())
    } else if commit_targets.is_empty() {
        Some(format!(
            "{} repository(ies) must be resolved before they can be committed",
            blocked.len()
        ))
    } else {
        None
    };

    let ahead: Vec<&str> = status
        .repositories
        .iter()
        .filter(|state| state.is_ahead())
        .map(|state| state.id.as_str())
        .collect();
    let push_reason = if ahead.is_empty() {
        Some("no repository has unpublished commits".to_string())
    } else {
        None
    };

    Json::object([
        (
            "commit",
            Json::object([
                ("enabled", Json::from(!commit_targets.is_empty())),
                ("reason", Json::opt(commit_reason.map(Json::from))),
                ("files", Json::from(committable_files)),
                (
                    "repositories",
                    Json::array(commit_targets.iter().map(|work| {
                        Json::object([
                            ("id", Json::from(work.id.clone())),
                            ("path", Json::from(work.path.clone())),
                            ("files", Json::from(work.files)),
                        ])
                    })),
                ),
                (
                    "blocked",
                    Json::array(blocked.iter().map(|work| {
                        Json::object([
                            ("id", Json::from(work.id.clone())),
                            ("path", Json::from(work.path.clone())),
                            ("conflicts", Json::from(work.conflicts)),
                            ("state", Json::from(work.state.key())),
                        ])
                    })),
                ),
            ]),
        ),
        (
            "push",
            Json::object([
                ("enabled", Json::from(true)),
                ("reason", Json::opt(push_reason.map(Json::from))),
                ("aheadIn", Json::array(ahead.into_iter().map(Json::from))),
            ]),
        ),
        (
            "branch",
            Json::object([("enabled", Json::from(true)), ("reason", Json::Null)]),
        ),
        (
            "session",
            Json::object([
                ("manifest", Json::from(to_slash(&session.manifest_path()))),
                (
                    "repositories",
                    Json::from(session.project().repositories.len()),
                ),
            ]),
        ),
    ])
}

/// Human sentence about how staging works, shown in the commit view so the user is
/// never surprised that pressing "Commit" also stages.
fn staging_note(changes: &[crate::analyzer::OwnedChange]) -> String {
    let staged = changes
        .iter()
        .filter(|change| owned_change_state(change) == service::ChangeState::Staged)
        .count();
    let conflicts = changes.iter().filter(|change| change.is_conflict()).count();
    if changes.is_empty() {
        "There is nothing to commit in this project.".to_string()
    } else if conflicts > 0 {
        format!(
            "{conflicts} conflicted file(s) must be resolved with Git before they can be \
             committed; the other changes are committed normally."
        )
    } else if staged > 0 {
        "GitMesh commits every change of the project, staged or not, with one message.".to_string()
    } else {
        "GitMesh stages the changes it commits automatically: one message, one real commit per \
         affected repository."
            .to_string()
    }
}

/// The project tree as the GUI shows it: one hierarchy, with the owning repository
/// marked, so `engine/foo.rs` is simply a file of the project.
fn tree_json(project: &GitMeshProject, node: &discovery::DirectoryNode) -> Json {
    let relative = node.relative_path.as_path();
    let repository = if relative == std::path::Path::new(".") {
        Some(project.root_repository().id.clone())
    } else {
        project
            .repository_for_relative(relative)
            .map(|repo| repo.id.clone())
    };
    let is_external = project
        .repository_for_relative(relative)
        .map(|repo| !repo.is_root() && repo.relative_path == relative)
        .unwrap_or(false);
    Json::object([
        (
            "name",
            Json::from(if relative == std::path::Path::new(".") {
                project.name.clone()
            } else {
                node.name.clone()
            }),
        ),
        ("path", Json::from(to_slash(relative))),
        ("repository", Json::opt(repository.map(Json::from))),
        ("isRepositoryRoot", Json::from(node.is_repository_root)),
        ("isExternalRepository", Json::from(is_external)),
        ("files", Json::from(node.file_count)),
        ("truncated", Json::from(node.truncated)),
        (
            "children",
            Json::array(node.children.iter().map(|child| tree_json(project, child))),
        ),
    ])
}

/// A sentence about scan limitations, so the tree never silently implies completeness.
fn tree_notice(status: &crate::analyzer::ProjectStatus) -> Option<String> {
    let unavailable = status.unavailable().count();
    if unavailable > 0 {
        return Some(format!(
            "{unavailable} repository(ies) could not be inspected; their directories are shown \
             from the filesystem only."
        ));
    }
    None
}

/// Milliseconds since the Unix epoch (display/debug only).
fn now_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// A short human sentence describing repository health, used in the GUI header.
pub fn state_sentence(status: &crate::analyzer::ProjectStatus) -> String {
    let kind = project_state_kind(status);
    let counts = (
        status.changed().count(),
        status.conflicted().count(),
        status.unavailable().count(),
    );
    match kind {
        service::ProjectStateKind::Clean => "Everything is committed.".to_string(),
        service::ProjectStateKind::Changed => {
            format!("{} repository(ies) have uncommitted changes.", counts.0)
        }
        service::ProjectStateKind::Conflicted => format!(
            "{} repository(ies) have unresolved conflicts; {} have other changes.",
            counts.1, counts.0
        ),
        service::ProjectStateKind::Unavailable => {
            format!("{} repository(ies) are unavailable.", counts.2)
        }
    }
}

/// Health of a repository as the GUI badge text.
pub fn state_badge(state: &crate::model::RepositoryState) -> &'static str {
    match repository_state_kind(state) {
        RepositoryStateKind::Clean => "clean",
        RepositoryStateKind::Changed => "modified",
        RepositoryStateKind::Conflicted => "conflicted",
        RepositoryStateKind::Unavailable => "unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::RepoFixture;

    fn model(fixture: &RepoFixture, dry_run: bool) -> Json {
        let session = ProjectSession::open(fixture.path()).expect("session");
        project_model(&session, dry_run)
    }

    #[test]
    fn model_describes_one_project_with_its_repositories() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let model = model(&fixture, false).to_string();
        assert!(model.contains("\"name\":\"demo\""));
        assert!(model.contains("\"kind\":\"project\""));
        assert!(model.contains("\"repositories\":["));
        assert!(model.contains("\"repository\":\"engine\""));
        assert!(model.contains("\"dryRun\":false"));
    }

    #[test]
    fn tree_marks_external_repositories_but_stays_one_tree() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        fixture.write("engine/lib.rs", "y");
        let session = ProjectSession::open(fixture.path()).expect("session");
        let model = project_model(&session, false).to_string();
        // The root node is the project itself, and the engine directory is a child of
        // it — not a separate top-level project.
        assert!(model.contains("\"name\":\"demo\",\"path\":\".\",\"repository\":\"root\""));
        assert!(model.contains("\"name\":\"engine\",\"path\":\"engine\""));
        assert!(model.contains("\"isExternalRepository\":true"));
        assert!(model.contains("\"children\":["));
    }

    #[test]
    fn readiness_lists_commit_targets_and_blocked_repositories() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.write("src/main.rs", "x");
        let session = ProjectSession::open(fixture.path()).expect("session");
        let json = project_model(&session, false).to_string();
        assert!(json.contains("\"commit\":{\"enabled\":true"));
        assert!(json.contains("\"files\":1"));
        assert!(json.contains("\"blocked\":[]"));
    }

    #[test]
    fn readiness_explains_when_there_is_nothing_to_commit() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let session = ProjectSession::open(fixture.path()).expect("session");
        let json = project_model(&session, false).to_string();
        assert!(json.contains("every repository is clean"));
        assert!(json.contains("\"commit\":{\"enabled\":false"));
    }

    #[test]
    fn conflicts_block_commit_in_that_repository_only() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.create_conflict("engine", "lib.rs");
        fixture.write("src/main.rs", "root change");
        let session = ProjectSession::open(fixture.path()).expect("session");
        let json = project_model(&session, false).to_string();
        assert!(json.contains("\"blocked\":[{\"id\":\"engine\""));
        assert!(json.contains("\"repositories\":[{\"id\":\"root\""));
        assert!(json.contains("\"state\":\"conflicted\""));
    }

    #[test]
    fn empty_model_explains_a_missing_project() {
        let fixture = RepoFixture::named("demo");
        let model = empty_model(
            fixture.path(),
            Some("no GitMesh project found at or above /x"),
            true,
        )
        .to_string();
        assert!(model.contains("\"kind\":\"no-project\""));
        assert!(model.contains("\"opened\":false"));
        assert!(model.contains("\"configuration\":true"));
    }

    #[test]
    fn branches_are_merged_across_repositories() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        fixture.git_ok(".", &["branch", "feature/one"]);
        let session = ProjectSession::open(fixture.path()).expect("session");
        let json = project_model(&session, false).to_string();
        assert!(json.contains("\"name\":\"feature/one\""));
        // Only the root repository has it: GitMesh says so instead of pretending.
        assert!(json.contains("\"presentIn\":[\"root\"]"));
        assert!(json.contains("\"everywhere\":false"));
        assert!(json.contains("\"name\":\"main\""));
    }

    #[test]
    fn state_sentences_and_badges_match_the_model() {
        let fixture = RepoFixture::named("demo");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);
        let session = ProjectSession::open(fixture.path()).expect("session");
        let status = session.status();
        assert_eq!(state_sentence(&status), "Everything is committed.");
        assert_eq!(state_badge(&status.repositories[0]), "clean");
        fixture.write("engine/lib.rs", "y");
        let status = session.status();
        assert_eq!(
            state_sentence(&status),
            "1 repository(ies) have uncommitted changes."
        );
        assert_eq!(state_badge(&status.repositories[1]), "modified");
    }
}
