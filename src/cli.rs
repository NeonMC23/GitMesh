//! Command line interface.
//!
//! The CLI is a thin shell over [`crate::ops`]: it parses arguments, calls the
//! orchestration layer and renders results. It contains no Git logic of its own — the
//! same functions are used by the terminal UI, which is what keeps the two front ends
//! from drifting apart.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::ops::sync::SyncOptions;
use crate::ops::{BranchAction, PullStrategy, RepositorySelection};

/// One logical project over many physical Git repositories.
#[derive(Debug, Parser)]
#[command(
    name = "gitmesh",
    version,
    about = "GitMesh - one logical project over many physical Git repositories",
    long_about = "GitMesh lets you work in a single local project directory while the project is \
                  physically stored in several Git repositories. Git remains the version control \
                  engine: GitMesh orchestrates real Git commands across repositories.\n\n\
                  Run `gitmesh init` in a project, mark directories as external repositories with \
                  `gitmesh configure add <dir>`, then use status/commit/pull/push normally.",
    disable_help_subcommand = true,
    after_help = "EXIT CODES:\n  0  the operation succeeded everywhere\n  1  the operation failed or was only partly successful\n  2  usage or configuration error"
)]
pub struct Cli {
    /// Project directory or any path inside it (defaults to the current directory).
    #[arg(long, short = 'C', global = true, value_name = "PATH")]
    pub project: Option<PathBuf>,

    /// Print machine-readable JSON instead of the human-readable view.
    #[arg(long, global = true)]
    pub json: bool,

    /// Show the underlying Git output as well.
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Global options extracted from the CLI, shared by all commands.
#[derive(Debug, Clone)]
pub struct GlobalOptions {
    pub project: Option<PathBuf>,
    pub json: bool,
    pub verbose: bool,
}

impl Cli {
    pub fn global(&self) -> GlobalOptions {
        GlobalOptions {
            project: self.project.clone(),
            json: self.json,
            verbose: self.verbose,
        }
    }
}

/// GitMesh subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a GitMesh project in a directory (does not touch existing files).
    Init(InitArgs),

    /// Scan a project tree and report the Git repositories inside it.
    Discover(DiscoverArgs),

    /// Show and modify the project configuration.
    #[command(subcommand)]
    Configure(ConfigureCommand),

    /// Show the unified status of the whole logical project.
    Status(StatusArgs),

    /// Commit changes in every affected physical repository with one message.
    Commit(CommitArgs),

    /// Show or modify the logical branch of the project.
    Branch(BranchArgs),

    /// Switch every physical repository to a branch.
    Checkout(CheckoutArgs),

    /// Merge a branch into the current branch in every repository.
    Merge(MergeArgs),

    /// Fetch from the remote in every repository.
    Fetch(SyncArgs),

    /// Pull every repository (never discarding local work).
    Pull(PullArgs),

    /// Push every repository that has commits to push.
    Push(PushArgs),

    /// Show remotes, hosting providers and GitHub coordinates.
    Remotes,

    /// Open the interactive terminal interface.
    #[command(alias = "tui")]
    Ui(UiArgs),

