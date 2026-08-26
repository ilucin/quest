//! `q doctor` (SPEC §19): checks the local environment and, with `--fix`,
//! repairs what it can. Only the M0 check set lives here — the rest arrive
//! with the features they diagnose.
//!
//! Every check is independent and swallows its own errors, so a broken
//! environment still produces a full report instead of a single failure.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Ctx;
use crate::config::Config;
use crate::db::Db;
use crate::model::{Session, now};
use crate::output;
use crate::tmux::{self, Tmux};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn symbol(self) -> char {
        match self {
            Status::Ok => '✓',
            Status::Warn => '⚠',
            Status::Fail => '✗',
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
    pub fix_hint: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub ok: bool,
    pub checks: Vec<Check>,
    /// What `--fix` actually repaired, one line each.
    pub fixed: Vec<String>,
}

impl Report {
    fn new(checks: Vec<Check>, fixed: Vec<String>) -> Report {
        let ok = !checks.iter().any(|c| c.status == Status::Fail);
        Report { ok, checks, fixed }
    }

    /// A warning is not a failure — only `Fail` makes the process exit non-zero.
    pub fn exit_code(&self) -> u8 {
        u8::from(!self.ok)
    }

    fn human(&self) -> String {
        let mut out = String::new();
        for check in &self.checks {
            out.push_str(&format!(
                "{} {} {}\n",
                check.status.symbol(),
                check.name,
                check.detail
            ));
            if let Some(hint) = &check.fix_hint {
                out.push_str(&format!("    → fix: {hint}\n"));
            }
        }
        for line in &self.fixed {
            out.push_str(&format!("fixed: {line}\n"));
        }
        out.trim_end().to_string()
    }
}

fn check(name: &'static str, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name,
        status,
        detail: detail.into(),
        fix_hint: None,
    }
}

fn with_hint(mut c: Check, hint: impl Into<String>) -> Check {
    c.fix_hint = Some(hint.into());
    c
}

// ------------------------------------------------------------------ PATH lookup

/// First executable named `name` on `$PATH`, in `PATH` order — a `which` that
/// does not shell out.
pub fn which(name: &str) -> Option<PathBuf> {
    which_in(name, std::env::var_os("PATH").as_deref())
}

