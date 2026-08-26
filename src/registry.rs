//! Claude Code's own session registry: one `<pid>.json` per running session
//! under `~/.claude/sessions/` (`$Q_CLAUDE_SESSIONS_DIR` overrides the
//! directory, so tests never read the real one).
//!
//! It is a *supplementary* source (SPEC §6): hooks are authoritative for
//! `session.status`, but a hook that never fired — or fired before Claude
//! started a fresh turn — leaves the database stale. Before send-keys into a
//! live TUI (SPEC §23 #5) the registry is the second opinion that keeps `q
//! send` from typing into a permission prompt.
//!
//! Claude owns the format, so every field is optional and unknown fields are
//! ignored: a registry `q` cannot understand must degrade to `Unknown`, never
//! to an error.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `$Q_CLAUDE_SESSIONS_DIR`, else `~/.claude/sessions`. `None` only when
/// neither is knowable, which makes every lookup `Unknown`.
pub fn dir() -> Option<PathBuf> {
    match std::env::var_os("Q_CLAUDE_SESSIONS_DIR") {
        Some(raw) if !raw.is_empty() => Some(PathBuf::from(raw)),
        _ => Some(dirs::home_dir()?.join(".claude").join("sessions")),
    }
}

/// What Claude reports a session is doing. Anything `q` does not know is
/// `Other`, and treated as "not provably idle".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Idle,
    Busy,
    Waiting,
    Other(String),
}

impl Status {
    pub fn parse(raw: &str) -> Status {
        match raw.trim().to_ascii_lowercase().as_str() {
            "idle" => Status::Idle,
            "busy" | "running" | "active" => Status::Busy,
            "waiting" | "waiting_for_input" | "blocked" => Status::Waiting,
            other => Status::Other(other.to_string()),
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            Status::Idle => "idle",
            Status::Busy => "busy",
            Status::Waiting => "waiting",
            Status::Other(raw) => raw,
        }
    }
}

/// One registry file. Only the fields `q` acts on are modelled; the rest of
/// Claude's payload is ignored on purpose.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub pid: Option<i64>,
    pub session_id: Option<String>,
    pub name: Option<String>,
    pub status: Option<Status>,
    /// What a `waiting` session is blocked on, when Claude says.
    pub waiting_for: Option<String>,
    pub cwd: Option<String>,
    /// Milliseconds since the epoch, as Claude writes it.
    pub status_updated_at: Option<i64>,
}

/// Claude writes camelCase and adds fields between versions; both are handled
/// here rather than in `Entry`, which stays a plain `q`-shaped struct.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEntry {
    #[serde(default)]
    pid: Option<i64>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    waiting_for: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    status_updated_at: Option<i64>,
    #[serde(default)]
    updated_at: Option<i64>,
}

/// The pure parser: registry JSON in, `Entry` out. `None` for anything that is
/// not a JSON object — a truncated or half-written file is indistinguishable
/// from no information at all.
pub fn parse(text: &str) -> Option<Entry> {
    // serde would happily read a JSON array as a struct with every field
    // defaulted; a registry file is an object or it is nothing.
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    if !value.is_object() {
        return None;
    }
    let raw: RawEntry = serde_json::from_value(value).ok()?;
    Some(Entry {
        pid: raw.pid,
        session_id: raw.session_id,
        name: raw.name.filter(|n| !n.is_empty()),
        status: raw.status.as_deref().map(Status::parse),
        waiting_for: raw.waiting_for.filter(|w| !w.is_empty()),
        cwd: raw.cwd,
        // `statusUpdatedAt` is the precise answer; older versions only wrote
        // `updatedAt`.
        status_updated_at: raw.status_updated_at.or(raw.updated_at),
    })
}

/// The entry for `pid` in `dir`, or `None` when the file is missing,
/// unreadable or unparseable. Never an error: the registry is advisory.
pub fn read_in(dir: &Path, pid: i64) -> Option<Entry> {
    let text = std::fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?;
    parse(&text)
}

