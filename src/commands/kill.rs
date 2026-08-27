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
use crate::model::{Session, SessionRole, now};
use crate::output;

/// What killing a session did, for the caller to report however it reports.
pub struct Killed {
    pub session: Session,
    /// `<slug>/<label>` — what the session is called to the user.
    pub name: String,
    /// The sweep had already ended it; nothing was done.
    pub already_ended: bool,
    /// tmux still had the pane and it is gone now.
    pub pane_killed: bool,
}

impl Killed {
    pub fn describe(&self) -> String {
        if self.already_ended {
            return format!("session {} ({}) already ended", self.name, self.session.id);
        }
        if self.pane_killed {
            format!("killed {} ({})", self.name, self.session.id)
        } else {
            format!(
                "killed {} ({}) · pane was already gone",
                self.name, self.session.id
            )
        }
    }
}

/// The master is never killable — see this module's own docs. Checked by
/// `q kill` and by the TUI's `k` (SPEC §17), and again inside [`apply`] so no
/// future caller can route around it.
pub fn guard_master(found: &target::Target) -> anyhow::Result<()> {
    let session = &found.session;
    if session.role == SessionRole::Master || session.label == MASTER {
        return Err(QError::Invalid(format!(
            "{} is the master of quest {}; run `q close {}` to end the whole Quest",
            found.name(),
            found.quest.slug,
            found.quest.slug
        ))
        .into());
    }
    Ok(())
}

/// End one worker: kill its tmux window and mark the row ended. The
/// confirmation is the caller's — `q kill` asks on the terminal, the TUI's `k`
/// asks with a guarded form (SPEC §17) — so nothing here reads stdin or
/// writes a byte.
pub fn apply(ctx: &Ctx, found: &target::Target) -> anyhow::Result<Killed> {
    guard_master(found)?;
    let session = found.session.clone();
    let name = found.name();

    // Killing twice is not an error; the sweep may already have caught it.
    if found.ended() {
        return Ok(Killed {
            session,
            name,
            already_ended: true,
            pane_killed: false,
        });
    }
    // A live row with no pane is a `q spawn` whose window never opened. tmux
    // reads an empty `-t` target as "whatever is current", so killing it would
    // take down the window `q` is itself running in. This is the one pane
    // command that cannot go through `require_live` — it accepts an ended row
    // on purpose — so it says so here.
    found.require_pane()?;

    // The row must end even if tmux lost the pane between the sweep and now.
    let pane_killed = ctx.tmux().kill_window(&session.tmux_pane).is_ok();
    let ended = ctx.db()?.mark_session_ended(&session.id, now())?;
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.end",
        &serde_json::json!({ "reason": "killed", "pane_killed": pane_killed }),
    )?;
    Ok(Killed {
        session: ended,
        name,
        already_ended: false,
        pane_killed,
    })
}

pub fn run(ctx: &Ctx, session_target: &str, force: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, session_target)?;
    guard_master(&found)?;

    // An already-ended session has nothing to confirm: `apply` reports the
    // no-op, and the question would be about work that is not going to happen.
    if !found.ended() && !force {
        // Before the question, not after the answer: a row whose window never
        // opened has no pane to name, and the prompt read "tmux window of
        // pane )". `apply` refuses it either way — this just refuses first.
        found.require_pane()?;
        confirm(
            ctx,
            &format!(
                "kill session {} (tmux window of pane {})?",
                found.name(),
                found.session.tmux_pane
            ),
        )?;
    }

    let killed = apply(ctx, &found)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": killed.session,
                "quest": found.quest.slug,
                "already_ended": killed.already_ended,
                "pane_killed": killed.pane_killed,
            }),
            || killed.describe(),
        )?;
    }
    Ok(())
}
