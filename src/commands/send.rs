//! `q send <session> "<text>" [--force]` — type into a session's pane
//! (SPEC §6), gated on the session actually being idle (SPEC §23 #5).
//!
//! Send-keys into a live TUI is destructive when mistimed: text typed mid-turn
//! is swallowed, and text typed at a permission prompt is read as the answer.
//! So the gate takes two opinions — the database (hook-fed) and Claude's own
//! registry — and only sends when neither objects. `--force` is the explicit
//! way past it.

use crate::Ctx;
use crate::commands::{sweep_quiet, target};
use crate::error::QError;
use crate::model::SessionStatus;
use crate::output;
use crate::registry::{self, Verdict};

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
    let verdict = registry::verdict(session.claude_pid);
    let refusal = gate(session.status, &verdict);
    if let Some(reason) = &refusal
        && !args.force
    {
        return Err(QError::Conflict(format!(
            "{} is not idle: {reason}. Pass --force to send anyway",
            found.name()
        ))
        .into());
    }

    ctx.tmux().send_keys(&session.tmux_pane, text, true)?;
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.send",
        &serde_json::json!({ "text": text, "forced": args.force }),
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
                "status": session.status,
                "registry": verdict,
            }),
            || {
                let name = found.name();
                match &refusal {
                    Some(reason) => format!("sent to {name} (forced past: {reason})"),
                    None => format!("sent to {name}"),
                }
            },
        )?;
    }
    Ok(())
}

/// Why the send should not happen, or `None` when both sources agree the
/// session is between turns.
///
/// The database decides first: `starting` has no prompt yet, `busy` is
/// mid-turn, `waiting` is sitting on a prompt. Only for a database that says
/// `idle` does the registry get a say — and a registry that knows nothing
/// (no pid yet, no file, an unreadable one) never blocks on its own.
fn gate(status: SessionStatus, verdict: &Verdict) -> Option<String> {
    match status {
        SessionStatus::Idle => verdict.refuses().then(|| verdict.describe()),
        SessionStatus::Ended => Some("session has ended".to_string()),
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
        for status in [
            SessionStatus::Busy,
            SessionStatus::Waiting,
            SessionStatus::Starting,
        ] {
            for verdict in [Verdict::Idle { name: None }, unknown(), busy()] {
                let reason = gate(status, &verdict).unwrap_or_else(|| panic!("{status} passed"));
                assert!(reason.contains(status.as_str()), "{reason}");
            }
        }
        assert_eq!(
            gate(SessionStatus::Ended, &Verdict::Idle { name: None }),
            Some("session has ended".to_string())
        );
    }
}
