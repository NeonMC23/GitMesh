//! Safe execution of the Git CLI.
//!
//! GitMesh never implements Git internals. It drives the real `git` executable and
//! parses its documented, stable output formats (`--porcelain=v2`, `rev-parse`,
//! `remote -v`, ...).
//!
//! Safety properties guaranteed by this module:
//!
//! * **Every repository-scoped command is bound to a working directory.** A
//!   [`GitRepo`] handle is the only way to run a command against a repository, and it
//!   always passes `-C <workdir>` explicitly. There is no API that runs a
//!   repository-scoped command "in the current directory", so GitMesh cannot
//!   accidentally operate on the wrong physical repository.
//! * **Identity is verifiable.** [`GitRepo::verify_identity`] asks Git for the
//!   repository top level and compares it with the expected path. Orchestration code
//!   calls it before any mutating operation.
//! * **Output is stable.** GitMesh forces `core.quotepath=false`, `color.ui=false`
//!   and disables interactive prompts unless explicitly allowed, so parsing does not
//!   depend on the user's Git configuration.
//! * **Nothing is interpreted by a shell.** Arguments are passed as an argument
//!   vector, never through `sh -c`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::status::{self, Head, Remote, RepoStatus};
use crate::error::{Error, Result};
use crate::paths::lexical_normalize;

/// Result of running a Git command.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Human-readable command line, for error messages and logs.
    pub command_line: String,
    /// Process exit code, when the process terminated normally.
    pub code: Option<i32>,
    /// Standard output (lossy UTF-8 decoded).
    pub stdout: String,
    /// Standard error (lossy UTF-8 decoded).
    pub stderr: String,
    /// True when stdout contained bytes that are not valid UTF-8.
    pub lossy: bool,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Turn a failed command into a structured [`Error::GitCommand`].
    pub fn into_error(self, repo_label: &str) -> Error {
        Error::GitCommand {
            command: self.command_line,
            in_repo: if repo_label.is_empty() {
                String::new()
            } else {
                format!(" in repository '{repo_label}'")
            },
            stderr: self.stderr.trim().to_string(),
            stdout: self.stdout,
            code: self.code,
        }
    }

    /// Requires success, returning stdout.
    pub fn require(self, repo_label: &str) -> Result<String> {
        if self.success() {
            Ok(self.stdout)
        } else {
            Err(self.into_error(repo_label))
        }
    }

    /// Requires success, returning trimmed stdout.
    pub fn require_trimmed(self, repo_label: &str) -> Result<String> {
        Ok(self.require(repo_label)?.trim().to_string())
    }
}

/// Runs the Git CLI.
///
/// Clone-free and cheap to share: it only holds the executable path and environment
/// policy.
#[derive(Debug, Clone)]
pub struct GitRunner {
    program: PathBuf,
    /// When false (default) GitMesh sets `GIT_TERMINAL_PROMPT=0`, so a command that
    /// would need credentials fails immediately with an actionable message instead of
    /// blocking a GUI/TUI waiting for input that can never arrive.
    allow_prompt: bool,
}

impl Default for GitRunner {
    fn default() -> Self {
        Self::with_program("git")
    }
}

