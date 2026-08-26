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
//! | `Q_FIXTURE_BD_RELABEL` | `bd update --add-label` succeeds (content unused) |
//! | `Q_FIXTURE_BD_CREATE_TIMEOUT` | `bd create` is killed mid-write (content unused) |
//! | `Q_FIXTURE_BD_LOG` | appended one line per call, the argv verbatim |

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Config;
use crate::db::Db;
use crate::model::{Quest, now};
use crate::proc;

/// Reads must never hold up a listing (SPEC §12: 3 s, then the cache).
const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// A write commits to a dolt database, which is slower than a query — but a
/// `q new` that sits silent for twenty seconds reads as a hang, so the budget
/// is short and [`SLOW_WRITE`] explains the wait.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a write may run before it says so on stderr.
const SLOW_WRITE: Duration = Duration::from_secs(2);
/// How long a cached progress reading counts as fresh.
pub const CACHE_TTL: i64 = 30;
/// How long a *failed* read is remembered before `bd` is tried again.
///
/// A failure writes no cache, so without this the "nothing fresh" test is true
/// again immediately and the next caller re-spawns `bd`. On the TUI's 2 s tick
/// that means a `bd` present but slow costs up to [`READ_TIMEOUT`] on the UI
/// thread every tick — longer than the tick itself — and the keyboard goes
/// dead. Matching [`CACHE_TTL`] keeps the two paths on the same cadence: at
/// most one `bd` call per Quest listing per half-minute, succeed or fail.
pub const FAILURE_TTL: i64 = CACHE_TTL;
/// `bd list` defaults to 50 rows; counts have to see all of them.
const NO_LIMIT: &[&str] = &["--all", "-n", "0", "--no-pager", "--json"];
/// A listing's stdout is a *document*, so [`proc`]'s default 64 KiB capture
/// cap is not enough: at roughly a kilobyte an issue it would truncate a
/// tracker mid-object a few dozen issues in, and the JSON would then parse as
/// nothing — every count silently zero. Big enough for any real tracker,
/// still bounded.
const READ_CAPTURE: usize = 8 * 1024 * 1024;
/// A killed `bd create` may still have committed: a dolt write can land a
/// moment after the kill, so the recovery read is retried once after this.
const RECOVERY_WAIT: Duration = Duration::from_secs(1);

// ------------------------------------------------------------------- progress

/// The counts SPEC §13 asks for. `total` is every issue carrying the Quest's
/// label except the epic itself.
///
/// **The buckets do not partition `total`.** Two reasons, both deliberate:
/// statuses `bd` has and this does not name (`deferred`, …) land in `total`
/// only, so `open + in_progress + closed` can be less than `total`; and
/// `blocked` is an **overlay** rather than a fifth disjoint bucket — `bd` has
/// no `blocked` status (verified against bd 1.2.2: `bd blocked` returns rows
/// stored `open`/`deferred`), so it is derived from the dependencies in the
/// same payload and its issues are *also* counted in `open`/`in_progress`.
/// Anything rendering these numbers has to say so or stay vague; nothing may
/// present them as a breakdown that adds up.
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

    /// The mini bar beside the cell (SPEC §17): `width` cells of `▓` for the
    /// closed share, `░` for the rest. A Quest with no issues yet is all
    /// empty rather than all full — nothing done is not everything done.
    pub fn bar(&self, width: usize) -> String {
        // Round down, but never claim "done" until it is: a single open issue
        // keeps at least one empty cell.
        let filled = match (self.closed * width).checked_div(self.total) {
            None => 0,
            Some(_) if self.closed >= self.total => width,
            Some(exact) => exact.min(width.saturating_sub(1)),
        };
        "▓".repeat(filled) + &"░".repeat(width - filled)
    }

    /// `3/7 closed · 2 open · 1 in progress · 1 blocked`. Empty buckets are
    /// left out, and the ones printed need not add up to the total — see the
    /// type's own docs for why.
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

/// A Quest's work in a `bd list --json` payload: the rows carrying its own
/// `quest:<id>` label, the epic excluded — it carries that label too and would
/// otherwise count against itself.
///
/// The one definition of "this Quest's issues" in `q`. Filtering by the label
/// rather than trusting the payload means a listing wider than it was asked
/// for cannot inflate one Quest's numbers, and everything that *renders* those
/// issues — `q show`, `q list`, `q brief` — selects them here, so no two views
/// can disagree about what the Quest's work is.
fn selected<'a>(all: &'a [Value], quest_id: &str, epic: Option<&str>) -> Vec<&'a Value> {
    all.iter()
        .filter(|i| quest_labels(i).any(|q| q == quest_id))
        .filter(|i| !epic.is_some_and(|e| field(i, "id") == e))
        .collect()
}

/// Counts `bd list --json` output for one Quest.
pub fn count(raw: &str, quest_id: &str, epic: Option<&str>) -> Option<Progress> {
    let all = issues(raw)?;
    let statuses = status_index(&all);
    Some(tally(&selected(&all, quest_id, epic), &statuses))
}

/// One issue of a Quest, for a renderer that wants the rows and not just the
/// tally. Exactly what [`count`] counted, in the same order `bd` returned.
pub struct Row {
    pub id: String,
    pub title: String,
    pub status: String,
    /// The `blocked` overlay for this row — see [`Progress`].
    pub blocked: bool,
}

