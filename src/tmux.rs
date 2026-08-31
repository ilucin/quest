//! tmux wrapper (SPEC §6). Everything goes through the `Tmux` trait so tests
//! run against `FixtureTmux` and never touch a real server.
//!
//! Consumed by the session commands in later milestones; M0 only ships the
//! module and the liveness sweep.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db::Db;
use crate::db::quest::QuestPatch;
use crate::error::QError;
use crate::model::{Session, SessionRole, SessionStatus, now};

/// Tab-separated so session and window names may contain spaces.
///
/// `pane_current_command` and `pane_current_path` are appended last so an older
/// `q`/tmux emitting only the first five fields still parses (SPEC §6): the
/// parser fills the two new fields with empty strings when they are absent.
const PANE_FORMAT: &str = "#{pane_id}\t#{pane_pid}\t#{session_name}\t#{window_name}\t\
     #{window_index}\t#{pane_current_command}\t#{pane_current_path}";

const TMUX_MISSING: &str = "tmux not found on PATH";

/// Per-session environment (`new-session -e`) arrived in tmux 3.2, and every
/// session `q` opens depends on it.
pub const MIN_TMUX: (u32, u32) = (3, 2);

/// `major.minor` out of a `tmux -V` string: `tmux 3.6b` → `(3, 6)`,
/// `tmux next-3.7` → `(3, 7)`. `None` when no number is in there at all.
pub fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let text = raw.trim();
    let digits = &text[text.find(|c: char| c.is_ascii_digit())?..];
    let (major, rest) = leading_number(digits)?;
    // A bare `3` is 3.0; the suffix in `3.2a` is not part of the number.
    let minor = rest
        .strip_prefix('.')
        .and_then(leading_number)
        .map_or(0, |(n, _)| n);
    Some((major, minor))
}

fn leading_number(s: &str) -> Option<(u32, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    Some((s[..end].parse().ok()?, &s[end..]))
}

