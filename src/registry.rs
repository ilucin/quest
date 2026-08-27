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

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// `$Q_CLAUDE_SESSIONS_DIR`, else `~/.claude/sessions`. `None` only when
/// neither is knowable, which makes every lookup `Unknown`.
///
/// Under `cfg(test)` there is no default at all — only what a test points it
/// at ([`dir_override`]), so no in-crate test can read the developer's own
/// registry. The same rule `beads::cache_root` follows, for the same reason.
pub fn dir() -> Option<PathBuf> {
    #[cfg(test)]
    return dir_override::get();
    #[cfg(not(test))]
    match std::env::var_os("Q_CLAUDE_SESSIONS_DIR") {
        Some(raw) if !raw.is_empty() => Some(PathBuf::from(raw)),
        _ => Some(dirs::home_dir()?.join(".claude").join("sessions")),
    }
}

/// The in-crate registry directory: none unless a test says otherwise.
#[cfg(test)]
pub(crate) mod dir_override {
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

    fn exclusive() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    pub(super) fn get() -> Option<PathBuf> {
        DIR.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `DIR` is process-global, so the guard also serializes the tests using it.
    pub(crate) struct Guard(#[allow(dead_code)] MutexGuard<'static, ()>);

    impl Drop for Guard {
        fn drop(&mut self) {
            *DIR.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    pub(crate) fn at(dir: PathBuf) -> Guard {
        let guard = Guard(exclusive().lock().unwrap_or_else(|e| e.into_inner()));
        *DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
        guard
    }
}

/// What Claude reports a session is doing. Anything `q` does not know is
/// `Other`, and treated as "not provably idle".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Busy,
    Waiting,
    Other(String),
}

/// Always a plain string, `Other` included — the derive would have written
/// `{"other": "..."}` for that one arm alone.
impl Serialize for Status {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
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
///
/// A `pid` field that contradicts the file name means the file was copied or
/// renamed, so it says nothing about the process `q` asked about.
pub fn read_in(dir: &Path, pid: i64) -> Option<Entry> {
    let text = std::fs::read_to_string(dir.join(format!("{pid}.json"))).ok()?;
    let entry = parse(&text)?;
    entry.pid.is_none_or(|p| p == pid).then_some(entry)
}

/// The pids `dir` holds entries for, from the `<pid>.json` file names.
fn pids_in(dir: &Path) -> Vec<i64> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".json")?
                .parse::<i64>()
                .ok()
        })
        .collect()
}

/// How long `ps` may take before its answer is worth less than the wait. The
/// registry is advisory, and `q send` is interactive.
const PS_TIMEOUT: Duration = Duration::from_secs(2);

/// `pid -> ppid` for every process, from one `ps`. Empty when `ps` is not
/// there, hangs, or says nothing usable — the callers all degrade to `Unknown`.
fn process_parents() -> HashMap<i64, i64> {
    parse_parents(&ps_output().unwrap_or_default())
}