/// The Quest's issues, epic excluded: the rows behind [`count`]'s numbers.
pub fn rows(raw: &str, quest_id: &str, epic: Option<&str>) -> Option<Vec<Row>> {
    let all = issues(raw)?;
    let statuses = status_index(&all);
    Some(
        selected(&all, quest_id, epic)
            .into_iter()
            .map(|i| Row {
                id: field(i, "id").to_string(),
                title: field(i, "title").to_string(),
                status: field(i, "status").to_string(),
                blocked: is_blocked(i, &statuses),
            })
            .collect(),
    )
}

/// Counts one multi-label `bd list` payload per Quest, so a whole listing
/// costs a single `bd` call.
fn count_by_quest(
    raw: &str,
    epics: &HashMap<String, Option<String>>,
) -> Option<Vec<(String, Progress)>> {
    let all = issues(raw)?;
    let statuses = status_index(&all);
    Some(
        epics
            .iter()
            .map(|(quest_id, epic)| {
                let mine = selected(&all, quest_id, epic.as_deref());
                (quest_id.clone(), tally(&mine, &statuses))
            })
            .collect(),
    )
}

/// Status by issue id over the whole payload, so a blocker outside the Quest's
/// own label can still be resolved when `bd` returned it.
fn status_index(issues: &[Value]) -> HashMap<&str, &str> {
    issues
        .iter()
        .map(|i| (field(i, "id"), field(i, "status")))
        .filter(|(id, _)| !id.is_empty())
        .collect()
}

fn tally(issues: &[&Value], statuses: &HashMap<&str, &str>) -> Progress {
    let mut p = Progress::default();
    for issue in issues {
        p.total += 1;
        let status = field(issue, "status");
        match status {
            "open" => p.open += 1,
            "in_progress" => p.in_progress += 1,
            "closed" => p.closed += 1,
            // A `bd` that ever does store the status is taken at its word.
            "blocked" => p.blocked += 1,
            _ => {}
        }
        if !matches!(status, "closed" | "blocked") && is_blocked(issue, statuses) {
            p.blocked += 1;
        }
    }
    p
}

/// An issue is blocked when something it depends on is known to be unclosed.
///
/// Only `blocks` dependencies count. `bd blocked` also treats `parent-child`
/// as blocking, which makes it useless here: every issue under a live epic has
/// exactly that dependency, so a Quest in progress would report all of its
/// work blocked (22 of 28 rows in the real tracker were parent-child only).
///
/// A dependency on an issue *outside* this payload — a blocker in another
/// repo, or one `bd` did not return — does **not** count. `q` cannot see
/// whether it is closed, and guessing "open" is the worse guess: the row
/// would then read blocked for as long as the dependency exists, with nothing
/// a person could do to clear it. An unknown blocker is reported as not
/// blocking, so `blocked` is a floor rather than a guess.
fn is_blocked(issue: &Value, statuses: &HashMap<&str, &str>) -> bool {
    let id = field(issue, "id");
    deps(issue).any(|dep| {
        let owner = field(dep, "issue_id");
        (owner.is_empty() || owner == id)
            && field(dep, "type") == "blocks"
            && statuses
                .get(field(dep, "depends_on_id"))
                .is_some_and(|status| *status != "closed")
    })
}

fn deps(issue: &Value) -> impl Iterator<Item = &Value> {
    issue
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
}

/// The epic among a Quest's issues — how the id of an epic a killed
/// `bd create` may still have committed is recovered.
fn epic_id(raw: &str, quest_id: &str) -> Option<String> {
    issues(raw)?
        .iter()
        .find(|i| {
            quest_labels(i).any(|q| q == quest_id)
                // `issue_type` is what bd 1.2.2 emits; `type` is tolerated.
                && (field(i, "issue_type") == "epic" || field(i, "type") == "epic")
        })
        .map(|i| field(i, "id").to_string())
        .filter(|id| !id.is_empty())
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
    /// `quest_id` is only for recovery: a killed write may already have
    /// committed the epic, and the label is the one way back to its id.
    fn create_epic(&self, title: &str, labels: &str, quest_id: &str) -> Result<String, String>;
    /// `bd list -l quest:<id>` for one Quest; `None` when `bd` failed.
    fn list_quest(&self, quest_id: &str) -> Option<String>;
    /// `bd list --label-any quest:<a>,quest:<b>,…` — a whole listing in one
    /// call. `--label-pattern` would be the obvious tool, but it is ignored by
    /// the `bd` in the field, which returns the entire tracker instead.
    fn list_quests(&self, quest_ids: &[&str]) -> Option<String>;
    /// `bd close <id> --reason <why>`.
    fn close(&self, id: &str, reason: &str) -> Result<(), String>;
    /// `bd update <epic> --remove-label repo:<old> --add-label repo:<new>` —
    /// one write, so the epic is never left carrying both labels or neither.
    fn relabel_repo(&self, epic: &str, old: Option<&str>, new: &str) -> Result<(), String>;

    /// The Quest's epic as `bd` has it, if any.
    fn find_epic(&self, quest_id: &str) -> Option<String> {
        self.list_quest(quest_id)
            .and_then(|raw| epic_id(&raw, quest_id))
    }
}

