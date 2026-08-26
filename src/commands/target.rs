//! `<session>` target resolution (SPEC §16): `<quest>/<label>`, a session id,
//! or a bare `<label>` when the caller sits inside a Quest (`$Q_QUEST`).
//!
//! Shared by `q peek`/`send`/`kill` — and by `q reset`/`q name` when they
//! land. `brief::resolve_session` stays as it is: it answers a narrower
//! question (which of *this* Quest's sessions is `--session`) and never needs
//! the database.

use serde::Serialize;

use crate::Ctx;
use crate::commands::live;
use crate::error::QError;
use crate::model::{Quest, Session, SessionRole, SessionStatus};

/// A resolved session together with the Quest it belongs to; every session
/// command needs both (the Quest for the tmux session name and the payload).
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub quest: Quest,
    pub session: Session,
}

impl Target {
    /// `<slug>/<label>` — how a session is named in output and to the user.
    pub fn name(&self) -> String {
        format!("{}/{}", self.quest.slug, self.session.label)
    }

    pub fn ended(&self) -> bool {
        self.session.status == SessionStatus::Ended
    }

    /// For the commands that can only act on a live pane (`q peek`, `q send`).
    pub fn require_live(&self) -> anyhow::Result<()> {
        if self.ended() {
            return Err(QError::Other(format!(
                "session {} ({}) has ended",
                self.name(),
                self.session.id
            ))
            .into());
        }
        Ok(())
    }
}

/// Resolves `target` against the database.
///
/// Stages, first hit wins:
/// 1. an exact session id, in any Quest;
/// 2. `<quest>/<label>`, where `<quest>` goes through the usual Quest
///    resolution (id, slug, unique fragment);
/// 3. a bare `<label>` inside the Quest named by `$Q_QUEST`;
/// 4. a bare `<label>` among the live sessions of every Quest;
/// 5. a bare `<label>` among *all* sessions, so an ended one gives a better
///    error than "not found".
///
/// Within a Quest a live session always beats an ended one of the same label —
/// a label is reused after its worker is gone. Two matches at the same stage
/// are ambiguous and listed.
pub fn resolve(ctx: &Ctx, target: &str) -> anyhow::Result<Target> {
    let target = target.trim();
    if target.is_empty() {
        return Err(QError::NotFound("session ``".to_string()).into());
    }
    let db = ctx.db()?;

    if let Some(session) = db.get_session(target)? {
        let quest = db.resolve_quest(&session.quest_id)?;
        return Ok(Target { quest, session });
    }

    if let Some((quest_target, label)) = target.split_once('/') {
        let quest = db.resolve_quest(quest_target)?;
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        let session =
            pick(&sessions, label).ok_or_else(|| not_found_in(&quest, &sessions, label))?;
        return Ok(Target {
            quest,
            session: session.clone(),
        });
    }

    // A stale `$Q_QUEST` (a deleted Quest) must not fail the lookup — the
    // later stages can still resolve the label.
    if let Some(quest) = env("Q_QUEST").and_then(|id| db.resolve_quest(&id).ok()) {
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        if let Some(session) = pick(&sessions, target) {
            return Ok(Target {
                quest,
                session: session.clone(),
            });
        }
    }

    // Outside a Quest — or for a label the current Quest does not have — a
    // unique label across the fleet is unambiguous enough to act on.
    let mut all: Vec<(Quest, Session)> = Vec::new();
    for quest in db.list_quests(true)? {
        for session in db.list_sessions_by_quest(&quest.id)? {
            all.push((quest.clone(), session));
        }
    }
    for only_live in [true, false] {
        let matches: Vec<&(Quest, Session)> = all
            .iter()
            .filter(|(_, s)| s.label == target)
            .filter(|(_, s)| !only_live || s.status != SessionStatus::Ended)
            .collect();
        match matches.len() {
            0 => continue,
            1 => {
                let (quest, session) = matches[0];
                return Ok(Target {
                    quest: quest.clone(),
                    session: session.clone(),
                });
            }
            _ => {
                return Err(QError::Ambiguous {
                    target: target.to_string(),
                    candidates: matches
                        .iter()
                        .map(|(q, s)| format!("{}/{} ({})", q.slug, s.label, s.id))
                        .collect(),
                }
                .into());
            }
        }
    }
    Err(QError::NotFound(format!("session `{target}`")).into())
}

