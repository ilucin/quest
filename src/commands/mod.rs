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

use serde::Serialize;

use crate::Ctx;
use crate::beads::Progress;
use crate::error::QError;
use crate::model::{DisplayState, Quest, Session, SessionStatus, display_state, needs_you};
use crate::tmux;

/// A Quest with the three fields SPEC §4 derives rather than stores. `q list`
/// flattens it; `q show` nests the Quest itself.
#[derive(Debug, Serialize)]
pub struct QuestView {
    #[serde(flatten)]
    pub quest: Quest,
    pub display_state: DisplayState,
    pub needs_you: bool,
    pub live_sessions: usize,
    /// Beads counts (SPEC §13); `null` without an epic, or when `bd` has never
    /// answered for this Quest.
    pub progress: Option<Progress>,
}

impl QuestView {
    pub fn new(quest: Quest, sessions: &[Session]) -> QuestView {
        QuestView {
            display_state: display_state(&quest, sessions),
            needs_you: needs_you(sessions),
            live_sessions: live(sessions).count(),
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
