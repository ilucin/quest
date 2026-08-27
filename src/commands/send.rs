//! `q send <session> "<text>" [--force]` — type into a session's pane
//! (SPEC §6), gated on the session actually being idle (SPEC §23 #5).
//!
//! Send-keys into a live TUI is destructive when mistimed: text typed mid-turn
//! is swallowed, and text typed at a permission prompt is read as the answer.
//! So the gate takes two opinions — the database (hook-fed) and Claude's own
//! registry — and only sends when neither objects. `--force` is the explicit
//! way past it.

use crate::Ctx;
use crate::commands::fmt::{EVENT_PROMPT_CHARS, truncate};
use crate::commands::{sweep_quiet, target};
use crate::error::QError;
use crate::model::Session;
use crate::output;
use crate::registry::Verdict;

pub struct Args<'a> {
    pub session: &'a str,
    pub text: &'a str,
    pub force: bool,
}

/// What a send did, for the caller to report however it reports.
pub struct Sent {
    pub session: Session,
    /// `<slug>/<label>` — what the session is called to the user.
    pub name: String,
    /// The gate's objection, when `force` overrode one; `None` when the two
    /// sources agreed the session was between turns.
    pub forced_past: Option<String>,
    /// Whether the text went in as a bracketed paste rather than typed keys.
    pub pasted: bool,
    pub verdict: Verdict,
}

impl Sent {
    pub fn describe(&self) -> String {
        match &self.forced_past {
            Some(reason) => format!("sent to {} (forced past: {reason})", self.name),
            None => format!("sent to {}", self.name),
        }
    }
}

/// Type `text` into the session's pane, refusing unless the session is between
/// turns (SPEC §23 #5) or `force` says otherwise.
///
/// Split out of [`run`] so the TUI's `t` (SPEC §17) goes through the same gate
/// and the same send-keys/paste decision, and so nothing writes to a terminal
/// the TUI still owns.
pub fn apply(ctx: &Ctx, found: &target::Target, text: &str, force: bool) -> anyhow::Result<Sent> {
    let text = text.trim_end_matches(['\r', '\n']);
    if text.trim().is_empty() {
        return Err(QError::Invalid("nothing to send".to_string()).into());
    }
    found.require_live()?;

    let (verdict, refusal) = found.idle_gate(ctx);
    let session = found.session.clone();
    let name = found.name();
    if let Some(reason) = &refusal
        && !force
    {
        let hint = verdict
            .hint()
            .map(|h| format!(" ({h})"))
            .unwrap_or_default();
        return Err(QError::Conflict(format!(
            "{name} is not idle: {reason}{hint}. Pass --force to send anyway"
        ))
        .into());
    }

    // A newline typed into a TUI is Enter, so a multi-line prompt would submit
    // once per line — and tmux rewrites a lone `\r` to the same thing. Every
    // other control byte is a key of its own once typed (ESC leaves the prompt,
    // Tab completes), so text carrying any of them goes in as a bracketed
    // paste, which is how a terminal says "this is text". Plain text keeps the
    // verified send-keys path.
    let pasted = text.chars().any(char::is_control);
    if pasted {
        ctx.tmux().paste(&session.tmux_pane, text, true)?;
    } else {
        ctx.tmux().send_keys(&session.tmux_pane, text, true)?;
    }
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.send",
        &serde_json::json!({
            "text": truncate(text, EVENT_PROMPT_CHARS),
            "forced": force,
            "pasted": pasted,
        }),
    )?;

    Ok(Sent {
        session,
        name,
        forced_past: refusal.filter(|_| force),
        pasted,
        verdict,
    })
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let text = args.text.trim_end_matches(['\r', '\n']);
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, args.session)?;
    let sent = apply(ctx, &found, text, args.force)?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": sent.session.id,
                "quest": found.quest.slug,
                "label": sent.session.label,
                "pane": sent.session.tmux_pane,
                "text": text,
                "forced": args.force,
                "pasted": sent.pasted,
                "status": sent.session.status,
                "registry": sent.verdict,
            }),
            || sent.describe(),
        )?;
    }
    Ok(())
}