/// One tmux pane. `pane_id` (`%42`) is a session's identity (SPEC §6) — it
/// survives rename, `/clear` and a Claude restart in the same pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub pane_id: String,
    pub pane_pid: i32,
    pub session_name: String,
    pub window_name: String,
    pub window_index: i32,
    /// The process running in the pane right now (`#{pane_current_command}`):
    /// a shell when no Claude is up, else Claude's own reported command — which
    /// is never literally `claude` (the native binary reports its version, npm
    /// reports `node`), so presence is judged with [`is_shell`], never `==`.
    /// Empty when read from an older 5-field pane line.
    pub current_command: String,
    /// The shell's cwd (`#{pane_current_path}`); the Quest cwd follows the
    /// main session's (SPEC §6). Empty when read from an older 5-field line.
    pub current_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewSession {
    pub name: String,
    pub window_name: String,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewWindow {
    pub session: String,
    pub window_name: String,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub command: Option<String>,
}

pub trait Tmux {
    fn list_panes(&self) -> anyhow::Result<Vec<Pane>>;
    /// Creates a detached session and returns its first pane.
    fn new_session(&self, spec: &NewSession) -> anyhow::Result<Pane>;
    fn new_window(&self, spec: &NewWindow) -> anyhow::Result<Pane>;
    /// Makes the window holding `pane_id` the active one in its own tmux
    /// session, without moving any client between sessions.
    fn select_window(&self, pane_id: &str) -> anyhow::Result<()>;
    /// Inside tmux this switches the client; outside it replaces the process
    /// with `tmux attach` and therefore does not return on success. `pane`
    /// selects its window first.
    fn attach(&self, session: &str, pane: Option<&str>) -> anyhow::Result<()>;
    /// [`Tmux::attach`] for a caller that needs its process back: outside tmux
    /// the attach runs as a *child* and returns when the client detaches,
    /// rather than replacing us. Inside tmux the two are the same thing — a
    /// `switch-client` returns either way. The TUI's `[ui] return_after_detach`
    /// is the only reason this exists.
    fn attach_child(&self, session: &str, pane: Option<&str>) -> anyhow::Result<()>;
    fn send_keys(&self, pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()>;
    /// Send a single named tmux key (e.g. `C-u`), interpreted rather than sent
    /// literally the way [`send_keys`](Tmux::send_keys) sends its text. Used to
    /// clear a pane's input line before typing a command that must start at
    /// column 0.
    fn send_key(&self, pane_id: &str, key: &str) -> anyhow::Result<()>;
    /// One bracketed paste, then Enter when asked — for text a TUI has to read
    /// as a single input even though it spans lines.
    fn paste(&self, pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()>;
    fn capture_pane(&self, pane_id: &str, lines: usize) -> anyhow::Result<String>;
    fn rename_session(&self, old: &str, new: &str) -> anyhow::Result<()>;
    fn rename_window(&self, pane_id: &str, new: &str) -> anyhow::Result<()>;
    fn kill_session(&self, name: &str) -> anyhow::Result<()>;
    /// Kills the window a pane belongs to, leaving the session alone.
    fn kill_window(&self, pane_id: &str) -> anyhow::Result<()>;
    fn has_session(&self, name: &str) -> anyhow::Result<bool>;
    fn in_tmux(&self) -> bool;
    fn version(&self) -> anyhow::Result<String>;
    /// Bind `key` (in the prefix table) server-wide to `run-shell <command>`.
    /// `q` sets it whenever a master comes up (the bind is idempotent), so any
    /// pane in a Quest can spawn a fresh worker (SPEC §6; `[tmux] spawn_key`).
    fn bind_key(&self, key: &str, command: &str) -> anyhow::Result<()>;

    /// The command `key` is currently bound to in the prefix table, or `None`
    /// when it is unbound. Used before [`bind_key`](Tmux::bind_key) so a master
    /// never silently clobbers the user's own prefix+key binding.
    fn prefix_binding(&self, key: &str) -> anyhow::Result<Option<String>>;
}

/// `FixtureTmux` when `$Q_FIXTURE` names a file, else the real thing.
pub fn tmux() -> Box<dyn Tmux> {
    match std::env::var_os("Q_FIXTURE") {
        Some(path) if !path.is_empty() => Box::new(FixtureTmux::new(PathBuf::from(path))),
        _ => Box::new(RealTmux),
    }
}

pub fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

// ---------------------------------------------------------------- arg building

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// tmux matches `-t` targets by prefix unless they start with `=`; without it
/// `q-a` would happily resolve to `q-alpha`. `pub` because the remote attach
/// (SPEC §15) builds the same target for a tmux on another machine.
pub fn exact(target: &str) -> String {
    format!("={target}")
}

fn args_list_panes() -> Vec<String> {
    args(&["list-panes", "-a", "-F", PANE_FORMAT])
}

fn args_display_pane(target: &str) -> Vec<String> {
    args(&["display-message", "-p", "-t", &exact(target), PANE_FORMAT])
}

fn args_bind_key(key: &str, command: &str) -> Vec<String> {
    // `-b` runs the shell command in the background: tmux is single-threaded,
    // so a foreground `run-shell` freezes the whole server (every session and
    // client) for the spawn's duration. `q spawn-here` selects its own window,
    // so nothing is lost by backgrounding it.
    args(&["bind-key", key, "run-shell", "-b", command])
}

fn args_list_keys(key: &str) -> Vec<String> {
    args(&["list-keys", "-T", "prefix", key])
}

fn args_new_session(spec: &NewSession) -> Vec<String> {
    let mut out = args(&[
        "new-session",
        "-d",
        "-s",
        &spec.name,
        "-P",
        "-F",
        PANE_FORMAT,
    ]);
    if !spec.window_name.is_empty() {
        out.extend(args(&["-n", &spec.window_name]));
    }
    if !spec.cwd.is_empty() {
        out.extend(args(&["-c", &spec.cwd]));
    }
    out.extend(env_args(&spec.env));
    if let Some(command) = &spec.command {
        out.push(command.clone());
    }
    out
}

/// `-d` keeps the caller's client where it is: which window becomes active is
/// the spawning command's decision, made with an explicit `select-window`.
fn args_new_window(spec: &NewWindow) -> Vec<String> {
    let mut out = args(&[
        "new-window",
        "-d",
        "-t",
        &exact(&format!("{}:", spec.session)),
        "-P",
        "-F",
        PANE_FORMAT,
    ]);
    if !spec.window_name.is_empty() {
        out.extend(args(&["-n", &spec.window_name]));
    }
    if !spec.cwd.is_empty() {
        out.extend(args(&["-c", &spec.cwd]));
    }
    out.extend(env_args(&spec.env));
    if let Some(command) = &spec.command {
        out.push(command.clone());
    }
    out
}

fn env_args(env: &[(String, String)]) -> Vec<String> {
    env.iter()
        .flat_map(|(k, v)| ["-e".to_string(), format!("{k}={v}")])
        .collect()
}

/// One or two invocations: the text is always sent literally (`-l`), so a
/// prompt containing `Enter` or `;` cannot be reinterpreted as a key name.
fn args_send_keys(pane_id: &str, text: &str, enter: bool) -> Vec<Vec<String>> {
    let mut out = vec![args(&["send-keys", "-t", pane_id, "-l", "--", text])];
    if enter {
        out.push(args_send_enter(pane_id));
    }
    out
}

fn args_send_enter(pane_id: &str) -> Vec<String> {
    args(&["send-keys", "-t", pane_id, "Enter"])
}

/// A single named key, sent *without* `-l` so tmux interprets it (`C-u`, not the
/// three literal characters). The key is a fixed constant at every call site, so
/// it can never be mistaken for pane text.
fn args_send_key(pane_id: &str, key: &str) -> Vec<String> {
    args(&["send-keys", "-t", pane_id, key])
}

/// A paste buffer of our own, per process: two concurrent `q send`s must not
/// consume each other's text.
fn send_buffer() -> String {
    format!("q-send-{}", std::process::id())
}

/// The text goes in as an argument rather than through stdin, exactly like
/// `send-keys -l`, so both paths share the same size ceiling and quoting.
fn args_set_buffer(buffer: &str, text: &str) -> Vec<String> {
    args(&["set-buffer", "-b", buffer, "--", text])
}

/// `-p` brackets the paste with `ESC[200~` / `ESC[201~` when the application in
/// the pane asked for bracketed paste — which is how a TUI tells a pasted
/// newline from a pressed Enter (verified against tmux 3.6b). Without the
/// request tmux sends the bytes bare, so this is never worse than `send-keys`.
/// `-d` drops the buffer once it is pasted.
fn args_paste_buffer(pane_id: &str, buffer: &str) -> Vec<String> {
    args(&["paste-buffer", "-p", "-d", "-b", buffer, "-t", pane_id])
}

fn args_delete_buffer(buffer: &str) -> Vec<String> {
    args(&["delete-buffer", "-b", buffer])
}

fn args_capture_pane(pane_id: &str, lines: usize) -> Vec<String> {
    args(&[
        "capture-pane",
        "-p",
        "-t",
        pane_id,
        "-S",
        &format!("-{lines}"),
    ])
}

fn args_rename_session(old: &str, new: &str) -> Vec<String> {
    args(&["rename-session", "-t", &exact(old), new])
}

/// Pane ids are already unambiguous, so they need no `=`.
fn args_rename_window(pane_id: &str, new: &str) -> Vec<String> {
    args(&["rename-window", "-t", pane_id, new])
}

fn args_kill_session(name: &str) -> Vec<String> {
    args(&["kill-session", "-t", &exact(name)])
}

fn args_kill_window(pane_id: &str) -> Vec<String> {
    args(&["kill-window", "-t", pane_id])
}

fn args_has_session(name: &str) -> Vec<String> {
    args(&["has-session", "-t", &exact(name)])
}

fn args_attach(session: &str) -> Vec<String> {
    args(&["attach", "-t", &exact(session)])
}

fn args_switch_client(session: &str) -> Vec<String> {
    args(&["switch-client", "-t", &exact(session)])
}

/// A pane id addresses its window, and it is the session's identity (SPEC §6):
/// window names are not persisted and a rename would break a name-based target.
fn args_select_window(pane_id: &str) -> Vec<String> {
    args(&["select-window", "-t", pane_id])
}

fn parse_pane(line: &str) -> Option<Pane> {
    let mut f = line.trim_end_matches('\r').split('\t');
    let pane = Pane {
        pane_id: f.next()?.to_string(),
        pane_pid: f.next()?.parse().ok()?,
        session_name: f.next()?.to_string(),
        window_name: f.next()?.to_string(),
        window_index: f.next()?.parse().ok()?,
        // Absent on an old 5-field line — empty, not a parse failure.
        current_command: f.next().unwrap_or("").to_string(),
        current_path: f.next().unwrap_or("").to_string(),
    };
    if pane.pane_id.is_empty() {
        return None;
    }
    Some(pane)
}

/// `%N` out of the first field of a pane line, for the cleanup path where the
/// rest of the line did not parse.
fn leading_pane_id(line: &str) -> Option<&str> {
    let id = line.split('\t').next()?;
    let n = id.strip_prefix('%')?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then_some(id)
}

fn parse_panes(stdout: &str) -> Vec<Pane> {
    stdout.lines().filter_map(parse_pane).collect()
}

/// The pane line of a `-P -F` or `display-message` run. tmux can print a
/// warning ahead of it, so the format output is the last line, not the whole
/// of stdout.
fn last_line(stdout: &str) -> &str {
    stdout.lines().next_back().unwrap_or("")
}

/// True when tmux failed only because no server is running — for a liveness
/// sweep that is indistinguishable from "no panes".
pub fn is_no_server(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("no server running")
}

/// True when tmux refused only because the session has no client to move —
/// the normal state of a Quest started with `-d` or since detached.
pub fn is_no_client(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("no current client")
}

/// An empty target is not "the current pane" as far as q is concerned: it means
/// a session row whose window never opened, and tmux would silently act on
/// whatever is active instead — which, when q runs inside tmux, is q's own
/// window. Every call that takes a pane id as a `-t` target goes through here,
/// in both implementations, so the hazard cannot be reintroduced by a new
/// caller forgetting to check.
fn require_pane_id(pane_id: &str) -> anyhow::Result<()> {
    // Whitespace is no more of a target than "" is: tmux trims the `-t`
    // argument and lands on the current pane just the same.
    if pane_id.trim().is_empty() {
        return Err(QError::Tmux("no pane to select (empty pane id)".to_string()).into());
    }
    Ok(())
}

// --------------------------------------------------------------------- real

pub struct RealTmux;

fn spawn(argv: &[String]) -> anyhow::Result<std::process::Output> {
    Command::new("tmux").args(argv).output().map_err(|e| {
        match e.kind() {
            std::io::ErrorKind::NotFound => QError::Tmux(TMUX_MISSING.to_string()),
            _ => QError::Tmux(format!("cannot run tmux: {e}")),
        }
        .into()
    })
}

fn run(argv: &[String]) -> anyhow::Result<String> {
    let out = spawn(argv)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(QError::Tmux(format!("`tmux {}` failed: {stderr}", argv.join(" "))).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Exit status as a boolean; only a missing binary is an error.
fn run_bool(argv: &[String]) -> anyhow::Result<bool> {
    Ok(spawn(argv)?.status.success())
}

impl RealTmux {
    fn pane_at(&self, target: &str) -> anyhow::Result<Pane> {
        let out = run(&args_display_pane(target))?;
        parse_pane(last_line(&out))
            .ok_or_else(|| QError::Tmux(format!("cannot read pane of `{target}`")).into())
    }

    /// The tmux session this process's own pane sits in, when it has one.
    /// `$TMUX_PANE` is the only thing that survives a shell inside tmux.
    fn current_session(&self) -> Option<String> {
        let pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())?;
        self.list_panes()
            .ok()?
            .into_iter()
            .find_map(|p| (p.pane_id == pane).then_some(p.session_name))
    }

    /// The half both attach paths share: select the target window, and, when
    /// we are already inside tmux, move the client there. `Ok(true)` means
    /// that was the whole job and there is nothing left to `exec` or spawn.
    fn switch_if_inside(&self, session: &str, pane: Option<&str>) -> anyhow::Result<bool> {
        if let Some(p) = pane {
            self.select_window(p)?;
        }
        if !in_tmux() {
            return Ok(false);
        }
        // Already inside the target session: the window is selected and there
        // is no client to move. `switch-client` would either be a no-op or
        // fail on a session nobody is attached to.
        if self.current_session().as_deref() == Some(session) {
            return Ok(true);
        }
        match run(&args_switch_client(session)) {
            Ok(_) => Ok(true),
            // A detached session (`q new -d`, or the user let go of it) has no
            // client to switch. The window is selected, so the next real
            // attach lands on it — not worth failing the command over.
            Err(e) if is_no_client(&e) => {
                eprintln!("warning: {e:#}");
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }
}

/// The two ways spawning `tmux attach` can fail before tmux itself runs.
fn attach_failed(session: &str, e: &std::io::Error) -> QError {
    match e.kind() {
        std::io::ErrorKind::NotFound => QError::Tmux(TMUX_MISSING.to_string()),
        _ => QError::Tmux(format!("cannot attach to `{session}`: {e}")),
    }
}

impl Tmux for RealTmux {
    fn list_panes(&self) -> anyhow::Result<Vec<Pane>> {
        Ok(parse_panes(&run(&args_list_panes())?))
    }

    fn new_session(&self, spec: &NewSession) -> anyhow::Result<Pane> {
        let out = run(&args_new_session(spec))?;
        let line = last_line(&out);
        parse_pane(line).ok_or_else(|| {
            // The session is up, but without its pane id nothing can address
            // it; a stray Claude is worse than no session at all.
            let _ = run(&args_kill_session(&spec.name));
            QError::Tmux(format!("cannot read the new session's pane: `{line}`")).into()
        })
    }

    fn new_window(&self, spec: &NewWindow) -> anyhow::Result<Pane> {
        let out = run(&args_new_window(spec))?;
        let line = last_line(&out);
        parse_pane(line).ok_or_else(|| {
            // Same as above, but only this window is ours to take down. The
            // leading field is the pane id even when the rest is unreadable.
            if let Some(id) = leading_pane_id(line) {
                let _ = run(&args_kill_window(id));
            }
            QError::Tmux(format!("cannot read the new window's pane: `{line}`")).into()
        })
    }

    fn select_window(&self, pane_id: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        run(&args_select_window(pane_id)).map(|_| ())
    }

    fn attach(&self, session: &str, pane: Option<&str>) -> anyhow::Result<()> {
        if self.switch_if_inside(session, pane)? {
            return Ok(());
        }
        use std::os::unix::process::CommandExt;
        let e = Command::new("tmux").args(args_attach(session)).exec();
        Err(attach_failed(session, &e).into())
    }

    fn attach_child(&self, session: &str, pane: Option<&str>) -> anyhow::Result<()> {
        if self.switch_if_inside(session, pane)? {
            return Ok(());
        }
        // Inherited stdio on purpose: `tmux attach` *is* the terminal until it
        // detaches, and this call blocks for exactly that long.
        let status = Command::new("tmux")
            .args(args_attach(session))
            .status()
            .map_err(|e| attach_failed(session, &e))?;
        if !status.success() {
            return Err(
                QError::Tmux(format!("`tmux attach` on `{session}` exited with {status}")).into(),
            );
        }
        Ok(())
    }

    fn send_keys(&self, pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        for argv in args_send_keys(pane_id, text, enter) {
            run(&argv)?;
        }
        Ok(())
    }

    fn send_key(&self, pane_id: &str, key: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        run(&args_send_key(pane_id, key)).map(|_| ())
    }

    fn paste(&self, pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        let buffer = send_buffer();
        run(&args_set_buffer(&buffer, text))?;
        if let Err(e) = run(&args_paste_buffer(pane_id, &buffer)) {
            // `-d` never ran, so the buffer would outlive the failure.
            let _ = run(&args_delete_buffer(&buffer));
            return Err(e);
        }
        if enter {
            run(&args_send_enter(pane_id))?;
        }
        Ok(())
    }

    fn capture_pane(&self, pane_id: &str, lines: usize) -> anyhow::Result<String> {
        require_pane_id(pane_id)?;
        Ok(tail(&run(&args_capture_pane(pane_id, lines))?, lines))
    }

    fn rename_session(&self, old: &str, new: &str) -> anyhow::Result<()> {
        run(&args_rename_session(old, new)).map(|_| ())
    }

    fn rename_window(&self, pane_id: &str, new: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        run(&args_rename_window(pane_id, new)).map(|_| ())
    }

    fn kill_session(&self, name: &str) -> anyhow::Result<()> {
        run(&args_kill_session(name)).map(|_| ())
    }

    fn kill_window(&self, pane_id: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        run(&args_kill_window(pane_id)).map(|_| ())
    }

    fn has_session(&self, name: &str) -> anyhow::Result<bool> {
        run_bool(&args_has_session(name))
    }

    fn in_tmux(&self) -> bool {
        in_tmux()
    }

    fn version(&self) -> anyhow::Result<String> {
        Ok(run(&args(&["-V"]))?.trim().to_string())
    }

    fn bind_key(&self, key: &str, command: &str) -> anyhow::Result<()> {
        run(&args_bind_key(key, command)).map(|_| ())
    }

    fn prefix_binding(&self, key: &str) -> anyhow::Result<Option<String>> {
        // An unbound key makes `list-keys` exit non-zero ("unknown key"); that
        // is not a failure worth surfacing — treat it as simply unbound.
        match run(&args_list_keys(key)) {
            Ok(out) => Ok(parse_prefix_binding(&out)),
            Err(_) => Ok(None),
        }
    }
}

/// The command half of a `list-keys -T prefix <key>` line, e.g.
/// `bind-key -T prefix N run-shell -b "…"` → `run-shell -b "…"`. tmux prints
/// one line; anything we cannot split past the key token counts as unbound.
fn parse_prefix_binding(out: &str) -> Option<String> {
    let line = out.lines().next()?.trim();
    // `bind-key [-r] [-N note] -T prefix <key> <command…>` — the command is
    // whatever follows the `-T prefix <key>` triple.
    let rest = line.split(" -T ").nth(1)?; // "prefix N <command…>"
    let mut it = rest.splitn(3, ' '); // "prefix", key, command
    it.next()?; // table
    it.next()?; // key
    it.next()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
}

// ------------------------------------------------------------------ fixture

/// A fake tmux persisted as JSON at `$Q_FIXTURE`, so state survives across the
/// CLI invocations of an integration test. The file is plain and every field
/// defaults, so tests can pre-seed or edit it by hand.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixtureState {
    #[serde(default)]
    pub next_pane: u32,
    #[serde(default)]
    pub panes: Vec<FixturePane>,
    /// Overrides what `version()` answers, so a test can pose as an old tmux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// The `(session, pane)` of the last `attach`, so tests can assert on it.
    #[serde(default)]
    pub attached: Option<(String, Option<String>)>,
    /// The pane whose window was last selected — by `select_window` or by the
    /// `attach` that precedes it.
    #[serde(default)]
    pub selected: Option<String>,
    /// How the last attach would have been carried out by the real thing:
    /// `switch` inside tmux, else `exec` (`q enter`, which never returns) or
    /// `child` (the TUI's `[ui] return_after_detach`, which does). Recorded
    /// because a fixture cannot imitate any of the three — it never replaces
    /// or suspends its own process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_mode: Option<String>,
    /// Poses as being inside (or outside) tmux, so a test need not write to
    /// `$TMUX` — process-global state another test may be changing. `None`
    /// falls back to the environment, like the real thing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_tmux: Option<bool>,
    /// Makes `new_window` fail with this message until a test clears it, so the
    /// caller's cleanup path is reachable. Real tmux fails for its own reasons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_new_window: Option<String>,
    /// The same for `new_session` — the step `q new` rolls the whole Quest back
    /// from when it fails (SPEC §5): the row is deleted and the epic closed,
    /// and there is no other way to reach that path from a test.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_new_session: Option<String>,
    /// The same for `rename_session`, which is how a rename fails once the
    /// slug itself has been checked (SPEC §10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_rename_session: Option<String>,
    /// Prefix-table key bindings, so a test can pre-seed a user's own binding
    /// and assert that a master leaves it alone (SPEC §6). `bind_key` records
    /// here; `prefix_binding` reads it back.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub prefix_keys: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FixturePane {
    pub pane_id: String,
    #[serde(default)]
    pub pane_pid: i32,
    #[serde(default)]
    pub session_name: String,
    #[serde(default)]
    pub window_name: String,
    #[serde(default)]
    pub window_index: i32,
    /// What `-c` asked for; real tmux keeps no such field, but a test has no
    /// other way to see the directory a window was opened in.
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub command: Option<String>,
    /// `#{pane_current_command}` — what is running in the pane now. `add` seeds
    /// it from the launch command (a login shell reports `zsh`, a `claude …`
    /// launch reports a version string); `send_keys` of `claude …` / `/exit`
    /// flips it. `#[serde(default)]` keeps a pre-v5 fixture JSON (no such field)
    /// loading — it just reports an empty command until something sets one.
    #[serde(default)]
    pub current_command: String,
    /// `#{pane_current_path}` — the pane's cwd. Seeded from `-c <cwd>`.
    #[serde(default)]
    pub current_path: String,
    /// What `send_keys` wrote and `capture_pane` reads back.
    #[serde(default)]
    pub buffer: String,
    /// Every `paste` in order, so a test can tell a bracketed paste from typed
    /// keys — the wire difference is invisible in `buffer`.
    #[serde(default)]
    pub pastes: Vec<String>,
}

impl FixturePane {
    fn as_pane(&self) -> Pane {
        Pane {
            pane_id: self.pane_id.clone(),
            pane_pid: self.pane_pid,
            session_name: self.session_name.clone(),
            window_name: self.window_name.clone(),
            window_index: self.window_index,
            current_command: self.current_command.clone(),
            current_path: self.current_path.clone(),
        }
    }
}

impl FixtureState {
    fn add(
        &mut self,
        session: &str,
        window: &str,
        index: i32,
        cwd: &str,
        spec_env: &[(String, String)],
        command: Option<&str>,
    ) -> Pane {
        let n = self.claim_pane_number();
        let pane = FixturePane {
            pane_id: format!("%{n}"),
            pane_pid: 90000 + n as i32,
            session_name: session.to_string(),
            window_name: window.to_string(),
            window_index: index,
            cwd: cwd.to_string(),
            env: spec_env.iter().cloned().collect(),
            command: command.map(str::to_string),
            current_command: launched_command(command),
            current_path: cwd.to_string(),
            buffer: String::new(),
            pastes: Vec::new(),
        };
        let out = pane.as_pane();
        self.panes.push(pane);
        out
    }

    /// A pane number no live pane holds. Real tmux never hands out an id twice
    /// while the pane lives, and pane ids are a session's identity (SPEC §6),
    /// so a hand-seeded fixture whose `next_pane` lags its panes — or omits the
    /// field entirely — must not mint a duplicate.
    fn claim_pane_number(&mut self) -> u32 {
        let highest = self
            .panes
            .iter()
            .filter_map(|p| p.pane_id.strip_prefix('%'))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        self.next_pane = self.next_pane.max(highest) + 1;
        self.next_pane
    }

    fn pane_mut(&mut self, pane_id: &str) -> anyhow::Result<&mut FixturePane> {
        self.panes
            .iter_mut()
            .find(|p| p.pane_id == pane_id)
            .ok_or_else(|| QError::Tmux(format!("can't find pane: {pane_id}")).into())
    }
}

/// Shaped like a real `tmux -V` so the version check parses it as any other.
const FIXTURE_VERSION: &str = "tmux 3.6 (fixture)";

/// The `current_command` the fixture reports for a pane whose login shell is
/// idle. A real macOS pane reports `zsh`; `-zsh` (a login shell) is handled by
/// [`is_shell`] stripping the leading `-`.
const FIXTURE_SHELL: &str = "zsh";

/// What the fixture reports while Claude runs in a pane: an opaque, non-shell
/// version string, mimicking the native macOS binary — deliberately *not*
/// `claude`, so nothing may test presence with `current_command == "claude"`.
const FIXTURE_CLAUDE_COMMAND: &str = "2.1.0";

/// A `send-keys`/launch line that starts Claude. The real command is
/// `claude -n …`, `claude --resume …` or `claude -n … -- "$(q prompt …)"` —
/// always `claude` followed by a flag (or nothing). A plain-text line that
/// merely begins with the word "claude" ("claude keeps crashing") is not a
/// launch, so a prompt typed with `q send` cannot masquerade as one. `q start`
/// always injects a flag first, so its command is always detected.
fn is_claude_launch(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    if tokens.next() != Some("claude") {
        return false;
    }
    match tokens.next() {
        None => true,
        Some(next) => next.starts_with('-'),
    }
}

/// The `current_command` a fixture pane reports right after it is opened with
/// `command`. A login shell (or `None`) reports a shell; a `claude …` launch
/// reports the opaque version string; anything else reports its own leading
/// token (a non-shell, e.g. `vim`).
fn launched_command(command: Option<&str>) -> String {
    match command {
        None => FIXTURE_SHELL.to_string(),
        Some(line) if is_claude_launch(line) => FIXTURE_CLAUDE_COMMAND.to_string(),
        Some(line) => line
            .split_whitespace()
            .next()
            .unwrap_or(FIXTURE_SHELL)
            .to_string(),
    }
}

pub struct FixtureTmux {
    path: PathBuf,
}

impl FixtureTmux {
    pub fn new(path: impl Into<PathBuf>) -> FixtureTmux {
        FixtureTmux { path: path.into() }
    }

    pub fn load(&self) -> anyhow::Result<FixtureState> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(FixtureState::default()),
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                QError::Tmux(format!("bad fixture {}: {e}", self.path.display())).into()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FixtureState::default()),
            Err(e) => Err(QError::Tmux(format!("cannot read {}: {e}", self.path.display())).into()),
        }
    }

    pub fn save(&self, state: &FixtureState) -> anyhow::Result<()> {
        write_json(&self.path, state)
    }

    /// Serialised against concurrent `q` processes sharing the fixture file.
    fn edit<T>(&self, f: impl FnOnce(&mut FixtureState) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let _lock = Lock::acquire(&self.path)?;
        let mut state = self.load()?;
        let out = f(&mut state)?;
        self.save(&state)?;
        Ok(out)
    }

    /// Both attaches, which differ only in what they do to the *caller's*
    /// process — nothing a fixture can imitate, so it records which was asked
    /// for instead.
    fn record_attach(&self, session: &str, pane: Option<&str>, child: bool) -> anyhow::Result<()> {
        let mode = if self.in_tmux() {
            "switch"
        } else if child {
            "child"
        } else {
            "exec"
        };
        self.edit(|state| {
            if !state.panes.iter().any(|p| p.session_name == session) {
                return Err(QError::Tmux(format!("can't find session: {session}")).into());
            }
            if let Some(id) = pane {
                // Real tmux would switch the client to wherever the pane lives;
                // asking for one outside `session` is a bug in the caller.
                if state.pane_mut(id)?.session_name != session {
                    return Err(
                        QError::Tmux(format!("pane {id} is not in session {session}")).into(),
                    );
                }
                state.selected = Some(id.to_string());
            }
            state.attached = Some((session.to_string(), pane.map(str::to_string)));
            state.attach_mode = Some(mode.to_string());
            Ok(())
        })
    }
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn ensure_parent(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| QError::Tmux(format!("cannot create {}: {e}", parent.display())))?;
    }
    Ok(())
}

/// An advisory lock held for the whole load → mutate → save cycle. Released on
/// drop, so an error inside the cycle cannot strand it.
struct Lock(PathBuf);

const LOCK_ATTEMPTS: u32 = 500;
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(10);

impl Lock {
    fn acquire(path: &Path) -> anyhow::Result<Lock> {
        ensure_parent(path)?;
        let lock = suffixed(path, ".lock");
        for _ in 0..LOCK_ATTEMPTS {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock)
            {
                Ok(_) => return Ok(Lock(lock)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(LOCK_WAIT)
                }
                Err(e) => {
                    return Err(QError::Tmux(format!("cannot lock {}: {e}", lock.display())).into());
                }
            }
        }
        Err(QError::Tmux(format!("fixture lock is stuck: {}", lock.display())).into())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Written to a sibling temp file and renamed, so a reader never sees a
/// half-written fixture.
fn write_json(path: &Path, state: &FixtureState) -> anyhow::Result<()> {
    ensure_parent(path)?;
    let text = serde_json::to_string_pretty(state)?;
    let tmp = suffixed(path, ".tmp");
    std::fs::write(&tmp, text)
        .map_err(|e| QError::Tmux(format!("cannot write {}: {e}", tmp.display())))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| QError::Tmux(format!("cannot write {}: {e}", path.display())).into())
}

impl Tmux for FixtureTmux {
    fn list_panes(&self) -> anyhow::Result<Vec<Pane>> {
        Ok(self
            .load()?
            .panes
            .iter()
            .map(FixturePane::as_pane)
            .collect())
    }

    fn new_session(&self, spec: &NewSession) -> anyhow::Result<Pane> {
        self.edit(|state| {
            if let Some(msg) = &state.fail_new_session {
                return Err(QError::Tmux(msg.clone()).into());
            }
            if state.panes.iter().any(|p| p.session_name == spec.name) {
                return Err(QError::Tmux(format!("duplicate session: {}", spec.name)).into());
            }
            Ok(state.add(
                &spec.name,
                &spec.window_name,
                0,
                &spec.cwd,
                &spec.env,
                spec.command.as_deref(),
            ))
        })
    }

    fn new_window(&self, spec: &NewWindow) -> anyhow::Result<Pane> {
        self.edit(|state| {
            if let Some(msg) = &state.fail_new_window {
                return Err(QError::Tmux(msg.clone()).into());
            }
            let index = state
                .panes
                .iter()
                .filter(|p| p.session_name == spec.session)
                .map(|p| p.window_index)
                .max()
                .ok_or_else(|| QError::Tmux(format!("can't find session: {}", spec.session)))?;
            Ok(state.add(
                &spec.session,
                &spec.window_name,
                index + 1,
                &spec.cwd,
                &spec.env,
                spec.command.as_deref(),
            ))
        })
    }

    fn select_window(&self, pane_id: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        self.edit(|state| {
            state.pane_mut(pane_id)?;
            state.selected = Some(pane_id.to_string());
            Ok(())
        })
    }

    fn attach(&self, session: &str, pane: Option<&str>) -> anyhow::Result<()> {
        self.record_attach(session, pane, false)
    }

    fn attach_child(&self, session: &str, pane: Option<&str>) -> anyhow::Result<()> {
        self.record_attach(session, pane, true)
    }

    fn send_keys(&self, pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        self.edit(|state| {
            let pane = state.pane_mut(pane_id)?;
            pane.buffer.push_str(text);
            if enter {
                pane.buffer.push('\n');
            }
            // `q start` types `claude …`, `q stop` types `/exit`: the fixture
            // reflects the running process so a sweep can read presence off it.
            let line = text.trim();
            if is_claude_launch(line) {
                pane.current_command = FIXTURE_CLAUDE_COMMAND.to_string();
            } else if line == "/exit" {
                pane.current_command = FIXTURE_SHELL.to_string();
            }
            Ok(())
        })
    }

    fn send_key(&self, pane_id: &str, key: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        self.edit(|state| {
            let pane = state.pane_mut(pane_id)?;
            // `C-u` kills the current (unsubmitted) input line: everything typed
            // since the last Enter. Model it by truncating back to the last
            // newline, so a `q stop` after stray typing lands `/exit` at col 0.
            if key == "C-u" {
                let keep = pane.buffer.rfind('\n').map(|i| i + 1).unwrap_or(0);
                pane.buffer.truncate(keep);
            }
            Ok(())
        })
    }

    fn paste(&self, pane_id: &str, text: &str, enter: bool) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        self.edit(|state| {
            let pane = state.pane_mut(pane_id)?;
            pane.pastes.push(text.to_string());
            pane.buffer.push_str(text);
            if enter {
                pane.buffer.push('\n');
            }
            Ok(())
        })
    }

    fn capture_pane(&self, pane_id: &str, lines: usize) -> anyhow::Result<String> {
        require_pane_id(pane_id)?;
        let mut state = self.load()?;
        let buffer = state.pane_mut(pane_id)?.buffer.clone();
        Ok(tail(&buffer, lines))
    }

    fn rename_session(&self, old: &str, new: &str) -> anyhow::Result<()> {
        self.edit(|state| {
            if let Some(msg) = &state.fail_rename_session {
                return Err(QError::Tmux(msg.clone()).into());
            }
            let mut found = false;
            for pane in state.panes.iter_mut().filter(|p| p.session_name == old) {
                pane.session_name = new.to_string();
                found = true;
            }
            if found {
                Ok(())
            } else {
                Err(QError::Tmux(format!("can't find session: {old}")).into())
            }
        })
    }

    fn rename_window(&self, pane_id: &str, new: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        self.edit(|state| {
            state.pane_mut(pane_id)?.window_name = new.to_string();
            Ok(())
        })
    }

    fn kill_session(&self, name: &str) -> anyhow::Result<()> {
        self.edit(|state| {
            let before = state.panes.len();
            state.panes.retain(|p| p.session_name != name);
            if state.panes.len() == before {
                return Err(QError::Tmux(format!("can't find session: {name}")).into());
            }
            Ok(())
        })
    }

    fn kill_window(&self, pane_id: &str) -> anyhow::Result<()> {
        require_pane_id(pane_id)?;
        self.edit(|state| {
            let before = state.panes.len();
            state.panes.retain(|p| p.pane_id != pane_id);
            if state.panes.len() == before {
                return Err(QError::Tmux(format!("can't find pane: {pane_id}")).into());
            }
            Ok(())
        })
    }

    fn has_session(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self.load()?.panes.iter().any(|p| p.session_name == name))
    }

    fn in_tmux(&self) -> bool {
        self.load()
            .ok()
            .and_then(|s| s.in_tmux)
            .unwrap_or_else(in_tmux)
    }

    fn version(&self) -> anyhow::Result<String> {
        Ok(self
            .load()?
            .version
            .unwrap_or_else(|| FIXTURE_VERSION.to_string()))
    }

    fn bind_key(&self, key: &str, command: &str) -> anyhow::Result<()> {
        self.edit(|state| {
            state
                .prefix_keys
                .insert(key.to_string(), command.to_string());
            Ok(())
        })
    }

    fn prefix_binding(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.load()?.prefix_keys.get(key).cloned())
    }
}

