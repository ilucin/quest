//! `q phase "<text>"` — a session reports what it is doing now (SPEC §7).

use crate::Ctx;
use crate::commands::report;
use crate::error::QError;
use crate::output;

pub fn run(ctx: &Ctx, text: &str, quest: Option<&str>) -> anyhow::Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Err(QError::Invalid("phase text must not be empty".to_string()).into());
    }
    let target = report::resolve(ctx, quest)?;
    let session = target.require_session()?;
    let db = ctx.db()?;
    let session = db.update_session_phase(&session.id, text)?;
    let event = db.append_event(
        &target.quest.id,
        Some(&session.id),
        "phase",
        &serde_json::json!({ "text": text }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest_id": target.quest.id,
                "session_id": session.id,
                "phase": text,
                "event_id": event.id,
            }),
            || format!("phase set: {text}"),
        )?;
    }
    Ok(())
}
