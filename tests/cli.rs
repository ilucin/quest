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
        .env("Q_CONFIG", dir.path().join("config.toml"))
        // Never a real tmux server, in any test.
        .env("Q_FIXTURE", dir.path().join("tmux.json"));
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

#[test]
fn config_set_repairs_an_invalid_file() {
    let mut cmd = q();
    let path = cmd.dir.path().join("config.toml");
    std::fs::write(&path, "[context]\nreset_strategy = \"nuke\"\n").unwrap();
    cmd.args(["config", "set", "context.reset_strategy", "clear"])
        .assert()
        .success();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("reset_strategy = \"clear\""));
}

/// `q()` builds the command; this returns the database path it points at.
fn db_path(cmd: &TestCmd) -> std::path::PathBuf {
    cmd.dir.path().join("q.db")
}

#[test]
fn config_works_without_touching_the_database() {
    let dir = TempDir::new().unwrap();
    // Nothing under `nope/` exists, and `Cargo.toml` is a file, so this path
    // can neither be opened nor created — and opening it would leave the
    // directory chain behind even where it can be created.
    let db = dir.path().join("nope").join("x").join("q.db");
    let config = dir.path().join("config.toml");

    for args in [
        vec!["config"],
        vec!["config", "get"],
        vec!["config", "get", "machine.name"],
        vec!["config", "set", "machine.name", "laptop"],
        vec!["config", "edit"],
        vec!["config", "path"],
    ] {
        let mut cmd = Command::cargo_bin("q").unwrap();
        cmd.env("Q_DB", &db)
            .env("Q_CONFIG", &config)
            // `q config edit` shells out; `true` is the do-nothing editor.
            .env("EDITOR", "true")
            .env_remove("VISUAL");
        cmd.args(&args).assert().success();
        assert!(
            !db.exists(),
            "`q {}` created {}",
            args.join(" "),
            db.display()
        );
        assert!(
            !dir.path().join("nope").exists(),
            "`q {}` created the Q_DB parent directory",
            args.join(" ")
        );
    }
}

#[test]
fn a_command_that_needs_the_database_reports_a_bad_q_db() {
    let dir = TempDir::new().unwrap();
    let mut cmd = Command::cargo_bin("q").unwrap();
    let unwritable = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml")
        .join("nested")
        .join("q.db");
    cmd.env("Q_DB", &unwritable)
        .env("Q_CONFIG", dir.path().join("config.toml"));
    let assert = cmd.args(["list", "--json"]).assert().code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "db");
}

#[test]
fn a_command_creates_the_database_at_q_db_in_wal_mode() {
    let mut cmd = q();
    let path = db_path(&cmd);
    assert!(!path.exists());
    // `list` is still a stub, but it goes through the same Ctx as every real
    // command, so the database is opened and migrated.
    cmd.arg("list").assert().code(1);
    assert!(path.exists(), "{} was not created", path.display());

    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert!(
        version >= 1,
        "schema was not migrated (user_version {version})"
    );
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('quest','session','event','link','template','name_cache')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 6);
}

#[test]
fn the_database_is_created_under_a_missing_directory() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("state").join("q").join("q.db");
    let mut cmd = Command::cargo_bin("q").unwrap();
    cmd.env("Q_DB", &path)
        .env("Q_CONFIG", dir.path().join("config.toml"));
    cmd.arg("list").assert().code(1);
    assert!(path.exists());
}

// ------------------------------------------------------------------- q new

/// Several invocations sharing one database, config and tmux fixture.
struct Env {
    dir: TempDir,
}

impl Env {
    fn new() -> Env {
        Env {
            dir: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = Command::cargo_bin("q").unwrap();
        cmd.env("Q_DB", self.dir.path().join("q.db"))
            .env("Q_CONFIG", self.dir.path().join("config.toml"))
            .env("Q_FIXTURE", self.dir.path().join("tmux.json"))
            // The attach mode depends on it, so it never leaks in from the
            // terminal `cargo test` happens to run in.
            .env_remove("TMUX");
        cmd
    }

    /// A directory to hand to `--dir`, canonicalized like `q new` does.
    fn work(&self, name: &str) -> std::path::PathBuf {
        let path = self.dir.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }

    fn fixture(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.dir.path().join("tmux.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn write_fixture(&self, state: serde_json::Value) {
        std::fs::write(self.dir.path().join("tmux.json"), state.to_string()).unwrap();
    }

    fn conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.path().join("q.db")).unwrap()
    }
}