pub fn client() -> Box<dyn Bd> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureBd),
        _ => Box::new(RealBd),
    }
}

// The argv of every call, built once and shared by the real client and the
// fixture — so the fixture log a test asserts on *is* the command line.

fn create_argv<'a>(title: &'a str, labels: &'a str) -> Vec<&'a str> {
    vec!["create", title, "--type", "epic", "-l", labels, "--json"]
}

fn list_argv<'a>(flag: &'a str, labels: &'a str) -> Vec<&'a str> {
    let mut args = vec!["list", flag, labels];
    args.extend_from_slice(NO_LIMIT);
    args
}

fn close_argv<'a>(id: &'a str, reason: &'a str) -> Vec<&'a str> {
    vec!["close", id, "--reason", reason]
}

/// Owned, because the labels are built here. `old` is dropped when there is
/// nothing to remove.
fn relabel_argv(epic: &str, old: Option<&str>, new: &str) -> Vec<String> {
    let mut args = vec!["update".to_string(), epic.to_string()];
    if let Some(old) = old.map(str::trim).filter(|o| !o.is_empty()) {
        args.push("--remove-label".to_string());
        args.push(format!("repo:{old}"));
    }
    args.push("--add-label".to_string());
    args.push(format!("repo:{new}"));
    args
}

fn as_argv(args: &[String]) -> Vec<&str> {
    args.iter().map(String::as_str).collect()
}

struct RealBd;

impl Bd for RealBd {
    fn create_epic(&self, title: &str, labels: &str, quest_id: &str) -> Result<String, String> {
        let notice = Some((SLOW_WRITE, "waiting on bd to create the epic…"));
        match bd(&create_argv(title, labels), WRITE_TIMEOUT, notice) {
            Ok(out) if out.success() => {
                let stdout = out.text();
                created_id(&stdout).ok_or_else(|| {
                    format!(
                        "`bd create` reported no issue id: {}",
                        stdout.lines().next().unwrap_or("(no output)")
                    )
                })
            }
            Ok(out) => Err(out.message()),
            Err(BdFail::Spawn) => Err(unavailable()),
            // The write was killed, not refused: it may have committed. Look
            // the epic up rather than orphan it with nothing pointing at it.
            Err(BdFail::Timeout) => self.recover_epic(quest_id).ok_or_else(|| {
                format!(
                    "`bd create` did not finish within {}s",
                    WRITE_TIMEOUT.as_secs()
                )
            }),
        }
    }

    fn list_quest(&self, quest_id: &str) -> Option<String> {
        let label = format!("quest:{quest_id}");
        proc::run_capped_bounded("bd", &list_argv("-l", &label), READ_TIMEOUT, READ_CAPTURE)
    }

    fn list_quests(&self, quest_ids: &[&str]) -> Option<String> {
        let labels = quest_ids
            .iter()
            .map(|id| format!("quest:{id}"))
            .collect::<Vec<_>>()
            .join(",");
        proc::run_capped_bounded(
            "bd",
            &list_argv("--label-any", &labels),
            READ_TIMEOUT,
            READ_CAPTURE,
        )
    }

    fn close(&self, id: &str, reason: &str) -> Result<(), String> {
        let notice = Some((SLOW_WRITE, "waiting on bd to close the epic…"));
        match bd(&close_argv(id, reason), WRITE_TIMEOUT, notice) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(out.message()),
            Err(BdFail::Spawn) => Err(unavailable()),
            Err(BdFail::Timeout) => Err(format!(
                "`bd close` did not finish within {}s",
                WRITE_TIMEOUT.as_secs()
            )),
        }
    }

    fn relabel_repo(&self, epic: &str, old: Option<&str>, new: &str) -> Result<(), String> {
        let notice = Some((SLOW_WRITE, "waiting on bd to relabel the epic…"));
        let args = relabel_argv(epic, old, new);
        match bd(&as_argv(&args), WRITE_TIMEOUT, notice) {
            Ok(out) if out.success() => Ok(()),
            Ok(out) => Err(out.message()),
            Err(BdFail::Spawn) => Err(unavailable()),
            Err(BdFail::Timeout) => Err(format!(
                "`bd update` did not finish within {}s",
                WRITE_TIMEOUT.as_secs()
            )),
        }
    }
}

impl RealBd {
    /// A killed `bd create` may have committed anyway. The dolt write can land
    /// slightly after the kill, so the label is looked up twice — once at once,
    /// once after [`RECOVERY_WAIT`] — before the epic is declared lost.
    fn recover_epic(&self, quest_id: &str) -> Option<String> {
        if let Some(id) = self.find_epic(quest_id) {
            return Some(id);
        }
        std::thread::sleep(RECOVERY_WAIT);
        self.find_epic(quest_id)
    }
}

/// Why a `bd` call came back with nothing. The distinction matters for a
/// write: a [`BdFail::Spawn`] never ran, while a [`BdFail::Timeout`] was killed
/// and may already have committed something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BdFail {
    /// Missing from `PATH`, or the spawn itself failed.
    Spawn,
    /// Outran its budget and was killed with its whole process group.
    Timeout,
}

