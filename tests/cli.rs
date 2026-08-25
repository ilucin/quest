use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A `Command` paired with its own `TempDir`, so `Q_DB`/`Q_CONFIG` never
/// touch real user state and each test gets an isolated directory.
struct TestCmd {
    dir: TempDir,
    cmd: Command,
}

impl std::ops::Deref for TestCmd {
    type Target = Command;
    fn deref(&self) -> &Command {
        &self.cmd
    }
}

impl std::ops::DerefMut for TestCmd {
    fn deref_mut(&mut self) -> &mut Command {
        &mut self.cmd
    }
}

fn q() -> TestCmd {
    let dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("q").unwrap();
    cmd.env("Q_DB", dir.path().join("q.db"))
        .env("Q_CONFIG", dir.path().join("config.toml"));
    TestCmd { dir, cmd }
}

#[test]
fn version_prints_crate_version() {
    q().arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_lists_subcommands() {
    let assert = q().arg("--help").assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for sub in [
        "new", "list", "show", "enter", "close", "resume", "rename", "set", "rm", "doctor",
        "config",
    ] {
        assert!(out.contains(sub), "`{sub}` missing from --help:\n{out}");
    }
}

#[test]
fn bare_invocation_succeeds() {
    q().assert()
        .success()
        .stdout(predicate::str::contains("--help"));
}

#[test]
fn quiet_suppresses_bare_output() {
    // `--quiet` only silences human output; without `--json` that leaves
    // nothing to print.
    q().arg("--quiet").assert().success().stdout("");
}

#[test]
fn quiet_does_not_suppress_json_output() {
    let assert = q().args(["--quiet", "--json"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert!(
        parsed["version"].is_string(),
        "unexpected payload: {parsed}"
    );
}

#[test]
fn stub_command_fails_with_not_implemented() {
    q().arg("list")
        .assert()
        .code(1)
        .stderr(predicate::str::starts_with("error: "))
        .stderr(predicate::str::contains("not implemented"));
}

#[test]
fn stub_command_json_error_goes_to_stderr() {
    let assert = q().args(["list", "--json"]).assert().code(1).stdout("");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("not implemented"),
        "unexpected payload: {parsed}"
    );
    assert_eq!(parsed["code"], "not_implemented");
}

#[test]
fn unknown_command_is_a_usage_error() {
    q().arg("definitely-not-a-command").assert().code(2);
}

#[test]
fn unknown_command_json_is_usage_error_json() {
    let assert = q()
        .args(["--json", "definitely-not-a-command"])
        .assert()
        .code(2)
        .stdout("");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "usage");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("definitely-not-a-command"),
        "unexpected payload: {parsed}"
    );
}

/// `q()` builds the command; this returns the config path it points at.
fn config_path(cmd: &TestCmd) -> std::path::PathBuf {
    cmd.dir.path().join("config.toml")
}

fn json_of(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {stdout}"))
}

#[test]
fn config_path_honors_q_config() {
    let mut cmd = q();
    let path = config_path(&cmd);
    cmd.args(["config", "path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(path.to_str().unwrap()));
}

#[test]
fn config_path_json_reports_existence() {
    let mut cmd = q();
    let path = config_path(&cmd);
    let assert = cmd.args(["config", "path", "--json"]).assert().success();
    let parsed = json_of(&assert);
    assert_eq!(parsed["path"], path.to_str().unwrap());
    assert_eq!(parsed["exists"], false);
}

#[test]
fn config_get_without_a_file_prints_the_defaults_as_toml() {
    let mut cmd = q();
    let path = config_path(&cmd);
    cmd.args(["config", "get"])
        .assert()
        .success()
        .stdout(predicate::str::contains("master_reset_pct = 35"))
        .stdout(predicate::str::contains("session_prefix = \"q-\""));
    // Reading must not create the file.
    assert!(!path.exists());
}

#[test]
fn config_get_json_is_parseable() {
    let assert = q().args(["config", "get", "--json"]).assert().success();
    let parsed = json_of(&assert);
    assert_eq!(parsed["context"]["master_reset_pct"], 35);
    assert_eq!(parsed["beads"]["default_repo_label"], "global");
    assert!(parsed["machine"]["name"].is_string());
}

#[test]
fn config_set_then_get_round_trips() {
    let mut cmd = q();
    let path = config_path(&cmd);
    cmd.args(["config", "set", "machine.name", "foo"])
        .assert()
        .success();
    assert!(path.exists());

    let mut cmd = q();
    // Point the second invocation at the file the first one wrote.
    cmd.env("Q_CONFIG", &path);
    let assert = cmd
        .args(["config", "get", "machine.name", "--json"])
        .assert()
        .success();
    let parsed = json_of(&assert);
    assert_eq!(parsed["key"], "machine.name");
    assert_eq!(parsed["value"], "foo");
}

#[test]
fn config_set_coerces_ints_and_bools() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("q").unwrap();
        cmd.env("Q_CONFIG", &path)
            .env("Q_DB", dir.path().join("q.db"));
        cmd.args(args).assert()
    };
    run(&["config", "set", "context.master_reset_pct", "42"]).success();
    run(&["config", "set", "ui.mouse", "false"]).success();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("master_reset_pct = 42"), "{text}");
    assert!(text.contains("mouse = false"), "{text}");
}

