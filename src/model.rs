//! Row types for the tables in `db/`, the enums behind their TEXT columns, and
//! the states that are derived rather than stored (SPEC §4).
//!
//! Scaffolding for the commands that land in later milestones; the M0 binary
//! only opens the database.
#![allow(dead_code)]

use std::fmt;
use std::hash::{BuildHasher, Hasher, RandomState};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unix seconds. Every timestamp column in the schema uses this.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Debug, Error)]
#[error("invalid {kind} `{value}`")]
pub struct ParseEnumError {
    pub kind: &'static str,
    pub value: String,
}

/// Declares a TEXT-column enum together with its wire spellings.
macro_rules! text_enum {
    ($name:ident, $kind:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseEnumError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok($name::$variant),)+
                    other => Err(ParseEnumError { kind: $kind, value: other.to_string() }),
                }
            }
        }

        impl rusqlite::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(self.as_str().into())
            }
        }
    };
}

text_enum!(QuestState, "quest state", { Active => "active", Finished => "finished" });
text_enum!(NameSource, "name source", {
    Manual => "manual",
    Auto => "auto",
    Template => "template",
});
text_enum!(SessionRole, "session role", { Master => "master", Worker => "worker" });
text_enum!(SessionStatus, "session status", {
    Starting => "starting",
    Busy => "busy",
    Idle => "idle",
    Waiting => "waiting",
    Ended => "ended",
});

/// What the user sees in `q list` / the TUI. Never stored — see `display_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayState {
    Active,
    Idle,
    Finished,
}

impl DisplayState {
    pub fn as_str(&self) -> &'static str {
        match self {
            DisplayState::Active => "active",
            DisplayState::Idle => "idle",
            DisplayState::Finished => "finished",
        }
    }
}

