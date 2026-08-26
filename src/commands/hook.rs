//! `q hook install | uninstall | status` — merges q's hooks and statusline
//! into Claude Code's `settings.json` (SPEC §7), touching only q-owned
//! entries — plus `q hook statusline`. The event handlers live in `crate::hooks`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::Ctx;
use crate::config;
use crate::db::Db;
use crate::error::QError;
use crate::model::SessionStatus;
use crate::output;
use crate::proc;

/// Claude Code event, the `q hook <sub>` that handles it, its matcher and
/// timeout. Order is the order entries are written in.
struct Event {
    name: &'static str,
    sub: &'static str,
    matcher: Option<&'static str>,
    timeout: u64,
}

const EVENTS: [Event; 7] = [
    Event {
        name: "SessionStart",
        sub: "session-start",
        matcher: None,
        timeout: 15,
    },
    Event {
        name: "UserPromptSubmit",
        sub: "user-prompt-submit",
        matcher: None,
        timeout: 10,
    },
    Event {
        name: "Stop",
        sub: "stop",
        matcher: None,
        timeout: 10,
    },
    Event {
        name: "Notification",
        sub: "notification",
        matcher: None,
        timeout: 10,
    },
    Event {
        name: "PreCompact",
        sub: "pre-compact",
        matcher: None,
        timeout: 10,
    },
    Event {
        name: "SessionEnd",
        sub: "session-end",
        matcher: None,
        timeout: 10,
    },
    Event {
        name: "PostToolUse",
        sub: "post-tool-use",
        matcher: Some("Bash|Write"),
        timeout: 10,
    },
];

const STATUSLINE_SUB: &str = "statusline";
const CHAIN_KEY: &str = "statusline.chain";
const CHAIN_TIMEOUT: Duration = Duration::from_secs(5);

fn known_sub(sub: &str) -> bool {
    sub == STATUSLINE_SUB || EVENTS.iter().any(|e| e.sub == sub)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Installed,
    Missing,
    Drifted,
}

