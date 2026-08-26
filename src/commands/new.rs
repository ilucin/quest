//! `q new` — creates the Quest row, its tmux session with the `master` window,
//! and launches Claude in it (SPEC §5, §6).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Ctx;
use crate::beads;
use crate::commands::{AttachMode, attach_mode, sweep_quiet};
use crate::db::quest::QuestPatch;
use crate::db::{Db, ID_ATTEMPTS};
use crate::error::QError;
use crate::model::{NameSource, Quest, Session, SessionRole, SessionStatus, new_id};
use crate::output;
use crate::tmux::{NewSession, config_override, db_override, quest_env, session_name};

const SLUG_MAX: usize = 40;
/// `foo`, `foo-2` … `foo-99` — how far an auto slug will step aside.
const SLUG_ATTEMPTS: u32 = 99;
const SLUG_RULE: &str = "must match ^[a-z0-9]+(-[a-z0-9]+)*$ and be at most 40 characters";
pub const MASTER: &str = "master";

/// Branch names that say nothing about the work, so they never become a slug.
const GENERIC_BRANCHES: [&str; 4] = ["main", "master", "develop", "HEAD"];

#[derive(Debug, Default)]
pub struct Args<'a> {
    pub name: Option<&'a str>,
    pub goal: Option<&'a str>,
    pub dir: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub repo: Option<&'a str>,
    pub no_beads: bool,
    pub prompt: Option<&'a str>,
    pub prompt_file: Option<&'a str>,
    pub no_auto_reset: bool,
    pub detach: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    // Both before anything is written: a bad label or a contradictory pair of
    // flags is the user's typo, not a half-created Quest.
    let repo = repo_flag(args)?;
    let cwd = resolve_dir(args.dir)?;
    let prompt = resolve_prompt(args.prompt, args.prompt_file)?;
    let (base, name_source) = resolve_slug(args.name, &cwd)?;
    let (slug, tmux_session) = claim_slug(ctx, db, &base, name_source)?;

    // TODO(M2): brain session (`--brain`).
    let mut row = Quest::new(&slug, &cwd.to_string_lossy(), ctx.machine());
    row.name_source = name_source;
    row.goal = args.goal.map(str::to_string);
    // TODO(M5): validate `--workflow` against the workflow registry.
    row.workflow = args.workflow.map(str::to_string);
    // Only the opt-out is stored; NULL keeps following `[context] auto_reset`.
    row.auto_reset = args.no_auto_reset.then_some(false);
    let quest = db.insert_quest(&row)?;
    db.append_event(
        &quest.id,
        None,
        "quest.created",
        &serde_json::json!({
            "slug": quest.slug,
            "goal": quest.goal,
            "cwd": quest.cwd,
            "machine": quest.machine,
            "workflow": quest.workflow,
            "name_source": quest.name_source,
            "auto_reset": quest.auto_reset,
        }),
    )?;

    // The epic goes in before the master starts, so the brief its SessionStart
    // hook injects already names it. A failing `bd` is a warning, never a
    // failed `q new` (SPEC §13).
    let quest = create_epic(ctx, quest, args, repo.as_deref());

    let master = match spawn_master(ctx, &quest, prompt) {
        Ok(master) => master,
        // Nothing was started, so the Quest row would only be an orphan — and
        // so would the epic, which lives in a tracker this row was the only
        // pointer to.
        Err(e) => {
            abandon_epic(beads::client().as_ref(), &quest);
            let _ = db.delete_quest(&quest.id);
            return Err(e);
        }
    };
    let session = master.session;

    let attach = attach_mode(ctx, !args.detach);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": session,
                "tmux_session": tmux_session,
                "attach": attach,
            }),
            || {
                format!(
                    "created quest {} ({}) · tmux {tmux_session} · run: q enter {}",
                    quest.id, quest.slug, quest.slug
                )
            },
        )?;
    }
    if attach != AttachMode::None {
        // An exec attach replaces this process, so nothing buffered survives it.
        std::io::stdout().flush()?;
        ctx.tmux().attach(&tmux_session, Some(&session.tmux_pane))?;
    }
    Ok(())
}

/// The Quest row is about to be deleted, so its epic loses its only pointer:
/// close it rather than leave a stray open epic in a shared tracker. A `bd`
/// that will not cooperate is named, so the id is not simply lost.
fn abandon_epic(bd: &dyn beads::Bd, quest: &Quest) {
    let Some(epic) = beads::epic_of(quest) else {
        return;
    };
    if let Err(err) = bd.close(epic, "quest creation failed") {
        eprintln!(
            "warning: quest creation failed and beads epic {epic} could not be closed \
             ({err}); close it with `bd close {epic}`"
        );
    }
}