/// The registry's opinion on one session, as `q send` consumes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// Claude says this session is between turns.
    Idle { name: Option<String> },
    /// Claude says it is mid-turn or blocked on a prompt.
    Busy {
        status: String,
        waiting_for: Option<String>,
        name: Option<String>,
    },
    /// No registry, no file for this pid, or no pid to look up: the database
    /// stands on its own.
    Unknown { reason: &'static str },
}

impl Verdict {
    /// Only a registry that actively disagrees blocks a send.
    pub fn refuses(&self) -> bool {
        matches!(self, Verdict::Busy { .. })
    }

    /// The parenthetical in a refusal or a confirmation.
    pub fn describe(&self) -> String {
        match self {
            Verdict::Idle { .. } => "registry: idle".to_string(),
            Verdict::Busy {
                status,
                waiting_for: Some(what),
                ..
            } => format!("registry: {status}, waiting for {what}"),
            Verdict::Busy { status, .. } => format!("registry: {status}"),
            Verdict::Unknown { reason } => format!("registry: {reason}"),
        }
    }
}

/// The verdict for the Claude process `pid`, looked up in `dir`.
pub fn verdict_in(dir: Option<&Path>, pid: Option<i64>) -> Verdict {
    let Some(dir) = dir else {
        return Verdict::Unknown {
            reason: "no sessions directory",
        };
    };
    // Until `SessionStart` fires there is no pid to look up.
    let Some(pid) = pid.filter(|p| *p > 0) else {
        return Verdict::Unknown {
            reason: "no claude pid on record",
        };
    };
    let Some(entry) = read_in(dir, pid) else {
        return Verdict::Unknown {
            reason: "no entry for this session",
        };
    };
    verdict_of(&entry)
}

/// The same decision as `verdict_in`, on an already-parsed entry.
pub fn verdict_of(entry: &Entry) -> Verdict {
    match &entry.status {
        Some(Status::Idle) => Verdict::Idle {
            name: entry.name.clone(),
        },
        Some(status) => Verdict::Busy {
            status: status.as_str().to_string(),
            waiting_for: entry.waiting_for.clone(),
            name: entry.name.clone(),
        },
        // A file without a status says only that the session exists.
        None => Verdict::Unknown {
            reason: "entry has no status",
        },
    }
}