/// One capped `bd` invocation, optionally announcing itself on stderr once it
/// has been running for the given warm-up.
fn bd(
    args: &[&str],
    timeout: Duration,
    notice: Option<(Duration, &str)>,
) -> Result<proc::Outcome, BdFail> {
    let mut cmd = std::process::Command::new("bd");
    cmd.args(args);
    match proc::run_noticed(&mut cmd, b"", timeout, notice) {
        Err(_) => Err(BdFail::Spawn),
        Ok(out) if out.timed_out() => Err(BdFail::Timeout),
        Ok(out) => Ok(out),
    }
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
    fn create_epic(&self, title: &str, labels: &str, quest_id: &str) -> Result<String, String> {
        log(&create_argv(title, labels));
        if std::env::var_os("Q_FIXTURE_BD_CREATE_TIMEOUT").is_some() {
            return self.find_epic(quest_id).ok_or_else(|| {
                format!(
                    "`bd create` did not finish within {}s",
                    WRITE_TIMEOUT.as_secs()
                )
            });
        }
        let stdout = fixture_file("Q_FIXTURE_BD_CREATE").ok_or_else(unavailable)?;
        created_id(&stdout).ok_or_else(|| "`bd create` reported no issue id".to_string())
    }

    fn list_quest(&self, quest_id: &str) -> Option<String> {
        log(&list_argv("-l", &format!("quest:{quest_id}")));
        fixture_file("Q_FIXTURE_BD")
    }

    fn list_quests(&self, quest_ids: &[&str]) -> Option<String> {
        let labels = quest_ids
            .iter()
            .map(|id| format!("quest:{id}"))
            .collect::<Vec<_>>()
            .join(",");
        log(&list_argv("--label-any", &labels));
        fixture_file("Q_FIXTURE_BD")
    }

    fn close(&self, id: &str, reason: &str) -> Result<(), String> {
        log(&close_argv(id, reason));
        fixture_file("Q_FIXTURE_BD_CLOSE")
            .map(|_| ())
            .ok_or_else(unavailable)
    }

    fn relabel_repo(&self, epic: &str, old: Option<&str>, new: &str) -> Result<(), String> {
        let args = relabel_argv(epic, old, new);
        log(&as_argv(&args));
        fixture_file("Q_FIXTURE_BD_RELABEL")
            .map(|_| ())
            .ok_or_else(unavailable)
    }
}

fn fixture_file(var: &str) -> Option<String> {
    std::fs::read_to_string(std::env::var_os(var)?).ok()
}

/// Appends the argv of each fixture call, so a test asserts on the command
/// line `q` would really have run — not on a paraphrase of it.
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

/// The Quest's epic, if it really has one. An empty or blank column is *not*
/// an id: `bd close` and `bd update` with no id act on "the last touched
/// issue", so handing one of those an empty string would close or relabel a
/// stranger's work.
pub fn epic_of(quest: &Quest) -> Option<&str> {
    quest
        .beads_epic
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
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

/// `q new --repo` / `q set <quest> beads_repo`. A label goes into a
/// comma-separated `-l` list, so a comma in it would silently mint a second
/// label; whitespace would make a label nothing can be typed back at `bd`.
/// A blank value is accepted (it unlinks, and `q new` falls back).
pub fn validate_repo_label(value: &str) -> anyhow::Result<String> {
    let label = value.trim();
    let bad = label
        .chars()
        .find(|c| *c == ',' || c.is_whitespace() || c.is_control());
    if bad.is_none() && label.len() <= 64 {
        return Ok(label.to_string());
    }
    Err(crate::error::QError::Invalid(format!(
        "invalid beads repo label `{value}`: no commas or whitespace, at most 64 characters"
    ))
    .into())
}

// ------------------------------------------------------------- the repo label

/// `repo:<name>` for a new Quest: `--repo` (already validated by the caller),
/// else the name of the cwd's repository, else `[beads] default_repo_label`
/// (SPEC §13).
pub fn repo_label(config: &Config, repo: Option<&str>, cwd: &Path) -> String {
    repo.map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| git_root_name(cwd))
        .unwrap_or_else(|| config.beads.default_repo_label.clone())
}

/// The repository's own name, which in a linked worktree is *not* the working
/// tree's: `--show-toplevel` there is `…/quest/.worktrees/bd-8lz.3.4`, so a
/// Quest started from a worktree would be labelled after the branch. The
/// common git dir (`…/quest/.git`) is the same for the main checkout and every
/// worktree, so its parent is the repository.
fn git_root_name(cwd: &Path) -> Option<String> {
    let cwd = cwd.to_string_lossy().into_owned();
    let common = proc::run_capped(
        "git",
        &[
            "-C",
            &cwd,
            "rev-parse",
            "--path-format=absolute",
            "--git-common-dir",
        ],
        READ_TIMEOUT,
    );
    let from_common = common
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(Path::new)
        // Only a `<root>/.git` names its repository; a detached or bare git
        // dir does not, so those fall through to the working tree's name.
        .filter(|p| p.file_name().is_some_and(|n| n == ".git"))
        .and_then(Path::parent)
        .and_then(base_name);
    from_common
        .or_else(|| {
            proc::run_capped(
                "git",
                &["-C", &cwd, "rev-parse", "--show-toplevel"],
                READ_TIMEOUT,
            )
            .as_deref()
            .map(str::trim)
            .map(Path::new)
            .and_then(base_name)
        })
        .map(|name| sanitize(&name))
}

