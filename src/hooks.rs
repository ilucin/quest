//! Claude Code hook handlers (SPEC §7): `q hook session-start | user-prompt-submit
//! | stop | notification | pre-compact | session-end`. Each reads the hook payload
//! from stdin, updates the session row and appends an event — in one transaction,
//! so a lock timeout leaves the row and the log consistent.
//!
//! A hook must never break Claude: every path exits 0, and only `SessionStart`
//! ever writes to stdout (the brief as `additionalContext`). Outside a Quest
//! pane — no `$Q_QUEST` — or without an existing database, nothing is read or
//! written; a hook never creates the database.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use crate::brief::{self, Opts};
use crate::commands::fmt::{EVENT_PROMPT_CHARS, truncate};
use crate::db::Db;
use crate::model::{Session, SessionStatus, now};

/// Stored on the session row.
const LAST_PROMPT_CHARS: usize = 500;

/// Lock budgets, well inside the 10s/15s timeouts `q hook install` sets for
/// these hooks in Claude's settings (Claude's own default is 60s): a hook
/// that cannot get the write lock drops its write rather than stall the user.
const BUSY_MS: u32 = 3000;
/// SessionStart also renders the brief and runs once; it may wait longer.
const SESSION_START_BUSY_MS: u32 = 8000;

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
    let Ok(payload) = serde_json::from_slice::<Value>(&raw) else {
        return Ok(0);
    };
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
    let path = Db::path().ok()?;
    if !path.exists() {
        return None;
    }
    let busy_ms = match event {
        Event::SessionStart => SESSION_START_BUSY_MS,
        _ => BUSY_MS,
    };
    let db = Db::open_with_timeout(&path, busy_ms).ok()?;
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

fn append(db: &Db, session: &Session, kind: &str, payload: Value) -> anyhow::Result<()> {
    db.append_event(&session.quest_id, Some(&session.id), kind, &payload)?;
    Ok(())
}

/// Claude's pid, best effort. The hook runs under a shell wrapper, so when
/// the parent is a shell its parent is Claude; otherwise the parent is.
/// One `ps` call; any hiccup yields `None` rather than a wrong pid.
fn claude_pid() -> Option<i64> {
    let parent = std::os::unix::process::parent_id();
    let out = Command::new("ps")
        .args(["-o", "ppid=,comm=", "-p", &parent.to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields = text.split_whitespace();
    let grandparent: i64 = fields.next()?.parse().ok()?;
    let comm = Path::new(fields.next()?).file_name()?.to_str()?;
    let is_shell = matches!(comm, "sh" | "bash" | "zsh" | "dash" | "fish");
    Some(if is_shell {
        grandparent
    } else {
        i64::from(parent)
    })
}

fn session_start(db: &Db, session: &Session, payload: &Value) -> Option<String> {
    let source = str_field(payload, "source");
    let claude_session_id = str_field(payload, "session_id");
    let pid = claude_pid();
    let _ = db.transaction(|db| {
        db.record_session_start(&session.id, claude_session_id, pid)?;
        append(db, session, "session.start", json!({ "source": source }))
    });

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
    let prompt = str_field(payload, "prompt");
    let stored = prompt.map(|p| truncate(p, LAST_PROMPT_CHARS));
    let _ = db.transaction(|db| {
        db.record_session_prompt(&session.id, stored.as_deref())?;
        append(
            db,
            session,
            "session.prompt",
            json!({ "prompt": prompt.map(|p| truncate(p, EVENT_PROMPT_CHARS)) }),
        )
    });
}

fn stop(db: &Db, session: &Session, payload: &Value) {
    let _ = db.transaction(|db| {
        db.update_session_status(&session.id, SessionStatus::Idle, None)?;
        append(
            db,
            session,
            "session.stop",
            json!({ "stop_hook_active": payload.get("stop_hook_active").and_then(Value::as_bool) }),
        )
    });
    // TODO(bd-8lz.3): master auto-name check (SPEC §10) — regenerate the slug
    // in the background when `name_source = auto` and the input hash changed.
    crate::commands::reset::maybe_schedule(db, session);
}

/// Only notifications that block on the user flip the session to `waiting`;
/// the rest (`idle_prompt`, `auth_success`, …) are logged and leave the
/// status alone.
fn notification(db: &Db, session: &Session, payload: &Value) {
    let kind = str_field(payload, "notification_type");
    let message = str_field(payload, "message");
    let waiting_for = waiting_for(kind, message);
    let event_kind = if waiting_for.is_some() {
        "session.waiting"
    } else {
        "session.notification"
    };
    let _ = db.transaction(|db| {
        if let Some(waiting_for) = waiting_for {
            db.update_session_status(&session.id, SessionStatus::Waiting, Some(waiting_for))?;
        }
        append(
            db,
            session,
            event_kind,
            json!({ "type": kind, "waiting_for": waiting_for, "message": message }),
        )
    });
}

/// Claude's `notification_type` mapped onto q's `waiting_for` vocabulary.
/// Older payloads carry only `message`, so that is sniffed as a fallback.
fn waiting_for(kind: Option<&str>, message: Option<&str>) -> Option<&'static str> {
    match kind {
        Some("permission_prompt") => Some("permission"),
        Some("elicitation_dialog") => Some("input"),
        Some(other) if !other.is_empty() => None,
        _ => {
            let msg = message?.to_ascii_lowercase();
            if msg.contains("permission") {
                Some("permission")
            } else if msg.contains("waiting for your input") {
                Some("input")
            } else {
                None
            }
        }
    }
}

fn pre_compact(db: &Db, session: &Session, payload: &Value) {
    let _ = db.transaction(|db| {
        append(
            db,
            session,
            "session.compact",
            json!({ "trigger": str_field(payload, "trigger") }),
        )
    });
}

fn session_end(db: &Db, session: &Session, payload: &Value) {
    let _ = db.transaction(|db| {
        db.mark_session_ended(&session.id, now())?;
        append(
            db,
            session,
            "session.end",
            json!({ "reason": str_field(payload, "reason") }),
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiting_for_flags_only_blocking_notifications() {
        assert_eq!(
            waiting_for(Some("permission_prompt"), None),
            Some("permission")
        );
        assert_eq!(waiting_for(Some("elicitation_dialog"), None), Some("input"));
        assert_eq!(waiting_for(Some("idle_prompt"), None), None);
        assert_eq!(waiting_for(Some("auth_success"), Some("Signed in")), None);
        assert_eq!(
            waiting_for(None, Some("Claude needs your permission to use Bash")),
            Some("permission")
        );
        assert_eq!(
            waiting_for(None, Some("Claude is waiting for your input")),
            Some("input")
        );
        assert_eq!(waiting_for(Some(""), Some("Task done")), None);
        assert_eq!(waiting_for(None, None), None);
    }
}