/// The last `lines` lines with trailing blank lines dropped. `capture-pane -S
/// -<lines>` counts back from the *history* start, so it can return more than
/// `lines` and pads the pane's unused rows with blanks.
fn tail(buffer: &str, lines: usize) -> String {
    let mut all: Vec<&str> = buffer.lines().collect();
    while all.last().is_some_and(|l| l.trim().is_empty()) {
        all.pop();
    }
    if lines > 0 && all.len() > lines {
        all.drain(..all.len() - lines);
    }
    all.join("\n")
}

// ------------------------------------------------------------------ helpers

/// `q-<slug>` — the main tmux session of a Quest (SPEC §6).
pub fn session_name(config: &Config, slug: &str) -> String {
    format!("{}{slug}", config.tmux.session_prefix)
}

/// Separates a Quest slug from a worker label in a tmux session name. Never a
/// kebab character, so the split back to `(slug, label)` is unambiguous even
/// for a slug that itself contains `-` (SPEC §6).
pub const WORKER_SEP: char = '+';

/// The shells a login pane may report as `pane_current_command`. A leading `-`
/// (a login shell, e.g. `-zsh`) is stripped before the lookup, so it is not
/// listed twice. Presence is one-way: a shell means no Claude (`off`); a
/// non-shell is never proof Claude *is* up — that comes only from hooks.
pub const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "nu", "tcsh", "csh",
];

