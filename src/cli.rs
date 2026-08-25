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

/// Variant fields are wired up by the milestone that implements each command;
/// the stubs below do not read them yet.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new Quest
    New {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        goal: Option<String>,
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,
        #[arg(long)]
        workflow: Option<String>,
        #[arg(long)]
        template: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
        /// Do not attach after creating
        #[arg(short = 'd', long)]
        detach: bool,
    },

    /// List Quests
    List {
        /// Include finished Quests
        #[arg(long)]
        all: bool,
        #[arg(long, value_enum)]
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
        #[arg(short, long)]
        force: bool,
        #[arg(long)]
        summarize: bool,
    },

    /// Reopen a closed Quest
    Resume {
        quest: String,
        #[arg(long)]
        prompt: Option<String>,
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

#[derive(ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "snake_case")]
pub enum SetKey {
    Goal,
    Cwd,
    Workflow,
    AutoReset,
    CtxResetPct,
    Brain,
}

impl Command {
    /// Name used in "not implemented" errors and, later, in dispatch logging.
    pub fn name(&self) -> &'static str {
        match self {
            Command::New { .. } => "new",
            Command::List { .. } => "list",
            Command::Show { .. } => "show",
            Command::Enter { .. } => "enter",
            Command::Close { .. } => "close",
            Command::Resume { .. } => "resume",
            Command::Rename { .. } => "rename",
            Command::Set { .. } => "set",
            Command::Rm { .. } => "rm",
            Command::Doctor { .. } => "doctor",
            Command::Config { .. } => "config",
        }
    }
}
