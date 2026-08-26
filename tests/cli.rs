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
    // `Q_FIXTURE` keeps every tmux call in the fake backend; no test may reach
    // a real tmux server.
    cmd.env("Q_DB", dir.path().join("q.db"))
        .env("Q_CONFIG", dir.path().join("config.toml"))
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
        "new", "list", "show", "enter", "close", "resume", "rename", "set", "rm", "brief",
        "doctor", "config",
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
    // `list` goes through the same Ctx as every real command, so the database
    // is opened and migrated.
    cmd.arg("list").assert().success();
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
    cmd.arg("list").assert().success();
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
            // The attach mode and the confirm refusal depend on them, so
            // neither leaks in from the terminal `cargo test` runs in.
            .env_remove("TMUX")
            .env_remove("Q_QUEST");
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

// ------------------------------------------------------------------- q doctor

/// A no-op executable named `name`, so a check that only looks for a binary
/// on `PATH` has a deterministic answer.
fn stub_exe(dir: &std::path::Path, name: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `q` with a `PATH` holding exactly `names` — `PATH`-dependent checks then
/// report the same thing on every machine.
fn doctor(names: &[&str]) -> TestCmd {
    let mut cmd = q();
    let bin = cmd.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in names {
        stub_exe(&bin, name);
    }
    cmd.env("PATH", &bin);
    cmd
}

fn fixture_path(cmd: &TestCmd) -> std::path::PathBuf {
    cmd.dir.path().join("tmux.json")
}

fn check<'a>(report: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .unwrap_or_else(|| panic!("no `{name}` check in {report}"))
}

/// A quest with one live session on `pane` of tmux session `tmux_session`.
fn seed_session(db: &std::path::Path, tmux_session: &str, pane: &str) {
    let conn = rusqlite::Connection::open(db).unwrap();
    conn.execute(
        "INSERT INTO quest (id, slug, name_source, cwd, machine, state, created_at, updated_at)
         VALUES ('q-0001', 'alpha', 'manual', '/tmp', 'laptop', 'active', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, quest_id, role, label, tmux_session, tmux_pane, status,
                              started_at, updated_at)
         VALUES ('s-0001', 'q-0001', 'worker', 'w1', ?1, ?2, 'idle', 1, 1)",
        [tmux_session, pane],
    )
    .unwrap();
}

#[test]
fn doctor_json_reports_every_m0_check() {
    let mut cmd = doctor(&["claude"]);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);

    let names: Vec<&str> = parsed["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "config",
            "tmux",
            "claude",
            "db",
            "q on PATH",
            "orphan sessions"
        ]
    );
    assert_eq!(parsed["ok"], true);
    assert!(parsed["fixed"].as_array().unwrap().is_empty());
}

#[test]
fn doctor_passes_with_a_fixture_tmux_and_an_empty_database() {
    let mut cmd = doctor(&["claude"]);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);

    assert_eq!(check(&parsed, "tmux")["status"], "ok");
    assert_eq!(check(&parsed, "tmux")["detail"], "3.6 (fixture)");
    assert_eq!(check(&parsed, "claude")["status"], "ok");
    assert_eq!(check(&parsed, "config")["status"], "ok");
    assert_eq!(check(&parsed, "db")["status"], "ok");
    assert!(
        check(&parsed, "db")["detail"]
            .as_str()
            .unwrap()
            .starts_with("schema v"),
        "{parsed}"
    );
    assert_eq!(check(&parsed, "orphan sessions")["status"], "ok");
    // Nothing but the stub `claude` is on PATH, so `q` itself is not.
    assert_eq!(check(&parsed, "q on PATH")["status"], "warn");
}

#[test]
fn doctor_human_output_marks_each_check() {
    doctor(&["claude"])
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ tmux 3.6 (fixture)"))
        .stdout(predicate::str::contains("✓ orphan sessions none"));
}

