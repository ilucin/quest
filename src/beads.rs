//! Beads (`bd`) integration: the epic a Quest gets on `q new`, the progress
//! counts `q list`/`q show` display, and closing the epic on `q close`
//! (SPEC §13).
//!
//! Every `bd` call lives here and goes through [`proc`], so it is capped and
//! can never fail a `q` command: a missing or broken `bd` degrades to a warning
//! (writes) or to the last cached reading (progress).
//!
//! **Testing.** Under `$Q_FIXTURE` (which already switches `tmux` and the
//! brief's external tools to their stubs) no real `bd` is ever spawned; canned
//! output comes from files named by env vars, and a missing var or file means
//! "`bd` is unavailable" — the same convention `brief.rs` uses:
//!
//! | var | stands for |
//! |---|---|
//! | `Q_FIXTURE_BD_CREATE` | stdout of `bd create … --json` |
//! | `Q_FIXTURE_BD` | stdout of `bd list … --json` (shared with the brief) |
//! | `Q_FIXTURE_BD_CLOSE` | `bd close` succeeds (content unused) |
//! | `Q_FIXTURE_BD_LOG` | appended one line per call: `<subcommand> <arg>…` |

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::db::Db;
use crate::model::{Quest, now};
use crate::proc;

/// Reads must never hold up a listing (SPEC §12: 3 s, then the cache).
const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// A write commits to a dolt database, which is slower than a query.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);
/// How long a cached progress reading counts as fresh.
pub const CACHE_TTL: i64 = 30;
/// `bd list` defaults to 50 rows; counts have to see all of them.
const NO_LIMIT: &[&str] = &["--all", "-n", "0", "--no-pager", "--json"];

// ------------------------------------------------------------------- progress

/// The counts SPEC §13 asks for. `total` is every issue carrying the Quest's
/// label except the epic itself; statuses `bd` has and this does not name
/// (`deferred`, …) land in `total` only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub open: usize,
    pub in_progress: usize,
    pub closed: usize,
    pub blocked: usize,
    pub total: usize,
}

impl Progress {
    /// `3/7` — the listing cell (SPEC §13).
    pub fn cell(&self) -> String {
        format!("{}/{}", self.closed, self.total)
    }

    /// `3/7 closed · 2 open · 1 in progress · 1 blocked`
    pub fn summary(&self) -> String {
        let mut out = format!("{} closed", self.cell());
        for (n, label) in [
            (self.open, "open"),
            (self.in_progress, "in progress"),
            (self.blocked, "blocked"),
        ] {
            if n > 0 {
                out.push_str(&format!(" · {n} {label}"));
            }
        }
        out
    }
}

/// Counts `bd list --json` output, skipping `epic` (an epic carries its own
/// `quest:<id>` label, so it would otherwise count against itself).
pub fn count(raw: &str, epic: Option<&str>) -> Option<Progress> {
    Some(tally(&issues(raw)?, epic))
}

/// Buckets one multi-label `bd list` payload by Quest id, so a whole listing
/// costs a single `bd` call.
fn count_by_quest(
    raw: &str,
    epics: &HashMap<String, Option<String>>,
) -> Option<Vec<(String, Progress)>> {
    let issues = issues(raw)?;
    let mut buckets: HashMap<&str, Vec<&Value>> = HashMap::new();
    for issue in &issues {
        for quest_id in quest_labels(issue) {
            buckets.entry(quest_id).or_default().push(issue);
        }
    }
    Some(
        epics
            .iter()
            .map(|(quest_id, epic)| {
                let mine: Vec<Value> = buckets
                    .get(quest_id.as_str())
                    .map(|v| v.iter().map(|i| (*i).clone()).collect())
                    .unwrap_or_default();
                (quest_id.clone(), tally(&mine, epic.as_deref()))
            })
            .collect(),
    )
}

fn tally(issues: &[Value], epic: Option<&str>) -> Progress {
    let mut p = Progress::default();
    for issue in issues {
        if epic.is_some_and(|e| field(issue, "id") == e) {
            continue;
        }
        p.total += 1;
        match field(issue, "status") {
            "open" => p.open += 1,
            "in_progress" => p.in_progress += 1,
            "closed" => p.closed += 1,
            "blocked" => p.blocked += 1,
            _ => {}
        }
    }
    p
}

/// `bd list --json` is an array; an object wrapping `issues` is tolerated.
fn issues(raw: &str) -> Option<Vec<Value>> {
    match serde_json::from_str(raw).ok()? {
        Value::Array(a) => Some(a),
        Value::Object(o) => Some(o.get("issues")?.as_array()?.clone()),
        _ => None,
    }
}

fn quest_labels(issue: &Value) -> impl Iterator<Item = &str> {
    issue
        .get("labels")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .filter_map(|l| l.as_str()?.strip_prefix("quest:"))
}

