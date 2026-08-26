//! Auto-naming (SPEC §10).
//!
//! A Quest's slug is given by hand (`q new --name`, `q rename`), by a template,
//! or generated: `claude -p --model <naming.model>` is handed the goal, the
//! working directory, the git branch and the master's first prompt, and must
//! answer with one kebab-case token. Anything else — no `claude`, a timeout, a
//! sentence instead of a slug — falls back to a heuristic (the git branch, else
//! the first prompt), marked `heuristic` so nobody mistakes it for a model
//! answer.
//!
//! Everything q starts here is isolated from the Quest it is naming: the pane
//! environment is scrubbed ([`PANE_ENV`]), `$Q_NAMING` makes any `q hook` it
//! reaches a no-op ([`suppressed`]), and `claude -p` is given neither settings
//! nor tools. Without that, the naming subprocess inherits `$Q_QUEST` and its
//! own hooks write to the master's row — brief injected into the naming prompt,
//! `Stop` flipping the master idle mid-turn, and that `Stop` scheduling naming
//! again.
//!
//! The generated answer is cached by a hash of that input, so the same state
//! never pays for a second model call; a rejected answer is *not* cached. The
//! same hash drives regeneration: the master's `Stop` hook compares it against
//! `quest.name_input_hash` and, when they differ, spawns
//! `q name <quest> --auto --apply` in the background. No periodic job.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::commands::new::{GENERIC_BRANCHES, SLUG_MAX, git_branch, is_slug, slugify};
use crate::config::Config;
use crate::db::Db;
use crate::model::{NameOrigin, NameSource, Quest, Session, SessionRole, SessionStatus};

/// The model gets one shot and the `Stop` hook must not stall behind it, so the
/// wait is bounded. Generation happens detached, off the hook's critical path.
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(20);
/// How much of the master's first prompt goes into the input (and its hash).
const PROMPT_CHARS: usize = 2_000;
/// A heuristic slug from a prompt keeps at most this many words.
const HEURISTIC_WORDS: usize = 4;
/// How far down a chatty answer a slug is still looked for. A model that
/// wrapped the token in a code fence put it on line 2; one that wrote an essay
/// did not answer the question.
const ANSWER_LINES: usize = 10;
/// Shortest lone token (no hyphen) taken as an answer. Below it a slug-shaped
/// line is far more likely to be a stray word than a name.
const LONE_TOKEN_MIN: usize = 4;
/// Lone tokens that are valid slugs but never an answer to "name this work" —
/// a chatty model opens with one of these on its own line. Only consulted for a
/// token with no hyphen in it.
const FILLER: [&str; 14] = [
    "sure", "okay", "yes", "yeah", "done", "text", "slug", "name", "here", "hmm", "thanks",
    "answer", "output", "none",
];

/// The environment that marks a Quest pane (SPEC §7). Every process q starts
/// for its own bookkeeping drops all of it: inherited, the child's own hooks —
/// and any `q hook` they run — would resolve to the very Quest being named and
/// overwrite the master's row, inject its brief into the naming prompt, flip it
/// idle mid-turn, and schedule another naming run from that `Stop`.
pub const PANE_ENV: [&str; 4] = ["Q_QUEST", "Q_SESSION", "Q_ROLE", "TMUX_PANE"];

/// Set on every process q starts for naming. `q hook <event>` does nothing at
/// all when it is set — the belt to the scrubbed environment's braces, for a
/// hook that finds a Quest some other way (a `$Q_QUEST` a user exported by
/// hand, a settings file q did not write).
pub const NAMING_ENV: &str = "Q_NAMING";

/// Whether this process was started by q for naming, and so must not act as a
/// hook.
pub fn suppressed() -> bool {
    std::env::var_os(NAMING_ENV).is_some_and(|v| !v.is_empty())
}

/// Strips the Quest identity from a child's environment and marks it as a
/// naming process. Every `Command` naming spawns goes through here.
fn scrub(cmd: &mut Command) -> &mut Command {
    for key in PANE_ENV {
        cmd.env_remove(key);
    }
    cmd.env(NAMING_ENV, "1")
}

/// What the model is asked about, and what the cache is keyed on (SPEC §10).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Input {
    pub goal: Option<String>,
    pub cwd: String,
    pub branch: Option<String>,
    pub first_prompt: Option<String>,
}

impl Input {
    /// The Quest's goal and cwd, the branch checked out there, and the first
    /// prompt the master was given.
    pub fn collect(db: &Db, quest: &Quest) -> Input {
        let cwd = Path::new(&quest.cwd);
        Input {
            goal: trimmed(quest.goal.as_deref()),
            cwd: quest.cwd.clone(),
            branch: git_branch(cwd),
            first_prompt: master_prompt(db, quest),
        }
    }

    /// The canonical rendering the hash is taken over — one `key: value` per
    /// line, always in this order, so an unchanged Quest hashes the same on
    /// every machine and across releases.
    fn canonical(&self) -> String {
        format!(
            "goal: {}\ncwd: {}\nbranch: {}\nprompt: {}\n",
            self.goal.as_deref().unwrap_or(""),
            self.cwd,
            self.branch.as_deref().unwrap_or(""),
            truncate(self.first_prompt.as_deref().unwrap_or(""), PROMPT_CHARS),
        )
    }

