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
//! The generated answer is cached by a hash of that input, so the same state
//! never pays for a second model call; a rejected answer is *not* cached. The
//! same hash drives regeneration: the master's `Stop` hook compares it against
//! `quest.name_input_hash` and, when they differ, spawns
//! `q name <quest> --auto --apply --detach` in the background. No periodic job.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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
/// `foo-2` … `foo-9` when the proposal is already another Quest's slug.
const SLUG_ATTEMPTS: u32 = 9;

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
        _ => Box::new(ClaudeNamer),
    }
}

struct ClaudeNamer;

impl Namer for ClaudeNamer {
    fn suggest(&self, model: &str, prompt: &str) -> Option<String> {
        run_capped(
            Command::new("claude").args(["-p", "--model", model]),
            prompt.as_bytes(),
            CLAUDE_TIMEOUT,
        )
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

/// A model answer reduced to a slug, or `None` when it is not one. Only the
/// first non-empty line is considered, stripped of the punctuation a chatty
/// model wraps a token in; whatever is left has to be a valid slug on its own.
fn sanitize(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty())?;
    let slug = line.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '.' | ',' | ':' | ';' | '*' | '#')
    });
    (slug.len() <= SLUG_MAX && is_slug(slug)).then(|| slug.to_string())
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

/// A slug no *other* Quest holds: the proposal itself, then `-2` … `-9`.
/// `None` when the Quest already carries it — there is nothing to rename. If
/// every variant is taken the base comes back anyway, so the rename fails
/// loudly rather than picking something arbitrary.
pub fn free_slug(db: &Db, quest: &Quest, base: &str) -> anyhow::Result<Option<String>> {
    for n in 1..=SLUG_ATTEMPTS {
        let candidate = if n == 1 {
            base.to_string()
        } else {
            crate::commands::new::numbered(base, n)
        };
        if candidate == quest.slug {
            return Ok(None);
        }
        if db.get_quest_by_slug(&candidate)?.is_none() {
            return Ok(Some(candidate));
        }
    }
    Ok(Some(base.to_string()))
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
pub fn sync_claude_names(
    db: &Db,
    tmux: &dyn crate::tmux::Tmux,
    quest: &Quest,
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
        if crate::commands::target::refusal(&session, pane_pid, None).is_none()
            && send_rename(tmux, &session.tmux_pane, &desired).is_ok()
        {
            db.set_pending_rename(&session.id, None)?;
            out.told.push(session.label.clone());
        } else {
            db.set_pending_rename(&session.id, Some(&desired))?;
            out.pending.push(session.label.clone());
        }
    }
    Ok(out)
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
    if session.pending_rename.is_none() {
        return;
    }
    // The Quest may have been renamed again while the send was held, so the
    // name that goes out is the current one, not the parked text.
    let desired = claude_name(&quest.slug, &session.label);
    let tmux = crate::tmux::tmux();
    // `Stop` means the turn is over, so the row is idle whatever it still says;
    // the registry is the second opinion.
    let pane_pid = pane_pid(&tmux.list_panes().unwrap_or_default(), &session.tmux_pane);
    let mut idle = session.clone();
    idle.status = SessionStatus::Idle;
    if crate::commands::target::refusal(&idle, pane_pid, None).is_some() {
        return;
    }
    if send_rename(tmux.as_ref(), &session.tmux_pane, &desired).is_err() {
        return;
    }
    let _ = db.transaction(|db| {
        db.set_pending_rename(&session.id, None)?;
        db.append_event(
            &quest.id,
            Some(&session.id),
            "name.synced",
            &serde_json::json!({ "name": desired }),
        )?;
        Ok(())
    });
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
    spawn_detached(&[
        "name".to_string(),
        quest.id.clone(),
        "--auto".to_string(),
        "--apply".to_string(),
        "--detach".to_string(),
    ])
}

/// Runs this binary again, fully detached: no stdio, no wait, so the caller
/// (a hook, or `q name --detach`) returns immediately.
///
/// `$Q_NO_DETACH` replaces the spawn with a JSON line appended to the file it
/// names, so tests can assert on the argv a hook *would* have run without ever
/// starting a process.
pub fn spawn_detached(args: &[String]) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    if let Some(path) = std::env::var_os("Q_NO_DETACH").filter(|v| !v.is_empty()) {
        return record_argv(&PathBuf::from(path), &exe, args);
    }
    Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
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

/// Runs `cmd` with `input` on stdin and at most `timeout` to answer; `None` on
/// a spawn failure, a non-zero exit or a timeout. Both pipes are drained on
/// their own threads so a chatty child cannot deadlock the wait.
///
/// TODO: fold into `src/proc.rs` once #19/#20 land it on main.
fn run_capped(cmd: &mut Command, input: &[u8], timeout: Duration) -> Option<String> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let input = input.to_vec();
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let reader = child.stdout.take().map(|mut stdout| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        })
    });

    let deadline = Instant::now() + timeout;
    let mut status = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            // Timed out, or unwaitable: either way, stop it.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    if !status.is_some_and(|s| s.success()) {
        return None;
    }
    let out = reader?.join().ok()?;
    Some(String::from_utf8_lossy(&out).to_string())
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
        assert_eq!(sanitize("\n\nq1"), Some("q1".to_string()));
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

    #[test]
    fn a_free_slug_steps_aside_for_another_quest() {
        let (db, quest) = seeded();
        assert_eq!(
            free_slug(&db, &quest, "cdc-backfill").unwrap(),
            Some("cdc-backfill".to_string())
        );
        // The Quest's own slug means there is nothing to do.
        assert_eq!(free_slug(&db, &quest, "old-name").unwrap(), None);

        db.insert_quest(&Quest::new("cdc-backfill", "/tmp", "laptop"))
            .unwrap();
        assert_eq!(
            free_slug(&db, &quest, "cdc-backfill").unwrap(),
            Some("cdc-backfill-2".to_string())
        );
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
