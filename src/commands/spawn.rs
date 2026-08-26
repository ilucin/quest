//! `q spawn` — a worker agent in a new window of the Quest's tmux session
//! (SPEC §6, §16). Window 0 is the master; workers get `w<n>-<label>`.

use std::io::Write;

use crate::Ctx;
use crate::commands::new::{MASTER, claude_command, fresh_session_id, resolve_dir, validate_label};
use crate::commands::{AttachMode, live, sweep_quiet};
use crate::error::QError;
use crate::model::{QuestState, Session, SessionRole, SessionStatus};
use crate::output;
use crate::tmux::{NewWindow, Tmux, config_override, db_override, quest_env, session_name};

#[derive(Debug)]
pub struct Args<'a> {
    pub quest: &'a str,
    pub label: &'a str,
    pub prompt: &'a str,
    pub workflow: Option<&'a str>,
    pub dir: Option<&'a str>,
    pub no_attach: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    validate_label(args.label)?;
    if args.label == MASTER {
        return Err(QError::Invalid(format!(
            "label `{MASTER}` is reserved for window 0 of a Quest"
        ))
        .into());
    }
    let prompt = args.prompt.trim();
    if prompt.is_empty() {
        return Err(QError::Invalid("a worker needs a prompt".to_string()).into());
    }

    let quest = db.resolve_quest(args.quest)?;
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished; run `q resume {}` first",
            quest.slug, quest.slug
        ))
        .into());
    }
    let tmux_session = session_name(&ctx.config, &quest.slug);
    if !ctx.tmux().has_session(&tmux_session)? {
        return Err(QError::Tmux(format!(
            "no tmux session `{tmux_session}`; run `q resume {}` first",
            quest.slug
        ))
        .into());
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    if let Some(taken) = live(&sessions).find(|s| s.label == args.label) {
        return Err(QError::Conflict(format!(
            "session `{}` is already live in quest {} ({})",
            args.label, quest.slug, taken.id
        ))
        .into());
    }
    let window = window_name(next_worker_index(&sessions), args.label);
    // The Quest's cwd is already canonical; `--dir` gets the same treatment.
    let cwd = match args.dir {
        Some(dir) => resolve_dir(Some(dir))?.to_string_lossy().into_owned(),
        None => quest.cwd.clone(),
    };

    // The row goes in before the window: `$Q_SESSION` is in the window's
    // environment, and Claude's `SessionStart` hook fires against it as soon as
    // the window exists. The pane — the session's identity — is filled in right
    // after tmux hands it over.
    let session_id = fresh_session_id(db)?;
    let mut row = Session::new(
        &quest.id,
        SessionRole::Worker,
        args.label,
        &tmux_session,
        "",
    );
    row.id = session_id.clone();
    row.status = SessionStatus::Starting;
    // Without `--workflow` a worker runs the Quest's, as the master does.
    // TODO(M5): validate `--workflow` against the workflow registry.
    row.workflow = args
        .workflow
        .map(str::to_string)
        .or_else(|| quest.workflow.clone());
    row.first_prompt = Some(prompt.to_string());
    // The name `claude -n` is given below (SPEC §6), recorded so the registry's
    // identity check has something true to compare against.
    row.claude_name = Some(crate::naming::claude_name(&quest.slug, args.label));
    // `session.start` is the hook's to append once Claude comes up.
    let pending = db.insert_session(&row)?;
    if pending.id != session_id {
        // A regenerated id would no longer match `Q_SESSION` in the window.
        let _ = db.delete_session(&pending.id);
        return Err(QError::Db(format!(
            "session id `{session_id}` was taken between allocating and inserting it"
        ))
        .into());
    }

    let spec = NewWindow {
        session: tmux_session.clone(),
        window_name: window.clone(),
        cwd: cwd.clone(),
        env: quest_env(
            &quest.id,
            &session_id,
            SessionRole::Worker,
            &quest.machine,
            db_override().as_deref(),
            config_override().as_deref(),
        ),
        // Claude is named after the label, not the window: `<slug>/<label>` is
        // also how `q send`/`q peek` address the session (SPEC §6, §16).
        command: Some(claude_command(&quest.slug, args.label, Some(prompt))),
    };
    let pane = match ctx.tmux().new_window(&spec) {
        Ok(pane) => pane,
        // Nothing was opened, so the row would only be a session that never ran.
        Err(e) => {
            let _ = db.delete_session(&session_id);
            return Err(e);
        }
    };
    let session = match db.update_session_pane(&session_id, &pane.pane_id) {
        Ok(session) => session,
        // Without its pane the row can never be addressed, entered or swept —
        // and the window would outlive it as a Claude nobody owns.
        Err(e) => {
            let _ = ctx.tmux().kill_window(&pane.pane_id);
            let _ = db.delete_session(&session_id);
            return Err(e);
        }
    };
    db.append_event(
        &quest.id,
        Some(&session.id),
        "session.spawn",
        &serde_json::json!({
            "label": session.label,
            "window": window,
            "role": session.role,
            "cwd": cwd,
            "workflow": session.workflow,
            "prompt": prompt,
        }),
    )?;

    // `new-window -d` left the caller's client alone. Only the same-session
    // case selects the worker's window, and that is `Select`, not an attach:
    // no client ever changes session here.
    let attaching = !args.no_attach && in_tmux_session(ctx, &quest.id, &tmux_session);
    let attach = if attaching {
        AttachMode::Select
    } else {
        AttachMode::None
    };
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": session,
                "tmux_session": tmux_session,
                "window": window,
                "attach": attach,
            }),
            || {
                format!(
                    "spawned {} ({}) · tmux {tmux_session}:{window} · run: q enter {} --session {}",
                    session.id, session.label, quest.slug, session.label
                )
            },
        )?;
    }
    if attaching {
        std::io::stdout().flush()?;
        ctx.tmux().select_window(&session.tmux_pane)?;
    }
    Ok(())
}