    /// sha256 of `canonical`, hex.
    pub fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.canonical().as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// The prompt handed to `claude -p` on stdin.
    fn prompt(&self) -> String {
        let mut out = String::from(
            "Name a software work session with one short slug.\n\
             Answer with the slug and nothing else: no quotes, no backticks, no explanation.\n\
             Rules: 2-4 words, lowercase a-z and digits joined by single hyphens, \
             at most 40 characters.\n\
             Name the work, not the tool: prefer `cdc-backfill-retry` over `bugfix`.\n\n\
             Session:\n",
        );
        for (key, value) in [
            ("goal", self.goal.as_deref()),
            ("directory", Some(self.cwd.as_str())),
            ("git branch", self.branch.as_deref()),
            (
                "first prompt",
                self.first_prompt.as_deref().map(|p| p.trim()),
            ),
        ] {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                out.push_str(&format!("- {key}: {}\n", truncate(value, PROMPT_CHARS)));
            }
        }
        out
    }
}

/// One proposed slug, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proposal {
    pub slug: String,
    pub source: NameOrigin,
    /// The answer came from `name_cache` rather than a fresh model call.
    pub cached: bool,
    pub input_hash: String,
}

impl Proposal {
    /// `cdc-backfill` or `cdc-backfill (heuristic)` — how a proposal reads.
    pub fn describe(&self) -> String {
        match self.source {
            NameOrigin::Claude => self.slug.clone(),
            NameOrigin::Heuristic => format!("{} (heuristic)", self.slug),
        }
    }
}

/// What auto-naming shells out to. Stubbed in tests and under `Q_FIXTURE`.
pub trait Namer {
    /// The model's raw answer, or `None` when `claude` is missing, failed or
    /// timed out.
    fn suggest(&self, model: &str, prompt: &str) -> Option<String>;
}

/// The real `claude`, or — under `$Q_FIXTURE` — the canned answer in the file
/// `$Q_FIXTURE_CLAUDE_NAME` (absent file = `claude` unavailable), following the
/// convention `brief.rs` uses for `bd` and `brain`.
pub fn namer() -> Box<dyn Namer> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureNamer),
        _ => Box::new(ClaudeNamer {
            launcher: Box::new(ProcLauncher),
        }),
    }
}

/// How a built `Command` is finally run. The model's answer is all the namer
/// wants back, so this is the whole surface — and it is the seam a test uses to
/// inspect the `Command` without starting anything.
pub trait Launcher {
    /// The child's stdout, or `None` when it could not be started, exited
    /// non-zero or outstayed [`CLAUDE_TIMEOUT`].
    fn run(&self, cmd: &mut Command, input: &[u8]) -> Option<String>;
}

struct ProcLauncher;

impl Launcher for ProcLauncher {
    fn run(&self, cmd: &mut Command, input: &[u8]) -> Option<String> {
        let out = crate::proc::run(cmd, input, CLAUDE_TIMEOUT).ok()?;
        out.success().then(|| out.text())
    }
}

/// The `claude -p` naming call, built where a test can read it back.
///
/// `--setting-sources ""` loads no settings file, so none of q's own hooks
/// (nor MCP servers, nor `CLAUDE.md`) reach a subprocess whose whole job is to
/// answer with one word; `--tools ""` leaves it nothing to run. Together with
/// [`scrub`] that is three independent reasons this child cannot write to the
/// Quest it is naming.
fn claude_command(model: &str) -> Command {
    let mut cmd = Command::new("claude");
    cmd.args([
        "-p",
        "--model",
        model,
        "--setting-sources",
        "",
        "--tools",
        "",
    ]);
    scrub(&mut cmd);
    cmd
}

struct ClaudeNamer {
    launcher: Box<dyn Launcher>,
}

impl Namer for ClaudeNamer {
    fn suggest(&self, model: &str, prompt: &str) -> Option<String> {
        self.launcher
            .run(&mut claude_command(model), prompt.as_bytes())
    }
}

struct FixtureNamer;

impl Namer for FixtureNamer {
    fn suggest(&self, _model: &str, _prompt: &str) -> Option<String> {
        std::fs::read_to_string(std::env::var_os("Q_FIXTURE_CLAUDE_NAME")?).ok()
    }
}

/// No model at all, for unit tests.
#[cfg(test)]
pub struct NoNamer;

#[cfg(test)]
impl Namer for NoNamer {
    fn suggest(&self, _model: &str, _prompt: &str) -> Option<String> {
        None
    }
}

/// The slug this Quest should carry, from the cache, the model, or the
/// heuristic. `refresh` ignores the cache; only a validated model answer is
/// written back to it (SPEC §10).
pub fn propose(
    db: &Db,
    quest: &Quest,
    input: &Input,
    model: &str,
    namer: &dyn Namer,
    refresh: bool,
) -> anyhow::Result<Proposal> {
    let input_hash = input.hash();
    if !refresh && let Some(hit) = db.name_cache_get(&input_hash)? {
        return Ok(Proposal {
            slug: hit.slug,
            source: hit.source,
            cached: true,
            input_hash,
        });
    }
    if let Some(slug) = namer
        .suggest(model, &input.prompt())
        .as_deref()
        .and_then(sanitize)
    {
        db.name_cache_put(&input_hash, &slug, NameOrigin::Claude)?;
        return Ok(Proposal {
            slug,
            source: NameOrigin::Claude,
            cached: false,
            input_hash,
        });
    }
    Ok(Proposal {
        slug: heuristic(input, quest),
        source: NameOrigin::Heuristic,
        cached: false,
        input_hash,
    })
}

