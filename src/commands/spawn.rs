//! `q spawn` — a worker agent in its own tmux session (SPEC §6 v2). The main
//! session is `q-<slug>`; each worker is `q-<slug>+<label>`, a login shell with
//! Claude launched into it (`--shell` leaves it a bare shell, row `off`).

use std::io::Write;

use crate::Ctx;
use crate::commands::new::{MASTER, fresh_session_id, resolve_dir, validate_label};
use crate::commands::{AttachMode, attach_mode, live, sweep_quiet};
use crate::error::QError;
use crate::model::{Quest, QuestState, Session, SessionRole, SessionStatus};
use crate::output;
use crate::tmux::{
    NewSession, config_override, db_override, quest_env, quest_slug_of_name, session_name,
    worker_session_name,
};

#[derive(Debug)]
pub struct Args<'a> {
    pub quest: &'a str,
    /// `None` picks an auto `w<n>` label.
    pub label: Option<&'a str>,
    /// `None` (or blank) is a bare interactive Claude — no first prompt.
    pub prompt: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub dir: Option<&'a str>,
    /// A shell only: no Claude launched, the row lands `off` (SPEC §6). Start it
    /// later with `q start`.
    pub shell: bool,
    /// Attach to the new worker's tmux session afterwards.
    pub enter: bool,
}

/// What [`spawn_core`] hands back so the caller can report it or land on it.
struct Spawned {
    quest: Quest,
    session: Session,
    /// The worker's own tmux session, `q-<slug>+<label>`.
    tmux_session: String,
    /// Whether Claude was launched (false under `--shell`).
    launched: bool,
}

/// `q spawn <quest> [prompt] [--label …] [--shell] [--enter]`.
pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(args.quest)?;
    let spawned = spawn_core(
        ctx,
        &quest,
        args.label,
        args.prompt,
        args.workflow,
        args.dir,
        args.shell,
    )?;
    report_and_attach(ctx, &spawned, args.enter)
}

/// Spawn a bare worker (auto `w<n>` label, no prompt, Claude launched) for a
/// caller that reports the result itself — the TUI, which owns the screen and
/// must not have `q` print to it.
pub fn spawn_bare(ctx: &Ctx, quest_ref: &str) -> anyhow::Result<Session> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(quest_ref)?;
    Ok(spawn_core(ctx, &quest, None, None, None, None, false)?.session)
}

/// `q spawn-here <pane>` — the `[tmux] spawn_key` binding. Resolves the Quest
/// from the tmux pane the key was pressed in, spawns a bare worker (its own
/// session), and attaches to it. A pane that is not one of ours is a no-op.
pub fn run_here(ctx: &Ctx, pane: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let Some(slug) = quest_slug_of_pane(ctx, pane)? else {
        return output::emit(
            ctx.json,
            &serde_json::json!({ "spawned": false, "pane": pane }),
            || "q: this tmux pane is not in a Quest".to_string(),
        );
    };
    let quest = ctx.db()?.resolve_quest(&slug)?;
    let spawned = spawn_core(ctx, &quest, None, None, None, None, false)?;
    // The key was pressed inside this Quest, so land the caller on the fresh
    // worker's own session (a switch-client inside tmux).
    ctx.tmux()
        .attach(&spawned.tmux_session, Some(&spawned.session.tmux_pane))?;
    let Spawned {
        quest,
        session,
        tmux_session,
        launched,
    } = &spawned;
    output::emit(
        ctx.json,
        &serde_json::json!({
            "spawned": true,
            "quest": quest,
            "session": session,
            "tmux_session": tmux_session,
            "launched": launched,
        }),
        || format!("spawned {} in {}", session.label, quest.slug),
    )
}

