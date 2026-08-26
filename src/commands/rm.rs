//! `q rm` — delete a Quest and everything hanging off it (SPEC §5).

use crate::Ctx;
use crate::beads;
use crate::commands::{confirm, sweep_quiet};
use crate::error::QError;
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, force: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);
    // The epic outlives the row that pointed at it — `q rm` deletes history,
    // it does not decide what happens in a shared tracker. Naming it is the
    // difference between "orphaned" and "silently orphaned".
    let epic = beads::epic_of(&quest).map(str::to_string);

    if ctx.tmux().has_session(&tmux_session)? {
        if !force {
            return Err(QError::Other(format!(
                "quest {} still runs in tmux session {tmux_session}; \
                 close it first or pass -f",
                quest.slug
            ))
            .into());
        }
        ctx.tmux().kill_session(&tmux_session)?;
    } else if !force {
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

    db.delete_quest(&quest.id)?;
    beads::forget(&quest.id);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "removed": quest.id,
                "slug": quest.slug,
                "orphaned_epic": epic,
            }),
            || match epic.as_deref() {
                Some(epic) => format!(
                    "removed {} ({}) · beads epic {epic} is left open; close it with \
                     `bd close {epic}`",
                    quest.id, quest.slug
                ),
                None => format!("removed {} ({})", quest.id, quest.slug),
            },
        )?;
    }
    Ok(())
}