/// `--repo` as a label, once: it is rejected outright when it cannot be one,
/// and refused as a contradiction alongside `--no-beads` (there is no epic for
/// it to label).
fn repo_flag(args: &Args) -> anyhow::Result<Option<String>> {
    let Some(repo) = args.repo else {
        return Ok(None);
    };
    if args.no_beads {
        return Err(QError::Invalid(
            "--repo labels the beads epic, which --no-beads skips; drop one of them".to_string(),
        )
        .into());
    }
    Ok(Some(beads::validate_repo_label(repo)?))
}

/// Creates the Quest's beads epic and stores it on the row. Returns the Quest
/// unchanged when `--no-beads` was given or `bd` could not be reached — the
/// warning goes to stderr so `--json` stdout stays a single payload.
fn create_epic(ctx: &Ctx, quest: Quest, args: &Args, repo: Option<&str>) -> Quest {
    if args.no_beads {
        return quest;
    }
    let repo = beads::repo_label(&ctx.config, repo, Path::new(&quest.cwd));
    let labels = format!("repo:{repo},quest:{}", quest.id);
    let title = match quest
        .goal
        .as_deref()
        .map(str::trim)
        .filter(|g| !g.is_empty())
    {
        Some(goal) => format!("{}: {goal}", quest.slug),
        None => quest.slug.clone(),
    };
    match beads::client().create_epic(&title, &labels, &quest.id) {
        Ok(epic) => store_epic(ctx, quest, &epic, &repo),
        Err(e) => {
            eprintln!(
                "warning: no beads epic for {} ({e}); link one later with \
                 `q set {} beads_epic <id>`, or pass --no-beads to skip this",
                quest.slug, quest.slug
            );
            quest
        }
    }
}

/// A stored epic the database then refuses is still a real epic, so the
/// database error is reported and the Quest carries on without the column.
fn store_epic(ctx: &Ctx, quest: Quest, epic: &str, repo: &str) -> Quest {
    let patch = QuestPatch {
        beads_epic: Some(Some(epic.to_string())),
        beads_repo: Some(Some(repo.to_string())),
        ..QuestPatch::default()
    };
    let stored = ctx.db().and_then(|db| {
        let stored = db.update_quest(&quest.id, &patch)?;
        db.append_event(
            &quest.id,
            None,
            "beads.epic",
            &serde_json::json!({ "epic": epic, "repo": repo }),
        )?;
        Ok(stored)
    });
    match stored {
        Ok(stored) => stored,
        Err(e) => {
            eprintln!("warning: beads epic {epic} could not be stored: {e:#}");
            quest
        }
    }
}

/// The first free slug and the tmux session that goes with it. An auto slug
/// steps aside (`-2`, `-3`, …); an explicit `--name` is a hard error instead.
fn claim_slug(
    ctx: &Ctx,
    db: &Db,
    base: &str,
    source: NameSource,
) -> anyhow::Result<(String, String)> {
    let auto = source == NameSource::Auto;
    for n in 1..=SLUG_ATTEMPTS {
        let slug = if n == 1 {
            base.to_string()
        } else {
            numbered(base, n)
        };
        let tmux_session = session_name(&ctx.config, &slug);
        if let Some(existing) = db.get_quest_by_slug(&slug)? {
            if auto {
                continue;
            }
            return Err(QError::Conflict(format!(
                "slug `{slug}` is already taken by quest {}; pick another with --name",
                existing.id
            ))
            .into());
        }
        if ctx.tmux().has_session(&tmux_session)? {
            if auto {
                continue;
            }
            return Err(QError::Conflict(format!(
                "tmux session `{tmux_session}` already exists; kill it or pick another slug with --name"
            ))
            .into());
        }
        return Ok((slug, tmux_session));
    }
    Err(QError::Conflict(format!(
        "`{base}` and its first {SLUG_ATTEMPTS} variants are all taken; pick a slug with --name"
    ))
    .into())
}

/// `base-<n>`, kept within `SLUG_MAX` by trimming the base.
fn numbered(base: &str, n: u32) -> String {
    let suffix = format!("-{n}");
    let mut head = base.to_string();
    if head.len() + suffix.len() > SLUG_MAX {
        head.truncate(SLUG_MAX - suffix.len());
    }
    format!("{}{suffix}", head.trim_end_matches('-'))
}