#[test]
fn doctor_warns_about_an_orphan_session_and_fixes_it() {
    let mut cmd = doctor(&["claude"]);
    let db = db_path(&cmd);
    let fixture = fixture_path(&cmd);
    // A live pane that is *not* the seeded session's: the check must key on
    // the pair, not just on "some pane exists".
    std::fs::write(
        &fixture,
        r#"{"next_pane":9,"panes":[{"pane_id":"%9","session_name":"q-other","window_name":"w","window_index":0}]}"#,
    )
    .unwrap();

    // Creates and migrates the database, then seed a session behind its back.
    cmd.args(["doctor", "--json"]).assert().success();
    seed_session(&db, "q-alpha", "%1");

    let mut cmd = doctor(&["claude"]);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let orphans = check(&parsed, "orphan sessions");
    assert_eq!(orphans["status"], "warn");
    assert!(
        orphans["detail"].as_str().unwrap().contains("alpha/w1"),
        "{parsed}"
    );
    assert_eq!(orphans["fix_hint"], "q doctor --fix");
    // A warning is not a failure.
    assert_eq!(parsed["ok"], true);

    let mut cmd = doctor(&["claude"]);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--fix", "--json"]).assert().success();
    let parsed = json_of(&assert);
    assert_eq!(check(&parsed, "orphan sessions")["status"], "ok");
    assert_eq!(
        parsed["fixed"],
        serde_json::json!(["ended orphan session alpha/w1"])
    );

    // The fix stuck: a rerun has nothing left to report.
    let mut cmd = doctor(&["claude"]);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    assert_eq!(check(&parsed, "orphan sessions")["status"], "ok");
    assert_eq!(check(&parsed, "orphan sessions")["detail"], "none");
    assert!(parsed["fixed"].as_array().unwrap().is_empty());
}

#[test]
fn doctor_keeps_a_session_whose_pane_is_alive() {
    let mut cmd = doctor(&["claude"]);
    let db = db_path(&cmd);
    let fixture = fixture_path(&cmd);
    std::fs::write(
        &fixture,
        r#"{"next_pane":1,"panes":[{"pane_id":"%1","session_name":"q-alpha","window_name":"w1","window_index":0}]}"#,
    )
    .unwrap();
    cmd.args(["doctor", "--json"]).assert().success();
    seed_session(&db, "q-alpha", "%1");

    let mut cmd = doctor(&["claude"]);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    assert_eq!(check(&json_of(&assert), "orphan sessions")["status"], "ok");
}

#[test]
fn doctor_fails_on_an_invalid_config() {
    let mut cmd = doctor(&["claude"]);
    let path = config_path(&cmd);
    std::fs::write(&path, "[context]\nreset_strategy = \"nuke\"\n").unwrap();
    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    assert_eq!(parsed["ok"], false);
    let config = check(&parsed, "config");
    assert_eq!(config["status"], "fail");
    assert!(
        config["detail"]
            .as_str()
            .unwrap()
            .contains("reset_strategy"),
        "{parsed}"
    );
    // Every other check still ran.
    assert_eq!(parsed["checks"].as_array().unwrap().len(), 6);
    assert_eq!(check(&parsed, "tmux")["status"], "ok");
}

#[test]
fn doctor_fails_when_claude_is_not_on_path() {
    // An empty PATH; the fixture keeps tmux passing, so only claude fails.
    let assert = doctor(&[]).args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    assert_eq!(check(&parsed, "claude")["status"], "fail");
    assert!(check(&parsed, "claude")["fix_hint"].is_string());
    assert_eq!(check(&parsed, "tmux")["status"], "ok");
    assert_eq!(parsed["ok"], false);
}

#[test]
fn doctor_reports_a_database_from_a_newer_q() {
    let mut cmd = doctor(&["claude"]);
    let db = db_path(&cmd);
    cmd.args(["doctor", "--json"]).assert().success();
    rusqlite::Connection::open(&db)
        .unwrap()
        .pragma_update(None, "user_version", 99)
        .unwrap();

    let mut cmd = doctor(&["claude"]);
    let fixture = fixture_path(&cmd);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    assert_eq!(check(&parsed, "db")["status"], "fail");
    assert!(
        check(&parsed, "db")["detail"]
            .as_str()
            .unwrap()
            .contains("upgrade q"),
        "{parsed}"
    );
    // The orphan check depends on the database, so it stands down.
    let orphans = check(&parsed, "orphan sessions");
    assert_eq!(orphans["status"], "warn");
    assert!(
        orphans["detail"].as_str().unwrap().starts_with("skipped"),
        "{parsed}"
    );
}