/// The one pane of `session`, from the fixture.
fn pane_of(fixture: &serde_json::Value, session: &str) -> serde_json::Value {
    let panes = fixture["panes"].as_array().unwrap();
    let mut found = panes.iter().filter(|p| p["session_name"] == session);
    let pane = found
        .next()
        .unwrap_or_else(|| panic!("no pane in `{session}`: {fixture}"));
    assert!(found.next().is_none(), "more than one pane in `{session}`");
    pane.clone()
}

#[test]
fn new_creates_the_quest_the_tmux_session_and_the_master_window() {
    let env = Env::new();
    let work = env.work("repo");
    let assert = env
        .cmd()
        .args(["new", "--name", "foo", "--goal", "ship it"])
        .args(["--workflow", "tdd", "--dir", work.to_str().unwrap()])
        .args(["-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);

    assert_eq!(out["quest"]["slug"], "foo");
    assert_eq!(out["quest"]["goal"], "ship it");
    assert_eq!(out["quest"]["workflow"], "tdd");
    assert_eq!(out["quest"]["state"], "active");
    assert_eq!(out["quest"]["name_source"], "manual");
    assert_eq!(out["quest"]["cwd"], work.to_str().unwrap());
    assert_eq!(out["tmux_session"], "q-foo");
    assert_eq!(out["attach"], "none");
    assert_eq!(out["session"]["role"], "master");
    assert_eq!(out["session"]["label"], "master");
    assert_eq!(out["session"]["status"], "starting");
    let quest_id = out["quest"]["id"].as_str().unwrap().to_string();
    let session_id = out["session"]["id"].as_str().unwrap().to_string();

    let fixture = env.fixture();
    assert_eq!(fixture["attached"], serde_json::Value::Null);
    let pane = pane_of(&fixture, "q-foo");
    assert_eq!(pane["window_name"], "master");
    assert_eq!(pane["window_index"], 0);
    assert_eq!(pane["env"]["Q_QUEST"], quest_id.as_str());
    assert_eq!(pane["env"]["Q_SESSION"], session_id.as_str());
    assert_eq!(pane["env"]["Q_ROLE"], "master");
    assert!(pane["env"]["Q_MACHINE"].is_string());
    // The test environment sets both overrides, so the window inherits them.
    assert!(pane["env"]["Q_DB"].is_string());
    assert!(pane["env"]["Q_CONFIG"].is_string());
    assert_eq!(pane["command"], "claude -n foo/master");
    assert_eq!(out["session"]["tmux_pane"], pane["pane_id"]);

    let conn = env.conn();
    let (slug, cwd, state): (String, String, String) = conn
        .query_row(
            "SELECT slug, cwd, state FROM quest WHERE id = ?1",
            [&quest_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (slug.as_str(), cwd.as_str(), state.as_str()),
        ("foo", work.to_str().unwrap(), "active")
    );
    let (q_id, role, tmux_session): (String, String, String) = conn
        .query_row(
            "SELECT quest_id, role, tmux_session FROM session WHERE id = ?1",
            [&session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (q_id.as_str(), role.as_str(), tmux_session.as_str()),
        (quest_id.as_str(), "master", "q-foo")
    );
    let kinds: Vec<String> = conn
        .prepare("SELECT kind FROM event WHERE quest_id = ?1")
        .unwrap()
        .query_map([&quest_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(kinds, vec!["quest.created".to_string()]);
    let payload: String = conn
        .query_row(
            "SELECT payload FROM event WHERE quest_id = ?1",
            [&quest_id],
            |r| r.get(0),
        )
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["goal"], "ship it");
    assert_eq!(payload["slug"], "foo");
}

#[test]
fn new_without_a_name_slugifies_the_directory() {
    let env = Env::new();
    let work = env.work("some-work-dir");
    let assert = env
        .cmd()
        .args(["new", "--dir", work.to_str().unwrap(), "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["slug"], "some-work-dir");
    assert_eq!(out["quest"]["name_source"], "auto");
    assert_eq!(out["tmux_session"], "q-some-work-dir");
}

#[test]
fn new_prints_a_human_one_liner() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("created quest q-"))
        .stdout(predicate::str::contains("tmux q-foo"))
        .stdout(predicate::str::contains("q enter foo"));
}

#[test]
fn new_is_silent_under_quiet() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
            "-q",
        ])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn new_attaches_to_the_master_window_unless_detached() {
    let env = Env::new();
    let work = env.work("repo");
    let assert = env
        .cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "--json",
        ])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["attach"], "exec");
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo", "master"])
    );
}

