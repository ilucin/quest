//! `q close` — end every session, kill the tmux session, mark the Quest
//! finished (SPEC §5).

use crate::Ctx;
use crate::commands::{confirm, live, sweep_quiet};
use crate::model::{QuestState, now};
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, force: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);

    // Closing twice is not an error; there is simply nothing left to do.
    if quest.state == QuestState::Finished {
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({
                    "quest": quest,
                    "already_finished": true,
                    "sessions_ended": 0,
                }),
                || format!("quest {} ({}) is already finished", quest.id, quest.slug),
            )?;
        }
        return Ok(());
    }

    if !force {
        confirm(&format!(
            "close quest {} (kills tmux session {tmux_session})?",
            quest.slug
        ))?;
    }

    if ctx.tmux().has_session(&tmux_session)? {
        ctx.tmux().kill_session(&tmux_session)?;
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let ending: Vec<String> = live(&sessions).map(|s| s.id.clone()).collect();
    let ts = now();
    for id in &ending {
        db.mark_session_ended(id, ts)?;
        db.append_event(
            &quest.id,
            Some(id),
            "session.end",
            &serde_json::json!({ "reason": "quest_closed" }),
        )?;
    }
    // TODO(M2): `--close-epic` (beads) and `--summarize` (brain).
    let quest = db.update_quest_state(&quest.id, QuestState::Finished, Some(ts))?;
    db.append_event(
        &quest.id,
        None,
        "quest.closed",
        &serde_json::json!({ "sessions_ended": ending.len() }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "already_finished": false,
                "sessions_ended": ending.len(),
            }),
            || {
                format!(
                    "closed {} ({}) · {} session(s) ended",
                    quest.id,
                    quest.slug,
                    ending.len()
                )
            },
        )?;
    }
    Ok(())
}
