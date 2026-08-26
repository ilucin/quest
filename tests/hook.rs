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
            .env("Q_CLAUDE_SETTINGS", self.settings())
            // The handlers key off these; none may leak in from the terminal
            // `cargo test` runs in.
            .env_remove("Q_QUEST")
            .env_remove("Q_SESSION")
            .env_remove("TMUX_PANE");
        cmd
    }

    fn db(&self) -> PathBuf {
        self.dir.path().join("q.db")
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.db()).unwrap()
    }

    /// Quest `q-0001` (`alpha`) with one live worker session `s-0001` on
    /// pane `%7`, in `status`.
    fn seed(&self, status: &str) {
        // `list` creates and migrates the database.
        self.q().arg("list").assert().success();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO quest (id, slug, name_source, goal, cwd, machine, state, \
             created_at, updated_at) VALUES ('q-0001', 'alpha', 'manual', 'ship it', \
             '/tmp', 'laptop', 'active', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, quest_id, role, label, tmux_session, tmux_pane, \
             status, started_at, updated_at) VALUES ('s-0001', 'q-0001', 'worker', 'w1', \
             'q-alpha', '%7', ?1, 1, 1)",
            [status],
        )
        .unwrap();
    }

    /// `q hook <sub>` inside the seeded Quest pane, fed `payload`.
    fn hook(&self, sub: &str, payload: &Value) -> assert_cmd::assert::Assert {
        self.q()
            .env("Q_QUEST", "q-0001")
            .env("Q_SESSION", "s-0001")
            .args(["hook", sub])
            .write_stdin(payload.to_string())
            .assert()
    }

    fn session(&self) -> Value {
        self.conn()
            .query_row(
                "SELECT status, waiting_for, claude_session_id, claude_pid, first_prompt, \
                 last_prompt, ended_at, updated_at FROM session WHERE id = 's-0001'",
                [],
                |r| {
                    Ok(json!({
                        "status": r.get::<_, String>(0)?,
                        "waiting_for": r.get::<_, Option<String>>(1)?,
                        "claude_session_id": r.get::<_, Option<String>>(2)?,
                        "claude_pid": r.get::<_, Option<i64>>(3)?,
                        "first_prompt": r.get::<_, Option<String>>(4)?,
                        "last_prompt": r.get::<_, Option<String>>(5)?,
                        "ended_at": r.get::<_, Option<i64>>(6)?,
                        "updated_at": r.get::<_, i64>(7)?,
                    }))
                },
            )
            .unwrap()
    }

    /// `(kind, payload)` of every event, oldest first.
    fn events(&self) -> Vec<(String, Value)> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT kind, payload FROM event ORDER BY id")
            .unwrap();
        stmt.query_map([], |r| {
            let payload: Option<String> = r.get(1)?;
            Ok((
                r.get::<_, String>(0)?,
                payload
                    .map(|p| serde_json::from_str(&p).unwrap())
                    .unwrap_or(Value::Null),
            ))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
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
fn command_override_with_spaces_is_written_unquoted_and_recognised() {
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

/// A quest with one live session, mirroring what `q new` writes.
fn seed_session(env: &Env, claude_session_id: Option<&str>) {
    seed_session_with(env, claude_session_id, "idle");
}

fn seed_session_with(env: &Env, claude_session_id: Option<&str>, status: &str) {
    // Let `q` create and migrate the database first.
    env.q().args(["list", "--json"]).assert().success();
    let conn = rusqlite::Connection::open(env.dir.path().join("q.db")).unwrap();
    conn.execute(
        "INSERT INTO quest (id, slug, name_source, cwd, machine, state, created_at, updated_at)
         VALUES ('q-0001', 'alpha', 'manual', '/tmp', 'laptop', 'active', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, quest_id, role, label, tmux_session, tmux_pane, status,
                              claude_session_id, started_at, updated_at)
         VALUES ('s-0001', 'q-0001', 'master', 'master', 'q-alpha', '%1', ?2, ?1, 1, 1)",
        rusqlite::params![claude_session_id, status],
    )
    .unwrap();
}

fn session_ctx(env: &Env) -> (Option<i64>, Option<i64>, Option<String>) {
    let conn = rusqlite::Connection::open(env.dir.path().join("q.db")).unwrap();
    conn.query_row(
        "SELECT ctx_pct, ctx_updated_at, claude_session_id FROM session WHERE id = 's-0001'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
    .unwrap()
}

fn payload(used: f64) -> String {
    json!({
        "session_id": "claude-abc",
        "cwd": "/tmp",
        "model": { "id": "claude-opus-4-1", "display_name": "Opus" },
        "context_window": {
            "used_percentage": used,
            "remaining_percentage": 100.0 - used,
            "context_window_size": 200000
        }
    })
    .to_string()
}

#[test]
fn statusline_records_ctx_pct_for_q_session() {
    let env = Env::new();
    seed_session(&env, None);
    env.q()
        .args(["hook", "statusline"])
        .env("Q_QUEST", "q-0001")
        .env("Q_SESSION", "s-0001")
        .write_stdin(payload(42.6))
        .assert()
        .success()
        .stdout("");

    let (pct, at, claude) = session_ctx(&env);
    assert_eq!(pct, Some(43));
    assert!(at.is_some_and(|t| t > 1), "ctx_updated_at set: {at:?}");
    assert_eq!(claude.as_deref(), Some("claude-abc"));
}

#[test]
fn statusline_resolves_by_claude_session_id_without_env() {
    let env = Env::new();
    seed_session(&env, Some("claude-abc"));
    env.q()
        .args(["hook", "statusline"])
        .env_remove("Q_QUEST")
        .env_remove("Q_SESSION")
        .write_stdin(payload(10.2))
        .assert()
        .success();
    assert_eq!(session_ctx(&env).0, Some(10));
}

#[test]
fn statusline_outside_a_quest_never_creates_the_db_and_still_chains() {
    let env = Env::new();
    env.q()
        .args(["config", "set", "statusline.chain", "echo chained"])
        .assert()
        .success();
    for q_session in [None, Some("s-0001")] {
        let mut cmd = env.q();
        cmd.args(["hook", "statusline"]).env_remove("Q_QUEST");
        match q_session {
            Some(id) => cmd.env("Q_SESSION", id),
            None => cmd.env_remove("Q_SESSION"),
        };
        cmd.write_stdin(payload(42.6))
            .assert()
            .success()
            .stdout("chained\n");
        assert!(!env.dir.path().join("q.db").exists(), "db created");
    }
}

#[test]
fn statusline_ignores_an_unrelated_claude_session() {
    let env = Env::new();
    seed_session(&env, None);
    env.q()
        .args(["hook", "statusline"])
        .env_remove("Q_QUEST")
        .env_remove("Q_SESSION")
        .write_stdin(payload(42.6))
        .assert()
        .success()
        .stdout("");
    assert_eq!(session_ctx(&env), (None, None, None));
}

#[test]
fn statusline_by_claude_id_requires_the_same_pane_when_tmux_pane_is_set() {
    let env = Env::new();
    seed_session(&env, Some("claude-abc"));
    let run = |pane: &str| {
        env.q()
            .args(["hook", "statusline"])
            .env_remove("Q_SESSION")
            .env("TMUX_PANE", pane)
            .write_stdin(payload(30.0))
            .assert()
            .success();
    };
    run("%7");
    assert_eq!(session_ctx(&env).0, None);
    run("%1");
    assert_eq!(session_ctx(&env).0, Some(30));
}

#[test]
fn statusline_replaces_a_stale_claude_session_id() {
    let env = Env::new();
    seed_session(&env, Some("claude-old"));
    env.q()
        .args(["hook", "statusline"])
        .env("Q_SESSION", "s-0001")
        .write_stdin(payload(5.0))
        .assert()
        .success();
    let (pct, _, claude) = session_ctx(&env);
    assert_eq!(pct, Some(5));
    assert_eq!(claude.as_deref(), Some("claude-abc"));
}

#[test]
fn statusline_leaves_an_ended_session_alone() {
    let env = Env::new();
    seed_session_with(&env, Some("claude-abc"), "ended");
    for q_session in [Some("s-0001"), None] {
        let mut cmd = env.q();
        cmd.args(["hook", "statusline"]);
        match q_session {
            Some(id) => cmd.env("Q_SESSION", id),
            None => cmd.env_remove("Q_SESSION"),
        };
        cmd.env_remove("TMUX_PANE")
            .write_stdin(payload(60.0))
            .assert()
            .success();
    }
    assert_eq!(session_ctx(&env).0, None);
}

#[test]
fn statusline_tolerates_bad_or_empty_input() {
    let env = Env::new();
    seed_session(&env, None);
    for input in ["", "not json", "[1,2]", "{\"context_window\": 5}"] {
        env.q()
            .args(["hook", "statusline"])
            .env("Q_SESSION", "s-0001")
            .write_stdin(input)
            .assert()
            .success()
            .stdout("");
    }
    assert_eq!(session_ctx(&env).0, None);

    env.q()
        .args(["config", "set", "statusline.chain", "echo chained"])
        .assert()
        .success();
    env.q()
        .args(["hook", "statusline"])
        .env("Q_SESSION", "s-0001")
        .write_stdin("garbage")
        .assert()
        .success()
        .stdout("chained\n");
}

#[test]
fn statusline_without_context_window_skips_the_write_but_chains() {
    let env = Env::new();
    seed_session(&env, None);
    env.q()
        .args(["config", "set", "statusline.chain", "cat"])
        .assert()
        .success();
    let input = json!({"session_id": "claude-abc", "cwd": "/tmp"}).to_string();
    env.q()
        .args(["hook", "statusline"])
        .env("Q_SESSION", "s-0001")
        .write_stdin(input.clone())
        .assert()
        .success()
        .stdout(input);
    assert_eq!(session_ctx(&env), (None, None, None));
}

#[test]
fn statusline_with_unknown_q_session_is_a_quiet_noop() {
    let env = Env::new();
    seed_session(&env, None);
    env.q()
        .args(["hook", "statusline"])
        .env("Q_SESSION", "s-nope")
        .write_stdin(payload(50.0))
        .assert()
        .success()
        .stdout("")
        .stderr("");
    assert_eq!(session_ctx(&env).0, None);

    // No database at all is fine too.
    let fresh = Env::new();
    fresh
        .q()
        .args(["hook", "statusline"])
        .env("Q_SESSION", "s-0001")
        .write_stdin(payload(50.0))
        .assert()
        .success()
        .stdout("");
}

// ------------------------------------------------------------ hook handlers

const HANDLERS: [&str; 6] = [
    "session-start",
    "user-prompt-submit",
    "stop",
    "notification",
    "pre-compact",
    "session-end",
];

#[test]
fn without_q_quest_handlers_are_silent_and_touch_nothing() {
    let env = Env::new();
    env.seed("idle");
    let before = env.session();
    for sub in HANDLERS {
        env.q()
            .env("Q_SESSION", "s-0001")
            .args(["hook", sub])
            .write_stdin(json!({ "session_id": "cs-1", "prompt": "hi" }).to_string())
            .assert()
            .success()
            .stdout("");
    }
    assert_eq!(env.session(), before);
    assert!(env.events().is_empty());
}

#[test]
fn session_start_records_identity_and_injects_the_brief() {
    let env = Env::new();
    env.seed("starting");
    let assert = env
        .hook(
            "session-start",
            &json!({ "session_id": "cs-42", "source": "startup", "cwd": "/tmp" }),
        )
        .success();
    let out: Value = serde_json::from_slice(&assert.get_output().stdout).unwrap();
    let hso = &out["hookSpecificOutput"];
    assert_eq!(hso["hookEventName"], "SessionStart");
    let ctx = hso["additionalContext"].as_str().unwrap();
    assert!(
        ctx.starts_with("You are running inside Quest `alpha` (q)"),
        "{ctx}"
    );
    assert!(ctx.contains("session `w1` (worker)"), "{ctx}");
    assert!(ctx.contains("# Quest q-0001 `alpha`"), "{ctx}");
    assert!(ctx.contains("## 1. Quest"), "{ctx}");
    assert!(ctx.contains("## 2. How you work here"), "{ctx}");

    let s = env.session();
    assert_eq!(s["status"], "idle");
    assert_eq!(s["claude_session_id"], "cs-42");
    assert!(s["claude_pid"].as_i64().unwrap() > 0);
    assert!(s["updated_at"].as_i64().unwrap() > 1);
    assert_eq!(
        env.events(),
        [("session.start".to_string(), json!({ "source": "startup" }))]
    );
}

#[test]
fn session_start_resumes_an_ended_session() {
    let env = Env::new();
    env.seed("ended");
    env.conn()
        .execute("UPDATE session SET ended_at = 99 WHERE id = 's-0001'", [])
        .unwrap();
    env.hook("session-start", &json!({ "source": "resume" }))
        .success();
    let s = env.session();
    assert_eq!(s["status"], "idle");
    assert_eq!(s["ended_at"], Value::Null);
}

#[test]
fn prompt_marks_busy_and_sets_first_prompt_once() {
    let env = Env::new();
    env.seed("idle");
    env.hook(
        "user-prompt-submit",
        &json!({ "prompt": "  first thing  " }),
    )
    .success()
    .stdout("");
    let s = env.session();
    assert_eq!(s["status"], "busy");
    assert_eq!(s["first_prompt"], "first thing");
    assert_eq!(s["last_prompt"], "first thing");

    let long = "x".repeat(700);
    env.hook("user-prompt-submit", &json!({ "prompt": long }))
        .success();
    let s = env.session();
    assert_eq!(s["first_prompt"], "first thing");
    let last = s["last_prompt"].as_str().unwrap();
    assert_eq!(last.chars().count(), 500, "{last}");
    assert!(last.ends_with('…'));

    let events = env.events();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].0, "session.prompt");
    assert_eq!(events[0].1["prompt"], "first thing");
    assert_eq!(events[1].1["prompt"].as_str().unwrap().chars().count(), 200);

    // No prompt text: still busy, the stored prompts stay.
    env.hook("stop", &json!({})).success();
    env.hook("user-prompt-submit", &json!({})).success();
    let s = env.session();
    assert_eq!(s["status"], "busy");
    assert_eq!(s["first_prompt"], "first thing");
    assert_eq!(s["last_prompt"], last);
    assert_eq!(env.events().len(), 4);
    assert_eq!(env.events()[3].1["prompt"], Value::Null);
}

#[test]
fn session_start_after_clear_overwrites_the_claude_session_id() {
    let env = Env::new();
    env.seed("idle");
    env.hook(
        "session-start",
        &json!({ "session_id": "cs-1", "source": "startup" }),
    )
    .success();
    assert_eq!(env.session()["claude_session_id"], "cs-1");
    env.hook(
        "session-start",
        &json!({ "session_id": "cs-2", "source": "clear" }),
    )
    .success();
    assert_eq!(env.session()["claude_session_id"], "cs-2");
    // A start without an id keeps the last known one.
    env.hook("session-start", &json!({ "source": "resume" }))
        .success();
    assert_eq!(env.session()["claude_session_id"], "cs-2");
    assert_eq!(env.events().len(), 3);
}

#[test]
fn non_blocking_notifications_only_log_an_event() {
    let env = Env::new();
    env.seed("idle");
    for kind in ["auth_success", "idle_prompt"] {
        env.hook(
            "notification",
            &json!({ "notification_type": kind, "message": "hello" }),
        )
        .success()
        .stdout("");
        let s = env.session();
        assert_eq!(s["status"], "idle", "{kind}");
        assert_eq!(s["waiting_for"], Value::Null, "{kind}");
    }
    env.hook(
        "notification",
        &json!({ "notification_type": "elicitation_dialog", "message": "Pick one" }),
    )
    .success();
    let s = env.session();
    assert_eq!(s["status"], "waiting");
    assert_eq!(s["waiting_for"], "input");

    let events = env.events();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].0, "session.notification");
    assert_eq!(events[0].1["type"], "auth_success");
    assert_eq!(events[0].1["waiting_for"], Value::Null);
    assert_eq!(events[1].0, "session.notification");
    assert_eq!(events[2].0, "session.waiting");
    assert_eq!(events[2].1["waiting_for"], "input");
}