fn field<'a>(issue: &'a Value, key: &str) -> &'a str {
    issue.get(key).and_then(Value::as_str).unwrap_or_default()
}

// --------------------------------------------------------------- the bd calls

/// Every `bd` invocation `q` makes. Stubbed under `$Q_FIXTURE`.
pub trait Bd {
    /// `bd create "<title>" --type epic -l <labels> --json` → the new id.
    fn create_epic(&self, title: &str, labels: &str) -> Result<String, String>;
    /// `bd list -l quest:<id>` for one Quest; `None` when `bd` failed.
    fn list_quest(&self, quest_id: &str) -> Option<String>;
    /// `bd list --label-any quest:<a>,quest:<b>,…` — a whole listing in one
    /// call. `--label-pattern` would be the obvious tool, but it is ignored by
    /// the `bd` in the field, which returns the entire tracker instead.
    fn list_quests(&self, quest_ids: &[&str]) -> Option<String>;
    /// `bd close <id>`.
    fn close(&self, id: &str) -> Result<(), String>;
}

pub fn client() -> Box<dyn Bd> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureBd),
        _ => Box::new(RealBd),
    }
}

struct RealBd;

impl Bd for RealBd {
    fn create_epic(&self, title: &str, labels: &str) -> Result<String, String> {
        let args = &["create", title, "--type", "epic", "-l", labels, "--json"];
        let out = bd(args, WRITE_TIMEOUT).ok_or_else(unavailable)?;
        if !out.success() {
            return Err(out.message());
        }
        let stdout = out.text();
        created_id(&stdout).ok_or_else(|| {
            format!(
                "`bd create` reported no issue id: {}",
                stdout.lines().next().unwrap_or("(no output)")
            )
        })
    }

    fn list_quest(&self, quest_id: &str) -> Option<String> {
        let label = format!("quest:{quest_id}");
        let mut args: Vec<&str> = vec!["list", "-l", &label];
        args.extend_from_slice(NO_LIMIT);
        proc::run_capped("bd", &args, READ_TIMEOUT)
    }

    fn list_quests(&self, quest_ids: &[&str]) -> Option<String> {
        let labels = quest_ids
            .iter()
            .map(|id| format!("quest:{id}"))
            .collect::<Vec<_>>()
            .join(",");
        let mut args: Vec<&str> = vec!["list", "--label-any", &labels];
        args.extend_from_slice(NO_LIMIT);
        proc::run_capped("bd", &args, READ_TIMEOUT)
    }

    fn close(&self, id: &str) -> Result<(), String> {
        let out = bd(&["close", id], WRITE_TIMEOUT).ok_or_else(unavailable)?;
        if out.success() {
            Ok(())
        } else {
            Err(out.message())
        }
    }
}

/// One capped `bd` invocation. `None` when `bd` never ran, or ran past its
/// budget and was killed — either way there is nothing to report but the
/// warning.
fn bd(args: &[&str], timeout: Duration) -> Option<proc::Outcome> {
    let mut cmd = std::process::Command::new("bd");
    cmd.args(args);
    proc::run(&mut cmd, b"", timeout)
        .ok()
        .filter(|out| !out.timed_out())
}

fn unavailable() -> String {
    "`bd` is not available (missing from PATH, or it timed out)".to_string()
}

/// `bd create --json` prints the created issue; fall back to the first `bd-…`
/// token so a plainer output still yields an id.
fn created_id(stdout: &str) -> Option<String> {
    let from_json = serde_json::from_str::<Value>(stdout)
        .ok()
        .as_ref()
        .and_then(|v| v.get("id").or_else(|| v.get("issue")?.get("id")).cloned())
        .and_then(|v| v.as_str().map(str::to_string))
        .filter(|id| !id.is_empty());
    from_json.or_else(|| {
        stdout
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
            .find(|w| w.starts_with("bd-") && w.len() > 3)
            .map(str::to_string)
    })
}

struct FixtureBd;

impl Bd for FixtureBd {
    fn create_epic(&self, title: &str, labels: &str) -> Result<String, String> {
        log(&["create", title, labels]);
        let stdout = fixture_file("Q_FIXTURE_BD_CREATE").ok_or_else(unavailable)?;
        created_id(&stdout).ok_or_else(|| "`bd create` reported no issue id".to_string())
    }

    fn list_quest(&self, quest_id: &str) -> Option<String> {
        log(&["list", quest_id]);
        fixture_file("Q_FIXTURE_BD")
    }

    fn list_quests(&self, quest_ids: &[&str]) -> Option<String> {
        log(&["list", &quest_ids.join(",")]);
        fixture_file("Q_FIXTURE_BD")
    }

