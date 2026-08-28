//! `q note "<text>" [--blocker]` — a free-form entry on the Quest timeline.
//! `q note --resolve <event-id>` — clear a blocker from brief §10.
//!
//! Payload contract shared with `q brief` (`brief::is_blocker`): `{"text"}`
//! for a plain note, `{"text", "blocker": true}` for a blocker. `blocker` is
//! never written as `false`, so plain notes render without noise.
//!
//! Resolution is append-only (SPEC principle: the timeline is regenerable from
//! the DB, never mutated): `--resolve <id>` writes a `note` event whose payload
//! is `{"resolves": <id>}`. `brief::section_blockers` reads those to hide the
//! referenced blockers. A double-resolve is a graceful no-op.

use crate::Ctx;
use crate::commands::report;
use crate::error::QError;
use crate::output;

pub fn run(
    ctx: &Ctx,
    text: Option<&str>,
    blocker: bool,
    resolve: Option<i64>,
    quest: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(target_id) = resolve {
        if text.is_some() {
            return Err(QError::Invalid(
                "note --resolve takes no text; resolve is standalone".to_string(),
            )
            .into());
        }
        return run_resolve(ctx, target_id, quest);
    }

    let text = text.unwrap_or("").trim();
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

/// `true` when the event is a blocker note (`{"blocker": true}`); mirrors
/// `brief::is_blocker`.
fn is_blocker(payload: Option<&serde_json::Value>) -> bool {
    payload
        .and_then(|p| p.get("blocker"))
        .and_then(serde_json::Value::as_bool)
        == Some(true)
}

fn run_resolve(ctx: &Ctx, target_id: i64, quest: Option<&str>) -> anyhow::Result<()> {
    let target = report::resolve(ctx, quest)?;
    let db = ctx.db()?;

    let event = db
        .event_in_quest(&target.quest.id, target_id)?
        .ok_or_else(|| QError::NotFound(format!("event #{target_id} not found in this quest")))?;
    if event.kind != "note" || !is_blocker(event.payload.as_ref()) {
        return Err(QError::Invalid(format!("event #{target_id} is not a blocker note")).into());
    }

    // Idempotent: if a resolution already references this blocker, do nothing.
    let notes = db.list_events_by_kinds(&target.quest.id, &["note"], usize::MAX)?;
    let already = notes.iter().any(|e| {
        e.payload
            .as_ref()
            .and_then(|p| p.get("resolves"))
            .and_then(serde_json::Value::as_i64)
            == Some(target_id)
    });

    let resolution_id = if already {
        None
    } else {
        let payload = serde_json::json!({ "resolves": target_id });
        Some(
            db.append_event(&target.quest.id, target.session_id(), "note", &payload)?
                .id,
        )
    };

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest_id": target.quest.id,
                "session_id": target.session_id(),
                "resolved_event_id": target_id,
                "event_id": resolution_id,
                "already_resolved": already,
            }),
            || {
                if already {
                    format!("blocker #{target_id} already resolved")
                } else {
                    let rid = resolution_id.unwrap_or_default();
                    format!("resolved blocker #{target_id} (note #{rid})")
                }
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::Quest;
    use crate::tmux::FixtureTmux;

    fn ctx_with(db: Db) -> Ctx {
        Ctx::for_tests(
            Config::default(),
            db,
            Box::new(FixtureTmux::new(std::path::PathBuf::from(
                "/nonexistent/tmux.json",
            ))),
        )
    }

    /// A Quest with one blocker note; returns its id and the blocker event id.
    fn seeded() -> (Db, String, i64) {
        let db = Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("alpha", "/tmp", "laptop"))
            .unwrap();
        let ev = db
            .append_event(
                &quest.id,
                None,
                "note",
                &serde_json::json!({ "text": "DB is locked", "blocker": true }),
            )
            .unwrap();
        (db, quest.id, ev.id)
    }

    fn resolutions(db: &Db, quest_id: &str, target: i64) -> usize {
        db.list_events_by_kinds(quest_id, &["note"], usize::MAX)
            .unwrap()
            .iter()
            .filter(|e| {
                e.payload
                    .as_ref()
                    .and_then(|p| p.get("resolves"))
                    .and_then(serde_json::Value::as_i64)
                    == Some(target)
            })
            .count()
    }

    fn code(err: anyhow::Error) -> &'static str {
        err.downcast_ref::<QError>()
            .map(QError::code)
            .unwrap_or("other")
    }

    #[test]
    fn resolving_a_blocker_appends_one_resolution_event() {
        let (db, quest_id, blocker) = seeded();
        let ctx = ctx_with(db);
        run(&ctx, None, false, Some(blocker), Some(&quest_id)).unwrap();

        let db = ctx.db().unwrap();
        assert_eq!(resolutions(db, &quest_id, blocker), 1);
        let payload = db
            .event_in_quest(&quest_id, blocker + 1)
            .unwrap()
            .unwrap()
            .payload
            .unwrap();
        assert_eq!(payload, serde_json::json!({ "resolves": blocker }));
    }

    #[test]
    fn double_resolve_is_a_graceful_no_op() {
        let (db, quest_id, blocker) = seeded();
        let ctx = ctx_with(db);
        run(&ctx, None, false, Some(blocker), Some(&quest_id)).unwrap();
        // Second call must not error and must not append a second resolution.
        run(&ctx, None, false, Some(blocker), Some(&quest_id)).unwrap();
        assert_eq!(resolutions(ctx.db().unwrap(), &quest_id, blocker), 1);
    }

    #[test]
    fn resolving_a_nonexistent_event_errors() {
        let (db, quest_id, blocker) = seeded();
        let ctx = ctx_with(db);
        let err = run(&ctx, None, false, Some(blocker + 999), Some(&quest_id)).unwrap_err();
        assert_eq!(code(err), "not_found");
    }

    #[test]
    fn resolving_a_plain_note_errors() {
        let db = Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("alpha", "/tmp", "laptop"))
            .unwrap();
        let plain = db
            .append_event(
                &quest.id,
                None,
                "note",
                &serde_json::json!({ "text": "hi" }),
            )
            .unwrap();
        let ctx = ctx_with(db);
        let err = run(&ctx, None, false, Some(plain.id), Some(&quest.id)).unwrap_err();
        assert_eq!(code(err), "invalid");
    }

    #[test]
    fn resolve_with_text_is_rejected() {
        let (db, quest_id, blocker) = seeded();
        let ctx = ctx_with(db);
        let err = run(&ctx, Some("text"), false, Some(blocker), Some(&quest_id)).unwrap_err();
        assert_eq!(code(err), "invalid");
    }

    #[test]
    fn empty_note_without_resolve_errors() {
        let (db, quest_id, _) = seeded();
        let ctx = ctx_with(db);
        let err = run(&ctx, None, false, None, Some(&quest_id)).unwrap_err();
        assert_eq!(code(err), "invalid");
    }
}
