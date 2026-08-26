//! `q kill <session> [-f]` — end one worker: kill its tmux window, mark the
//! row ended (SPEC §6, §16).
//!
//! The master is never killable here. Window 0 *is* the Quest — killing it
//! strands the tmux session with no way back in (`q enter` refuses a Quest
//! whose master is gone), so `q close` is the only sanctioned path and `-f`
//! does not change that: `-f` means "don't ask", not "do something else".

use crate::Ctx;
use crate::commands::new::MASTER;
use crate::commands::{confirm, sweep_quiet, target};
use crate::error::QError;
use crate::model::{SessionRole, now};
use crate::output;

pub fn run(ctx: &Ctx, session_target: &str, force: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, session_target)?;
    let session = found.session.clone();

    if session.role == SessionRole::Master || session.label == MASTER {
        return Err(QError::Invalid(format!(
            "{} is the master of quest {}; run `q close {}` to end the whole Quest",
            found.name(),
            found.quest.slug,
            found.quest.slug
        ))
        .into());
    }

    // Killing twice is not an error; the sweep may already have caught it.
    if found.ended() {
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({
                    "session": session,
                    "quest": found.quest.slug,
                    "already_ended": true,
                }),
                || format!("session {} ({}) already ended", found.name(), session.id),
            )?;
        }
        return Ok(());
    }

    if !force {
        confirm(
            ctx,
            &format!(
                "kill session {} (tmux window of pane {})?",
                found.name(),
                session.tmux_pane
            ),
        )?;
    }

    // The row must end even if tmux lost the pane between the sweep and now.
    let killed = ctx.tmux().kill_window(&session.tmux_pane).is_ok();
    let ended = ctx.db()?.mark_session_ended(&session.id, now())?;
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.end",
        &serde_json::json!({ "reason": "killed", "pane_killed": killed }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": ended,
                "quest": found.quest.slug,
                "already_ended": false,
                "pane_killed": killed,
            }),
            || {
                let name = found.name();
                if killed {
                    format!("killed {name} ({})", session.id)
                } else {
                    format!("killed {name} ({}) · pane was already gone", session.id)
                }
            },
        )?;
    }
    Ok(())
}