impl State {
    pub fn symbol(self) -> char {
        match self {
            State::Installed => '✓',
            State::Missing => '✗',
            State::Drifted => '~',
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            State::Installed => "installed",
            State::Missing => "missing",
            State::Drifted => "drifted",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct EventStatus {
    pub event: &'static str,
    pub state: State,
}

#[derive(Debug, Serialize)]
pub struct StatuslineStatus {
    pub state: State,
    pub command: Option<String>,
    pub chain: String,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub settings: PathBuf,
    pub command: String,
    pub ok: bool,
    pub events: Vec<EventStatus>,
    pub statusline: StatuslineStatus,
    /// sha256 over the entry set `install` would write vs. the q entries found.
    pub expected_hash: String,
    pub actual_hash: String,
}

/// `$Q_CLAUDE_SETTINGS`, else `~/.claude/settings.json`. Symlinks are
/// followed so the atomic rename replaces the target, not the link.
pub fn settings_path() -> anyhow::Result<PathBuf> {
    let path = match std::env::var_os("Q_CLAUDE_SETTINGS") {
        Some(raw) if !raw.is_empty() => PathBuf::from(raw),
        _ => {
            let home = dirs::home_dir()
                .ok_or_else(|| QError::Config("cannot determine the home directory".to_string()))?;
            home.join(".claude").join("settings.json")
        }
    };
    Ok(fs::canonicalize(&path).unwrap_or(path))
}

/// The command hooks invoke q with: `--command` if given, else the absolute
/// path of the running binary — `q` might not be on Claude's PATH. The path
/// is not canonicalized: a symlinked install (homebrew) must keep pointing at
/// the link so upgrades don't strand the hooks on an old version.
fn q_command(override_cmd: Option<&str>) -> anyhow::Result<String> {
    let cmd = match override_cmd {
        Some(c) => {
            let c = c.trim();
            if c.is_empty() {
                return Err(QError::Invalid("--command must not be empty".to_string()).into());
            }
            c.to_string()
        }
        None => {
            let exe = std::env::current_exe().map_err(QError::Io)?;
            let exe = if exe.is_absolute() {
                exe
            } else {
                std::env::current_dir().map_err(QError::Io)?.join(exe)
            };
            shell_quote(&exe.to_string_lossy())
        }
    };
    Ok(cmd)
}

/// Single-quotes a path when it contains whitespace so `sh` keeps it as one
/// argument; a bare path passes through untouched.
fn shell_quote(path: &str) -> String {
    if path.chars().any(char::is_whitespace) {
        format!("'{}'", path.replace('\'', "'\\''"))
    } else {
        path.to_string()
    }
}

/// Ownership marker: any command whose last two words are `hook <sub>` for
/// a sub q handles. Everything before is the binary — any name, any path,
/// quoted or not — so a reinstall from a moved or renamed binary still finds
/// the old entries.
fn owned_sub(command: &str) -> Option<&str> {
    let mut words = command.split_whitespace().rev();
    let sub = words.next()?;
    if words.next()? != "hook" || words.next().is_none() {
        return None;
    }
    known_sub(sub).then_some(sub)
}

fn is_owned(hook: &Value) -> bool {
    hook["command"]
        .as_str()
        .is_some_and(|c| owned_sub(c).is_some())
}

fn expected_group(cmd: &str, ev: &Event) -> Value {
    let mut group = Map::new();
    if let Some(m) = ev.matcher {
        group.insert("matcher".to_string(), json!(m));
    }
    group.insert(
        "hooks".to_string(),
        json!([{
            "type": "command",
            "command": format!("{cmd} hook {}", ev.sub),
            "timeout": ev.timeout,
        }]),
    );
    Value::Object(group)
}

fn statusline_command(cmd: &str) -> String {
    format!("{cmd} hook {STATUSLINE_SUB}")
}

fn read_settings(path: &Path) -> anyhow::Result<Value> {
    match fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => {
            let v: Value = serde_json::from_str(&text)
                .map_err(|e| QError::Config(format!("{}: {e}", path.display())))?;
            if !v.is_object() {
                return Err(QError::Config(format!(
                    "{}: expected a JSON object at the top level",
                    path.display()
                ))
                .into());
            }
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(QError::Config(format!("{}: {e}", path.display())).into()),
    }
}

fn write_settings(path: &Path, settings: &Value) -> anyhow::Result<()> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    config::write_atomic(path, &text)
}

fn groups_mut<'a>(settings: &'a mut Value, event: &str) -> Option<&'a mut Vec<Value>> {
    settings.get_mut("hooks")?.get_mut(event)?.as_array_mut()
}

/// The q-owned groups under one event, as `install` would compare them.
/// A foreign group that also carries a q hook is reduced to just our hook.
fn owned_groups(settings: &Value, event: &str) -> Vec<Value> {
    let Some(groups) = settings["hooks"][event].as_array() else {
        return vec![];
    };
    groups
        .iter()
        .filter_map(|g| {
            let hooks: Vec<Value> = g["hooks"]
                .as_array()
                .map(|h| h.iter().filter(|h| is_owned(h)).cloned().collect())
                .unwrap_or_default();
            if hooks.is_empty() {
                return None;
            }
            let mut ours = Map::new();
            if let Some(m) = g.get("matcher") {
                ours.insert("matcher".to_string(), m.clone());
            }
            ours.insert("hooks".to_string(), Value::Array(hooks));
            Some(Value::Object(ours))
        })
        .collect()
}

/// Strips q's hooks out of one event. Returns where the first q group sat, so
/// a reinstall lands in the same place and stays byte-identical.
fn remove_owned(settings: &mut Value, event: &str) -> Option<usize> {
    let groups = groups_mut(settings, event)?;
    let mut first = None;
    let mut i = 0;
    while i < groups.len() {
        let g = &mut groups[i];
        let Some(hooks) = g.get_mut("hooks").and_then(Value::as_array_mut) else {
            i += 1;
            continue;
        };
        let before = hooks.len();
        hooks.retain(|h| !is_owned(h));
        if hooks.len() != before {
            first.get_or_insert(i);
        }
        if hooks.is_empty() {
            groups.remove(i);
        } else {
            i += 1;
        }
    }
    first
}