#[test]
fn a_held_write_lock_makes_the_hook_give_up_quickly_and_atomically() {
    use std::time::{Duration, Instant};
    let env = Env::new();
    env.seed("idle");
    let holder = env.conn();
    holder
        .execute_batch("BEGIN IMMEDIATE; UPDATE quest SET updated_at = 2;")
        .unwrap();
    // Held well past the upper bound below so the hook is guaranteed to give
    // up (via busy_timeout) and return before the lock is ever released.
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        holder.execute_batch("COMMIT").unwrap();
    });

    let started = Instant::now();
    env.hook("user-prompt-submit", &json!({ "prompt": "blocked" }))
        .success()
        .stdout("");
    let took = started.elapsed();
    assert!(took >= Duration::from_millis(2500), "{took:?}");
    // Upper bound is intentionally loose (busy_timeout is ~3s) to absorb
    // wall-clock jitter on slower/loaded CI runners (observed up to ~4.3s on
    // macOS); it still proves the hook returned well before the 10s lock
    // release above, so nothing partially written is ever observed.
    assert!(took <= Duration::from_millis(8000), "{took:?}");
    release.join().unwrap();

    let s = env.session();
    assert_eq!(s["status"], "idle", "nothing partially written");
    assert_eq!(s["last_prompt"], Value::Null);
    assert!(env.events().is_empty());
}

