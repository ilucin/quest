//! `q skill install | uninstall | status`, and the `q doctor` skill check,
//! against a sandboxed `~/.claude/skills/q/SKILL.md`.

use std::fs;
use std::path::PathBuf;

use assert_cmd::Command;
use serde_json::Value;
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

    /// The sandboxed q skill file: `$Q_CLAUDE_SKILL`, never the real ~/.claude.
    fn skill(&self) -> PathBuf {
        self.dir
            .path()
            .join("claude")
            .join("skills")
            .join("q")
            .join("SKILL.md")
    }

    fn config(&self) -> PathBuf {
        self.dir.path().join("config.toml")
    }

    fn q(&self) -> Command {
        let mut cmd = Command::cargo_bin("q").unwrap();
        cmd.env("Q_DB", self.dir.path().join("q.db"))
            .env("Q_CONFIG", self.config())
            .env("Q_FIXTURE", self.dir.path().join("tmux.json"))
            .env("Q_CLAUDE_SKILL", self.skill())
            .env("Q_CLAUDE_SETTINGS", self.dir.path().join("settings.json"))
            .env("Q_CLAUDE_SESSIONS_DIR", self.dir.path().join("registry"))
            .env_remove("Q_QUEST")
            .env_remove("Q_SESSION")
            .env_remove("TMUX_PANE");
        cmd
    }

    fn status_json(&self) -> Value {
        let out = self
            .q()
            .args(["--json", "skill", "status"])
            .output()
            .unwrap();
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn install_writes_the_skill_and_is_idempotent() {
    let env = Env::new();
    assert!(!env.skill().exists());

    // First install writes the file into a directory it creates.
    let out = env
        .q()
        .args(["--json", "skill", "install"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["action"], "install");
    assert_eq!(v["changed"], true);
    assert_eq!(v["status"]["state"], "installed");
    let body = fs::read_to_string(env.skill()).unwrap();
    assert!(body.contains("confirmation"));

    // Second install is a no-op: nothing changed, and the file is untouched.
    let out = env
        .q()
        .args(["--json", "skill", "install"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["changed"], false);
    assert_eq!(v["status"]["state"], "installed");
    assert_eq!(fs::read_to_string(env.skill()).unwrap(), body);
}

#[test]
fn status_tracks_not_installed_then_installed_then_out_of_date() {
    let env = Env::new();

    // Not installed: missing, and a non-zero exit.
    env.q().args(["skill", "status"]).assert().code(1);
    let v = env.status_json();
    assert_eq!(v["state"], "missing");
    assert!(v["actual_hash"].is_null());

    // Installed: matches, exit 0.
    env.q().args(["skill", "install"]).assert().success();
    env.q().args(["skill", "status"]).assert().success();
    let v = env.status_json();
    assert_eq!(v["state"], "installed");
    assert_eq!(v["actual_hash"], v["expected_hash"]);

    // A hand-edited file drifts: out of date (hash mismatch), non-zero exit.
    fs::write(env.skill(), "tampered\n").unwrap();
    env.q().args(["skill", "status"]).assert().code(1);
    let v = env.status_json();
    assert_eq!(v["state"], "drifted");
    assert_ne!(v["actual_hash"], v["expected_hash"]);

    // Reinstall repairs the drift.
    env.q().args(["skill", "install"]).assert().success();
    assert_eq!(env.status_json()["state"], "installed");
}

#[test]
fn uninstall_removes_only_the_q_owned_path() {
    let env = Env::new();
    env.q().args(["skill", "install"]).assert().success();
    assert!(env.skill().exists());

    // A sibling skill under skills/ must survive: uninstall touches only q's.
    let other = env
        .dir
        .path()
        .join("claude")
        .join("skills")
        .join("other")
        .join("SKILL.md");
    fs::create_dir_all(other.parent().unwrap()).unwrap();
    fs::write(&other, "not ours\n").unwrap();

    let out = env
        .q()
        .args(["--json", "skill", "uninstall"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["removed"], true);

    // q's file and its now-empty directory are gone; the sibling remains.
    assert!(!env.skill().exists());
    assert!(!env.skill().parent().unwrap().exists());
    assert!(other.exists());

    // A second uninstall reports nothing to remove and still succeeds.
    let out = env
        .q()
        .args(["--json", "skill", "uninstall"])
        .assert()
        .success();
    let v: Value = serde_json::from_slice(&out.get_output().stdout).unwrap();
    assert_eq!(v["removed"], false);
}

#[test]
fn doctor_reports_the_skill_and_fix_installs_it() {
    let env = Env::new();

    // Missing: doctor's skill check fails with the install hint.
    let out = env.q().args(["--json", "doctor"]).output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let skill = find_check(&report, "skill");
    assert_eq!(skill["status"], "fail");
    assert_eq!(skill["fix_hint"], "q skill install");

    // `--fix` installs it and records the repair.
    let out = env
        .q()
        .args(["--json", "doctor", "--fix"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(find_check(&report, "skill")["status"], "ok");
    assert!(env.skill().exists());
    let fixed = report["fixed"].as_array().unwrap();
    assert!(
        fixed.iter().any(|f| f.as_str().unwrap().contains("skill")),
        "expected a skill line in {fixed:?}"
    );

    // A drifted file is a doctor failure, and `--fix` refreshes it.
    fs::write(env.skill(), "stale\n").unwrap();
    let out = env.q().args(["--json", "doctor"]).output().unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(find_check(&report, "skill")["status"], "fail");

    let out = env
        .q()
        .args(["--json", "doctor", "--fix"])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(find_check(&report, "skill")["status"], "ok");
}

/// The doctor check named `name`.
fn find_check<'a>(report: &'a Value, name: &str) -> &'a Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` check in the doctor report"))
}