/// `ps -Ao pid=,ppid=` under a wall-clock cap. A `ps` on a wedged filesystem
/// can block indefinitely, so the pipe is drained on a thread and the child is
/// killed once the cap passes; `None` on any failure or timeout.
fn ps_output() -> Option<String> {
    let mut child = Command::new("ps")
        .args(["-Ao", "pid=,ppid="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = tx.send(stdout.read_to_string(&mut buf).ok().map(|_| buf));
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if started.elapsed() < PS_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    rx.recv_timeout(PS_TIMEOUT.saturating_sub(started.elapsed()))
        .ok()
        .flatten()
}

fn parse_parents(text: &str) -> HashMap<i64, i64> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .collect()
}

/// Does `pid` sit under `ancestor`? The walk is capped because a `ps` snapshot
/// taken while processes come and go can contain a cycle.
fn descends_from(parents: &HashMap<i64, i64>, pid: i64, ancestor: i64) -> bool {
    let mut at = pid;
    for _ in 0..64 {
        if at == ancestor {
            return true;
        }
        match parents.get(&at) {
            Some(&next) if next > 0 && next != at => at = next,
            _ => return false,
        }
    }
    false
}

/// The registry pid whose process descends from `pane_pid`. `None` unless
/// exactly one does: with two candidates a guess is worse than no opinion.
fn pid_under(dir: &Path, pane_pid: i64) -> Option<i64> {
    let candidates = pids_in(dir);
    if candidates.is_empty() {
        return None;
    }
    let parents = process_parents();
    let mut found = candidates
        .into_iter()
        .filter(|pid| descends_from(&parents, *pid, pane_pid));
    let first = found.next()?;
    found.next().is_none().then_some(first)
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

    /// Claude's own word that the session is between turns — enough to send to
    /// a row no hook ever moved off `starting`.
    pub fn agrees_idle(&self) -> bool {
        matches!(self, Verdict::Idle { .. })
    }

    /// The registry status as a listing shows it, or `None` when it knows
    /// nothing worth a column.
    pub fn status(&self) -> Option<String> {
        match self {
            Verdict::Idle { .. } => Some("idle".to_string()),
            Verdict::Busy {
                status,
                waiting_for: Some(what),
                ..
            } => Some(format!("{status}: {what}")),
            Verdict::Busy { status, .. } => Some(status.clone()),
            Verdict::Unknown { .. } => None,
        }
    }

    /// What the user can do about a verdict that carries no information. The
    /// registry is the only second opinion `q send` has, so a missing pid is
    /// worth naming: it means `SessionStart` never ran.
    pub fn hint(&self) -> Option<&'static str> {
        match self {
            Verdict::Unknown {
                reason: NO_PID_REASON,
            } => Some("`q hook install` may not have run — see `q doctor`"),
            _ => None,
        }
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

const NO_PID_REASON: &str = "no claude pid on record";

/// How old an entry may be and still be believed. Claude rewrites the file on
/// every status change, so an entry older than this belongs to a session that
/// stopped reporting — no information, rather than a refusal that never lifts.
pub const STALE_MS: i64 = 30 * 60 * 1000;

/// What `q` knows about the session it is asking about, so an entry can be
/// checked for identity and freshness before its status is believed. Every
/// field is spelled out at the call site on purpose: an identity check that
/// silently gets `None` is no check at all.
#[derive(Debug, Clone, Copy)]
pub struct Ask<'a> {
    /// `session.claude_pid`, when a `SessionStart` hook recorded one.
    pub pid: Option<i64>,
    /// The pane's own process id: Claude runs under it, which is how the pid
    /// is found when no hook ever wrote one.
    pub pane_pid: Option<i64>,
    /// `<slug>/<label>` — the name `q` launched this session with (`claude -n
    /// <slug>/<label>`, SPEC §6). Claude writes it back to the registry
    /// verbatim, slash included (verified against Claude Code 2.1.246).
    pub name: Option<&'a str>,
    /// `session.claude_session_id`, when a `SessionStart` hook recorded one.
    pub session_id: Option<&'a str>,
    /// Now, in milliseconds — the clock `statusUpdatedAt` uses.
    pub now_ms: i64,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The verdict for one session, looked up in `dir`.
pub fn verdict_in(dir: Option<&Path>, ask: Ask) -> Verdict {
    let Some(dir) = dir else {
        return Verdict::Unknown {
            reason: "no sessions directory",
        };
    };
    // Until `SessionStart` fires there is no pid on the row; the pane's process
    // tree is the fallback, so a fleet without hooks still gets a verdict.
    let pid = ask.pid.filter(|p| *p > 0).or_else(|| {
        ask.pane_pid
            .filter(|p| *p > 0)
            .and_then(|p| pid_under(dir, p))
    });
    let Some(pid) = pid else {
        return Verdict::Unknown {
            reason: NO_PID_REASON,
        };
    };
    let Some(entry) = read_in(dir, pid) else {
        return Verdict::Unknown {
            reason: "no entry for this session",
        };
    };
    verdict_of(&entry, ask)
}

/// The same decision as `verdict_in`, on an already-parsed entry.
pub fn verdict_of(entry: &Entry, ask: Ask) -> Verdict {
    if let Some(reason) = mismatch(entry, ask) {
        return Verdict::Unknown { reason };
    }
    if entry
        .status_updated_at
        .is_some_and(|t| ask.now_ms.saturating_sub(t) > STALE_MS)
    {
        return Verdict::Unknown {
            reason: "entry is stale",
        };
    }
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

/// Why this entry is about some other session, or `None` when nothing
/// contradicts.
///
/// Pids are recycled and a registry file can outlive the process it described,
/// so a file found by pid still has to prove whose it is. `sessionId` is the
/// exact answer and is checked first; a Claude session can be renamed while it
/// runs, so a name never overrules a `sessionId` that matches. When only one
/// side has a `sessionId` the launch name is all there is to go on.
fn mismatch(entry: &Entry, ask: Ask) -> Option<&'static str> {
    match (entry.session_id.as_deref(), ask.session_id) {
        (Some(theirs), Some(ours)) => {
            (theirs != ours).then_some("entry is a different claude session")
        }
        _ => match (entry.name.as_deref(), ask.name) {
            (Some(theirs), Some(ours)) => (theirs != ours).then_some("entry names another session"),
            _ => None,
        },
    }
}

/// `verdict_in` against the configured directory.
pub fn verdict(ask: Ask) -> Verdict {
    verdict_in(dir().as_deref(), ask)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing known but the pid, on a clock that leaves every fixture fresh.
    fn ask(pid: i64) -> Ask<'static> {
        Ask {
            pid: Some(pid),
            pane_pid: None,
            name: None,
            session_id: None,
            now_ms: 0,
        }
    }

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
        assert!(verdict_of(&entry, ask(1)).refuses());
        assert_eq!(
            verdict_of(&entry, ask(1)).describe(),
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
        let verdict = verdict_of(&entry, ask(1));
        assert!(verdict.refuses(), "{verdict:?}");
        assert_eq!(verdict.describe(), "registry: hibernating");
    }

    #[test]
    fn an_older_entry_without_status_updated_at_falls_back_to_updated_at() {
        let entry = parse(r#"{"status":"idle","updatedAt":17}"#).unwrap();
        assert_eq!(entry.status_updated_at, Some(17));
        assert_eq!(verdict_of(&entry, ask(1)), Verdict::Idle { name: None });
    }

    #[test]
    fn an_entry_without_a_status_is_no_information() {
        let entry = parse(r#"{"pid":1,"name":"x"}"#).unwrap();
        assert!(matches!(
            verdict_of(&entry, ask(1)),
            Verdict::Unknown { .. }
        ));
        assert!(!verdict_of(&entry, ask(1)).refuses());
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
            (None, NO_PID_REASON),
            (Some(0), NO_PID_REASON),
        ] {
            assert_eq!(
                verdict_in(Some(dir.path()), Ask { pid, ..ask(0) }),
                Verdict::Unknown { reason }
            );
        }
        // A pid q never learned is worth a hint: it means no SessionStart hook.
        assert!(
            verdict_in(
                Some(dir.path()),
                Ask {
                    pid: None,
                    ..ask(0)
                }
            )
            .hint()
            .is_some()
        );
        assert!(verdict_in(Some(dir.path()), ask(4242)).hint().is_none());
        assert_eq!(
            verdict_in(None, ask(1)),
            Verdict::Unknown {
                reason: "no sessions directory"
            }
        );
    }

    #[test]
    fn a_file_on_disk_is_read_by_pid() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("58391.json"), REAL).unwrap();
        assert_eq!(
            read_in(dir.path(), 58391).unwrap().name.as_deref(),
            Some("work-28")
        );
        let verdict = verdict_in(Some(dir.path()), ask(58391));
        assert!(verdict.refuses(), "{verdict:?}");
        assert_eq!(verdict.describe(), "registry: busy");

        std::fs::write(
            dir.path().join("78.json"),
            r#"{"status":"idle","name":"alpha/master"}"#,
        )
        .unwrap();
        assert_eq!(
            verdict_in(Some(dir.path()), ask(78)),
            Verdict::Idle {
                name: Some("alpha/master".to_string())
            }
        );
    }

    /// A `<pid>.json` whose `pid` field says otherwise was copied or renamed:
    /// it describes some other process.
    #[test]
    fn an_entry_whose_pid_contradicts_its_file_name_is_ignored() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("77.json"), REAL).unwrap();
        assert!(read_in(dir.path(), 77).is_none());
        assert_eq!(
            verdict_in(Some(dir.path()), ask(77)),
            Verdict::Unknown {
                reason: "no entry for this session"
            }
        );
        // No `pid` field at all is the older shape, and still trusted.
        std::fs::write(dir.path().join("79.json"), r#"{"status":"idle"}"#).unwrap();
        assert!(read_in(dir.path(), 79).is_some());
    }

    #[test]
    fn an_entry_that_names_another_session_says_nothing_about_this_one() {
        let entry = parse(r#"{"status":"busy","name":"beta/w1"}"#).unwrap();
        let mine = Ask {
            name: Some("alpha/master"),
            ..ask(1)
        };
        assert_eq!(
            verdict_of(&entry, mine),
            Verdict::Unknown {
                reason: "entry names another session"
            }
        );
        // The same name agrees, and an entry with no name is still believed.
        let theirs = Ask {
            name: Some("beta/w1"),
            ..ask(1)
        };
        assert!(verdict_of(&entry, theirs).refuses());
        let nameless = parse(r#"{"status":"busy"}"#).unwrap();
        assert!(verdict_of(&nameless, mine).refuses());
    }

    #[test]
    fn an_entry_older_than_the_staleness_window_stops_being_believed() {
        let entry = parse(r#"{"status":"waiting","statusUpdatedAt":1000}"#).unwrap();
        let fresh = Ask {
            now_ms: 1000 + STALE_MS,
            ..ask(1)
        };
        assert!(verdict_of(&entry, fresh).refuses());
        let stale = Ask {
            now_ms: 1000 + STALE_MS + 1,
            ..ask(1)
        };
        assert_eq!(
            verdict_of(&entry, stale),
            Verdict::Unknown {
                reason: "entry is stale"
            }
        );
        // No timestamp is no evidence of age, so the status still counts.
        let undated = parse(r#"{"status":"waiting"}"#).unwrap();
        assert!(verdict_of(&undated, stale).refuses());
    }

    #[test]
    fn a_status_serializes_as_a_plain_string_in_every_arm() {
        let json = serde_json::to_string(&parse(r#"{"status":"hibernating"}"#).unwrap()).unwrap();
        assert!(json.contains(r#""status":"hibernating""#), "{json}");
    }

    /// `sessionId` is the exact identity, so it settles the question the name
    /// can only guess at — including for a session the user renamed under q.
    #[test]
    fn a_session_id_decides_identity_and_a_rename_cannot_overrule_it() {
        let entry =
            parse(r#"{"status":"idle","sessionId":"s-1","name":"renamed by hand"}"#).unwrap();
        let mine = Ask {
            name: Some("alpha/master"),
            session_id: Some("s-1"),
            ..ask(1)
        };
        assert!(verdict_of(&entry, mine).agrees_idle());

        let other = Ask {
            session_id: Some("s-2"),
            ..mine
        };
        assert_eq!(
            verdict_of(&entry, other),
            Verdict::Unknown {
                reason: "entry is a different claude session"
            }
        );

        // Only one side knowing a session id falls back to the launch name.
        let no_id_on_the_row = Ask {
            session_id: None,
            ..mine
        };
        assert_eq!(
            verdict_of(&entry, no_id_on_the_row),
            Verdict::Unknown {
                reason: "entry names another session"
            }
        );
        let no_id_in_the_entry = parse(r#"{"status":"idle","name":"alpha/master"}"#).unwrap();
        assert!(verdict_of(&no_id_in_the_entry, mine).agrees_idle());
    }

    #[test]
    fn the_process_table_is_walked_upwards_to_the_pane() {
        let parents = parse_parents(
            "  100 1
 200 100
 300 200
header junk
",
        );
        assert_eq!(parents.len(), 3);
        assert!(descends_from(&parents, 300, 100));
        assert!(descends_from(&parents, 100, 100));
        assert!(!descends_from(&parents, 100, 300));
        assert!(!descends_from(&parents, 999, 100));
        // A snapshot with a cycle must terminate rather than spin.
        let cyclic = parse_parents(
            "1 2
2 1
",
        );
        assert!(!descends_from(&cyclic, 1, 99));
    }

    #[test]
    fn a_pid_without_one_on_the_row_is_found_under_the_pane() {
        let dir = tempfile::TempDir::new().unwrap();
        // This process is its own descendant, so it stands in for Claude.
        let me = std::process::id() as i64;
        std::fs::write(
            dir.path().join(format!("{me}.json")),
            format!(r#"{{"pid":{me},"status":"idle"}}"#),
        )
        .unwrap();
        assert_eq!(pids_in(dir.path()), vec![me]);
        // A row that already carries a pid never pays for the process walk.
        assert_eq!(
            verdict_in(
                Some(dir.path()),
                Ask {
                    pid: None,
                    pane_pid: Some(me),
                    ..ask(0)
                }
            ),
            Verdict::Idle { name: None }
        );
        // An entry for a process that is not under the pane is filtered out.
        std::fs::write(dir.path().join("1.json"), r#"{"pid":1,"status":"busy"}"#).unwrap();
        assert_eq!(
            verdict_in(
                Some(dir.path()),
                Ask {
                    pid: None,
                    pane_pid: Some(me),
                    ..ask(0)
                }
            ),
            Verdict::Idle { name: None }
        );
        // Two candidates under the same pane: a guess is worse than no opinion.
        assert!(matches!(
            verdict_in(
                Some(dir.path()),
                Ask {
                    pid: None,
                    pane_pid: Some(1),
                    ..ask(0)
                }
            ),
            Verdict::Unknown { .. }
        ));
    }
}