    fn close(&self, id: &str) -> Result<(), String> {
        log(&["close", id]);
        fixture_file("Q_FIXTURE_BD_CLOSE")
            .map(|_| ())
            .ok_or_else(unavailable)
    }
}

fn fixture_file(var: &str) -> Option<String> {
    std::fs::read_to_string(std::env::var_os(var)?).ok()
}

/// Appends one line per fixture call, so a test can assert `bd` was invoked.
fn log(parts: &[&str]) {
    let Some(path) = std::env::var_os("Q_FIXTURE_BD_LOG") else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", parts.join(" "));
    }
}

/// `q set <quest> beads_epic <id>`: an issue id `bd` could have minted, or a
/// blank value to unlink the epic. Trimmed, never guessed at.
pub fn validate_epic_id(value: &str) -> anyhow::Result<String> {
    let id = value.trim();
    let shaped = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '_');
    if id.is_empty() || shaped {
        return Ok(id.to_string());
    }
    Err(crate::error::QError::Invalid(format!(
        "invalid beads epic id `{value}`: expected something like `bd-7fx`, or an empty value to unlink it"
    ))
    .into())
}

// ------------------------------------------------------------- the repo label

/// `repo:<name>` for a new Quest: `--repo`, else the basename of the cwd's git
/// root, else `[beads] default_repo_label` (SPEC §13).
pub fn repo_label(config: &Config, repo: Option<&str>, cwd: &Path) -> String {
    repo.map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| git_root_name(cwd))
        .unwrap_or_else(|| config.beads.default_repo_label.clone())
}

fn git_root_name(cwd: &Path) -> Option<String> {
    let root = proc::run_capped(
        "git",
        &["-C", &cwd.to_string_lossy(), "rev-parse", "--show-toplevel"],
        READ_TIMEOUT,
    )?;
    Path::new(root.trim())
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
}

// -------------------------------------------------------------------- caching

/// A progress reading and when it was taken. Cached as one small JSON file per
/// Quest next to the database (`<db dir>/cache/beads-<quest id>.json`) — no
/// schema migration, and two `q` processes cannot clobber each other's Quest.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    fetched_at: i64,
    progress: Progress,
}

fn cache_path(quest_id: &str) -> Option<PathBuf> {
    let db = Db::path().ok()?;
    Some(
        db.parent()?
            .join("cache")
            .join(format!("beads-{quest_id}.json")),
    )
}

