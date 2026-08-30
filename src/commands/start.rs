//! `q start <session> [prompt] [--resume] [--force]` — launch Claude in a
//! session's shell pane (SPEC §6).
//!
//! A q session is a login shell; Claude is a child launched by typing the
//! command into that shell. `q start` refuses when the pane is already running
//! something that is not a shell (a Claude that is up, or `vim`, or a build),
//! unless `--force`. Shared by `q new`/`q resume` (the master) and `q spawn`
//! (a worker), so every launch path types the same command and marks the row
//! the same way.

use crate::Ctx;
use crate::commands::new::shell_quote;
use crate::commands::{sweep_quiet, target};
use crate::error::QError;
use crate::model::{Quest, Session, now};
use crate::output;
use crate::tmux::is_shell;

/// A prompt longer than this — or one that spans lines — is fetched at run time
/// with `q prompt <id>` rather than typed literally: a newline sent with
/// `send-keys -l` is Enter, so an inline multi-line prompt would submit early.
const INLINE_PROMPT_MAX: usize = 2048;

pub struct Args<'a> {
    pub session: &'a str,
    /// A first prompt for this launch; `None` reuses the session's stored one.
    pub prompt: Option<&'a str>,
    pub resume: bool,
    pub force: bool,
}

/// What a launch did, for the caller to report however it reports.
pub struct Started {
    pub session: Session,
    /// `<slug>/<label>` — what the session is called to the user.
    pub name: String,
    /// The exact command typed into the shell.
    pub command: String,
    /// The non-shell command `--force` typed Claude on top of; `None` when the
    /// pane was an idle shell.
    pub forced_past: Option<String>,
}

impl Started {
    pub fn describe(&self) -> String {
        match &self.forced_past {
            Some(cmd) => format!("started {} (forced past: {cmd})", self.name),
            None => format!("started {}", self.name),
        }
    }
}

/// The command `q start` types into the shell (SPEC §6). `claude -n <slug>/<label>`
/// keeps the registry's identity check truthful and `/rename` honest; `--resume`
/// re-attaches Claude's own last session in this pane; a small single-line prompt
/// is embedded, a large or multi-line one is read back with `q prompt <id>`.
pub fn launch_command(
    slug: &str,
    label: &str,
    prompt: Option<&str>,
    session_id: &str,
    resume: bool,
) -> String {
    let mut cmd = format!("claude -n {}", shell_quote(&format!("{slug}/{label}")));
    if resume {
        cmd.push_str(" --resume");
    }
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        cmd.push_str(" -- ");
        if prompt.contains('\n') || prompt.len() > INLINE_PROMPT_MAX {
            // Double-quoted so the shell runs the substitution; `q prompt` reads
            // the text back out of the database this session was recorded in.
            cmd.push_str(&format!("\"$(q prompt {session_id})\""));
        } else {
            cmd.push_str(&shell_quote(prompt));
        }
    }
    cmd
}

/// The pane's `pane_current_command` right now, or `None` when tmux no longer
/// has the pane.
fn current_command(ctx: &Ctx, pane_id: &str) -> Option<String> {
    ctx.tmux()
        .list_panes()
        .ok()?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
        .map(|p| p.current_command)
}

/// Type the launch command into `session`'s pane and mark the row `starting`
/// (SPEC §6). Refuses a pane running a non-shell command unless `force`.
///
/// Shared by `q start`, `q new`/`q resume` (via `spawn_master`) and `q spawn`.
/// The prompt stored on the row is what `q prompt` reads back, so it is written
/// before the command is typed — a large prompt's `$(q prompt …)` runs in the
/// pane a beat later and must find it there.
pub fn launch(
    ctx: &Ctx,
    quest: &Quest,
    session: &Session,
    prompt: Option<&str>,
    resume: bool,
    force: bool,
) -> anyhow::Result<Started> {
    let db = ctx.db()?;
    let name = format!("{}/{}", quest.slug, session.label);
    if session.tmux_pane.is_empty() {
        return Err(QError::Other(format!(
            "session {name} ({}) has no pane; it never finished starting",
            session.id
        ))
        .into());
    }
    // tmux execs the login shell before its rc runs, so a pane q just opened is
    // a shell from t=0 — no readiness wait, only this check (plan review 2 #3).
    let forced_past = match current_command(ctx, &session.tmux_pane) {
        Some(cmd) if !cmd.is_empty() && !is_shell(&cmd) => {
            if !force {
                return Err(QError::Conflict(format!(
                    "pane of {name} is running `{cmd}` (not a shell); \
                     pass --force to launch Claude anyway"
                ))
                .into());
            }
            Some(cmd)
        }
        _ => None,
    };

    let effective = prompt.or(session.first_prompt.as_deref());
    let updated = db.record_claude_launch(
        &session.id,
        &crate::naming::claude_name(&quest.slug, &session.label),
        prompt,
        now(),
    )?;
    let command = launch_command(&quest.slug, &session.label, effective, &session.id, resume);
    ctx.tmux().send_keys(&session.tmux_pane, &command, true)?;
    // No event here: `q new`/`q spawn` launch as part of their own flow and log
    // their own `quest.created`/`session.spawn`; Claude's `SessionStart` hook is
    // what records `session.start` once it is up. `q start` (the explicit verb)
    // appends its own `session.start_requested` in [`run`].
    Ok(Started {
        session: updated,
        name,
        command,
        forced_past,
    })
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, args.session)?;
    let started = launch(
        ctx,
        &found.quest,
        &found.session,
        args.prompt,
        args.resume,
        args.force,
    )?;
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&started.session.id),
        "session.start_requested",
        &serde_json::json!({ "resume": args.resume, "forced": args.force }),
    )?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": started.session.id,
                "quest": found.quest.slug,
                "label": started.session.label,
                "pane": started.session.tmux_pane,
                "command": started.command,
                "status": started.session.status,
                "forced_past": started.forced_past,
            }),
            || started.describe(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_prompt_is_embedded_and_a_big_one_is_fetched() {
        assert_eq!(
            launch_command("foo", "master", None, "s-1", false),
            "claude -n foo/master"
        );
        assert_eq!(
            launch_command("foo", "master", Some("go now"), "s-1", false),
            "claude -n foo/master -- 'go now'"
        );
        assert_eq!(
            launch_command("foo", "w1", None, "s-1", true),
            "claude -n foo/w1 --resume"
        );
        // Multi-line: read back at run time so the newline is not an early Enter.
        assert_eq!(
            launch_command("foo", "w1", Some("one\ntwo"), "s-9", false),
            "claude -n foo/w1 -- \"$(q prompt s-9)\""
        );
        // Oversized: the same.
        let long = "x".repeat(INLINE_PROMPT_MAX + 1);
        assert_eq!(
            launch_command("foo", "w1", Some(&long), "s-9", false),
            "claude -n foo/w1 -- \"$(q prompt s-9)\""
        );
    }
}
