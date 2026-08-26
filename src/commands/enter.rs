//! `q enter` — attach to a Quest's tmux session, master window first (SPEC §6).

use std::io::Write;

use crate::Ctx;
use crate::commands::new::MASTER;
use crate::commands::{attach_mode, live, sweep_quiet};
use crate::error::QError;
use crate::model::QuestState;
use crate::output;
use crate::tmux::{session_name, window_of};

pub fn run(ctx: &Ctx, target: &str, label: Option<&str>) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished; run `q resume {}`",
            quest.slug, quest.slug
        ))
        .into());
    }
    let tmux_session = session_name(&ctx.config, &quest.slug);
    if !ctx.tmux().has_session(&tmux_session)? {
        return Err(QError::Tmux(format!(
            "no tmux session `{tmux_session}`; run `q resume {}`",
            quest.slug
        ))
        .into());
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let wanted = label.unwrap_or(MASTER);
    let session = match live(&sessions).find(|s| s.label == wanted) {
        Some(session) => session,
        // The tmux session can outlive its master window; attaching would then
        // land on whatever window is left instead of the Quest's master.
        None if label.is_none() => {
            return Err(QError::Other(format!(
                "master session of {} ended; run `q resume {}`",
                quest.slug, quest.slug
            ))
            .into());
        }
        None => {
            let known: Vec<&str> = live(&sessions).map(|s| s.label.as_str()).collect();
            let live_labels = if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            };
            return Err(QError::NotFound(format!(
                "session `{wanted}` in quest {} (live: {live_labels})",
                quest.slug
            ))
            .into());
        }
    };
    // A row inserted by a spawn that then died has no pane. tmux reads an
    // empty target as "whatever is active", so entering it would land on the
    // master while claiming to be the worker. The sweep ends such a row a few
    // seconds in; until then, say so.
    if session.tmux_pane.is_empty() {
        return Err(QError::Other(format!(
            "session `{wanted}` of {} has no pane yet; it never finished starting",
            quest.slug
        ))
        .into());
    }
    // The pane is the session's identity (SPEC §6); the window name is only
    // ever reported, and tmux is the one that knows it.
    let pane = session.tmux_pane.clone();
    let window = window_of(ctx.tmux(), &pane).unwrap_or_else(|| session.label.clone());

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": session,
                "tmux_session": tmux_session,
                "window": window,
                "attach": attach_mode(ctx, true),
            }),
            || format!("attaching to {tmux_session}:{window}"),
        )?;
    }
    // A real attach replaces this process, so nothing buffered survives it.
    std::io::stdout().flush()?;
    ctx.tmux().attach(&tmux_session, Some(&pane))
}