fn base_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
}

/// A directory name is not a label: a comma in it would mint a second label
/// and whitespace would make it untypeable, so both are folded to `-`.
/// Unlike `--repo` this cannot be an error — nobody asked for this name.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == ',' || c.is_whitespace() || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect()
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
    Some(
        cache_dir(&Db::path().ok()?)
            .join("cache")
            .join(format!("beads-{quest_id}.json")),
    )
}

/// A bare relative `Q_DB` (`q.db`) has an empty parent; its cache belongs in
/// the cwd, not in a `/cache` at the filesystem root. Mirrors the guard in
/// `Db::open_with_timeout`.
fn cache_dir(db: &Path) -> PathBuf {
    match db.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
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
    // A half-written or unrenameable temp file is swept up either way, so a
    // failing write never litters the cache directory.
    let renamed = std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_ok();
    if !renamed {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Drops a Quest's cached reading — `q rm` deletes its history, and a stale
/// file would otherwise outlive it and be handed to the next Quest that
/// happened to reuse the id.
pub fn forget(quest_id: &str) {
    if let Some(path) = cache_path(quest_id) {
        let _ = std::fs::remove_file(path);
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
    let epic = epic_of(quest)?;
    let cached = read_cache(&quest.id);
    if let Some(hit) = cached.as_ref().filter(|c| fresh(c)) {
        return Some(hit.progress);
    }
    let fetched = bd
        .list_quest(&quest.id)
        .and_then(|raw| count(&raw, &quest.id, Some(epic)));
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

/// When the last whole-listing read failed, as a unix timestamp; 0 is never.
/// Process-local on purpose: it exists to protect a long-lived TUI from its own
/// tick, and a one-shot `q list` should not inherit an older process's bad luck.
static LAST_FAILURE: AtomicI64 = AtomicI64::new(0);

fn backing_off(now: i64) -> bool {
    let last = LAST_FAILURE.load(Ordering::Relaxed);
    last != 0 && (now - last).abs() < FAILURE_TTL
}

pub fn progress_all_with(bd: &dyn Bd, quests: &[&Quest]) -> HashMap<String, Progress> {
    let epics: HashMap<String, Option<String>> = quests
        .iter()
        .filter_map(|q| epic_of(q).map(|e| (q.id.clone(), Some(e.to_string()))))
        .collect();
    if epics.is_empty() {
        return HashMap::new();
    }
    let cached: HashMap<String, Cached> = epics
        .keys()
        .filter_map(|id| read_cache(id).map(|c| (id.clone(), c)))
        .collect();
    let stale = || -> HashMap<String, Progress> {
        // Stale beats blank: a listing shows the last reading it had.
        cached
            .iter()
            .map(|(k, c)| (k.clone(), c.progress))
            .collect()
    };
    if epics.keys().all(|id| cached.get(id).is_some_and(fresh)) || backing_off(now()) {
        return stale();
    }
    let ids: Vec<&str> = epics.keys().map(String::as_str).collect();
    match bd
        .list_quests(&ids)
        .and_then(|raw| count_by_quest(&raw, &epics))
    {
        Some(counted) => {
            LAST_FAILURE.store(0, Ordering::Relaxed);
            for (id, progress) in &counted {
                write_cache(id, progress);
            }
            counted.into_iter().collect()
        }
        None => {
            LAST_FAILURE.store(now(), Ordering::Relaxed);
            stale()
        }
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

    /// An issue with dependencies, in the shape `bd list --json` really emits.
    fn dependent(id: &str, status: &str, quest: &str, deps: &[(&str, &str)]) -> Value {
        serde_json::json!({
            "id": id,
            "status": status,
            "labels": [format!("quest:{quest}")],
            "dependencies": deps.iter().map(|(kind, on)| serde_json::json!({
                "issue_id": id,
                "depends_on_id": on,
                "type": kind,
            })).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn counts_every_named_status_and_totals_the_rest() {
        let raw = serde_json::json!([
            issue("bd-1", "open", &["q-1"]),
            issue("bd-2", "in_progress", &["q-1"]),
            issue("bd-3", "closed", &["q-1"]),
            issue("bd-4", "blocked", &["q-1"]),
            issue("bd-5", "deferred", &["q-1"]),
        ])
        .to_string();
        let p = count(&raw, "q-1", None).unwrap();
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
    fn the_progress_bar_fills_with_the_closed_share() {
        let p = |closed, total| Progress {
            closed,
            total,
            ..Progress::default()
        };
        assert_eq!(p(0, 0).bar(7), "░░░░░░░");
        assert_eq!(p(0, 7).bar(7), "░░░░░░░");
        assert_eq!(p(3, 7).bar(7), "▓▓▓░░░░");
        assert_eq!(p(7, 7).bar(7), "▓▓▓▓▓▓▓");
        // One issue short of done never paints a full bar.
        assert_eq!(p(99, 100).bar(7), "▓▓▓▓▓▓░");
        assert_eq!(p(3, 7).bar(0), "");
        for (closed, total) in [(0, 0), (0, 3), (1, 3), (3, 3), (99, 100)] {
            assert_eq!(p(closed, total).bar(7).chars().count(), 7);
        }
    }

    #[test]
    fn the_epic_does_not_count_against_itself() {
        let raw = serde_json::json!([
            issue("bd-e", "open", &["q-1"]),
            issue("bd-1", "closed", &["q-1"])
        ])
        .to_string();
        let p = count(&raw, "q-1", Some("bd-e")).unwrap();
        assert_eq!(p.total, 1);
        assert_eq!(p.closed, 1);
        assert_eq!(p.open, 0);
    }

    #[test]
    fn only_the_quests_own_issues_are_counted() {
        // `bd` was asked for one label; a payload wider than that (a filter
        // regression, a shared cache file) must not inflate the count.
        let raw = serde_json::json!([
            issue("bd-1", "open", &["q-1"]),
            issue("bd-2", "open", &["q-2"]),
            issue("bd-3", "open", &[]),
        ])
        .to_string();
        assert_eq!(count(&raw, "q-1", None).unwrap().total, 1);
        assert_eq!(count(&raw, "q-2", None).unwrap().total, 1);
        assert_eq!(count(&raw, "q-3", None).unwrap(), Progress::default());
    }

    #[test]
    fn blocked_comes_from_blocks_dependencies_not_from_the_status() {
        // What the real tracker looks like: every child of a live epic has a
        // `parent-child` dependency on it, and that is not being blocked.
        let raw = serde_json::json!([
            issue("bd-e", "open", &["q-1"]),
            dependent("bd-1", "open", "q-1", &[("parent-child", "bd-e")]),
            dependent(
                "bd-2",
                "open",
                "q-1",
                &[("parent-child", "bd-e"), ("blocks", "bd-1")]
            ),
            // Its blocker is done, so it is not blocked any more.
            dependent("bd-3", "in_progress", "q-1", &[("blocks", "bd-done")]),
            // A blocker `bd` did not return cannot be judged, so it does
            // not block — see `is_blocked`.
            dependent("bd-4", "open", "q-1", &[("blocks", "bd-elsewhere")]),
            // Closed work is never reported blocked.
            dependent("bd-5", "closed", "q-1", &[("blocks", "bd-1")]),
            issue("bd-done", "closed", &["q-1"]),
        ])
        .to_string();
        let p = count(&raw, "q-1", Some("bd-e")).unwrap();
        assert_eq!(p.total, 6);
        assert_eq!(p.blocked, 1, "bd-2 only");
        // The overlay does not move an issue out of its own status bucket.
        assert_eq!(p.open, 3);
        assert_eq!(p.in_progress, 1);
        assert_eq!(p.closed, 2);
    }

    #[test]
    fn an_object_payload_and_garbage_are_both_handled() {
        let p = count(
            r#"{"issues":[{"id":"bd-1","status":"open","labels":["quest:q-1"]}]}"#,
            "q-1",
            None,
        )
        .unwrap();
        assert_eq!(p.total, 1);
        assert!(count("not json", "q-1", None).is_none());
        assert_eq!(count("[]", "q-1", None).unwrap(), Progress::default());
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
    fn the_epic_is_recoverable_from_a_listing() {
        let raw = serde_json::json!([
            {"id": "bd-1", "status": "open", "issue_type": "task", "labels": ["quest:q-1"]},
            {"id": "bd-e", "status": "open", "issue_type": "epic", "labels": ["quest:q-1"]},
            {"id": "bd-other", "status": "open", "issue_type": "epic", "labels": ["quest:q-2"]},
        ])
        .to_string();
        assert_eq!(epic_id(&raw, "q-1").unwrap(), "bd-e");
        assert_eq!(epic_id(&raw, "q-2").unwrap(), "bd-other");
        assert_eq!(epic_id(&raw, "q-3"), None);
        // A tracker with no epic for the Quest yields nothing to recover.
        assert_eq!(epic_id("[]", "q-1"), None);
    }

    #[test]
    fn the_argv_is_exactly_what_bd_accepts() {
        // Verified against bd 1.2.2 (`--dry-run` for the write).
        assert_eq!(
            create_argv("slug: goal", "repo:quest,quest:q-1"),
            [
                "create",
                "slug: goal",
                "--type",
                "epic",
                "-l",
                "repo:quest,quest:q-1",
                "--json"
            ]
        );
        assert_eq!(
            list_argv("-l", "quest:q-1"),
            [
                "list",
                "-l",
                "quest:q-1",
                "--all",
                "-n",
                "0",
                "--no-pager",
                "--json"
            ]
        );
        assert_eq!(
            close_argv("bd-e", "quest closed"),
            ["close", "bd-e", "--reason", "quest closed"]
        );
    }

    #[test]
    fn a_repo_label_may_not_carry_a_comma_or_whitespace() {
        assert_eq!(validate_repo_label("  quest ").unwrap(), "quest");
        // A blank value is how `q set` unlinks it.
        assert_eq!(validate_repo_label("").unwrap(), "");
        for bad in ["evil,repo:other", "two words", "tab\there"] {
            assert!(validate_repo_label(bad).is_err(), "{bad} must be rejected");
        }
        assert!(validate_repo_label(&"x".repeat(65)).is_err());
    }

    /// A directory `git` refuses to look above: a `.git` *file* pointing
    /// nowhere makes `rev-parse` fail outright instead of walking up, so the
    /// "no repository here" case is the same whether or not `$TMPDIR` itself
    /// happens to sit inside a checkout.
    fn no_repo_here(dir: &Path) -> PathBuf {
        let plain = dir.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join(".git"), "gitdir: nowhere-at-all").unwrap();
        plain
    }

    #[test]
    fn the_repo_label_prefers_the_flag_then_git_then_the_config() {
        let config = Config::default();
        let dir = tempfile::tempdir().unwrap();
        let plain = no_repo_here(dir.path());
        assert_eq!(repo_label(&config, Some("explicit"), &plain), "explicit");
        // A blank flag is not a label, and with no repository to name the
        // configured default is what is left.
        assert_eq!(
            repo_label(&config, Some("  "), &plain),
            config.beads.default_repo_label
        );
        assert_eq!(
            repo_label(&config, None, &plain),
            config.beads.default_repo_label
        );
    }

    /// `git init` in `dir`, or `false` when this machine has no usable git.
    fn git_init(dir: &Path) -> bool {
        std::process::Command::new("git")
            .args(["-C", &dir.to_string_lossy(), "init", "-q"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn git(args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn the_repo_label_uses_the_git_root_basename() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("some-repo");
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        if !git_init(&root) {
            return; // no git on this machine; the other cases still hold
        }
        assert_eq!(
            repo_label(&Config::default(), None, &nested),
            "some-repo".to_string()
        );
    }

    #[test]
    fn a_linked_worktree_is_labelled_after_its_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("some-repo");
        std::fs::create_dir_all(&root).unwrap();
        if !git_init(&root) {
            return;
        }
        let root_arg = root.to_string_lossy().into_owned();
        // A worktree needs a commit to branch from.
        std::fs::write(root.join("f"), "x").unwrap();
        let staged = git(&["-C", &root_arg, "add", "f"])
            && git(&[
                "-C",
                &root_arg,
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "init",
            ]);
        if !staged {
            return;
        }
        // The worktree is named after a branch, the way `q` itself is worked
        // on — and it must not become the label.
        let wt = root.join(".worktrees/feat-x");
        let wt_arg = wt.to_string_lossy().into_owned();
        if !git(&[
            "-C", &root_arg, "worktree", "add", "-q", "-b", "feat-x", &wt_arg,
        ]) {
            return;
        }
        assert_eq!(
            repo_label(&Config::default(), None, &wt),
            "some-repo".to_string(),
            "a linked worktree must be labelled after the repository"
        );
    }

    #[test]
    fn a_directory_name_that_cannot_be_a_label_is_folded() {
        assert_eq!(sanitize("some repo"), "some-repo");
        assert_eq!(sanitize("a,b"), "a-b");
        assert_eq!(sanitize("plain"), "plain");
    }

    #[test]
    fn a_relative_db_path_caches_beside_the_cwd() {
        // A bare filename has an empty parent, which must not become `/`.
        assert_eq!(cache_dir(Path::new("q.db")), Path::new("."));
        assert_eq!(cache_dir(Path::new("var/q.db")), Path::new("var"));
        assert_eq!(cache_dir(Path::new("/a/b/q.db")), Path::new("/a/b"));
    }

    /// A `bd` that only knows how to answer one listing.
    struct CannedBd(&'static str);

    impl Bd for CannedBd {
        fn create_epic(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
            unreachable!()
        }
        fn list_quest(&self, _: &str) -> Option<String> {
            Some(self.0.to_string())
        }
        fn list_quests(&self, _: &[&str]) -> Option<String> {
            Some(self.0.to_string())
        }
        fn close(&self, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }
        fn relabel_repo(&self, _: &str, _: Option<&str>, _: &str) -> Result<(), String> {
            unreachable!()
        }
    }

    /// A `bd` that never answers, and counts how often it was asked.
    #[derive(Default)]
    struct FailingBd(std::cell::Cell<usize>);

    impl Bd for FailingBd {
        fn create_epic(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
            unreachable!()
        }
        fn list_quest(&self, _: &str) -> Option<String> {
            self.0.set(self.0.get() + 1);
            None
        }
        fn list_quests(&self, _: &[&str]) -> Option<String> {
            self.0.set(self.0.get() + 1);
            None
        }
        fn close(&self, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }
        fn relabel_repo(&self, _: &str, _: Option<&str>, _: &str) -> Result<(), String> {
            unreachable!()
        }
    }

    /// A failed read writes no cache, so nothing stops the next caller asking
    /// again — and on the TUI's 2 s tick a `bd` that is present but slow then
    /// costs `READ_TIMEOUT` on the UI thread every tick, which is longer than
    /// the tick. The keyboard goes dead while the process looks idle.
    #[test]
    fn a_failing_bd_is_not_respawned_on_every_tick() {
        LAST_FAILURE.store(0, Ordering::Relaxed);
        let mut quest = Quest::new("slow", "/tmp/work", "laptop");
        quest.beads_epic = Some("bd-42".to_string());
        let bd = FailingBd::default();

        assert!(progress_all_with(&bd, &[&quest]).is_empty());
        assert_eq!(bd.0.get(), 1);
        for _ in 0..10 {
            assert!(progress_all_with(&bd, &[&quest]).is_empty());
        }
        assert_eq!(bd.0.get(), 1, "bd was respawned inside the backoff window");

        // The window is not a lockout: once it has passed, bd is asked again.
        LAST_FAILURE.store(now() - FAILURE_TTL, Ordering::Relaxed);
        assert!(progress_all_with(&bd, &[&quest]).is_empty());
        assert_eq!(bd.0.get(), 2);
        LAST_FAILURE.store(0, Ordering::Relaxed);
    }

    #[test]
    fn a_blocker_outside_the_payload_does_not_block_forever() {
        // bd returned the dependency but not the issue it points at (another
        // repo, a narrower filter): q cannot know it is open, and calling it
        // blocked would leave a row nobody can unblock.
        let raw = serde_json::json!([
            dependent("bd-1", "open", "q-1", &[("blocks", "bd-elsewhere")]),
            dependent("bd-2", "open", "q-1", &[("blocks", "bd-3")]),
            issue("bd-3", "open", &["q-1"]),
        ])
        .to_string();
        let p = count(&raw, "q-1", None).unwrap();
        assert_eq!(p.blocked, 1, "only the resolvable blocker counts");
        let rows = rows(&raw, "q-1", None).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(!rows[0].blocked);
        assert!(rows[1].blocked);
    }

    #[test]
    fn the_rows_are_exactly_what_the_counts_counted() {
        let raw = serde_json::json!([
            issue("bd-e", "open", &["q-1"]),
            issue("bd-1", "closed", &["q-1"]),
            issue("bd-2", "open", &["q-1"]),
            issue("bd-9", "open", &["q-2"]),
        ])
        .to_string();
        let p = count(&raw, "q-1", Some("bd-e")).unwrap();
        let rows = rows(&raw, "q-1", Some("bd-e")).unwrap();
        // The epic is in neither, the other Quest's issue is in neither, and
        // the two agree on the total — the guarantee `q brief` leans on.
        assert_eq!(p.total, rows.len());
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["bd-1", "bd-2"]
        );
        assert_eq!(
            rows.iter().filter(|r| r.status == "closed").count(),
            p.closed
        );
    }

    /// A payload past [`proc`]'s default capture cap really does have to come
    /// back whole: truncated JSON parses as nothing, so the counts would be
    /// silently absent rather than wrong.
    #[test]
    fn a_listing_larger_than_the_default_capture_cap_survives_the_read() {
        let big: Vec<Value> = (0..200)
            .map(|n| {
                serde_json::json!({
                    "id": format!("bd-{n}"),
                    "status": if n % 2 == 0 { "closed" } else { "open" },
                    "title": "x".repeat(400),
                    "labels": ["quest:q-1"],
                })
            })
            .collect();
        let payload = serde_json::to_string(&big).unwrap();
        assert!(
            payload.len() > proc::MAX_CAPTURE,
            "the fixture must exceed the default cap: {}",
            payload.len()
        );
        assert!(READ_CAPTURE > payload.len());
        // Through the very call `RealBd::list_quest` makes, cap included.
        let script = format!("cat <<'EOF'\n{payload}\nEOF");
        let read = proc::run_capped_bounded("sh", &["-c", &script], READ_TIMEOUT, READ_CAPTURE)
            .expect("the read succeeded");
        let p = count(&read, "q-1", None).expect("a whole payload parses");
        assert_eq!(p.total, 200);
        assert_eq!(p.closed, 100);
        // And this is what the default cap would have done to it.
        let truncated = proc::run_capped("sh", &["-c", &script], READ_TIMEOUT).unwrap();
        assert!(count(&truncated, "q-1", None).is_none());
    }

    #[test]
    fn relabelling_the_epic_is_one_write() {
        assert_eq!(
            relabel_argv("bd-e", Some("old"), "new"),
            [
                "update",
                "bd-e",
                "--remove-label",
                "repo:old",
                "--add-label",
                "repo:new"
            ]
        );
        // Nothing to remove: only the new label goes on.
        assert_eq!(
            relabel_argv("bd-e", None, "new"),
            ["update", "bd-e", "--add-label", "repo:new"]
        );
        assert_eq!(
            relabel_argv("bd-e", Some("  "), "new"),
            ["update", "bd-e", "--add-label", "repo:new"]
        );
    }

    #[test]
    fn a_blank_epic_column_is_not_an_epic() {
        // `bd close` with no id closes "the last touched issue", so an empty
        // column must never reach it.
        let mut quest = Quest::new("slug", "cwd", "m");
        assert_eq!(epic_of(&quest), None);
        quest.beads_epic = Some(String::new());
        assert_eq!(epic_of(&quest), None);
        quest.beads_epic = Some("   ".to_string());
        assert_eq!(epic_of(&quest), None);
        quest.beads_epic = Some(" bd-e ".to_string());
        assert_eq!(epic_of(&quest), Some("bd-e"));
    }

    #[test]
    fn find_epic_recovers_the_id_a_killed_create_left_behind() {
        let canned = CannedBd(
            r#"[{"id":"bd-e","status":"open","issue_type":"epic","labels":["quest:q-1"]}]"#,
        );
        assert_eq!(canned.find_epic("q-1").unwrap(), "bd-e");
        // The epic has to be labelled for this Quest to be adopted.
        assert_eq!(canned.find_epic("q-2"), None);
        // Nothing in the tracker, nothing to recover.
        assert_eq!(CannedBd("[]").find_epic("q-1"), None);
    }
}