/// A model answer reduced to a slug, or `None` when it is not one.
///
/// The first line is usually the whole answer, but a model that fenced its
/// token put `` ```text `` there instead — and `text` is a perfectly valid
/// slug, so taking the first line alone would name the Quest after the fence.
/// Fence lines are dropped and every *remaining* line that is a slug on its
/// own — once the punctuation a chatty model wraps a token in is stripped — is a
/// candidate. A prose line is never a slug (it has spaces), so an explanation
/// still falls through to the heuristic.
///
/// A hyphen is what an answer to *this* question looks like, so a hyphenated
/// candidate always beats a lone token above it (`"Sure\ncdc-backfill"`). A lone
/// token is only taken when it could not be an acknowledgement: long enough, and
/// not one of [`FILLER`].
fn sanitize(raw: &str) -> Option<String> {
    let candidates: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with("~~~"))
        .take(ANSWER_LINES)
        .filter_map(|line| {
            let slug = line.trim_matches(|c: char| {
                c.is_whitespace()
                    || matches!(c, '`' | '"' | '\'' | '.' | ',' | ':' | ';' | '*' | '#')
            });
            (slug.len() <= SLUG_MAX && is_slug(slug)).then_some(slug)
        })
        .collect();
    candidates
        .iter()
        .find(|s| s.contains('-'))
        .or_else(|| {
            candidates
                .iter()
                .find(|s| s.len() >= LONE_TOKEN_MIN && !FILLER.contains(*s))
        })
        .map(|s| s.to_string())
}

/// The fallback when the model is unavailable or unusable: the git branch if it
/// says anything, else the first prompt, else the directory, else the slug the
/// Quest already has (which is never worse than what it has).
fn heuristic(input: &Input, quest: &Quest) -> String {
    let from_branch = input
        .branch
        .as_deref()
        .filter(|b| !GENERIC_BRANCHES.contains(b))
        .map(slugify)
        .filter(|s| !s.is_empty());
    if let Some(slug) = from_branch {
        return slug;
    }
    let from_prompt = input
        .first_prompt
        .as_deref()
        .map(|p| first_words(&slugify(p), HEURISTIC_WORDS))
        .filter(|s| !s.is_empty());
    if let Some(slug) = from_prompt {
        return slug;
    }
    Path::new(&input.cwd)
        .file_name()
        .map(|n| slugify(&n.to_string_lossy()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| quest.slug.clone())
}

/// The first `n` hyphen-separated words of an already-slugified string.
fn first_words(slug: &str, n: usize) -> String {
    slug.split('-')
        .filter(|p| !p.is_empty())
        .take(n)
        .collect::<Vec<_>>()
        .join("-")
}

// ------------------------------------------------------ Claude session names

/// A Claude session carries its own name (`claude -n <slug>/<label>`), which a
/// Quest rename has to follow. `/rename` is send-keys into a live TUI, so it
/// only goes out when the session is idle by the same gate `q send` uses (SPEC
/// §23 #5); otherwise the new name is parked in `session.pending_rename` and
/// the session's next `Stop` hook flushes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Sync {
    /// Labels told right away.
    pub told: Vec<String>,
    /// Labels whose `/rename` is held until they go idle.
    pub pending: Vec<String>,
}

/// Tells every live session of `quest` its new `<slug>/<label>`.
///
/// `previous_slug` is what the Quest was called a moment ago, which is what
/// Claude is still called: the registry's identity check needs the name Claude
/// has, not the one it is about to be given (SPEC §23 #5).
pub fn sync_claude_names(
    db: &Db,
    tmux: &dyn crate::tmux::Tmux,
    quest: &Quest,
    previous_slug: &str,
) -> anyhow::Result<Sync> {
    // One `list-panes` for the whole fleet: the pane pid is how the registry
    // finds Claude when no hook ever recorded its own pid.
    let panes = tmux.list_panes().unwrap_or_default();
    let mut out = Sync::default();
    for session in db.list_sessions_by_quest(&quest.id)? {
        if session.status == SessionStatus::Ended {
            continue;
        }
        let pane_pid = pane_pid(&panes, &session.tmux_pane);
        let desired = claude_name(&quest.slug, &session.label);
        let current = current_name(&session, previous_slug);
        if crate::commands::target::refusal(&session, pane_pid, Some(&current)).is_none()
            && send_rename(tmux, &session.tmux_pane, &desired).is_ok()
        {
            db.record_claude_name(&session.id, &desired)?;
            out.told.push(session.label.clone());
        } else {
            db.set_pending_rename(&session.id, Some(&desired))?;
            out.pending.push(session.label.clone());
        }
    }
    Ok(out)
}

