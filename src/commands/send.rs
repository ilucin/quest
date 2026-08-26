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
use crate::commands::{pane_pid, sweep_quiet, target};
use crate::error::QError;
use crate::model::SessionStatus;
use crate::output;
use crate::registry::{self, Ask, Verdict};

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

    let session = &found.session;
    // The pane's pid is only needed when no hook ever recorded Claude's, so it
    // costs a `list-panes` exactly in the case the registry would otherwise
    // have nothing to say.
    let pane = session
        .claude_pid
        .is_none()
        .then(|| pane_pid(ctx, &session.tmux_pane))
        .flatten();
    // Identity, so a recycled pid cannot make another session's entry speak
    // for this one: `<slug>/<label>` is the name q launched Claude with, and
    // the session id is the exact match when a hook recorded one.
    let name = found.name();
    let verdict = registry::verdict(Ask {
        pid: session.claude_pid,
        pane_pid: pane,
        name: Some(&name),
        session_id: session.claude_session_id.as_deref(),
        now_ms: registry::now_ms(),
    });
    let refusal = gate(session.status, &verdict);
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
    // once per line. Bracketed paste is how a terminal says "these newlines
    // are text"; a single line keeps the plain, verified send-keys path.
    let pasted = text.contains('\n');
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

/// Why the send should not happen, or `None` when the two sources agree the
/// session is between turns.
///
/// The database decides first: `busy` is mid-turn, `waiting` is sitting on a
/// prompt. For a database that says `idle` the registry can object — and one
/// that knows nothing (no pid, no file, a stale or unreadable one) never
/// objects on its own.
///
/// `starting` is the one row the registry can overrule. Without hooks the row
/// never leaves `starting`, so Claude's own "idle" is the only evidence there
/// is that it is up and between turns.
fn gate(status: SessionStatus, verdict: &Verdict) -> Option<String> {
    match status {
        SessionStatus::Idle => verdict.refuses().then(|| verdict.describe()),
        SessionStatus::Starting if verdict.agrees_idle() => None,
        other => Some(format!("q has it as {other} ({})", verdict.describe())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> Verdict {
        Verdict::Busy {
            status: "waiting".to_string(),
            waiting_for: Some("permission_prompt".to_string()),
            name: None,
        }
    }

    fn unknown() -> Verdict {
        Verdict::Unknown {
            reason: "no entry for this session",
        }
    }

    #[test]
    fn an_idle_session_the_registry_agrees_on_passes() {
        assert_eq!(
            gate(SessionStatus::Idle, &Verdict::Idle { name: None }),
            None
        );
    }

    #[test]
    fn an_unknown_registry_does_not_block_an_idle_session() {
        assert_eq!(gate(SessionStatus::Idle, &unknown()), None);
    }

    #[test]
    fn the_registry_overrides_a_stale_idle_row() {
        let reason = gate(SessionStatus::Idle, &busy()).unwrap();
        assert_eq!(reason, "registry: waiting, waiting for permission_prompt");
    }

    #[test]
    fn a_database_that_says_not_idle_refuses_whatever_the_registry_says() {
        for status in [SessionStatus::Busy, SessionStatus::Waiting] {
            for verdict in [Verdict::Idle { name: None }, unknown(), busy()] {
                let reason = gate(status, &verdict).unwrap_or_else(|| panic!("{status} passed"));
                assert!(reason.contains(status.as_str()), "{reason}");
            }
        }
    }

    /// The hooks-not-installed case: the row is stuck on `starting`, and only
    /// Claude's own registry can say the session is actually up and waiting.
    #[test]
    fn a_starting_row_the_registry_calls_idle_passes() {
        assert_eq!(
            gate(SessionStatus::Starting, &Verdict::Idle { name: None }),
            None
        );
        for verdict in [unknown(), busy()] {
            let reason = gate(SessionStatus::Starting, &verdict).unwrap();
            assert!(reason.contains("starting"), "{reason}");
        }
    }
}
