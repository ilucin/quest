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
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        goal: Option<String>,
        /// Working directory for the agents (default: the current one)
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,
        #[arg(long, value_name = "NAME")]
        workflow: Option<String>,
        /// `repo:<name>` label for the beads epic (default: the cwd's git root)
        #[arg(long, value_name = "NAME")]
        repo: Option<String>,
        /// Do not create a beads epic for this Quest
        #[arg(long)]
        no_beads: bool,
        /// First prompt for the master
        #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
        prompt: Option<String>,
        /// First prompt for the master, from a file (`-` reads stdin)
        #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
        prompt_file: Option<String>,
        /// Never auto-reset this Quest's master at the context threshold
        #[arg(long)]
        no_auto_reset: bool,
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
        /// Also close the Quest's beads epic
        #[arg(long)]
        close_epic: bool,
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

    /// Spawn a worker agent in a new window of the Quest's tmux session
    Spawn {
        quest: String,
        /// First prompt for the worker; a leading `-` is text, not a flag
        #[arg(allow_hyphen_values = true)]
        prompt: String,
        /// Session label: lowercase kebab-case, unique among live sessions
        #[arg(long, value_name = "LABEL")]
        label: String,
        /// Workflow for this worker (default: the Quest's)
        #[arg(long, value_name = "NAME")]
        workflow: Option<String>,
        /// Working directory (default: the Quest's)
        #[arg(long, value_name = "PATH")]
        dir: Option<String>,
        /// Do not select the new window (already the case outside the Quest's
        /// own tmux session, where there is no client of ours to move)
        #[arg(long)]
        no_attach: bool,
    },

    /// List sessions: one Quest's, or every live one across active Quests
    Sessions {
        /// Defaults to every active Quest
        quest: Option<String>,
        /// Include ended sessions and finished Quests
        #[arg(long)]
        all: bool,
    },

    /// Print what a session's pane currently shows
    Peek {
        /// `<quest>/<label>`, a session id, or `<label>` inside a Quest
        session: String,
        /// How many trailing lines to capture
        #[arg(long, value_name = "N", default_value_t = crate::commands::peek::DEFAULT_LINES)]
        lines: usize,
    },

    /// Type a line into a session, if it is idle
    Send {
        /// `<quest>/<label>`, a session id, or `<label>` inside a Quest
        session: String,
        /// Free text; a leading `-` is text, not a flag
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Send even though the session is busy, waiting or starting
        #[arg(long)]
        force: bool,
    },

    /// Hand a session a fresh context window: `/clear` (plus a follow-up
    /// prompt) or `/compact <goal>`
    Reset {
        /// `<quest>/<label>`, a session id, or `<label>` inside a Quest
        session: String,
        /// Seconds to wait first; also marks the scheduled path, where a
        /// session that turned out busy is a skip rather than an error
        #[arg(long, value_name = "N")]
        delay: Option<u64>,
        /// Defaults to `[context] reset_strategy`
        #[arg(long, value_enum, value_name = "STRATEGY")]
        strategy: Option<ResetStrategy>,
    },

    /// Kill a worker session's tmux window and end its row
    Kill {
        /// `<quest>/<label>`, a session id, or `<label>` inside a Quest
        session: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// Rename a Quest's slug
    Rename { quest: String, slug: String },

    /// Propose (and optionally apply) an auto-generated slug (SPEC §10)
    Name {
        quest: String,
        /// Ask `claude -p` for a slug, falling back to a heuristic
        #[arg(long)]
        auto: bool,
        /// Rename the Quest to the proposal instead of only printing it
        #[arg(long, requires = "auto")]
        apply: bool,
        /// Ignore any cached proposal for the same input
        #[arg(long, requires = "auto")]
        refresh: bool,
        /// Re-run this command in the background and return immediately
        #[arg(long, requires = "auto")]
        detach: bool,
        /// Let auto-naming take over a Quest that was named by hand
        #[arg(long, requires = "apply")]
        force: bool,
    },

    /// Set a Quest property
    Set {
        quest: String,
        #[arg(value_enum)]
        key: SetKey,
        /// Free text for `goal`; a leading `-` is text, not a flag
        #[arg(allow_hyphen_values = true)]
        value: String,
    },

    /// Delete a Quest
    Rm {
        quest: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// The Quest brief: deterministic markdown from the database (SPEC §9)
    Brief {
        /// Defaults to $Q_QUEST
        quest: Option<String>,
        /// Whose instructions to include; defaults to $Q_ROLE, else master
        #[arg(long, value_enum, value_name = "ROLE")]
        r#for: Option<Role>,
        /// The session (id or label) the brief is for; defaults to $Q_SESSION
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
    },

    /// A Quest's event log, oldest first; `--follow` tails it (SPEC §16)
    Events {
        /// Defaults to $Q_QUEST
        quest: Option<String>,
        /// Keep polling and print new events as they arrive
        #[arg(short = 'f', long)]
        follow: bool,
        /// Only these kinds; exact (`note`) or prefix glob (`session.*`), repeatable
        #[arg(short = 'k', long = "kind", value_name = "KIND")]
        kinds: Vec<String>,
        /// How many of the most recent events to show first
        #[arg(short = 'n', long = "limit", value_name = "N", default_value_t = 50)]
        limit: usize,
        /// Only events of this session (id or label)
        #[arg(long, value_name = "SESSION")]
        session: Option<String>,
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

    // Agent self-report (SPEC §7, §12): quest from $Q_QUEST, session from
    // $Q_SESSION unless overridden.
    /// Report what this session is doing now (requires $Q_SESSION)
    Phase {
        text: String,
        #[arg(long, value_name = "QUEST")]
        quest: Option<String>,
    },

    /// Leave a note on the Quest's timeline
    Note {
        /// Free text; a leading `-` is text, not a flag
        #[arg(allow_hyphen_values = true)]
        text: String,
        /// Mark the note as a blocker the master must resolve
        #[arg(long)]
        blocker: bool,
        #[arg(long, value_name = "QUEST")]
        quest: Option<String>,
    },

    /// Attach a reference (PR, task, worktree, URL, ...) to the Quest
    Link {
        #[command(subcommand)]
        action: LinkAction,
    },

    /// List a Quest's links grouped by kind
    Links {
        quest: Option<String>,
        /// Re-fetch enrichment (reserved: enrichment lands in a later milestone)
        #[arg(long, hide = true)]
        refresh: bool,
    },

    /// Attach a produced file to the Quest
    Artifact {
        #[command(subcommand)]
        action: ArtifactAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum LinkAction {
    /// Add a link; the kind is detected from the reference unless given
    Add {
        r#ref: String,
        #[arg(long, value_enum)]
        kind: Option<LinkKind>,
        #[arg(long, value_name = "TEXT")]
        title: Option<String>,
        #[arg(long, value_name = "QUEST")]
        quest: Option<String>,
    },
    /// Remove a link by id
    Rm {
        id: i64,
        #[arg(long, value_name = "QUEST")]
        quest: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ArtifactAction {
    /// Add a file (stored by absolute path)
    Add {
        path: String,
        #[arg(long, value_name = "TEXT")]
        note: Option<String>,
        #[arg(long, value_name = "QUEST")]
        quest: Option<String>,
    },
}

/// `link.kind` (SPEC §4).
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[value(rename_all = "lowercase")]
pub enum LinkKind {
    Pr,
    Task,
    Worktree,
    Url,
    Branch,
    Beads,
    Brain,
    Artifact,
}

impl LinkKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Pr => "pr",
            LinkKind::Task => "task",
            LinkKind::Worktree => "worktree",
            LinkKind::Url => "url",
            LinkKind::Branch => "branch",
            LinkKind::Beads => "beads",
            LinkKind::Brain => "brain",
            LinkKind::Artifact => "artifact",
        }
    }
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

/// TODO(M6): `brain` waits on the brain integration, so it is not offered yet.
#[derive(ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "snake_case")]
pub enum SetKey {
    Goal,
    Cwd,
    Workflow,
    AutoReset,
    CtxResetPct,
    BeadsEpic,
    BeadsRepo,
}

/// `[context] reset_strategy`, as `q reset --strategy` spells it.
#[derive(ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "lowercase")]
pub enum ResetStrategy {
    Clear,
    Compact,
}

impl From<ResetStrategy> for crate::commands::reset::Strategy {
    fn from(strategy: ResetStrategy) -> Self {
        match strategy {
            ResetStrategy::Clear => Self::Clear,
            ResetStrategy::Compact => Self::Compact,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
pub enum Role {
    Master,
    Worker,
}

impl From<Role> for crate::model::SessionRole {
    fn from(role: Role) -> Self {
        match role {
            Role::Master => Self::Master,
            Role::Worker => Self::Worker,
        }
    }
}