#[test]
fn stop_notification_compact_and_end_update_status_and_log_events() {
    let env = Env::new();
    env.seed("busy");

    env.hook("stop", &json!({ "stop_hook_active": false }))
        .success()
        .stdout("");
    assert_eq!(env.session()["status"], "idle");

    env.hook(
        "notification",
        &json!({ "notification_type": "permission_prompt", "message": "Claude needs your permission to use Bash" }),
    )
    .success()
    .stdout("");
    let s = env.session();
    assert_eq!(s["status"], "waiting");
    assert_eq!(s["waiting_for"], "permission");

    env.hook(
        "notification",
        &json!({ "message": "Claude is waiting for your input" }),
    )
    .success();
    assert_eq!(env.session()["waiting_for"], "input");

    env.hook("pre-compact", &json!({ "trigger": "auto" }))
        .success()
        .stdout("");
    assert_eq!(
        env.session()["status"],
        "waiting",
        "compact leaves status alone"
    );

    env.hook("session-end", &json!({ "reason": "exit" }))
        .success()
        .stdout("");
    let s = env.session();
    assert_eq!(s["status"], "ended");
    assert_eq!(s["waiting_for"], Value::Null);
    assert!(s["ended_at"].as_i64().unwrap() > 1);

    let events = env.events();
    let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "session.stop",
            "session.waiting",
            "session.waiting",
            "session.compact",
            "session.end"
        ]
    );
    assert_eq!(events[0].1["stop_hook_active"], false);
    assert_eq!(events[1].1["type"], "permission_prompt");
    assert_eq!(events[1].1["waiting_for"], "permission");
    assert_eq!(events[2].1["type"], Value::Null);
    assert_eq!(events[2].1["waiting_for"], "input");
    assert_eq!(
        events[1].1["message"],
        "Claude needs your permission to use Bash"
    );
    assert_eq!(events[3].1["trigger"], "auto");
    assert_eq!(events[4].1["reason"], "exit");
}