fn prune_empty(settings: &mut Value) {
    let Some(obj) = settings.as_object_mut() else {
        return;
    };
    if let Some(hooks) = obj.get_mut("hooks").and_then(Value::as_object_mut) {
        hooks.retain(|_, v| !v.as_array().is_some_and(Vec::is_empty));
        if hooks.is_empty() {
            obj.remove("hooks");
        }
    }
}

/// Same value with object keys sorted at every level, so the hash doesn't
/// depend on the order a hand-edited file happens to use.
fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let sorted: std::collections::BTreeMap<_, _> =
                m.iter().map(|(k, v)| (k.clone(), canonical(v))).collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        _ => v.clone(),
    }
}

fn hash(values: &[Value]) -> String {
    let mut h = Sha256::new();
    for v in values {
        h.update(canonical(v).to_string());
        h.update([0]);
    }
    let out = h.finalize();
    out.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn compute_status(settings: &Value, path: PathBuf, cmd: &str, chain: &str) -> Status {
    let mut expected = Vec::new();
    let mut actual = Vec::new();
    let mut events = Vec::new();
    for ev in &EVENTS {
        let want = expected_group(cmd, ev);
        let have = owned_groups(settings, ev.name);
        let state = if have.is_empty() {
            State::Missing
        } else if have.len() == 1 && have[0] == want {
            State::Installed
        } else {
            State::Drifted
        };
        events.push(EventStatus {
            event: ev.name,
            state,
        });
        expected.push(want);
        actual.extend(have);
    }

    let sl = &settings["statusLine"];
    let sl_cmd = sl["command"].as_str().map(str::to_string);
    let want_sl = statusline_command(cmd);
    let sl_state = match &sl_cmd {
        Some(c) if *c == want_sl && sl["type"] == "command" => State::Installed,
        Some(c) if owned_sub(c) == Some(STATUSLINE_SUB) => State::Drifted,
        _ => State::Missing,
    };
    expected.push(json!(want_sl));
    if let Some(c) = &sl_cmd
        && owned_sub(c).is_some()
    {
        actual.push(json!(c));
    }

    let ok = events.iter().all(|e| e.state == State::Installed) && sl_state == State::Installed;
    Status {
        settings: path,
        command: cmd.to_string(),
        ok,
        events,
        statusline: StatuslineStatus {
            state: sl_state,
            command: sl_cmd,
            chain: chain.to_string(),
        },
        expected_hash: hash(&expected),
        actual_hash: hash(&actual),
    }
}

impl Status {
    fn human(&self) -> String {
        let mut lines = vec![format!("settings: {}", self.settings.display())];
        for e in &self.events {
            lines.push(format!(
                "{} {:<18} {}",
                e.state.symbol(),
                e.event,
                e.state.label()
            ));
        }
        let sl = &self.statusline;
        let mut line = format!(
            "{} {:<18} {}",
            sl.state.symbol(),
            "statusLine",
            sl.state.label()
        );
        if let Some(c) = &sl.command {
            line.push_str(&format!(" · {c}"));
        }
        if !sl.chain.is_empty() {
            line.push_str(&format!(" · chain: {}", sl.chain));
        }
        lines.push(line);
        lines.push(format!(
            "hash: expected {} · actual {}",
            self.expected_hash, self.actual_hash
        ));
        lines.join("\n")
    }
}

pub fn install(ctx: &Ctx, command: Option<&str>) -> anyhow::Result<u8> {
    let path = settings_path()?;
    let cmd = q_command(command)?;
    let before = read_settings(&path)?;
    let mut settings = before.clone();

    // `null` counts as absent; anything else of the wrong shape is an error.
    if !matches!(before["statusLine"], Value::Null | Value::Object(_)) {
        return Err(
            QError::Config(format!("{}: `statusLine` is not an object", path.display())).into(),
        );
    }

    for ev in &EVENTS {
        let want = expected_group(&cmd, ev);
        let at = remove_owned(&mut settings, ev.name);
        let hooks = settings
            .as_object_mut()
            .expect("settings is an object")
            .entry("hooks")
            .or_insert(Value::Null);
        if hooks.is_null() {
            *hooks = json!({});
        }
        if !hooks.is_object() {
            return Err(
                QError::Config(format!("{}: `hooks` is not an object", path.display())).into(),
            );
        }
        let groups = hooks
            .as_object_mut()
            .expect("checked above")
            .entry(ev.name)
            .or_insert(Value::Null);
        if groups.is_null() {
            *groups = json!([]);
        }
        let Some(groups) = groups.as_array_mut() else {
            return Err(QError::Config(format!(
                "{}: `hooks.{}` is not an array",
                path.display(),
                ev.name
            ))
            .into());
        };
        let at = at.unwrap_or(groups.len()).min(groups.len());
        groups.insert(at, want);
    }

    // A foreign statusline becomes the chain `q hook statusline` passes
    // through to; our own is never chained.
    let mut chain = ctx.config.statusline.chain.clone();
    let existing = before["statusLine"]["command"]
        .as_str()
        .map(str::trim)
        .filter(|c| !c.is_empty() && owned_sub(c).is_none());
    let new_chain = existing.filter(|foreign| *foreign != chain);
    let mut sl = before["statusLine"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    sl.insert("type".to_string(), json!("command"));
    sl.insert("command".to_string(), json!(statusline_command(&cmd)));
    settings["statusLine"] = Value::Object(sl);

    // Settings first: if that write fails the config must not already claim
    // a chain that nothing calls.
    let changed = settings != before;
    if changed {
        write_settings(&path, &settings)?;
    }
    if let Some(foreign) = new_chain {
        chain = foreign.to_string();
        config::set_and_write(CHAIN_KEY, &chain)?;
    }

    let status = compute_status(&settings, path.clone(), &cmd, &chain);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &json!({ "action": "install", "changed": changed, "status": status }),
            || {
                let verb = if changed {
                    "installed"
                } else {
                    "already installed"
                };
                format!("{verb} q hooks into {}\n{}", path.display(), status.human())
            },
        )?;
    }
    Ok(0)
}