impl GitRunner {
    /// Create a runner using the given executable.
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        GitRunner {
            program: program.into(),
            allow_prompt: false,
        }
    }

    /// Create a runner using `git` from `PATH`, verifying that it can be executed.
    pub fn detect() -> Result<Self> {
        let runner = GitRunner::default();
        runner.version()?;
        Ok(runner)
    }

    /// Allow Git to prompt on the terminal (useful when a human runs the CLI
    /// interactively and authentication is needed).
    pub fn with_prompt_allowed(mut self, allow: bool) -> Self {
        self.allow_prompt = allow;
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn prompts_allowed(&self) -> bool {
        self.allow_prompt
    }

    /// `git --version`, used as a smoke test that the executable works.
    pub fn version(&self) -> Result<String> {
        let mut command = Command::new(&self.program);
        command.arg("--version").stdin(Stdio::null());
        apply_env(&mut command, self.allow_prompt);
        let output = command.output().map_err(|source| Error::GitUnavailable {
            program: self.program.display().to_string(),
            source,
        })?;
        if !output.status.success() {
            return Err(Error::GitUnavailable {
                program: self.program.display().to_string(),
                source: std::io::Error::other(String::from_utf8_lossy(&output.stderr).to_string()),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run Git with no repository binding (e.g. `git --version`, `git clone`).
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<CommandOutput> {
        let args: Vec<OsString> = args.iter().map(|a| a.as_ref().to_os_string()).collect();
        run_command(&self.program, None, &args, self.allow_prompt)
    }

    /// Bind a working directory and obtain a repository handle.
    ///
    /// The handle does not check that the directory is a repository; use
    /// [`GitRepo::is_repository`] or [`GitRepo::verify_identity`] for that.
    pub fn repo(&self, workdir: impl Into<PathBuf>) -> GitRepo<'_> {
        GitRepo {
            runner: self,
            workdir: workdir.into(),
        }
    }
}

/// A handle to a Git working directory. All repository-scoped commands go through
/// this type so the working directory can never be forgotten.
#[derive(Debug)]
pub struct GitRepo<'a> {
    runner: &'a GitRunner,
    workdir: PathBuf,
}

impl<'a> GitRepo<'a> {
    /// The working directory every command is executed in.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn runner(&self) -> &GitRunner {
        self.runner
    }

    /// Run a Git command inside this working directory. Never fails because of a
    /// non-zero exit status; callers decide how to interpret it.
    pub fn run<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<CommandOutput> {
        let args: Vec<OsString> = args.iter().map(|a| a.as_ref().to_os_string()).collect();
        run_command(
            &self.runner.program,
            Some(&self.workdir),
            &args,
            self.runner.allow_prompt,
        )
    }

    /// Run a command and require success, returning stdout.
    pub fn run_checked<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<String> {
        let label = self.workdir.display().to_string();
        self.run(args)?.require(&label)
    }

    /// Run a command that is expected to fail in normal operation (e.g.
    /// `rev-parse --verify`). Returns `None` on non-zero exit.
    pub fn run_optional<S: AsRef<OsStr>>(&self, args: &[S]) -> Result<Option<String>> {
        let output = self.run(args)?;
        if output.success() {
            Ok(Some(output.stdout.trim().to_string()))
        } else {
            Ok(None)
        }
    }

    // ---------------------------------------------------------------- identity --

    /// True when this directory is inside a Git work tree.
    pub fn is_repository(&self) -> bool {
        if !self.workdir.is_dir() {
            return false;
        }
        matches!(self.run(&["rev-parse", "--is-inside-work-tree"]), Ok(o) if o.success() && o.stdout.trim() == "true")
    }

    /// Path of the repository top level (`git rev-parse --show-toplevel`).
    pub fn top_level(&self) -> Result<Option<PathBuf>> {
        match self.run_optional(&["rev-parse", "--show-toplevel"])? {
            Some(path) if !path.is_empty() => Ok(Some(PathBuf::from(path))),
            _ => Ok(None),
        }
    }

    /// Absolute path of the Git directory (`git rev-parse --absolute-git-dir`).
    pub fn git_dir(&self) -> Result<Option<PathBuf>> {
        match self.run_optional(&["rev-parse", "--absolute-git-dir"])? {
            Some(path) if !path.is_empty() => Ok(Some(PathBuf::from(path))),
            _ => Ok(None),
        }
    }

    /// Verify that the working directory really is the repository root GitMesh thinks
    /// it is. This is the guard that makes it impossible to run an operation against
    /// the wrong physical repository (e.g. when a configured path points at a
    /// subdirectory of another repository instead of its own repository root).
    pub fn verify_identity(&self) -> Result<()> {
        let expected = canonical(&self.workdir);
        match self.top_level()? {
            Some(actual) => {
                if canonical(&actual) == expected {
                    Ok(())
                } else {
                    Err(Error::Other(format!(
                        "refusing to operate on {}: it is inside the Git repository rooted at {} \
                         (repository boundaries must not overlap; check the GitMesh manifest)",
                        self.workdir.display(),
                        actual.display()
                    )))
                }
            }
            None => Err(Error::NotARepository {
                path: self.workdir.clone(),
            }),
        }
    }

    // ------------------------------------------------------------ read helpers --

    /// Current `HEAD` state.
    pub fn head(&self) -> Result<Head> {
        // symbolic-ref first: distinguishes unborn and attached branches from
        // detached HEAD without relying on error text.
        if let Some(name) = self.run_optional(&["symbolic-ref", "--quiet", "--short", "HEAD"])? {
            if name.is_empty() {
                return Ok(Head::Unknown);
            }
            if self
                .run_optional(&["rev-parse", "--verify", "--quiet", "HEAD"])?
                .is_none()
            {
                return Ok(Head::Unborn { branch: name });
            }
            return Ok(Head::Branch { name });
        }
        match self.run_optional(&["rev-parse", "--verify", "--quiet", "HEAD"])? {
            Some(oid) => Ok(Head::Detached { oid }),
            None => Ok(Head::Unknown),
        }
    }

    /// Short commit id of `HEAD`, if any.
    pub fn head_oid(&self) -> Result<Option<String>> {
        self.run_optional(&["rev-parse", "--verify", "--quiet", "HEAD"])
    }

    /// `git status --porcelain=v2 --branch`.
    pub fn status(&self) -> Result<RepoStatus> {
        // `-uall` reports every untracked file instead of collapsing untracked
        // directories, which is what a path-ownership view needs.
        let out = self.run(&[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=all",
            "-z",
        ])?;
        let label = self.workdir.display().to_string();
        let lossy = out.lossy;
        let raw = out.require(&label)?;
        status::parse_porcelain_v2(&raw, self.workdir.as_path(), lossy)
    }

    /// Configured remotes with their fetch/push URLs.
    pub fn remotes(&self) -> Result<Vec<Remote>> {
        let out = self.run(&["remote", "-v"])?;
        if !out.success() {
            // A repository without any remote exits 0 with empty output; a failure
            // here means something is genuinely wrong, but it must not abort a
            // project-wide operation.
            return Ok(Vec::new());
        }
        Ok(status::parse_remotes(&out.stdout))
    }

    /// True when a branch exists locally.
    pub fn branch_exists(&self, name: &str) -> Result<bool> {
        Ok(self
            .run_optional(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{name}"),
            ])?
            .is_some())
    }

    /// True when the named remote exists.
    pub fn remote_exists(&self, name: &str) -> Result<bool> {
        Ok(self
            .run_optional(&["remote", "get-url", name])?
            .is_some_and(|s| !s.is_empty()))
    }

    /// Upstream of the current branch, if configured.
    pub fn upstream(&self) -> Result<Option<String>> {
        match self.run_optional(&[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ])? {
            Some(s) if !s.is_empty() => Ok(Some(s)),
            _ => Ok(None),
        }
    }

    /// Number of commits ahead / behind the upstream: `(ahead, behind)`.
    pub fn ahead_behind(&self) -> Result<Option<(u32, u32)>> {
        let out = self.run(&["rev-list", "--left-right", "--count", "@{upstream}...HEAD"])?;
        if !out.success() {
            return Ok(None);
        }
        let mut parts = out.stdout.split_whitespace();
        match (parts.next(), parts.next()) {
            (Some(behind), Some(ahead)) => {
                let behind = behind.parse().unwrap_or(0);
                let ahead = ahead.parse().unwrap_or(0);
                Ok(Some((ahead, behind)))
            }
            _ => Ok(None),
        }
    }

    /// Local branches.
    pub fn local_branches(&self) -> Result<Vec<String>> {
        let out = self.run(&["for-each-ref", "--format=%(refname:short)", "refs/heads"])?;
        if !out.success() {
            return Ok(Vec::new());
        }
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// A long-running Git operation that is currently in progress, if any.
    pub fn operation_in_progress(&self) -> Result<Option<InProgressOperation>> {
        // The markers live in this worktree's Git directory. `rev-parse --git-path` prints
        // them *relative to the repository*, so testing that string from the process's own
        // working directory would look in the wrong place for every repository except the
        // one GitMesh runs in. The absolute Git directory is resolved against the repository
        // itself, which is what makes the check correct for every repository.
        let Some(git_dir) = self.git_dir()? else {
            return Ok(None);
        };
        for (marker, operation) in [
            ("MERGE_HEAD", InProgressOperation::Merge),
            ("CHERRY_PICK_HEAD", InProgressOperation::CherryPick),
            ("REVERT_HEAD", InProgressOperation::Revert),
            ("BISECT_LOG", InProgressOperation::Bisect),
        ] {
            if git_dir.join(marker).exists() {
                return Ok(Some(operation));
            }
        }
        if git_dir.join("rebase-merge").exists() || git_dir.join("rebase-apply").exists() {
            return Ok(Some(InProgressOperation::Rebase));
        }
        Ok(None)
    }

    /// Tracked files that live under `relative` inside this repository.
    ///
    /// Used to detect the situation where the root repository still tracks files that
    /// were assigned to an external repository.
    pub fn tracked_files_under(&self, relative: &Path) -> Result<Vec<PathBuf>> {
        let mut args: Vec<OsString> = vec!["ls-files".into(), "-z".into(), "--".into()];
        args.push(relative.as_os_str().to_os_string());
        let out = self.run(&args)?;
        if !out.success() {
            return Ok(Vec::new());
        }
        Ok(out
            .stdout
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect())
    }
}

/// Long-running Git operations GitMesh recognises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProgressOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl InProgressOperation {
    pub fn label(self) -> &'static str {
        match self {
            InProgressOperation::Merge => "merge in progress",
            InProgressOperation::Rebase => "rebase in progress",
            InProgressOperation::CherryPick => "cherry-pick in progress",
            InProgressOperation::Revert => "revert in progress",
            InProgressOperation::Bisect => "bisect in progress",
        }
    }
}