/// `w<n>-<label>` (SPEC §6).
fn window_name(index: usize, label: &str) -> String {
    format!("w{index}-{label}")
}

/// Workers are numbered from 1 for the life of the Quest — an ended window's
/// number is not reused, so the numbering keeps matching the event log.
fn next_worker_index(sessions: &[Session]) -> usize {
    sessions
        .iter()
        .filter(|s| s.role == SessionRole::Worker)
        .count()
        + 1
}

/// True when this process runs in a pane of `tmux_session` — the only case
/// where selecting the new window moves the caller's own client rather than
/// yanking a client elsewhere. `$TMUX_PANE` is the precise answer; an agent
/// whose shell lost it still has `$Q_QUEST`.
fn in_tmux_session(ctx: &Ctx, quest_id: &str, tmux_session: &str) -> bool {
    if !ctx.tmux().in_tmux() {
        return false;
    }
    match std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty()) {
        Some(pane) => here(ctx.tmux(), &pane, tmux_session),
        None => std::env::var("Q_QUEST").is_ok_and(|v| v == quest_id),
    }
}

fn here(tmux: &dyn Tmux, pane_id: &str, tmux_session: &str) -> bool {
    tmux.list_panes().is_ok_and(|panes| {
        panes
            .iter()
            .any(|p| p.pane_id == pane_id && p.session_name == tmux_session)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(role: SessionRole, label: &str) -> Session {
        Session::new("q-0001", role, label, "q-alpha", "%1")
    }

    #[test]
    fn worker_windows_are_numbered_from_one_and_never_reused() {
        assert_eq!(next_worker_index(&[]), 1);
        let master = session(SessionRole::Master, MASTER);
        assert_eq!(next_worker_index(std::slice::from_ref(&master)), 1);

        let mut rows = vec![master, session(SessionRole::Worker, "tests")];
        assert_eq!(next_worker_index(&rows), 2);
        // An ended worker still holds its number.
        rows[1].status = SessionStatus::Ended;
        assert_eq!(next_worker_index(&rows), 2);
        rows.push(session(SessionRole::Worker, "migration"));
        assert_eq!(next_worker_index(&rows), 3);
    }

    #[test]
    fn window_names_follow_the_spec() {
        assert_eq!(window_name(1, "tests"), "w1-tests");
        assert_eq!(window_name(12, "cdc-backfill"), "w12-cdc-backfill");
    }

    #[test]
    fn a_reserved_or_malformed_label_is_rejected() {
        for bad in ["", "Upper", "with space", "double--dash", "-lead"] {
            assert!(validate_label(bad).is_err(), "accepted `{bad}`");
        }
        assert!(validate_label("cdc-backfill").is_ok());
    }
}