pub fn uninstall(ctx: &Ctx) -> anyhow::Result<u8> {
    let path = settings_path()?;
    let before = read_settings(&path)?;
    let mut settings = before.clone();

    for ev in &EVENTS {
        remove_owned(&mut settings, ev.name);
    }
    prune_empty(&mut settings);

    // The chain is only ours to restore (and forget) while statusLine still
    // points at us; if the user replaced it, leave both alone.
    let chain = ctx.config.statusline.chain.clone();
    let ours = settings["statusLine"]["command"]
        .as_str()
        .is_some_and(|c| owned_sub(c).is_some());
    let restored = ours && !chain.is_empty();
    if ours {
        let obj = settings.as_object_mut().expect("settings is an object");
        if chain.is_empty() {
            obj.remove("statusLine");
        } else {
            let mut sl = before["statusLine"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            sl.insert("type".to_string(), json!("command"));
            sl.insert("command".to_string(), json!(chain));
            obj.insert("statusLine".to_string(), Value::Object(sl));
        }
    }
    let changed = settings != before;
    if changed {
        write_settings(&path, &settings)?;
    }
    if restored {
        config::set_and_write(CHAIN_KEY, "")?;
    }
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &json!({
                "action": "uninstall",
                "changed": changed,
                "settings": path,
                "restored_statusline": restored.then_some(&chain),
            }),
            || {
                let verb = if changed {
                    "removed"
                } else {
                    "nothing to remove:"
                };
                let mut s = format!("{verb} q hooks from {}", path.display());
                if restored {
                    s.push_str(&format!("\nrestored statusLine: {chain}"));
                }
                s
            },
        )?;
    }
    Ok(0)
}

/// Exit 1 when anything is missing or drifted, so `q doctor` can lean on it.
pub fn status(ctx: &Ctx, command: Option<&str>) -> anyhow::Result<u8> {
    let path = settings_path()?;
    let cmd = q_command(command)?;
    let settings = read_settings(&path)?;
    let status = compute_status(&settings, path, &cmd, &ctx.config.statusline.chain);
    output::emit(ctx.json, &status, || status.human())?;
    Ok(u8::from(!status.ok))
}

