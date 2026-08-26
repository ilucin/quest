//! `q hook install | uninstall | status` against a temp settings.json.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            dir: TempDir::new().unwrap(),
        }
    }

    fn settings(&self) -> PathBuf {
        self.dir.path().join("claude").join("settings.json")
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn q(&self) -> Command {
        let mut cmd = Command::cargo_bin("q").unwrap();
        cmd.env("Q_DB", self.dir.path().join("q.db"))
            .env("Q_CONFIG", self.config())
            .env("Q_FIXTURE", self.dir.path().join("tmux.json"))
            .env("Q_CLAUDE_SETTINGS", self.settings());
        cmd
    }

    fn read(&self) -> Value {
        serde_json::from_str(&fs::read_to_string(self.settings()).unwrap()).unwrap()
    }

    fn write(&self, v: &Value) {
        fs::create_dir_all(self.settings().parent().unwrap()).unwrap();
        fs::write(self.settings(), serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    fn chain(&self) -> String {
        let out = self
            .q()
            .args(["--json", "config", "get", "statusline.chain"])
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["value"].as_str().unwrap().to_string()
    }
}

fn foreign_settings() -> Value {
    json!({
        "permissions": { "allow": ["Bash(ls:*)"] },
        "statusLine": { "type": "command", "command": "npx ccusage statusline", "padding": 0 },
        "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": "echo stopped" }] }
            ],
            "PostToolUse": [
                { "matcher": "Edit", "hooks": [{ "type": "command", "command": "prettier" }] }
            ],
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard" }] }
            ]
        }
    })
}

const EVENTS: [&str; 7] = [
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
    "Notification",
    "PreCompact",
    "SessionEnd",
    "PostToolUse",
];

fn q_groups<'a>(settings: &'a Value, event: &str) -> Vec<&'a Value> {
    settings["hooks"][event]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| {
            g["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains(" hook "))
        })
        .collect()
}

#[test]
fn install_merges_around_foreign_entries_and_chains_the_statusline() {
    let env = Env::new();
    env.write(&foreign_settings());

    env.q().args(["hook", "install"]).assert().success();
    let s = env.read();

    // Non-q keys survive untouched.
    assert_eq!(s["permissions"], foreign_settings()["permissions"]);
    assert_eq!(
        s["hooks"]["PreToolUse"],
        foreign_settings()["hooks"]["PreToolUse"]
    );
    assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "echo stopped");
    assert_eq!(s["hooks"]["PostToolUse"][0]["matcher"], "Edit");

    let bin = Path::new(env!("CARGO_BIN_EXE_q"))
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    for ev in EVENTS {
        let ours = q_groups(&s, ev);
        assert_eq!(ours.len(), 1, "{ev}: {ours:?}");
        let hook = &ours[0]["hooks"][0];
        assert_eq!(hook["type"], "command");
        assert!(
            hook["command"].as_str().unwrap().starts_with(&bin),
            "{ev}: {hook}"
        );
        assert!(hook["timeout"].is_number());
    }
    assert_eq!(q_groups(&s, "PostToolUse")[0]["matcher"], "Bash|Write");
    assert_eq!(
        q_groups(&s, "SessionStart")[0]["hooks"][0]["command"],
        format!("{bin} hook session-start")
    );
    assert!(q_groups(&s, "SessionStart")[0].get("matcher").is_none());

    assert_eq!(s["statusLine"]["command"], format!("{bin} hook statusline"));
    assert_eq!(s["statusLine"]["type"], "command");
    assert_eq!(s["statusLine"]["padding"], 0);
    assert_eq!(env.chain(), "npx ccusage statusline");

    let text = fs::read_to_string(env.settings()).unwrap();
    assert!(text.ends_with("}\n"));
    assert!(text.contains("\n  \"hooks\""), "2-space indent:\n{text}");

    env.q().args(["hook", "status"]).assert().success();
}

#[test]
fn install_is_idempotent_and_does_not_chain_itself() {
    let env = Env::new();
    env.write(&foreign_settings());
    env.q().args(["hook", "install"]).assert().success();
    let first = fs::read_to_string(env.settings()).unwrap();

    let out = env
        .q()
        .args(["--json", "hook", "install"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["changed"], false);
    assert_eq!(fs::read_to_string(env.settings()).unwrap(), first);
    assert_eq!(env.chain(), "npx ccusage statusline");
}

#[test]
fn uninstall_restores_foreign_state() {
    let env = Env::new();
    env.write(&foreign_settings());
    env.q().args(["hook", "install"]).assert().success();
    env.q().args(["hook", "uninstall"]).assert().success();

    let s = env.read();
    assert_eq!(s["permissions"], foreign_settings()["permissions"]);
    assert_eq!(s["hooks"], foreign_settings()["hooks"]);
    assert_eq!(s["statusLine"]["command"], "npx ccusage statusline");
    assert_eq!(s["statusLine"]["padding"], 0);
    assert_eq!(env.chain(), "");

    env.q().args(["hook", "status"]).assert().code(1);
}

#[test]
fn install_on_missing_file_and_uninstall_leaves_it_minimal() {
    let env = Env::new();
    env.q().args(["hook", "install"]).assert().success();
    let s = env.read();
    assert_eq!(s["hooks"].as_object().unwrap().len(), EVENTS.len());

    env.q().args(["hook", "uninstall"]).assert().success();
    assert_eq!(env.read(), json!({}));
}

#[test]
fn status_reports_drift_after_a_manual_edit() {
    let env = Env::new();
    env.q().args(["hook", "install"]).assert().success();

    let mut s = env.read();
    s["hooks"]["Stop"][0]["hooks"][0]["timeout"] = json!(1);
    s["hooks"]["Notification"] = json!([]);
    env.write(&s);

    let out = env.q().args(["--json", "hook", "status"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    let state = |ev: &str| {
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["event"] == ev)
            .unwrap()["state"]
            .clone()
    };
    assert_eq!(state("Stop"), "drifted");
    assert_eq!(state("Notification"), "missing");
    assert_eq!(state("SessionEnd"), "installed");
    assert_eq!(v["statusline"]["state"], "installed");
    assert_ne!(v["expected_hash"], v["actual_hash"]);
    assert_eq!(v["settings"], env.settings().to_string_lossy().as_ref());

    // Reinstall repairs it in place.
    env.q().args(["hook", "install"]).assert().success();
    env.q().args(["hook", "status"]).assert().success();
}

#[test]
fn command_override_is_honoured() {
    let env = Env::new();
    env.q()
        .args(["hook", "install", "--command", "q"])
        .assert()
        .success();
    let s = env.read();
    assert_eq!(s["hooks"]["Stop"][0]["hooks"][0]["command"], "q hook stop");
    assert_eq!(s["statusLine"]["command"], "q hook statusline");
    env.q()
        .args(["hook", "status", "--command", "q"])
        .assert()
        .success();
    // Compared against the binary path, the plain `q` entries are drift.
    env.q().args(["hook", "status"]).assert().code(1);
}

#[test]
fn hook_handlers_parse_and_exit_silently() {
    let env = Env::new();
    for sub in [
        "session-start",
        "user-prompt-submit",
        "stop",
        "notification",
        "pre-compact",
        "session-end",
        "post-tool-use",
        "statusline",
    ] {
        env.q()
            .args(["hook", sub])
            .write_stdin("{\"session_id\":\"x\"}")
            .assert()
            .success()
            .stdout("");
    }
}
