//! Test support: temporary directories, real Git repositories and ready-made
//! GitMesh projects.
//!
//! GitMesh's behaviour is about real Git repositories, so its tests use real ones.
//! Everything here creates actual repositories on disk in a temporary directory and
//! drives them with the real Git CLI — no mocks. This module is intentionally small,
//! dependency-free (no `tempfile` crate: the temporary directory logic is ~40 lines)
//! and available to integration tests, which cannot reach `#[cfg(test)]` items.
//!
//! Nothing in this module is used by the `gitmesh` binary at runtime.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::git::{CommandOutput, GitRunner};
use crate::manifest;
use crate::model::{GitMeshProject, PhysicalRepository, RepositoryRole};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A temporary directory that is deleted when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create a uniquely named temporary directory.
    pub fn new(label: &str) -> Result<TempDir> {
        let unique = format!(
            "gitmesh-{label}-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let path = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&path).map_err(|source| Error::io(path.clone(), source))?;
        Ok(TempDir { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Join a relative path.
    pub fn join(&self, rel: &str) -> PathBuf {
        if rel == "." || rel.is_empty() {
            self.path.clone()
        } else {
            self.path.join(rel)
        }
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A temporary directory containing a GitMesh-shaped project.
#[derive(Debug)]
pub struct RepoFixture {
    /// The project root: a real Git repository with a GitMesh manifest.
    temp: TempDir,
    /// A directory *outside* the project, used for bare remotes and for clones that
    /// play the part of another developer. Keeping them out of the project tree is
    /// what makes the fixture behave like a real setup: nothing inside the project
    /// root is dirty just because a remote exists.
    outside: TempDir,
    runner: GitRunner,
    name: String,
}

impl RepoFixture {
    /// Create a temporary directory and initialise the root repository in it.
    ///
    /// The root repository starts with one commit (`README.md`) and a `main` branch,
    /// which is what almost every test expects.
    pub fn new() -> Self {
        Self::named("project")
    }

    /// Same as [`RepoFixture::new`] with a custom label (useful when debugging).
    pub fn named(label: &str) -> Self {
        let temp = TempDir::new(label).expect("temporary directory");
        let outside = TempDir::new(&format!("{label}-remotes")).expect("temporary directory");
        let runner = GitRunner::detect().expect("git must be installed to run GitMesh tests");
        let fixture = RepoFixture {
            temp,
            outside,
            runner,
            name: label.to_string(),
        };
        fixture.init_repo(".");
        fixture.write("README.md", "# project\n");
        fixture.commit(".", "initial commit");
        fixture
    }

    /// The project root.
    pub fn path(&self) -> &Path {
        self.temp.path()
    }

    /// The directory outside the project that holds bare remotes and other clones.
    pub fn outside_path(&self) -> &Path {
        self.outside.path()
    }

    /// Absolute path of a bare remote created earlier.
    pub fn bare_path(&self, rel: &str) -> PathBuf {
        self.outside.join(rel)
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    /// Initialise a Git repository and give it a commit identity.
    pub fn init_repo(&self, rel: &str) -> PathBuf {
        let path = self.temp.join(rel);
        std::fs::create_dir_all(&path).expect("create repository directory");
        let repo = self.runner.repo(&path);
        repo.run_checked(&["init", "-q", "-b", "main"])
            .expect("git init");
        self.configure_identity(&path);
        path
    }

    /// Configure a commit identity inside one repository.
    pub fn configure_identity(&self, repo_path: &Path) {
        let repo = self.runner.repo(repo_path);
        repo.run_checked(&["config", "user.email", "dev@gitmesh.test"])
            .ok();
        repo.run_checked(&["config", "user.name", "GitMesh Test"])
            .ok();
        // Deterministic, isolated from the developer's global configuration.
        repo.run_checked(&["config", "commit.gpgsign", "false"])
            .ok();
        repo.run_checked(&["config", "init.defaultBranch", "main"])
            .ok();
    }

    /// Write a file, creating parent directories.
    pub fn write(&self, rel_path: &str, content: &str) -> PathBuf {
        let full = self.temp.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent directories");
        }
        std::fs::write(&full, content).expect("write file");
        full
    }

    /// Append to a file (creating it if needed) so that a change is guaranteed.
    pub fn append(&self, rel_path: &str, content: &str) -> PathBuf {
        let full = self.temp.join(rel_path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).expect("create parent directories");
        }
        let mut existing = std::fs::read_to_string(&full).unwrap_or_default();
        existing.push_str(content);
        std::fs::write(&full, existing).expect("append to file");
        full
    }

    /// Remove a file.
    pub fn remove(&self, rel_path: &str) {
        let full = self.temp.join(rel_path);
        std::fs::remove_file(&full).unwrap_or_else(|e| panic!("remove {full:?}: {e}"));
    }

    /// Create a directory.
    pub fn mkdir(&self, rel_path: &str) -> PathBuf {
        let full = self.temp.join(rel_path);
        std::fs::create_dir_all(&full).expect("create directory");
        full
    }

    /// The repository root that owns `rel_path`: the closest ancestor containing a
    /// `.git` entry.
    pub fn owning_repo(&self, rel_path: &str) -> PathBuf {
        let start = self.temp.join(rel_path);
        let mut current = if start.is_dir() {
            start
        } else {
            start
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| self.temp.path().to_path_buf())
        };
        loop {
            if current.join(".git").exists() {
                return current;
            }
            if !current.pop() || !current.starts_with(self.temp.path()) {
                return self.temp.path().to_path_buf();
            }
        }
    }

    /// Stage everything and commit inside the repository that owns `repo_rel`
    /// (`"."` for the root repository).
    pub fn commit(&self, repo_rel: &str, message: &str) -> String {
        let path = self.temp.join(repo_rel);
        self.add_all(repo_rel);
        let repo = self.runner.repo(&path);
        repo.run_checked(&["commit", "-q", "-m", message])
            .expect("git commit");
        repo.head_oid().unwrap().unwrap_or_default()
    }

    /// `git add -A` inside one repository, excluding directories that are themselves
    /// Git repositories. This mirrors what GitMesh does for the root repository: a
    /// nested repository must never be staged as a gitlink by a test helper either.
    pub fn add_all(&self, repo_rel: &str) {
        let path = self.temp.join(repo_rel);
        let repo = self.runner.repo(&path);
        let mut args: Vec<String> = vec!["add".into(), "-A".into(), "--".into(), ".".into()];
        for nested in self.nested_repository_dirs(&path) {
            args.push(format!(":(exclude){nested}"));
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        if let Err(err) = repo.run_checked(&args) {
            // A repository whose nested repository has no commits yet cannot be added
            // as a gitlink; retry with every nested directory excluded.
            let mut fallback: Vec<String> =
                vec!["add".into(), "-A".into(), "--".into(), ".".into()];
            for dir in self.child_git_dirs(&path) {
                fallback.push(format!(":(exclude){dir}"));
            }
            let fallback: Vec<&str> = fallback.iter().map(String::as_str).collect();
            repo.run_checked(&fallback)
                .unwrap_or_else(|e2| panic!("git add failed: {err}\nand: {e2}"));
        }
    }

    /// Nested repositories below `repo_path`, up to a small depth, as `/`-separated
    /// paths relative to that repository.
    fn nested_repository_dirs(&self, repo_path: &Path) -> Vec<String> {
        let mut found = Vec::new();
        collect_git_dirs(repo_path, repo_path, 4, &mut found);
        found
    }

    fn child_git_dirs(&self, repo_path: &Path) -> Vec<String> {
        let mut found = Vec::new();
        collect_git_dirs(repo_path, repo_path, 1, &mut found);
        found
    }

    /// Commit whatever exists at `project_rel_path` in its owning repository.
    pub fn commit_path(&self, project_rel_path: &str, message: &str) -> String {
        let repo_path = self.owning_repo(project_rel_path);
        let repo_rel = crate::paths::project_relative(self.temp.path(), &repo_path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        self.add_all(&repo_rel);
        let repo = self.runner.repo(&repo_path);
        repo.run_checked(&["commit", "-q", "-m", message])
            .expect("git commit");
        repo.head_oid().unwrap().unwrap_or_default()
    }

    /// Write a file and commit it in its owning repository.
    pub fn write_and_commit(&self, rel_path: &str, content: &str) -> String {
        self.write(rel_path, content);
        self.commit_path(rel_path, &format!("update {rel_path}"))
    }

    /// Run a Git command inside a repository of the fixture.
    pub fn git(&self, repo_rel: &str, args: &[&str]) -> CommandOutput {
        self.runner
            .repo(self.temp.join(repo_rel))
            .run(args)
            .expect("run git")
    }

    /// Run a Git command and assert that it succeeded.
    pub fn git_ok(&self, repo_rel: &str, args: &[&str]) -> String {
        let out = self.git(repo_rel, args);
        assert!(
            out.success(),
            "git {:?} in {repo_rel} failed: {}{}",
            args,
            out.stdout,
            out.stderr
        );
        out.stdout
    }

    /// Create a bare repository outside the project (a local stand-in for a hosted
    /// remote).
    pub fn create_bare(&self, rel: &str) -> PathBuf {
        let path = self.outside.join(rel);
        std::fs::create_dir_all(&path).expect("create bare repository directory");
        let out = self
            .runner
            .run(&[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                path.to_string_lossy().as_ref(),
            ])
            .expect("git init --bare");
        assert!(out.success(), "git init --bare failed: {}", out.stderr);
        path
    }

    /// Point a repository at a bare repository and push its current branch, so that
    /// upstream tracking is configured exactly like a real remote.
    pub fn publish(&self, repo_rel: &str, bare_rel: &str) -> PathBuf {
        let bare = self.create_bare(bare_rel);
        let repo = self.runner.repo(self.temp.join(repo_rel));
        let url = bare.to_string_lossy().to_string();
        let remotes = repo.remotes().unwrap_or_default();
        if remotes.iter().any(|r| r.name == "origin") {
            repo.run_checked(&["remote", "set-url", "origin", &url])
                .unwrap();
        } else {
            repo.run_checked(&["remote", "add", "origin", &url])
                .unwrap();
        }
        let out = repo.run(&["push", "-u", "origin", "HEAD"]);
        let out = out.expect("git push");
        assert!(out.success(), "git push failed: {}", out.stderr);
        bare
    }

    /// Clone a bare repository into the project tree (used to simulate a repository
    /// that was cloned into the project).
    pub fn clone_from(&self, bare: &Path, rel: &str) -> PathBuf {
        let target = self.temp.join(rel);
        self.clone_into(bare, &target)
    }

    /// Clone a bare repository outside the project tree, i.e. "another developer".
    pub fn clone_outside(&self, bare: &Path, rel: &str) -> PathBuf {
        let target = self.outside.join(rel);
        self.clone_into(bare, &target)
    }

    fn clone_into(&self, bare: &Path, target: &Path) -> PathBuf {
        let out = self
            .runner
            .run(&[
                "clone",
                "-q",
                bare.to_string_lossy().as_ref(),
                target.to_string_lossy().as_ref(),
            ])
            .expect("git clone");
        assert!(out.success(), "git clone failed: {}", out.stderr);
        self.configure_identity(target);
        target.to_path_buf()
    }

    /// Build a GitMesh project for this fixture, initialising any repositories that do
    /// not exist yet and writing the manifest.
    ///
    /// `repos` is a list of `(logical id, project-relative path)`; the entry with path
    /// `"."` is the root repository and must be present.
    pub fn project_with(&self, repos: &[(&str, &str)]) -> GitMeshProject {
        self.project_with_options(repos, true)
    }

    /// Build a project whose newly created external repositories start *without*
    /// commits, which is the state `git init` leaves behind.
    pub fn project_without_commits(&self, repos: &[(&str, &str)]) -> GitMeshProject {
        self.project_with_options(repos, false)
    }

    fn project_with_options(
        &self,
        repos: &[(&str, &str)],
        initial_commits: bool,
    ) -> GitMeshProject {
        let mut repositories = Vec::new();
        for (id, rel) in repos {
            let path = self.temp.join(rel);
            if *rel != "." && !path.join(".git").exists() {
                self.init_repo(rel);
                if initial_commits {
                    self.write(&format!("{rel}/README.md"), &format!("# {id}\n"));
                    self.commit(rel, "initial commit");
                }
            }
            let role = if *rel == "." {
                RepositoryRole::Root
            } else {
                RepositoryRole::External
            };
            repositories.push(PhysicalRepository {
                id: (*id).to_string(),
                role,
                relative_path: PathBuf::from(*rel),
                remote_url: None,
                branch: None,
                absolute_path: path,
            });
        }
        let project = GitMeshProject {
            name: self.name.clone(),
            root: self.temp.path().to_path_buf(),
            repositories,
        };
        manifest::save_project(&project).expect("save manifest");
        // Like a real project after `gitmesh init`: the manifest is a normal file
        // that belongs to the root repository. Commit it so the project starts clean.
        self.add_all(".");
        let root = self.runner.repo(self.temp.path());
        if !matches!(root.head(), Ok(crate::git::Head::Unborn { .. })) {
            root.run_checked(&["commit", "-q", "-m", "gitmesh: add project manifest"])
                .expect("commit manifest");
        }
        project
    }

    /// Reload the project from disk the way the CLI does (restart simulation).
    pub fn load_project(&self) -> GitMeshProject {
        manifest::load_from_root(self.temp.path()).expect("load manifest")
    }

    /// Create a merge conflict inside one repository between `main` and `feature`.
    pub fn create_conflict(&self, repo_rel: &str, file_rel: &str) {
        let repo_path = self.temp.join(repo_rel);
        let file = repo_path.join(file_rel);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file, "base\n").unwrap();
        let repo = self.runner.repo(&repo_path);
        repo.run_checked(&["add", "-A"]).unwrap();
        repo.run_checked(&["commit", "-q", "-m", "base"]).unwrap();

        repo.run_checked(&["checkout", "-q", "-b", "feature"])
            .unwrap();
        std::fs::write(&file, "feature side\n").unwrap();
        repo.run_checked(&["add", "-A"]).unwrap();
        repo.run_checked(&["commit", "-q", "-m", "feature change"])
            .unwrap();

        repo.run_checked(&["checkout", "-q", "main"]).unwrap();
        std::fs::write(&file, "main side\n").unwrap();
        repo.run_checked(&["add", "-A"]).unwrap();
        repo.run_checked(&["commit", "-q", "-m", "main change"])
            .unwrap();

        let out = repo.run(&["merge", "feature"]);
        assert!(
            matches!(out, Ok(ref o) if !o.success()),
            "merge was expected to conflict"
        );
    }
}

impl Default for RepoFixture {
    fn default() -> Self {
        Self::new()
    }
}

/// Collect nested Git repositories below `root` (bounded depth), excluding `root`
/// itself.
fn collect_git_dirs(root: &Path, current: &Path, depth: usize, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        if path.join(".git").exists() {
            if let Some(rel) = crate::paths::project_relative(root, &path) {
                found.push(crate::paths::to_slash(&rel));
            }
        }
        if depth > 1 {
            collect_git_dirs(root, &path, depth - 1, found);
        }
    }
}

/// Convenience: build an analyzer for a fixture project.
pub fn fixture_analyzer<'a>(
    project: &'a GitMeshProject,
    fixture: &'a RepoFixture,
) -> crate::analyzer::Analyzer<'a> {
    crate::analyzer::Analyzer::new(project, fixture.runner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_dir_is_unique_and_cleaned_up() {
        let a = TempDir::new("a").unwrap();
        let b = TempDir::new("b").unwrap();
        assert_ne!(a.path(), b.path());
        assert!(a.path().exists());
        let path = a.path().to_path_buf();
        drop(a);
        assert!(!path.exists());
    }

    #[test]
    fn fixture_creates_a_real_repository_with_a_commit() {
        let fixture = RepoFixture::new();
        let repo = fixture.runner().repo(fixture.path());
        assert!(repo.is_repository());
        let status = repo.status().unwrap();
        assert!(status.is_fully_clean());
        assert_eq!(status.head.branch(), Some("main"));
    }

    #[test]
    fn fixture_builds_projects_with_several_repositories() {
        let fixture = RepoFixture::new();
        let project = fixture.project_with(&[("root", "."), ("engine", "engine")]);
        assert_eq!(project.len(), 2);
        assert!(fixture.path().join("engine/.git").exists());
        let reloaded = fixture.load_project();
        assert_eq!(reloaded, project);
    }

    #[test]
    fn fixture_publishes_to_a_bare_remote() {
        let fixture = RepoFixture::new();
        let bare = fixture.publish(".", "remotes/root.git");
        assert!(bare.join("HEAD").exists());
        let repo = fixture.runner().repo(fixture.path());
        let (ahead, behind) = repo.ahead_behind().unwrap().unwrap();
        assert_eq!((ahead, behind), (0, 0));
    }
}
