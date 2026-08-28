use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(
    name = "q",
    // Not clap's plain `version`: the far end of an ssh reads this line to
    // decide whether the two `q`s speak the same remote protocol (SPEC §19,
    // `q doctor`), so it carries the wire version as well as the crate one.
    version = crate::remote::VERSION,
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

    /// Stay on this machine: never reach out over ssh (SPEC §15). Set on the
    /// commands `q` proxies to a remote, so the recursion stops there.
    #[arg(long, global = true)]
    pub no_remote: bool,

    /// This command's confirmation has already been answered, by a human, on
    /// the machine that had the terminal (SPEC §15's proxy).
    ///
    /// It skips the `[y/N]` and **nothing else**. `-f` on `q rm` means two
    /// things — don't ask, *and* kill a tmux session that is still running —
    /// and a proxied command must not be able to buy the second one with an
    /// answer to the first. Hidden because it is the wire's word, not a
    /// spelling anyone needs to type.
    #[arg(long, global = true, hide = true)]
    pub confirmed: bool,

    /// The Quest this command was resolved — and confirmed — against, on the
    /// machine that had the terminal (SPEC §15's proxy): `<id>.<created_at>`.
    ///
    /// A Quest id is 16 bits and is freed when the Quest is deleted, so it can
    /// be drawn again by a later `q new`: the id alone does not say *which*
    /// Quest, only which row is there now. The creation time is what tells a
    /// reused id apart, and it is immutable, so a rename between the two
    /// resolutions is still the same Quest and still goes through. If this does
    /// not name the Quest the target resolves to here, the command refuses
    /// rather than acting on the wrong one. Hidden because it is the wire's
    /// word, not a spelling anyone needs to type.
    #[arg(long, global = true, hide = true, value_name = "IDENTITY")]
    pub expect: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create a new Quest
    ///
    /// `--template` starts from a stored definition (SPEC §11) and every
    /// other flag wins over it: the template fills only what was left out.
    /// Without `--name` the Quest is named after the template, stepping
    /// aside to `-2`, `-3`, … as a routine is run again. To take a
    /// definition whole, with `{{arg.k}}` and no overrides, use
    /// `q tpl run` instead.
    New {
        /// Slug: lowercase kebab-case, at most 40 characters
        #[arg(long, value_name = "SLUG")]
        name: Option<String>,
        /// Template to start from: name, id, or an unambiguous fragment
        #[arg(long, value_name = "NAME")]
        template: Option<String>,
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

    /// The embedded agent SKILL.md: install into ~/.claude/skills/q, or check
    Skill {
        #[command(subcommand)]
        action: SkillAction,
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
        /// Free text; a leading `-` is text, not a flag. Optional with `--resolve`.
        #[arg(allow_hyphen_values = true)]
        text: Option<String>,
        /// Mark the note as a blocker the master must resolve
        #[arg(long)]
        blocker: bool,
        /// Resolve the blocker note with this event id, clearing it from brief §10
        #[arg(long, value_name = "EVENT_ID", conflicts_with = "blocker")]
        resolve: Option<i64>,
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
        /// Force-refresh enrichment, ignoring the 5-minute cache
        #[arg(long)]
        refresh: bool,
    },

    /// Attach a produced file to the Quest
    Artifact {
        #[command(subcommand)]
        action: ArtifactAction,
    },

    /// Quest templates: reusable Quest definitions (SPEC §11)
    ///
    /// Templates are rows in this machine's database, so no `q tpl`
    /// subcommand is ever proxied and a global `--machine <other>` is
    /// refused rather than ignored; move a definition with
    /// `q tpl export <name> | ssh <alias> q tpl import -`.
    Tpl {
        #[command(subcommand)]
        action: TplAction,
    },

    /// Workflows: the markdown prompt that tells a master how to work (SPEC §11)
    ///
    /// Workflows are files, not rows: five are built into the binary
    /// (`orchestrator`, `solo`, `review`, `research`, `routine`) and the rest
    /// live in `<config dir>/workflows/<name>.md`, where a file shadows the
    /// built-in of the same name. The whole file goes into the master's brief;
    /// a worker gets the file's `## worker` section when it defines one.
    ///
    /// Every subcommand but `set` is about files on *this* machine, so a
    /// global `--machine <other>` is refused rather than ignored.
    /// `q workflow set` targets a Quest and travels like `q set` does.
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorkflowAction {
    /// List every workflow, saying which are built in and which are yours
    List,
    /// Print a workflow's markdown
    Show {
        /// Name of the workflow to print (built-in or one of yours)
        name: String,
        /// Only the `## worker` section, as a worker's brief would get it
        #[arg(long)]
        worker: bool,
    },
    /// Create a workflow: with no `--file`, its buffer opens in $EDITOR
    ///
    /// `--file <path>` reads the body from a file (`-` reads stdin) instead.
    /// The name follows the slug grammar — it becomes `<name>.md`.
    Add {
        /// Name: lowercase kebab-case, at most 40 characters
        name: String,
        /// Read the body from a file instead of opening an editor (`-` is stdin)
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
    },
    /// Change a workflow; editing a built-in copies it to your config first
    ///
    /// The copy is what shadows the built-in from then on. `q workflow rm`
    /// deletes the copy and the built-in comes back, so a built-in is never
    /// lost by editing it.
    Edit {
        /// Name of the workflow to change (a built-in is copied to your config first)
        name: String,
        /// Replace the body from a file instead of opening an editor (`-` is stdin)
        #[arg(long, value_name = "PATH")]
        file: Option<String>,
    },
    /// Delete one of your workflow files; a built-in it shadowed comes back
    Rm {
        /// Name of the workflow file to delete (a built-in cannot be removed)
        name: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Set a Quest's workflow (SPEC §11: the master may change its own)
    ///
    /// The same write as `q set <quest> workflow <name>`, with the same
    /// `quest.updated` event — spelled the way SPEC §11 spells it. A blank
    /// name clears the Quest's workflow.
    Set {
        /// Quest to change: slug, id, or an unambiguous fragment
        quest: String,
        /// Workflow to set; a blank name clears it
        name: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum TplAction {
    /// List templates
    List,
    /// Show one template
    Show {
        /// Name, id, or an unambiguous fragment
        name: String,
    },
    /// Create a template
    ///
    /// `--goal` and `--prompt` may use `{{date}}` (today, local, YYYY-MM-DD)
    /// and `{{arg.<key>}}`, which `q tpl run --arg key=value` fills in. Any
    /// other `{{...}}` is rejected here rather than at run time.
    Add {
        /// Name: lowercase kebab-case, at most 40 characters
        name: String,
        #[command(flatten)]
        fields: TplFields,
    },
    /// Change a template: with no field flag, its TOML opens in $EDITOR
    ///
    /// With field flags it is a plain patch: a flag that is not given leaves
    /// its field alone, and a blank one clears it. Without them the template's
    /// whole definition is rendered as TOML into $Q_EDITOR / $VISUAL /
    /// $EDITOR (else `vi`) and read back — so a field the saved file leaves
    /// out is cleared, and `name` may be changed to rename the template.
    Edit {
        /// Name, id, or an unambiguous fragment
        name: String,
        #[command(flatten)]
        fields: TplFields,
    },
    /// Delete a template; Quests made from it are unlinked, never removed
    Rm {
        /// Name, id, or an unambiguous fragment
        name: String,
        /// Do not ask for confirmation
        #[arg(short, long)]
        force: bool,
    },
    /// Create a Quest from a template
    ///
    /// The template's goal and master prompt are expanded first: `{{date}}` is
    /// today's local date (YYYY-MM-DD) and `{{arg.<key>}}` comes from `--arg`.
    /// A placeholder nothing fills is an error naming every missing key — the
    /// Quest is not created and the run is not counted — rather than a prompt
    /// handed to an agent with the braces still in it.
    ///
    /// The template's `cwd` has to be a directory on this machine at this
    /// point (it is not checked before then, so a definition can travel
    /// ahead of its repository); a template with no `cwd` uses the current
    /// directory. Everything else is `q new`, `-d` included, and the Quest is
    /// named after the template.
    Run {
        /// Name, id, or an unambiguous fragment
        name: String,
        /// `k=v` for a `{{arg.k}}` in the goal or the prompt; repeatable
        #[arg(long = "arg", value_name = "K=V", allow_hyphen_values = true)]
        args: Vec<String>,
        /// Do not attach after creating
        #[arg(short = 'd', long)]
        detach: bool,
    },
    /// Print templates as TOML; no name prints every one
    Export {
        /// Name, id, or an unambiguous fragment
        name: Option<String>,
    },
    /// Read templates from a TOML file (`-` reads stdin)
    ///
    /// All or nothing: one bad or already-taken name leaves the database
    /// untouched. `--replace` overwrites a template's definition in place and
    /// keeps its run count and last run, which are this machine's history
    /// rather than part of the definition — and are why neither ever appears
    /// in a `q tpl export`.
    Import {
        path: String,
        /// Overwrite a template that already has the name, keeping its run
        /// count and last run
        #[arg(long)]
        replace: bool,
    },
    /// Build a template out of an existing Quest
    From {
        quest: String,
        /// Name for the new template
        name: String,
    },
}

/// The fields `q tpl add` sets and `q tpl edit` patches. Blank clears a field
/// (`--goal ""` stores NULL); an omitted flag leaves it as it was.
///
/// Placeholders: `goal` and `--prompt` support `{{date}}` (today, local, ISO)
/// and `{{arg.<key>}}`, filled by `q tpl run --arg key=value`. A run whose
/// template still has an unfilled `{{arg.…}}`, or any other `{{…}}`, fails and
/// names the keys rather than passing the braces on to an agent.
#[derive(clap::Args, Debug, Default)]
pub struct TplFields {
    /// One line on what this template is for
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub description: Option<String>,
    /// Working directory for the Quest (blank: whatever `q tpl run`'s is)
    #[arg(long, value_name = "PATH")]
    pub cwd: Option<String>,
    /// Workflow for the Quest
    #[arg(long, value_name = "NAME")]
    pub workflow: Option<String>,
    /// Goal for the Quest; supports {{date}} and {{arg.k}}
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub goal: Option<String>,
    /// First prompt for the master; supports {{date}} and {{arg.k}}
    #[arg(long, value_name = "TEXT", allow_hyphen_values = true)]
    pub prompt: Option<String>,
    /// The same, from a file (`-` reads stdin)
    #[arg(long, value_name = "PATH", conflicts_with = "prompt")]
    pub prompt_file: Option<String>,
    /// `repo:<name>` label for the Quest's beads epic
    #[arg(long, value_name = "NAME")]
    pub repo: Option<String>,
    /// Record that Quests from this template want a brain session
    ///
    /// Stored, shown and exported; nothing creates the session yet — `q new`
    /// has no `--brain` either.
    #[arg(long)]
    pub brain: bool,
    /// The opposite, for `q tpl edit`
    #[arg(long, conflicts_with = "brain")]
    pub no_brain: bool,
    /// Tag, repeatable; giving any replaces the whole set, `--tag ""` clears it
    #[arg(long = "tag", value_name = "TAG")]
    pub tags: Vec<String>,
}

impl TplFields {
    /// Whether anything at all was given — what tells `q tpl edit` to patch
    /// rather than open an editor.
    pub fn any(&self) -> bool {
        self.description.is_some()
            || self.cwd.is_some()
            || self.workflow.is_some()
            || self.goal.is_some()
            || self.prompt.is_some()
            || self.prompt_file.is_some()
            || self.repo.is_some()
            || self.brain
            || self.no_brain
            || !self.tags.is_empty()
    }
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
pub enum SkillAction {
    /// Write the embedded SKILL.md to ~/.claude/skills/q/SKILL.md
    Install,
    /// Remove q's skill from ~/.claude/skills/q, touching nothing else
    Uninstall,
    /// Report whether the skill is installed, missing or out of date
    Status,
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
