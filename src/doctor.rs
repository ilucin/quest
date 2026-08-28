//! `q doctor` (SPEC §19): checks the local environment and, with `--fix`,
//! repairs what it can. Checks arrive with the features they diagnose; the
//! rest of §19 (remotes, `brain`, `gh`) is still to come.
//!
//! Every check is independent and swallows its own errors, so a broken
//! environment still produces a full report instead of a single failure.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};

use crate::Ctx;
use crate::commands::hook;
use crate::commands::skill;
use crate::config::Config;
use crate::db::Db;
use crate::model::{Session, now};
use crate::output;
use crate::proc;
use crate::remote;
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
/// Budget for the statusline probe: the chain's own budget plus room for the
/// two spawns around it. Anything less would fail a chain the handler itself
/// would have accepted.
const PROBE_MARGIN: Duration = Duration::from_secs(2);
const STATUSLINE_TIMEOUT: Duration = hook::CHAIN_TIMEOUT.saturating_add(PROBE_MARGIN);

fn check_claude(claude: Option<&Path>) -> Check {
    let Some(path) = claude else {
        return with_hint(
            check("claude", Status::Fail, "not found on PATH"),
            "install Claude Code (https://claude.com/claude-code)",
        );
    };
    match claude_version(path) {
        Some(v) => check("claude", Status::Ok, format!("{v} · {}", path.display())),
        // On PATH but not answering: a broken install, a wrapper script that
        // swallows `--version`, something. Not proof it cannot run, so a warning.
        None => with_hint(
            check(
                "claude",
                Status::Warn,
                format!("{} did not answer `--version`", path.display()),
            ),
            "reinstall Claude Code (https://claude.com/claude-code)",
        ),
    }
}

/// `None` when the call failed or printed something unrecognisable.
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
    // Deliberately not `email`: `q doctor` output ends up pasted into issues.
    let who: Vec<&str> = ["authMethod", "subscriptionType"]
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
    // `claude auth status --json` exits 1 when logged out but still prints the
    // payload (verified on 2.1.246), so the JSON decides, not the exit status.
    match out.as_ref().and_then(|o| parse_auth_status(&o.text())) {
        Some(Login::In(who)) if who.is_empty() => check(LOGIN, Status::Ok, "logged in"),
        Some(Login::In(who)) => check(LOGIN, Status::Ok, format!("logged in · {who}")),
        Some(Login::Out) => with_hint(
            check(LOGIN, Status::Fail, "not logged in"),
            "claude auth login",
        ),
        None => credentials_fallback(out.is_some_and(|o| o.timed_out())),
    }
}

/// `~/.claude/.credentials.json` — where Claude Code keeps a token, when it
/// keeps one in a file at all. Not derived from `$Q_CLAUDE_SETTINGS`: that
/// override says which settings file q edits, not where Claude stores its
/// credentials.
fn credentials_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".claude").join(".credentials.json"))
}

/// No usable `claude auth status` (an older Claude Code has no `auth`
/// subcommand): fall back to the credentials file. It is absent when the
/// token lives in the macOS keychain, so its absence is only a warning —
/// nothing here proves the user is logged *out*.
fn credentials_fallback(timed_out: bool) -> Check {
    credentials_status(credentials_path().as_deref(), timed_out)
}