/// Build and run the process.
fn run_command(
    program: &Path,
    workdir: Option<&Path>,
    args: &[OsString],
    allow_prompt: bool,
) -> Result<CommandOutput> {
    let mut command = Command::new(program);
    // Stable, parseable output regardless of the user's Git configuration.
    command
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("--no-pager")
        .arg("-c")
        .arg("color.ui=false")
        .arg("-c")
        .arg("advice.detachedHead=false");
    if let Some(dir) = workdir {
        command.arg("-C").arg(dir);
    }
    command.args(args);
    command.stdin(Stdio::null());
    apply_env(&mut command, allow_prompt);

    let output = command.output().map_err(|source| Error::GitUnavailable {
        program: program.display().to_string(),
        source,
    })?;

    let stdout_raw = output.stdout;
    let stderr_raw = output.stderr;
    let stdout_decoded = String::from_utf8(stdout_raw);
    let lossy = stdout_decoded.is_err();
    let stdout = match stdout_decoded {
        Ok(text) => text,
        Err(err) => String::from_utf8_lossy(&err.into_bytes()).into_owned(),
    };
    let stderr = String::from_utf8_lossy(&stderr_raw).into_owned();

    Ok(CommandOutput {
        command_line: render_command_line(program, workdir, args),
        code: output.status.code(),
        stdout,
        stderr,
        lossy,
    })
}