#[test]
fn tmux_pane_resolves_the_session_when_q_session_is_missing() {
    let env = Env::new();
    env.seed("idle");
    env.q()
        .env("Q_QUEST", "q-0001")
        .env("TMUX_PANE", "%7")
        .args(["hook", "user-prompt-submit"])
        .write_stdin(json!({ "prompt": "via pane" }).to_string())
        .assert()
        .success();
    assert_eq!(env.session()["last_prompt"], "via pane");

    // An unknown pane, or a session from another Quest, is not ours.
    for (quest, pane) in [("q-0001", "%99"), ("q-other", "%7")] {
        env.q()
            .env("Q_QUEST", quest)
            .env("TMUX_PANE", pane)
            .args(["hook", "stop"])
            .write_stdin("{}")
            .assert()
            .success()
            .stdout("");
    }
    assert_eq!(env.session()["status"], "busy");
    assert_eq!(env.events().len(), 1);
}

#[test]
fn malformed_stdin_and_unknown_sessions_exit_zero_silently() {
    let env = Env::new();
    env.seed("idle");
    for sub in HANDLERS {
        env.q()
            .env("Q_QUEST", "q-0001")
            .env("Q_SESSION", "s-0001")
            .args(["hook", sub])
            .write_stdin("this is not json")
            .assert()
            .success();
        env.q()
            .env("Q_QUEST", "q-0001")
            .env("Q_SESSION", "s-nope")
            .args(["hook", sub])
            .write_stdin("{}")
            .assert()
            .success()
            .stdout("");
    }
    // Neither a malformed payload nor an unknown session leaves a trace.
    let events = env.events();
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(env.session()["status"], "idle");

    // No database at all: still silent, and the hook never creates one.
    let bare = Env::new();
    for sub in HANDLERS {
        bare.q()
            .env("Q_QUEST", "q-0001")
            .env("Q_SESSION", "s-0001")
            .args(["hook", sub])
            .write_stdin("{}")
            .assert()
            .success()
            .stdout("");
    }
    assert!(!bare.db().exists());
}