fn read_cache(quest_id: &str) -> Option<Cached> {
    let raw = std::fs::read_to_string(cache_path(quest_id)?).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Written through a temp file so a reader never sees half a payload.
fn write_cache(quest_id: &str, progress: &Progress) {
    let Some(path) = cache_path(quest_id) else {
        return;
    };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let cached = Cached {
        fetched_at: now(),
        progress: *progress,
    };
    let Ok(body) = serde_json::to_string(&cached) else {
        return;
    };
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn fresh(cached: &Cached) -> bool {
    (now() - cached.fetched_at).abs() < CACHE_TTL
}

// ------------------------------------------------------------- what q calls

/// Progress for one Quest: a fresh cache hit, else `bd`, else the stale cache.
/// `None` when the Quest has no epic, or nothing has ever been read.
pub fn progress(quest: &Quest) -> Option<Progress> {
    progress_with(client().as_ref(), quest)
}

pub fn progress_with(bd: &dyn Bd, quest: &Quest) -> Option<Progress> {
    quest.beads_epic.as_deref()?;
    let cached = read_cache(&quest.id);
    if let Some(hit) = cached.as_ref().filter(|c| fresh(c)) {
        return Some(hit.progress);
    }
    let fetched = bd
        .list_quest(&quest.id)
        .and_then(|raw| count(&raw, quest.beads_epic.as_deref()));
    match fetched {
        Some(progress) => {
            write_cache(&quest.id, &progress);
            Some(progress)
        }
        None => cached.map(|c| c.progress),
    }
}

/// Progress for a whole listing in one `bd` call. Quests without an epic are
/// absent from the result; a Quest `bd` knows nothing about counts as empty.
pub fn progress_all(quests: &[&Quest]) -> HashMap<String, Progress> {
    progress_all_with(client().as_ref(), quests)
}

pub fn progress_all_with(bd: &dyn Bd, quests: &[&Quest]) -> HashMap<String, Progress> {
    let epics: HashMap<String, Option<String>> = quests
        .iter()
        .filter(|q| q.beads_epic.is_some())
        .map(|q| (q.id.clone(), q.beads_epic.clone()))
        .collect();
    if epics.is_empty() {
        return HashMap::new();
    }
    let cached: HashMap<String, Cached> = epics
        .keys()
        .filter_map(|id| read_cache(id).map(|c| (id.clone(), c)))
        .collect();
    if epics.keys().all(|id| cached.get(id).is_some_and(fresh)) {
        return cached.into_iter().map(|(k, c)| (k, c.progress)).collect();
    }
    let ids: Vec<&str> = epics.keys().map(String::as_str).collect();
    match bd
        .list_quests(&ids)
        .and_then(|raw| count_by_quest(&raw, &epics))
    {
        Some(counted) => {
            for (id, progress) in &counted {
                write_cache(id, progress);
            }
            counted.into_iter().collect()
        }
        // Stale beats blank: a listing shows the last reading it had.
        None => cached.into_iter().map(|(k, c)| (k, c.progress)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(id: &str, status: &str, quests: &[&str]) -> Value {
        serde_json::json!({
            "id": id,
            "status": status,
            "labels": quests.iter().map(|q| format!("quest:{q}")).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn counts_every_named_status_and_totals_the_rest() {
        let raw = serde_json::json!([
            issue("bd-1", "open", &[]),
            issue("bd-2", "in_progress", &[]),
            issue("bd-3", "closed", &[]),
            issue("bd-4", "blocked", &[]),
            issue("bd-5", "deferred", &[]),
        ])
        .to_string();
        let p = count(&raw, None).unwrap();
        assert_eq!(
            p,
            Progress {
                open: 1,
                in_progress: 1,
                closed: 1,
                blocked: 1,
                total: 5,
            }
        );
        assert_eq!(p.cell(), "1/5");
        assert_eq!(
            p.summary(),
            "1/5 closed · 1 open · 1 in progress · 1 blocked"
        );
    }

    #[test]
    fn the_epic_does_not_count_against_itself() {
        let raw = serde_json::json!([issue("bd-e", "open", &[]), issue("bd-1", "closed", &[])])
            .to_string();
        let p = count(&raw, Some("bd-e")).unwrap();
        assert_eq!(p.total, 1);
        assert_eq!(p.closed, 1);
        assert_eq!(p.open, 0);
    }

    #[test]
    fn an_object_payload_and_garbage_are_both_handled() {
        let p = count(r#"{"issues":[{"id":"bd-1","status":"open"}]}"#, None).unwrap();
        assert_eq!(p.total, 1);
        assert!(count("not json", None).is_none());
        assert_eq!(count("[]", None).unwrap(), Progress::default());
    }

    #[test]
    fn a_batch_payload_is_bucketed_by_quest_label() {
        let raw = serde_json::json!([
            issue("bd-a", "closed", &["q-1"]),
            issue("bd-b", "open", &["q-1"]),
            issue("bd-c", "open", &["q-2"]),
            issue("bd-epic1", "open", &["q-1"]),
            issue("bd-x", "open", &["q-unknown"]),
        ])
        .to_string();
        let epics = HashMap::from([
            ("q-1".to_string(), Some("bd-epic1".to_string())),
            ("q-2".to_string(), None),
            ("q-3".to_string(), Some("bd-epic3".to_string())),
        ]);
        let counted: HashMap<String, Progress> =
            count_by_quest(&raw, &epics).unwrap().into_iter().collect();
        assert_eq!(counted["q-1"].total, 2);
        assert_eq!(counted["q-1"].closed, 1);
        assert_eq!(counted["q-2"].total, 1);
        // A Quest with an epic but no issues is a real zero, not a gap.
        assert_eq!(counted["q-3"], Progress::default());
    }

    #[test]
    fn the_created_id_comes_from_json_or_from_the_text() {
        assert_eq!(
            created_id(r#"{"id":"bd-7fx","title":"x"}"#).unwrap(),
            "bd-7fx"
        );
        assert_eq!(created_id(r#"{"issue":{"id":"bd-9"}}"#).unwrap(), "bd-9");
        assert_eq!(created_id("created issue bd-4a2 (epic)").unwrap(), "bd-4a2");
        assert_eq!(created_id(r#"{"id":""}"#), None);
        assert_eq!(created_id("nothing here"), None);
    }

    #[test]
    fn the_repo_label_prefers_the_flag_then_git_then_the_config() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            repo_label(&config, Some("explicit"), dir.path()),
            "explicit"
        );
        // A blank flag is not a label.
        assert_eq!(
            repo_label(&config, Some("  "), dir.path()),
            config.beads.default_repo_label
        );
        // Not a git checkout, so the config default is what is left.
        assert_eq!(
            repo_label(&config, None, dir.path()),
            config.beads.default_repo_label
        );
    }

    #[test]
    fn the_repo_label_uses_the_git_root_basename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("some-repo");
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let ok = std::process::Command::new("git")
            .args(["-C", &root.to_string_lossy(), "init", "-q"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return; // no git on this machine; the other cases still hold
        }
        assert_eq!(
            repo_label(&Config::default(), None, &nested),
            "some-repo".to_string()
        );
    }
}