/// Whether `command` is a login shell — the pane is idle, no Claude running.
/// Never test `command == "claude"`: the native binary reports its version and
/// npm reports `node`, so presence is the *absence* of a shell, plus a hook.
pub fn is_shell(command: &str) -> bool {
    let name = command.trim().strip_prefix('-').unwrap_or(command.trim());
    // A shell may report a path (`/bin/zsh`); take the final component.
    let name = name.rsplit('/').next().unwrap_or(name);
    SHELLS.contains(&name)
}

/// `q-<slug>+<label>` — a worker's own tmux session (SPEC §6).
pub fn worker_session_name(config: &Config, slug: &str, label: &str) -> String {
    format!("{}{WORKER_SEP}{label}", session_name(config, slug))
}

/// The distinct tmux session names belonging to Quest `slug`, in first-seen
/// order: the main `q-<slug>` and every worker `q-<slug>+<label>`. An exact
/// match or a `+`-prefixed one only — never a bare `starts_with`, so `q-foo`
/// never claims a sibling Quest `q-foo-bar`'s sessions (SPEC §6).
pub fn sessions_of_quest(config: &Config, panes: &[Pane], slug: &str) -> Vec<String> {
    let main = session_name(config, slug);
    let worker_prefix = format!("{main}{WORKER_SEP}");
    let mut out: Vec<String> = Vec::new();
    for pane in panes {
        let name = &pane.session_name;
        let ours = *name == main || name.starts_with(&worker_prefix);
        if ours && !out.iter().any(|n| n == name) {
            out.push(name.clone());
        }
    }
    out
}

/// The Quest slug owning a tmux session name: strip the configured prefix, then
/// keep everything before the first `+` (SPEC §6). `None` when the name does
/// not carry the prefix, or nothing is left of the slug. The `+`-split is what
/// keeps a worker `q-foo+review` reporting `foo`, and `q-foo-bar` reporting
/// `foo-bar` rather than being mistaken for `foo`.
pub fn quest_slug_of_name(config: &Config, session_name: &str) -> Option<String> {
    let rest = session_name.strip_prefix(config.tmux.session_prefix.as_str())?;
    let slug = rest.split_once(WORKER_SEP).map_or(rest, |(slug, _)| slug);
    (!slug.is_empty()).then(|| slug.to_string())
}

/// The Quest slug owning `pane`, read off its tmux session name (SPEC §6).
pub fn quest_slug_of_pane(config: &Config, pane: &Pane) -> Option<String> {
    quest_slug_of_name(config, &pane.session_name)
}

/// The environment `q` sets on a window; Claude and its hooks inherit it
/// (SPEC §7). `Q_DB` and `Q_CONFIG` are passed on only when this process itself
/// runs on an override — otherwise the child would resolve them differently.
pub fn quest_env(
    quest_id: &str,
    session_id: &str,
    role: SessionRole,
    machine: &str,
    db_path_override: Option<&str>,
    config_path_override: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = vec![
        ("Q_QUEST".to_string(), quest_id.to_string()),
        ("Q_SESSION".to_string(), session_id.to_string()),
        ("Q_ROLE".to_string(), role.as_str().to_string()),
        ("Q_MACHINE".to_string(), machine.to_string()),
    ];
    if let Some(db) = db_path_override.filter(|d| !d.is_empty()) {
        env.push(("Q_DB".to_string(), db.to_string()));
    }
    if let Some(config) = config_path_override.filter(|c| !c.is_empty()) {
        env.push(("Q_CONFIG".to_string(), config.to_string()));
    }
    env
}

/// `$Q_DB` if the caller set one, for `quest_env`.
pub fn db_override() -> Option<String> {
    std::env::var("Q_DB")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| absolutize(&v))
}

/// `$Q_CONFIG` if the caller set one, for `quest_env`.
pub fn config_override() -> Option<String> {
    std::env::var("Q_CONFIG")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| absolutize(&v))
}

/// The window runs in the Quest's cwd, so a relative override would resolve
/// against the wrong directory there. Purely lexical — the path need not exist.
fn absolutize(value: &str) -> String {
    std::path::absolute(value)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| value.to_string())
}