#[test]
fn new_inside_tmux_reports_a_switch_instead_of_an_exec() {
    let env = Env::new();
    let work = env.work("repo");
    let assert = env
        .cmd()
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .args(["new", "--name", "foo", "--dir", work.to_str().unwrap()])
        .arg("--json")
        .assert()
        .success();
    assert_eq!(json_of(&assert)["attach"], "switch");
}

#[test]
fn new_forwards_absolute_overrides_even_when_they_are_relative() {
    let env = Env::new();
    let work = env.work("repo");
    Command::cargo_bin("q")
        .unwrap()
        .env("Q_DB", "state/q.db")
        .env("Q_CONFIG", "state/config.toml")
        .env("Q_FIXTURE", env.dir.path().join("tmux.json"))
        .env_remove("TMUX")
        .current_dir(env.dir.path())
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .success();
    let pane = pane_of(&env.fixture(), "q-foo");
    for key in ["Q_DB", "Q_CONFIG"] {
        let value = pane["env"][key].as_str().unwrap();
        assert!(value.starts_with('/'), "{key} is relative: {value}");
    }
}

#[test]
fn new_without_a_dir_uses_the_current_one() {
    let env = Env::new();
    let work = env.work("cwd-repo");
    let assert = env
        .cmd()
        .current_dir(&work)
        .args(["new", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["slug"], "cwd-repo");
    assert_eq!(out["quest"]["cwd"], work.to_str().unwrap());
}

#[test]
fn new_names_an_auto_quest_after_the_git_branch() {
    let env = Env::new();
    let work = env.work("repo");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(["-C", work.to_str().unwrap()])
            .args(args)
            .output()
    };
    let Ok(init) = git(&["init", "-q"]) else {
        eprintln!("skipping: no git binary");
        return;
    };
    assert!(init.status.success(), "git init failed: {init:?}");
    for args in [
        &["checkout", "-q", "-b", "feature/ABC-1"][..],
        &[
            "-c",
            "user.email=q@example.com",
            "-c",
            "user.name=q",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ][..],
    ] {
        let out = git(args).unwrap();
        assert!(out.status.success(), "`git {args:?}` failed: {out:?}");
    }

    let assert = env
        .cmd()
        .args(["new", "--dir", work.to_str().unwrap(), "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["slug"], "feature-abc-1");
    assert_eq!(out["quest"]["name_source"], "auto");
}

#[test]
fn new_steps_an_auto_slug_aside_instead_of_failing() {
    let env = Env::new();
    let work = env.work("busy");
    let mut slugs = Vec::new();
    for _ in 0..3 {
        let assert = env
            .cmd()
            .args(["new", "--dir", work.to_str().unwrap(), "-d", "--json"])
            .assert()
            .success();
        let out = json_of(&assert);
        assert_eq!(out["quest"]["name_source"], "auto");
        slugs.push(out["quest"]["slug"].as_str().unwrap().to_string());
    }
    assert_eq!(slugs, vec!["busy", "busy-2", "busy-3"]);
    assert!(env.fixture()["panes"].as_array().unwrap().len() == 3);
}

#[test]
fn new_json_errors_carry_stable_codes() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["new", "--name", "taken", "--dir", work.to_str().unwrap()])
        .args(["-d", "--json"])
        .assert()
        .success();

    let missing = env.dir.path().join("nope");
    let cases = [
        (
            vec!["--name", "taken", "--dir", work.to_str().unwrap()],
            "conflict",
        ),
        (
            vec!["--name", "Not A Slug", "--dir", work.to_str().unwrap()],
            "invalid",
        ),
        (
            vec!["--name", "fresh", "--dir", missing.to_str().unwrap()],
            "not_found",
        ),
    ];
    for (args, code) in cases {
        let assert = env
            .cmd()
            .arg("new")
            .args(&args)
            .args(["-d", "--json"])
            .assert()
            .code(1);
        let err: serde_json::Value = serde_json::from_slice(&assert.get_output().stderr).unwrap();
        assert_eq!(err["code"], code, "for `q new {args:?}`: {err}");
        assert!(err["error"].is_string());
    }
}