/// `verdict_in` against the configured directory.
pub fn verdict(pid: Option<i64>) -> Verdict {
    verdict_in(dir().as_deref(), pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real file from `~/.claude/sessions/`, Claude Code 2.1.246.
    const REAL: &str = r#"{"pid":58391,"sessionId":"7f8a50ae-1cdb-47d3-8fa1-7640b846a8e9",
      "cwd":"/Users/ivan/Code/work","startedAt":1787740025932,
      "procStart":"Wed Aug 26 10:27:05 2026","version":"2.1.246","peerProtocol":1,
      "peerFeatures":["notify_idle","artifact_yield"],"kind":"interactive",
      "entrypoint":"cli","pidDomain":"darwin",
      "messagingSocketPath":"/tmp/cc-socks/58391.sock","name":"work-28",
      "nameSource":"derived","nameSince":1787740025932,"status":"busy",
      "updatedAt":1787740405457,"statusUpdatedAt":1787740405457}"#;

    #[test]
    fn a_real_registry_file_parses_and_ignores_what_q_does_not_use() {
        let entry = parse(REAL).unwrap();
        assert_eq!(entry.pid, Some(58391));
        assert_eq!(
            entry.session_id.as_deref(),
            Some("7f8a50ae-1cdb-47d3-8fa1-7640b846a8e9")
        );
        assert_eq!(entry.name.as_deref(), Some("work-28"));
        assert_eq!(entry.status, Some(Status::Busy));
        assert_eq!(entry.waiting_for, None);
        assert_eq!(entry.cwd.as_deref(), Some("/Users/ivan/Code/work"));
        assert_eq!(entry.status_updated_at, Some(1787740405457));
    }

    #[test]
    fn waiting_for_is_read_when_claude_writes_one() {
        let entry = parse(r#"{"status":"waiting","waitingFor":"permission_prompt"}"#).unwrap();
        assert_eq!(entry.status, Some(Status::Waiting));
        assert_eq!(entry.waiting_for.as_deref(), Some("permission_prompt"));
        assert!(verdict_of(&entry).refuses());
        assert_eq!(
            verdict_of(&entry).describe(),
            "registry: waiting, waiting for permission_prompt"
        );
    }

    #[test]
    fn status_spellings_map_onto_the_three_q_cares_about() {
        assert_eq!(Status::parse("idle"), Status::Idle);
        assert_eq!(Status::parse("IDLE"), Status::Idle);
        assert_eq!(Status::parse(" busy "), Status::Busy);
        assert_eq!(Status::parse("running"), Status::Busy);
        assert_eq!(Status::parse("waiting_for_input"), Status::Waiting);
        assert_eq!(
            Status::parse("hibernating"),
            Status::Other("hibernating".to_string())
        );
        assert_eq!(Status::parse("hibernating").as_str(), "hibernating");
    }

    #[test]
    fn an_unknown_status_never_reads_as_idle() {
        let entry = parse(r#"{"status":"hibernating"}"#).unwrap();
        let verdict = verdict_of(&entry);
        assert!(verdict.refuses(), "{verdict:?}");
        assert_eq!(verdict.describe(), "registry: hibernating");
    }

    #[test]
    fn an_older_entry_without_status_updated_at_falls_back_to_updated_at() {
        let entry = parse(r#"{"status":"idle","updatedAt":17}"#).unwrap();
        assert_eq!(entry.status_updated_at, Some(17));
        assert_eq!(verdict_of(&entry), Verdict::Idle { name: None });
    }

    #[test]
    fn an_entry_without_a_status_is_no_information() {
        let entry = parse(r#"{"pid":1,"name":"x"}"#).unwrap();
        assert!(matches!(verdict_of(&entry), Verdict::Unknown { .. }));
        assert!(!verdict_of(&entry).refuses());
    }

    #[test]
    fn an_empty_name_or_waiting_for_is_absent_rather_than_blank() {
        let entry = parse(r#"{"name":"","waitingFor":"","status":"idle"}"#).unwrap();
        assert_eq!(entry.name, None);
        assert_eq!(entry.waiting_for, None);
    }

    #[test]
    fn garbage_and_non_objects_parse_to_nothing() {
        for text in ["", "not json", "[]", "42", "null", r#"{"status":7}"#] {
            assert!(parse(text).is_none(), "accepted `{text}`");
        }
        // An empty object is valid, and says nothing.
        assert_eq!(parse("{}"), Some(Entry::default()));
    }

    #[test]
    fn a_missing_file_or_pid_is_unknown_not_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(read_in(dir.path(), 4242).is_none());
        for (pid, reason) in [
            (Some(4242), "no entry for this session"),
            (None, "no claude pid on record"),
            (Some(0), "no claude pid on record"),
        ] {
            assert_eq!(
                verdict_in(Some(dir.path()), pid),
                Verdict::Unknown { reason }
            );
        }
        assert_eq!(
            verdict_in(None, Some(1)),
            Verdict::Unknown {
                reason: "no sessions directory"
            }
        );
    }

    #[test]
    fn a_file_on_disk_is_read_by_pid() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("77.json"), REAL).unwrap();
        assert_eq!(
            read_in(dir.path(), 77).unwrap().name.as_deref(),
            Some("work-28")
        );
        let verdict = verdict_in(Some(dir.path()), Some(77));
        assert!(verdict.refuses(), "{verdict:?}");
        assert_eq!(verdict.describe(), "registry: busy");

        std::fs::write(
            dir.path().join("78.json"),
            r#"{"status":"idle","name":"alpha/master"}"#,
        )
        .unwrap();
        assert_eq!(
            verdict_in(Some(dir.path()), Some(78)),
            Verdict::Idle {
                name: Some("alpha/master".to_string())
            }
        );
    }
}