/// Every pane tmux knows about. No server means no panes; a missing binary is
/// a real failure.
pub fn live_panes(tmux: &dyn Tmux) -> anyhow::Result<Vec<Pane>> {
    match tmux.list_panes() {
        Ok(panes) => Ok(panes),
        Err(e) if is_no_server(&e) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Why a live row is being ended, for the `session.end` payload.
pub const PANE_GONE: &str = "pane_gone";
pub const NEVER_STARTED: &str = "never_started";

/// How long a row whose pane is not filled in yet is left alone. A spawn
/// inserts the row before it opens the window (the `SessionStart` hook has to
/// find it), so there is always a moment with no pane; if q dies in that
/// moment nothing will ever fill it, and without the age-out the row stays
/// live forever — burning its label and bouncing `q enter`/`q resume` off each
/// other. Longer than tmux needs to open a window, short enough to self-heal.
pub const START_GRACE_SECS: i64 = 10;

/// How long a `starting` row is left alone before the presence sweep, seeing a
/// shell in its pane, demotes it to `off`. Longer than [`START_GRACE_SECS`]
/// because this is not "tmux opened a window" but "Claude finished booting": a
/// cold `claude`/npm/node on a loaded machine can read as a shell well past
/// ten seconds, and demoting it to `off` invites a spurious `q start` on a
/// session that is in fact coming up (correctness review #3). `q start`'s own
/// launch types the command, so a genuine crash still surfaces once the row is
/// reported by a hook or the pane truly goes.
pub const BOOT_GRACE_SECS: i64 = 30;

/// An orphaned session row and why it counts as one.
#[derive(Debug)]
pub struct Orphan {
    pub session: Session,
    pub reason: &'static str,
}

/// The sessions among `sessions` that no longer have a live pane. Keyed on the
/// `(tmux_session, pane)` pair: tmux recycles pane ids, so `%1` in another
/// tmux session is not this session's pane. A row with no pane at all is only
/// an orphan once `START_GRACE_SECS` have passed since it started. Shared by
/// `sweep` and `q doctor`.
pub fn find_orphans(sessions: Vec<Session>, panes: &[Pane], now: i64) -> Vec<Orphan> {
    let alive: HashSet<(&str, &str)> = panes
        .iter()
        .map(|p| (p.session_name.as_str(), p.pane_id.as_str()))
        .collect();
    sessions
        .into_iter()
        .filter_map(|s| {
            if s.tmux_pane.is_empty() {
                let stale = now - s.started_at > START_GRACE_SECS;
                return stale.then_some(Orphan {
                    session: s,
                    reason: NEVER_STARTED,
                });
            }
            let gone = !alive.contains(&(s.tmux_session.as_str(), s.tmux_pane.as_str()));
            gone.then_some(Orphan {
                session: s,
                reason: PANE_GONE,
            })
        })
        .collect()
}

/// The name of the window a pane sits in. Display only — window names are not
/// an address (SPEC §6), and a vanished pane simply has none.
pub fn window_of(tmux: &dyn Tmux, pane_id: &str) -> Option<String> {
    tmux.list_panes()
        .ok()?
        .into_iter()
        .find_map(|p| (p.pane_id == pane_id && !p.window_name.is_empty()).then_some(p.window_name))
}

/// Whether a row's status still claims Claude is up, so a shell in its pane is
/// news (a demotion to `off`). `off` is already there; `ended` is terminal.
fn claims_claude(status: SessionStatus) -> bool {
    matches!(
        status,
        SessionStatus::Starting
            | SessionStatus::Busy
            | SessionStatus::Idle
            | SessionStatus::Waiting
    )
}

/// Liveness + presence (SPEC §6). Two one-way transitions, applied in this
/// order to the live rows:
///
/// * pane gone → `ended` (an orphan), with a `session.end` event;
/// * pane alive but reporting a shell while the row still claims Claude → `off`
///   (Ctrl-C, `/exit`, a crash), with a `session.off` event. Never the reverse:
///   a non-shell pane is not proof Claude is up — that comes only from hooks.
///   A `starting` row is left alone until `BOOT_GRACE_SECS` after `q start`
///   typed its command (`claude_started_at`, else the row's own age), because
///   tmux execs the login shell before Claude, so the pane reads as a shell
///   while a cold `claude`/npm/node boots.
///
/// Returns every row it changed, ended or demoted. Rows whose only change is a
/// reseeded `last_pane_path` (cwd-follow) are *not* returned — that bookkeeping
/// must not read as an ended session to `q watch`'s notifier.
pub fn sweep(db: &Db, tmux: &dyn Tmux, config: &Config) -> anyhow::Result<Vec<Session>> {
    let live = db.list_live_sessions()?;
    if live.is_empty() {
        return Ok(Vec::new());
    }
    let panes = live_panes(tmux)?;

    let ts = now();
    let mut changed = Vec::new();

    let orphans = find_orphans(live.clone(), &panes, ts);
    let orphaned: HashSet<&str> = orphans.iter().map(|o| o.session.id.as_str()).collect();
    for orphan in &orphans {
        let row = db.mark_session_ended(&orphan.session.id, ts)?;
        db.append_event(
            &row.quest_id,
            Some(&row.id),
            "session.end",
            &serde_json::json!({ "reason": orphan.reason }),
        )?;
        changed.push(row);
    }

    // Presence off-detection over the rows the orphan pass did not end.
    let command_of: BTreeMap<(&str, &str), &str> = panes
        .iter()
        .map(|p| {
            (
                (p.session_name.as_str(), p.pane_id.as_str()),
                p.current_command.as_str(),
            )
        })
        .collect();
    for session in &live {
        if orphaned.contains(session.id.as_str()) || !claims_claude(session.status) {
            continue;
        }
        if session.tmux_pane.is_empty() {
            continue;
        }
        let Some(command) =
            command_of.get(&(session.tmux_session.as_str(), session.tmux_pane.as_str()))
        else {
            continue;
        };
        // An empty `current_command` is an old 5-field pane line (a pre-v2 or
        // remote tmux, SPEC §6): unknown, not a shell — leave the row be.
        if command.is_empty() || !is_shell(command) {
            continue;
        }
        if session.status == SessionStatus::Starting {
            let anchor = session.claude_started_at.unwrap_or(session.updated_at);
            if ts - anchor <= BOOT_GRACE_SECS {
                continue;
            }
        }
        let row = db.update_session_status(&session.id, SessionStatus::Off, None)?;
        db.append_event(
            &row.quest_id,
            Some(&row.id),
            "session.off",
            &serde_json::json!({ "reason": "shell_detected", "command": command }),
        )?;
        changed.push(row);
    }

    // Edge-triggered cwd-follow, gated on the config flag (SPEC §6 v2). It
    // touches only `quest.cwd`/`last_pane_path`, never a session status, so it
    // stays out of `changed`.
    if config.quest.follow_main_cwd {
        follow_cwd(db, &panes)?;
    }
    Ok(changed)
}

/// Quest cwd follows the main session's *shell* cwd, edge-triggered on
/// `pane_current_path` changing (SPEC §6 v2, decision 4). For each Quest's live
/// main session:
///
/// * a non-shell pane (Claude up) or an empty command/path is skipped — one-way
///   and shell-gated, so a `q set cwd` while Claude runs is never reverted;
/// * the first shell observation only seeds `last_pane_path` — it never rewrites
///   an existing cwd;
/// * a later change (a real edge) moves `quest.cwd`, emits one
///   `quest.cwd_changed`, and reseeds `last_pane_path`.
///
/// Paths are canonicalised the way `q set cwd` canonicalises its argument, so a
/// symlinked `cd` and an explicit `set cwd` agree on the same directory and a
/// cwd already at the target is consumed silently (no event).
fn follow_cwd(db: &Db, panes: &[Pane]) -> anyhow::Result<()> {
    let pane_of: BTreeMap<(&str, &str), (&str, &str)> = panes
        .iter()
        .map(|p| {
            (
                (p.session_name.as_str(), p.pane_id.as_str()),
                (p.current_command.as_str(), p.current_path.as_str()),
            )
        })
        .collect();
    // One Quest can momentarily carry more than one live master (a resume or a
    // crash-recovery overlap). Follow only the newest per Quest — the same
    // `max_by_key(started_at)` the reseed in `q set cwd` protects — so the edge
    // and the reseed never disagree about which master owns the cwd.
    let live = db.list_live_sessions()?;
    let mut newest: BTreeMap<&str, &Session> = BTreeMap::new();
    for s in &live {
        if s.role != SessionRole::Master || s.tmux_pane.is_empty() {
            continue;
        }
        newest
            .entry(s.quest_id.as_str())
            .and_modify(|cur| {
                if s.started_at > cur.started_at {
                    *cur = s;
                }
            })
            .or_insert(s);
    }
    for session in newest.into_values() {
        let Some(&(command, path)) =
            pane_of.get(&(session.tmux_session.as_str(), session.tmux_pane.as_str()))
        else {
            continue;
        };
        // A non-shell pane is not proof Claude is up, but it is never a shell we
        // can read a `cd` off; an empty command is an old 5-field line.
        if command.is_empty() || !is_shell(command) || path.is_empty() {
            continue;
        }
        // A vanished/unreadable path (rmdir'd under the shell between the `cd`
        // and this sweep) is no directory `spawn`/`enter` could open; skip the
        // edge rather than write a dead path — the same rejection `q set cwd`
        // makes for a nonexistent dir. `canonical_path`'s raw-string fallback
        // is fine for the reseed baseline but must not become a stored cwd.
        let Ok(canonical) = std::fs::canonicalize(path) else {
            continue;
        };
        let canonical = canonical.to_string_lossy().into_owned();
        // No edge: the shell has not moved since we last saw it.
        if session.last_pane_path.as_deref() == Some(canonical.as_str()) {
            continue;
        }
        // First observation seeds the baseline and stops; a real edge is judged
        // against it next time.
        let first = session.last_pane_path.is_none();
        db.update_session_last_pane_path(&session.id, &canonical)?;
        if first {
            continue;
        }
        let Some(quest) = db.get_quest(&session.quest_id)? else {
            continue;
        };
        // Already there — e.g. `q set cwd` moved the cwd and reseeded, and this
        // is the same directory: consume the edge without a spurious event.
        if quest.cwd == canonical {
            continue;
        }
        let patch = QuestPatch {
            cwd: Some(canonical.clone()),
            ..QuestPatch::default()
        };
        let updated = db.update_quest(&quest.id, &patch)?;
        db.append_event(
            &updated.id,
            Some(&session.id),
            "quest.cwd_changed",
            &serde_json::json!({ "from": quest.cwd, "to": canonical, "source": "main_shell" }),
        )?;
    }
    Ok(())
}

/// Resolve symlinks the way `q set cwd` (`resolve_dir`) does, so a shell `cd`
/// and an explicit `set cwd` name the same directory. A path tmux reports may
/// since have vanished; then the raw value is the best we have.
pub(crate) fn canonical_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Quest, SessionStatus};
    use tempfile::TempDir;

    fn env() -> Vec<(String, String)> {
        vec![
            ("Q_QUEST".to_string(), "q-7f3a".to_string()),
            ("Q_ROLE".to_string(), "worker".to_string()),
        ]
    }

    #[test]
    fn parse_version_reads_major_and_minor_from_every_tmux_spelling() {
        assert_eq!(parse_version("tmux 3.6b"), Some((3, 6)));
        assert_eq!(parse_version("tmux 3.2a\n"), Some((3, 2)));
        assert_eq!(parse_version("tmux next-3.7"), Some((3, 7)));
        assert_eq!(parse_version("tmux 3.6 (fixture)"), Some((3, 6)));
        assert_eq!(parse_version("3.4"), Some((3, 4)));
        // No minor at all, and a two-digit minor.
        assert_eq!(parse_version("tmux 3"), Some((3, 0)));
        assert_eq!(parse_version("tmux 3.10"), Some((3, 10)));
    }

    #[test]
    fn parse_version_gives_up_on_a_version_without_a_number() {
        assert_eq!(parse_version("tmux fixture"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("tmux master"), None);
    }

    #[test]
    fn the_fixture_reports_a_parsable_version() {
        let dir = TempDir::new().unwrap();
        let tmux = FixtureTmux::new(dir.path().join("tmux.json"));
        assert_eq!(tmux.version().unwrap(), FIXTURE_VERSION);
        assert!(parse_version(&tmux.version().unwrap()).unwrap() >= MIN_TMUX);

        tmux.save(&FixtureState {
            version: Some("tmux 3.0".to_string()),
            ..FixtureState::default()
        })
        .unwrap();
        assert_eq!(tmux.version().unwrap(), "tmux 3.0");
    }

    #[test]
    fn new_session_args_carry_window_cwd_env_and_command() {
        let spec = NewSession {
            name: "q-alpha".to_string(),
            window_name: "master".to_string(),
            cwd: "/tmp/repo".to_string(),
            env: env(),
            command: Some("claude -n alpha/master".to_string()),
        };
        assert_eq!(
            args_new_session(&spec),
            [
                "new-session",
                "-d",
                "-s",
                "q-alpha",
                "-P",
                "-F",
                PANE_FORMAT,
                "-n",
                "master",
                "-c",
                "/tmp/repo",
                "-e",
                "Q_QUEST=q-7f3a",
                "-e",
                "Q_ROLE=worker",
                "claude -n alpha/master",
            ]
        );
    }

    #[test]
    fn optional_new_session_parts_are_omitted() {
        let spec = NewSession {
            name: "q-alpha".to_string(),
            ..NewSession::default()
        };
        assert_eq!(
            args_new_session(&spec),
            [
                "new-session",
                "-d",
                "-s",
                "q-alpha",
                "-P",
                "-F",
                PANE_FORMAT
            ]
        );
    }

    #[test]
    fn new_window_args_target_the_session_and_print_the_pane() {
        let spec = NewWindow {
            session: "q-alpha".to_string(),
            window_name: "w1-tests".to_string(),
            cwd: "/tmp/repo".to_string(),
            env: vec![("Q_SESSION".to_string(), "s-1".to_string())],
            command: Some("claude".to_string()),
        };
        assert_eq!(
            args_new_window(&spec),
            [
                "new-window",
                "-d",
                "-t",
                "=q-alpha:",
                "-P",
                "-F",
                PANE_FORMAT,
                "-n",
                "w1-tests",
                "-c",
                "/tmp/repo",
                "-e",
                "Q_SESSION=s-1",
                "claude",
            ]
        );
    }

    #[test]
    /// Every session target carries `=`, or `q-a` would resolve to `q-alpha`.
    fn the_remaining_operations_build_their_args() {
        assert_eq!(args_list_panes(), ["list-panes", "-a", "-F", PANE_FORMAT]);
        assert_eq!(
            args_display_pane("q-alpha:master"),
            [
                "display-message",
                "-p",
                "-t",
                "=q-alpha:master",
                PANE_FORMAT
            ]
        );
        assert_eq!(
            args_capture_pane("%42", 200),
            ["capture-pane", "-p", "-t", "%42", "-S", "-200"]
        );
        assert_eq!(
            args_rename_session("q-a", "q-b"),
            ["rename-session", "-t", "=q-a", "q-b"]
        );
        assert_eq!(
            args_rename_window("%42", "w1-tests"),
            ["rename-window", "-t", "%42", "w1-tests"]
        );
        assert_eq!(args_kill_session("q-a"), ["kill-session", "-t", "=q-a"]);
        assert_eq!(args_kill_window("%42"), ["kill-window", "-t", "%42"]);
        assert_eq!(args_has_session("q-a"), ["has-session", "-t", "=q-a"]);
        assert_eq!(args_attach("q-a"), ["attach", "-t", "=q-a"]);
        assert_eq!(args_switch_client("q-a"), ["switch-client", "-t", "=q-a"]);
        assert_eq!(args_select_window("%42"), ["select-window", "-t", "%42"]);
    }

    #[test]
    fn send_keys_sends_text_literally_and_enter_separately() {
        assert_eq!(
            args_send_keys("%42", "Enter; ls", false),
            [["send-keys", "-t", "%42", "-l", "--", "Enter; ls"]]
        );
        let with_enter = args_send_keys("%42", "go", true);
        assert_eq!(with_enter.len(), 2);
        assert_eq!(with_enter[1], ["send-keys", "-t", "%42", "Enter"]);
    }

    #[test]
    fn panes_parse_from_the_tab_separated_format() {
        // A 7-field line (current tmux) and, on the second row, an old 5-field
        // line: both parse, the shorter one leaving the two trailing fields
        // empty (back-compat with a remote `q` on the old format, SPEC §6).
        let out = "%42\t1234\tq-alpha\tmaster\t0\tzsh\t/tmp/repo\n\
                   %43\t1235\tq alpha\tw1 tests\t1\nrubbish\n";
        let panes = parse_panes(out);
        assert_eq!(
            panes[0],
            Pane {
                pane_id: "%42".to_string(),
                pane_pid: 1234,
                session_name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                window_index: 0,
                current_command: "zsh".to_string(),
                current_path: "/tmp/repo".to_string(),
            }
        );
        assert_eq!(panes[1].window_name, "w1 tests");
        assert_eq!(panes[1].current_command, "", "old 5-field line: no command");
        assert_eq!(panes[1].current_path, "", "old 5-field line: no path");
        assert_eq!(panes.len(), 2, "malformed lines are skipped");
    }

    #[test]
    fn no_server_is_recognised_but_a_missing_binary_is_not() {
        let no_server: anyhow::Error = QError::Tmux(
            "`tmux list-panes` failed: no server running on /tmp/tmux-501".to_string(),
        )
        .into();
        assert!(is_no_server(&no_server));
        let missing: anyhow::Error = QError::Tmux(TMUX_MISSING.to_string()).into();
        assert!(!is_no_server(&missing));
    }

    #[test]
    fn the_session_name_uses_the_configured_prefix() {
        let mut config = Config::default();
        assert_eq!(session_name(&config, "alpha"), "q-alpha");
        config.tmux.session_prefix = "quest_".to_string();
        assert_eq!(session_name(&config, "alpha"), "quest_alpha");
    }

    #[test]
    fn quest_env_adds_the_paths_only_when_overridden() {
        let base = quest_env(
            "q-7f3a",
            "s-1a2b",
            SessionRole::Master,
            "laptop",
            None,
            None,
        );
        assert_eq!(
            base,
            [
                ("Q_QUEST".to_string(), "q-7f3a".to_string()),
                ("Q_SESSION".to_string(), "s-1a2b".to_string()),
                ("Q_ROLE".to_string(), "master".to_string()),
                ("Q_MACHINE".to_string(), "laptop".to_string()),
            ]
        );
        let overridden = quest_env(
            "q-7f3a",
            "s-1a2b",
            SessionRole::Worker,
            "laptop",
            Some("/tmp/q.db"),
            Some("/tmp/q.toml"),
        );
        assert_eq!(overridden[2].1, "worker");
        assert_eq!(overridden[4], ("Q_DB".to_string(), "/tmp/q.db".to_string()));
        assert_eq!(
            overridden[5],
            ("Q_CONFIG".to_string(), "/tmp/q.toml".to_string())
        );
        // Only the config is set, and empty strings count as unset.
        let config_only = quest_env(
            "q",
            "s",
            SessionRole::Worker,
            "m",
            Some(""),
            Some("/tmp/q.toml"),
        );
        assert_eq!(config_only.len(), 5);
        assert_eq!(config_only[4].0, "Q_CONFIG");
        assert_eq!(
            quest_env("q", "s", SessionRole::Worker, "m", Some(""), Some("")).len(),
            4
        );
    }

    fn fixture() -> (TempDir, FixtureTmux) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nested").join("tmux.json");
        let t = FixtureTmux::new(path);
        (dir, t)
    }

    #[test]
    fn the_fixture_creates_lists_and_kills() {
        let (_dir, t) = fixture();
        assert!(t.list_panes().unwrap().is_empty());
        assert!(!t.has_session("q-alpha").unwrap());

        let master = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                cwd: "/tmp/repo".to_string(),
                env: env(),
                command: None,
            })
            .unwrap();
        assert_eq!(master.pane_id, "%1");
        assert_eq!(master.window_index, 0);
        assert!(master.pane_pid > 0);

        let worker = t
            .new_window(&NewWindow {
                session: "q-alpha".to_string(),
                window_name: "w1-tests".to_string(),
                ..NewWindow::default()
            })
            .unwrap();
        assert_eq!((worker.pane_id.as_str(), worker.window_index), ("%2", 1));

        assert_eq!(t.list_panes().unwrap().len(), 2);
        assert!(t.has_session("q-alpha").unwrap());
        assert!(
            t.new_session(&NewSession {
                name: "q-alpha".to_string(),
                ..NewSession::default()
            })
            .is_err()
        );

        t.rename_window(&worker.pane_id, "w1-migration").unwrap();
        t.rename_session("q-alpha", "q-beta").unwrap();
        let panes = t.list_panes().unwrap();
        assert_eq!(panes[1].window_name, "w1-migration");
        assert!(panes.iter().all(|p| p.session_name == "q-beta"));

        t.kill_session("q-beta").unwrap();
        assert!(t.list_panes().unwrap().is_empty());
        assert!(t.kill_session("q-beta").is_err());
    }

    #[test]
    fn the_fixture_buffers_keys_per_pane() {
        let (_dir, t) = fixture();
        let a = t
            .new_session(&NewSession {
                name: "q-a".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let b = t
            .new_window(&NewWindow {
                session: "q-a".to_string(),
                ..NewWindow::default()
            })
            .unwrap();

        t.send_keys(&a.pane_id, "one", true).unwrap();
        t.send_keys(&a.pane_id, "two", true).unwrap();
        t.send_keys(&a.pane_id, "three", false).unwrap();
        t.send_keys(&b.pane_id, "elsewhere", true).unwrap();

        assert_eq!(t.capture_pane(&a.pane_id, 100).unwrap(), "one\ntwo\nthree");
        assert_eq!(t.capture_pane(&a.pane_id, 2).unwrap(), "two\nthree");
        assert_eq!(t.capture_pane(&b.pane_id, 100).unwrap(), "elsewhere");
        assert!(t.capture_pane("%99", 10).is_err());
    }

    #[test]
    fn capture_drops_the_blank_rows_tmux_pads_a_pane_with() {
        assert_eq!(tail("one\ntwo\n\n   \n\n", 100), "one\ntwo");
        assert_eq!(tail("one\ntwo\nthree\n\n", 2), "two\nthree");
        // A blank line inside the output is content, not padding.
        assert_eq!(tail("one\n\ntwo\n\n", 0), "one\n\ntwo");
        assert_eq!(tail("\n\n", 10), "");
    }

    #[test]
    fn the_fixture_records_the_attach_target() {
        let (_dir, t) = fixture();
        let master = t
            .new_session(&NewSession {
                name: "q-a".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let other = t
            .new_session(&NewSession {
                name: "q-b".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();

        assert!(t.attach("q-nope", None).is_err(), "unknown session");
        assert!(t.attach("q-a", Some("%404")).is_err(), "unknown pane");
        let e = t.attach("q-a", Some(&other.pane_id)).unwrap_err();
        assert!(format!("{e}").contains("not in session q-a"), "{e}");
        assert!(t.load().unwrap().attached.is_none());

        t.attach("q-a", Some(&master.pane_id)).unwrap();
        let state = t.load().unwrap();
        assert_eq!(
            state.attached,
            Some(("q-a".to_string(), Some(master.pane_id.clone())))
        );
        // An attach that names a pane selects its window on the way.
        assert_eq!(state.selected, Some(master.pane_id.clone()));

        t.attach("q-a", None).unwrap();
        assert_eq!(t.load().unwrap().attached, Some(("q-a".to_string(), None)));
    }

    #[test]
    fn the_fixture_selects_a_window_by_pane_without_attaching() {
        let (_dir, t) = fixture();
        let master = t
            .new_session(&NewSession {
                name: "q-a".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let worker = t
            .new_window(&NewWindow {
                session: "q-a".to_string(),
                window_name: "w1-tests".to_string(),
                ..NewWindow::default()
            })
            .unwrap();

        assert!(t.select_window("%404").is_err(), "unknown pane");
        t.select_window(&worker.pane_id).unwrap();
        let state = t.load().unwrap();
        assert_eq!(state.selected, Some(worker.pane_id));
        // Selecting is not attaching.
        assert!(state.attached.is_none());
        assert_ne!(state.selected, Some(master.pane_id));
    }

    #[test]
    fn bind_key_binds_run_shell_in_the_prefix_table() {
        assert_eq!(
            args_bind_key("N", "/usr/local/bin/q spawn-here '#{pane_id}'"),
            vec![
                "bind-key",
                "N",
                "run-shell",
                "-b",
                "/usr/local/bin/q spawn-here '#{pane_id}'",
            ],
        );
    }

    #[test]
    fn prefix_binding_parses_the_command_out_of_list_keys() {
        // A user's own binding.
        assert_eq!(
            parse_prefix_binding("bind-key -T prefix N next-window\n").as_deref(),
            Some("next-window"),
        );
        // Ours, with the extra flags tmux may print, still recognised by the
        // `spawn-here` marker `bind_spawn_key` checks for.
        let ours =
            parse_prefix_binding("bind-key -r -T prefix N run-shell -b \"/q spawn-here '%1'\"\n")
                .unwrap();
        assert!(ours.contains("spawn-here"), "{ours}");
        // Unbound: tmux prints nothing.
        assert_eq!(parse_prefix_binding(""), None);
    }

    #[test]
    fn the_pane_line_is_the_last_line_of_stdout() {
        // tmux can warn ahead of the `-P -F` output; the format line is last.
        assert_eq!(last_line("%42\t1\tq-a\tw\t0\n"), "%42\t1\tq-a\tw\t0");
        assert_eq!(last_line("sessions should be nested\n%42\t1\n"), "%42\t1");
        assert_eq!(last_line(""), "");
        assert_eq!(last_line("\n"), "");
    }

    #[test]
    fn a_clientless_session_is_told_apart_from_a_real_failure() {
        let no_client: anyhow::Error =
            QError::Tmux("`tmux switch-client` failed: no current client".to_string()).into();
        let other: anyhow::Error =
            QError::Tmux("`tmux switch-client` failed: can\'t find session".to_string()).into();
        assert!(is_no_client(&no_client));
        assert!(!is_no_client(&other));
    }

    #[test]
    fn an_empty_pane_id_is_refused_rather_than_read_as_the_active_pane() {
        // Real tmux takes `-t ''` as "whatever is active" and exits 0.
        assert!(require_pane_id("").is_err());
        // R2-5: and it trims, so a blank target is the same active pane.
        for blank in [" ", "\t", "\n", "  \t "] {
            assert!(require_pane_id(blank).is_err(), "{blank:?} got through");
        }
        assert!(require_pane_id("%42").is_ok());
        let (_dir, t) = fixture();
        assert!(t.select_window("").is_err());
        assert!(t.select_window(" ").is_err());
    }

    /// Every call that hands tmux a pane as its `-t` target. An empty one is
    /// the current window, so `kill_window("")` kills the window `q` runs in,
    /// `capture_pane("")` reads q's own screen and `send_keys("")` types into
    /// it. Verified against a real tmux on a private socket: with two windows
    /// up, `kill-window -t ''` took the *active* one down and exited 0.
    #[test]
    fn every_pane_targeting_call_refuses_an_empty_target() {
        let (_dir, t) = fixture();
        t.new_session(&NewSession {
            name: "q-alpha".to_string(),
            window_name: "master".to_string(),
            ..NewSession::default()
        })
        .unwrap();

        let refusals: Vec<anyhow::Error> = vec![
            t.kill_window("").unwrap_err(),
            t.send_keys("", "rm -rf /", true).unwrap_err(),
            t.paste("", "rm -rf /", true).unwrap_err(),
            t.capture_pane("", 10).unwrap_err(),
            t.rename_window("", "gone").unwrap_err(),
            t.select_window("").unwrap_err(),
        ];
        for e in &refusals {
            assert!(
                format!("{e:#}").contains("empty pane id"),
                "wrong refusal: {e:#}"
            );
        }
        // The one window that does exist was not touched by any of them.
        let panes = t.list_panes().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].window_name, "master");
        assert_eq!(t.capture_pane(&panes[0].pane_id, 10).unwrap(), "");
    }

    #[test]
    fn a_leading_pane_id_is_recovered_from_an_unparsable_line() {
        assert_eq!(leading_pane_id("%42\tnonsense"), Some("%42"));
        assert_eq!(leading_pane_id("%42"), Some("%42"));
        assert_eq!(leading_pane_id("no server running"), None);
        assert_eq!(leading_pane_id("%\t1"), None);
        assert_eq!(leading_pane_id("%4x\t1"), None);
    }

    #[test]
    fn the_fixture_kills_one_window_without_the_session() {
        let (_dir, t) = fixture();
        let master = t
            .new_session(&NewSession {
                name: "q-a".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let worker = t
            .new_window(&NewWindow {
                session: "q-a".to_string(),
                window_name: "w1".to_string(),
                ..NewWindow::default()
            })
            .unwrap();

        t.kill_window(&worker.pane_id).unwrap();
        assert_eq!(
            t.list_panes()
                .unwrap()
                .iter()
                .map(|p| p.pane_id.clone())
                .collect::<Vec<_>>(),
            [master.pane_id]
        );
        assert!(t.has_session("q-a").unwrap());
        assert!(t.kill_window(&worker.pane_id).is_err());
    }

    #[test]
    fn the_fixture_survives_a_new_instance_on_the_same_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tmux.json");
        let first = FixtureTmux::new(&path);
        let pane = first
            .new_session(&NewSession {
                name: "q-a".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        first.send_keys(&pane.pane_id, "hello", true).unwrap();

        let second = FixtureTmux::new(&path);
        assert_eq!(second.list_panes().unwrap(), vec![pane.clone()]);
        assert_eq!(second.capture_pane(&pane.pane_id, 10).unwrap(), "hello");
        // Ids keep counting up rather than restarting at %1.
        let next = second
            .new_window(&NewWindow {
                session: "q-a".to_string(),
                ..NewWindow::default()
            })
            .unwrap();
        assert_eq!(next.pane_id, "%2");
    }

    #[test]
    fn a_hand_written_fixture_file_is_accepted() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tmux.json");
        std::fs::write(
            &path,
            r#"{"next_pane": 42, "panes": [{"pane_id": "%42", "session_name": "q-alpha"}]}"#,
        )
        .unwrap();
        let t = FixtureTmux::new(&path);
        let panes = t.list_panes().unwrap();
        assert_eq!(panes[0].pane_id, "%42");
        assert!(t.has_session("q-alpha").unwrap());
        assert_eq!(t.version().unwrap(), FIXTURE_VERSION);
    }

    #[test]
    fn a_fixture_without_a_counter_still_mints_unique_pane_ids() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tmux.json");
        // `next_pane` omitted, as any hand-written fixture may leave it: it
        // must not put `%1` on top of the pane that already holds it.
        std::fs::write(
            &path,
            r#"{"panes": [{"pane_id": "%1", "session_name": "q-alpha"}]}"#,
        )
        .unwrap();
        let t = FixtureTmux::new(&path);
        let pane = t
            .new_session(&NewSession {
                name: "q-beta".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        assert_eq!(pane.pane_id, "%2");

        let ids: Vec<String> = t
            .list_panes()
            .unwrap()
            .into_iter()
            .map(|p| p.pane_id)
            .collect();
        assert_eq!(ids, vec!["%1".to_string(), "%2".to_string()]);
    }

    #[test]
    fn a_stale_counter_never_reuses_a_live_pane_id() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tmux.json");
        std::fs::write(
            &path,
            r#"{"next_pane": 1, "panes": [{"pane_id": "%7", "session_name": "q-alpha"}]}"#,
        )
        .unwrap();
        let t = FixtureTmux::new(&path);
        let pane = t
            .new_session(&NewSession {
                name: "q-beta".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        assert_eq!(pane.pane_id, "%8");
    }

    #[test]
    fn is_shell_ignores_login_dashes_paths_and_claude_version_strings() {
        for shell in [
            "zsh", "-zsh", "bash", "-bash", "/bin/zsh", "fish", "sh", "nu",
        ] {
            assert!(is_shell(shell), "{shell} should read as a shell");
        }
        // Claude never reports `claude`: the native binary reports a version,
        // npm reports `node`. Neither, nor an editor, is a shell.
        for busy in ["2.1.0", "2.1.251", "node", "vim", "less", "cargo", "claude"] {
            assert!(!is_shell(busy), "{busy} must not read as a shell");
        }
    }

    #[test]
    fn worker_and_main_session_names_split_back_without_confusing_siblings() {
        let config = Config::default();
        assert_eq!(session_name(&config, "foo"), "q-foo");
        assert_eq!(
            worker_session_name(&config, "foo", "review"),
            "q-foo+review"
        );

        // The slug comes back off both the main and the worker name…
        assert_eq!(quest_slug_of_name(&config, "q-foo").as_deref(), Some("foo"));
        assert_eq!(
            quest_slug_of_name(&config, "q-foo+review").as_deref(),
            Some("foo"),
        );
        // …and a sibling Quest whose slug merely starts with `foo` is its own
        // Quest, never a worker of `foo` (the bug a bare `starts_with` invites).
        assert_eq!(
            quest_slug_of_name(&config, "q-foo-bar").as_deref(),
            Some("foo-bar"),
        );
        assert_eq!(
            quest_slug_of_name(&config, "q-foo-bar+w1").as_deref(),
            Some("foo-bar"),
        );
        // Not a Quest session at all, or nothing left of the slug.
        assert_eq!(quest_slug_of_name(&config, "irssi"), None);
        assert_eq!(quest_slug_of_name(&config, "q-"), None);
    }

    #[test]
    fn sessions_of_quest_gathers_main_and_workers_but_not_a_sibling() {
        let config = Config::default();
        let panes = [
            pane("q-foo", "%1"),
            pane("q-foo+review", "%2"),
            pane("q-foo+tests", "%3"),
            // A sibling Quest and an unrelated session must be left out.
            pane("q-foo-bar", "%4"),
            pane("q-foo-bar+w1", "%5"),
            pane("irssi", "%6"),
            // A second pane in the main session does not duplicate the name.
            pane("q-foo", "%7"),
        ];
        assert_eq!(
            sessions_of_quest(&config, &panes, "foo"),
            ["q-foo", "q-foo+review", "q-foo+tests"],
        );
        assert_eq!(
            sessions_of_quest(&config, &panes, "foo-bar"),
            ["q-foo-bar", "q-foo-bar+w1"],
        );
        assert!(sessions_of_quest(&config, &panes, "nope").is_empty());
    }

    #[test]
    fn a_fixture_pane_reports_a_shell_until_claude_launches_and_after_exit() {
        let (_dir, t) = fixture();
        // A login-shell pane (`command: None`) reports a shell in its cwd.
        let main = t
            .new_session(&NewSession {
                name: "q-foo".to_string(),
                window_name: "master".to_string(),
                cwd: "/tmp/repo".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        assert_eq!(main.current_command, FIXTURE_SHELL);
        assert_eq!(main.current_path, "/tmp/repo");

        // `q start` types `claude …`: the pane now reports the opaque version
        // string, never the literal `claude`.
        t.send_keys(&main.pane_id, "claude -n foo/master", true)
            .unwrap();
        let running = current_command(&t, &main.pane_id);
        assert_eq!(running, FIXTURE_CLAUDE_COMMAND);
        assert!(!is_shell(&running));

        // `/exit` (or `q stop`) drops back to the shell.
        t.send_keys(&main.pane_id, "/exit", true).unwrap();
        assert_eq!(current_command(&t, &main.pane_id), FIXTURE_SHELL);
    }

    #[test]
    fn a_pane_launched_straight_into_claude_reports_the_version_string() {
        // The M1a topology still launches Claude as the pane command; the
        // fixture reflects that as a non-shell current_command.
        let (_dir, t) = fixture();
        let pane = t
            .new_session(&NewSession {
                name: "q-foo".to_string(),
                window_name: "master".to_string(),
                cwd: "/tmp/repo".to_string(),
                command: Some("claude -n foo/master".to_string()),
                ..NewSession::default()
            })
            .unwrap();
        assert_eq!(pane.current_command, FIXTURE_CLAUDE_COMMAND);
        assert_eq!(pane.current_path, "/tmp/repo");
    }

    #[test]
    fn a_pre_v5_fixture_without_the_new_fields_still_loads() {
        // A fixture JSON written before v5 carries neither current_command nor
        // current_path; `#[serde(default)]` fills them with empty strings.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tmux.json");
        std::fs::write(
            &path,
            r#"{"next_pane":1,"panes":[{"pane_id":"%1","session_name":"q-foo","window_name":"master","window_index":0}]}"#,
        )
        .unwrap();
        let t = FixtureTmux::new(&path);
        let panes = t.list_panes().unwrap();
        assert_eq!(panes[0].current_command, "");
        assert_eq!(panes[0].current_path, "");
    }

    /// The `current_command` a fixture pane reports now.
    fn current_command(t: &FixtureTmux, pane_id: &str) -> String {
        t.list_panes()
            .unwrap()
            .into_iter()
            .find(|p| p.pane_id == pane_id)
            .unwrap()
            .current_command
    }

    fn seeded_db() -> (Db, Quest) {
        let db = Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("alpha", "/tmp/repo", "laptop"))
            .unwrap();
        (db, quest)
    }

    fn pane(session: &str, id: &str) -> Pane {
        Pane {
            pane_id: id.to_string(),
            pane_pid: 1,
            session_name: session.to_string(),
            window_name: "w".to_string(),
            window_index: 0,
            current_command: FIXTURE_SHELL.to_string(),
            current_path: String::new(),
        }
    }

    fn session_on(label: &str, tmux_session: &str, pane_id: &str) -> Session {
        Session::new("q-0001", SessionRole::Worker, label, tmux_session, pane_id)
    }

    #[test]
    fn find_orphans_keys_on_the_session_and_pane_pair() {
        let sessions = vec![
            session_on("alive", "q-alpha", "%1"),
            session_on("gone", "q-alpha", "%2"),
            // Same pane id, different tmux session: tmux recycles ids, so this
            // one is an orphan too.
            session_on("recycled", "q-beta", "%1"),
        ];
        let panes = [pane("q-alpha", "%1"), pane("q-other", "%9")];

        let ts = now();
        let orphans: Vec<(String, &str)> = find_orphans(sessions.clone(), &panes, ts)
            .into_iter()
            .map(|o| (o.session.label, o.reason))
            .collect();
        assert_eq!(
            orphans,
            [
                ("gone".to_string(), PANE_GONE),
                ("recycled".to_string(), PANE_GONE)
            ]
        );

        // No panes at all (a dead tmux server) orphans everything; every pane
        // present orphans nothing.
        assert_eq!(find_orphans(sessions.clone(), &[], ts).len(), 3);
        let all: Vec<Pane> = sessions
            .iter()
            .map(|s| pane(&s.tmux_session, &s.tmux_pane))
            .collect();
        assert!(find_orphans(sessions, &all, ts).is_empty());
    }

    #[test]
    fn a_row_with_no_pane_is_an_orphan_only_once_the_grace_has_passed() {
        let ts = now();
        let mut pending = session_on("starting", "q-alpha", "");
        pending.started_at = ts;
        // The window is still being opened: nothing to look for, nothing to end.
        assert!(find_orphans(vec![pending.clone()], &[], ts).is_empty());
        assert!(
            find_orphans(
                vec![pending.clone()],
                &[pane("q-alpha", "%1")],
                ts + START_GRACE_SECS
            )
            .is_empty()
        );

        // Past the grace nobody is going to fill the pane in — the row would
        // otherwise hold its label forever.
        pending.started_at = ts - START_GRACE_SECS - 1;
        let orphans = find_orphans(vec![pending], &[pane("q-alpha", "%1")], ts);
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].session.label, "starting");
        assert_eq!(orphans[0].reason, NEVER_STARTED);
    }

    #[test]
    fn the_sweep_ends_sessions_whose_pane_is_gone() {
        let (_dir, t) = fixture();
        let (db, quest) = seeded_db();
        let master = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let worker = t
            .new_window(&NewWindow {
                session: "q-alpha".to_string(),
                window_name: "w1".to_string(),
                ..NewWindow::default()
            })
            .unwrap();
        let live = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                &master.pane_id,
            ))
            .unwrap();
        let doomed = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Worker,
                "w1",
                "q-alpha",
                &worker.pane_id,
            ))
            .unwrap();

        assert!(
            sweep(&db, &t, &Config::default()).unwrap().is_empty(),
            "both panes are still there"
        );

        t.kill_session("q-alpha").unwrap();
        let ended = sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(ended.len(), 2);
        assert!(
            ended
                .iter()
                .all(|s| s.status == SessionStatus::Ended && s.ended_at.is_some())
        );
        assert_eq!(
            db.get_session(&live.id).unwrap().unwrap().status,
            SessionStatus::Ended
        );

        let events = db.list_events_by_quest(&quest.id, 10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e.kind == "session.end"));
        assert_eq!(
            events[0].payload.as_ref().unwrap()["reason"],
            serde_json::json!("pane_gone")
        );
        assert!(
            events
                .iter()
                .any(|e| e.session_id.as_deref() == Some(doomed.id.as_str()))
        );

        assert!(
            sweep(&db, &t, &Config::default()).unwrap().is_empty(),
            "already-ended sessions are not swept twice"
        );
    }

    #[test]
    fn the_sweep_keeps_sessions_whose_pane_survives() {
        let (_dir, t) = fixture();
        let (db, quest) = seeded_db();
        let master = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let kept = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                &master.pane_id,
            ))
            .unwrap();
        let ghost = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Worker,
                "w1",
                "q-alpha",
                "%404",
            ))
            .unwrap();
        // Same pane id, different tmux session — liveness is keyed on the pair.
        let stale = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Worker,
                "master",
                "q-elsewhere",
                &master.pane_id,
            ))
            .unwrap();

        let mut ended: Vec<String> = sweep(&db, &t, &Config::default())
            .unwrap()
            .iter()
            .map(|s| s.id.clone())
            .collect();
        ended.sort();
        let mut expected = vec![ghost.id, stale.id];
        expected.sort();
        assert_eq!(ended, expected);
        assert_ne!(
            db.get_session(&kept.id).unwrap().unwrap().status,
            SessionStatus::Ended
        );
    }

    #[test]
    fn a_dead_tmux_server_ends_every_live_session() {
        let (db, quest) = seeded_db();
        db.insert_session(&Session::new(
            &quest.id,
            SessionRole::Master,
            "master",
            "q-alpha",
            "%1",
        ))
        .unwrap();
        let ended = sweep(
            &db,
            &Stub::Fails("`tmux list-panes` failed: no server running on /tmp/tmux-501"),
            &Config::default(),
        )
        .unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(ended[0].status, SessionStatus::Ended);
    }

    #[test]
    fn a_missing_tmux_binary_fails_the_sweep() {
        let (db, quest) = seeded_db();
        db.insert_session(&Session::new(
            &quest.id,
            SessionRole::Master,
            "master",
            "q-alpha",
            "%1",
        ))
        .unwrap();
        let err = sweep(&db, &Stub::Fails(TMUX_MISSING), &Config::default()).unwrap_err();
        assert!(format!("{err:#}").contains(TMUX_MISSING));
    }

    #[test]
    fn an_empty_database_needs_no_tmux_at_all() {
        let (db, _quest) = seeded_db();
        assert!(
            sweep(&db, &Stub::Never, &Config::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_sweep_demotes_a_live_row_whose_pane_went_back_to_a_shell() {
        let (_dir, t) = fixture();
        let (db, quest) = seeded_db();
        // A login-shell pane (`command: None`), so `current_command` is a shell.
        let pane = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let row = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                &pane.pane_id,
            ))
            .unwrap();
        // The row still claims Claude is up (idle = between turns).
        db.update_session_status(&row.id, SessionStatus::Idle, None)
            .unwrap();

        let changed = sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, SessionStatus::Off);
        assert_eq!(
            db.get_session(&row.id).unwrap().unwrap().status,
            SessionStatus::Off
        );
        let events = db.list_events_by_quest(&quest.id, 10).unwrap();
        assert!(events.iter().any(|e| e.kind == "session.off"));
        // One-way: an `off` row is not swept again, and never promoted.
        assert!(sweep(&db, &t, &Config::default()).unwrap().is_empty());
    }

    #[test]
    fn a_starting_row_is_spared_within_the_off_grace_and_demoted_after() {
        let (_dir, t) = fixture();
        let (db, quest) = seeded_db();
        let pane = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let row = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                &pane.pane_id,
            ))
            .unwrap();

        // `q start` just typed the command; the pane still reads as a shell for
        // the blink before Claude execs. Within the grace the row is spared.
        db.record_claude_launch(&row.id, "alpha/master", None, now())
            .unwrap();
        assert!(
            sweep(&db, &t, &Config::default()).unwrap().is_empty(),
            "a fresh `starting` row must not be demoted within the grace"
        );
        assert_eq!(
            db.get_session(&row.id).unwrap().unwrap().status,
            SessionStatus::Starting
        );

        // Past the boot grace, a pane still on a shell means Claude never came up.
        db.record_claude_launch(&row.id, "alpha/master", None, now() - BOOT_GRACE_SECS - 1)
            .unwrap();
        let changed = sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].status, SessionStatus::Off);
    }

    #[test]
    fn the_sweep_leaves_a_live_claude_pane_alone() {
        let (_dir, t) = fixture();
        let (db, quest) = seeded_db();
        let pane = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                ..NewSession::default()
            })
            .unwrap();
        let row = db
            .insert_session(&Session::new(
                &quest.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                &pane.pane_id,
            ))
            .unwrap();
        db.update_session_status(&row.id, SessionStatus::Idle, None)
            .unwrap();
        // Claude is running: the pane reports its version string, not a shell.
        t.send_keys(&pane.pane_id, "claude -n alpha/master", true)
            .unwrap();
        assert!(sweep(&db, &t, &Config::default()).unwrap().is_empty());
        assert_eq!(
            db.get_session(&row.id).unwrap().unwrap().status,
            SessionStatus::Idle
        );
    }

    /// Move a fixture pane's shell into `path` — the sweep reads the edge off
    /// `current_path`, with `current_command` a shell.
    fn cd_shell(t: &FixtureTmux, pane_id: &str, path: &Path) {
        let mut state = t.load().unwrap();
        let pane = state
            .panes
            .iter_mut()
            .find(|p| p.pane_id == pane_id)
            .unwrap();
        pane.current_path = path.to_string_lossy().into_owned();
        pane.current_command = FIXTURE_SHELL.to_string();
        t.save(&state).unwrap();
    }

    fn canon(path: &Path) -> String {
        path.canonicalize().unwrap().to_string_lossy().into_owned()
    }

    fn main_session(db: &Db, t: &FixtureTmux, quest: &Quest, cwd: &Path) -> Session {
        let pane = t
            .new_session(&NewSession {
                name: "q-alpha".to_string(),
                window_name: "master".to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                ..NewSession::default()
            })
            .unwrap();
        db.insert_session(&Session::new(
            &quest.id,
            SessionRole::Master,
            "master",
            "q-alpha",
            &pane.pane_id,
        ))
        .unwrap()
    }

    #[test]
    fn follow_cwd_seeds_first_then_moves_on_a_shell_edge() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let x = dir.path().join("x");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&x).unwrap();
        let t = FixtureTmux::new(dir.path().join("tmux.json"));
        let (db, quest) = seeded_db();
        let row = main_session(&db, &t, &quest, &a);

        // First sweep only seeds the baseline; the cwd is left where it was.
        sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(
            db.get_session(&row.id).unwrap().unwrap().last_pane_path,
            Some(canon(&a))
        );
        assert_eq!(db.get_quest(&quest.id).unwrap().unwrap().cwd, quest.cwd);
        assert!(
            db.list_events_by_quest(&quest.id, 10)
                .unwrap()
                .iter()
                .all(|e| e.kind != "quest.cwd_changed")
        );

        // A shell `cd` to `x` is a real edge: cwd moves, one event.
        cd_shell(&t, &row.tmux_pane, &x);
        sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(db.get_quest(&quest.id).unwrap().unwrap().cwd, canon(&x));
        assert_eq!(
            db.list_events_by_quest(&quest.id, 10)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "quest.cwd_changed")
                .count(),
            1
        );

        // Idempotent: another sweep at the same path is not another edge.
        sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(
            db.list_events_by_quest(&quest.id, 10)
                .unwrap()
                .iter()
                .filter(|e| e.kind == "quest.cwd_changed")
                .count(),
            1
        );
    }

    #[test]
    fn follow_cwd_ignores_a_non_shell_main_pane() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let x = dir.path().join("x");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&x).unwrap();
        let t = FixtureTmux::new(dir.path().join("tmux.json"));
        let (db, quest) = seeded_db();
        let row = main_session(&db, &t, &quest, &a);
        // Claude is up in the main pane.
        t.send_keys(&row.tmux_pane, "claude -n alpha/master", true)
            .unwrap();
        // Even as the frozen path "moves", a non-shell pane never follows.
        let mut state = t.load().unwrap();
        state
            .panes
            .iter_mut()
            .find(|p| p.pane_id == row.tmux_pane)
            .unwrap()
            .current_path = x.to_string_lossy().into_owned();
        t.save(&state).unwrap();

        sweep(&db, &t, &Config::default()).unwrap();
        sweep(&db, &t, &Config::default()).unwrap();
        assert_eq!(db.get_quest(&quest.id).unwrap().unwrap().cwd, quest.cwd);
        assert_eq!(
            db.get_session(&row.id).unwrap().unwrap().last_pane_path,
            None
        );
    }

    #[test]
    fn follow_cwd_is_off_when_the_flag_is_off() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("a");
        let x = dir.path().join("x");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&x).unwrap();
        let t = FixtureTmux::new(dir.path().join("tmux.json"));
        let (db, quest) = seeded_db();
        let row = main_session(&db, &t, &quest, &a);
        let mut off = Config::default();
        off.quest.follow_main_cwd = false;

        sweep(&db, &t, &off).unwrap();
        cd_shell(&t, &row.tmux_pane, &x);
        sweep(&db, &t, &off).unwrap();
        assert_eq!(db.get_quest(&quest.id).unwrap().unwrap().cwd, quest.cwd);
        assert_eq!(
            db.get_session(&row.id).unwrap().unwrap().last_pane_path,
            None
        );
    }

    /// A `Tmux` for `sweep`: only `list_panes` is ever reachable.
    enum Stub {
        /// Panics if consulted.
        Never,
        /// Fails `list_panes` with this message.
        Fails(&'static str),
    }

    impl Tmux for Stub {
        fn list_panes(&self) -> anyhow::Result<Vec<Pane>> {
            match self {
                Stub::Never => panic!("tmux must not be consulted"),
                Stub::Fails(msg) => Err(QError::Tmux((*msg).to_string()).into()),
            }
        }
        fn new_session(&self, _: &NewSession) -> anyhow::Result<Pane> {
            unreachable!()
        }
        fn new_window(&self, _: &NewWindow) -> anyhow::Result<Pane> {
            unreachable!()
        }
        fn select_window(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn attach(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            unreachable!()
        }
        fn attach_child(&self, _: &str, _: Option<&str>) -> anyhow::Result<()> {
            unreachable!()
        }
        fn send_keys(&self, _: &str, _: &str, _: bool) -> anyhow::Result<()> {
            unreachable!()
        }
        fn send_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn paste(&self, _: &str, _: &str, _: bool) -> anyhow::Result<()> {
            unreachable!()
        }
        fn capture_pane(&self, _: &str, _: usize) -> anyhow::Result<String> {
            unreachable!()
        }
        fn rename_session(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn rename_window(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn kill_session(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn kill_window(&self, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn has_session(&self, _: &str) -> anyhow::Result<bool> {
            unreachable!()
        }
        fn in_tmux(&self) -> bool {
            false
        }
        fn version(&self) -> anyhow::Result<String> {
            unreachable!()
        }
        fn bind_key(&self, _: &str, _: &str) -> anyhow::Result<()> {
            unreachable!()
        }
        fn prefix_binding(&self, _: &str) -> anyhow::Result<Option<String>> {
            unreachable!()
        }
    }
}