/// The name Claude currently answers to: the one q last told it, else the one
/// it was launched with (`<previous slug>/<label>`, SPEC §6).
fn current_name(session: &Session, previous_slug: &str) -> String {
    session
        .claude_name
        .clone()
        .unwrap_or_else(|| claude_name(previous_slug, &session.label))
}

pub fn claude_name(slug: &str, label: &str) -> String {
    format!("{slug}/{label}")
}

/// The pane's own process id, out of an already-fetched `list-panes`.
fn pane_pid(panes: &[crate::tmux::Pane], pane_id: &str) -> Option<i64> {
    panes
        .iter()
        .find(|p| p.pane_id == pane_id)
        .map(|p| i64::from(p.pane_pid))
}

fn send_rename(tmux: &dyn crate::tmux::Tmux, pane: &str, name: &str) -> anyhow::Result<()> {
    tmux.send_keys(pane, &format!("/rename {name}"), true)
}

// ------------------------------------------------------------- the Stop hook

/// The master's `Stop` hook, in one call (SPEC §7, §10): flush a `/rename` this
/// session still owes, then — for a master whose name is auto — schedule a
/// regeneration when the naming input has changed.
///
/// Best effort throughout: a hook must never fail, so every step swallows its
/// own errors.
pub fn maybe_rename(db: &Db, session: &Session) {
    let Ok(Some(quest)) = db.get_quest(&session.quest_id) else {
        return;
    };
    flush_pending(db, &quest, session);
    let _ = schedule(db, &quest, session);
}

/// The session just went idle, so a held `/rename` can go out now — unless
/// Claude's own registry still disagrees.
fn flush_pending(db: &Db, quest: &Quest, session: &Session) {
    let Some((session, desired)) = owed(db, quest, session) else {
        return;
    };
    // The `list-panes` and the `send-keys` below are unbounded tmux calls on a
    // hook's critical path. Both are local socket round trips against a server
    // this pane is already living in — a tmux that cannot answer them has the
    // user's terminal wedged too — and every other hook path already makes
    // them (the liveness sweep, `q peek`), so they are taken as they are.
    let tmux = crate::tmux::tmux();
    let pane_pid = pane_pid(&tmux.list_panes().unwrap_or_default(), &session.tmux_pane);
    // Claude still answers to whatever it was last told, which is never the
    // parked name — that is precisely the send that did not happen.
    if crate::commands::target::refusal(&session, pane_pid, session.claude_name.as_deref())
        .is_some()
    {
        return;
    }
    if send_rename(tmux.as_ref(), &session.tmux_pane, &desired).is_err() {
        return;
    }
    let _ = db.transaction(|db| {
        db.record_claude_name(&session.id, &desired)?;
        db.append_event(
            &quest.id,
            Some(&session.id),
            "name.synced",
            &serde_json::json!({ "name": desired }),
        )?;
        Ok(())
    });
}

/// The row `stop` just wrote — not the one it was handed — and the `/rename` it
/// owes. `None` when nothing is owed, or when the session is not idle after
/// all: `stop`'s write may have been dropped on a lock timeout, and a
/// `Notification` racing this turn may have moved the session to `waiting`,
/// where a `/rename` would answer a permission prompt (SPEC §8 — the hazard
/// `reset::schedule` guards the same way).
fn owed(db: &Db, quest: &Quest, session: &Session) -> Option<(Session, String)> {
    let row = db.get_session(&session.id).ok().flatten()?;
    row.pending_rename.as_deref()?;
    if row.status != SessionStatus::Idle {
        return None;
    }
    // The Quest may have been renamed again while the send was held, so the
    // name that goes out is the current one, not the parked text.
    let desired = claude_name(&quest.slug, &row.label);
    Some((row, desired))
}

/// Regenerate in the background when this is the master of an auto-named Quest
/// and the input no longer hashes to `quest.name_input_hash`.
fn schedule(db: &Db, quest: &Quest, session: &Session) -> anyhow::Result<()> {
    if session.role != SessionRole::Master || quest.name_source != NameSource::Auto {
        return Ok(());
    }
    if !Config::load().unwrap_or_default().naming.auto {
        return Ok(());
    }
    let hash = Input::collect(db, quest).hash();
    if quest.name_input_hash.as_deref() == Some(hash.as_str()) {
        return Ok(());
    }
    db.append_event(
        &quest.id,
        Some(&session.id),
        "name.scheduled",
        &serde_json::json!({ "input_hash": hash }),
    )?;
    // No `--detach`: `spawn_detached` already is the detach, and the flag would
    // make the child fork a grandchild and exit.
    spawn_detached(&[
        "name".to_string(),
        quest.id.clone(),
        "--auto".to_string(),
        "--apply".to_string(),
    ])
}

/// Runs this binary again, fully detached: no stdio, no wait, so the caller
/// (a hook, or `q name --detach`) returns immediately.
pub fn spawn_detached(args: &[String]) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    if let Some(path) = record_path() {
        return record_argv(&path, &exe, args);
    }
    detached_command(&exe, args).spawn()?;
    Ok(())
}

