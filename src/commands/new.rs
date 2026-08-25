//! `q new` — creates the Quest row, its tmux session with the `master` window,
//! and launches Claude in it (SPEC §5, §6).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Ctx;
use crate::db::{Db, ID_ATTEMPTS};
use crate::error::QError;
use crate::model::{NameSource, Quest, Session, SessionRole, SessionStatus, new_id};
use crate::output;
use crate::tmux::{NewSession, config_override, db_override, quest_env, session_name};

const SLUG_MAX: usize = 40;
const SLUG_RULE: &str = "must match ^[a-z0-9]+(-[a-z0-9]+)*$ and be at most 40 characters";
const MASTER: &str = "master";

/// Branch names that say nothing about the work, so they never become a slug.
const GENERIC_BRANCHES: [&str; 4] = ["main", "master", "develop", "HEAD"];

#[derive(Debug, Default)]
pub struct Args<'a> {
    pub name: Option<&'a str>,
    pub goal: Option<&'a str>,
    pub dir: Option<&'a str>,
    pub workflow: Option<&'a str>,
    pub prompt: Option<&'a str>,
    pub prompt_file: Option<&'a str>,
    pub detach: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let cwd = resolve_dir(args.dir)?;
    let prompt = resolve_prompt(args.prompt, args.prompt_file)?;
    let (slug, name_source) = resolve_slug(args.name, &cwd)?;

    if let Some(existing) = db.get_quest_by_slug(&slug)? {
        return Err(QError::Other(format!(
            "slug `{slug}` is already taken by quest {}; pick another with --name",
            existing.id
        ))
        .into());
    }
    let tmux_session = session_name(&ctx.config, &slug);
    if ctx.tmux().has_session(&tmux_session)? {
        return Err(QError::Tmux(format!(
            "tmux session `{tmux_session}` already exists; kill it or pick another slug with --name"
        ))
        .into());
    }

    // TODO(M2): beads epic and brain session (`--repo`, `--brain`, `--no-beads`).
    let mut row = Quest::new(&slug, &cwd.to_string_lossy(), ctx.machine());
    row.name_source = name_source;
    row.goal = args.goal.map(str::to_string);
    // TODO(M5): validate `--workflow` against the workflow registry.
    row.workflow = args.workflow.map(str::to_string);
    let quest = db.insert_quest(&row)?;
    db.append_event(
        &quest.id,
        None,
        "quest.created",
        &serde_json::json!({
            "slug": quest.slug,
            "cwd": quest.cwd,
            "machine": quest.machine,
            "workflow": quest.workflow,
            "name_source": quest.name_source,
        }),
    )?;

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
    let pane = match ctx.tmux().new_session(&spec) {
        Ok(pane) => pane,
        // Nothing was started, so the Quest row would only be an orphan.
        Err(e) => {
            db.delete_quest(&quest.id)?;
            return Err(e);
        }
    };

    let mut row = Session::new(
        &quest.id,
        SessionRole::Master,
        MASTER,
        &tmux_session,
        &pane.pane_id,
    );
    row.id = session_id;
    row.status = SessionStatus::Starting;
    row.workflow = quest.workflow.clone();
    row.first_prompt = prompt;
    let session = match db.insert_session(&row) {
        Ok(session) => session,
        Err(e) => {
            let _ = ctx.tmux().kill_session(&tmux_session);
            db.delete_quest(&quest.id)?;
            return Err(e);
        }
    };
    // `session.start` is the hook's to append once Claude comes up (M1).

    let attached = !args.detach;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": session,
                "tmux_session": tmux_session,
                "attached": attached,
            }),
            || {
                format!(
                    "created quest {} ({}) · tmux {tmux_session} · run: q enter {}",
                    quest.id, quest.slug, quest.slug
                )
            },
        )?;
    }
    if attached {
        // A real attach replaces this process, so nothing buffered survives it.
        std::io::stdout().flush()?;
        ctx.tmux().attach(&tmux_session, Some(MASTER))?;
    }
    Ok(())
}

fn resolve_dir(dir: Option<&str>) -> anyhow::Result<PathBuf> {
    let raw = match dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()
            .map_err(|e| QError::Other(format!("cannot read the current directory: {e}")))?,
    };
    if !raw.exists() {
        return Err(QError::Other(format!("no such directory: {}", raw.display())).into());
    }
    if !raw.is_dir() {
        return Err(QError::Other(format!("not a directory: {}", raw.display())).into());
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
            .map_err(|e| QError::Other(format!("cannot read {path}: {e}")))?,
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

fn validate_slug(slug: &str) -> anyhow::Result<()> {
    if slug.len() > SLUG_MAX || !is_slug(slug) {
        return Err(QError::Other(format!("invalid slug `{slug}`: it {SLUG_RULE}")).into());
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
fn claude_command(slug: &str, label: &str, prompt: Option<&str>) -> String {
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

fn fresh_session_id(db: &Db) -> anyhow::Result<String> {
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
}
