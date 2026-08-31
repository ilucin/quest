//! `q stop <session> [--force]` — type `/exit` into a session, idle-gated
//! (SPEC §6). Claude leaves the pane and the login shell remains; the tmux
//! session lives on. The row goes `off` on its own — Claude's `SessionEnd`
//! hook, or the next sweep seeing the shell.
//!
//! Idle-gated for the same reason `q send` is: `/exit` typed mid-turn is
//! swallowed, and typed at a permission prompt is read as the answer. `--force`
//! is the way past the gate.

use crate::Ctx;
use crate::commands::{sweep_quiet, target};
use crate::error::QError;
use crate::model::{Session, SessionStatus};
use crate::output;
use crate::registry::Verdict;

pub struct Args<'a> {
    pub session: &'a str,
    pub force: bool,
}

/// What a stop did, for the caller to report however it reports.
pub struct Stopped {
    pub session: Session,
    /// `<slug>/<label>` — what the session is called to the user.
    pub name: String,
    /// The gate's objection `force` overrode; `None` when the session was idle.
    pub forced_past: Option<String>,
    pub verdict: Verdict,
}

impl Stopped {
    pub fn describe(&self) -> String {
        match &self.forced_past {
            Some(reason) => format!("stopped {} (forced past: {reason})", self.name),
            None => format!("stopped {}", self.name),
        }
    }
}

/// Type `/exit`, refusing unless the session is between turns or `force` says
/// otherwise. Shared with the TUI's `X` (SPEC §17) so both go through one gate.
pub fn apply(ctx: &Ctx, found: &target::Target, force: bool) -> anyhow::Result<Stopped> {
    found.require_live()?;
    let session = found.session.clone();
    let name = found.name();
    // Nothing to exit: an `off` row has no Claude in it.
    if session.status == SessionStatus::Off {
        return Err(QError::Conflict(format!(
            "{name} is off (no Claude running); nothing to stop"
        ))
        .into());
    }

    let (verdict, refusal) = found.idle_gate(ctx);
    if let Some(reason) = &refusal
        && !force
    {
        let hint = verdict
            .hint()
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        return Err(QError::Conflict(format!(
            "{name} is not idle: {reason}{hint}. Pass --force to stop anyway"
        ))
        .into());
    }

    // `/exit` is a slash-command only when it starts the input line; appended
    // to text the user already typed it becomes an ordinary `foo/exit` message
    // and Claude never leaves (correctness review #4). Clear the line first.
    // `C-u` (kill-to-start) over Escape: Claude Code's input binds it to a plain
    // line-kill, whereas Escape is mode-sensitive (interrupts a turn, dismisses
    // a dialog). The common case — a stray prompt typed then left — has the
    // cursor at the end, which C-u clears whole.
    ctx.tmux().send_key(&session.tmux_pane, "C-u")?;
    ctx.tmux().send_keys(&session.tmux_pane, "/exit", true)?;
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.stop_requested",
        &serde_json::json!({ "forced": force }),
    )?;
    Ok(Stopped {
        session,
        name,
        forced_past: refusal.filter(|_| force),
        verdict,
    })
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, args.session)?;
    let stopped = apply(ctx, &found, args.force)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": stopped.session.id,
                "quest": found.quest.slug,
                "label": stopped.session.label,
                "pane": stopped.session.tmux_pane,
                "forced": args.force,
                "registry": stopped.verdict,
            }),
            || stopped.describe(),
        )?;
    }
    Ok(())
}