    /// Open the graphical interface in a browser (served locally by GitMesh).
    Gui(GuiArgs),
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory to initialise (defaults to the current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Logical project name (defaults to the directory name).
    #[arg(long)]
    pub name: Option<String>,
    /// Remote URL of the root repository (recorded in the manifest; use
    /// --add-git-remote to also configure `origin`).
    #[arg(long)]
    pub remote: Option<String>,
    /// Also configure `origin` in the root repository.
    #[arg(long)]
    pub add_git_remote: bool,
    /// Default branch name recorded in the manifest.
    #[arg(long)]
    pub branch: Option<String>,
    /// Replace an existing manifest.
    #[arg(long)]
    pub force: bool,
    /// Create a Git repository in the project root when it is not one yet.
    #[arg(long)]
    pub git_init: bool,
}

#[derive(Debug, Args)]
pub struct DiscoverArgs {
    /// Directory to scan (defaults to the project root or the current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Maximum directory depth.
    #[arg(long, default_value_t = 6)]
    pub depth: usize,
    /// Include hidden directories.
    #[arg(long)]
    pub hidden: bool,
    /// Descend into nested repositories instead of stopping at their root.
    #[arg(long)]
    pub deep: bool,
}

#[derive(Debug, Subcommand)]
pub enum ConfigureCommand {
    /// Mark a directory as an independent physical repository.
    Add(ConfigureAddArgs),
    /// Remove a repository from the configuration (never deletes files).
    Remove(ConfigureRemoveArgs),
    /// Rename the logical identifier of a repository.
    Rename(ConfigureRenameArgs),
    /// Set the remote URL of a repository.
    Remote(ConfigureRemoteArgs),
    /// List the configured repositories.
    List,
}

#[derive(Debug, Args)]
pub struct ConfigureAddArgs {
    /// Directory relative to the project root.
    pub path: PathBuf,
    /// Logical identifier (defaults to the directory name).
    #[arg(long)]
    pub id: Option<String>,
    /// Remote URL for this repository.
    #[arg(long)]
    pub remote: Option<String>,
    /// Create a Git repository in the directory if it is not one yet.
    #[arg(long)]
    pub git_init: bool,
    /// Branch hint recorded in the manifest.
    #[arg(long)]
    pub branch: Option<String>,
    /// Report what would change without writing the manifest.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ConfigureRemoveArgs {
    /// Logical identifier of the repository.
    pub id: String,
    /// Report what would change without writing the manifest.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ConfigureRenameArgs {
    pub id: String,
    pub new_id: String,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ConfigureRemoteArgs {
    pub id: String,
    /// Remote URL; omit with --clear to remove it.
    #[arg(long, default_value = "")]
    pub url: String,
    /// Remove the configured remote URL.
    #[arg(long)]
    pub clear: bool,
    /// Also run `git remote add/set-url origin` in that repository.
    #[arg(long)]
    pub set_git_remote: bool,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    /// Show one line per repository instead of the full change list.
    #[arg(long, short = 's')]
    pub short: bool,
    /// List every change with its owning repository.
    #[arg(long)]
    pub changes: bool,
}

#[derive(Debug, Args)]
pub struct CommitArgs {
    /// Logical commit message used in every affected repository.
    #[arg(long, short = 'm')]
    pub message: String,
    /// Only these repositories (repeatable).
    #[arg(long = "repo", value_name = "ID")]
    pub repositories: Vec<String>,
    /// Only repositories inside this subtree.
    #[arg(long, value_name = "PATH")]
    pub path: Option<PathBuf>,
    /// Show what would be committed without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct BranchArgs {
    #[command(subcommand)]
    pub action: Option<BranchCommand>,
    /// Report what would happen without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub enum BranchCommand {
    /// Create a branch in every repository.
    Create { name: String },
    /// Delete a branch in every repository where it is safe.
    Delete {
        name: String,
        /// Delete even when the branch is not merged.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub struct CheckoutArgs {
    /// Branch to switch to.
    pub name: String,
    /// Create the branch where it does not exist.
    #[arg(long)]
    pub create: bool,
    /// Report what would happen without changing anything.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct MergeArgs {
    /// Branch to merge into the current branch.
    pub name: String,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub dry_run: bool,
    /// Only these repositories (repeatable).
    #[arg(long = "repo", value_name = "ID")]
    pub repositories: Vec<String>,
}

#[derive(Debug, Args)]
pub struct PullArgs {
    #[arg(long)]
    pub dry_run: bool,
    /// Only these repositories (repeatable).
    #[arg(long = "repo", value_name = "ID")]
    pub repositories: Vec<String>,
    /// How to integrate upstream commits.
    #[arg(long, value_enum, default_value_t = StrategyArg::FfOnly)]
    pub strategy: StrategyArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StrategyArg {
    /// Fast-forward only; refuses when the branches diverged.
    FfOnly,
    /// Create a merge commit when needed.
    Merge,
    /// Replay local commits on top of upstream.
    Rebase,
}

impl From<StrategyArg> for PullStrategy {
    fn from(value: StrategyArg) -> Self {
        match value {
            StrategyArg::FfOnly => PullStrategy::FastForwardOnly,
            StrategyArg::Merge => PullStrategy::Merge,
            StrategyArg::Rebase => PullStrategy::Rebase,
        }
    }
}

#[derive(Debug, Args)]
pub struct PushArgs {
    #[arg(long)]
    pub dry_run: bool,
    /// Only these repositories (repeatable).
    #[arg(long = "repo", value_name = "ID")]
    pub repositories: Vec<String>,
    /// Do not set the upstream automatically when a repository has none.
    #[arg(long)]
    pub no_set_upstream: bool,
}

#[derive(Debug, Args)]
pub struct UiArgs {
    /// Directory to open (defaults to the current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Start in dry-run mode: operations are simulated.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct GuiArgs {
    /// Directory to open (defaults to the current directory).
    #[arg(default_value = ".")]
    pub path: PathBuf,
    /// Port of the local interface.
    #[arg(long, default_value_t = 7345)]
    pub port: u16,
    /// Address to bind. 127.0.0.1 keeps the interface local; 0.0.0.0 exposes it
    /// on the network (there is no authentication).
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    /// Also accept requests addressed to this host name. Needed when the interface
    /// is reached through a proxy or a port forward; repeat for several names.
    #[arg(long = "allow-host", value_name = "NAME")]
    pub allow_hosts: Vec<String>,
    /// Try to open the interface in a browser.
    #[arg(long)]
    pub open: bool,
    /// Start in dry-run mode: every operation is simulated.
    #[arg(long)]
    pub dry_run: bool,
}

// ------------------------------------------------------------------- helpers --

impl SyncArgs {
    pub fn sync_options(&self) -> SyncOptions {
        SyncOptions {
            selection: RepositorySelection::from_ids(self.repositories.clone()),
            strategy: PullStrategy::default(),
            dry_run: self.dry_run,
            prune: true,
        }
    }
}

impl PullArgs {
    pub fn sync_options(&self) -> SyncOptions {
        SyncOptions {
            selection: RepositorySelection::from_ids(self.repositories.clone()),
            strategy: self.strategy.into(),
            dry_run: self.dry_run,
            prune: true,
        }
    }
}

impl BranchArgs {
    /// Translate CLI arguments into a branch action.
    pub fn action(&self) -> BranchAction {
        match &self.action {
            None => BranchAction::Show,
            Some(BranchCommand::Create { name }) => BranchAction::Create { name: name.clone() },
            Some(BranchCommand::Delete { name, .. }) => BranchAction::Delete { name: name.clone() },
        }
    }

