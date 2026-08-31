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

pub fn run(
    ctx: &Ctx,
    target: &str,
    force: bool,
    close_epic: bool,
    summarize: bool,
) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(target)?;

    if !force && let Some(question) = confirmation(ctx, &quest, close_epic)? {
        confirm(ctx, &question)?;
    }
    let out = apply(ctx, &quest, close_epic);
    // Before the payload, exactly where the `eprintln!`s used to land.
    crate::commands::flush_warnings(ctx);
    let out = out?;

    // SPEC §14: `--summarize` proposes a brain knowledge summary after the
    // close. Best effort — a missing brain, no session, an unavailable `claude`
    // or a declined prompt all leave the Quest closed without a summary.
    if summarize {
        maybe_summarize(ctx, &out.quest);
        crate::commands::flush_warnings(ctx);
    }

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

    // Kill the whole fleet (SPEC §6 v2): the main `q-<slug>` and every worker
    // `q-<slug>+*`, including a pane that has no row — best effort, so one that
    // is already gone does not fail the close.
    crate::commands::kill_quest_fleet(ctx, quest)?;

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

/// `q close --summarize` (SPEC §14): proposes a brain knowledge summary, then —
/// if the human confirms — hands the Quest brief to `claude -p`, which writes a
/// `knowledge/…` note and prints its path; that path is recorded as
/// `summarized_to:` on the session note.
///
/// Best effort throughout: no `brain_session`, no brain root, a declined or
/// impossible prompt (no terminal, `--json`), or an unavailable `claude` are
/// each a buffered warning at most — the Quest is already closed.
fn maybe_summarize(ctx: &Ctx, quest: &Quest) {
    let Some(slug) = quest.brain_session.as_deref() else {
        ctx.warn(format!(
            "warning: --summarize: quest {} has no brain session to summarize into",
            quest.slug
        ));
        return;
    };
    let Some(root) = crate::brain::root() else {
        ctx.warn("warning: --summarize: no brain root; skipping the summary".to_string());
        return;
    };
    // PROPOSES; the human confirms. A refusal, or no terminal to ask, is a
    // skip — the close itself already happened and must not be undone.
    if confirm(
        ctx,
        &format!("summarize quest {slug} into the brain via claude?"),
    )
    .is_err()
    {
        ctx.warn(format!(
            "note: --summarize declined; {slug} left without a summary"
        ));
        return;
    }
    let brief = match crate::brief::render(
        ctx.db().expect("db open for close"),
        quest,
        &crate::brief::Opts {
            workflows: ctx.workflows(),
            ..Default::default()
        },
    ) {
        Ok(brief) => brief,
        Err(e) => {
            ctx.warn(format!(
                "warning: --summarize: could not build the brief ({e:#})"
            ));
            return;
        }
    };
    let Some(path) = crate::brain::summarizer().summarize(&brief) else {
        ctx.warn("warning: --summarize: claude is unavailable; no summary written".to_string());
        return;
    };
    match crate::brain::set_summarized_to(&root, slug, &path) {
        Ok(true) => ctx.warn(format!("note: summarized {slug} into {path}")),
        Ok(false) => ctx.warn(format!(
            "warning: --summarize: session note for {slug} not found; summary at {path}"
        )),
        Err(e) => ctx.warn(format!(
            "warning: --summarize: summary written to {path} but not recorded on {slug}: {e}"
        )),
    }
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