/// The session `label` names inside one Quest: the live one, else the most
/// recently ended one.
fn pick<'a>(sessions: &'a [Session], label: &str) -> Option<&'a Session> {
    if let Some(hit) = sessions.iter().find(|s| s.id == label) {
        return Some(hit);
    }
    live(sessions).find(|s| s.label == label).or_else(|| {
        sessions
            .iter()
            .filter(|s| s.label == label)
            .max_by_key(|s| (s.ended_at, s.started_at))
    })
}

fn not_found_in(quest: &Quest, sessions: &[Session], label: &str) -> anyhow::Error {
    // Master first, then the workers in spawn order — `started_at` is
    // second-precision, so the label breaks a same-second tie and the message
    // reads the same twice in a row.
    let mut rows: Vec<&Session> = live(sessions).collect();
    rows.sort_by_key(|s| (s.role != SessionRole::Master, s.started_at, &s.label));
    let known: Vec<&str> = rows.into_iter().map(|s| s.label.as_str()).collect();
    let live_labels = if known.is_empty() {
        "none".to_string()
    } else {
        known.join(", ")
    };
    QError::NotFound(format!(
        "session `{label}` in quest {} (live: {live_labels})",
        quest.slug
    ))
    .into()
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(label: &str, status: SessionStatus, ended_at: Option<i64>) -> Session {
        let mut s = Session::new("q-0001", SessionRole::Worker, label, "q-alpha", "%1");
        s.status = status;
        s.ended_at = ended_at;
        s
    }

    #[test]
    fn pick_prefers_a_live_session_over_an_ended_one_with_the_same_label() {
        let rows = vec![
            session("tests", SessionStatus::Ended, Some(10)),
            session("tests", SessionStatus::Idle, None),
        ];
        assert_eq!(pick(&rows, "tests").unwrap().status, SessionStatus::Idle);
    }

    #[test]
    fn pick_falls_back_to_the_most_recently_ended_session() {
        let rows = vec![
            session("tests", SessionStatus::Ended, Some(10)),
            session("tests", SessionStatus::Ended, Some(99)),
            session("other", SessionStatus::Idle, None),
        ];
        assert_eq!(pick(&rows, "tests").unwrap().ended_at, Some(99));
        assert!(pick(&rows, "nope").is_none());
    }

    #[test]
    fn pick_also_answers_to_a_session_id() {
        let rows = vec![session("tests", SessionStatus::Idle, None)];
        let id = rows[0].id.clone();
        assert_eq!(pick(&rows, &id).unwrap().label, "tests");
    }

    #[test]
    fn the_not_found_error_lists_the_live_labels() {
        let quest = Quest::new("alpha", "/tmp", "laptop");
        let mut master = session("master", SessionStatus::Idle, None);
        master.role = SessionRole::Master;
        master.started_at = 900;
        let rows = vec![
            session("tests", SessionStatus::Idle, None),
            master,
            session("gone", SessionStatus::Ended, Some(1)),
        ];
        let msg = not_found_in(&quest, &rows, "nope").to_string();
        assert!(msg.contains("`nope`"), "{msg}");
        // The master leads even though it started last.
        assert!(msg.contains("live: master, tests"), "{msg}");
        assert!(!msg.contains("gone"), "{msg}");
        let empty = not_found_in(&quest, &[], "nope").to_string();
        assert!(empty.contains("live: none"), "{empty}");
    }
}
