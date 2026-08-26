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
use crate::output;

pub struct Args<'a> {
    pub session: &'a str,
    pub text: &'a str,
    pub force: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let text = args.text.trim_end_matches(['\r', '\n']);
    if text.trim().is_empty() {
        return Err(QError::Invalid("nothing to send".to_string()).into());
    }
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, args.session)?;
    found.require_live()?;

    let (verdict, refusal) = found.idle_gate(ctx);
    let session = &found.session;
    let name = found.name();
    if let Some(reason) = &refusal
        && !args.force
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
            "forced": args.force,
            "pasted": pasted,
        }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": session.id,
                "quest": found.quest.slug,
                "label": session.label,
                "pane": session.tmux_pane,
                "text": text,
                "forced": args.force,
                "pasted": pasted,
                "status": session.status,
                "registry": verdict,
            }),
            || match &refusal {
                Some(reason) => format!("sent to {name} (forced past: {reason})"),
                None => format!("sent to {name}"),
            },
        )?;
    }
    Ok(())
}