    pub fn force(&self) -> bool {
        matches!(
            &self.action,
            Some(BranchCommand::Delete { force: true, .. })
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_the_documented_workflow() {
        let cli = Cli::try_parse_from(["gitmesh", "commit", "-m", "message", "--repo", "engine"])
            .unwrap();
        let Some(Command::Commit(args)) = cli.command else {
            panic!("expected commit")
        };
        assert_eq!(args.message, "message");
        assert_eq!(args.repositories, vec!["engine"]);
    }

    #[test]
    fn global_flags_work_before_and_after_the_subcommand() {
        let cli = Cli::try_parse_from(["gitmesh", "--json", "status"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["gitmesh", "status", "--json"]).unwrap();
        assert!(cli.json);
        let cli = Cli::try_parse_from(["gitmesh", "-C", "/tmp/x", "status"]).unwrap();
        assert_eq!(cli.project, Some(PathBuf::from("/tmp/x")));
    }

    #[test]
    fn branch_arguments_map_to_actions() {
        let cli = Cli::try_parse_from(["gitmesh", "branch"]).unwrap();
        let Some(Command::Branch(args)) = cli.command else {
            panic!()
        };
        assert_eq!(args.action(), BranchAction::Show);

        let cli = Cli::try_parse_from(["gitmesh", "branch", "create", "feature/x"]).unwrap();
        let Some(Command::Branch(args)) = cli.command else {
            panic!()
        };
        assert_eq!(
            args.action(),
            BranchAction::Create {
                name: "feature/x".into()
            }
        );

        let cli = Cli::try_parse_from(["gitmesh", "branch", "delete", "old", "--force"]).unwrap();
        let Some(Command::Branch(args)) = cli.command else {
            panic!()
        };
        assert!(args.force());
    }

    #[test]
    fn pull_strategy_is_parsed() {
        let cli = Cli::try_parse_from(["gitmesh", "pull", "--strategy", "merge"]).unwrap();
        let Some(Command::Pull(args)) = cli.command else {
            panic!()
        };
        assert_eq!(args.sync_options().strategy, PullStrategy::Merge);
        let cli = Cli::try_parse_from(["gitmesh", "pull"]).unwrap();
        let Some(Command::Pull(args)) = cli.command else {
            panic!()
        };
        assert_eq!(args.sync_options().strategy, PullStrategy::FastForwardOnly);
    }

    #[test]
    fn tui_alias_is_accepted() {
        let cli = Cli::try_parse_from(["gitmesh", "tui"]).unwrap();
        assert!(matches!(cli.command, Some(Command::Ui(_))));
    }

    #[test]
    fn commit_requires_a_message() {
        assert!(Cli::try_parse_from(["gitmesh", "commit"]).is_err());
    }
}
