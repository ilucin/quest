//! `q rm` — delete a Quest and everything hanging off it (SPEC §5).

use crate::Ctx;
use crate::commands::{confirm, sweep_quiet};
use crate::error::QError;
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, force: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);

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
        confirm(&format!(
            "remove quest {} and all of its history?",
            quest.slug
        ))?;
    }

    db.delete_quest(&quest.id)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "removed": quest.id, "slug": quest.slug }),
            || format!("removed {} ({})", quest.id, quest.slug),
        )?;
    }
    Ok(())
}
