
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{RepoFixture, TempDir};
    use crate::ops::OutcomeKind;

    /// A plain directory (no `.git`, no manifest) with a Git runner — the starting point
    /// the wizard is designed for.
    fn plain(label: &str) -> (TempDir, GitRunner) {
        let dir = TempDir::new(label).expect("temporary directory");
        let runner = GitRunner::detect().expect("git");
        (dir, runner)
    }

    fn request(root: &Path) -> SetupRequest {
        SetupRequest {
            root: root.to_path_buf(),
            create_root_repository: true,
            set_git_remote: true,
            overwrite_manifest: false,
            overwrite_remotes: false,
            untrack_from_root: true,
            ..SetupRequest::default()
        }
    }

    fn repo(path: &str) -> RepositoryRequest {
        RepositoryRequest {
            path: path.to_string(),
            create: true,
            ..RepositoryRequest::default()
        }
    }

    fn candidate<'a>(inspection: &'a Inspection, path: &str) -> &'a CandidateDirectory {
        fn find<'a>(nodes: &'a [CandidateDirectory], path: &str) -> Option<&'a CandidateDirectory> {
            for node in nodes {
                if node.relative_path == Path::new(path) {
                    return Some(node);
                }
                if let Some(found) = find(&node.children, path) {
                    return Some(found);
                }
            }
            None
        }
        find(&inspection.candidates, path).unwrap_or_else(|| panic!("no candidate at {path}"))
    }

    fn planned(gui: &SetupPlan, kind: SetupStepKind, target: &str) -> bool {
        gui.steps
            .iter()
            .any(|step| step.kind == kind && step.target == target && step.planned())
    }

    // ------------------------------------------------------------- scanning --

    #[test]
    fn inspection_of_an_empty_directory_reports_facts_only() {
        let (dir, runner) = plain("setup-scan-empty");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("include")).unwrap();

        let inspection = inspect(dir.path(), &runner).unwrap();
        assert!(inspection.exists);
        assert!(!inspection.is_gitmesh_project);
        assert!(!inspection.manifest_path.exists());
        assert!(!inspection.root_is_repository);
        assert_eq!(inspection.repositories.len(), 0);
        assert_eq!(candidate(&inspection, "src").path_label(), "src");
        assert!(!candidate(&inspection, "src").suggested());
        assert_eq!(
            inspection.suggested_name,
            dir.path().file_name().unwrap().to_string_lossy()
        );
        // Nothing was created by inspecting.
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitmesh").exists());
    }

    #[test]
    fn inspection_marks_existing_repositories_as_candidates() {
        let fixture = RepoFixture::named("setup-scan-repos");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        fixture.mkdir("tools/src");

        let inspection = inspect(fixture.path(), &fixture.runner()).unwrap();
        assert!(inspection.root_is_repository);
        let engine = candidate(&inspection, "engine");
        assert!(engine.is_repository && engine.suggested() && engine.has_commits);
        assert_eq!(engine.branch.as_deref(), Some("main"));
        assert_eq!(engine.files + 1, candidate(&inspection, "engine").subtree_files + 0);
        let tools = candidate(&inspection, "tools");
        assert!(!tools.is_repository && !tools.suggested());
        assert_eq!(tools.depth, 1);
    }

    #[test]
    fn inspection_counts_files_the_root_repository_tracks_inside_a_candidate() {
        let fixture = RepoFixture::named("setup-scan-tracked");
        fixture.init_repo("engine");
        fixture.write("engine/tracked.txt", "owned by the root repository\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks engine");

        let inspection = inspect(fixture.path(), &fixture.runner()).unwrap();
        let engine = candidate(&inspection, "engine");
        assert_eq!(engine.tracked_by_root, 1, "{engine:?}");
    }

    #[test]
    fn inspection_finds_nested_repositories_and_existing_projects() {
        let fixture = RepoFixture::named("setup-scan-nested");
        fixture.init_repo("engine/vendor/dep");
        fixture.commit("engine/vendor/dep", "vendored");

        let inspection = inspect(fixture.path(), &fixture.runner()).unwrap();
        let engine = candidate(&inspection, "engine");
        assert!(engine.is_repository);
        let vendor = candidate(&inspection, "engine/vendor");
        assert_eq!(vendor.nested_repositories, vec![PathBuf::from("engine/vendor/dep")]);
        assert!(
            inspection
                .notices
                .iter()
                .any(|notice| notice.contains("nested inside")),
            "{:?}",
            inspection.notices
        );

        // A configured project is recognised, with its manifest text.
        fixture.project_with(&[("root", ".")]);
        let inspection = inspect(fixture.path(), &fixture.runner()).unwrap();
        assert!(inspection.is_gitmesh_project);
        assert!(inspection.manifest_text.is_some());
        assert!(inspection.manifest_error.is_none());
    }

    #[test]
    fn inspection_reports_a_malformed_manifest_and_a_missing_directory() {
        let fixture = RepoFixture::named("setup-scan-broken");
        std::fs::create_dir_all(fixture.path().join(".gitmesh")).unwrap();
        std::fs::write(
            fixture.path().join(".gitmesh/project.toml"),
            "version = 1\nname = \"demo\"\n\n[repositories]\n",
        )
        .unwrap();

        let inspection = inspect(fixture.path(), &fixture.runner()).unwrap();
        assert!(inspection.is_gitmesh_project);
        assert!(inspection.manifest_error.is_some());
        assert!(
            inspection
                .notices
                .iter()
                .any(|notice| notice.contains("could not be read")),
            "{:?}",
            inspection.notices
        );

        let missing = inspect(&fixture.path().join("nope"), &fixture.runner()).unwrap();
        assert!(!missing.exists);
        assert!(missing.candidates.is_empty());
    }

    // ------------------------------------------------------------ planning --

    #[test]
    fn plan_for_a_root_only_project_generates_the_manifest() {
        let (dir, runner) = plain("setup-plan-root");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();

        let plan = plan(&request(dir.path()), &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert_eq!(plan.name, dir.path().file_name().unwrap().to_string_lossy());
        assert!(planned(&plan, SetupStepKind::CreateMetadataDir, "manifest"));
        assert!(planned(&plan, SetupStepKind::CreateRepository, "root"));
        assert!(planned(&plan, SetupStepKind::WriteManifest, "manifest"));
        assert_eq!(plan.repositories.len(), 1);
        assert!(plan.manifest.contains("version = 1"));
        assert!(plan.manifest.contains("[root]"));
        assert!(!plan.manifest.contains("[[repositories]]"));
        assert!(plan.safety.iter().any(|line| line.contains("no existing .git")));
        // Planning did not touch the directory.
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitmesh").exists());
    }

    #[test]
    fn plan_uses_the_requested_and_suggested_names() {
        let (dir, runner) = plain("setup-plan-names");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        std::fs::create_dir_all(dir.join("My Engine")).unwrap();
        std::fs::create_dir_all(dir.join("a/tools")).unwrap();
        std::fs::create_dir_all(dir.join("b/tools")).unwrap();

        let mut req = request(dir.path());
        req.name = " MyProject ".to_string();
        req.repositories = vec![
            repo("engine"),
            repo("My Engine"),
            repo("a/tools"),
            repo("b/tools"),
        ];
        let plan = plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert_eq!(plan.name, "MyProject");
        let ids: Vec<&str> = plan.repositories.iter().map(|repo| repo.id.as_str()).collect();
        assert!(ids.contains(&"engine"));
        assert!(ids.contains(&"my-engine"), "{ids:?}");
        assert!(ids.contains(&"tools"));
        assert!(ids.contains(&"tools-2"), "{ids:?}");
        assert!(plan.manifest.contains("id = \"my-engine\""));
        assert!(plan.manifest.contains("id = \"tools-2\""));

        // A custom id wins, a duplicate is refused.
        let mut req = request(dir.path());
        req.repositories = vec![
            RepositoryRequest {
                path: "engine".into(),
                id: "backend".into(),
                create: true,
                ..RepositoryRequest::default()
            },
            RepositoryRequest {
                path: "a/tools".into(),
                id: "backend".into(),
                create: true,
                ..RepositoryRequest::default()
            },
        ];
        let plan = plan(&req, &runner).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("already used")),
            "{:?}",
            plan.blockers
        );
    }

    #[test]
    fn plan_never_reinitialises_an_existing_repository_and_reports_it() {
        let fixture = RepoFixture::named("setup-plan-existing");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        let head_before = fixture.git_ok("engine", &["rev-parse", "HEAD"]);

        let mut req = request(fixture.path());
        req.create_root_repository = true;
        req.repositories = vec![repo("engine")];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(!planned(&plan, SetupStepKind::CreateRepository, "engine"));
        assert!(!planned(&plan, SetupStepKind::CreateRepository, "root"));
        assert_eq!(plan.created_repositories().count(), 0);
        let step = plan
            .steps
            .iter()
            .find(|step| step.kind == SetupStepKind::CreateRepository && step.target == "engine")
            .unwrap();
        assert!(
            matches!(&step.state, StepState::AlreadySatisfied(reason) if reason.contains("never re-initialised")),
            "{step:?}"
        );
        assert!(
            plan.notices
                .iter()
                .any(|notice| notice.contains("already a Git repository")),
            "{:?}",
            plan.notices
        );
        assert_eq!(fixture.git_ok("engine", &["rev-parse", "HEAD"]), head_before);
    }

    #[test]
    fn plan_rejects_overlapping_nested_and_invalid_selections() {
        let fixture = RepoFixture::named("setup-plan-overlap");
        fixture.mkdir("engine/core");
        fixture.write("notes.txt", "notes\n");

        let mut req = request(fixture.path());
        req.repositories = vec![repo("engine"), repo("engine/core")];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("contains the configured repository")),
            "{:?}",
            plan.blockers
        );
        assert!(plan.blocked_steps().count() > 0);

        // The project root cannot become an external repository.
        let mut req = request(fixture.path());
        req.repositories = vec![repo(".")];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("always the root repository")),
            "{:?}",
            plan.blockers
        );

        // A file and a missing directory are refused with a reason each.
        let mut req = request(fixture.path());
        req.repositories = vec![repo("notes.txt"), repo("does-not-exist")];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("is not a directory")),
            "{:?}",
            plan.blockers
        );
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("does not exist")),
            "{:?}",
            plan.blockers
        );

        // A path outside the project is refused too.
        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "../escape".into(),
            create: true,
            ..RepositoryRequest::default()
        }];
        let err = plan(&req, &fixture.runner()).unwrap_err();
        assert!(err.to_string().contains("invalid repository path"), "{err}");
    }

    #[test]
    fn plan_requires_confirmation_before_replacing_a_remote() {
        let fixture = RepoFixture::named("setup-plan-remote");
        let bare = fixture.create_bare("remotes/engine.git");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.commit("engine", "engine start");
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", bare.to_string_lossy().as_ref()],
        );
        fixture.git_ok("engine", &["push", "-q", "-u", "origin", "main"]);

        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("git@github.com:acme/engine.git".into()),
            ..RepositoryRequest::default()
        }];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("must be confirmed explicitly")),
            "{:?}",
            plan.blockers
        );
        assert!(plan.blocked_steps().any(|step| step.kind == SetupStepKind::ConfigureRemote));

        // With the confirmation, the step is planned and the safety lines say so.
        let mut confirmed = req.clone();
        confirmed.overwrite_remotes = true;
        let plan = plan(&confirmed, &fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(planned(&plan, SetupStepKind::ConfigureRemote, "engine"));
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("the origin of engine is replaced")),
            "{:?}",
            plan.safety
        );

        // A request that leaves the remote alone keeps it, recorded in the manifest.
        let mut keep = request(fixture.path());
        keep.repositories = vec![repo("engine")];
        let plan = plan(&keep, &fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let engine = plan.repositories.iter().find(|r| r.id == "engine").unwrap();
        assert_eq!(engine.remote_action, RemoteAction::Keep);
        assert_eq!(engine.remote.as_deref(), Some(bare.to_string_lossy().as_ref()));
        assert!(!planned(&plan, SetupStepKind::ConfigureRemote, "engine"));
        assert!(plan.manifest.contains("remotes/engine.git"));
    }

    #[test]
    fn plan_requires_confirmation_before_replacing_the_manifest() {
        let fixture = RepoFixture::named("setup-plan-manifest");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);

        let mut req = request(fixture.path());
        req.name = "renamed-project".to_string();
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("confirmed explicitly")),
            "{:?}",
            plan.blockers
        );

        req.overwrite_manifest = true;
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(planned(&plan, SetupStepKind::WriteManifest, "manifest"));
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("the existing manifest is replaced")),
            "{:?}",
            plan.safety
        );
    }

    #[test]
    fn plan_is_idempotent_for_an_already_matching_configuration() {
        let fixture = RepoFixture::named("setup-plan-idempotent");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);

        let mut req = request(fixture.path());
        req.repositories = vec![repo("engine")];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(plan.is_noop(), "{:?}", plan.planned_steps().collect::<Vec<_>>());
        assert_eq!(plan.summary(), "nothing to do: the project is already set up as requested");
        assert!(
            plan.notices
                .iter()
                .any(|notice| notice.contains("manifest on disk is already up to date")),
            "{:?}",
            plan.notices
        );
    }

    #[test]
    fn plan_describes_local_only_and_hosted_repositories() {
        let (dir, runner) = plain("setup-plan-remotes");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        std::fs::create_dir_all(dir.join("tools")).unwrap();

        let mut req = request(dir.path());
        req.repositories = vec![
            RepositoryRequest {
                path: "engine".into(),
                create: true,
                remote: Some("git@github.com:acme/myproject-engine.git".into()),
                visibility: Some("private".into()),
                ..RepositoryRequest::default()
            },
            repo("tools"),
        ];
        let plan = plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        let engine = plan.repositories.iter().find(|r| r.id == "engine").unwrap();
        assert_eq!(engine.provider.as_deref(), Some("github"));
        let hosted = engine.hosted.as_ref().unwrap();
        assert_eq!(hosted.full_name(), "acme/myproject-engine");
        assert_eq!(hosted.web_url(), "https://github.com/acme/myproject-engine");
        assert_eq!(engine.visibility.as_deref(), Some("private"));
        assert_eq!(engine.remote_action, RemoteAction::Add);
        assert!(planned(&plan, SetupStepKind::ConfigureRemote, "engine"));

        let tools = plan.repositories.iter().find(|r| r.id == "tools").unwrap();
        assert_eq!(tools.remote_action, RemoteAction::None);
        assert!(tools.remote.is_none());
        // The manifest records the URL and nothing else: no provider, no visibility.
        assert!(plan.manifest.contains("git@github.com:acme/myproject-engine.git"));
        assert!(!plan.manifest.contains("visibility"));
        assert!(!plan.manifest.contains("provider"));
        // The repository rows explain themselves.
        assert!(engine.sentence().contains("acme"), "{}", engine.sentence());
        assert!(tools.sentence().contains("tools"), "{}", tools.sentence());
    }

    #[test]
    fn plan_offers_to_untrack_root_files_and_warns_when_it_does_not() {
        let fixture = RepoFixture::named("setup-plan-untrack");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks engine/lib.rs");

        let mut req = request(fixture.path());
        req.repositories = vec![repo("engine")];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        let engine = plan.repositories.iter().find(|r| r.id == "engine").unwrap();
        assert_eq!(engine.tracked_by_root, 1);
        assert!(!engine.untrack);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("still tracks 1 file(s)")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.safety
                .iter()
                .any(|line| line.contains("no file is moved")),
            "{:?}",
            plan.safety
        );

        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            untrack_from_root: true,
            ..RepositoryRequest::default()
        }];
        let plan = plan(&req, &fixture.runner()).unwrap();
        assert!(planned(&plan, SetupStepKind::UntrackFromRoot, "engine"));
        assert!(plan
            .safety
            .iter()
            .any(|line| line.contains("stay on disk")));
    }

    #[test]
    fn plan_warns_about_nested_repositories_and_about_not_creating_a_repository() {
        let (dir, runner) = plain("setup-plan-warnings");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        // A nested repository inside the directory that is becoming a repository.
        let nested = dir.join("engine/vendor/dep");
        std::fs::create_dir_all(&nested).unwrap();
        runner
            .repo(&nested)
            .run_checked(&["init", "-q", "-b", "main"])
            .unwrap();
        // A directory that will not get a Git repository.
        std::fs::create_dir_all(dir.join("tools")).unwrap();

        let mut req = request(dir.path());
        req.repositories = vec![
            repo("engine"),
            RepositoryRequest {
                path: "tools".into(),
                create: false,
                ..RepositoryRequest::default()
            },
        ];
        let plan = plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("contains another Git repository")),
            "{:?}",
            plan.warnings
        );
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("not a Git repository yet")),
            "{:?}",
            plan.warnings
        );
        // The manifest still lists it: GitMesh reports such a repository as unavailable
        // instead of hiding it.
        assert!(plan.manifest.contains("tools"));
    }

    #[test]
    fn plan_reports_a_branch_hint_and_a_vague_request_is_cleaned_up() {
        let (dir, runner) = plain("setup-plan-branch");
        std::fs::create_dir_all(dir.join("engine")).unwrap();

        let mut req = request(dir.path());
        req.name = "   ".to_string();
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some("   ".into()),
            ..RepositoryRequest::default()
        }];
        let plan = plan(&req, &runner).unwrap();
        assert_eq!(plan.name, dir.path().file_name().unwrap().to_string_lossy());
        assert!(!plan.is_ready());
        assert!(
            plan.blockers
                .iter()
                .any(|blocker| blocker.contains("remote URL is empty")),
            "{:?}",
            plan.blockers
        );

        let mut req = request(dir.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            branch: Some(" develop ".into()),
            ..RepositoryRequest::default()
        }];
        let plan = plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);
        assert!(plan.manifest.contains("branch = \"develop\""));
    }

    // ----------------------------------------------------------- execution --

    #[test]
    fn apply_creates_repositories_the_manifest_and_validates_the_project() {
        let (dir, runner) = plain("setup-apply");
        for name in ["src", "include", "engine", "renderer", "tools"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("engine/lib.rs"), "pub fn go() {}\n").unwrap();
        let bare = TempDir::new("setup-apply-remote").unwrap();
        let remote = bare.join("myproject-engine.git");
        runner
            .run(&[
                "init".into(),
                "--bare".into(),
                "-q".into(),
                remote.as_os_str().to_os_string(),
            ])
            .unwrap();

        let mut req = request(dir.path());
        req.name = "MyProject".to_string();
        req.repositories = vec![
            RepositoryRequest {
                path: "engine".into(),
                create: true,
                remote: Some(remote.to_string_lossy().to_string()),
                ..RepositoryRequest::default()
            },
            repo("renderer"),
            repo("tools"),
        ];
        let plan = plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert_eq!(result.exit_code(), 0);
        assert!(result.failures().next().is_none());
        assert!(result.succeeded() >= 6, "{}", result.summary());

        // Root and external repositories exist as real Git repositories.
        assert!(dir.join(".git").is_dir());
        for name in ["engine", "renderer", "tools"] {
            assert!(dir.join(name).join(".git").is_dir(), "{name} has its own .git");
        }
        // The manifest exists, parses, and describes the architecture.
        let manifest = dir.join(".gitmesh/project.toml");
        assert!(manifest.is_file());
        let text = std::fs::read_to_string(&manifest).unwrap();
        assert_eq!(text, plan.manifest);
        let project = manifest::load_from_root(dir.path()).unwrap();
        assert_eq!(project.name, "MyProject");
        assert_eq!(project.len(), 4);
        assert_eq!(
            project.repository("engine").unwrap().remote_url.as_deref(),
            Some(remote.to_string_lossy().as_ref())
        );
        // The remote was configured in Git as well.
        assert_eq!(
            discovery::origin_url(&dir.join("engine"), &runner).unwrap(),
            Some(remote.to_string_lossy().to_string())
        );
        assert_eq!(discovery::origin_url(&dir.join("renderer"), &runner).unwrap(), None);

        // Final validation runs through the normal opening path.
        let validation = result.validation.as_ref().expect("validation");
        assert!(validation.ok, "{:?}", validation.issues);
        assert_eq!(validation.repositories.len(), 4);
        assert!(validation.repositories.iter().all(|check| check.is_ok()));
        assert_eq!(validation.project_name.as_deref(), Some("MyProject"));
        assert!(validation
            .repositories
            .iter()
            .any(|check| check.id == "engine" && check.remote_ok));

        // The user's files are all still there.
        assert!(dir.join("src/main.rs").is_file());
        assert!(dir.join("engine/lib.rs").is_file());
    }

    #[test]
    fn apply_untracks_root_files_without_deleting_them() {
        let fixture = RepoFixture::named("setup-apply-untrack");
        fixture.init_repo("engine");
        fixture.write("engine/lib.rs", "pub fn go() {}\n");
        fixture.add_all(".");
        fixture.commit(".", "root tracks engine/lib.rs");

        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            untrack_from_root: true,
            ..RepositoryRequest::default()
        }];
        let plan = plan(&req, &fixture.runner()).unwrap();
        let result = apply(&plan, false, &fixture.runner(), &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert!(fixture.path().join("engine/lib.rs").is_file());
        assert!(
            fixture
                .runner()
                .repo(fixture.path())
                .tracked_files_under(Path::new("engine"))
                .unwrap()
                .is_empty(),
            "the root repository no longer tracks the file"
        );
        // The external repository keeps its own file, and the root's index change is
        // staged, not committed: nothing is discarded silently.
        assert!(fixture
            .runner()
            .repo(fixture.path().join("engine"))
            .status()
            .unwrap()
            .has_changes());
    }

    #[test]
    fn apply_reports_a_partial_failure_and_keeps_the_successful_work() {
        let (dir, runner) = plain("setup-apply-partial");
        for name in ["engine", "tools"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine"), repo("tools")];
        let plan = plan(&req, &runner).unwrap();
        assert!(plan.is_ready(), "{:?}", plan.blockers);

        // The directory disappears and is replaced by a file between the review and the
        // confirmation: the step fails at execution time, exactly like a permission error.
        std::fs::remove_dir_all(dir.join("tools")).unwrap();
        std::fs::write(dir.join("tools"), "not a directory\n").unwrap();

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert_eq!(result.kind, SetupKind::Partial, "{}", result.summary());
        assert_eq!(result.exit_code(), 1);
        assert!(result.failures().any(|outcome| outcome.target == "tools"));
        assert!(result.outcomes.iter().any(|outcome| {
            outcome.target == "tools"
                && outcome.outcome == OutcomeKind::Skipped
                && outcome.summary.contains("could not be created")
        }));
        // What worked is kept and reported.
        assert!(dir.join(".git").is_dir());
        assert!(dir.join("engine/.git").is_dir());
        assert!(dir.join(".gitmesh/project.toml").is_file());
        let validation = result.validation.as_ref().unwrap();
        assert!(!validation.ok);
        assert!(
            validation
                .issues
                .iter()
                .any(|issue| issue.contains("'tools'")),
            "{:?}",
            validation.issues
        );
        // The message says what to fix rather than claiming success.
        assert!(result.summary().contains("completed with errors"), "{}", result.summary());
    }

    #[test]
    fn apply_refuses_a_blocked_plan_without_touching_anything() {
        let (dir, runner) = plain("setup-apply-blocked");
        std::fs::create_dir_all(dir.join("engine/core")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine"), repo("engine/core")];
        let plan = plan(&req, &runner).unwrap();
        assert!(!plan.is_ready());

        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert_eq!(result.kind, SetupKind::Failed);
        assert!(!result.refused.is_empty());
        assert!(result.outcomes.is_empty());
        assert!(!dir.join(".git").exists());
        assert!(!dir.join(".gitmesh").exists());
        assert!(!dir.join("engine/.git").exists());
    }

    #[test]
    fn apply_in_dry_run_changes_nothing_but_reports_the_plan() {
        let (dir, runner) = plain("setup-apply-dry");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine")];
        let plan = plan(&req, &runner).unwrap();

        let result = apply(&plan, true, &runner, &mut SetupObserver::silent());
        assert!(result.dry_run);
        assert!(result.succeeded() == 0);
        assert!(result.outcomes.iter().all(|o| o.outcome == OutcomeKind::Skipped));
        assert!(result.summary().contains("dry run"), "{}", result.summary());
        assert!(!dir.join(".git").exists());
        assert!(!dir.join("engine/.git").exists());
        assert!(!dir.join(".gitmesh").exists());
        assert!(result.manifest_path.is_none());
    }

    #[test]
    fn apply_is_idempotent_and_never_touches_a_repository_outside_the_project() {
        let (dir, runner) = plain("setup-apply-idempotent");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let outside = TempDir::new("setup-apply-outside").unwrap();
        let sibling = outside.join("sibling");
        std::fs::create_dir_all(&sibling).unwrap();
        runner.repo(&sibling).run_checked(&["init", "-q", "-b", "main"]).unwrap();
        std::fs::write(sibling.join("work.txt"), "someone else's work\n").unwrap();
        let sibling_head_before = {
            runner.repo(&sibling).run_checked(&["add", "-A"]).unwrap();
            runner
                .repo(&sibling)
                .run_checked(&["-c", "user.name=T", "-c", "user.email=t@t", "commit", "-qm", "x"])
                .unwrap();
            runner.repo(&sibling).run_checked(&["rev-parse", "HEAD"]).unwrap()
        };

        let mut req = request(dir.path());
        req.repositories = vec![repo("engine")];
        let plan = plan(&req, &runner).unwrap();
        let first = apply(&plan, false, &runner, &mut SetupObserver::silent());
        assert!(first.is_success(), "{}", first.summary());
        let manifest_before = std::fs::read_to_string(dir.join(".gitmesh/project.toml")).unwrap();
        let root_head_before = runner
            .repo(dir.path())
            .run_checked(&["rev-parse", "HEAD"])
            .ok();

        // Re-plan and re-apply the very same request.
        let plan_again = plan(&req, &runner).unwrap();
        assert!(plan_again.is_ready());
        assert!(plan_again.is_noop(), "{:?}", plan_again.planned_steps().collect::<Vec<_>>());
        let second = apply(&plan_again, false, &runner, &mut SetupObserver::silent());
        assert!(second.is_success(), "{}", second.summary());
        assert_eq!(second.succeeded(), 0);
        assert!(second.skipped() >= 4);
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitmesh/project.toml")).unwrap(),
            manifest_before
        );
        assert_eq!(
            runner.repo(dir.path()).run_checked(&["rev-parse", "HEAD"]).ok(),
            root_head_before,
            "no commit is created by a setup"
        );
        assert_eq!(
            runner.repo(&sibling).run_checked(&["rev-parse", "HEAD"]).unwrap(),
            sibling_head_before
        );
        assert!(sibling.join("work.txt").is_file());
        assert_eq!(runner.repo(&sibling).status().unwrap().entries.len(), 0);
    }

    #[test]
    fn apply_changes_an_existing_remote_only_when_confirmed() {
        let fixture = RepoFixture::named("setup-apply-remote");
        let first = fixture.create_bare("remotes/one.git");
        let second = fixture.create_bare("remotes/two.git");
        fixture.init_repo("engine");
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", first.to_string_lossy().as_ref()],
        );

        // Not confirmed: the plan is blocked, nothing changes.
        let mut req = request(fixture.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: true,
            remote: Some(second.to_string_lossy().to_string()),
            ..RepositoryRequest::default()
        }];
        let blocked = plan(&req, &fixture.runner()).unwrap();
        apply(&blocked, false, &fixture.runner(), &mut SetupObserver::silent());
        assert_eq!(
            discovery::origin_url(&fixture.path().join("engine"), fixture.runner()).unwrap(),
            Some(first.to_string_lossy().to_string())
        );

        // Confirmed: the remote is replaced and the change is reported.
        req.overwrite_remotes = true;
        let plan = plan(&req, &fixture.runner()).unwrap();
        let result = apply(&plan, false, &fixture.runner(), &mut SetupObserver::silent());
        assert!(result.is_success(), "{}", result.summary());
        assert_eq!(
            discovery::origin_url(&fixture.path().join("engine"), fixture.runner()).unwrap(),
            Some(second.to_string_lossy().to_string())
        );
        assert!(result
            .outcomes
            .iter()
            .any(|outcome| outcome.summary.contains("origin replaced")));
    }

    #[test]
    fn observer_sees_every_step_in_order() {
        let (dir, runner) = plain("setup-apply-observer");
        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![repo("engine")];
        let plan = plan(&req, &runner).unwrap();

        let mut started: Vec<String> = Vec::new();
        let mut finished: Vec<String> = Vec::new();
        {
            let mut on_step = |step: &SetupStep| started.push(step.kind.label().to_string());
            let mut on_outcome =
                |outcome: &SetupStepOutcome| finished.push(outcome.kind.label().to_string());
            let mut observer = SetupObserver::silent()
                .on_step(&mut on_step)
                .on_outcome(&mut on_outcome);
            apply(&plan, false, &runner, &mut observer);
        }
        assert_eq!(started.len(), plan.steps.len());
        assert_eq!(started, finished);
        assert_eq!(started[0], "create-metadata");
        assert_eq!(started.last().unwrap(), "write-manifest");
    }

    // ---------------------------------------------------------- validation --

    #[test]
    fn verify_reports_a_missing_manifest_and_a_missing_repository() {
        let (dir, runner) = plain("setup-verify");
        let report = verify(dir.path(), &runner);
        assert!(!report.ok);
        assert!(report.issues.iter().any(|issue| issue.contains("does not exist")));

        std::fs::create_dir_all(dir.join("engine")).unwrap();
        let mut req = request(dir.path());
        req.repositories = vec![RepositoryRequest {
            path: "engine".into(),
            create: false,
            ..RepositoryRequest::default()
        }];
        let plan = plan(&req, &runner).unwrap();
        let result = apply(&plan, false, &runner, &mut SetupObserver::silent());
        // The directory has no repository: the setup says so instead of pretending.
        assert!(result.kind == SetupKind::Partial);
        let validation = result.validation.as_ref().unwrap();
        assert!(!validation.ok);
        assert!(validation
            .issues
            .iter()
            .any(|issue| issue.contains("no Git repository here yet")));
        let check = validation
            .repositories
            .iter()
            .find(|check| check.id == "engine")
            .unwrap();
        assert!(!check.is_repository && !check.is_ok());
    }

    #[test]
    fn verify_reports_a_remote_that_does_not_match_the_manifest() {
        let fixture = RepoFixture::named("setup-verify-remote");
        fixture.project_with(&[("root", "."), ("engine", "engine")]);

        let report = verify(fixture.path(), &fixture.runner()).unwrap_or_else(|| {
            unreachable!("verify never returns an Option")
        });
        assert!(report.ok, "{:?}", report.issues);

        // Break the remote in Git: the manifest still records the old URL.
        fixture.git_ok(
            "engine",
            &["remote", "add", "origin", "/tmp/somewhere-else.git"],
        );
        let report = verify(fixture.path(), &fixture.runner());
        assert!(!report.ok);
        let check = report
            .repositories
            .iter()
            .find(|check| check.id == "engine")
            .unwrap();
        assert!(!check.remote_ok);
        assert!(
            report
                .issues
                .iter()
                .any(|issue| issue.contains("the manifest records")),
            "{:?}",
            report.issues
        );
    }
}

impl CandidateDirectory {
    /// Test helper: the candidate's project-relative path as a slash string.
    fn path_label(&self) -> String {
        to_slash(&self.relative_path)
    }
}