fn credentials_status(creds: Option<&Path>, timed_out: bool) -> Check {
    match creds {
        Some(path) if path.exists() => check(
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
    let status = match hook::installed_status(None, chain) {
        Ok(status) => status,
        Err(e) => {
            return vec![with_hint(
                check("hooks", Status::Fail, detail("hooks", &e)),
                hooks_hint(&e),
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

/// `q hook install` cannot help with a settings file that does not parse — it
/// reads the same file and fails the same way — so say what actually unblocks.
fn hooks_hint(e: &anyhow::Error) -> String {
    match (
        e.downcast_ref::<crate::error::QError>(),
        hook::settings_path(),
    ) {
        (Some(crate::error::QError::Settings(_)), Ok(path)) => {
            format!("fix the JSON at {}", path.display())
        }
        _ => "q hook install".to_string(),
    }
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

// --------------------------------------------------------------------- skill

/// The embedded agent SKILL.md (SPEC §18). `drifted` is a failure like
/// `missing` is: an out-of-date file means agents read a stale command surface,
/// which is exactly what `q skill install` refreshes. With `--fix` it installs
/// or updates the file and reports it as repaired.
const SKILL: &str = "skill";

fn check_skill(fix: bool, fixed: &mut Vec<String>) -> Check {
    let status = match skill::installed_status() {
        Ok(status) => status,
        Err(e) => {
            return with_hint(
                check(SKILL, Status::Fail, detail(SKILL, &e)),
                "q skill install",
            );
        }
    };
    if fix && status.state != hook::State::Installed {
        match skill::ensure_installed() {
            Ok(true) => {
                fixed.push(format!("installed q skill at {}", status.path.display()));
                return check(
                    SKILL,
                    Status::Ok,
                    format!("installed · {}", status.path.display()),
                );
            }
            // Nothing to do (already installed) falls through to the report.
            Ok(false) => {}
            Err(e) => {
                return with_hint(
                    check(SKILL, Status::Fail, detail(SKILL, &e)),
                    "q skill install",
                );
            }
        }
    }
    skill_check(status.state, &status.path.display().to_string())
}

fn skill_check(state: hook::State, path: &str) -> Check {
    match state {
        hook::State::Installed => check(SKILL, Status::Ok, path.to_string()),
        hook::State::Missing => with_hint(
            check(SKILL, Status::Fail, format!("not installed · {path}")),
            "q skill install",
        ),
        hook::State::Drifted => with_hint(
            check(SKILL, Status::Fail, format!("out of date · {path}")),
            "q skill install",
        ),
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
        .env(hook::PROBE_ENV, "1")
        .env_remove("Q_SESSION")
        .env_remove("Q_QUEST")
        .env_remove("TMUX_PANE");
    let started = Instant::now();
    match proc::run(&mut cmd, probe_payload().as_bytes(), STATUSLINE_TIMEOUT) {
        Ok(out) => evaluate_statusline(
            Probe {
                timed_out: out.timed_out(),
                code: out.code(),
                out: out.text(),
                // `Q_PROBE` makes the handler report the chain's fate here.
                diag: out.stderr_text(),
                elapsed: started.elapsed(),
            },
            chain,
        ),
        Err(e) => check(STATUSLINE, Status::Fail, format!("cannot probe: {e}")),
    }
}

/// A database path nothing ever creates: the parent directory does not exist,
/// so even a handler that tried to open it could not — and a leftover file
/// from an earlier run cannot be picked up by mistake.
fn probe_db() -> PathBuf {
    std::env::temp_dir()
        .join(format!("q-doctor-probe-{}", std::process::id()))
        .join("q.db")
}

/// A configured chain that prints nothing only warns: it may legitimately
/// have nothing to say about a synthetic payload, and a red doctor over a
/// cosmetic status bar would be worse than a quiet one.
/// What the probe run produced: the handler's own outcome plus, on stderr,
/// how the chain inside it fared (see `hook::PROBE_ENV`).
struct Probe {
    timed_out: bool,
    code: Option<i32>,
    out: String,
    diag: String,
    /// How long the handler actually took. A handler that outlives the budget
    /// its own chain gets is holding something open (a backgrounded process
    /// on the chain's stdout, say) and would do the same in the status bar.
    elapsed: Duration,
}

fn evaluate_statusline(probe: Probe, chain: &str) -> Check {
    let Probe {
        timed_out,
        code,
        out,
        diag,
        elapsed,
    } = probe;
    // The handler's stderr is another program's output arriving in a report
    // line: one line, escapes stripped, bounded — same as the chain's stdout.
    let diag = output::first_line(&diag, 100);
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
    // The handler never fails over a bad chain — a status bar is cosmetic — so
    // a chain that timed out or exited non-zero is reported, not hidden.
    if !diag.is_empty() {
        return with_hint(
            check(STATUSLINE, Status::Warn, format!("`{chain}` {diag}")),
            "check the [statusline] chain command",
        );
    }
    // Succeeded, but not in time to be a status bar: the chain came back
    // inside its own budget yet the handler did not, so something it spawned
    // is still holding a pipe.
    let budget = hook::CHAIN_TIMEOUT;
    if elapsed > budget {
        return with_hint(
            check(
                STATUSLINE,
                Status::Warn,
                format!(
                    "`{chain}` took {:.1}s — longer than the {}s budget",
                    elapsed.as_secs_f32(),
                    budget.as_secs()
                ),
            ),
            "check the [statusline] chain for a background process holding stdout",
        );
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
        format!("`{chain}` → {}", output::first_line(&out, 60)),
    )
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

/// `bd` and `BEADS_DIR` (SPEC §13, §19). A warning, not a failure: a Quest is
/// perfectly usable without beads.
fn check_beads() -> Check {
    const NAME: &str = "bd";
    let Some(path) = which("bd") else {
        return with_hint(
            check(NAME, Status::Warn, "not found on PATH"),
            "quests are created without an epic; install bd or pass --no-beads",
        );
    };
    match std::env::var_os("BEADS_DIR").filter(|v| !v.is_empty()) {
        Some(dir) => check(
            NAME,
            Status::Ok,
            format!("{} · BEADS_DIR={}", path.display(), dir.to_string_lossy()),
        ),
        None => with_hint(
            check(
                NAME,
                Status::Warn,
                format!("{} · BEADS_DIR is not set", path.display()),
            ),
            "bd will look for a .beads directory below the cwd",
        ),
    }
}

// ------------------------------------------------------------------- remotes
//
// **What the remote block measures.** One probe per remote: `ssh <alias> q
// --version`. That is SPEC §19's probe and it is all doctor has. `q list` asks
// a different question (`q list --json --no-remote --all`) and a proxied
// command asks a third, so a version banner cannot promise what either of
// those will do — the live test for this bead found a far end whose
// `--version` fails while every other verb works, and one whose `--version` is
// perfect while its `q list --json` is garbage.
//
// **How severity is assigned**, given that:
//
// * `Fail` — the probe *proves* that no `q` command on that machine can run:
//   ssh got there and the shell found no `q` at all ([`Reach::NoQ`], exit
//   127). This is the only remote failure.
// * `Warn` — everything else the probe found unusual, because everything else
//   is a *prediction* about a command doctor did not run: a host that did not
//   answer, a `q --version` that failed on its own terms, a wire tag below the
//   floor, above this build, missing or unreadable, an answer that is not a
//   version, a missing `ControlMaster`.
// * `Ok` — a `q` over there answered with a version this `q` can read.
//
// **The coherence claim, one-directional.** Doctor never fails a host `q list`
// is willing to use: the one `Fail` is 127, which `q list` also refuses
// (`incompatible (reachable, but no q on PATH there …)`). The converse does
// **not** hold and cannot — a far end can answer `q --version` correctly and
// still fail `q list --json` or a proxied command — so the `Ok` line says what
// it actually saw rather than issuing a clean bill of health.
//
// This is also why no wire verdict fails. The wire number is diagnostic only
// (see [`remote::WIRE`]): nothing in `q` consults it before talking to a
// remote, so a `Fail` on a tag below [`remote::MIN_REMOTE_WIRE`] would condemn
// a host the listing and the proxy go on using — the exact contradiction that
// made the first cut of this bead red on a healthy setup, one
// `MIN_REMOTE_WIRE` bump later.
//
// A missing **local** `q` on PATH is a `Warn` while a missing **remote** one
// is a `Fail`, deliberately: locally a `q` is provably running and the check
// is only about which one a shell would pick, whereas remotely there is none
// at all and every remote feature for that machine is dead.

/// What `ssh <alias> q --version` established about one remote (SPEC §19:
/// *every remote reachable and version compatible*).
///
/// The states the report has to keep apart, each with its own fix, are
/// [`Reach::Down`] (ssh itself did not get there), [`Reach::Silent`] (nothing
/// came back in time), [`Reach::NoQ`] (ssh got there and there is no `q` on
/// that machine) and [`Reach::TooOld`] (there is one, and it says it speaks a
/// wire older than the contract this `q` expects).
#[derive(Debug, PartialEq, Eq)]
enum Reach {
    /// Reachable, and its wire is one this `q` can drive.
    Ok(remote::RemoteVersion),
    /// Reachable, and its tag is readable and below the floor it carries
    /// ([`remote::MIN_REMOTE_WIRE`], except in a test that fakes a bump).
    ///
    /// The strongest statement a banner can make about compatibility — and
    /// still only a prediction about a command doctor never ran, since nothing
    /// in `q` consults the wire before talking to a remote. It warns.
    TooOld(remote::RemoteVersion, u32),
    /// Reachable, and its `q` speaks a wire *newer* than this one's.
    Newer(remote::RemoteVersion),
    /// Reachable, and its `q` reports no wire at all: every `q` from before
    /// the wire was numbered. That range holds both a `q` this one drives end
    /// to end (`main` as of bd-8lz.5.3, which `q list` talks to happily) and a
    /// `q` that rejects the hidden `--expect` with clap's exit 2 — the case
    /// bd-8lz.5.3 left cryptic. A banner cannot tell those apart, so this is
    /// *unknown*, not *too old*.
    Untagged(remote::RemoteVersion),
    /// Reachable, and its wire tag is not a number this `q` can read. A broken
    /// answer, not an old `q`.
    BadWire(remote::RemoteVersion),
    /// Answered, but not with anything resembling a version.
    Unreadable(String),
    /// ssh got there; the shell could not find `q`.
    NoQ,
    /// ssh never got there: host down, unroutable, unknown, or refusing the key.
    Down(String),
    /// Nothing came back before the deadline. Kept apart from [`Reach::Down`]
    /// because it is *not* evidence of a network problem: the far end may be
    /// answering `q list` perfectly while this one probe hangs, so the fix
    /// line must not send the user to `~/.ssh/config` (bd-8lz.5.4 D4).
    Silent(String),
    /// `q --version` ran over there and failed on its own terms. Says nothing
    /// about the other verbs: the live test found a far end whose `--version`
    /// exits 3 while `q list` and every proxied command work (D1).
    Ran(String),
}

fn diagnose(outcome: &remote::SshOutcome) -> Reach {
    diagnose_with(outcome, remote::MIN_REMOTE_WIRE)
}

/// [`diagnose`], with the wire floor passed in so a test can fake the bump
/// that would otherwise make [`Reach::TooOld`] unreachable — no shipped `q`
/// prints a readable tag below 1.
fn diagnose_with(outcome: &remote::SshOutcome, floor: u32) -> Reach {
    use remote::SshOutcome;
    let (code, stdout, stderr) = match outcome {
        SshOutcome::TimedOut => {
            return Reach::Silent(format!(
                "no answer within {}s",
                remote::PROBE_TIMEOUT.as_secs()
            ));
        }
        SshOutcome::TooLarge => {
            return Reach::Unreadable(format!(
                "more than {} MiB of output",
                remote::MAX_OUTPUT >> 20
            ));
        }
        SshOutcome::Failed(e) => return Reach::Down(e.clone()),
        SshOutcome::Done {
            code,
            stdout,
            stderr,
        } => (*code, stdout, stderr),
    };
    // ssh reports its own failures as 255 and a signalled command as no code;
    // anything else came from the far end, so the host is up.
    match code {
        Some(remote::SSH_FAILED) | None => {
            return Reach::Down(
                said(stderr).unwrap_or_else(|| "ssh could not connect".to_string()),
            );
        }
        Some(remote::NO_COMMAND) => return Reach::NoQ,
        Some(0) => {}
        Some(c) => {
            let said = said(stderr).map_or(String::new(), |s| format!(": {s}"));
            return Reach::Ran(format!("`q --version` exited {c}{said}"));
        }
    }
    let Some(version) = remote::parse_version(stdout) else {
        return Reach::Unreadable(
            said(stdout)
                .or_else(|| said(stderr))
                .unwrap_or_else(|| "nothing".to_string()),
        );
    };
    match &version.wire {
        remote::Wire::Speaks(wire) if *wire > remote::WIRE => Reach::Newer(version),
        remote::Wire::Speaks(wire) if *wire < floor => Reach::TooOld(version, floor),
        remote::Wire::Speaks(_) => Reach::Ok(version),
        remote::Wire::Untagged => Reach::Untagged(version),
        remote::Wire::Unreadable(_) => Reach::BadWire(version),
    }
}

/// Another program's output on its way into a report line: one line, bounded.
fn said(text: &str) -> Option<String> {
    let line = output::first_line(text, 120);
    (!line.is_empty()).then_some(line)
}

/// `wire 1` / `no wire version` — what a remote `q` says it speaks.
fn spoken(version: &remote::RemoteVersion) -> String {
    match &version.wire {
        remote::Wire::Speaks(wire) => format!("wire {wire}"),
        remote::Wire::Untagged => "no wire version".to_string(),
        remote::Wire::Unreadable(tag) => format!("`wire {tag}`"),
    }
}

fn remote_check(probe: &remote::Probe, local: &str) -> Check {
    reach_check(diagnose(&probe.version), &probe.name, &probe.ssh, local)
}

fn reach_check(reach: Reach, machine: &str, alias: &str, local: &str) -> Check {
    let name = format!("remote {machine}");
    // The fix the whole version story exists to be able to print.
    let upgrade = format!("upgrade `q` on {machine}");
    match reach {
        // A statement about the probe, not a clean bill of health: `q list`
        // and a proxied command ask this host different questions, and a
        // banner cannot answer for them (bd-8lz.5.4 D3).
        Reach::Ok(v) => check(
            name,
            Status::Ok,
            format!(
                "{} · ssh {alias} — it answered `q --version`; only `q list` can show it serves",
                v.label()
            ),
        ),
        // The strongest thing a banner can say about compatibility, and still
        // a prediction: nothing in `q` consults the wire before talking to a
        // remote, so failing here would condemn a host the listing and the
        // proxy go on using (bd-8lz.5.4 D2).
        Reach::TooOld(v, floor) => with_hint(
            check(
                name,
                Status::Warn,
                format!(
                    "{} speaks {}, older than the wire {floor} this `q` expects — \
                     a proxied command may fail",
                    v.label(),
                    spoken(&v),
                ),
            ),
            upgrade,
        ),
        // No tag means "older than wire tagging", which spans everything up to
        // and including bd-8lz.5.3 — binaries this `q` drives end to end and
        // binaries that reject `--expect`, indistinguishable from here. `q
        // list` will talk to it and will usually be right, so failing it would
        // be doctor contradicting the listing about the same host. Report what
        // is unknown instead (bd-8lz.5.4 review F3).
        Reach::Untagged(v) => with_hint(
            check(
                name,
                Status::Warn,
                format!(
                    "{} · ssh {alias} — no wire tag, so whether it speaks wire {} \
                     cannot be told from here; a proxied command may still fail with \
                     `unexpected argument '--expect'`",
                    v.label(),
                    remote::WIRE
                ),
            ),
            format!("{upgrade} to make its wire knowable"),
        ),
        // A tag that is not a number is a broken answer, not an old `q`, and
        // must not be read as "older than everything".
        Reach::BadWire(v) => with_hint(
            check(
                name,
                Status::Warn,
                format!(
                    "{} · ssh {alias} — its wire tag is not a number this `q` can read",
                    v.label()
                ),
            ),
            format!("check what `ssh {alias} q --version` prints"),
        ),
        // Not a failure: the far end knows a wire this one does not, and the
        // listing parse already ignores fields it has never heard of. The fix
        // is on this machine, so it is worth saying which one that is.
        Reach::Newer(v) => with_hint(
            check(
                name,
                Status::Warn,
                format!(
                    "{} speaks {}, newer than this `q` (wire {})",
                    v.label(),
                    spoken(&v),
                    remote::WIRE
                ),
            ),
            format!("upgrade `q` on {local}"),
        ),
        // The one remote failure, and the only state the probe *proves*: the
        // shell over there looked for `q` and found none, so every `q` command
        // aimed at this machine dies the same way — which is also why `q list`
        // calls it `incompatible` and refuses to use it. Unlike the *local* `q
        // on PATH` warning above, where a `q` is provably running and the
        // question is only which one a shell would pick.
        Reach::NoQ => with_hint(
            check(
                name,
                Status::Fail,
                format!("`ssh {alias}` works, but there is no `q` on PATH there"),
            ),
            format!("install q on {machine}, or put it on the PATH its login shell uses"),
        ),
        // A warning, not a failure: a laptop that is asleep or off the VPN is
        // neither a broken setup nor one this machine can repair, and a
        // scripted `q doctor` must not flap with it (bd-8lz.5.4 review F4).
        Reach::Down(why) => with_hint(
            check(name, Status::Warn, format!("ssh {alias}: {why}")),
            format!("check that `ssh {alias}` works — host, network, key, ~/.ssh/config"),
        ),
        // Silence is not a network diagnosis. The probe alone hung; `q list`
        // may be talking to this same host perfectly, so the fix line asks
        // about the probe rather than blaming the connection (D4).
        Reach::Silent(why) => with_hint(
            check(
                name,
                Status::Warn,
                format!(
                    "`ssh {alias} q --version`: {why} — the host may be asleep, \
                     or `q` there may be wedged"
                ),
            ),
            format!("check that `ssh {alias} q --version` answers"),
        ),
        // The probe failed on its own terms, which says nothing about the
        // other verbs — the live test found a far end whose `--version` exits
        // 3 while `q list` and every proxied command work (D1).
        Reach::Ran(what) => with_hint(
            check(
                name,
                Status::Warn,
                format!("{what} — the probe failed, not necessarily anything else"),
            ),
            format!("check what `ssh {alias} q --version` runs, or {upgrade}"),
        ),
        // Something answered and it was not a version: a login shell banner, a
        // wrapper that swallows `--version`, an rc file printing at us. Not
        // proof of an old `q`, so it warns rather than failing.
        Reach::Unreadable(said) => with_hint(
            check(
                name,
                Status::Warn,
                format!("`q --version` on {alias} answered {said}"),
            ),
            format!("check what `ssh {alias} q --version` runs"),
        ),
    }
}

/// SPEC §23 #6: ssh multiplexing is recommended, and `q doctor` warns when it
/// is missing. A warning and never a failure — a remote without it works, it
/// is merely slow (measured on a real host: a proxied command goes from ~0.9 s
/// to ~0.35 s once the mux is warm, and the TUI pays that every `tick_remote`).
///
/// `q` never edits `~/.ssh/config`; the hint says what to add.
fn multiplexing_check(probe: &remote::Probe) -> Check {
    use remote::Multiplexing;
    let name = format!("ssh multiplexing {}", probe.name);
    let alias = &probe.ssh;
    match &probe.multiplexing {
        Multiplexing::On { persist } => check(
            name,
            Status::Ok,
            format!("ControlMaster for {alias}, ControlPersist {persist}"),
        ),
        // A master with no persistence is the trap: `ssh -G` says
        // `controlmaster auto` and nothing is ever reused, because every mux
        // dies with the command that opened it.
        Multiplexing::NotPersisted => with_hint(
            check(
                name,
                Status::Warn,
                format!(
                    "{alias} has a ControlMaster but `ControlPersist no` — no connection outlives its command"
                ),
            ),
            format!("add `ControlPersist 10m` for {alias} in ~/.ssh/config"),
        ),
        Multiplexing::Off => with_hint(
            check(
                name,
                Status::Warn,
                format!("not configured for {alias} — every remote command pays a fresh handshake"),
            ),
            format!(
                "add `ControlMaster auto`, `ControlPath ~/.ssh/cm-%r@%h:%p` and \
                 `ControlPersist 10m` for {alias} in ~/.ssh/config"
            ),
        ),
        Multiplexing::Unknown(why) => with_hint(
            check(name, Status::Warn, format!("`ssh -G {alias}`: {why}")),
            format!("check that `ssh -G {alias}` answers"),
        ),
    }
}

/// Two lines per configured remote, and **nothing at all** when there are
/// none: the common case is `remotes = []`, and it must cost no ssh and no
/// time (bd-8lz.5.4).
fn check_remotes(ctx: &Ctx) -> Vec<Check> {
    remote::probe_all(ctx)
        .iter()
        .flat_map(|probe| {
            [
                // The machine this `q` is *running on*, never `--machine`'s
                // target: `Reach::Newer`'s hint says "the fix is here", and
                // under `--machine ws` `ctx.machine()` is `ws` — the machine
                // that is already newer (bd-8lz.5.4 review F1).
                remote_check(probe, &ctx.config.machine.name),
                multiplexing_check(probe),
            ]
        })
        .collect()
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
        check_beads(),
    ];
    checks.extend(check_hooks(chain));
    checks.push(check_skill(fix, &mut fixed));
    checks.push(check_statusline(chain));
    // SPEC §19's order: the remotes, then the orphans.
    checks.extend(check_remotes(ctx));
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

    fn answered(code: i32, stdout: &str, stderr: &str) -> remote::SshOutcome {
        remote::SshOutcome::Done {
            code: Some(code),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        }
    }

    /// The three failures SPEC §19 has to keep apart, each with its own fix:
    /// ssh did not get there, it did and there is no `q`, and there is one and
    /// it is too old (bd-8lz.5.4).
    #[test]
    fn a_probe_tells_a_dead_host_a_missing_q_and_an_old_q_apart() {
        // ssh's own failure code, and ssh that never started.
        assert!(matches!(
            diagnose(&answered(255, "", "ssh: Could not resolve hostname ws")),
            Reach::Down(why) if why.contains("Could not resolve")
        ));
        assert!(matches!(
            diagnose(&remote::SshOutcome::Failed("ssh not found on PATH".into())),
            Reach::Down(_)
        ));
        // A deadline is not a network diagnosis: kept apart from `Down` so the
        // fix line does not blame ssh for a probe that hung (D4).
        assert!(matches!(
            diagnose(&remote::SshOutcome::TimedOut),
            Reach::Silent(why) if why.contains("no answer within")
        ));

        // The shell got as far as looking for `q`: the machine is up.
        assert_eq!(
            diagnose(&answered(127, "", "zsh: command not found: q")),
            Reach::NoQ
        );

        // A `q` that answers and predates the wire tag: unknown, not too old.
        assert!(matches!(
            diagnose(&answered(0, "q 0.1.0\n", "")),
            Reach::Untagged(v) if v.wire == remote::Wire::Untagged
        ));

        // A tag that is there and below the floor.
        assert!(matches!(
            diagnose(&answered(0, "q 0.0.1 (wire 0)\n", "")),
            Reach::TooOld(_, 1)
        ));
        // …and the same verdict under a floor that no shipped `q` has yet, so
        // the branch a `MIN_REMOTE_WIRE` bump would light up is covered rather
        // than left unreachable: this build's own banner would then be "old".
        assert!(matches!(
            diagnose_with(&answered(0, &format!("q {}\n", remote::VERSION), ""), 2),
            Reach::TooOld(_, 2)
        ));

        // A tag that is not a number is a broken answer, not an old `q`.
        for line in ["q 0.1.0 (wire 4294967296)", "q 0.1.0 (wire -1)"] {
            assert!(
                matches!(diagnose(&answered(0, line, "")), Reach::BadWire(_)),
                "{line}"
            );
        }

        // A `q` this one can drive, and one from the future.
        assert!(matches!(
            diagnose(&answered(0, &format!("q {}\n", remote::VERSION), "")),
            Reach::Ok(_)
        ));
        assert!(matches!(
            diagnose(&answered(0, "q 9.9.9 (wire 99)\n", "")),
            Reach::Newer(_)
        ));

        // Answered, but not with a version — and a `q --version` that failed
        // on its own terms.
        assert!(matches!(
            diagnose(&answered(0, "Welcome to ws!\n", "")),
            Reach::Unreadable(said) if said.contains("Welcome")
        ));
        assert!(matches!(
            diagnose(&answered(2, "", "error: nope")),
            Reach::Ran(_)
        ));
    }

    fn remote_probe(
        version: remote::SshOutcome,
        multiplexing: remote::Multiplexing,
    ) -> remote::Probe {
        remote::Probe {
            name: "ws".to_string(),
            ssh: "ws-host".to_string(),
            version,
            multiplexing,
        }
    }

    /// The fix line the whole version story exists to be able to print — and
    /// the severity each wire verdict earns. No wire verdict fails: nothing in
    /// `q` consults the wire before talking to a remote, so every one of them
    /// is a prediction about a command doctor never ran (bd-8lz.5.4 D2).
    #[test]
    fn no_wire_verdict_fails_the_remote_check() {
        let checked = |stdout: &str| {
            remote_check(
                &remote_probe(answered(0, stdout, ""), remote::Multiplexing::Off),
                "laptop",
            )
        };

        // No tag: everything up to and including bd-8lz.5.3 prints this, and
        // `q list` talks to most of it happily. Advisory, and it says so.
        let untagged = checked("q 0.1.0\n");
        assert_eq!(untagged.status, Status::Warn);
        assert!(
            untagged
                .fix_hint
                .as_deref()
                .unwrap()
                .contains("upgrade `q` on ws"),
            "{untagged:?}"
        );
        assert!(untagged.detail.contains("no wire tag"), "{untagged:?}");

        // A tag below the floor: the strongest thing a banner can say, and
        // still only advice.
        let old = checked("q 0.0.1 (wire 0)\n");
        assert_eq!(old.status, Status::Warn);
        assert_eq!(old.fix_hint.as_deref(), Some("upgrade `q` on ws"));
        assert!(old.detail.contains("may fail"), "{old:?}");

        // A tag that is not a number: unreadable, never "older than nothing".
        let bad = checked("q 0.1.0 (wire -1)\n");
        assert_eq!(bad.status, Status::Warn);
        assert!(bad.detail.contains("(wire -1)"), "{bad:?}");
        assert!(!bad.detail.contains("older than"), "{bad:?}");

        // The other direction points at this machine instead.
        let new = checked("q 9.9.9 (wire 99)\n");
        assert_eq!(new.status, Status::Warn);
        assert_eq!(new.fix_hint.as_deref(), Some("upgrade `q` on laptop"));
    }

    /// The bump that has not happened yet: with the floor raised, this build's
    /// own banner reads as too old — and still only warns, so the first bead
    /// to raise `MIN_REMOTE_WIRE` cannot turn a whole rollout window red on
    /// setups that work (bd-8lz.5.4 D2).
    #[test]
    fn a_floor_bump_would_warn_and_not_fail() {
        let banner = answered(0, &format!("q {}\n", remote::VERSION), "");
        let check = reach_check(diagnose_with(&banner, 2), "ws", "ws-host", "laptop");
        assert_eq!(check.status, Status::Warn, "{check:?}");
        assert!(check.detail.contains("older than the wire 2"), "{check:?}");
        assert_eq!(check.fix_hint.as_deref(), Some("upgrade `q` on ws"));
    }

    /// `Fail` is a claim, and the probe only ever proves one thing: ssh got
    /// there and the shell found no `q`. Everything else — a host that did not
    /// answer, a `q --version` that failed on its own terms — is a prediction
    /// about commands doctor never ran, and warns (bd-8lz.5.4 D1/D4).
    #[test]
    fn only_a_missing_q_fails_the_remote_check() {
        let probe = |v| remote_check(&remote_probe(v, remote::Multiplexing::Off), "laptop");

        for advisory in [
            answered(255, "", "ssh: connect to host ws port 22: No route to host"),
            remote::SshOutcome::TimedOut,
            remote::SshOutcome::Failed("ssh not found on PATH".into()),
            // D1: `q --version` exits non-zero while every other verb serves.
            answered(3, "", "boom: cannot start"),
            answered(0, "Welcome to ws!", ""),
        ] {
            let check = probe(advisory);
            assert_eq!(check.status, Status::Warn, "{check:?}");
        }

        let no_q = probe(answered(127, "", "zsh: command not found: q"));
        assert_eq!(no_q.status, Status::Fail);

        // The timeout's fix line asks about the probe, not about the network:
        // `q list` may be talking to this same host perfectly (D4).
        let silent = probe(remote::SshOutcome::TimedOut);
        let hint = silent.fix_hint.as_deref().unwrap();
        assert!(hint.contains("q --version` answers"), "{hint}");
        assert!(!hint.contains("~/.ssh/config"), "{hint}");
    }

    /// A green remote line reports what the probe saw and nothing more: a far
    /// end can answer `q --version` perfectly and still fail `q list --json`
    /// or a proxied command (bd-8lz.5.4 D3).
    #[test]
    fn a_green_remote_line_does_not_claim_the_remote_serves() {
        let ok = remote_check(
            &remote_probe(
                answered(0, &format!("q {}\n", remote::VERSION), ""),
                remote::Multiplexing::Off,
            ),
            "laptop",
        );
        assert_eq!(ok.status, Status::Ok);
        assert!(ok.detail.contains("`q --version`"), "{ok:?}");
        assert!(ok.detail.contains("q list"), "{ok:?}");
    }

    /// SPEC §23 #6: a missing `ControlMaster` is advice, never a failure — and
    /// the hint says what to add, because `q` never edits `~/.ssh/config`.
    #[test]
    fn a_missing_control_master_only_warns() {
        let off = multiplexing_check(&remote_probe(
            answered(0, "q 0.1.0 (wire 1)", ""),
            remote::Multiplexing::Off,
        ));
        assert_eq!(off.status, Status::Warn);
        assert!(off.fix_hint.unwrap().contains("~/.ssh/config"));

        let on = multiplexing_check(&remote_probe(
            answered(0, "q 0.1.0 (wire 1)", ""),
            remote::Multiplexing::On {
                persist: "10m".to_string(),
            },
        ));
        assert_eq!(on.status, Status::Ok);
        assert!(on.fix_hint.is_none());

        for state in [
            remote::Multiplexing::NotPersisted,
            remote::Multiplexing::Unknown("ssh -G said nothing".to_string()),
        ] {
            let check =
                multiplexing_check(&remote_probe(answered(0, "q 0.1.0 (wire 1)", ""), state));
            assert_eq!(check.status, Status::Warn, "{check:?}");
        }
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
        assert_eq!(who, "claude.ai · team", "the email must not be reported");

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
    fn the_credentials_fallback_only_warns_when_there_is_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let creds = dir.path().join(".credentials.json");

        let unknown = credentials_status(Some(&creds), false);
        assert_eq!(unknown.status, Status::Warn);
        assert!(unknown.detail.starts_with("unknown: "), "{unknown:?}");
        assert!(unknown.detail.contains("no answer"), "{unknown:?}");
        assert!(
            credentials_status(Some(&creds), true)
                .detail
                .contains("timed out")
        );
        assert_eq!(credentials_status(None, false).status, Status::Warn);

        fs::write(&creds, "{}").unwrap();
        let found = credentials_status(Some(&creds), false);
        assert_eq!(found.status, Status::Ok);
        assert!(found.detail.starts_with("credentials at "), "{found:?}");
    }

    #[test]
    fn the_credentials_path_is_anchored_at_the_home_directory() {
        // Never derived from `$Q_CLAUDE_SETTINGS`: that is q's override for
        // the file it edits, not Claude's token store.
        let path = credentials_path().expect("a home directory");
        assert!(path.ends_with(".claude/.credentials.json"), "{path:?}");
        assert_eq!(
            Some(path),
            dirs::home_dir().map(|h| h.join(".claude/.credentials.json"))
        );
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
    fn a_claude_that_does_not_answer_its_version_warns() {
        let dir = tempfile::tempdir().unwrap();
        let broken = dir.path().join("claude");
        executable(&broken); // an empty script: exits 0, prints nothing
        let check = check_claude(Some(&broken));
        assert_eq!(check.status, Status::Warn);
        assert!(check.detail.contains("did not answer"), "{check:?}");
        assert!(check.fix_hint.is_some());
    }

    /// A probe outcome: the handler exited `code`, printed `out`, and said
    /// `diag` about the chain.
    fn probe(code: Option<i32>, out: &str, diag: &str) -> Probe {
        Probe {
            timed_out: false,
            code,
            out: out.to_string(),
            diag: diag.to_string(),
            elapsed: Duration::ZERO,
        }
    }

    #[test]
    fn statusline_probe_grades_the_handler_then_the_chain() {
        // The handler itself must exit 0.
        let timeout = evaluate_statusline(
            Probe {
                timed_out: true,
                code: None,
                out: String::new(),
                diag: String::new(),
                elapsed: STATUSLINE_TIMEOUT,
            },
            "ccusage",
        );
        assert_eq!(timeout.status, Status::Fail);
        assert!(timeout.detail.contains("did not finish"), "{timeout:?}");
        assert!(timeout.fix_hint.is_some());

        let failed = evaluate_statusline(probe(Some(2), "", ""), "");
        assert_eq!(failed.status, Status::Fail);
        assert!(failed.detail.contains("exited 2"), "{failed:?}");
        assert_eq!(
            evaluate_statusline(probe(None, "", ""), "").detail,
            "`q hook statusline` exited a signal"
        );

        // No chain: nothing to echo, and that is fine.
        let none = evaluate_statusline(probe(Some(0), "", ""), "");
        assert_eq!(none.status, Status::Ok);
        assert_eq!(none.detail, "no chain configured");

        // A configured chain that says nothing is suspicious, not fatal.
        let silent = evaluate_statusline(probe(Some(0), "", ""), "ccusage");
        assert_eq!(silent.status, Status::Warn);
        assert!(silent.detail.contains("printed nothing"), "{silent:?}");

        let echoed = evaluate_statusline(probe(Some(0), "ctx 42%\nsecond", ""), "ccusage");
        assert_eq!(echoed.status, Status::Ok);
        assert_eq!(echoed.detail, "`ccusage` → ctx 42%");
    }

    #[test]
    fn a_chain_that_failed_inside_the_handler_is_reported() {
        // The handler still exits 0, so only the diagnostic tells us.
        let failed = evaluate_statusline(probe(Some(0), "", "exited 3"), "exit 3");
        assert_eq!(failed.status, Status::Warn);
        assert_eq!(failed.detail, "`exit 3` exited 3");
        assert!(failed.fix_hint.is_some());

        // A chain that both printed and failed is still reported as failing.
        let noisy = evaluate_statusline(probe(Some(0), "ctx", "exited 1: boom"), "ccusage");
        assert_eq!(noisy.status, Status::Warn);
        assert_eq!(noisy.detail, "`ccusage` exited 1: boom");
    }

    #[test]
    fn a_handler_slower_than_the_chain_budget_warns_even_when_it_succeeded() {
        let slow = evaluate_statusline(
            Probe {
                elapsed: hook::CHAIN_TIMEOUT + Duration::from_millis(500),
                ..probe(Some(0), "ctx 42%", "")
            },
            "ccusage",
        );
        assert_eq!(slow.status, Status::Warn);
        assert!(slow.detail.contains("longer than"), "{slow:?}");
        assert!(slow.fix_hint.is_some());

        // Inside the budget it is just a working statusline.
        let quick = evaluate_statusline(
            Probe {
                elapsed: Duration::from_millis(30),
                ..probe(Some(0), "ctx 42%", "")
            },
            "ccusage",
        );
        assert_eq!(quick.status, Status::Ok);
    }

    #[test]
    fn a_diagnostic_reaches_the_report_as_one_plain_line() {
        let dirty = evaluate_statusline(
            probe(
                Some(0),
                "",
                &("exited 1: \u{1b}[31m".to_string() + &"x".repeat(200) + "\u{1b}[0m\nand more"),
            ),
            "ccusage",
        );
        assert_eq!(dirty.status, Status::Warn);
        assert!(!dirty.detail.contains('\u{1b}'), "{dirty:?}");
        assert!(!dirty.detail.contains('\n'), "{dirty:?}");
        assert!(dirty.detail.chars().count() < 130, "{dirty:?}");
    }

    #[test]
    fn the_probe_budget_covers_the_handlers_own_chain_budget() {
        assert!(
            STATUSLINE_TIMEOUT > hook::CHAIN_TIMEOUT,
            "a chain the handler would accept must not be reported as a hang"
        );
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
