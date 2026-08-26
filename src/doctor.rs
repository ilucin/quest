//! `q doctor` (SPEC §19): checks the local environment and, with `--fix`,
//! repairs what it can. Checks arrive with the features they diagnose; the
//! rest of §19 (remotes, `brain`, `gh`) is still to come.
//!
//! Every check is independent and swallows its own errors, so a broken
//! environment still produces a full report instead of a single failure.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};

use crate::Ctx;
use crate::commands::hook;
use crate::config::Config;
use crate::db::Db;
use crate::model::{Session, now};
use crate::output;
use crate::proc;
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
    pub name: String,
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

fn check(name: impl Into<String>, status: Status, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
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

/// Budget for the two `claude` calls; both read local state, so a slow answer
/// means something is wrong rather than merely busy.
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(5);
/// Budget for the statusline probe. Claude calls the statusline after every
/// message, so anything slower than this is already a problem.
const STATUSLINE_TIMEOUT: Duration = Duration::from_secs(3);

fn check_claude(claude: Option<&Path>) -> Check {
    let Some(path) = claude else {
        return with_hint(
            check("claude", Status::Fail, "not found on PATH"),
            "install Claude Code (https://claude.com/claude-code)",
        );
    };
    let version = claude_version(path);
    let detail = match &version {
        Some(v) => format!("{v} · {}", path.display()),
        None => path.display().to_string(),
    };
    check("claude", Status::Ok, detail)
}

/// An unreadable version is not a problem of its own — the check reports the
/// path either way — so this returns `None` rather than a status.
fn claude_version(claude: &Path) -> Option<String> {
    let mut cmd = Command::new(claude);
    cmd.arg("--version");
    let out = proc::run(&mut cmd, b"", CLAUDE_TIMEOUT).ok()?;
    out.success().then(|| parse_claude_version(&out.text()))?
}

/// `claude --version` answers `2.1.246 (Claude Code)`.
fn parse_claude_version(out: &str) -> Option<String> {
    let word = out.lines().next()?.split_whitespace().next()?;
    word.starts_with(|c: char| c.is_ascii_digit())
        .then(|| word.to_string())
}

/// What `claude auth status --json` says about the local credentials.
enum Login {
    In(String),
    Out,
}

/// Claude Code 2.x answers `claude auth status --json` from local state, no
/// network round trip: `{"loggedIn": true, "authMethod": …, "email": …}`.
fn parse_auth_status(out: &str) -> Option<Login> {
    let payload: Value = serde_json::from_str(out).ok()?;
    if !payload.get("loggedIn")?.as_bool()? {
        return Some(Login::Out);
    }
    let who: Vec<&str> = ["authMethod", "email", "subscriptionType"]
        .iter()
        .filter_map(|k| payload.get(*k).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    Some(Login::In(who.join(" · ")))
}

const LOGIN: &str = "claude login";

fn check_claude_login(claude: Option<&Path>) -> Check {
    let Some(claude) = claude else {
        return check(LOGIN, Status::Warn, "skipped: no claude on PATH");
    };
    let mut cmd = Command::new(claude);
    cmd.args(["auth", "status", "--json"]);
    let out = proc::run(&mut cmd, b"", CLAUDE_TIMEOUT).ok();
    let answered = out.as_ref().filter(|o| o.success());
    match answered.and_then(|o| parse_auth_status(&o.text())) {
        Some(Login::In(who)) if who.is_empty() => check(LOGIN, Status::Ok, "logged in"),
        Some(Login::In(who)) => check(LOGIN, Status::Ok, format!("logged in · {who}")),
        Some(Login::Out) => with_hint(
            check(LOGIN, Status::Fail, "not logged in"),
            "claude auth login",
        ),
        None => credentials_fallback(out.is_some_and(|o| o.timed_out())),
    }
}

/// No usable `claude auth status` (an older Claude Code has no `auth`
/// subcommand): fall back to the credentials file. It is absent when the
/// token lives in the macOS keychain, so its absence is only a warning —
/// nothing here proves the user is logged *out*.
fn credentials_fallback(timed_out: bool) -> Check {
    let creds = hook::claude_dir().map(|d| d.join(".credentials.json"));
    match creds {
        Ok(path) if path.exists() => check(
            LOGIN,
            Status::Ok,
            format!("credentials at {}", path.display()),
        ),
        _ => {
            let why = if timed_out {
                "`claude auth status` timed out"
            } else {
                "`claude auth status` gave no answer"
            };
            with_hint(
                check(LOGIN, Status::Warn, format!("unknown: {why}")),
                "claude auth login",
            )
        }
    }
}

// -------------------------------------------------------------------- hooks

/// One line per Claude Code hook q owns, plus the statusline entry. `drifted`
/// is a failure like `missing` is: it means the entry points at another binary
/// or carries a stale timeout, which is exactly what `q hook install` fixes.
fn check_hooks(chain: &str) -> Vec<Check> {
    let status = match hook::installed_status(chain) {
        Ok(status) => status,
        Err(e) => {
            return vec![with_hint(
                check("hooks", Status::Fail, detail("hooks", &e)),
                "q hook install",
            )];
        }
    };
    let mut checks: Vec<Check> = status
        .events
        .iter()
        .map(|e| hook_check(format!("hook {}", e.event), e.state, None))
        .collect();
    checks.push(hook_check(
        "hook statusLine".to_string(),
        status.statusline.state,
        status.statusline.command.as_deref(),
    ));
    checks
}

fn hook_check(name: String, state: hook::State, command: Option<&str>) -> Check {
    let mut text = state.label().to_string();
    if let Some(command) = command {
        text.push_str(&format!(" · {command}"));
    }
    match state {
        hook::State::Installed => check(name, Status::Ok, text),
        _ => with_hint(check(name, Status::Fail, text), "q hook install"),
    }
}

// --------------------------------------------------------------- statusline

const STATUSLINE: &str = "statusline chain";

/// A statusline payload shaped like the one Claude Code 2.1.x sends
/// (SPEC §23 #1) — enough for `q hook statusline` to take its normal path.
fn probe_payload() -> String {
    json!({
        "hook_event_name": "Status",
        "session_id": "q-doctor-probe",
        "cwd": "/",
        "model": { "id": "q-doctor-probe", "display_name": "Doctor" },
        "context_window": {
            "used_percentage": 42.0,
            "remaining_percentage": 58.0,
            "context_window_size": 200_000,
        },
    })
    .to_string()
}

/// Runs the real handler end to end — this binary, `q hook statusline`, the
/// sample payload on stdin — so a broken chain shows up here instead of in
/// the user's status bar. `Q_DB` points at a path that does not exist, and
/// the handler only records the context window when the database is already
/// there, so the probe writes nothing.
fn check_statusline(chain: &str) -> Check {
    let chain = chain.trim();
    let Ok(exe) = std::env::current_exe() else {
        return check(
            STATUSLINE,
            Status::Warn,
            "skipped: cannot locate the running binary",
        );
    };
    let mut cmd = Command::new(exe);
    cmd.args(["hook", "statusline"])
        .env("Q_DB", probe_db())
        .env_remove("Q_SESSION")
        .env_remove("Q_QUEST")
        .env_remove("TMUX_PANE");
    match proc::run(&mut cmd, probe_payload().as_bytes(), STATUSLINE_TIMEOUT) {
        Ok(out) => evaluate_statusline(out.timed_out(), out.code(), &out.text(), chain),
        Err(e) => check(STATUSLINE, Status::Fail, format!("cannot probe: {e}")),
    }
}

/// A database path nothing ever creates, so the probe stays side-effect free.
fn probe_db() -> PathBuf {
    std::env::temp_dir().join(format!("q-doctor-probe-{}.db", std::process::id()))
}

/// A configured chain that prints nothing only warns: it may legitimately
/// have nothing to say about a synthetic payload, and a red doctor over a
/// cosmetic status bar would be worse than a quiet one.
fn evaluate_statusline(timed_out: bool, code: Option<i32>, out: &str, chain: &str) -> Check {
    let secs = STATUSLINE_TIMEOUT.as_secs();
    if timed_out {
        return with_hint(
            check(
                STATUSLINE,
                Status::Fail,
                format!("`q hook statusline` did not finish in {secs}s"),
            ),
            "check the [statusline] chain command for a hang",
        );
    }
    if code != Some(0) {
        let shown = code.map_or_else(|| "a signal".to_string(), |c| format!("{c}"));
        return check(
            STATUSLINE,
            Status::Fail,
            format!("`q hook statusline` exited {shown}"),
        );
    }
    if chain.is_empty() {
        return check(STATUSLINE, Status::Ok, "no chain configured");
    }
    if out.is_empty() {
        return with_hint(
            check(
                STATUSLINE,
                Status::Warn,
                format!("`{chain}` printed nothing"),
            ),
            "q config set statusline.chain <command>",
        );
    }
    check(
        STATUSLINE,
        Status::Ok,
        format!("`{chain}` → {}", first_line(out, 60)),
    )
}

/// First line, at most `max` chars, ellipsised — a statusline is wide and
/// often coloured, and one report line has room for neither.
fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
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
    let claude = which("claude");
    let chain = &ctx.config.statusline.chain;
    let mut checks = vec![
        check_config(),
        check_tmux(ctx.tmux()),
        check_claude(claude.as_deref()),
        check_claude_login(claude.as_deref()),
        db_check,
        // SPEC §23 #8 is only half checkable: which `q` a shell runs is
        // answered here, but a shell *alias* or function named `q` would take
        // `$SHELL -ic 'type q'` to find — sourcing the user's rc files for a
        // diagnostic is not worth the side effects, so it is left unprobed.
        check_q_on_path(
            std::env::current_exe().ok().as_deref(),
            which("q").as_deref(),
        ),
    ];
    checks.extend(check_hooks(chain));
    checks.push(check_statusline(chain));
    checks.push(check_orphans(db.as_ref(), ctx.tmux(), fix, &mut fixed));
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
    fn claude_version_is_the_leading_number_of_the_first_line() {
        assert_eq!(
            parse_claude_version("2.1.246 (Claude Code)"),
            Some("2.1.246".to_string())
        );
        assert_eq!(
            parse_claude_version("  1.0.0-beta (Claude Code)\nnoise\n"),
            Some("1.0.0-beta".to_string())
        );
        assert_eq!(parse_claude_version("claude 2.1.246"), None);
        assert_eq!(parse_claude_version(""), None);
    }

    #[test]
    fn auth_status_reads_logged_in_and_who() {
        let Some(Login::In(who)) = parse_auth_status(
            r#"{"loggedIn":true,"authMethod":"claude.ai","email":"a@b.c","subscriptionType":"team"}"#,
        ) else {
            panic!("expected a logged-in answer");
        };
        assert_eq!(who, "claude.ai · a@b.c · team");

        let Some(Login::In(who)) = parse_auth_status(r#"{"loggedIn":true}"#) else {
            panic!("expected a logged-in answer");
        };
        assert!(who.is_empty());

        assert!(matches!(
            parse_auth_status(r#"{"loggedIn":false}"#),
            Some(Login::Out)
        ));
        // Anything unparsable falls through to the credentials fallback.
        assert!(parse_auth_status("").is_none());
        assert!(parse_auth_status("not json").is_none());
        assert!(parse_auth_status(r#"{"loggedIn":"yes"}"#).is_none());
        assert!(parse_auth_status(r#"{"authMethod":"claude.ai"}"#).is_none());
    }

    #[test]
    fn a_login_check_without_claude_is_skipped_not_failed() {
        let skipped = check_claude_login(None);
        assert_eq!(skipped.status, Status::Warn);
        assert!(skipped.detail.contains("skipped"), "{skipped:?}");
    }

    #[test]
    fn claude_missing_from_path_fails_with_an_install_hint() {
        let missing = check_claude(None);
        assert_eq!(missing.status, Status::Fail);
        assert!(missing.fix_hint.is_some());
    }

    #[test]
    fn statusline_probe_grades_the_handler_then_the_chain() {
        // The handler itself must exit 0.
        let timeout = evaluate_statusline(true, None, "", "ccusage");
        assert_eq!(timeout.status, Status::Fail);
        assert!(timeout.detail.contains("did not finish"), "{timeout:?}");
        assert!(timeout.fix_hint.is_some());

        let failed = evaluate_statusline(false, Some(2), "", "");
        assert_eq!(failed.status, Status::Fail);
        assert!(failed.detail.contains("exited 2"), "{failed:?}");
        assert_eq!(
            evaluate_statusline(false, None, "", "").detail,
            "`q hook statusline` exited a signal"
        );

        // No chain: nothing to echo, and that is fine.
        let none = evaluate_statusline(false, Some(0), "", "");
        assert_eq!(none.status, Status::Ok);
        assert_eq!(none.detail, "no chain configured");

        // A configured chain that says nothing is suspicious, not fatal.
        let silent = evaluate_statusline(false, Some(0), "", "ccusage");
        assert_eq!(silent.status, Status::Warn);
        assert!(silent.detail.contains("printed nothing"), "{silent:?}");

        let echoed = evaluate_statusline(false, Some(0), "ctx 42%\nsecond", "ccusage");
        assert_eq!(echoed.status, Status::Ok);
        assert_eq!(echoed.detail, "`ccusage` → ctx 42%");
    }

    #[test]
    fn first_line_is_one_line_and_bounded() {
        assert_eq!(first_line("  a  \nb", 10), "a");
        assert_eq!(first_line("", 10), "");
        assert_eq!(first_line("čćžšđ", 3), "čć…");
        assert_eq!(first_line("čćžšđ", 5), "čćžšđ");
    }

    #[test]
    fn a_hook_is_ok_only_when_installed() {
        let ok = hook_check("hook Stop".to_string(), hook::State::Installed, None);
        assert_eq!(ok.status, Status::Ok);
        assert_eq!(ok.detail, "installed");
        assert!(ok.fix_hint.is_none());

        for state in [hook::State::Missing, hook::State::Drifted] {
            let bad = hook_check(
                "hook statusLine".to_string(),
                state,
                Some("/old/q hook statusline"),
            );
            assert_eq!(bad.status, Status::Fail, "{state:?}");
            assert!(bad.detail.ends_with("· /old/q hook statusline"), "{bad:?}");
            assert_eq!(bad.fix_hint.as_deref(), Some("q hook install"));
        }
    }

    #[test]
    fn the_probe_payload_carries_the_field_the_handler_reads() {
        let payload: Value = serde_json::from_str(&probe_payload()).unwrap();
        assert!(payload["context_window"]["used_percentage"].is_number());
        assert!(payload["session_id"].as_str().is_some());
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
