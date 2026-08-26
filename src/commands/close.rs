//! `q close` — end every session, kill the tmux session, mark the Quest
//! finished (SPEC §5).

use crate::Ctx;
use crate::beads;
use crate::commands::{confirm, live, sweep_quiet};
use crate::model::{Quest, QuestState, now};
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, force: bool, close_epic: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);

    // Closing twice is not an error; there is nothing left to do but the epic,
    // which is worth a second run when the first one did not ask for it.
    if quest.state == QuestState::Finished {
        let epic_closed = close_epic && close_epic_again(ctx, &quest, force)?;
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({
                    "quest": quest,
                    "already_finished": true,
                    "sessions_ended": 0,
                    "epic_closed": epic_closed,
                }),
                || {
                    format!(
                        "quest {} ({}) is already finished{}",
                        quest.id,
                        quest.slug,
                        epic_note(&quest, epic_closed)
                    )
                },
            )?;
        }
        return Ok(());
    }

    if !force {
        let epic = match beads::epic_of(&quest).filter(|_| close_epic) {
            Some(epic) => format!(" and its beads epic {epic}"),
            None => String::new(),
        };
        confirm(
            ctx,
            &format!(
                "close quest {}{epic} (kills tmux session {tmux_session})?",
                quest.slug
            ),
        )?;
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
    // TODO(M2): `--summarize` (brain).
    let epic_closed = close_epic && close_the_epic(ctx, &quest);
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
                "epic_closed": epic_closed,
            }),
            || {
                format!(
                    "closed {} ({}) · {} session(s) ended{}",
                    quest.id,
                    quest.slug,
                    ending.len(),
                    epic_note(&quest, epic_closed)
                )
            },
        )?;
    }
    Ok(())
}

/// ` · epic bd-e closed`, for the one-liner — the epic is the half of what
/// `--close-epic` did that is not in the Quest's own row.
fn epic_note(quest: &Quest, epic_closed: bool) -> String {
    match beads::epic_of(quest) {
        Some(epic) if epic_closed => format!(" · epic {epic} closed"),
        _ => String::new(),
    }
}

/// `--close-epic` on a Quest that is already finished. The epic is a row in a
/// shared tracker, so a repeat run must neither write to it twice nor append a
/// second `beads.epic_closed` event: the recorded event is the proof it was
/// already done. Anything that does reach `bd` is confirmed first, since the
/// Quest itself needs no closing and the tracker write is the whole action.
fn close_epic_again(ctx: &Ctx, quest: &Quest, force: bool) -> anyhow::Result<bool> {
    let Some(epic) = beads::epic_of(quest) else {
        eprintln!(
            "warning: --close-epic: quest {} has no beads epic",
            quest.slug
        );
        return Ok(false);
    };
    if epic_already_closed(ctx, quest)? {
        eprintln!("note: beads epic {epic} was already closed by an earlier `q close`");
        return Ok(false);
    }
    if !force {
        confirm(ctx, &format!("close beads epic {epic}?"))?;
    }
    Ok(close_the_epic(ctx, quest))
}

/// Whether an earlier `q close --close-epic` already closed this epic.
fn epic_already_closed(ctx: &Ctx, quest: &Quest) -> anyhow::Result<bool> {
    let db = ctx.db()?;
    Ok(!db
        .list_events_by_kinds(&quest.id, &["beads.epic_closed"], 1)?
        .is_empty())
}

/// `--close-epic`: closes the Quest's beads epic (SPEC §13). A missing epic or
/// an unreachable `bd` is a warning — the Quest still closes.
fn close_the_epic(ctx: &Ctx, quest: &Quest) -> bool {
    let Some(epic) = beads::epic_of(quest) else {
        eprintln!(
            "warning: --close-epic: quest {} has no beads epic",
            quest.slug
        );
        return false;
    };
    match beads::client().close(epic, "quest closed") {
        Ok(()) => {
            let _ = ctx.db().and_then(|db| {
                db.append_event(
                    &quest.id,
                    None,
                    "beads.epic_closed",
                    &serde_json::json!({ "epic": epic }),
                )
            });
            true
        }
        Err(e) => {
            eprintln!("warning: `bd close {epic}` failed: {e}");
            false
        }
    }
}