fn apply_env(command: &mut Command, allow_prompt: bool) {
    if !allow_prompt {
        command.env("GIT_TERMINAL_PROMPT", "0");
    }
    // Keep output stable and locale-independent.
    command.env("LC_ALL", "C");
    command.env("GIT_PAGER", "cat");
}

fn render_command_line(program: &Path, workdir: Option<&Path>, args: &[OsString]) -> String {
    let mut parts = vec![program.display().to_string()];
    if let Some(dir) = workdir {
        parts.push(format!("-C {}", dir.display()));
    }
    parts.extend(args.iter().map(|a| a.to_string_lossy().to_string()));
    parts.join(" ")
}

/// Canonicalise for comparison, falling back to lexical normalisation when the path
/// cannot be resolved (it may not exist yet).
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runner() -> GitRunner {
        GitRunner::detect().expect("git must be available in the test environment")
    }

    #[test]
    fn detects_git_version() {
        let version = runner().version().unwrap();
        assert!(version.starts_with("git version"), "got {version}");
    }

    #[test]
    fn missing_git_reports_a_clear_error() {
        let runner = GitRunner::with_program("git-that-does-not-exist-gitmesh");
        let err = runner.version().unwrap_err();
        let message = err.to_string();
        assert!(message.contains("could not be run"), "{message}");
        assert!(message.contains("PATH"), "{message}");
    }

    #[test]
    fn repository_handle_binds_the_working_directory() {
        let git = runner();
        let repo = git.repo("/tmp");
        assert_eq!(repo.workdir(), Path::new("/tmp"));
        // `-C` must be present in the rendered command line
        let out = repo.run(&["rev-parse", "--is-inside-work-tree"]).unwrap();
        assert!(out.command_line.contains("-C /tmp"), "{}", out.command_line);
    }

    #[test]
    fn non_repository_directory_is_reported_not_a_repository() {
        let dir = crate::testkit::TempDir::new("cmd-tests").unwrap();
        let git = runner();
        let repo = git.repo(dir.path());
        assert!(!repo.is_repository());
        let err = repo.verify_identity().unwrap_err();
        assert!(matches!(err, Error::NotARepository { .. }), "{err:?}");
    }

    #[test]
    fn identity_check_rejects_a_subdirectory_of_another_repository() {
        let dir = crate::testkit::TempDir::new("cmd-tests").unwrap();
        let path = dir.path();
        let git = runner();
        let repo = git.repo(path);
        repo.run_checked(&["init", "-q", "-b", "main"]).unwrap();
        std::fs::create_dir_all(path.join("sub")).unwrap();

        // The root of the work tree verifies fine.
        repo.verify_identity().unwrap();

        // A subdirectory of the same work tree must be rejected: it is not a
        // repository root, so treating it as one would run commands against the
        // parent repository.
        let sub = git.repo(path.join("sub"));
        let err = sub.verify_identity().unwrap_err();
        assert!(err.to_string().contains("refusing to operate"), "{err}");
    }

    #[test]
    fn optional_commands_do_not_error_on_failure() {
        let dir = crate::testkit::TempDir::new("cmd-tests").unwrap();
        let git = runner();
        let repo = git.repo(dir.path());
        repo.run_checked(&["init", "-q", "-b", "main"]).unwrap();
        assert!(repo
            .run_optional(&["rev-parse", "--verify", "HEAD"])
            .unwrap()
            .is_none());
    }

    #[test]
    fn failed_commands_carry_repository_context() {
        let dir = crate::testkit::TempDir::new("cmd-tests").unwrap();
        let git = runner();
        let repo = git.repo(dir.path());
        repo.run_checked(&["init", "-q", "-b", "main"]).unwrap();
        // No commit yet: `rev-parse HEAD` must fail through run_checked.
        let err = repo.run_checked(&["rev-parse", "HEAD"]).unwrap_err();
        assert!(err.to_string().contains("rev-parse HEAD"), "{err}");
        assert!(!err.is_configuration_error());
    }

    #[test]
    fn prompts_are_disabled_by_default() {
        let git = runner();
        assert!(!git.prompts_allowed());
        assert!(!git
            .with_prompt_allowed(true)
            .program()
            .as_os_str()
            .is_empty());
    }
}
