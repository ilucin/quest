//! `q hook install | uninstall | status` against a temp settings.json.

use std::fs;
use std::path::PathBuf;

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

    // The binary path is written as-is (not canonicalized), so a symlinked
    // install keeps pointing at the link.
    let bin = env!("CARGO_BIN_EXE_q").to_string();
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
    // Reported path is the canonical one (symlinks followed).
    let canonical = env.settings().canonicalize().unwrap();
    assert_eq!(v["settings"], canonical.to_string_lossy().as_ref());

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

#[test]
fn command_with_spaces_is_quoted_and_recognised() {
    let env = Env::new();
    env.write(&foreign_settings());
    let cmd = "/opt/my q/q";
    env.q()
        .args(["hook", "install", "--command", cmd])
        .assert()
        .success();
    let s = env.read();
    assert_eq!(q_groups(&s, "Stop").len(), 1);
    assert_eq!(
        q_groups(&s, "Stop")[0]["hooks"][0]["command"],
        "/opt/my q/q hook stop"
    );
    assert_eq!(s["statusLine"]["command"], "/opt/my q/q hook statusline");
    assert_eq!(env.chain(), "npx ccusage statusline");
    env.q()
        .args(["hook", "status", "--command", cmd])
        .assert()
        .success();

    // Recognised as ours: reinstall doesn't duplicate, doesn't chain itself.
    env.q()
        .args(["hook", "install", "--command", cmd])
        .assert()
        .success();
    assert_eq!(q_groups(&env.read(), "Stop").len(), 1);
    assert_eq!(env.chain(), "npx ccusage statusline");

    env.q().args(["hook", "uninstall"]).assert().success();
    let s = env.read();
    assert_eq!(s["hooks"], foreign_settings()["hooks"]);
    assert_eq!(s["statusLine"]["command"], "npx ccusage statusline");
    assert_eq!(env.chain(), "");
}

#[test]
fn binary_not_named_q_is_still_recognised() {
    let env = Env::new();
    env.write(&foreign_settings());
    for _ in 0..2 {
        env.q()
            .args(["hook", "install", "--command", "/opt/quest-bin"])
            .assert()
            .success();
    }
    let s = env.read();
    for ev in EVENTS {
        assert_eq!(q_groups(&s, ev).len(), 1, "{ev}");
    }
    assert_eq!(s["statusLine"]["command"], "/opt/quest-bin hook statusline");
    assert_eq!(env.chain(), "npx ccusage statusline");
    env.q()
        .args(["hook", "status", "--command", "/opt/quest-bin"])
        .assert()
        .success();

    env.q().args(["hook", "uninstall"]).assert().success();
    assert_eq!(env.read()["hooks"], foreign_settings()["hooks"]);
    assert_eq!(env.chain(), "");
}

#[test]
fn install_rejects_an_empty_command() {
    let env = Env::new();
    env.q()
        .args(["hook", "install", "--command", "  "])
        .assert()
        .failure();
    assert!(!env.settings().exists());
}

#[test]
fn mixed_group_keeps_the_foreign_hook_through_install_and_uninstall() {
    let env = Env::new();
    let mut s = foreign_settings();
    s["hooks"]["Stop"] = json!([{ "hooks": [
        { "type": "command", "command": "/old/q hook stop", "timeout": 3 },
        { "type": "command", "command": "echo stopped" }
    ] }]);
    env.write(&s);

    env.q().args(["hook", "install"]).assert().success();
    let s = env.read();
    let stop = s["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop.len(), 2, "{stop:?}");
    assert_eq!(stop[0]["hooks"].as_array().unwrap().len(), 1);
    assert!(
        stop[0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .ends_with(" hook stop")
    );
    assert_eq!(
        stop[1]["hooks"],
        json!([{ "type": "command", "command": "echo stopped" }])
    );
    env.q().args(["hook", "status"]).assert().success();

    env.q().args(["hook", "uninstall"]).assert().success();
    assert_eq!(
        env.read()["hooks"]["Stop"],
        json!([{ "hooks": [{ "type": "command", "command": "echo stopped" }] }])
    );
}

#[test]
fn null_hooks_and_events_count_as_absent() {
    let env = Env::new();
    env.write(&json!({ "hooks": null }));
    env.q().args(["hook", "install"]).assert().success();
    assert_eq!(env.read()["hooks"].as_object().unwrap().len(), EVENTS.len());
    env.q().args(["hook", "status"]).assert().success();

    env.write(&json!({ "hooks": { "Stop": null, "PreToolUse": null } }));
    env.q().args(["hook", "install"]).assert().success();
    let s = env.read();
    assert_eq!(q_groups(&s, "Stop").len(), 1);
    assert_eq!(s["hooks"]["PreToolUse"], Value::Null);
}

#[test]
fn uninstall_leaves_a_user_replaced_statusline_and_its_chain_alone() {
    let env = Env::new();
    env.write(&foreign_settings());
    env.q().args(["hook", "install"]).assert().success();
    let mut s = env.read();
    s["statusLine"] = json!({ "type": "command", "command": "my-new-statusline" });
    env.write(&s);

    let out = env
        .q()
        .args(["--json", "hook", "uninstall"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["restored_statusline"], Value::Null);
    assert_eq!(env.read()["statusLine"]["command"], "my-new-statusline");
    assert_eq!(env.read()["hooks"], foreign_settings()["hooks"]);
    assert_eq!(env.chain(), "npx ccusage statusline");
}

#[test]
fn a_string_statusline_is_an_error_and_leaves_the_file_untouched() {
    let env = Env::new();
    let s = json!({ "statusLine": "npx ccusage statusline" });
    env.write(&s);
    let before = fs::read_to_string(env.settings()).unwrap();
    env.q().args(["hook", "install"]).assert().failure();
    assert_eq!(fs::read_to_string(env.settings()).unwrap(), before);
    assert_eq!(env.chain(), "");
}

#[cfg(unix)]
#[test]
fn writes_follow_symlinks_and_keep_the_file_mode() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new();
    let target = env.dir.path().join("dotfiles").join("settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, serde_json::to_string(&foreign_settings()).unwrap()).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(env.settings().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, env.settings()).unwrap();

    env.q().args(["hook", "install"]).assert().success();

    assert!(fs::symlink_metadata(env.settings()).unwrap().is_symlink());
    let meta = fs::metadata(&target).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    let s: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(q_groups(&s, "Stop").len(), 1);
}

#[test]
fn statusline_passes_through_to_the_chain() {
    let env = Env::new();
    env.q()
        .args(["hook", "statusline"])
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("");

    env.q()
        .args([
            "config",
            "set",
            "statusline.chain",
            "cat | tr a-z A-Z; echo tail",
        ])
        .assert()
        .success();
    env.q()
        .args(["hook", "statusline"])
        .write_stdin("{\"ctx\":1}")
        .assert()
        .success()
        .stdout("{\"CTX\":1}tail\n");

    // A failing chain is swallowed.
    env.q()
        .args(["config", "set", "statusline.chain", "exit 3"])
        .assert()
        .success();
    env.q()
        .args(["hook", "statusline"])
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("");
}