// ---------------------------------- Stop-hook auto-reset (bd-8lz.3.3)

impl Env {
    /// Quest `q-0001` (`alpha`) with a live **master** `s-0001` on pane `%7`,
    /// `idle`, at `ctx_pct`. What the `Stop` hook sees when a reset is due.
    fn seed_master(&self, ctx_pct: Option<i64>) {
        self.q().arg("list").assert().success();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO quest (id, slug, name_source, goal, cwd, machine, state, \
             created_at, updated_at) VALUES ('q-0001', 'alpha', 'manual', 'ship it', \
             '/tmp', 'laptop', 'active', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session (id, quest_id, role, label, tmux_session, tmux_pane, \
             status, ctx_pct, started_at, updated_at) VALUES ('s-0001', 'q-0001', 'master', \
             'master', 'q-alpha', '%7', 'busy', ?1, 1, 1)",
            [ctx_pct],
        )
        .unwrap();
    }

    fn set_quest(&self, sql: &str) {
        self.conn().execute(sql, []).unwrap();
    }

    fn kinds(&self) -> Vec<String> {
        self.events().into_iter().map(|(kind, _)| kind).collect()
    }
}

#[test]
fn stop_schedules_a_reset_once_the_master_crosses_the_threshold() {
    let env = Env::new();
    env.seed_master(Some(40));
    env.hook("stop", &json!({})).success().stdout("");

    assert_eq!(env.session()["status"], "idle");
    assert_eq!(env.kinds(), ["session.stop", "session.reset_scheduled"]);
    let payload = &env.events()[1].1;
    assert_eq!(payload["ctx_pct"], 40);
    assert_eq!(payload["threshold"], 35);
    assert_eq!(payload["strategy"], "clear");
    assert_eq!(payload["delay"], 2);
    // `$Q_FIXTURE` is set, so the detached child is described rather than run.
    assert_eq!(payload["spawned"], false);
    let argv: Vec<String> = payload["argv"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        argv[1..],
        [
            "reset",
            "s-0001",
            "--delay",
            "2",
            "--strategy",
            "clear",
            "--quiet"
        ]
    );

    // The cooldown means the next turn does not schedule a second one.
    env.hook("stop", &json!({})).success();
    assert_eq!(
        env.kinds(),
        ["session.stop", "session.reset_scheduled", "session.stop"]
    );
}

