//! One module per CLI command; the dispatcher in `main.rs` calls into them.

pub mod brief;
pub mod close;
pub mod enter;
pub mod events;
pub mod fmt;
pub mod hook;
pub mod hook_capture;
pub mod kill;
pub mod list;
pub mod name;
pub mod new;
pub mod peek;
pub mod rename;
pub mod reset;
pub mod resume;
pub mod rm;
pub mod send;
pub mod sessions;
pub mod set;
pub mod show;
pub mod spawn;
pub mod target;

// Agent self-report (bd-8lz.2.5).
pub mod link;
pub mod note;
pub mod phase;
pub mod report;

use std::io::{BufRead, IsTerminal, Write};

use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::beads::{self, Progress};
use crate::error::QError;
use crate::model::{
    DisplayState, Quest, Session, SessionStatus, display_state, master_ctx_pct, needs_you,
};
use crate::tmux;

/// A Quest with the three fields SPEC §4 derives rather than stores. `q list`
/// flattens it; `q show` nests the Quest itself.
///
/// `Deserialize` because this *is* the wire format between machines: a remote's
/// `q list --json` is read straight back into these (SPEC §15), so local and
/// remote rows need no translation. The optional fields default so a remote on
/// an older `q` that never learned to report one still parses.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuestView {
    #[serde(flatten)]
    pub quest: Quest,
    pub display_state: DisplayState,
    pub needs_you: bool,
    pub live_sessions: usize,
    /// The live master's context reading (SPEC §8); `null` when there is no
    /// live master, or the statusline hook has never reported one.
    #[serde(default)]
    pub master_ctx_pct: Option<u8>,
    /// Beads counts (SPEC §13); `null` without an epic, or when `bd` has never
    /// answered for this Quest.
    #[serde(default)]
    pub progress: Option<Progress>,
}

impl QuestView {
    pub fn new(quest: Quest, sessions: &[Session]) -> QuestView {
        QuestView {
            display_state: display_state(&quest, sessions),
            needs_you: needs_you(sessions),
            live_sessions: live(sessions).count(),
            master_ctx_pct: master_ctx_pct(sessions),
            progress: None,
            quest,
        }
    }

    pub fn with_progress(mut self, progress: Option<Progress>) -> QuestView {
        self.progress = progress;
        self
    }

    /// `3/7`, or `-` when there is nothing to report.
    pub fn progress_cell(&self) -> String {
        self.progress
            .map(|p| p.cell())
            .unwrap_or_else(|| "-".to_string())
    }

    /// `active` / `idle · needs you` — the state column of a listing.
    pub fn state_cell(&self) -> String {
        if self.needs_you {
            format!("{} · needs you", self.display_state)
        } else {
            self.display_state.to_string()
        }
    }
}

pub fn live(sessions: &[Session]) -> impl Iterator<Item = &Session> {
    sessions.iter().filter(|s| s.status != SessionStatus::Ended)
}

/// A Quest's derived view together with the sessions it was derived from.
/// `q list` only needs the view; the TUI's Quests tab also renders the
/// sessions, so the one loader hands back both rather than querying twice.
pub struct QuestRow {
    pub view: QuestView,
    pub sessions: Vec<Session>,
}

/// Every Quest this `q` speaks for, swept, machine-filtered and ranked.
///
/// The single definition of "the Quest listing": `q list` and the TUI's Quests
/// tab both come through here, so they can never disagree about which Quests
/// exist, what state they are in, or what order they belong in.
pub fn load_quests(ctx: &Ctx, include_finished: bool) -> anyhow::Result<Vec<QuestRow>> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let mut rows: Vec<QuestRow> = Vec::new();
    for quest in db.list_quests(include_finished)? {
        // TODO(M4): a remote machine's Quests come over ssh, not out of this db.
        if ctx.machine_filter().is_some_and(|m| m != quest.machine) {
            continue;
        }
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        rows.push(QuestRow {
            view: QuestView::new(quest, &sessions),
            sessions,
        });
    }
    sort_quests(&mut rows);
    Ok(rows)
}