fn which_in(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Best-effort identity: symlinks resolved where possible, the raw path
/// otherwise.
fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `{e:#}` without a leading `<name>: `, so a line does not read
/// "✗ tmux tmux: ..." when the error already names its subsystem.
fn detail(name: &str, e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    text.strip_prefix(&format!("{name}: "))
        .unwrap_or(&text)
        .to_string()
}

// --------------------------------------------------------------------- checks

fn check_config() -> Check {
    let path = match Config::path() {
        Ok(path) => path,
        Err(e) => {
            return with_hint(
                check("config", Status::Fail, detail("config", &e)),
                "set Q_CONFIG to a writable path",
            );
        }
    };
    config_check(&path)
}

fn config_check(path: &Path) -> Check {
    match Config::load_from(path) {
        Ok(_) if path.exists() => check("config", Status::Ok, path.display().to_string()),
        Ok(_) => check(
            "config",
            Status::Ok,
            format!("{} (defaults, file missing)", path.display()),
        ),
        Err(e) => with_hint(
            check("config", Status::Fail, detail("config", &e)),
            "q config edit",
        ),
    }
}

/// `q` needs `MIN_TMUX`; an unreadable version is not proof of an old one, so
/// it only warns.
fn evaluate_tmux_version(version: Option<(u32, u32)>) -> Status {
    match version {
        Some(v) if v >= tmux::MIN_TMUX => Status::Ok,
        Some(_) => Status::Fail,
        None => Status::Warn,
    }
}

fn check_tmux(tmux: &dyn Tmux) -> Check {
    let (min_major, min_minor) = tmux::MIN_TMUX;
    let version = match tmux.version() {
        Ok(version) => version,
        Err(e) => {
            return with_hint(
                check("tmux", Status::Fail, detail("tmux", &e)),
                "install tmux (`brew install tmux`)",
            );
        }
    };
    // `tmux -V` answers "tmux 3.6b"; the check is already named "tmux".
    let shown = version.strip_prefix("tmux ").unwrap_or(&version).trim();
    match evaluate_tmux_version(tmux::parse_version(&version)) {
        Status::Ok => check("tmux", Status::Ok, shown),
        Status::Fail => with_hint(
            check(
                "tmux",
                Status::Fail,
                format!("{shown} is older than the required {min_major}.{min_minor}"),
            ),
            format!("upgrade tmux to {min_major}.{min_minor} or newer"),
        ),
        Status::Warn => check(
            "tmux",
            Status::Warn,
            format!("{shown} (unrecognised version)"),
        ),
    }
}

// TODO(M1): also check that `claude` is logged in; running it costs a process
// spawn and a network round trip, so it waits for the milestone that needs it.
fn check_claude() -> Check {
    match which("claude") {
        Some(path) => check("claude", Status::Ok, path.display().to_string()),
        None => with_hint(
            check("claude", Status::Fail, "not found on PATH"),
            "install Claude Code (https://claude.com/claude-code)",
        ),
    }
}

/// The database check doubles as the handle the orphan check needs.
fn check_db() -> (Check, Option<Db>) {
    let path = match Db::path() {
        Ok(path) => path,
        Err(e) => {
            return (
                with_hint(
                    check("db", Status::Fail, detail("db", &e)),
                    "set Q_DB to a writable path",
                ),
                None,
            );
        }
    };
    // Before `Db::open`, which creates the file it is asked about.
    let created = !path.exists();
    match Db::open(&path) {
        Ok(db) => match db.schema_version() {
            Ok(version) => (
                check(
                    "db",
                    Status::Ok,
                    format!(
                        "schema v{version} at {}{}",
                        path.display(),
                        if created { " (created)" } else { "" }
                    ),
                ),
                Some(db),
            ),
            Err(e) => (check("db", Status::Fail, detail("db", &e)), None),
        },
        // A schema from a newer binary reports itself here, with its own hint.
        Err(e) => (check("db", Status::Fail, detail("db", &e)), None),
    }
}

/// SPEC §23.8: `q` is a short name, so say which one a shell would run.
fn check_q_on_path(current: Option<&Path>, found: Option<&Path>) -> Check {
    let Some(found) = found else {
        return with_hint(
            check("q on PATH", Status::Warn, "no `q` on PATH"),
            "add the directory holding q to PATH",
        );
    };
    // The resolved path is the one that answers "which q would run?".
    let shown = real(found).display().to_string();
    match current {
        Some(current) if real(current) != real(found) => with_hint(
            check(
                "q on PATH",
                Status::Warn,
                format!("{shown} shadows this binary ({})", current.display()),
            ),
            "reorder PATH, or remove the other q",
        ),
        _ => check("q on PATH", Status::Ok, shown),
    }
}

fn check_orphans(db: Option<&Db>, tmux: &dyn Tmux, fix: bool, fixed: &mut Vec<String>) -> Check {
    const NAME: &str = "orphan sessions";

    let Some(db) = db else {
        return check(NAME, Status::Warn, "skipped: the database is unreadable");
    };
    // `sweep` does the finding and the ending in one pass, so `--fix` reports
    // exactly what it changed rather than recomputing the set.
    if fix {
        let ended = match tmux::sweep(db, tmux) {
            Ok(ended) => ended,
            Err(e) => return check(NAME, Status::Fail, format!("{e:#}")),
        };
        if ended.is_empty() {
            return check(NAME, Status::Ok, "none");
        }
        let names: Vec<String> = ended.iter().map(|s| describe(db, s)).collect();
        fixed.extend(names.iter().map(|n| format!("ended orphan session {n}")));
        return check(NAME, Status::Ok, format!("ended {}", names.join(", ")));
    }

    let live = match db.list_live_sessions() {
        Ok(live) => live,
        Err(e) => return check(NAME, Status::Fail, format!("{e:#}")),
    };
    // Nothing to be orphaned: answer without asking tmux anything.
    if live.is_empty() {
        return check(NAME, Status::Ok, "none");
    }
    // A dead tmux server means every live session is an orphan; only a missing
    // binary lands here, and that is the tmux check's business.
    let panes = match tmux::live_panes(tmux) {
        Ok(panes) => panes,
        Err(e) => return check(NAME, Status::Warn, format!("skipped: {e:#}")),
    };

    let orphans = tmux::find_orphans(live, &panes, now());
    if orphans.is_empty() {
        return check(NAME, Status::Ok, "none");
    }
    let names: Vec<String> = orphans.iter().map(|o| describe(db, &o.session)).collect();
    with_hint(
        check(
            NAME,
            Status::Warn,
            format!("{} live in the database: {}", names.len(), names.join(", ")),
        ),
        "q doctor --fix",
    )
}

/// `quest-slug/label`, falling back to the quest id when the row is unreadable.
fn describe(db: &Db, session: &Session) -> String {
    let quest = db
        .get_quest(&session.quest_id)
        .ok()
        .flatten()
        .map(|q| q.slug)
        .unwrap_or_else(|| session.quest_id.clone());
    format!("{quest}/{}", session.label)
}

// ----------------------------------------------------------------- entry point

fn report(ctx: &Ctx, fix: bool) -> Report {
    let mut fixed = Vec::new();
    let (db_check, db) = check_db();
    let checks = vec![
        check_config(),
        check_tmux(ctx.tmux()),
        check_claude(),
        db_check,
        check_q_on_path(
            std::env::current_exe().ok().as_deref(),
            which("q").as_deref(),
        ),
        check_orphans(db.as_ref(), ctx.tmux(), fix, &mut fixed),
    ];
    Report::new(checks, fixed)
}

/// Returns the process exit code; a failing check is reported, not an error.
pub fn run(ctx: &Ctx, fix: bool) -> anyhow::Result<u8> {
    let report = report(ctx, fix);
    output::emit(ctx.json, &report, || report.human())?;
    Ok(report.exit_code())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn checks(statuses: &[Status]) -> Report {
        Report::new(
            statuses
                .iter()
                .map(|s| check("x", *s, "detail"))
                .collect::<Vec<_>>(),
            Vec::new(),
        )
    }

    #[test]
    fn only_a_failure_makes_the_exit_code_non_zero() {
        assert_eq!(checks(&[]).exit_code(), 0);
        assert_eq!(checks(&[Status::Ok, Status::Ok]).exit_code(), 0);
        assert_eq!(checks(&[Status::Ok, Status::Warn]).exit_code(), 0);
        assert_eq!(checks(&[Status::Warn, Status::Fail]).exit_code(), 1);
        assert!(!checks(&[Status::Fail]).ok);
        assert!(checks(&[Status::Warn]).ok);
    }

    #[test]
    fn human_output_lists_a_line_per_check_and_indents_hints() {
        let report = Report::new(
            vec![
                check("tmux", Status::Ok, "tmux 3.6b"),
                with_hint(
                    check("claude", Status::Fail, "not found on PATH"),
                    "install",
                ),
            ],
            vec!["ended orphan session a/w1".to_string()],
        );
        assert_eq!(
            report.human(),
            "✓ tmux tmux 3.6b\n✗ claude not found on PATH\n    → fix: install\nfixed: ended orphan session a/w1"
        );
    }

    #[test]
    fn tmux_below_the_minimum_fails_and_an_unreadable_version_only_warns() {
        assert_eq!(evaluate_tmux_version(Some((3, 0))), Status::Fail);
        assert_eq!(evaluate_tmux_version(Some((2, 9))), Status::Fail);
        assert_eq!(evaluate_tmux_version(Some((3, 1))), Status::Fail);
        assert_eq!(evaluate_tmux_version(Some(tmux::MIN_TMUX)), Status::Ok);
        assert_eq!(evaluate_tmux_version(Some((3, 6))), Status::Ok);
        assert_eq!(evaluate_tmux_version(Some((4, 0))), Status::Ok);
        assert_eq!(evaluate_tmux_version(None), Status::Warn);
    }

    #[test]
    fn detail_drops_a_prefix_the_check_name_already_carries() {
        let e: anyhow::Error = crate::error::QError::Tmux("not found on PATH".to_string()).into();
        assert_eq!(detail("tmux", &e), "not found on PATH");
        assert_eq!(detail("db", &e), "tmux: not found on PATH");
    }

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn which_finds_the_first_executable_on_path() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();

        // Only in the second directory: found, and the empty one is skipped.
        executable(&second.path().join("claude"));
        let path = std::env::join_paths([empty.path(), first.path(), second.path()]).unwrap();
        assert_eq!(
            which_in("claude", Some(&path)),
            Some(second.path().join("claude"))
        );

        // Now in both: PATH order wins.
        executable(&first.path().join("claude"));
        assert_eq!(
            which_in("claude", Some(&path)),
            Some(first.path().join("claude"))
        );

        assert_eq!(which_in("nope", Some(&path)), None);
        assert_eq!(which_in("claude", None), None);
        assert_eq!(
            which_in("claude", Some(OsStr::new(""))),
            None,
            "an empty PATH must not fall back to the cwd"
        );
    }

    #[test]
    fn which_ignores_directories_and_non_executable_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("claude")).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(which_in("claude", Some(&path)), None);

        let other = tempfile::tempdir().unwrap();
        fs::write(other.path().join("claude"), "not executable").unwrap();
        let path = std::env::join_paths([other.path()]).unwrap();
        assert_eq!(which_in("claude", Some(&path)), None);
    }

    #[test]
    fn q_on_path_warns_when_missing_or_shadowed() {
        let dir = tempfile::tempdir().unwrap();
        let mine = dir.path().join("target/q");
        let theirs = dir.path().join("usr/q");

        assert_eq!(check_q_on_path(Some(&mine), None).status, Status::Warn);
        assert_eq!(check_q_on_path(Some(&mine), Some(&mine)).status, Status::Ok);
        assert_eq!(check_q_on_path(None, Some(&theirs)).status, Status::Ok);

        let shadowed = check_q_on_path(Some(&mine), Some(&theirs));
        assert_eq!(shadowed.status, Status::Warn);
        assert!(shadowed.detail.contains("shadows"), "{shadowed:?}");
        assert!(shadowed.fix_hint.is_some());
    }

    #[test]
    fn config_check_accepts_a_missing_file_and_fails_a_broken_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let missing = config_check(&path);
        assert_eq!(missing.status, Status::Ok);
        assert!(
            missing.detail.contains("defaults, file missing"),
            "{missing:?}"
        );

        fs::write(&path, "[machine]\nname = \"laptop\"\n").unwrap();
        let present = config_check(&path);
        assert_eq!(present.status, Status::Ok);
        assert!(!present.detail.contains("missing"), "{present:?}");

        fs::write(&path, "[context]\nreset_strategy = \"nuke\"\n").unwrap();
        let broken = config_check(&path);
        assert_eq!(broken.status, Status::Fail);
        assert!(broken.detail.contains("reset_strategy"), "{broken:?}");
        assert!(broken.fix_hint.is_some());
    }
}
