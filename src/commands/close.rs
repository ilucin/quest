//! `q close` — end every session, kill the tmux session, mark the Quest
//! finished (SPEC §5).

use crate::Ctx;
use crate::beads;
use crate::commands::{confirm, live, sweep_quiet};
use crate::model::{Quest, QuestState, now};
use crate::output;
use crate::tmux::session_name;

/// What a close did, for the payload of whatever asked for it.
pub struct Closed {
    pub quest: Quest,
    /// The Quest was already finished, so only the epic could still be done.
    pub already_finished: bool,
    pub sessions_ended: usize,
    pub epic_closed: bool,
}

impl Closed {
    /// The one-line human rendering, shared by `q close` and the TUI's prompt.
    pub fn describe(&self) -> String {
        if self.already_finished {
            return format!(
                "quest {} ({}) is already finished{}",
                self.quest.id,
                self.quest.slug,
                epic_note(&self.quest, self.epic_closed)
            );
        }
        format!(
            "closed {} ({}) · {} session(s) ended{}",
            self.quest.id,
            self.quest.slug,
            self.sessions_ended,
            epic_note(&self.quest, self.epic_closed)
        )
    }
}

pub fn run(ctx: &Ctx, target: &str, force: bool, close_epic: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(target)?;

    if !force && let Some(question) = confirmation(ctx, &quest, close_epic)? {
        confirm(ctx, &question)?;
    }
    let out = apply(ctx, &quest, close_epic);
    // Before the payload, exactly where the `eprintln!`s used to land.
    crate::commands::flush_warnings(ctx);
    let out = out?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": out.quest,
                "already_finished": out.already_finished,
                "sessions_ended": out.sessions_ended,
                "epic_closed": out.epic_closed,
            }),
            || out.describe(),
        )?;
    }
    Ok(())
}

/// The question to put before closing, or `None` when there is nothing left to
/// ask about — an already-finished Quest whose epic is done, or was never
/// asked for, has no side effect to confirm.
///
/// Split out of [`apply`] so the TUI's prompt asks the same thing the terminal
/// does, in a place where `confirm`'s read of stdin would be fatal.
pub fn confirmation(ctx: &Ctx, quest: &Quest, close_epic: bool) -> anyhow::Result<Option<String>> {
    if quest.state == QuestState::Finished {
        if !close_epic {
            return Ok(None);
        }
        let Some(epic) = epic_pending(ctx, quest)? else {
            return Ok(None);
        };
        return Ok(Some(format!("close beads epic {epic}?")));
    }
    let epic = match beads::epic_of(quest).filter(|_| close_epic) {
        Some(epic) => format!(" and its beads epic {epic}"),
        None => String::new(),
    };
    Ok(Some(format!(
        "close quest {}{epic} (kills tmux session {})?",
        quest.slug,
        session_name(&ctx.config, &quest.slug)
    )))
}

/// The close itself: kill the tmux session, end every live session row, close
/// the epic if asked, mark the Quest finished (SPEC §5). Confirmation is the
/// caller's — [`confirmation`] above builds the question.
pub fn apply(ctx: &Ctx, quest: &Quest, close_epic: bool) -> anyhow::Result<Closed> {
    let db = ctx.db()?;
    let tmux_session = session_name(&ctx.config, &quest.slug);

    // Closing twice is not an error; there is nothing left to do but the epic,
    // which is worth a second run when the first one did not ask for it.
    if quest.state == QuestState::Finished {
        let epic_closed = close_epic && close_epic_again(ctx, quest)?;
        return Ok(Closed {
            quest: quest.clone(),
            already_finished: true,
            sessions_ended: 0,
            epic_closed,
        });
    }

    if ctx.tmux().has_session(&tmux_session)? {
        ctx.tmux().kill_session(&tmux_session)?;
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let ending: Vec<&crate::model::Session> = live(&sessions).collect();
    let ts = now();
    for session in &ending {
        db.mark_session_ended(&session.id, ts)?;
        db.append_event(
            &quest.id,
            Some(&session.id),
            "session.end",
            &serde_json::json!({ "reason": "quest_closed" }),
        )?;
        // Each row goes live -> ended exactly once here, so the transition is
        // the de-dupe (SPEC §20).
        crate::notify::emit(
            &ctx.config.notify,
            crate::notify::runner().as_ref(),
            crate::notify::Kind::Ended,
            &format!("{} · ended", quest.slug),
            &format!("{} ended", session.label),
        );
    }
    // TODO(M2): `--summarize` (brain).
    let epic_closed = close_epic && close_the_epic(ctx, quest);
    let quest = db.update_quest_state(&quest.id, QuestState::Finished, Some(ts))?;
    db.append_event(
        &quest.id,
        None,
        "quest.closed",
        &serde_json::json!({ "sessions_ended": ending.len() }),
    )?;

    Ok(Closed {
        quest,
        already_finished: false,
        sessions_ended: ending.len(),
        epic_closed,
    })
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
/// already done.
fn close_epic_again(ctx: &Ctx, quest: &Quest) -> anyhow::Result<bool> {
    let Some(epic) = beads::epic_of(quest) else {
        ctx.warn(format!(
            "warning: --close-epic: quest {} has no beads epic",
            quest.slug
        ));
        return Ok(false);
    };
    // Deliberately asked again rather than reusing what `confirmation` found:
    // between the two lies a `[y/N]` blocked on stdin, which can be minutes,
    // and closing an epic somebody else closed in that window is the write
    // this check exists to prevent.
    if epic_pending(ctx, quest)?.is_none() {
        ctx.warn(format!(
            "note: beads epic {epic} was already closed by an earlier `q close`"
        ));
        return Ok(false);
    }
    Ok(close_the_epic(ctx, quest))
}

/// The epic a `--close-epic` still has to close: `None` when the Quest has no
/// epic, or an earlier `q close --close-epic` already closed it — the recorded
/// event is the proof.
fn epic_pending<'a>(ctx: &Ctx, quest: &'a Quest) -> anyhow::Result<Option<&'a str>> {
    let Some(epic) = beads::epic_of(quest) else {
        return Ok(None);
    };
    let done = !ctx
        .db()?
        .list_events_by_kinds(&quest.id, &["beads.epic_closed"], 1)?
        .is_empty();
    Ok((!done).then_some(epic))
}

/// `--close-epic`: closes the Quest's beads epic (SPEC §13). A missing epic or
/// an unreachable `bd` is a warning on the `Ctx` — the Quest still closes, and
/// nothing is written to a screen the caller may own.
fn close_the_epic(ctx: &Ctx, quest: &Quest) -> bool {
    let Some(epic) = beads::epic_of(quest) else {
        ctx.warn(format!(
            "warning: --close-epic: quest {} has no beads epic",
            quest.slug
        ));
        return false;
    };
    match ctx.bd().close(epic, "quest closed") {
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
            ctx.warn(format!("warning: `bd close {epic}` failed: {e}"));
            false
        }
    }
}