/// One `bd` call for the whole listing, capped and cache-backed, so a slow or
/// missing `bd` can never hold up a listing or a TUI tick (SPEC §13).
pub fn fill_progress(ctx: &Ctx, rows: &mut [QuestRow]) {
    let quests: Vec<&Quest> = rows.iter().map(|r| &r.view.quest).collect();
    let progress = beads::progress_all_with(ctx.bd(), &quests);
    for row in rows.iter_mut() {
        row.view.progress = progress.get(&row.view.quest.id).copied();
    }
}

/// What needs the human first, then what is running, then the rest; ties go to
/// the most recently touched Quest.
pub fn sort_quests(rows: &mut [QuestRow]) {
    rows.sort_by(|a, b| {
        rank(&a.view)
            .cmp(&rank(&b.view))
            .then(b.view.quest.updated_at.cmp(&a.view.quest.updated_at))
    });
}

/// The group a Quest belongs to, and the order the groups are shown in
/// (SPEC §17: needs-you on top, then active, then idle, finished last).
pub fn rank(view: &QuestView) -> u8 {
    if view.needs_you {
        return 0;
    }
    match view.display_state {
        DisplayState::Active => 1,
        DisplayState::Idle => 2,
        DisplayState::Finished => 3,
    }
}

/// The pane's own process id, when tmux still has that pane. `None` is an
/// ordinary answer: the pane may be gone, or tmux may not be running.
pub fn pane_pid(ctx: &Ctx, pane_id: &str) -> Option<i64> {
    ctx.tmux()
        .list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
        .map(|p| i64::from(p.pane_pid))
}

/// The liveness sweep every command that reads Quests runs first (SPEC §6).
/// Silent: what it changed shows up in the listing it precedes.
pub fn sweep_quiet(ctx: &Ctx) -> anyhow::Result<()> {
    tmux::sweep(ctx.db()?, ctx.tmux())?;
    Ok(())
}

/// Write out and clear whatever the call buffered on the `Ctx` (see
/// [`Ctx::warn`]). Every command that can warn calls this before its own
/// output, so the order on the terminal is exactly what it was when these were
/// `eprintln!`s inside the library — and the TUI, which drains the same buffer
/// into its status bar, never sees a byte reach the screen behind its back.
pub fn flush_warnings(ctx: &Ctx) {
    for warning in ctx.take_warnings() {
        eprintln!("{warning}");
    }
}

/// `[y/N]` on a terminal, asked on stderr so it never pollutes the payload.
/// Anything else — a plain "no", a pipe, a closed stdin — refuses, and `-f` is
/// the only way past it.
///
/// `--json` and `$Q_QUEST` (an agent running inside a Quest pane) refuse
/// without asking at all: nobody is there to answer, and a blocked agent is
/// worse than a failed command.
pub fn confirm(ctx: &Ctx, question: &str) -> anyhow::Result<()> {
    if ctx.json || in_quest_pane() || !std::io::stdin().is_terminal() {
        return Err(aborted());
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => Err(aborted()),
    }
}

fn in_quest_pane() -> bool {
    std::env::var_os("Q_QUEST").is_some_and(|v| !v.is_empty())
}

/// What a command did with the terminal, for the payload and the one-liner.
/// `Switch` and `Exec` move a client between sessions; `Select` only changes
/// which window of the caller's own session is active, which is all `q spawn`
/// ever does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachMode {
    /// `-d` / `--no-attach`: the terminal is left alone.
    None,
    Select,
    Switch,
    Exec,
}

impl std::fmt::Display for AttachMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AttachMode::None => "none",
            AttachMode::Select => "select",
            AttachMode::Switch => "switch",
            AttachMode::Exec => "exec",
        })
    }
}

/// What an attach will do: inside tmux the client switches, outside it the
/// process is replaced by `tmux attach`. The one helper every attaching
/// command reports through.
pub fn attach_mode(ctx: &Ctx, attaching: bool) -> AttachMode {
    match (attaching, ctx.tmux().in_tmux()) {
        (false, _) => AttachMode::None,
        (true, true) => AttachMode::Switch,
        (true, false) => AttachMode::Exec,
    }
}

fn aborted() -> anyhow::Error {
    QError::Other("aborted (use -f)".to_string()).into()
}
