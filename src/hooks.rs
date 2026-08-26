//! Claude Code hook handlers (SPEC §7): `q hook session-start | user-prompt-submit
//! | stop | notification | pre-compact | session-end`. Each reads the hook payload
//! from stdin, updates the session row and appends an event.
//!
//! A hook must never break Claude: every path exits 0, and only `SessionStart`
//! ever writes to stdout (the brief as `additionalContext`). Outside a Quest
//! pane — no `$Q_QUEST` — nothing is read or written.

use std::io::{Read, Write};

use serde_json::{Value, json};

use crate::brief::{self, Opts};
use crate::db::Db;
use crate::model::{Session, SessionStatus, now};

/// Stored on the session row.
const LAST_PROMPT_CHARS: usize = 500;
/// Stored in the `session.prompt` event payload.
const EVENT_PROMPT_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    Stop,
    Notification,
    PreCompact,
    SessionEnd,
}

/// Entry point for the dispatcher. Never fails: a hook that errors would
/// surface in Claude's UI, so problems are swallowed and the exit code is 0.
pub fn run(event: Event) -> anyhow::Result<u8> {
    let mut raw = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut raw);
    if !in_quest_pane() {
        return Ok(0);
    }
    let payload: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    if let Some(out) = handle(event, &payload) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(out.as_bytes());
        let _ = stdout.write_all(b"\n");
        let _ = stdout.flush();
    }
    Ok(0)
}

fn in_quest_pane() -> bool {
    env_non_empty("Q_QUEST").is_some()
}

fn env_non_empty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

/// Returns what to print on stdout, if anything.
fn handle(event: Event, payload: &Value) -> Option<String> {
    let db = Db::open_default().ok()?;
    let session = resolve_session(&db)?;
    match event {
        Event::SessionStart => session_start(&db, &session, payload),
        Event::UserPromptSubmit => {
            user_prompt_submit(&db, &session, payload);
            None
        }
        Event::Stop => {
            stop(&db, &session, payload);
            None
        }
        Event::Notification => {
            notification(&db, &session, payload);
            None
        }
        Event::PreCompact => {
            pre_compact(&db, &session, payload);
            None
        }
        Event::SessionEnd => {
            session_end(&db, &session, payload);
            None
        }
    }
}

/// `$Q_SESSION`, else the live session in `$TMUX_PANE` (SPEC §23 #2). The
/// session must belong to `$Q_QUEST`; a stale env from another Quest is not
/// ours to update.
fn resolve_session(db: &Db) -> Option<Session> {
    let quest_id = env_non_empty("Q_QUEST")?;
    let session = match env_non_empty("Q_SESSION") {
        Some(id) => db.get_session(&id).ok().flatten(),
        None => db
            .find_session_by_pane(&env_non_empty("TMUX_PANE")?)
            .ok()
            .flatten(),
    }?;
    (session.quest_id == quest_id).then_some(session)
}

fn str_field<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

fn append(db: &Db, session: &Session, kind: &str, payload: Value) {
    let _ = db.append_event(&session.quest_id, Some(&session.id), kind, &payload);
}

fn session_start(db: &Db, session: &Session, payload: &Value) -> Option<String> {
    let source = str_field(payload, "source");
    let claude_session_id = str_field(payload, "session_id");
    // The hook is a child of Claude (possibly via a shell); the parent is the
    // closest thing to Claude's pid without walking the process tree.
    let pid = i64::from(std::os::unix::process::parent_id());
    let _ = db.record_session_start(&session.id, claude_session_id, Some(pid));
    append(db, session, "session.start", json!({ "source": source }));

    let quest = db.get_quest(&session.quest_id).ok().flatten()?;
    let mut context = format!(
        "You are running inside Quest `{}` (q), as session `{}` ({}). \
         The brief below is authoritative context for your work.",
        quest.slug, session.label, session.role
    );
    let opts = Opts {
        role: session.role,
        session: Some(session.id.clone()),
        ..Opts::default()
    };
    if let Ok(markdown) = brief::render(db, &quest, &opts) {
        context.push_str("\n\n");
        context.push_str(&markdown);
    }
    Some(
        json!({
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "additionalContext": context,
            }
        })
        .to_string(),
    )
}

