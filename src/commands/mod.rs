//! One module per CLI command; the dispatcher in `main.rs` calls into them.

pub mod close;
pub mod enter;
pub mod fmt;
pub mod hook;
pub mod list;
pub mod new;
pub mod rename;
pub mod resume;
pub mod rm;
pub mod set;
pub mod show;

use std::io::{BufRead, IsTerminal, Write};

use serde::Serialize;

use crate::Ctx;
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
}

impl QuestView {
    pub fn new(quest: Quest, sessions: &[Session]) -> QuestView {
        QuestView {
            display_state: display_state(&quest, sessions),
            needs_you: needs_you(sessions),
            live_sessions: live(sessions).count(),
            quest,
        }
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

/// The attach mode that leaves the terminal alone (`-d`).
pub const NONE: &str = "none";

/// What an attach will do, for the payload: inside tmux the client switches,
/// outside it the process is replaced by `tmux attach`. The one helper every
/// attaching command reports through.
pub fn attach_mode(ctx: &Ctx, attaching: bool) -> &'static str {
    match (attaching, ctx.tmux().in_tmux()) {
        (false, _) => NONE,
        (true, true) => "switch",
        (true, false) => "exec",
    }
}

fn aborted() -> anyhow::Error {
    QError::Other("aborted (use -f)".to_string()).into()
}