#[test]
fn new_refuses_a_slug_another_quest_already_uses() {
    let env = Env::new();
    let work = env.work("repo");
    let first = env
        .cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
            "--json",
        ])
        .assert()
        .success();
    let id = json_of(&first)["quest"]["id"].as_str().unwrap().to_string();

    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already taken by quest"))
        .stderr(predicate::str::contains(id))
        .stderr(predicate::str::contains("--name"));
}

#[test]
fn new_refuses_a_slug_whose_tmux_session_exists() {
    let env = Env::new();
    let work = env.work("repo");
    env.write_fixture(serde_json::json!({
        "next_pane": 1,
        "panes": [{ "pane_id": "%1", "session_name": "q-foo", "window_name": "master" }],
    }));
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "tmux session `q-foo` already exists",
        ));
    // The failed attempt must not have left a Quest behind.
    let quests: i64 = env
        .conn()
        .query_row("SELECT count(*) FROM quest", [], |r| r.get(0))
        .unwrap();
    assert_eq!(quests, 0);
}

#[test]
fn new_rejects_an_invalid_slug() {
    let env = Env::new();
    let work = env.work("repo");
    for bad in ["Foo", "with space", "trailing-", "under_score"] {
        env.cmd()
            .args(["new", "--name", bad, "--dir", work.to_str().unwrap(), "-d"])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("invalid slug"))
            .stderr(predicate::str::contains("^[a-z0-9]+(-[a-z0-9]+)*$"));
    }
}

#[test]
fn new_rejects_a_directory_that_is_not_there() {
    let env = Env::new();
    let missing = env.dir.path().join("nope");
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            missing.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no such directory"));
}

#[test]
fn new_rejects_a_file_as_the_directory() {
    let env = Env::new();
    let file = env.dir.path().join("a-file");
    std::fs::write(&file, "x").unwrap();
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            file.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a directory"));
}

#[test]
fn new_passes_the_prompt_to_claude() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .args(["--prompt", "fix it; don't 'break' it"])
        .assert()
        .success();
    assert_eq!(
        pane_of(&env.fixture(), "q-foo")["command"],
        r#"claude -n foo/master -- 'fix it; don'\''t '\''break'\'' it'"#
    );
    let first: String = env
        .conn()
        .query_row("SELECT first_prompt FROM session", [], |r| r.get(0))
        .unwrap();
    assert_eq!(first, "fix it; don't 'break' it");
}

#[test]
fn new_reads_the_prompt_file_from_stdin() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .args(["--prompt-file", "-"])
        .write_stdin("from stdin\n")
        .assert()
        .success();
    assert_eq!(
        pane_of(&env.fixture(), "q-foo")["command"],
        "claude -n foo/master -- 'from stdin'"
    );
}

#[test]
fn new_reads_the_prompt_from_a_file() {
    let env = Env::new();
    let work = env.work("repo");
    let file = env.dir.path().join("prompt.md");
    std::fs::write(&file, "from a file").unwrap();
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .args(["--prompt-file", file.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(
        pane_of(&env.fixture(), "q-foo")["command"],
        "claude -n foo/master -- 'from a file'"
    );
}

#[test]
fn new_rejects_prompt_together_with_prompt_file() {
    let env = Env::new();
    env.cmd()
        .args([
            "new",
            "--name",
            "foo",
            "--prompt",
            "a",
            "--prompt-file",
            "-",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("--prompt"));
}

#[test]
fn new_help_only_lists_the_implemented_flags() {
    let assert = q().args(["new", "--help"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for flag in [
        "--name",
        "--goal",
        "--dir",
        "--workflow",
        "--prompt",
        "--prompt-file",
        "--detach",
    ] {
        assert!(
            out.contains(flag),
            "`{flag}` missing from `q new --help`:\n{out}"
        );
    }
    for later in [
        "--repo",
        "--brain",
        "--no-beads",
        "--template",
        "--from-brief",
    ] {
        assert!(
            !out.contains(later),
            "`{later}` is not implemented yet:\n{out}"
        );
    }
}