/// The `master` window of `q-<slug>`, and the session row recording it.
pub struct Master {
    pub session: Session,
    pub tmux_session: String,
}

/// Creates the Quest's tmux session with `master` in window 0, starts Claude
/// there and records the session row (SPEC §5, §6). Shared by `q new` and
/// `q resume`; the caller owns whatever else has to be undone on failure.
pub fn spawn_master(ctx: &Ctx, quest: &Quest, prompt: Option<String>) -> anyhow::Result<Master> {
    let db = ctx.db()?;
    let tmux_session = session_name(&ctx.config, &quest.slug);
    // The session id goes into the window's environment, so it has to exist
    // before the pane it will be stored against.
    let session_id = fresh_session_id(db)?;
    let spec = NewSession {
        name: tmux_session.clone(),
        window_name: MASTER.to_string(),
        cwd: quest.cwd.clone(),
        env: quest_env(
            &quest.id,
            &session_id,
            SessionRole::Master,
            &quest.machine,
            db_override().as_deref(),
            config_override().as_deref(),
        ),
        command: Some(claude_command(&quest.slug, MASTER, prompt.as_deref())),
    };
    let pane = ctx.tmux().new_session(&spec)?;

    let mut row = Session::new(
        &quest.id,
        SessionRole::Master,
        MASTER,
        &tmux_session,
        &pane.pane_id,
    );
    row.id = session_id.clone();
    row.status = SessionStatus::Starting;
    row.workflow = quest.workflow.clone();
    row.first_prompt = prompt;
    // `session.start` is the hook's to append once Claude comes up (M1).
    match db.insert_session(&row) {
        // A regenerated id would no longer match `Q_SESSION` in the window.
        Ok(session) if session.id != session_id => {
            let _ = ctx.tmux().kill_session(&tmux_session);
            Err(QError::Db(format!(
                "session id `{session_id}` was taken between allocating and inserting it"
            ))
            .into())
        }
        Ok(session) => Ok(Master {
            session,
            tmux_session,
        }),
        Err(e) => {
            let _ = ctx.tmux().kill_session(&tmux_session);
            Err(e)
        }
    }
}

/// An existing directory, canonicalized. `None` is the current one.
pub fn resolve_dir(dir: Option<&str>) -> anyhow::Result<PathBuf> {
    let raw = match dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()
            .map_err(|e| QError::Other(format!("cannot read the current directory: {e}")))?,
    };
    if !raw.exists() {
        return Err(QError::NotFound(format!("no such directory: {}", raw.display())).into());
    }
    if !raw.is_dir() {
        return Err(QError::Invalid(format!("not a directory: {}", raw.display())).into());
    }
    raw.canonicalize()
        .map_err(|e| QError::Other(format!("cannot resolve {}: {e}", raw.display())).into())
}

/// `--prompt`, or `--prompt-file <path>`; `-` reads stdin. Blank is no prompt.
fn resolve_prompt(prompt: Option<&str>, file: Option<&str>) -> anyhow::Result<Option<String>> {
    let text = match (prompt, file) {
        (Some(text), _) => text.to_string(),
        (None, Some("-")) => std::io::read_to_string(std::io::stdin())
            .map_err(|e| QError::Other(format!("cannot read the prompt from stdin: {e}")))?,
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|e| QError::Invalid(format!("cannot read {path}: {e}")))?,
        (None, None) => return Ok(None),
    };
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// `--name` is taken as given (validated); everything else is the M0 heuristic.
fn resolve_slug(name: Option<&str>, cwd: &Path) -> anyhow::Result<(String, NameSource)> {
    match name {
        Some(name) => {
            validate_slug(name)?;
            Ok((name.to_string(), NameSource::Manual))
        }
        // TODO(M2): auto-naming via `claude -p --model haiku`, with this as the
        // fallback.
        None => Ok((
            heuristic_slug(git_branch(cwd).as_deref(), cwd),
            NameSource::Auto,
        )),
    }
}

pub fn validate_slug(slug: &str) -> anyhow::Result<()> {
    validate_kebab("slug", slug)
}

/// A session label follows the slug grammar — it becomes part of a tmux window
/// name and of `claude -n <slug>/<label>` (SPEC §6).
pub fn validate_label(label: &str) -> anyhow::Result<()> {
    validate_kebab("label", label)
}

fn validate_kebab(what: &str, value: &str) -> anyhow::Result<()> {
    if value.len() > SLUG_MAX || !is_slug(value) {
        return Err(QError::Invalid(format!("invalid {what} `{value}`: it {SLUG_RULE}")).into());
    }
    Ok(())
}

