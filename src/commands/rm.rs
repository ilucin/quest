//! `q rm` — delete a Quest and everything hanging off it (SPEC §5).

use serde::Serialize;

use crate::Ctx;
use crate::beads;
use crate::commands::{confirm, sweep_quiet};
use crate::error::QError;
use crate::model::Quest;
use crate::output;
use crate::tmux::session_name;

/// What a removal did, for the payload of whatever asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct Removed {
    #[serde(rename = "removed")]
    pub id: String,
    pub slug: String,
    /// The beads epic the row pointed at, left open in its shared tracker.
    pub orphaned_epic: Option<String>,
    /// Whether a live tmux session (master and every worker window) was killed.
    pub tmux_killed: bool,
}

impl Removed {
    /// The one-line human rendering, shared by `q rm` and the TUI's prompt.
    pub fn describe(&self) -> String {
        match self.orphaned_epic.as_deref() {
            Some(epic) => format!(
                "removed {} ({}) · beads epic {epic} is left open; close it with \
                 `bd close {epic}`",
                self.id, self.slug
            ),
            None => format!("removed {} ({})", self.id, self.slug),
        }
    }
}

pub fn run(ctx: &Ctx, target: &str, force: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);
    let epic = beads::epic_of(&quest).map(str::to_string);

    // Without `-f`: refuse a running Quest, or confirm a stopped one. The TUI's
    // Backspace prompt is the consent instead, so it calls `apply` directly.
    if !force {
        if ctx.tmux().has_session(&tmux_session)? {
            return Err(QError::Other(format!(
                "quest {} still runs in tmux session {tmux_session}; \
                 close it first or pass -f",
                quest.slug
            ))
            .into());
        }
        let epic_note = match epic.as_deref() {
            Some(epic) => format!(" (beads epic {epic} stays open)"),
            None => String::new(),
        };
        confirm(
            ctx,
            &format!(
                "remove quest {} and all of its history{epic_note}?",
                quest.slug
            ),
        )?;
    }

    let out = apply(ctx, &quest)?;
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &out, || out.describe())?;
    }
    Ok(())
}

/// Delete a Quest and everything hanging off it: its tmux session — master and
/// every worker window with it (SPEC §6) — its rows, and its cached progress.
/// The beads epic outlives the row that pointed at it; `q rm` deletes history,
/// it does not close issues in a shared tracker, so the id is reported rather
/// than silently orphaned.
///
/// The consent is the caller's: `run` confirms or takes `-f`, the TUI's prompt
/// stands in for it. Shared so `q rm -f` and the TUI cannot drift apart.
pub fn apply(ctx: &Ctx, quest: &Quest) -> anyhow::Result<Removed> {
    let db = ctx.db()?;
    let epic = beads::epic_of(quest).map(str::to_string);

    // The whole fleet (SPEC §6 v2): main and every `q-<slug>+*`, rowless panes
    // included. Works when the main is already gone but a worker lingers.
    let tmux_killed = crate::commands::kill_quest_fleet(ctx, quest)? > 0;
    db.delete_quest(&quest.id)?;
    beads::forget(&quest.id);
    Ok(Removed {
        id: quest.id.clone(),
        slug: quest.slug.clone(),
        orphaned_epic: epic,
        tmux_killed,
    })
}