/// How the child is configured, kept separate from the spawn so a test can
/// read the `Command` back.
///
/// No stdio: a hook's pipes must not be held open by a child that outlives it.
/// Its own process group: Claude kills the hook's group when the hook times out
/// or its pane goes away, and naming has to survive that (the same reason
/// `proc::run` groups its children). No Quest identity, and `$Q_NAMING` set —
/// see [`scrub`].
fn detached_command(exe: &Path, args: &[String]) -> Command {
    use std::os::unix::process::CommandExt;

    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    scrub(&mut cmd);
    cmd
}

/// Where a spawn records its argv instead of happening — `$Q_NO_DETACH`, so
/// tests can assert on what a hook *would* have run without starting a
/// process. Honoured only under `$Q_FIXTURE`: it is a test hook, and a stray
/// export in a real shell must not be able to quietly turn naming off.
fn record_path() -> Option<PathBuf> {
    std::env::var_os("Q_FIXTURE").filter(|v| !v.is_empty())?;
    std::env::var_os("Q_NO_DETACH")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn record_argv(path: &Path, exe: &Path, args: &[String]) -> anyhow::Result<()> {
    let line = serde_json::json!({ "exe": exe, "args": args }).to_string();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

// ----------------------------------------------------------------- internals

/// The master's first prompt — what the Quest was actually asked to do. The
/// oldest master session wins, so a `q resume` does not rewrite the input.
fn master_prompt(db: &Db, quest: &Quest) -> Option<String> {
    let mut masters: Vec<Session> = db
        .list_sessions_by_quest(&quest.id)
        .ok()?
        .into_iter()
        .filter(|s| s.role == SessionRole::Master && s.first_prompt.is_some())
        .collect();
    masters.sort_by_key(|s| (s.started_at, s.id.clone()));
    masters.into_iter().next().and_then(|s| s.first_prompt)
}

fn trimmed(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// At most `max` chars, on a char boundary.
fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quest(slug: &str) -> Quest {
        Quest::new(slug, "/tmp/some-repo", "laptop")
    }

    fn input() -> Input {
        Input {
            goal: Some("retry the cdc backfill".to_string()),
            cwd: "/tmp/some-repo".to_string(),
            branch: Some("feat/cdc".to_string()),
            first_prompt: Some("fix the backfill".to_string()),
        }
    }

    /// Reads back what a `Command` was configured with, in the only two forms
    /// `Command` exposes: the argv, and the environment deltas (a `None` value
    /// is a removal).
    fn argv(cmd: &Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn removed(cmd: &Command) -> Vec<String> {
        cmd.get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect()
    }

    fn set(cmd: &Command, key: &str) -> Option<String> {
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v)
            .map(|v| v.to_string_lossy().into_owned())
    }

    /// What a launcher was handed, in the only forms `Command` exposes.
    #[derive(Debug, Default)]
    struct Seen {
        argv: Vec<String>,
        removed: Vec<String>,
        naming: Option<String>,
        input: String,
    }

    /// A launcher that records the `Command` and starts nothing.
    struct Spy {
        answer: Option<&'static str>,
        seen: std::rc::Rc<std::cell::RefCell<Seen>>,
    }

    impl Launcher for Spy {
        fn run(&self, cmd: &mut Command, input: &[u8]) -> Option<String> {
            let mut removed = removed(cmd);
            removed.sort();
            *self.seen.borrow_mut() = Seen {
                argv: argv(cmd),
                removed,
                naming: set(cmd, NAMING_ENV),
                input: String::from_utf8_lossy(input).into_owned(),
            };
            self.answer.map(str::to_string)
        }
    }

    /// The one that matters most (round-1 review, blocking #1): a `claude -p`
    /// that inherited `$Q_QUEST` runs q's own hooks against the very Quest
    /// being named — overwriting its master's row, injecting its brief into the
    /// naming prompt and scheduling another naming run from the `Stop` that
    /// follows.
    #[test]
    fn the_naming_subprocess_carries_no_quest_identity() {
        let seen = std::rc::Rc::new(std::cell::RefCell::new(Seen::default()));
        let namer = ClaudeNamer {
            launcher: Box::new(Spy {
                answer: Some("cdc-backfill"),
                seen: std::rc::Rc::clone(&seen),
            }),
        };
        assert_eq!(
            namer.suggest("haiku", "name this"),
            Some("cdc-backfill".to_string())
        );

        let seen = seen.borrow();
        let mut expected = PANE_ENV.to_vec();
        expected.sort();
        assert_eq!(seen.removed, expected);
        assert_eq!(seen.naming.as_deref(), Some("1"));
        assert_eq!(
            seen.argv,
            [
                "claude",
                "-p",
                "--model",
                "haiku",
                // No settings file, so none of q's hooks load in the child.
                "--setting-sources",
                "",
                // Naming reads nothing and writes nothing.
                "--tools",
                "",
            ]
        );
        assert_eq!(seen.input, "name this");
    }

    #[test]
    fn the_detached_child_is_scrubbed_and_leads_its_own_process_group() {
        let args = vec!["name".to_string(), "q-0001".to_string()];
        let cmd = detached_command(Path::new("/bin/echo"), &args);
        let mut removals = removed(&cmd);
        removals.sort();
        let mut expected = PANE_ENV.to_vec();
        expected.sort();
        assert_eq!(removals, expected);
        assert_eq!(set(&cmd, NAMING_ENV).as_deref(), Some("1"));
        assert_eq!(argv(&cmd), ["/bin/echo", "name", "q-0001"]);

        // `process_group` has no getter, so it is checked where it shows: the
        // child is its own group leader, which is what lets naming outlive the
        // hook whose group Claude kills.
        let mut sleeper = detached_command(
            Path::new("/bin/sh"),
            &["-c".to_string(), "sleep 5".to_string()],
        );
        let mut child = sleeper.spawn().expect("spawned");
        let pid = child.id();
        let pgid = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &pid.to_string()])
            .output()
            .expect("ps ran");
        let pgid = String::from_utf8_lossy(&pgid.stdout).trim().to_string();
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(pgid, pid.to_string(), "the child joined its parent's group");
    }

    #[test]
    fn scrub_marks_every_child_as_a_naming_process() {
        // `suppressed` is what makes `q hook <event>` a no-op inside one, and
        // the variable it reads is the one `scrub` sets.
        let mut cmd = Command::new("true");
        scrub(&mut cmd);
        assert_eq!(set(&cmd, NAMING_ENV).as_deref(), Some("1"));
    }

    #[test]
    fn a_bare_slug_is_accepted() {
        assert_eq!(sanitize("cdc-backfill"), Some("cdc-backfill".to_string()));
        assert_eq!(
            sanitize("  cdc-backfill  \n"),
            Some("cdc-backfill".to_string())
        );
        assert_eq!(sanitize("`cdc-backfill`"), Some("cdc-backfill".to_string()));
        assert_eq!(
            sanitize("\"cdc-backfill\""),
            Some("cdc-backfill".to_string())
        );
        assert_eq!(
            sanitize("cdc-backfill.\n\nthoughts"),
            Some("cdc-backfill".to_string())
        );
        // A lone token with no hyphen is still an answer when it is long
        // enough not to be an acknowledgement.
        assert_eq!(sanitize("\n\nbackfill"), Some("backfill".to_string()));
    }

    /// Round-2 review, low #2: any lone lowercase token used to be taken as the
    /// answer, so a model that opened with "Sure" named the Quest `sure`. A
    /// hyphenated candidate now wins over anything above it, and a lone token
    /// has to be long enough and not a filler word.
    #[test]
    fn a_filler_word_never_wins_over_the_real_token() {
        assert_eq!(
            sanitize("Sure\ncdc-backfill"),
            Some("cdc-backfill".to_string())
        );
        assert_eq!(
            sanitize("okay\n\nhere\ncdc-backfill\nhope that helps"),
            Some("cdc-backfill".to_string())
        );
        // Nothing but filler is no answer at all.
        assert_eq!(sanitize("sure\nokay"), None);
        for filler in FILLER {
            assert_eq!(sanitize(filler), None, "accepted `{filler}`");
        }
        // Short lone tokens are rejected whatever they say.
        assert_eq!(sanitize("q1"), None);
        assert_eq!(sanitize("cdc"), None);
        // …but a hyphen makes even a short one an answer.
        assert_eq!(sanitize("a-b"), Some("a-b".to_string()));
    }

    /// Round-1 review, blocking #2: the first line of a fenced answer is the
    /// fence, and `text` in ```` ```text ```` is itself a valid slug — so
    /// reading only the first line named the Quest after the fence language.
    #[test]
    fn a_fenced_answer_yields_the_token_not_the_fence() {
        for fenced in [
            "```text\ncdc-backfill\n```",
            "```\ncdc-backfill\n```",
            "```bash\ncdc-backfill\n```",
            "~~~\ncdc-backfill\n~~~",
            "```text\ncdc-backfill\n```\nHope that helps!",
        ] {
            assert_eq!(
                sanitize(fenced),
                Some("cdc-backfill".to_string()),
                "on `{fenced}`"
            );
        }
        // A fence with nothing usable inside is still no answer.
        assert_eq!(sanitize("```text\nnot a slug at all\n```"), None);
    }

    #[test]
    fn a_slug_after_a_line_of_preamble_is_still_found() {
        assert_eq!(
            sanitize("Here you go:\ncdc-backfill"),
            Some("cdc-backfill".to_string())
        );
        // Prose all the way down is not an answer, and the scan is bounded.
        let essay = (0..50)
            .map(|n| format!("line {n} of prose"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(sanitize(&format!("{essay}\ncdc-backfill")), None);
    }

    #[test]
    fn anything_that_is_not_a_slug_is_rejected() {
        for bad in [
            "",
            "   ",
            "Sure! Here is a slug: cdc-backfill",
            "cdc backfill",
            "CDC-Backfill",
            "cdc--backfill",
            "-cdc",
            "cdc_backfill",
            "cdc/backfill",
            &"x".repeat(SLUG_MAX + 1),
        ] {
            assert_eq!(sanitize(bad), None, "accepted `{bad}`");
        }
    }

    #[test]
    fn the_hash_is_stable_and_input_sensitive() {
        let a = input();
        assert_eq!(a.hash(), a.clone().hash());
        assert_eq!(a.hash().len(), 64);
        for mutate in [
            (|i: &mut Input| i.goal = None) as fn(&mut Input),
            |i: &mut Input| i.cwd = "/tmp/other".to_string(),
            |i: &mut Input| i.branch = Some("main".to_string()),
            |i: &mut Input| i.first_prompt = Some("something else".to_string()),
        ] {
            let mut b = a.clone();
            mutate(&mut b);
            assert_ne!(a.hash(), b.hash(), "{b:?} hashed the same");
        }
    }

    #[test]
    fn the_hash_ignores_prompt_text_past_the_cap() {
        let mut a = input();
        a.first_prompt = Some("x".repeat(PROMPT_CHARS));
        let mut b = a.clone();
        b.first_prompt = Some("x".repeat(PROMPT_CHARS + 500));
        assert_eq!(a.hash(), b.hash());
    }

    #[test]
    fn the_prompt_carries_what_is_known_and_omits_what_is_not() {
        let text = input().prompt();
        assert!(text.contains("- goal: retry the cdc backfill"), "{text}");
        assert!(text.contains("- git branch: feat/cdc"), "{text}");
        assert!(text.contains("- first prompt: fix the backfill"), "{text}");

        let bare = Input {
            cwd: "/tmp/some-repo".to_string(),
            ..Input::default()
        };
        let text = bare.prompt();
        assert!(!text.contains("goal:"), "{text}");
        assert!(!text.contains("git branch:"), "{text}");
        assert!(text.contains("- directory: /tmp/some-repo"), "{text}");
    }

    #[test]
    fn the_heuristic_prefers_a_meaningful_branch() {
        let q = quest("old-name");
        assert_eq!(heuristic(&input(), &q), "feat-cdc");
    }

    #[test]
    fn the_heuristic_falls_through_branch_prompt_directory() {
        let q = quest("old-name");
        let mut i = input();
        for generic in GENERIC_BRANCHES {
            i.branch = Some(generic.to_string());
            assert_eq!(heuristic(&i, &q), "fix-the-backfill");
        }
        i.branch = None;
        assert_eq!(heuristic(&i, &q), "fix-the-backfill");
        i.first_prompt = None;
        assert_eq!(heuristic(&i, &q), "some-repo");
        i.cwd = "/".to_string();
        assert_eq!(heuristic(&i, &q), "old-name");
    }

    #[test]
    fn a_heuristic_prompt_slug_keeps_only_the_first_few_words() {
        let q = quest("old-name");
        let i = Input {
            branch: None,
            first_prompt: Some(
                "please retry the CDC backfill for the largest accounts, carefully".to_string(),
            ),
            ..input()
        };
        assert_eq!(heuristic(&i, &q), "please-retry-the-cdc");
    }

    #[test]
    fn every_heuristic_answer_is_a_valid_slug() {
        let q = quest("old-name");
        for prompt in ["ČĆŽ!!!", "a".repeat(200).as_str(), "-- --", "1"] {
            let i = Input {
                branch: None,
                first_prompt: Some(prompt.to_string()),
                ..input()
            };
            let slug = heuristic(&i, &q);
            assert!(
                slug.len() <= SLUG_MAX && is_slug(&slug),
                "`{prompt}` produced `{slug}`"
            );
        }
    }

    #[test]
    fn first_words_is_word_bounded() {
        assert_eq!(first_words("a-b-c-d-e", 3), "a-b-c");
        assert_eq!(first_words("a-b", 3), "a-b");
        assert_eq!(first_words("", 3), "");
    }

    #[test]
    fn a_proposal_reads_with_its_provenance() {
        let claude = Proposal {
            slug: "cdc-backfill".to_string(),
            source: NameOrigin::Claude,
            cached: false,
            input_hash: "h".to_string(),
        };
        assert_eq!(claude.describe(), "cdc-backfill");
        let heuristic = Proposal {
            source: NameOrigin::Heuristic,
            ..claude
        };
        assert_eq!(heuristic.describe(), "cdc-backfill (heuristic)");
    }

    struct Canned(&'static str);

    impl Namer for Canned {
        fn suggest(&self, _model: &str, _prompt: &str) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    fn seeded() -> (Db, Quest) {
        let db = Db::open_in_memory().unwrap();
        let quest = db.insert_quest(&quest("old-name")).unwrap();
        (db, quest)
    }

    /// Round-2 review, blocking #1: the `Stop` hook used to force the row it was
    /// handed to `idle` before the gate, so a session the database says is
    /// `waiting` (a `Notification` won the race, or `stop`'s write was dropped)
    /// had `/rename` typed into its permission prompt.
    #[test]
    fn a_held_rename_waits_for_the_stored_row_to_say_idle() {
        let (db, quest) = seeded();
        let mut session = Session::new(&quest.id, SessionRole::Master, "master", "q-old", "%7");
        session.status = SessionStatus::Busy;
        let session = db.insert_session(&session).unwrap();
        db.set_pending_rename(&session.id, Some("old-name/master"))
            .unwrap();

        // Nothing owed until the row itself is idle — the handed row says so.
        let mut stale = session.clone();
        stale.status = SessionStatus::Idle;
        assert!(owed(&db, &quest, &stale).is_none());
        db.update_session_status(
            &session.id,
            SessionStatus::Waiting,
            Some("permission_prompt"),
        )
        .unwrap();
        assert!(owed(&db, &quest, &stale).is_none());

        // Idle in the database, and the name that goes out is the Quest's
        // current one rather than the parked text.
        db.update_session_status(&session.id, SessionStatus::Idle, None)
            .unwrap();
        let mut renamed = quest.clone();
        renamed.slug = "new-name".to_string();
        let (row, desired) = owed(&db, &renamed, &stale).unwrap();
        assert_eq!(desired, "new-name/master");
        assert_eq!(row.pending_rename.as_deref(), Some("old-name/master"));

        // And nothing is owed once the send has happened.
        db.set_pending_rename(&session.id, None).unwrap();
        assert!(owed(&db, &renamed, &stale).is_none());
    }

    #[test]
    fn a_valid_model_answer_is_taken_and_cached() {
        let (db, quest) = seeded();
        let input = input();
        let out = propose(&db, &quest, &input, "haiku", &Canned("cdc-backfill"), false).unwrap();
        assert_eq!(out.slug, "cdc-backfill");
        assert_eq!(out.source, NameOrigin::Claude);
        assert!(!out.cached);
        assert_eq!(
            db.name_cache_get(&input.hash()).unwrap().unwrap().slug,
            "cdc-backfill"
        );

        // A second pass answers from the cache without asking again.
        let again = propose(&db, &quest, &input, "haiku", &NoNamer, false).unwrap();
        assert_eq!(again.slug, "cdc-backfill");
        assert!(again.cached);
        assert_eq!(again.source, NameOrigin::Claude);
    }

    #[test]
    fn an_invalid_model_answer_falls_back_and_is_not_cached() {
        let (db, quest) = seeded();
        let input = input();
        let out = propose(
            &db,
            &quest,
            &input,
            "haiku",
            &Canned("Sure! How about `cdc backfill`?"),
            false,
        )
        .unwrap();
        assert_eq!(out.slug, "feat-cdc");
        assert_eq!(out.source, NameOrigin::Heuristic);
        assert!(!out.cached);
        assert!(db.name_cache_get(&input.hash()).unwrap().is_none());
    }

    #[test]
    fn an_unavailable_model_falls_back_and_is_not_cached() {
        let (db, quest) = seeded();
        let input = input();
        let out = propose(&db, &quest, &input, "haiku", &NoNamer, false).unwrap();
        assert_eq!(out.source, NameOrigin::Heuristic);
        assert!(db.name_cache_get(&input.hash()).unwrap().is_none());
    }

    #[test]
    fn refresh_ignores_the_cache_and_overwrites_it() {
        let (db, quest) = seeded();
        let input = input();
        propose(&db, &quest, &input, "haiku", &Canned("first-try"), false).unwrap();
        let out = propose(&db, &quest, &input, "haiku", &Canned("second-try"), true).unwrap();
        assert_eq!(out.slug, "second-try");
        assert!(!out.cached);
        assert_eq!(
            db.name_cache_get(&input.hash()).unwrap().unwrap().slug,
            "second-try"
        );
    }

    #[test]
    fn a_refresh_that_fails_leaves_the_cached_answer_alone() {
        let (db, quest) = seeded();
        let input = input();
        propose(&db, &quest, &input, "haiku", &Canned("first-try"), false).unwrap();
        let out = propose(&db, &quest, &input, "haiku", &NoNamer, true).unwrap();
        assert_eq!(out.source, NameOrigin::Heuristic);
        assert_eq!(
            db.name_cache_get(&input.hash()).unwrap().unwrap().slug,
            "first-try"
        );
    }

    /// The registry can only vouch for a session it can identify, and during a
    /// rename the row already carries the new slug while Claude still answers
    /// to the old one (round-1 review, medium #4).
    #[test]
    fn the_name_claude_answers_to_is_the_one_it_was_last_told() {
        let mut session = Session::new("q-0001", SessionRole::Master, "master", "q-new", "%1");
        assert_eq!(current_name(&session, "old-name"), "old-name/master");
        session.claude_name = Some("older-still/master".to_string());
        assert_eq!(current_name(&session, "old-name"), "older-still/master");
    }

    #[test]
    fn the_master_prompt_is_the_oldest_masters() {
        let (db, quest) = seeded();
        let mut first = Session::new(&quest.id, SessionRole::Master, "master", "q-old", "%1");
        first.started_at = 100;
        first.first_prompt = Some("the original ask".to_string());
        db.insert_session(&first).unwrap();
        let mut resumed = Session::new(&quest.id, SessionRole::Master, "master", "q-old", "%2");
        resumed.started_at = 200;
        resumed.first_prompt = Some("carry on".to_string());
        db.insert_session(&resumed).unwrap();
        let mut worker = Session::new(&quest.id, SessionRole::Worker, "tests", "q-old", "%3");
        worker.started_at = 50;
        worker.first_prompt = Some("write tests".to_string());
        db.insert_session(&worker).unwrap();

        assert_eq!(
            master_prompt(&db, &quest).as_deref(),
            Some("the original ask")
        );
    }
}