/// Statusline refresh (SPEC §7): forwards the raw payload to the chained
/// command and prints whatever it prints, then records the context-window %
/// Claude reports for the session this pane belongs to. Runs after every
/// message, so it never fails, never hangs, and skips a busy database.
pub fn statusline(ctx: &Ctx) -> anyhow::Result<u8> {
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    let chain = ctx.config.statusline.chain.trim();
    if !chain.is_empty()
        && let Some(out) = run_chain(chain, &input, CHAIN_TIMEOUT)
    {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(&out);
        let _ = stdout.flush();
    }
    record_ctx(&input);
    Ok(0)
}

const CTX_DB_BUSY_MS: u32 = 200;

/// Best effort: any failure just means this refresh is not recorded. The
/// session is `$Q_SESSION` when q started this pane; otherwise the live
/// session Claude's `session_id` was last recorded on — narrowed to
/// `$TMUX_PANE` when set, since `claude --resume` replays an id elsewhere.
/// Never creates the database: Claude calls this in every session, most of
/// them nothing to do with q.
fn record_ctx(input: &[u8]) {
    let Ok(payload) = serde_json::from_slice::<Value>(input) else {
        return;
    };
    let Some(pct) = ctx_pct(&payload) else {
        return;
    };
    let claude_id = payload["session_id"].as_str().filter(|s| !s.is_empty());
    let q_session = env_var("Q_SESSION");
    if q_session.is_none() && claude_id.is_none() {
        return;
    }
    let Ok(path) = Db::path() else {
        return;
    };
    if !path.exists() {
        return;
    }
    let Ok(db) = Db::open_with_timeout(&path, CTX_DB_BUSY_MS) else {
        return;
    };
    let session = match (q_session, claude_id) {
        (Some(id), _) => db.get_session(&id),
        (None, Some(cid)) => db.find_session_by_claude_id(cid, env_var("TMUX_PANE").as_deref()),
        (None, None) => return,
    };
    let Ok(Some(session)) = session else {
        return;
    };
    if session.status == SessionStatus::Ended {
        return;
    }
    let _ = db.update_session_ctx(&session.id, pct, claude_id);
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

fn ctx_pct(payload: &Value) -> Option<u8> {
    let used = payload["context_window"]["used_percentage"].as_f64()?;
    if !used.is_finite() {
        return None;
    }
    Some(used.round().clamp(0.0, 100.0) as u8)
}

/// Runs the chain with the payload on stdin. A chain that outlives `timeout`
/// is killed and whatever it printed so far is kept.
fn run_chain(chain: &str, input: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(chain);
    proc::run(&mut cmd, input, timeout).ok().map(|o| o.stdout)
}

/// The install state `q hook status` reports, for callers that only want to
/// read it — `q doctor`. `chain` is the configured statusline chain.
pub fn installed_status(chain: &str) -> anyhow::Result<Status> {
    let path = settings_path()?;
    let cmd = q_command(None)?;
    let settings = read_settings(&path)?;
    Ok(compute_status(&settings, path, &cmd, chain))
}

/// Directory holding Claude Code's user files, i.e. where `settings.json`
/// lives. `q doctor` looks for credentials next to it.
pub fn claude_dir() -> anyhow::Result<PathBuf> {
    let path = settings_path()?;
    Ok(path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(".")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_out_chain_keeps_its_partial_output() {
        let out = run_chain("echo partial; sleep 30", b"", Duration::from_millis(200));
        assert_eq!(out.as_deref(), Some(&b"partial\n"[..]));
    }

    #[test]
    fn ctx_pct_rounds_and_tolerates_missing_fields() {
        assert_eq!(
            ctx_pct(&json!({"context_window": {"used_percentage": 42.6}})),
            Some(43)
        );
        assert_eq!(
            ctx_pct(&json!({"context_window": {"used_percentage": 100.4}})),
            Some(100)
        );
        assert_eq!(
            ctx_pct(&json!({"context_window": {"used_percentage": 7}})),
            Some(7)
        );
        assert_eq!(ctx_pct(&json!({"context_window": {}})), None);
        assert_eq!(ctx_pct(&json!({"session_id": "x"})), None);
        assert_eq!(
            ctx_pct(&json!({"context_window": {"used_percentage": "42"}})),
            None
        );
    }

    #[test]
    fn ownership_is_any_binary_followed_by_a_known_hook_sub() {
        assert_eq!(owned_sub("q hook stop"), Some("stop"));
        assert_eq!(
            owned_sub("/usr/local/bin/q hook session-start"),
            Some("session-start")
        );
        assert_eq!(owned_sub("  q   hook statusline "), Some("statusline"));
        assert_eq!(owned_sub("/opt/quest-bin hook stop"), Some("stop"));
        assert_eq!(owned_sub("'/opt/my q/q' hook stop"), Some("stop"));
        assert_eq!(owned_sub("q --db x hook pre-compact"), Some("pre-compact"));
        assert_eq!(owned_sub("hook stop"), None);
        assert_eq!(owned_sub("q hooks stop"), None);
        assert_eq!(owned_sub("q hook stop --now"), None);
        assert_eq!(owned_sub("q hook unknown"), None);
        assert_eq!(owned_sub("npx ccusage statusline"), None);
    }

    #[test]
    fn shell_quote_only_when_needed() {
        assert_eq!(shell_quote("/usr/bin/q"), "/usr/bin/q");
        assert_eq!(shell_quote("/opt/my q/q"), "'/opt/my q/q'");
        assert_eq!(shell_quote("/it's q/q"), "'/it'\\''s q/q'");
    }

    #[test]
    fn hash_ignores_key_order() {
        let a =
            json!({ "hooks": [{ "type": "command", "command": "q hook stop" }], "matcher": "X" });
        let b =
            json!({ "matcher": "X", "hooks": [{ "command": "q hook stop", "type": "command" }] });
        assert_eq!(hash(std::slice::from_ref(&a)), hash(&[b]));
        assert_ne!(hash(&[a]), hash(&[json!({ "matcher": "Y" })]));
    }

    #[test]
    fn remove_owned_keeps_foreign_hooks_and_reports_position() {
        let mut s = json!({ "hooks": { "Stop": [
            { "hooks": [{ "type": "command", "command": "echo hi" }] },
            { "hooks": [
                { "type": "command", "command": "q hook stop" },
                { "type": "command", "command": "other" }
            ] },
            { "hooks": [{ "type": "command", "command": "/x/q hook stop" }] }
        ] } });
        assert_eq!(remove_owned(&mut s, "Stop"), Some(1));
        let groups = s["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0]["hooks"][0]["command"], "echo hi");
        assert_eq!(groups[1]["hooks"][0]["command"], "other");
        assert_eq!(remove_owned(&mut s, "Stop"), None);
        assert_eq!(remove_owned(&mut s, "Nope"), None);
    }

    #[test]
    fn status_classifies_each_event() {
        let cmd = "/bin/q";
        let mut s = json!({});
        let st = compute_status(&s, PathBuf::new(), cmd, "");
        assert!(!st.ok);
        assert!(st.events.iter().all(|e| e.state == State::Missing));
        assert_eq!(st.statusline.state, State::Missing);

        for ev in &EVENTS {
            s["hooks"][ev.name] = json!([expected_group(cmd, ev)]);
        }
        s["statusLine"] = json!({ "type": "command", "command": statusline_command(cmd) });
        let st = compute_status(&s, PathBuf::new(), cmd, "");
        assert!(st.ok, "{st:?}");
        assert_eq!(st.expected_hash, st.actual_hash);

        s["hooks"]["Stop"][0]["hooks"][0]["timeout"] = json!(99);
        s["statusLine"]["command"] = json!("/old/q hook statusline");
        let st = compute_status(&s, PathBuf::new(), cmd, "");
        assert!(!st.ok);
        let stop = st.events.iter().find(|e| e.event == "Stop").unwrap();
        assert_eq!(stop.state, State::Drifted);
        assert_eq!(st.statusline.state, State::Drifted);
        assert_ne!(st.expected_hash, st.actual_hash);
    }
}