#[test]
fn stop_does_not_schedule_below_the_threshold_or_without_a_reading() {
    for ctx_pct in [None, Some(34)] {
        let env = Env::new();
        env.seed_master(ctx_pct);
        env.hook("stop", &json!({})).success();
        assert_eq!(env.kinds(), ["session.stop"], "{ctx_pct:?}");
    }
}

#[test]
fn stop_never_schedules_a_reset_for_a_worker() {
    let env = Env::new();
    // `seed` makes a worker; fill its context right up.
    env.seed("busy");
    env.conn()
        .execute("UPDATE session SET ctx_pct = 99 WHERE id = 's-0001'", [])
        .unwrap();
    env.hook("stop", &json!({})).success();
    assert_eq!(env.kinds(), ["session.stop"]);
}

#[test]
fn auto_reset_off_in_the_config_or_on_the_quest_stops_the_scheduling() {
    let env = Env::new();
    env.seed_master(Some(90));
    env.q()
        .args(["config", "set", "context.auto_reset", "false"])
        .assert()
        .success();
    env.hook("stop", &json!({})).success();
    assert_eq!(env.kinds(), ["session.stop"]);

    // The Quest column is a real override in both directions.
    env.set_quest("UPDATE quest SET auto_reset = 1 WHERE id = 'q-0001'");
    env.hook("stop", &json!({})).success();
    assert_eq!(
        env.kinds(),
        ["session.stop", "session.stop", "session.reset_scheduled"]
    );

    let env = Env::new();
    env.seed_master(Some(90));
    env.set_quest("UPDATE quest SET auto_reset = 0 WHERE id = 'q-0001'");
    env.hook("stop", &json!({})).success();
    assert_eq!(env.kinds(), ["session.stop"]);
}

#[test]
fn the_quest_threshold_overrides_the_configured_one() {
    let env = Env::new();
    env.seed_master(Some(50));
    env.q()
        .args(["config", "set", "context.master_reset_pct", "80"])
        .assert()
        .success();
    env.hook("stop", &json!({})).success();
    assert_eq!(env.kinds(), ["session.stop"]);

    env.set_quest("UPDATE quest SET ctx_reset_pct = 45 WHERE id = 'q-0001'");
    env.hook("stop", &json!({})).success();
    assert_eq!(env.events()[2].1["threshold"], 45);
}