#[test]
fn doctor_fixes_orphans_even_when_another_check_fails() {
    // An empty PATH: `claude` fails, so the run cannot end in success — the
    // repair must happen anyway.
    let mut cmd = doctor(&[]);
    let db = db_path(&cmd);
    let fixture = fixture_path(&cmd);
    std::fs::write(&fixture, r#"{"next_pane":0,"panes":[]}"#).unwrap();
    cmd.args(["doctor", "--json"]).assert().code(1);
    seed_session(&db, "q-alpha", "%1");

    let mut cmd = doctor(&[]);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--fix", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    assert_eq!(parsed["ok"], false);
    assert_eq!(check(&parsed, "claude")["status"], "fail");
    assert_eq!(check(&parsed, "orphan sessions")["status"], "ok");
    assert_eq!(
        parsed["fixed"],
        serde_json::json!(["ended orphan session alpha/w1"])
    );

    // Durable: the row itself is ended, not merely reported as such.
    let status: String = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row("SELECT status FROM session WHERE id = 's-0001'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(status, "ended");
}

#[test]
fn doctor_exits_non_zero_on_a_failure_in_human_mode() {
    doctor(&[])
        .arg("doctor")
        .assert()
        .code(1)
        .stdout(predicate::str::contains("✗ claude not found on PATH"));
}

#[test]
fn doctor_fails_on_a_tmux_older_than_the_minimum() {
    let mut cmd = doctor(&["claude"]);
    let fixture = fixture_path(&cmd);
    std::fs::write(&fixture, r#"{"version":"tmux 3.0","panes":[]}"#).unwrap();
    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    let tmux = check(&parsed, "tmux");
    assert_eq!(tmux["status"], "fail");
    assert!(tmux["detail"].as_str().unwrap().contains("3.2"), "{parsed}");
    assert!(
        tmux["fix_hint"].as_str().unwrap().contains("3.2"),
        "{parsed}"
    );
}

#[test]
fn doctor_warns_about_an_unparsable_tmux_version() {
    let mut cmd = doctor(&["claude"]);
    let fixture = fixture_path(&cmd);
    std::fs::write(&fixture, r#"{"version":"tmux master","panes":[]}"#).unwrap();
    // A warning is not a failure, so the run still succeeds.
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    assert_eq!(check(&json_of(&assert), "tmux")["status"], "warn");
}

#[test]
fn doctor_says_when_it_created_the_database() {
    let mut cmd = doctor(&["claude"]);
    let db = db_path(&cmd);
    let fixture = fixture_path(&cmd);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let detail = json_of(&assert);
    let detail = check(&detail, "db")["detail"].as_str().unwrap().to_string();
    assert!(detail.ends_with("(created)"), "{detail}");

    let mut cmd = doctor(&["claude"]);
    cmd.env("Q_DB", &db).env("Q_FIXTURE", &fixture);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    assert!(
        !check(&parsed, "db")["detail"]
            .as_str()
            .unwrap()
            .contains("created"),
        "{parsed}"
    );
}
// ------------------------------------------------------- lifecycle commands

impl Env {
    /// `q new -d --name <slug>` in a fresh work directory of the same name.
    fn new_quest(&self, slug: &str) -> serde_json::Value {
        let work = self.work(slug);
        let assert = self
            .cmd()
            .args(["new", "--name", slug, "--dir", work.to_str().unwrap()])
            .args(["-d", "--json"])
            .assert()
            .success();
        json_of(&assert)
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut cmd = self.cmd();
        let assert = cmd.args(args).arg("--json").assert().success();
        json_of(&assert)
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }
}

/// Rows of `event.kind` for a Quest, oldest first.
fn event_kinds(env: &Env, quest_id: &str) -> Vec<String> {
    env.conn()
        .prepare("SELECT kind FROM event WHERE quest_id = ?1 ORDER BY id")
        .unwrap()
        .query_map([quest_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn the_full_quest_lifecycle() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();

    // list: the fresh master counts as an active session.
    let list = env.json(&["list"]);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["slug"], "foo");
    assert_eq!(list[0]["display_state"], "active");
    assert_eq!(list[0]["needs_you"], false);
    assert_eq!(list[0]["live_sessions"], 1);

    // show
    let shown = env.json(&["show", "foo"]);
    assert_eq!(shown["id"], quest_id.as_str());
    assert_eq!(shown["slug"], "foo");
    assert_eq!(shown["live_sessions"], 1);
    assert_eq!(shown["display_state"], "active");
    assert_eq!(shown["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(shown["sessions"][0]["label"], "master");
    assert_eq!(shown["events"][0]["kind"], "quest.created");

    // enter
    let entered = env.json(&["enter", "foo"]);
    assert_eq!(entered["tmux_session"], "q-foo");
    assert_eq!(entered["window"], "master");
    assert_eq!(entered["attach"], "exec");
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo", "master"])
    );
    env.cmd()
        .args(["enter", "foo", "--session", "nope"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("session `nope`"))
        .stderr(predicate::str::contains("live: master"));

    // close
    let closed = env.json(&["close", "foo", "-f"]);
    assert_eq!(closed["quest"]["state"], "finished");
    assert_eq!(closed["sessions_ended"], 1);
    assert!(closed["quest"]["finished_at"].is_i64());
    assert!(
        env.fixture()["panes"].as_array().unwrap().is_empty(),
        "the tmux session outlived the close"
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE status = 'ended'"),
        1
    );
    assert!(event_kinds(&env, &quest_id).contains(&"quest.closed".to_string()));

    // a finished Quest is hidden unless asked for
    assert!(env.json(&["list"]).as_array().unwrap().is_empty());
    let all = env.json(&["list", "--all"]);
    assert_eq!(all.as_array().unwrap().len(), 1);
    assert_eq!(all[0]["display_state"], "finished");
    assert_eq!(all[0]["live_sessions"], 0);
    env.cmd()
        .args(["enter", "foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("q resume foo"));

    // resume
    let resumed = env.json(&["resume", "foo", "-d"]);
    assert_eq!(resumed["quest"]["state"], "active");
    assert_eq!(resumed["quest"]["finished_at"], serde_json::Value::Null);
    assert_eq!(resumed["attach"], "none");
    assert_ne!(resumed["session"]["id"], created["session"]["id"]);
    assert_eq!(env.count("SELECT count(*) FROM session"), 2);
    assert_eq!(pane_of(&env.fixture(), "q-foo")["window_name"], "master");
    assert_eq!(env.json(&["list"])[0]["display_state"], "active");
    assert!(event_kinds(&env, &quest_id).contains(&"quest.resumed".to_string()));

    // rename
    let renamed = env.json(&["rename", "foo", "bar"]);
    assert_eq!(renamed["quest"]["slug"], "bar");
    assert_eq!(renamed["quest"]["name_source"], "manual");
    assert_eq!(renamed["tmux_session"], "q-bar");
    assert_eq!(pane_of(&env.fixture(), "q-bar")["window_name"], "master");
    assert_eq!(env.json(&["list"])[0]["slug"], "bar");
    // Only the live session follows the rename; the closed one is history.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-bar'"),
        1
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-foo' AND status = 'ended'"),
        1
    );
    assert!(event_kinds(&env, &quest_id).contains(&"name.changed".to_string()));

    // set
    env.json(&["set", "bar", "goal", "x"]);
    assert_eq!(env.json(&["show", "bar"])["goal"], "x");
    assert!(event_kinds(&env, &quest_id).contains(&"quest.updated".to_string()));

    // rm
    env.cmd()
        .args(["rm", "bar", "-f"])
        .assert()
        .success()
        .stdout(predicate::str::contains("removed q-"))
        .stdout(predicate::str::contains("(bar)"));
    assert!(env.json(&["list", "--all"]).as_array().unwrap().is_empty());
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
    assert_eq!(env.count("SELECT count(*) FROM session"), 0);
    assert_eq!(env.count("SELECT count(*) FROM event"), 0);
}

#[test]
fn list_is_a_table_and_says_when_it_is_empty() {
    let env = Env::new();
    env.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("no quests"));
    env.new_quest("foo");
    env.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("SLUG"))
        .stdout(predicate::str::contains("foo"))
        .stdout(predicate::str::contains("active"));
}

#[test]
fn list_filters_by_derived_state() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["close", "foo", "-f"]);
    env.new_quest("bar");
    assert_eq!(
        env.json(&["list", "--state", "active"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(env.json(&["list", "--state", "active"])[0]["slug"], "bar");
    assert!(
        env.json(&["list", "--state", "idle"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    // `--state finished` implies `--all`.
    let finished = env.json(&["list", "--state", "finished"]);
    assert_eq!(finished.as_array().unwrap().len(), 1);
    assert_eq!(finished[0]["slug"], "foo");
}

#[test]
fn list_filters_by_machine_only_when_asked() {
    let env = Env::new();
    env.new_quest("foo");
    assert_eq!(env.json(&["list"]).as_array().unwrap().len(), 1);
    assert!(
        env.json(&["--machine", "elsewhere", "list"])
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn close_without_force_and_without_a_tty_aborts() {
    let env = Env::new();
    env.new_quest("foo");
    env.cmd()
        .args(["close", "foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("aborted (use -f)"));
    assert_eq!(env.json(&["list"])[0]["display_state"], "active");
    assert_eq!(
        env.count("SELECT count(*) FROM quest WHERE state = 'active'"),
        1
    );
}

#[test]
fn closing_a_finished_quest_is_a_no_op() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["close", "foo", "-f"]);
    // No `-f` needed the second time: there is nothing left to confirm.
    let again = env.json(&["close", "foo"]);
    assert_eq!(again["already_finished"], true);
    assert_eq!(again["sessions_ended"], 0);
    assert_eq!(
        event_kinds(&env, again["quest"]["id"].as_str().unwrap())
            .iter()
            .filter(|k| *k == "quest.closed")
            .count(),
        1
    );
}

#[test]
fn rm_refuses_a_running_quest_without_force() {
    let env = Env::new();
    env.new_quest("foo");
    env.cmd()
        .args(["rm", "foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("still runs in tmux session q-foo"));
    assert_eq!(env.count("SELECT count(*) FROM quest"), 1);
}

#[test]
fn resume_refuses_a_quest_that_is_still_running() {
    let env = Env::new();
    env.new_quest("foo");
    env.cmd()
        .args(["resume", "foo", "-d"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("q enter foo"));
}

#[test]
fn rename_refuses_a_slug_that_is_taken() {
    let env = Env::new();
    env.new_quest("foo");
    env.new_quest("bar");
    env.cmd()
        .args(["rename", "foo", "bar"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already taken by quest"));
    env.cmd()
        .args(["rename", "foo", "Not A Slug"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("invalid slug"));
}

#[test]
fn set_validates_its_values() {
    let env = Env::new();
    env.new_quest("foo");
    let work = env.work("elsewhere");
    let out = env.json(&["set", "foo", "cwd", work.to_str().unwrap()]);
    assert_eq!(out["key"], "cwd");
    assert_eq!(out["quest"]["cwd"], work.to_str().unwrap());

    env.cmd()
        .args(["set", "foo", "cwd", "/nope/nope"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no such directory"));

    assert_eq!(
        env.json(&["set", "foo", "ctx_reset_pct", "42"])["quest"]["ctx_reset_pct"],
        42
    );
    assert_eq!(
        env.json(&["set", "foo", "ctx_reset_pct", "default"])["quest"]["ctx_reset_pct"],
        serde_json::Value::Null
    );
    env.cmd()
        .args(["set", "foo", "ctx_reset_pct", "0"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("expected 1-100"));
    env.cmd()
        .args(["set", "foo", "auto_reset", "on"])
        .assert()
        .code(2);
}

#[test]
fn an_ambiguous_target_lists_the_candidates() {
    let env = Env::new();
    let one = env.new_quest("alpha-one");
    let two = env.new_quest("alpha-two");
    let assert = env.cmd().args(["show", "alpha", "--json"]).assert().code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "ambiguous");
    let message = parsed["error"].as_str().unwrap();
    for id in [
        one["quest"]["id"].as_str().unwrap(),
        two["quest"]["id"].as_str().unwrap(),
    ] {
        assert!(message.contains(id), "{message}");
    }
    // A prefix that only one Quest answers to still resolves.
    assert_eq!(env.json(&["show", "alpha-o"])["slug"], "alpha-one");
}

#[test]
fn an_unknown_target_is_not_found() {
    let env = Env::new();
    let assert = env.cmd().args(["show", "nope", "--json"]).assert().code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
}

#[test]
fn a_listing_sweeps_a_pane_that_is_gone() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    // The pane died without a hook ever reporting it.
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));

    let list = env.json(&["list"]);
    assert_eq!(list[0]["display_state"], "idle");
    assert_eq!(list[0]["live_sessions"], 0);

    let shown = env.json(&["show", "foo"]);
    assert_eq!(shown["display_state"], "idle");
    assert_eq!(shown["sessions"][0]["status"], "ended");
    assert!(shown["sessions"][0]["ended_at"].is_i64());
    assert!(event_kinds(&env, &quest_id).contains(&"session.end".to_string()));
    let reason: String = env
        .conn()
        .query_row(
            "SELECT json_extract(payload, '$.reason') FROM event WHERE kind = 'session.end'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "pane_gone");
}

#[test]
fn enter_refuses_a_quest_whose_master_window_is_gone() {
    let env = Env::new();
    env.new_quest("foo");
    // The master window died; the tmux session lives on with another window.
    env.write_fixture(serde_json::json!({
        "next_pane": 2,
        "panes": [{
            "pane_id": "%2",
            "session_name": "q-foo",
            "window_name": "w1",
            "window_index": 1,
        }],
    }));

    let assert = env
        .cmd()
        .args(["enter", "foo", "--json"])
        .assert()
        .code(1)
        .stdout("");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    let message = parsed["error"].as_str().unwrap();
    assert!(message.contains("master session of foo ended"), "{message}");
    assert!(message.contains("q resume foo"), "{message}");
    // Nothing was attached to.
    assert_eq!(env.fixture()["attached"], serde_json::Value::Null);
}

#[test]
fn resume_revives_an_active_quest_that_lost_its_sessions() {
    let env = Env::new();
    let created = env.new_quest("foo");
    // Both the pane and the whole tmux session are gone, but the Quest was
    // never closed.
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));

    let resumed = env.json(&["resume", "foo", "-d"]);
    assert_eq!(resumed["quest"]["state"], "active");
    assert_eq!(resumed["attach"], "none");
    assert_ne!(resumed["session"]["id"], created["session"]["id"]);
    assert_eq!(pane_of(&env.fixture(), "q-foo")["window_name"], "master");
    assert_eq!(env.json(&["list"])[0]["live_sessions"], 1);
}

#[test]
fn resume_of_a_finished_quest_points_at_the_stray_tmux_session() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["close", "foo", "-f"]);
    // Something recreated `q-foo` after the close.
    env.write_fixture(serde_json::json!({
        "next_pane": 1,
        "panes": [{ "pane_id": "%1", "session_name": "q-foo", "window_name": "shell" }],
    }));

    env.cmd()
        .args(["resume", "foo", "-d"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("still exists; kill it first"))
        .stderr(predicate::str::contains("tmux kill-session -t =q-foo"))
        .stderr(predicate::str::contains("q rm -f"));
}

#[test]
fn rename_works_without_a_tmux_session() {
    let env = Env::new();
    env.new_quest("foo");
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));

    let renamed = env.json(&["rename", "foo", "bar"]);
    assert_eq!(renamed["quest"]["slug"], "bar");
    assert_eq!(renamed["tmux_session"], "q-bar");
    assert_eq!(renamed["changed"], true);
    assert_eq!(env.json(&["list"])[0]["slug"], "bar");
    // The swept session ended under the old name and keeps it.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-foo'"),
        1
    );
}

#[test]
fn renaming_to_the_same_slug_is_a_no_op_of_the_same_shape() {
    let env = Env::new();
    env.new_quest("foo");
    let out = env.json(&["rename", "foo", "foo"]);
    assert_eq!(out["quest"]["slug"], "foo");
    assert_eq!(out["from"], "foo");
    assert_eq!(out["to"], "foo");
    assert_eq!(out["tmux_session"], "q-foo");
    assert_eq!(out["changed"], false);
    assert!(
        !event_kinds(&env, out["quest"]["id"].as_str().unwrap())
            .contains(&"name.changed".to_string())
    );
}

#[test]
fn rm_without_force_and_without_a_tty_aborts() {
    let env = Env::new();
    env.new_quest("foo");
    // No tmux session left, so `rm` reaches the confirmation instead of the
    // "still runs in tmux" refusal.
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));

    env.cmd()
        .args(["rm", "foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("aborted (use -f)"));
    assert_eq!(env.count("SELECT count(*) FROM quest"), 1);
}

#[test]
fn a_json_caller_is_never_prompted() {
    let env = Env::new();
    env.new_quest("foo");
    let assert = env
        .cmd()
        .args(["close", "foo", "--json"])
        .assert()
        .code(1)
        .stdout("");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert!(
        parsed["error"].as_str().unwrap().contains("-f"),
        "unexpected payload: {parsed}"
    );
    assert_eq!(
        env.count("SELECT count(*) FROM quest WHERE state = 'active'"),
        1
    );
}

#[test]
fn an_agent_inside_a_quest_pane_is_never_prompted() {
    let env = Env::new();
    env.new_quest("foo");
    env.cmd()
        .env("Q_QUEST", "q-7f3a")
        .args(["close", "foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("aborted (use -f)"));
    assert_eq!(
        env.count("SELECT count(*) FROM quest WHERE state = 'active'"),
        1
    );
}

#[test]
fn set_clears_goal_and_workflow_with_an_empty_value() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["set", "foo", "goal", "ship it"]);
    env.json(&["set", "foo", "workflow", "tdd"]);

    assert_eq!(
        env.json(&["set", "foo", "goal", ""])["quest"]["goal"],
        serde_json::Value::Null
    );
    assert_eq!(
        env.json(&["set", "foo", "workflow", "  "])["quest"]["workflow"],
        serde_json::Value::Null
    );
    let shown = env.json(&["show", "foo"]);
    assert_eq!(shown["goal"], serde_json::Value::Null);
    assert_eq!(shown["workflow"], serde_json::Value::Null);
}

#[test]
fn new_sweeps_before_it_creates() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));

    // Creating an unrelated Quest still notices that `foo`'s pane is gone.
    env.new_quest("bar");
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE status = 'ended'"),
        1
    );
    assert!(event_kinds(&env, &quest_id).contains(&"session.end".to_string()));
}

// ----------------------------------------------------------------- q brief

#[test]
fn brief_renders_markdown_and_json() {
    let env = Env::new();
    let quest = env.new_quest("briefed");
    let id = quest["quest"]["id"].as_str().unwrap();

    let assert = env.cmd().args(["brief", "briefed"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.starts_with(&format!("# Quest {id} `briefed`")), "{out}");
    for header in [
        "## 1. Quest",
        "## 5. Sessions",
        "## 10. Open questions / blockers",
    ] {
        assert!(out.contains(header), "{header} missing:\n{out}");
    }
    assert!(out.contains("| master | master |"), "{out}");
    assert!(out.contains("You are the **master**"));

    let json = env.json(&["brief", "brief", "--for", "worker"]);
    assert_eq!(json["quest_id"], id);
    assert_eq!(json["for"], "worker");
    assert!(
        json["markdown"]
            .as_str()
            .unwrap()
            .contains("You are a **worker**")
    );
}

#[test]
fn brief_resolves_quest_and_role_from_env() {
    let env = Env::new();
    let quest = env.new_quest("from-env");
    let id = quest["quest"]["id"].as_str().unwrap();

    env.cmd()
        .arg("brief")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Q_QUEST"));

    let mut cmd = env.cmd();
    let assert = cmd
        .env("Q_QUEST", id)
        .env("Q_ROLE", "worker")
        .env("Q_SESSION", "master")
        .args(["brief", "--json"])
        .assert()
        .success();
    let json = json_of(&assert);
    assert_eq!(json["for"], "worker");
    let md = json["markdown"].as_str().unwrap();
    assert!(md.contains("You are session `master`"), "{md}");
}

#[test]
fn brief_reads_bd_and_brain_from_fixtures() {
    let env = Env::new();
    let quest = env.new_quest("fixtured");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    env.conn()
        .execute(
            "UPDATE quest SET beads_epic = 'bd-9', brain_session = 'fixtured' WHERE id = ?1",
            [&id],
        )
        .unwrap();
    let bd = env.dir.path().join("bd.json");
    std::fs::write(&bd, r#"[{"id":"bd-9.1","title":"first","status":"open"}]"#).unwrap();
    let brain = env.dir.path().join("brain.md");
    std::fs::write(&brain, "quest: q\n\nnotes from the brain").unwrap();

    let mut cmd = env.cmd();
    let assert = cmd
        .env("Q_FIXTURE_BD", &bd)
        .env("Q_FIXTURE_BRAIN", &brain)
        .args(["brief", "fixtured"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("[open] `bd-9.1` first"), "{out}");
    assert!(out.contains("notes from the brain"), "{out}");

    // Without the fixture files the tools count as unavailable.
    let assert = env.cmd().args(["brief", "fixtured"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("unavailable (bd missing or failed)"), "{out}");
    assert!(out.contains("_(brain note unavailable)_"), "{out}");
}