/// The Quest slug owning `pane`, read off its tmux session name (SPEC §6): the
/// prefix is stripped and everything before the first `+` is the slug, so a
/// worker pane in `q-foo+review` reports `foo` and a sibling `q-foo-bar` reports
/// `foo-bar` rather than being mistaken for `foo`.
fn quest_slug_of_pane(ctx: &Ctx, pane: &str) -> anyhow::Result<Option<String>> {
    Ok(ctx
        .tmux()
        .list_panes()?
        .iter()
        .find(|p| p.pane_id == pane)
        .and_then(|p| quest_slug_of_name(&ctx.config, &p.session_name)))
}

/// Create the worker's tmux session and its row (SPEC §6). Shared by the CLI
/// (`run`), the TUI (`spawn_bare`) and the tmux binding (`run_here`); none of
/// them reports or attaches here — that is each caller's own.
#[allow(clippy::too_many_arguments)]
fn spawn_core(
    ctx: &Ctx,
    quest: &Quest,
    label: Option<&str>,
    prompt: Option<&str>,
    workflow: Option<&str>,
    dir: Option<&str>,
    shell: bool,
) -> anyhow::Result<Spawned> {
    let db = ctx.db()?;
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished; run `q resume {}` first",
            quest.slug, quest.slug
        ))
        .into());
    }
    // Checked and normalized in one, so the row stores the name that was
    // validated; see `crate::workflows::Registry::check_opt`.
    let workflow = ctx.workflows().check_opt(workflow)?;

    let main_session = session_name(&ctx.config, &quest.slug);
    if !ctx.tmux().has_session(&main_session)? {
        return Err(QError::Tmux(format!(
            "no tmux session `{main_session}`; run `q resume {}` first",
            quest.slug
        ))
        .into());
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let idx = next_worker_index(&sessions);
    let label = match label {
        Some(label) => {
            validate_label(label)?;
            if label == MASTER {
                return Err(QError::Invalid(format!(
                    "label `{MASTER}` is reserved for the main session of a Quest"
                ))
                .into());
            }
            if let Some(taken) = live(&sessions).find(|s| s.label == label) {
                return Err(QError::Conflict(format!(
                    "session `{}` is already live in quest {} ({})",
                    label, quest.slug, taken.id
                ))
                .into());
            }
            label.to_string()
        }
        None => auto_label(&sessions, idx),
    };
    let worker_session = worker_session_name(&ctx.config, &quest.slug, &label);
    if ctx.tmux().has_session(&worker_session)? {
        return Err(
            QError::Conflict(format!("tmux session `{worker_session}` already exists")).into(),
        );
    }

    let prompt = prompt.map(str::trim).filter(|p| !p.is_empty());
    // The Quest's cwd is already canonical; `--dir` gets the same treatment.
    let cwd = match dir {
        Some(dir) => resolve_dir(Some(dir))?.to_string_lossy().into_owned(),
        None => quest.cwd.clone(),
    };

    // The row goes in before the session: `$Q_SESSION` is in the session's
    // environment, and Claude's `SessionStart` hook fires against it as soon as
    // the pane exists. The pane — the session's identity — is filled in right
    // after tmux hands it over.
    let session_id = fresh_session_id(db)?;
    let mut row = Session::new(&quest.id, SessionRole::Worker, &label, &worker_session, "");
    row.id = session_id.clone();
    // A `--shell` worker is a bare shell from the start; otherwise the row is
    // `starting` and `launch` types Claude below.
    row.status = if shell {
        SessionStatus::Off
    } else {
        SessionStatus::Starting
    };
    row.workflow = workflow.or_else(|| quest.workflow.clone());
    row.first_prompt = prompt.map(str::to_string);
    row.claude_name = Some(crate::naming::claude_name(&quest.slug, &label));
    let pending = db.insert_session(&row)?;
    if pending.id != session_id {
        let _ = db.delete_session(&pending.id);
        return Err(QError::Db(format!(
            "session id `{session_id}` was taken between allocating and inserting it"
        ))
        .into());
    }

    let spec = NewSession {
        name: worker_session.clone(),
        window_name: label.clone(),
        cwd: cwd.clone(),
        env: quest_env(
            &quest.id,
            &session_id,
            SessionRole::Worker,
            &quest.machine,
            db_override().as_deref(),
            config_override().as_deref(),
        ),
        // The login shell (SPEC §6 v2); Claude is launched into it below.
        command: None,
    };
    let pane = match ctx.tmux().new_session(&spec) {
        Ok(pane) => pane,
        Err(e) => {
            let _ = db.delete_session(&session_id);
            return Err(e);
        }
    };
    let session = match db.update_session_pane(&session_id, &pane.pane_id) {
        Ok(session) => session,
        Err(e) => {
            let _ = ctx.tmux().kill_session(&worker_session);
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
            "role": session.role,
            "tmux_session": worker_session,
            "cwd": cwd,
            "workflow": session.workflow,
            "prompt": prompt,
            "shell": shell,
        }),
    )?;

    // Launch Claude unless this is a bare shell. A launch that will not go
    // takes its own session down — a shell with no Claude the caller did not
    // ask for is nobody's.
    let session = if shell {
        session
    } else {
        match crate::commands::start::launch(ctx, quest, &session, None, false, false) {
            Ok(started) => started.session,
            Err(e) => {
                let _ = ctx.tmux().kill_session(&worker_session);
                let _ = db.delete_session(&session_id);
                return Err(e);
            }
        }
    };
    Ok(Spawned {
        quest: quest.clone(),
        session,
        tmux_session: worker_session,
        launched: !shell,
    })
}