impl fmt::Display for DisplayState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quest {
    pub id: String,
    pub slug: String,
    pub name_source: NameSource,
    pub name_input_hash: Option<String>,
    pub goal: Option<String>,
    pub cwd: String,
    pub machine: String,
    pub state: QuestState,
    pub workflow: Option<String>,
    pub template_id: Option<String>,
    pub beads_epic: Option<String>,
    pub beads_repo: Option<String>,
    pub brain_session: Option<String>,
    /// NULL means "fall back to `[context] master_reset_pct`".
    pub ctx_reset_pct: Option<u8>,
    /// NULL means "fall back to `[context] auto_reset`".
    pub auto_reset: Option<bool>,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

impl Quest {
    /// A minimal row: the columns `q new` must supply, everything else empty.
    pub fn new(slug: &str, cwd: &str, machine: &str) -> Quest {
        let ts = now();
        Quest {
            id: new_id("q"),
            slug: slug.to_string(),
            name_source: NameSource::Manual,
            name_input_hash: None,
            goal: None,
            cwd: cwd.to_string(),
            machine: machine.to_string(),
            state: QuestState::Active,
            workflow: None,
            template_id: None,
            beads_epic: None,
            beads_repo: None,
            brain_session: None,
            ctx_reset_pct: None,
            auto_reset: None,
            created_at: ts,
            updated_at: ts,
            finished_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub quest_id: String,
    pub role: SessionRole,
    pub label: String,
    pub tmux_session: String,
    /// `%42` — the session's identity, stable across rename, `/clear`, restart.
    pub tmux_pane: String,
    pub claude_pid: Option<i64>,
    pub claude_session_id: Option<String>,
    pub claude_name: Option<String>,
    pub workflow: Option<String>,
    pub phase: Option<String>,
    pub status: SessionStatus,
    pub waiting_for: Option<String>,
    pub ctx_pct: Option<u8>,
    pub ctx_updated_at: Option<i64>,
    pub first_prompt: Option<String>,
    pub last_prompt: Option<String>,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub updated_at: i64,
}

impl Session {
    pub fn new(
        quest_id: &str,
        role: SessionRole,
        label: &str,
        tmux_session: &str,
        tmux_pane: &str,
    ) -> Session {
        let ts = now();
        Session {
            id: new_id("s"),
            quest_id: quest_id.to_string(),
            role,
            label: label.to_string(),
            tmux_session: tmux_session.to_string(),
            tmux_pane: tmux_pane.to_string(),
            claude_pid: None,
            claude_session_id: None,
            claude_name: None,
            workflow: None,
            phase: None,
            status: SessionStatus::Starting,
            waiting_for: None,
            ctx_pct: None,
            ctx_updated_at: None,
            first_prompt: None,
            last_prompt: None,
            started_at: ts,
            ended_at: None,
            updated_at: ts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub quest_id: String,
    pub session_id: Option<String>,
    pub ts: i64,
    pub kind: String,
    pub payload: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub id: i64,
    pub quest_id: String,
    /// Who added it; NULL for CLI/manual.
    pub session_id: Option<String>,
    pub kind: String,
    pub r#ref: String,
    pub title: Option<String>,
    pub meta: Option<serde_json::Value>,
    pub enriched_at: Option<i64>,
    pub created_at: i64,
}

impl Link {
    /// A manual reference: no session, no enrichment yet. `id` is assigned by
    /// the insert.
    pub fn new(quest_id: &str, kind: &str, r#ref: &str) -> Link {
        Link {
            id: 0,
            quest_id: quest_id.to_string(),
            session_id: None,
            kind: kind.to_string(),
            r#ref: r#ref.to_string(),
            title: None,
            meta: None,
            enriched_at: None,
            created_at: now(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub cwd: Option<String>,
    pub workflow: Option<String>,
    pub goal: Option<String>,
    pub master_prompt: Option<String>,
    pub beads_repo: Option<String>,
    pub create_brain: bool,
    pub tags: Option<Vec<String>>,
    pub run_count: i64,
    pub last_run_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Template {
    pub fn new(name: &str) -> Template {
        let ts = now();
        Template {
            id: new_id("t"),
            name: name.to_string(),
            description: None,
            cwd: None,
            workflow: None,
            goal: None,
            master_prompt: None,
            beads_repo: None,
            create_brain: false,
            tags: None,
            run_count: 0,
            last_run_at: None,
            created_at: ts,
            updated_at: ts,
        }
    }
}

/// `finished` wins; otherwise any session that is doing or awaiting something
/// makes the Quest `active`; anything else is `idle` (SPEC §4).
pub fn display_state(quest: &Quest, sessions: &[Session]) -> DisplayState {
    if quest.state == QuestState::Finished {
        return DisplayState::Finished;
    }
    let busy = sessions.iter().any(|s| {
        matches!(
            s.status,
            SessionStatus::Busy | SessionStatus::Waiting | SessionStatus::Starting
        )
    });
    if busy {
        DisplayState::Active
    } else {
        DisplayState::Idle
    }
}

/// Any session blocked on the human.
pub fn needs_you(sessions: &[Session]) -> bool {
    sessions.iter().any(|s| s.status == SessionStatus::Waiting)
}

/// `q-7f3a` / `s-3b9c` / `t-1a2b`. 16 bits of entropy, so callers must retry on
/// a UNIQUE collision — `Db::insert_quest` and friends do.
///
/// Randomness comes from `RandomState`, which std seeds from the OS and bumps
/// per instance; the nanosecond clock and a process-local counter keep ids
/// distinct within a single tight loop. No RNG crate needed for 4 hex digits.
pub fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{prefix}-{:04x}", hasher.finish() & 0xffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: SessionStatus) -> Session {
        let mut s = Session::new("q-0001", SessionRole::Worker, "w1", "q-x", "%1");
        s.status = status;
        s
    }

    fn quest(state: QuestState) -> Quest {
        let mut q = Quest::new("slug", "/tmp", "laptop");
        q.state = state;
        q
    }

    #[test]
    fn enums_round_trip_through_their_text_spellings() {
        for text in ["active", "finished"] {
            assert_eq!(QuestState::from_str(text).unwrap().as_str(), text);
        }
        for text in ["manual", "auto", "template"] {
            assert_eq!(NameSource::from_str(text).unwrap().as_str(), text);
        }
        for text in ["master", "worker"] {
            assert_eq!(SessionRole::from_str(text).unwrap().as_str(), text);
        }
        for text in ["starting", "busy", "idle", "waiting", "ended"] {
            assert_eq!(SessionStatus::from_str(text).unwrap().as_str(), text);
        }
    }

    #[test]
    fn unknown_enum_text_is_rejected() {
        let e = SessionStatus::from_str("napping").unwrap_err();
        assert_eq!(e.kind, "session status");
        assert!(e.to_string().contains("napping"), "{e}");
        assert!(QuestState::from_str("idle").is_err(), "idle is derived");
    }

    #[test]
    fn enums_serialize_as_snake_case() {
        let json = serde_json::to_string(&SessionStatus::Waiting).unwrap();
        assert_eq!(json, "\"waiting\"");
        assert_eq!(
            serde_json::to_string(&DisplayState::Finished).unwrap(),
            "\"finished\""
        );
    }

    #[test]
    fn derived_display_state() {
        use DisplayState as D;
        use QuestState as Q;
        use SessionStatus as S;
        let cases: &[(Q, &[S], D)] = &[
            (Q::Finished, &[], D::Finished),
            (Q::Finished, &[S::Busy], D::Finished),
            (Q::Finished, &[S::Waiting], D::Finished),
            (Q::Active, &[], D::Idle),
            (Q::Active, &[S::Ended], D::Idle),
            (Q::Active, &[S::Idle], D::Idle),
            (Q::Active, &[S::Idle, S::Ended], D::Idle),
            (Q::Active, &[S::Busy], D::Active),
            (Q::Active, &[S::Starting], D::Active),
            (Q::Active, &[S::Waiting], D::Active),
            (Q::Active, &[S::Idle, S::Busy], D::Active),
        ];
        for (state, statuses, want) in cases {
            let sessions: Vec<Session> = statuses.iter().copied().map(session).collect();
            let got = display_state(&quest(*state), &sessions);
            assert_eq!(got, *want, "{state:?} + {statuses:?}");
        }
    }

    #[test]
    fn derived_needs_you() {
        use SessionStatus as S;
        assert!(!needs_you(&[]));
        assert!(!needs_you(&[session(S::Busy), session(S::Idle)]));
        assert!(needs_you(&[session(S::Idle), session(S::Waiting)]));
    }

    #[test]
    fn generated_ids_are_prefixed_four_hex_digits() {
        let id = new_id("q");
        assert_eq!(id.len(), 6, "{id}");
        assert!(id.starts_with("q-"), "{id}");
        assert!(id[2..].chars().all(|c| c.is_ascii_hexdigit()), "{id}");
        assert!(
            id[2..].chars().all(|c| !c.is_ascii_uppercase()),
            "expected lowercase hex: {id}"
        );
    }

    #[test]
    fn generated_ids_rarely_repeat() {
        let ids: std::collections::HashSet<String> = (0..200).map(|_| new_id("s")).collect();
        // 16 bits of entropy: a handful of birthday collisions in 200 draws is
        // expected, wholesale repetition is not.
        assert!(ids.len() > 190, "only {} distinct ids", ids.len());
    }
}
