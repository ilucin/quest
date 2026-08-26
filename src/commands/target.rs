//! `<session>` target resolution (SPEC §16): `<quest>/<label>`, a session id,
//! or a bare `<label>` when the caller sits inside a Quest (`$Q_QUEST`).
//!
//! Shared by `q peek`/`send`/`kill` — and by `q reset`/`q name` when they
//! land. `brief::resolve_session` stays as it is: it answers a narrower
//! question (which of *this* Quest's sessions is `--session`) and never needs
//! the database.

use serde::Serialize;

use crate::Ctx;
use crate::commands::{live, pane_pid};
use crate::error::QError;
use crate::model::{Quest, Session, SessionRole, SessionStatus};
use crate::registry::{self, Ask, Verdict};

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

    /// The two-source idle gate (SPEC §23 #5): the registry's opinion, and
    /// why send-keys should not happen — `None` when it may. Shared by
    /// `q send` and `q reset`, which both type into a live TUI.
    pub fn idle_gate(&self, ctx: &Ctx) -> (Verdict, Option<String>) {
        let session = &self.session;
        // The pane's pid is only needed when no hook ever recorded Claude's, so
        // it costs a `list-panes` exactly in the case the registry would
        // otherwise have nothing to say.
        let pane = session
            .claude_pid
            .is_none()
            .then(|| pane_pid(ctx, &session.tmux_pane))
            .flatten();
        // Identity, so a recycled pid cannot make another session's entry speak
        // for this one: `<slug>/<label>` is the name q launched Claude with, and
        // the session id is the exact match when a hook recorded one.
        let name = self.name();
        let verdict = registry::verdict(Ask {
            pid: session.claude_pid,
            pane_pid: pane,
            name: Some(&name),
            session_id: session.claude_session_id.as_deref(),
            now_ms: registry::now_ms(),
        });
        let refusal = gate(session.status, &verdict);
        (verdict, refusal)
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

/// Why the send should not happen, or `None` when the two sources agree the
/// session is between turns.
///
/// The database decides first: `busy` is mid-turn, `waiting` is sitting on a
/// prompt. For a database that says `idle` the registry can object — and one
/// that knows nothing (no pid, no file, a stale or unreadable one) never
/// objects on its own.
///
/// `starting` is the one row the registry can overrule. Without hooks the row
/// never leaves `starting`, so Claude's own "idle" is the only evidence there
/// is that it is up and between turns.
fn gate(status: SessionStatus, verdict: &Verdict) -> Option<String> {
    match status {
        SessionStatus::Idle => verdict.refuses().then(|| verdict.describe()),
        SessionStatus::Starting if verdict.agrees_idle() => None,
        other => Some(format!("q has it as {other} ({})", verdict.describe())),
    }
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

    fn busy_registry() -> Verdict {
        Verdict::Busy {
            status: "waiting".to_string(),
            waiting_for: Some("permission_prompt".to_string()),
            name: None,
        }
    }

    fn unknown_registry() -> Verdict {
        Verdict::Unknown {
            reason: "no entry for this session",
        }
    }

    #[test]
    fn an_idle_session_the_registry_agrees_on_passes() {
        assert_eq!(
            gate(SessionStatus::Idle, &Verdict::Idle { name: None }),
            None
        );
    }

    #[test]
    fn an_unknown_registry_does_not_block_an_idle_session() {
        assert_eq!(gate(SessionStatus::Idle, &unknown_registry()), None);
    }

    #[test]
    fn the_registry_overrides_a_stale_idle_row() {
        let reason = gate(SessionStatus::Idle, &busy_registry()).unwrap();
        assert_eq!(reason, "registry: waiting, waiting for permission_prompt");
    }

    #[test]
    fn a_database_that_says_not_idle_refuses_whatever_the_registry_says() {
        for status in [SessionStatus::Busy, SessionStatus::Waiting] {
            for verdict in [
                Verdict::Idle { name: None },
                unknown_registry(),
                busy_registry(),
            ] {
                let reason = gate(status, &verdict).unwrap_or_else(|| panic!("{status} passed"));
                assert!(reason.contains(status.as_str()), "{reason}");
            }
        }
    }

    /// The hooks-not-installed case: the row is stuck on `starting`, and only
    /// Claude's own registry can say the session is actually up and waiting.
    #[test]
    fn a_starting_row_the_registry_calls_idle_passes() {
        assert_eq!(
            gate(SessionStatus::Starting, &Verdict::Idle { name: None }),
            None
        );
        for verdict in [unknown_registry(), busy_registry()] {
            let reason = gate(SessionStatus::Starting, &verdict).unwrap();
            assert!(reason.contains("starting"), "{reason}");
        }
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