/// Emit the spawn result and, with `--enter`, attach to the worker's own tmux
/// session (a switch-client inside tmux, an exec outside it).
fn report_and_attach(ctx: &Ctx, spawned: &Spawned, enter: bool) -> anyhow::Result<()> {
    let Spawned {
        quest,
        session,
        tmux_session,
        launched,
    } = spawned;
    let attach = attach_mode(ctx, enter);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": session,
                "tmux_session": tmux_session,
                "launched": launched,
                "attach": attach,
            }),
            || {
                let note = if *launched { "" } else { " (shell only)" };
                format!(
                    "spawned {} ({}){note} · tmux {tmux_session} · run: q enter {} --session {}",
                    session.id, session.label, quest.slug, session.label
                )
            },
        )?;
    }
    if attach != AttachMode::None {
        std::io::stdout().flush()?;
        ctx.tmux().attach(tmux_session, Some(&session.tmux_pane))?;
    }
    Ok(())
}

/// The auto worker's label — `w<n>`, bumped past any number a live session
/// already carries so a mashed key never collides with itself.
fn auto_label(sessions: &[Session], start: usize) -> String {
    let mut n = start;
    loop {
        let label = format!("w{n}");
        if !live(sessions).any(|s| s.label == label) {
            return label;
        }
        n += 1;
    }
}

/// Workers are numbered from 1 for the life of the Quest — an ended worker's
/// number is not reused, so the numbering keeps matching the event log.
fn next_worker_index(sessions: &[Session]) -> usize {
    sessions
        .iter()
        .filter(|s| s.role == SessionRole::Worker)
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(role: SessionRole, label: &str) -> Session {
        Session::new("q-0001", role, label, "q-alpha", "%1")
    }

    #[test]
    fn worker_numbers_are_from_one_and_never_reused() {
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
    fn auto_label_is_wn_and_skips_a_live_collision() {
        assert_eq!(auto_label(&[], 1), "w1");
        let mut rows = vec![
            session(SessionRole::Master, MASTER),
            session(SessionRole::Worker, "tests"),
        ];
        assert_eq!(auto_label(&rows, next_worker_index(&rows)), "w2");
        rows.push(session(SessionRole::Worker, "w2"));
        assert_eq!(auto_label(&rows, 2), "w3");
    }

    #[test]
    fn a_reserved_or_malformed_label_is_rejected() {
        for bad in ["", "Upper", "with space", "double--dash", "-lead"] {
            assert!(validate_label(bad).is_err(), "accepted `{bad}`");
        }
        assert!(validate_label("cdc-backfill").is_ok());
    }
}