fn user_prompt_submit(db: &Db, session: &Session, payload: &Value) {
    let prompt = str_field(payload, "prompt").unwrap_or("");
    let _ = db.record_session_prompt(&session.id, &truncate(prompt, LAST_PROMPT_CHARS));
    append(
        db,
        session,
        "session.prompt",
        json!({ "prompt": truncate(prompt, EVENT_PROMPT_CHARS) }),
    );
}

fn stop(db: &Db, session: &Session, payload: &Value) {
    let _ = db.update_session_status(&session.id, SessionStatus::Idle, None);
    append(
        db,
        session,
        "session.stop",
        json!({ "stop_hook_active": payload.get("stop_hook_active").and_then(Value::as_bool) }),
    );
    // TODO(bd-8lz.3): master auto-name check (SPEC §10) — regenerate the slug
    // in the background when `name_source = auto` and the input hash changed.
    // TODO(bd-8lz.3): master ctx reset check (SPEC §8) — when `ctx_pct` is at
    // or above the threshold, log `session.reset_scheduled` and spawn
    // `q reset <session> --delay 2` detached.
}

fn notification(db: &Db, session: &Session, payload: &Value) {
    let kind = str_field(payload, "notification_type");
    let message = str_field(payload, "message");
    let waiting_for = waiting_for(kind, message);
    let _ = db.update_session_status(&session.id, SessionStatus::Waiting, Some(&waiting_for));
    append(
        db,
        session,
        "session.waiting",
        json!({ "type": waiting_for, "message": message }),
    );
}

/// Claude's `notification_type` mapped onto q's `waiting_for` vocabulary;
/// older payloads carry only `message`, so that is sniffed as a fallback.
fn waiting_for(kind: Option<&str>, message: Option<&str>) -> String {
    match kind {
        Some("permission_prompt") => "permission".to_string(),
        Some("idle_prompt") => "input".to_string(),
        Some(other) if !other.is_empty() => other.to_string(),
        _ => {
            let msg = message.unwrap_or("").to_ascii_lowercase();
            if msg.contains("permission") {
                "permission".to_string()
            } else {
                "input".to_string()
            }
        }
    }
}

fn pre_compact(db: &Db, session: &Session, payload: &Value) {
    append(
        db,
        session,
        "session.compact",
        json!({ "trigger": str_field(payload, "trigger") }),
    );
}

fn session_end(db: &Db, session: &Session, payload: &Value) {
    let _ = db.mark_session_ended(&session.id, now());
    append(
        db,
        session,
        "session.end",
        json!({ "reason": str_field(payload, "reason") }),
    );
}

/// At most `max` chars, on a char boundary, with an ellipsis when cut.
fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_for_maps_known_types_and_sniffs_messages() {
        assert_eq!(waiting_for(Some("permission_prompt"), None), "permission");
        assert_eq!(waiting_for(Some("idle_prompt"), None), "input");
        assert_eq!(
            waiting_for(Some("elicitation_dialog"), None),
            "elicitation_dialog"
        );
        assert_eq!(
            waiting_for(None, Some("Claude needs your permission to use Bash")),
            "permission"
        );
        assert_eq!(
            waiting_for(None, Some("Claude is waiting for your input")),
            "input"
        );
        assert_eq!(waiting_for(Some(""), None), "input");
    }

    #[test]
    fn truncate_is_char_safe() {
        assert_eq!(truncate("  abc ", 10), "abc");
        assert_eq!(truncate("čćžšđ", 3), "čć…");
        assert_eq!(truncate("čćžšđ", 5), "čćžšđ");
    }
}
