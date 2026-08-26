//! `q note "<text>" [--blocker]` — a free-form entry on the Quest timeline.
//!
//! Payload contract shared with `q brief` (`brief::is_blocker`): `{"text"}`
//! for a plain note, `{"text", "blocker": true}` for a blocker. `blocker` is
//! never written as `false`, so plain notes render without noise.

use crate::Ctx;
use crate::commands::report;
use crate::error::QError;
use crate::output;

pub fn run(ctx: &Ctx, text: &str, blocker: bool, quest: Option<&str>) -> anyhow::Result<()> {
    let text = text.trim();
    if text.is_empty() {
        return Err(QError::Invalid("note text must not be empty".to_string()).into());
    }
    let target = report::resolve(ctx, quest)?;
    let mut payload = serde_json::json!({ "text": text });
    if blocker {
        payload["blocker"] = serde_json::Value::Bool(true);
    }
    let event = ctx
        .db()?
        .append_event(&target.quest.id, target.session_id(), "note", &payload)?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest_id": target.quest.id,
                "session_id": target.session_id(),
                "event_id": event.id,
                "text": text,
                "blocker": blocker,
            }),
            || {
                let tag = if blocker { " [blocker]" } else { "" };
                format!("note #{}{tag} {text}", event.id)
            },
        )?;
    }
    Ok(())
}