fn is_slug(s: &str) -> bool {
    !s.is_empty()
        && s.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() && !c.is_ascii_uppercase())
        })
}

/// Branch first — it usually names the work; then the directory; then the id.
fn heuristic_slug(branch: Option<&str>, cwd: &Path) -> String {
    let from_branch = branch
        .filter(|b| !GENERIC_BRANCHES.contains(b))
        .map(slugify)
        .filter(|s| !s.is_empty());
    if let Some(slug) = from_branch {
        return slug;
    }
    let from_dir = cwd
        .file_name()
        .map(|n| slugify(&n.to_string_lossy()))
        .filter(|s| !s.is_empty());
    from_dir.unwrap_or_else(|| new_id("quest"))
}

/// Lowercased, every other run of characters collapsed to one `-`.
fn slugify(raw: &str) -> String {
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.truncate(SLUG_MAX);
    out.trim_matches('-').to_string()
}

fn git_branch(cwd: &Path) -> Option<String> {
    let out = Command::new("git")
        .args([
            "-C",
            &cwd.to_string_lossy(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let branch = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// `claude -n <slug>/<label> [-- <prompt>]`, run by tmux through a shell.
pub fn claude_command(slug: &str, label: &str, prompt: Option<&str>) -> String {
    let mut cmd = format!("claude -n {}", shell_quote(&format!("{slug}/{label}")));
    if let Some(prompt) = prompt {
        cmd.push_str(" -- ");
        cmd.push_str(&shell_quote(prompt));
    }
    cmd
}

/// Single-quoted unless the word is plainly safe; `'` is closed, escaped and
/// reopened, which is the only way out of single quotes in sh.
fn shell_quote(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !word.is_empty() && word.chars().all(safe) {
        return word.to_string();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

pub fn fresh_session_id(db: &Db) -> anyhow::Result<String> {
    for _ in 0..ID_ATTEMPTS {
        let id = new_id("s");
        if db.get_session(&id)?.is_none() {
            return Ok(id);
        }
    }
    Err(QError::Db("cannot allocate a session id".to_string()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_validation_follows_the_spec_grammar() {
        for good in [
            "a",
            "q1",
            "cdc-backfill-retry",
            "a-1-b",
            &"x".repeat(SLUG_MAX),
        ] {
            assert!(validate_slug(good).is_ok(), "rejected `{good}`");
        }
        for bad in [
            "",
            "-lead",
            "trail-",
            "double--dash",
            "Upper",
            "with space",
            "under_score",
            "sla/sh",
            &"x".repeat(SLUG_MAX + 1),
        ] {
            let e = validate_slug(bad).unwrap_err();
            assert!(format!("{e}").contains("invalid slug"), "accepted `{bad}`");
        }
    }

    #[test]
    fn heuristic_prefers_a_meaningful_branch() {
        let dir = Path::new("/tmp/some-repo");
        assert_eq!(
            heuristic_slug(Some("feat/CDC-backfill"), dir),
            "feat-cdc-backfill"
        );
        assert_eq!(heuristic_slug(Some("main"), dir), "some-repo");
        assert_eq!(heuristic_slug(Some("HEAD"), dir), "some-repo");
        assert_eq!(heuristic_slug(None, dir), "some-repo");
        assert_eq!(heuristic_slug(Some("///"), dir), "some-repo");
    }

    #[test]
    fn heuristic_falls_back_to_a_generated_slug() {
        let slug = heuristic_slug(None, Path::new("/"));
        assert!(slug.starts_with("quest-"), "{slug}");
        assert!(validate_slug(&slug).is_ok(), "{slug}");
    }

    #[test]
    fn heuristic_output_is_always_a_valid_slug() {
        for branch in ["Feature/ABC 123", "a--b", "-lead-", "x".repeat(60).as_str()] {
            let slug = heuristic_slug(Some(branch), Path::new("/tmp/repo"));
            assert!(validate_slug(&slug).is_ok(), "`{branch}` produced `{slug}`");
        }
    }

    #[test]
    fn numbered_slugs_stay_valid_and_within_the_limit() {
        assert_eq!(numbered("foo", 2), "foo-2");
        assert_eq!(numbered("foo", 99), "foo-99");
        let long = "x".repeat(SLUG_MAX);
        let slug = numbered(&long, 12);
        assert_eq!(slug.len(), SLUG_MAX);
        assert!(validate_slug(&slug).is_ok(), "{slug}");
        // Trimming must not leave a dangling separator behind.
        let dashed = format!("{}-a", "y".repeat(SLUG_MAX - 2));
        assert!(validate_slug(&numbered(&dashed, 7)).is_ok());
    }

    #[test]
    fn claude_command_without_a_prompt_is_bare() {
        assert_eq!(
            claude_command("foo", "master", None),
            "claude -n foo/master"
        );
    }

    #[test]
    fn claude_command_quotes_the_prompt() {
        assert_eq!(
            claude_command("foo", "master", Some("fix the bug")),
            "claude -n foo/master -- 'fix the bug'"
        );
        assert_eq!(
            claude_command("foo", "w1-tests", Some("it's \"broken\"; rm -rf $HOME")),
            r#"claude -n foo/w1-tests -- 'it'\''s "broken"; rm -rf $HOME'"#
        );
        assert_eq!(
            claude_command("foo", "master", Some("two\nlines")),
            "claude -n foo/master -- 'two\nlines'"
        );
    }

    #[test]
    fn shell_quote_leaves_safe_words_alone() {
        assert_eq!(shell_quote("foo/master"), "foo/master");
        assert_eq!(shell_quote("a-b_c.d:e,f=g+h%i@j"), "a-b_c.d:e,f=g+h%i@j");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("`whoami`"), "'`whoami`'");
    }

    #[test]
    fn prompt_sources_are_trimmed_and_optional() {
        assert_eq!(resolve_prompt(None, None).unwrap(), None);
        assert_eq!(
            resolve_prompt(Some("  hi  "), None).unwrap(),
            Some("hi".to_string())
        );
        assert_eq!(resolve_prompt(Some("   "), None).unwrap(), None);
    }

    /// Records the `bd close` calls a rollback makes.
    #[derive(Default)]
    struct SpyBd {
        closed: std::cell::RefCell<Vec<(String, String)>>,
        refuse: bool,
    }

    impl beads::Bd for SpyBd {
        fn create_epic(&self, _: &str, _: &str, _: &str) -> Result<String, String> {
            unreachable!("a rollback never creates")
        }
        fn list_quest(&self, _: &str) -> Option<String> {
            None
        }
        fn list_quests(&self, _: &[&str]) -> Option<String> {
            None
        }
        fn close(&self, id: &str, reason: &str) -> Result<(), String> {
            self.closed
                .borrow_mut()
                .push((id.to_string(), reason.to_string()));
            if self.refuse {
                Err("bd is wedged".to_string())
            } else {
                Ok(())
            }
        }
        fn relabel_repo(&self, _: &str, _: Option<&str>, _: &str) -> Result<(), String> {
            unreachable!("a rollback never relabels")
        }
    }

    fn quest_with_epic(epic: Option<&str>) -> Quest {
        let mut quest = Quest::new("slug", "/tmp", "machine");
        quest.beads_epic = epic.map(str::to_string);
        quest
    }

    #[test]
    fn a_rolled_back_quest_closes_the_epic_it_had_already_minted() {
        let bd = SpyBd::default();
        abandon_epic(&bd, &quest_with_epic(Some("bd-7fx")));
        assert_eq!(
            bd.closed.borrow().as_slice(),
            [("bd-7fx".to_string(), "quest creation failed".to_string())]
        );
    }

    #[test]
    fn a_rollback_with_no_epic_asks_bd_for_nothing() {
        let bd = SpyBd::default();
        abandon_epic(&bd, &quest_with_epic(None));
        assert!(bd.closed.borrow().is_empty());
    }

    #[test]
    fn a_bd_that_refuses_the_rollback_is_survivable() {
        let bd = SpyBd {
            refuse: true,
            ..SpyBd::default()
        };
        abandon_epic(&bd, &quest_with_epic(Some("bd-7fx")));
        assert_eq!(bd.closed.borrow().len(), 1);
    }

    #[test]
    fn the_repo_flag_is_validated_and_refused_alongside_no_beads() {
        let args = Args {
            repo: Some("  quest "),
            ..Args::default()
        };
        assert_eq!(repo_flag(&args).unwrap(), Some("quest".to_string()));
        assert!(repo_flag(&Args::default()).unwrap().is_none());

        let bad = Args {
            repo: Some("evil,repo:other"),
            ..Args::default()
        };
        assert!(repo_flag(&bad).is_err());

        let contradictory = Args {
            repo: Some("quest"),
            no_beads: true,
            ..Args::default()
        };
        let err = repo_flag(&contradictory).unwrap_err().to_string();
        assert!(err.contains("--no-beads"), "{err}");
    }
}