#[test]
fn config_set_with_an_invalid_value_fails_and_writes_nothing() {
    let mut cmd = q();
    let path = config_path(&cmd);
    let assert = cmd
        .args([
            "config",
            "set",
            "context.master_reset_pct",
            "nope",
            "--json",
        ])
        .assert()
        .code(1)
        .stdout("");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "config");
    assert!(!path.exists(), "config file was written despite the error");
}

#[test]
fn config_set_out_of_range_fails_and_leaves_the_file_untouched() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    let run = |args: &[&str]| {
        let mut cmd = Command::cargo_bin("q").unwrap();
        cmd.env("Q_CONFIG", &path)
            .env("Q_DB", dir.path().join("q.db"));
        cmd.args(args).assert()
    };
    run(&["config", "set", "machine.name", "keep"]).success();
    let before = std::fs::read_to_string(&path).unwrap();
    run(&["config", "set", "context.worker_warn_pct", "0"])
        .code(1)
        .stderr(predicate::str::contains("between 1 and 100"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
}

#[test]
fn config_get_unknown_key_is_not_found() {
    let assert = q()
        .args(["config", "get", "nope.nope", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
}

#[test]
fn config_rejects_an_unknown_key_in_the_file() {
    let mut cmd = q();
    let path = config_path(&cmd);
    std::fs::write(&path, "[tmux]\nsession_prefx = \"q-\"\n").unwrap();
    cmd.args(["config", "get"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("session_prefx"))
        .stderr(predicate::str::contains(path.to_str().unwrap()));
}

#[test]
fn config_path_still_works_with_a_broken_file() {
    let mut cmd = q();
    let path = config_path(&cmd);
    std::fs::write(&path, "[machine\n").unwrap();
    cmd.args(["config", "path"]).assert().success();
}

#[test]
fn config_edit_creates_the_file_and_validates_it() {
    let mut cmd = q();
    let path = config_path(&cmd);
    cmd.env("VISUAL", "true") // a no-op "editor"
        .args(["config", "edit"])
        .assert()
        .success();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[machine]"), "{text}");
}

#[test]
fn config_edit_reports_an_invalid_result() {
    let mut cmd = q();
    let path = config_path(&cmd);
    std::fs::write(&path, "[context]\nreset_strategy = \"nuke\"\n").unwrap();
    cmd.env("VISUAL", "true")
        .args(["config", "edit"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("reset_strategy"));
}

#[test]
fn machine_flag_is_a_targeting_flag_not_reflected_by_config_get() {
    // `--machine` targets a remote for this invocation (SPEC §15); it must
    // not change what `config get` reports for `machine.name`.
    let baseline = q()
        .args(["config", "get", "machine.name", "--json"])
        .assert()
        .success();
    let default_name = json_of(&baseline)["value"].clone();

    let assert = q()
        .args(["--machine", "ws", "config", "get", "machine.name", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["value"], default_name);
}

#[test]
fn machine_flag_is_validated() {
    q().args(["--machine", "Not Valid", "config", "get"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("machine.name"));
}

#[test]
fn machine_flag_is_not_persisted_by_config_set() {
    let mut cmd = q();
    let path = config_path(&cmd);
    cmd.args(["--machine", "ws", "config", "set", "ui.mouse", "false"])
        .assert()
        .success();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("name = \"ws\""), "{text}");
    assert!(text.contains("mouse = false"), "{text}");
}

#[test]
fn q_config_in_a_nonexistent_nested_dir_reads_defaults() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a").join("b").join("config.toml");
    let mut cmd = Command::cargo_bin("q").unwrap();
    cmd.env("Q_CONFIG", &path)
        .env("Q_DB", dir.path().join("q.db"));
    cmd.args(["config", "get", "--json"]).assert().success();
    assert!(!path.exists(), "reading must not create the nested dirs");
}

#[test]
fn q_config_in_a_nonexistent_nested_dir_creates_it_on_set() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("a").join("b").join("config.toml");
    let mut cmd = Command::cargo_bin("q").unwrap();
    cmd.env("Q_CONFIG", &path)
        .env("Q_DB", dir.path().join("q.db"));
    cmd.args(["config", "set", "machine.name", "ws"])
        .assert()
        .success();
    assert!(path.exists());
    assert!(std::fs::read_to_string(&path).unwrap().contains("ws"));
}

#[test]
fn remotes_table_survives_a_cli_config_set_of_another_key() {
    let mut cmd = q();
    let path = config_path(&cmd);
    std::fs::write(&path, "[[remotes]]\nname = \"ws\"\nssh = \"ws.local\"\n").unwrap();
    cmd.args(["config", "set", "ui.mouse", "false"])
        .assert()
        .success();

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("mouse = false"), "{text}");
    assert!(text.contains("[[remotes]]"), "{text}");
    assert!(text.contains("name = \"ws\""), "{text}");
    assert!(text.contains("ssh = \"ws.local\""), "{text}");

    let mut cmd = q();
    cmd.env("Q_CONFIG", &path);
    let assert = cmd
        .args(["config", "get", "remotes", "--json"])
        .assert()
        .success();
    assert_eq!(
        json_of(&assert)["value"],
        serde_json::json!([{ "name": "ws", "ssh": "ws.local" }])
    );
}

#[test]
fn config_get_remotes_defaults_to_an_empty_array() {
    let assert = q()
        .args(["config", "get", "remotes", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["value"], serde_json::json!([]));
}

#[test]
fn config_get_json_always_has_a_remotes_key() {
    let assert = q().args(["config", "get", "--json"]).assert().success();
    assert!(json_of(&assert)["remotes"].is_array());
}
