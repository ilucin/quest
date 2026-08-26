use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "q",
    version,
    about = "Quest orchestrator for Claude Code agents",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Machine-readable output; errors go to stderr as {"error": "..."}
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress non-essential human output
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Operate against a specific machine from the remotes config
    #[arg(long, global = true, value_name = "NAME")]
    pub machine: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new Quest
    New {
        /// Slug: lowercase kebab-case, at most 40 characters
        #[arg(long, value_name = "SLUG")]
        name: Option<String>,
        /// One line on what this Quest is for
        #[arg(long, value_name = "TEXT")]
        goal: Option<String>,
        /// Working directory for the agents (default: the current one)
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,
        #[arg(long, value_name = "NAME")]
        workflow: Option<String>,
        /// First prompt for the master
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
        /// First prompt for the master, from a file (`-` reads stdin)
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Do not attach after creating
        #[arg(short = 'd', long)]
        detach: bool,
    },

    /// List Quests
    List {
        /// Include finished Quests
        #[arg(long)]
        all: bool,
        /// Only Quests in this derived state
        #[arg(long, value_enum, value_name = "STATE")]
        state: Option<QuestState>,
    },

    /// Show a Quest with its sessions and links
    Show { quest: String },

    /// Attach to a Quest's tmux session
    Enter {
        quest: String,
        #[arg(long, value_name = "LABEL")]
        session: Option<String>,
    },

    /// Close a Quest
    Close {
        quest: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// Reopen a closed Quest with a fresh master
    Resume {
        quest: String,
        /// First prompt for the new master
        #[arg(long, value_name = "TEXT")]
        prompt: Option<String>,
        /// Do not attach after resuming
        #[arg(short = 'd', long)]
        detach: bool,
    },

    /// Rename a Quest's slug
    Rename { quest: String, slug: String },

    /// Set a Quest property
    Set {
        quest: String,
        #[arg(value_enum)]
        key: SetKey,
        value: String,
    },

    /// Delete a Quest
    Rm {
        quest: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// Check the local environment
    Doctor {
        /// Repair what can be repaired
        #[arg(long)]
        fix: bool,
    },

    /// Read or edit the config
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },

    /// Claude Code hooks: install into settings.json, or run one (internal)
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum HookAction {
    /// Merge q's hooks and statusline into Claude Code's settings.json
    Install {
        /// Command to invoke q with (default: absolute path of this binary)
        #[arg(long, value_name = "CMD")]
        command: Option<String>,
    },
    /// Remove q's entries from settings.json, restoring the chained statusline
    Uninstall,
    /// Report which q entries are installed, missing or drifted
    Status {
        /// Compare against this command instead of this binary's path
        #[arg(long, value_name = "CMD")]
        command: Option<String>,
    },
    // Hook handlers Claude Code invokes; each reads the hook payload on stdin.
    #[command(hide = true)]
    SessionStart,
    #[command(hide = true)]
    UserPromptSubmit,
    #[command(hide = true)]
    Stop,
    #[command(hide = true)]
    Notification,
    #[command(hide = true)]
    PreCompact,
    #[command(hide = true)]
    SessionEnd,
    #[command(hide = true)]
    PostToolUse,
    #[command(hide = true)]
    Statusline,
}

#[derive(Subcommand, Debug)]
pub enum ConfigAction {
    /// Print one dotted key, or the whole effective config
    Get { key: Option<String> },
    /// Set one dotted key and rewrite the config file
    Set {
        key: String,
        #[arg(allow_hyphen_values = true)]
        value: String,
    },
    /// Open the config in $VISUAL/$EDITOR, then re-validate
    Edit,
    /// Print the config file path
    Path,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum QuestState {
    Active,
    Idle,
    Finished,
}

/// TODO(M2): `auto_reset` has no column of its own in SPEC §4, and `brain`
/// waits on the brain integration — neither is offered yet.
#[derive(ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "snake_case")]
pub enum SetKey {
    Goal,
    Cwd,
    Workflow,
    CtxResetPct,
}
