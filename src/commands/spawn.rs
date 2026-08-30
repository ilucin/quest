//! `q spawn` — a worker agent in a new window of the Quest's tmux session
//! (SPEC §6, §16). Window 0 is the master; workers get `w<n>-<label>`.

use std::io::Write;

use crate::Ctx;
use crate::commands::new::{MASTER, claude_command, fresh_session_id, resolve_dir, validate_label};
use crate::commands::{AttachMode, live, sweep_quiet};
use crate::error::QError;
use crate::model::{Quest, QuestState, Session, SessionRole, SessionStatus};
use crate::output;
use crate::tmux::{NewWindow, Tmux, config_override, db_override, quest_env, session_name};

#[derive(Debug)]
pub struct Args<'a> {
    pub quest: &'a str,
    /// `None` picks an auto `w<n>` label.
    pub label: Option<&'a str>,
    /// `None` (or blank) is a bare interactive Claude — no first prompt.
    pub prompt: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub dir: Option<&'a str>,
    pub no_attach: bool,
}

/// What [`spawn_core`] hands back so the caller can report it or land on it.
struct Spawned {
    quest: Quest,
    session: Session,
    tmux_session: String,
    window: String,
}

/// `q spawn <quest> [prompt] [--label …]`.
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
    )?;
    report_and_attach(ctx, &spawned, args.no_attach)
}

/// Spawn a bare worker (auto `w<n>` label, no prompt) for a caller that reports
/// the result itself — the TUI, which owns the screen and must not have `q`
/// print to it. Resolves the Quest fresh, creates the window, returns the row;
/// no stdout, no window select.
pub fn spawn_bare(ctx: &Ctx, quest_ref: &str) -> anyhow::Result<Session> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(quest_ref)?;
    Ok(spawn_core(ctx, &quest, None, None, None, None)?.session)
}

/// `q spawn-here <pane>` — the `[tmux] spawn_key` binding. Resolves the Quest
/// from the tmux pane the key was pressed in, spawns a bare worker there, and
/// selects it. A pane that is not one of ours is a friendly no-op.
pub fn run_here(ctx: &Ctx, pane: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let Some(slug) = quest_slug_of_pane(ctx, pane)? else {
        if !ctx.quiet {
            println!("q: this tmux pane is not in a Quest");
        }
        return Ok(());
    };
    let quest = ctx.db()?.resolve_quest(&slug)?;
    let spawned = spawn_core(ctx, &quest, None, None, None, None)?;
    // The key was pressed inside this Quest's session, so land the caller on the
    // fresh worker. `run_here` runs detached from the pane's environment, so the
    // `in_tmux_session` heuristic cannot see it — select unconditionally.
    ctx.tmux().select_window(&spawned.session.tmux_pane)?;
    if !ctx.quiet {
        println!("spawned {} in {}", spawned.session.label, quest.slug);
    }
    Ok(())
}

/// The Quest slug owning `pane`, read off its tmux session name
/// (`<session_prefix><slug>`), or `None` when the pane is not a Quest's.
fn quest_slug_of_pane(ctx: &Ctx, pane: &str) -> anyhow::Result<Option<String>> {
    let prefix = ctx.config.tmux.session_prefix.as_str();
    Ok(ctx
        .tmux()
        .list_panes()?
        .iter()
        .find(|p| p.pane_id == pane)
        .and_then(|p| p.session_name.strip_prefix(prefix))
        .filter(|slug| !slug.is_empty())
        .map(str::to_string))
}

/// Create the worker window and its session row (SPEC §6). Shared by the CLI
/// (`run`) and the tmux binding (`run_here`); neither reports nor attaches —
/// that is each caller's own.
fn spawn_core(
    ctx: &Ctx,
    quest: &Quest,
    label: Option<&str>,
    prompt: Option<&str>,
    workflow: Option<&str>,
    dir: Option<&str>,
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

    let tmux_session = session_name(&ctx.config, &quest.slug);
    if !ctx.tmux().has_session(&tmux_session)? {
        return Err(QError::Tmux(format!(
            "no tmux session `{tmux_session}`; run `q resume {}` first",
            quest.slug
        ))
        .into());
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let idx = next_worker_index(&sessions);
    // A named worker validates and must be free; an auto one is `w<n>`, and the
    // window carries that name on its own (no `w<n>-w<n>`).
    let (label, window) = match label {
        Some(label) => {
            validate_label(label)?;
            if label == MASTER {
                return Err(QError::Invalid(format!(
                    "label `{MASTER}` is reserved for window 0 of a Quest"
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
            (label.to_string(), window_name(idx, label))
        }
        None => {
            let label = auto_label(&sessions, idx);
            (label.clone(), label)
        }
    };
    let prompt = prompt.map(str::trim).filter(|p| !p.is_empty());
    // The Quest's cwd is already canonical; `--dir` gets the same treatment.
    let cwd = match dir {
        Some(dir) => resolve_dir(Some(dir))?.to_string_lossy().into_owned(),
        None => quest.cwd.clone(),
    };

    // The row goes in before the window: `$Q_SESSION` is in the window's
    // environment, and Claude's `SessionStart` hook fires against it as soon as
    // the window exists. The pane — the session's identity — is filled in right
    // after tmux hands it over.
    let session_id = fresh_session_id(db)?;
    let mut row = Session::new(&quest.id, SessionRole::Worker, &label, &tmux_session, "");
    row.id = session_id.clone();
    row.status = SessionStatus::Starting;
    // Without `--workflow` a worker runs the Quest's, as the master does. The
    // Quest's own was checked when it was set, so only the flag is checked
    // here — see `crate::workflows`.
    row.workflow = workflow.or_else(|| quest.workflow.clone());
    row.first_prompt = prompt.map(str::to_string);
    // The name `claude -n` is given below (SPEC §6), recorded so the registry's
    // identity check has something true to compare against.
    row.claude_name = Some(crate::naming::claude_name(&quest.slug, &label));
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
        command: Some(claude_command(&quest.slug, &label, prompt)),
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
    Ok(Spawned {
        quest: quest.clone(),
        session,
        tmux_session,
        window,
    })
}

/// Emit the spawn result and, only when the caller sits in the Quest's own tmux
/// session, select the new window (`Select`, never an attach: no client ever
/// changes session here — `new-window -d` left it alone).
fn report_and_attach(ctx: &Ctx, spawned: &Spawned, no_attach: bool) -> anyhow::Result<()> {
    let Spawned {
        quest,
        session,
        tmux_session,
        window,
    } = spawned;
    let attaching = !no_attach && in_tmux_session(ctx, &quest.id, tmux_session);
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
    fn auto_label_is_wn_and_skips_a_live_collision() {
        assert_eq!(auto_label(&[], 1), "w1");
        let mut rows = vec![
            session(SessionRole::Master, MASTER),
            session(SessionRole::Worker, "tests"),
        ];
        // Next index is 2, and `w2` is free.
        assert_eq!(auto_label(&rows, next_worker_index(&rows)), "w2");
        // A live `w2` (say a hand-named worker) pushes the auto one to `w3`.
        rows.push(session(SessionRole::Worker, "w2"));
        assert_eq!(auto_label(&rows, 2), "w3");
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
