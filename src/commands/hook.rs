//! `q hook install | uninstall | status` — merges q's hooks and statusline
//! into Claude Code's `settings.json` (SPEC §7), touching only q-owned
//! entries. The event handlers themselves live in later beads.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::Ctx;
use crate::config;
use crate::error::QError;
use crate::output;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum State {
    Installed,
    Missing,
    Drifted,
}

impl State {
    fn symbol(self) -> char {
        match self {
            State::Installed => '✓',
            State::Missing => '✗',
            State::Drifted => '~',
        }
    }

    fn label(self) -> &'static str {
        match self {
            State::Installed => "installed",
            State::Missing => "missing",
            State::Drifted => "drifted",
        }
    }
}

#[derive(Debug, Serialize)]
struct EventStatus {
    event: &'static str,
    state: State,
}

#[derive(Debug, Serialize)]
struct StatuslineStatus {
    state: State,
    command: Option<String>,
    chain: String,
}

#[derive(Debug, Serialize)]
struct Status {
    settings: PathBuf,
    command: String,
    ok: bool,
    events: Vec<EventStatus>,
    statusline: StatuslineStatus,
    /// sha256 over the entry set `install` would write vs. the q entries found.
    expected_hash: String,
    actual_hash: String,
}

/// `$Q_CLAUDE_SETTINGS`, else `~/.claude/settings.json`.
pub fn settings_path() -> anyhow::Result<PathBuf> {
    if let Some(raw) = std::env::var_os("Q_CLAUDE_SETTINGS")
        && !raw.is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| QError::Config("cannot determine the home directory".to_string()))?;
    Ok(home.join(".claude").join("settings.json"))
}

/// The command hooks invoke q with: `--command` if given, else the absolute
/// path of the running binary — `q` might not be on Claude's PATH.
fn q_command(override_cmd: Option<&str>) -> anyhow::Result<String> {
    if let Some(c) = override_cmd {
        let c = c.trim();
        if c.is_empty() {
            return Err(QError::Invalid("--command must not be empty".to_string()).into());
        }
        return Ok(c.to_string());
    }
    let exe = std::env::current_exe().map_err(QError::Io)?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(exe.to_string_lossy().into_owned())
}

/// Ownership marker: `<bin> hook <sub>` where `<bin>` is a binary named `q`.
/// `<bin>` may be any path, so a reinstall from a moved binary still finds
/// the old entries.
fn owned_sub(command: &str) -> Option<&str> {
    let mut parts = command.split_whitespace();
    let bin = parts.next()?;
    if parts.next()? != "hook" {
        return None;
    }
    let sub = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let is_q = Path::new(bin).file_name().is_some_and(|n| n == "q");
    is_q.then_some(sub)
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

fn hash(values: &[Value]) -> String {
    let mut h = Sha256::new();
    for v in values {
        h.update(v.to_string());
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

    for ev in &EVENTS {
        let want = expected_group(&cmd, ev);
        let at = remove_owned(&mut settings, ev.name);
        let hooks = settings
            .as_object_mut()
            .expect("settings is an object")
            .entry("hooks")
            .or_insert_with(|| json!({}));
        if !hooks.is_object() {
            return Err(
                QError::Config(format!("{}: `hooks` is not an object", path.display())).into(),
            );
        }
        let groups = hooks
            .as_object_mut()
            .expect("checked above")
            .entry(ev.name)
            .or_insert_with(|| json!([]));
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
    if let Some(foreign) = existing
        && foreign != chain
    {
        chain = foreign.to_string();
        config::set_and_write(CHAIN_KEY, &chain)?;
    }
    let mut sl = before["statusLine"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    sl.insert("type".to_string(), json!("command"));
    sl.insert("command".to_string(), json!(statusline_command(&cmd)));
    settings["statusLine"] = Value::Object(sl);

    let changed = settings != before;
    if changed {
        write_settings(&path, &settings)?;
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

    let chain = ctx.config.statusline.chain.clone();
    let ours = settings["statusLine"]["command"]
        .as_str()
        .is_some_and(|c| owned_sub(c).is_some());
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
    if !chain.is_empty() {
        config::set_and_write(CHAIN_KEY, "")?;
    }

    let changed = settings != before;
    if changed {
        write_settings(&path, &settings)?;
    }
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &json!({
                "action": "uninstall",
                "changed": changed,
                "settings": path,
                "restored_statusline": (!chain.is_empty()).then_some(&chain),
            }),
            || {
                let verb = if changed {
                    "removed"
                } else {
                    "nothing to remove:"
                };
                let mut s = format!("{verb} q hooks from {}", path.display());
                if !chain.is_empty() {
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

// TODO(bd-8lz.2.2): the event handlers. Until then: drain stdin, exit 0.
pub fn noop() -> anyhow::Result<u8> {
    let mut sink = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut sink);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_needs_a_q_binary_and_the_hook_shape() {
        assert_eq!(owned_sub("q hook stop"), Some("stop"));
        assert_eq!(
            owned_sub("/usr/local/bin/q hook session-start"),
            Some("session-start")
        );
        assert_eq!(owned_sub("  q   hook statusline "), Some("statusline"));
        assert_eq!(owned_sub("qq hook stop"), None);
        assert_eq!(owned_sub("q hooks stop"), None);
        assert_eq!(owned_sub("q hook stop --now"), None);
        assert_eq!(owned_sub("npx ccusage statusline"), None);
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
