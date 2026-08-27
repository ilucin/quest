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
    sandbox(&mut cmd, dir.path());
    TestCmd { dir, cmd }
}

/// Every override that keeps a `q` run inside `dir`. One place, so a second
/// command against the same directory (`install_hooks`) cannot drift from
/// what `q()` sets.
fn sandbox(cmd: &mut Command, dir: &std::path::Path) {
    // `Q_FIXTURE` keeps every tmux call in the fake backend; no test may reach
    // a real tmux server.
    cmd.env("Q_DB", dir.join("q.db"))
        .env("Q_CONFIG", dir.join("config.toml"))
        .env("Q_FIXTURE", dir.join("tmux.json"))
        // Never read the real `~/.claude/sessions`.
        .env("Q_CLAUDE_SESSIONS_DIR", dir.join("registry"))
        // `q doctor` reads Claude's settings.json and looks for credentials
        // under `$HOME`; no test may see the real ones. `HOME` is a
        // subdirectory so paths elsewhere in `dir` are not home-relative.
        .env("HOME", dir.join("home"))
        .env("Q_CLAUDE_SETTINGS", claude_dir(dir).join("settings.json"));
}

/// Claude Code's user directory inside a sandboxed `HOME`.
fn claude_dir(dir: &std::path::Path) -> std::path::PathBuf {
    dir.join("home").join(".claude")
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
        "new", "list", "show", "enter", "close", "resume", "rename", "name", "set", "rm", "brief",
        "events", "doctor", "config", "phase", "note", "link", "links", "artifact", "spawn",
        "sessions", "peek", "send", "reset", "kill",
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

/// Bare `q` is the TUI (SPEC §16) — but only on a terminal. Under a pipe (and
/// so under every test and every script) it must fall back to the banner
/// rather than reaching for the alternate screen.
#[test]
fn bare_invocation_off_a_terminal_prints_the_banner() {
    q().assert()
        .success()
        .stdout(predicate::str::contains("q --help"));
}

/// `--json` short-circuits before the `is_terminal` check, so this pins the
/// reported flag rather than the fallback above.
#[test]
fn bare_json_reports_that_the_tui_was_not_launched() {
    let assert = q().arg("--json").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["tui"], serde_json::Value::Bool(false), "{parsed}");
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

/// The `quests` array of a `q list --json` envelope. The listing is the one
/// command whose `--json` is an object rather than an array: it also has to
/// report the machines it asked (see `commands::list` for the contract).
fn quests_of(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
    json_of(assert)["quests"].clone()
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
    let mut cmd = q();
    std::fs::write(
        config_path(&cmd),
        "[machine]\nname = \"laptop\"\n\n[[remotes]]\nname = \"ws\"\nssh = \"ws-host\"\n",
    )
    .unwrap();
    let assert = cmd
        .args(["--machine", "ws", "config", "get", "machine.name", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["value"], "laptop");
}

#[test]
fn machine_flag_is_validated() {
    q().args(["--machine", "Not Valid", "config", "get"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("machine.name"));
}

/// A `--machine` that names neither this machine nor a configured remote is a
/// typo, and answering it with an empty listing would read as a fact about that
/// machine rather than as the mistake it is.
#[test]
fn machine_flag_refuses_a_machine_nobody_has_heard_of() {
    let mut cmd = q();
    std::fs::write(
        config_path(&cmd),
        "[machine]\nname = \"laptop\"\n\n[[remotes]]\nname = \"ws\"\nssh = \"ws-host\"\n",
    )
    .unwrap();
    let assert = cmd
        .args(["--machine", "bogus", "list", "--json"])
        .assert()
        .code(1);
    let err = error_json(&assert);
    assert_eq!(err["code"], "not_found");
    let said = err["error"].as_str().unwrap();
    assert!(said.contains("bogus"), "{said}");
    // It names the ones that would have worked.
    assert!(said.contains("laptop") && said.contains("ws"), "{said}");
}

/// The local machine is a valid `--machine`: it filters the listing, it is not
/// a remote to dial.
#[test]
fn machine_flag_accepts_this_machines_own_name() {
    let mut cmd = q();
    std::fs::write(config_path(&cmd), "[machine]\nname = \"laptop\"\n").unwrap();
    let assert = cmd
        .args(["--machine", "laptop", "list", "--json"])
        .assert()
        .success();
    assert!(json_of(&assert)["quests"].as_array().unwrap().is_empty());
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
            // Never read the real `~/.claude/sessions`; a test that wants a
            // registry entry writes one with `Env::registry`.
            .env("Q_CLAUDE_SESSIONS_DIR", self.dir.path().join("registry"))
            // The attach mode and the confirm refusal depend on them, so
            // neither leaks in from the terminal `cargo test` runs in.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
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
    assert_eq!(pane["cwd"], work.to_str().unwrap());
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
    let out = json_of(&assert);
    assert_eq!(out["attach"], "exec");
    // Attach targets are pane ids: the window name is not an address (SPEC §6).
    let master_pane = out["session"]["tmux_pane"].as_str().unwrap();
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo", master_pane])
    );
    assert_eq!(env.fixture()["selected"], master_pane);
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
        "--repo",
        "--no-beads",
        "--prompt",
        "--prompt-file",
        "--detach",
    ] {
        assert!(
            out.contains(flag),
            "`{flag}` missing from `q new --help`:\n{out}"
        );
    }
    for later in ["--brain", "--template", "--from-brief"] {
        assert!(
            !out.contains(later),
            "`{later}` is not implemented yet:\n{out}"
        );
    }
}

// ------------------------------------------------------------------- q doctor

/// An executable named `name` that does nothing but answer `--version`, so a
/// check that looks for a binary on `PATH` has a deterministic answer.
fn stub_exe(dir: &std::path::Path, name: &str) {
    write_exe(
        &dir.join(name),
        "#!/bin/sh\ncase \"$1\" in --version) echo '0.0.0 (stub)' ;; esac\nexit 0\n",
    );
}

fn write_exe(path: &std::path::Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A `claude` stub answering the two calls `q doctor` makes: `--version` and
/// `auth status --json`. An empty `auth` prints nothing and exits 1, the way
/// an older Claude Code without an `auth` subcommand would.
///
/// The real 2.1.246 exits **1** whenever it prints `"loggedIn": false` (and 0
/// when logged in), so the stub does the same: the check must read the JSON,
/// not the exit status.
fn stub_claude(dir: &std::path::Path, version: &str, auth: &str) {
    let answer = if auth.is_empty() {
        "exit 1".to_string()
    } else {
        // Single-quoted: the payload is JSON, so it holds no single quotes.
        let code = i32::from(auth.contains("\"loggedIn\":false"));
        format!("echo '{auth}'; exit {code}")
    };
    write_exe(
        &dir.join("claude"),
        &format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo '{version}' ;;\nauth) {answer} ;;\n*) exit 0 ;;\nesac\n"
        ),
    );
}

/// `sh` inside the sandboxed `PATH`, for the checks that run a shell.
fn link_sh(bin: &std::path::Path) {
    std::os::unix::fs::symlink("/bin/sh", bin.join("sh")).unwrap();
}

fn bin_dir(cmd: &TestCmd) -> std::path::PathBuf {
    cmd.dir.path().join("bin")
}

fn settings_path(cmd: &TestCmd) -> std::path::PathBuf {
    claude_dir(cmd.dir.path()).join("settings.json")
}

/// Where the login check falls back to when `claude auth status` is no help.
fn credentials_path(cmd: &TestCmd) -> std::path::PathBuf {
    claude_dir(cmd.dir.path()).join(".credentials.json")
}

/// `q` with a `PATH` holding exactly `names` — `PATH`-dependent checks then
/// report the same thing on every machine. The hook checks are left failing:
/// `doctor` installs them, this does not.
fn doctor_bare(names: &[&str]) -> TestCmd {
    let mut cmd = q();
    let bin = cmd.dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for name in names {
        stub_exe(&bin, name);
    }
    cmd.env("PATH", &bin);
    cmd
}

/// `doctor_bare` plus q's hooks installed into the temp settings.json, so a
/// test about some other check is not drowned in hook failures.
fn doctor(names: &[&str]) -> TestCmd {
    let cmd = doctor_bare(names);
    install_hooks(&cmd);
    cmd
}

/// `q hook install` against this command's temp settings.json.
fn install_hooks(cmd: &TestCmd) {
    install_hooks_at(cmd, &settings_path(cmd));
}

/// `q hook install` into `settings`, for a test that moved the file.
fn install_hooks_at(cmd: &TestCmd, settings: &std::path::Path) {
    let mut installer = Command::cargo_bin("q").unwrap();
    sandbox(&mut installer, cmd.dir.path());
    installer
        .env("Q_CLAUDE_SETTINGS", settings)
        .args(["hook", "install"])
        .assert()
        .success();
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
fn doctor_json_reports_every_check() {
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
            "claude login",
            "db",
            "q on PATH",
            "bd",
            "hook SessionStart",
            "hook UserPromptSubmit",
            "hook Stop",
            "hook Notification",
            "hook PreCompact",
            "hook SessionEnd",
            "hook PostToolUse",
            "hook statusLine",
            "statusline chain",
            "orphan sessions",
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
    assert_eq!(check(&parsed, "tmux")["status"], "ok");
    assert_eq!(check(&parsed, "claude")["status"], "ok");
    assert_eq!(check(&parsed, "statusline chain")["status"], "ok");
    assert_eq!(check(&parsed, "hook Stop")["status"], "ok");
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
// ------------------------------------------- doctor: hooks, statusline, login

/// Every hook check, in report order.
fn hook_checks(report: &serde_json::Value) -> Vec<(&str, &str)> {
    report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| {
            let name = c["name"].as_str().unwrap();
            name.strip_prefix("hook ")
                .map(|n| (n, c["status"].as_str().unwrap()))
        })
        .collect()
}

#[test]
fn doctor_fails_when_the_hooks_are_not_installed() {
    let mut cmd = doctor_bare(&["claude"]);
    let settings = settings_path(&cmd);
    assert!(!settings.exists());

    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    assert_eq!(parsed["ok"], false);

    let hooks = hook_checks(&parsed);
    assert_eq!(hooks.len(), 8, "{parsed}");
    for (name, status) in &hooks {
        assert_eq!(*status, "fail", "hook {name} in {parsed}");
    }
    let stop = check(&parsed, "hook Stop");
    assert_eq!(stop["detail"], "missing");
    assert_eq!(stop["fix_hint"], "q hook install");
    // The probe still runs, and the handler itself is fine.
    assert_eq!(check(&parsed, "statusline chain")["status"], "ok");
    // Nothing wrote the settings file just to inspect it.
    assert!(!settings.exists());
}

#[test]
fn doctor_passes_once_the_hooks_are_installed() {
    let mut cmd = doctor(&["claude"]);
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);

    for (name, status) in hook_checks(&parsed) {
        assert_eq!(status, "ok", "hook {name} in {parsed}");
    }
    let statusline = check(&parsed, "hook statusLine");
    assert!(
        statusline["detail"]
            .as_str()
            .unwrap()
            .contains("hook statusline"),
        "{parsed}"
    );
    assert!(statusline["fix_hint"].is_null());
}

#[test]
fn doctor_fails_a_hook_that_drifted() {
    let mut cmd = doctor(&["claude"]);
    let settings = settings_path(&cmd);
    let mut json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
    // A stale timeout: installed, but not what this q would write.
    json["hooks"]["Stop"][0]["hooks"][0]["timeout"] = serde_json::json!(99);
    json["statusLine"]["command"] = serde_json::json!("/old/q hook statusline");
    std::fs::write(&settings, json.to_string()).unwrap();

    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    assert_eq!(check(&parsed, "hook Stop")["status"], "fail");
    assert_eq!(check(&parsed, "hook Stop")["detail"], "drifted");
    assert_eq!(check(&parsed, "hook Stop")["fix_hint"], "q hook install");
    assert_eq!(check(&parsed, "hook statusLine")["status"], "fail");
    // The untouched entries still pass.
    assert_eq!(check(&parsed, "hook SessionStart")["status"], "ok");
}

#[test]
fn doctor_reports_unreadable_claude_settings_as_one_hook_failure() {
    let mut cmd = doctor_bare(&["claude"]);
    let settings = settings_path(&cmd);
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    std::fs::write(&settings, "{ not json").unwrap();

    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    let hooks = check(&parsed, "hooks");
    assert_eq!(hooks["status"], "fail");
    // `q hook install` reads the same file and fails the same way, so the hint
    // has to name the repair the user can actually make.
    // `settings_path` canonicalizes, so compare against the resolved path.
    let resolved = std::fs::canonicalize(&settings).unwrap();
    assert_eq!(
        hooks["fix_hint"],
        serde_json::json!(format!("fix the JSON at {}", resolved.display()))
    );
    let detail = hooks["detail"].as_str().unwrap();
    assert!(detail.contains("settings.json"), "{parsed}");
    // Not `config: <path>`: that prefix belongs to q's own config.toml, and
    // this line is already named `hooks`.
    assert!(!detail.starts_with("config:"), "{detail}");
    assert_eq!(hook_checks(&parsed).len(), 0);
}

#[test]
fn doctor_probes_a_configured_statusline_chain() {
    let mut cmd = doctor(&["claude"]);
    link_sh(&bin_dir(&cmd));
    std::fs::write(config_path(&cmd), "[statusline]\nchain = \"/bin/cat\"\n").unwrap();

    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let probe = check(&parsed, "statusline chain");
    assert_eq!(probe["status"], "ok");
    let detail = probe["detail"].as_str().unwrap();
    // The chain echoed the sample payload back, so the whole path works.
    assert!(detail.starts_with("`/bin/cat` → {"), "{detail}");
    assert!(detail.contains("hook_event_name"), "{detail}");
}

#[test]
fn doctor_warns_when_the_statusline_chain_says_nothing() {
    let mut cmd = doctor(&["claude"]);
    link_sh(&bin_dir(&cmd));
    std::fs::write(
        config_path(&cmd),
        "[statusline]\nchain = \"/bin/cat > /dev/null\"\n",
    )
    .unwrap();

    // A warning, not a failure: the report still exits 0.
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let probe = check(&parsed, "statusline chain");
    assert_eq!(probe["status"], "warn");
    assert!(
        probe["detail"]
            .as_str()
            .unwrap()
            .contains("printed nothing"),
        "{parsed}"
    );
    assert!(probe["fix_hint"].is_string());
}

#[test]
fn doctor_reports_a_chain_that_exits_non_zero() {
    let mut cmd = doctor(&["claude"]);
    link_sh(&bin_dir(&cmd));
    std::fs::write(
        config_path(&cmd),
        "[statusline]\nchain = \"echo boom >&2; exit 3\"\n",
    )
    .unwrap();

    // The handler still exits 0 — a status bar may not break Claude — so the
    // chain's own failure has to be surfaced by the probe.
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let probe = check(&parsed, "statusline chain");
    assert_eq!(probe["status"], "warn");
    let detail = probe["detail"].as_str().unwrap();
    assert!(detail.contains("exited 3"), "{detail}");
    assert!(detail.contains("boom"), "{detail}");
    assert!(probe["fix_hint"].is_string(), "{parsed}");
}

#[test]
fn doctor_strips_colour_from_what_the_chain_printed() {
    let mut cmd = doctor(&["claude"]);
    link_sh(&bin_dir(&cmd));
    // A TOML literal string, so the escapes reach `sh` untouched.
    std::fs::write(
        config_path(&cmd),
        "[statusline]\nchain = 'printf \"\\033[1;32mctx 42%%\\033[0m\\n\"'\n",
    )
    .unwrap();

    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let probe = check(&parsed, "statusline chain");
    assert_eq!(probe["status"], "ok", "{parsed}");
    let detail = probe["detail"].as_str().unwrap();
    assert!(detail.ends_with("→ ctx 42%"), "{detail}");
    assert!(!detail.contains('\u{1b}'), "escapes reached the report");
}

#[test]
fn doctor_reports_the_claude_version_and_who_is_logged_in() {
    let mut cmd = doctor_bare(&[]);
    stub_claude(
        &bin_dir(&cmd),
        "2.1.246 (Claude Code)",
        r#"{"loggedIn":true,"authMethod":"claude.ai","email":"a@b.c","subscriptionType":"team"}"#,
    );
    install_hooks(&cmd);

    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let claude = check(&parsed, "claude");
    assert_eq!(claude["status"], "ok");
    assert!(
        claude["detail"].as_str().unwrap().starts_with("2.1.246 · "),
        "{parsed}"
    );
    let login = check(&parsed, "claude login");
    assert_eq!(login["status"], "ok");
    // The email is deliberately absent: doctor output gets pasted around.
    assert_eq!(login["detail"], "logged in · claude.ai · team");
    assert!(
        !String::from_utf8_lossy(&assert.get_output().stdout).contains("a@b.c"),
        "the email must not appear anywhere in the report"
    );
}

#[test]
fn doctor_fails_when_claude_is_logged_out() {
    // The real `claude auth status --json` exits 1 while printing this, so the
    // stub does too (see `stub_claude`): reading the exit status instead of the
    // payload would report "unknown" for every logged-out user.
    let mut cmd = doctor_bare(&[]);
    stub_claude(
        &bin_dir(&cmd),
        "2.1.246 (Claude Code)",
        r#"{"loggedIn":false,"authMethod":"none"}"#,
    );
    install_hooks(&cmd);
    // A credentials file must not talk the check out of a `loggedIn: false`.
    std::fs::create_dir_all(claude_dir(cmd.dir.path())).unwrap();
    std::fs::write(credentials_path(&cmd), "{}").unwrap();

    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    let login = check(&parsed, "claude login");
    assert_eq!(login["status"], "fail");
    assert_eq!(login["detail"], "not logged in");
    assert_eq!(login["fix_hint"], "claude auth login");
}

#[test]
fn doctor_falls_back_to_the_credentials_file_then_to_a_warning() {
    // No `claude auth status` (an older Claude Code) and no credentials file:
    // the login state is unknown, which warns rather than fails.
    let mut cmd = doctor_bare(&[]);
    stub_claude(&bin_dir(&cmd), "1.0.0 (Claude Code)", "");
    install_hooks(&cmd);
    let creds = credentials_path(&cmd);

    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let login = check(&parsed, "claude login");
    assert_eq!(login["status"], "warn");
    assert!(
        login["detail"].as_str().unwrap().starts_with("unknown: "),
        "{parsed}"
    );

    // The same run with `~/.claude/.credentials.json` in place passes — and
    // that path comes from `$HOME`, not from `$Q_CLAUDE_SETTINGS`, which this
    // run points somewhere else entirely.
    let mut cmd = doctor_bare(&[]);
    stub_claude(&bin_dir(&cmd), "1.0.0 (Claude Code)", "");
    let elsewhere = cmd.dir.path().join("elsewhere").join("settings.json");
    std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
    install_hooks_at(&cmd, &elsewhere);
    cmd.env("Q_CLAUDE_SETTINGS", &elsewhere);
    std::fs::create_dir_all(claude_dir(cmd.dir.path())).unwrap();
    std::fs::write(credentials_path(&cmd), "{}").unwrap();
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let login = check(&parsed, "claude login");
    assert_eq!(login["status"], "ok");
    assert!(
        login["detail"]
            .as_str()
            .unwrap()
            .starts_with("credentials at "),
        "{parsed}"
    );
    // The first run really had none.
    assert!(!creds.exists());
}

#[test]
fn doctor_human_output_lists_the_hooks_and_the_probe() {
    doctor(&["claude"])
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains("✓ hook SessionStart installed"))
        .stdout(predicate::str::contains(
            "✓ statusline chain no chain configured",
        ));
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

/// The pane ids of every session row, keyed by quest slug.
fn live_panes_by_slug(env: &Env) -> Vec<(String, String, String)> {
    env.conn()
        .prepare(
            "SELECT quest.slug, session.tmux_session, session.tmux_pane
             FROM session JOIN quest ON quest.id = session.quest_id
             WHERE session.ended_at IS NULL ORDER BY quest.slug",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

#[test]
fn two_quests_get_distinct_panes_and_both_stay_alive() {
    let env = Env::new();
    let alpha = env.new_quest("alpha");
    let beta = env.new_quest("beta");

    assert_ne!(alpha["session"]["tmux_pane"], beta["session"]["tmux_pane"]);
    assert_eq!(
        live_panes_by_slug(&env),
        vec![
            (
                "alpha".to_string(),
                "q-alpha".to_string(),
                alpha["session"]["tmux_pane"].as_str().unwrap().to_string()
            ),
            (
                "beta".to_string(),
                "q-beta".to_string(),
                beta["session"]["tmux_pane"].as_str().unwrap().to_string()
            ),
        ]
    );

    // The second `q new` must not have swept the first quest's session.
    let list = env.quests(&["list"]);
    for row in list.as_array().unwrap() {
        assert_eq!(row["display_state"], "active", "{row}");
        assert_eq!(row["live_sessions"], 1, "{row}");
    }
    for slug in ["alpha", "beta"] {
        let shown = env.json(&["show", slug]);
        assert_eq!(shown["live_sessions"], 1, "{shown}");
        assert_eq!(shown["sessions"][0]["status"], "starting", "{shown}");
        assert!(shown["sessions"][0]["ended_at"].is_null(), "{shown}");
    }
    assert!(
        !event_kinds(&env, alpha["quest"]["id"].as_str().unwrap())
            .contains(&"session.end".to_string())
    );
}

/// The fixture's `next_pane` defaults, so a hand-seeded file can omit it. That
/// must not hand the next session a pane id another session already holds.
#[test]
fn a_fixture_seeded_without_a_pane_counter_keeps_pane_ids_unique() {
    let env = Env::new();
    let alpha = env.new_quest("alpha");
    let mut fixture = env.fixture();
    fixture.as_object_mut().unwrap().remove("next_pane");
    env.write_fixture(fixture);

    let beta = env.new_quest("beta");
    assert_ne!(alpha["session"]["tmux_pane"], beta["session"]["tmux_pane"]);

    let panes = env.fixture();
    let ids: Vec<&str> = panes["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["pane_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["%1", "%2"], "{panes}");
    // Both sessions are still live and each keeps its own pane.
    assert_eq!(live_panes_by_slug(&env).len(), 2);
    for slug in ["alpha", "beta"] {
        assert_eq!(env.json(&["show", slug])["live_sessions"], 1);
    }
}

#[test]
fn the_full_quest_lifecycle() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();

    // list: the fresh master counts as an active session.
    let list = env.quests(&["list"]);
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
    assert_eq!(entered["session"]["id"], created["session"]["id"]);
    assert_eq!(entered["attach"], "exec");
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo", created["session"]["tmux_pane"].as_str().unwrap()])
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
    assert!(env.quests(&["list"]).as_array().unwrap().is_empty());
    let all = env.quests(&["list", "--all"]);
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
    assert_eq!(env.quests(&["list"])[0]["display_state"], "active");
    assert!(event_kinds(&env, &quest_id).contains(&"quest.resumed".to_string()));

    // rename
    let renamed = env.json(&["rename", "foo", "bar"]);
    assert_eq!(renamed["quest"]["slug"], "bar");
    assert_eq!(renamed["quest"]["name_source"], "manual");
    assert_eq!(renamed["tmux_session"], "q-bar");
    assert_eq!(pane_of(&env.fixture(), "q-bar")["window_name"], "master");
    assert_eq!(env.quests(&["list"])[0]["slug"], "bar");
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
    assert!(
        env.quests(&["list", "--all"])
            .as_array()
            .unwrap()
            .is_empty()
    );
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
        env.quests(&["list", "--state", "active"])
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(env.quests(&["list", "--state", "active"])[0]["slug"], "bar");
    assert!(
        env.quests(&["list", "--state", "idle"])
            .as_array()
            .unwrap()
            .is_empty()
    );
    // `--state finished` implies `--all`.
    let finished = env.quests(&["list", "--state", "finished"]);
    assert_eq!(finished.as_array().unwrap().len(), 1);
    assert_eq!(finished[0]["slug"], "foo");
}

#[test]
fn list_filters_by_machine_only_when_asked() {
    let env = Env::new();
    env.new_quest("foo");
    assert_eq!(env.quests(&["list"]).as_array().unwrap().len(), 1);
    // `elsewhere` has to be a machine this `q` knows: a configured remote,
    // asked for nothing here because `--no-remote` keeps the round local.
    env.with_remotes(&[("elsewhere", "elsewhere-host")]);
    assert!(
        env.quests(&["--machine", "elsewhere", "--no-remote", "list"])
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
    assert_eq!(env.quests(&["list"])[0]["display_state"], "active");
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
    assert_eq!(
        env.json(&["set", "foo", "auto_reset", "off"])["quest"]["auto_reset"],
        false
    );
    assert_eq!(
        env.json(&["set", "foo", "auto_reset", "on"])["quest"]["auto_reset"],
        true
    );
    assert_eq!(
        env.json(&["set", "foo", "auto_reset", "default"])["quest"]["auto_reset"],
        serde_json::Value::Null
    );
    env.cmd()
        .args(["set", "foo", "auto_reset", "maybe"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("expected on, off"));
    // `brain` still waits on the brain integration.
    env.cmd()
        .args(["set", "foo", "brain", "x"])
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

    let list = env.quests(&["list"]);
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
    assert_eq!(env.quests(&["list"])[0]["live_sessions"], 1);
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
    assert_eq!(env.quests(&["list"])[0]["slug"], "bar");
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
    // The resolved session's role beats $Q_ROLE, and the payload agrees.
    assert_eq!(json["for"], "master");
    let md = json["markdown"].as_str().unwrap();
    assert!(md.contains("You are session `master`"), "{md}");
    assert!(
        md.contains("role `worker` was requested, the session's wins"),
        "{md}"
    );

    let json = env.json(&["brief", "from-env", "--session", "from-env/master"]);
    assert!(
        json["markdown"]
            .as_str()
            .unwrap()
            .contains("You are session `master`")
    );
    let json = env.json(&["brief", "from-env", "--session", "ghost", "--for", "worker"]);
    assert_eq!(json["for"], "worker");
    assert!(
        json["markdown"]
            .as_str()
            .unwrap()
            .contains("_(session not found: ghost)_")
    );
}

#[test]
fn brief_survives_a_closed_pipe() {
    let env = Env::new();
    env.new_quest("piped");
    // The read end is closed before `q` writes, so the write hits EPIPE.
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("q"))
        .env("Q_DB", env.dir.path().join("q.db"))
        .env("Q_CONFIG", env.dir.path().join("config.toml"))
        .env("Q_FIXTURE", env.dir.path().join("tmux.json"))
        .args(["brief", "piped"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
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
    // The Quest's label is what makes a row this Quest's work (see
    // `beads::selected`), so the fixture has to carry it.
    std::fs::write(
        &bd,
        serde_json::json!([{
            "id": "bd-9.1", "title": "first", "status": "open",
            "labels": [format!("quest:{id}")],
        }])
        .to_string(),
    )
    .unwrap();
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
// ------------------------------------------------------------------ q events

impl Env {
    /// Appends an event directly, returning its id. `ts` climbs with the id so
    /// ordering by either agrees.
    fn seed_event(
        &self,
        quest_id: &str,
        session_id: Option<&str>,
        kind: &str,
        payload: &str,
    ) -> i64 {
        let conn = self.conn();
        let ts: i64 = conn
            .query_row("SELECT COALESCE(MAX(ts), 0) + 1 FROM event", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO event (quest_id, session_id, ts, kind, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![quest_id, session_id, ts, kind, payload],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn master_id(&self, quest_id: &str) -> String {
        self.conn()
            .query_row(
                "SELECT id FROM session WHERE quest_id = ?1 AND label = 'master'",
                [quest_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn stdout(&self, args: &[&str]) -> String {
        let mut cmd = self.cmd();
        let assert = cmd.args(args).assert().success();
        String::from_utf8(assert.get_output().stdout.clone()).unwrap()
    }
}

/// A quest whose log, after `q new`'s own events, holds the same five rows
/// every events test reads.
fn events_env(slug: &str) -> (Env, String, String) {
    let env = Env::new();
    let quest = env.new_quest(slug);
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let master = env.master_id(&id);
    env.seed_event(&id, Some(&master), "session.start", "null");
    env.seed_event(&id, Some(&master), "note", r#"{"text":"first note"}"#);
    env.seed_event(&id, None, "note", r#"{"text":"cli note"}"#);
    env.seed_event(&id, Some(&master), "phase", r#"{"phase":"implementing"}"#);
    env.seed_event(&id, Some(&master), "session.stop", "null");
    (env, id, master)
}

fn kinds_in(out: &str) -> Vec<String> {
    out.lines()
        .map(|l| l.split("  ").nth(1).unwrap().trim().to_string())
        .collect()
}

#[test]
fn events_default_page_is_chronological_with_session_labels() {
    let (env, _id, _master) = events_env("evt");
    let out = env.stdout(&["events", "evt"]);
    let kinds = kinds_in(&out);
    assert_eq!(
        &kinds[kinds.len() - 5..],
        ["session.start", "note", "note", "phase", "session.stop"]
    );
    // `q new` recorded its own creation first.
    assert_eq!(kinds[0], "quest.created", "{out}");
    let note = out.lines().find(|l| l.contains("first note")).unwrap();
    assert!(note.contains("[master]"), "{note}");
    assert!(note.contains("text=first note"), "{note}");
    let cli = out.lines().find(|l| l.contains("cli note")).unwrap();
    assert!(cli.contains("[-]"), "{cli}");
    // `YYYY-MM-DD HH:MM:SS` then two spaces.
    let stamp = &note[..21];
    assert!(
        stamp.len() == 21
            && &stamp[4..5] == "-"
            && &stamp[10..11] == " "
            && &stamp[13..14] == ":"
            && &stamp[16..17] == ":"
            && &stamp[19..21] == "  ",
        "{note}"
    );
}

#[test]
fn events_limit_keeps_the_newest_n_oldest_first() {
    let (env, _id, _master) = events_env("evt");
    let out = env.stdout(&["events", "evt", "-n", "2"]);
    assert_eq!(kinds_in(&out), ["phase", "session.stop"]);
}

#[test]
fn events_kind_filters_exact_glob_and_multi() {
    let (env, _id, _master) = events_env("evt");
    assert_eq!(
        kinds_in(&env.stdout(&["events", "evt", "--kind", "note"])),
        ["note", "note"]
    );
    assert_eq!(
        kinds_in(&env.stdout(&["events", "evt", "-k", "session.*"])),
        ["session.start", "session.stop"]
    );
    assert_eq!(
        kinds_in(&env.stdout(&["events", "evt", "-k", "phase", "-k", "session.stop"])),
        ["phase", "session.stop"]
    );
    env.cmd()
        .args(["events", "evt", "-k", "se*sion"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("trailing"));
}

#[test]
fn events_session_filters_by_label_or_id() {
    let (env, id, master) = events_env("evt");
    let by_label = kinds_in(&env.stdout(&["events", "evt", "--session", "master", "-k", "note"]));
    assert_eq!(by_label, ["note"]);
    let by_id = env.json(&["events", &id, "--session", &master, "-k", "note"]);
    assert_eq!(by_id.as_array().unwrap().len(), 1);
    assert_eq!(by_id[0]["session_id"], master.as_str());
    let mut cmd = env.cmd();
    let assert = cmd
        .args(["events", "evt", "--session", "ghost", "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
}

#[test]
fn events_json_is_an_array_of_events() {
    let (env, id, master) = events_env("evt");
    let json = env.json(&["events", "evt", "-n", "3"]);
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0]["kind"], "note");
    assert_eq!(arr[0]["quest_id"], id.as_str());
    assert_eq!(arr[0]["payload"]["text"], "cli note");
    assert_eq!(arr[2]["kind"], "session.stop");
    assert_eq!(arr[2]["session_id"], master.as_str());
    assert!(arr[0]["id"].as_i64().unwrap() < arr[2]["id"].as_i64().unwrap());
}

#[test]
fn events_resolves_the_quest_from_env_and_errors_without_one() {
    let (env, id, _master) = events_env("evt");
    env.cmd()
        .arg("events")
        .assert()
        .failure()
        .stderr(predicate::str::contains("Q_QUEST"));
    let mut cmd = env.cmd();
    let assert = cmd
        .env("Q_QUEST", &id)
        .args(["events", "-n", "1", "--json"])
        .assert()
        .success();
    let json = json_of(&assert);
    assert_eq!(json[0]["kind"], "session.stop");
}

#[test]
fn events_follow_prints_rows_inserted_between_polls() {
    let (env, id, master) = events_env("evt");
    let db_path = env.dir.path().join("q.db");
    let quest_id = id.clone();
    let session_id = master.clone();
    let inserter = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        conn.execute(
            "INSERT INTO event (quest_id, session_id, ts, kind, payload) \
             VALUES (?1, ?2, 999999, 'note', '{\"text\":\"late\"}')",
            rusqlite::params![quest_id, session_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event (quest_id, session_id, ts, kind, payload) \
             VALUES (?1, NULL, 999999, 'phase', '{\"phase\":\"done\"}')",
            [&quest_id],
        )
        .unwrap();
    });

    let mut cmd = env.cmd();
    let assert = cmd
        .env("Q_FOLLOW_ITERATIONS", "6")
        .args([
            "events", "evt", "--follow", "-n", "1", "-k", "note", "--json",
        ])
        .assert()
        .success();
    inserter.join().unwrap();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    // NDJSON: the initial page and the late row, one object per line; the
    // `phase` row is filtered out.
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert_eq!(rows.len(), 2, "{out}");
    assert_eq!(rows[0]["payload"]["text"], "cli note");
    assert_eq!(rows[1]["payload"]["text"], "late");
    assert_eq!(rows[1]["session_id"], master.as_str());

    // Human follow renders the same stream as lines.
    let mut cmd = env.cmd();
    let assert = cmd
        .env("Q_FOLLOW_ITERATIONS", "1")
        .args(["events", "evt", "-f", "-n", "1"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(kinds_in(&out), ["phase"]);
}

#[test]
fn events_follow_with_an_empty_first_page_tails_from_the_end() {
    let (env, id, _master) = events_env("evt");
    let db_path = env.dir.path().join("q.db");
    let quest_id = id.clone();
    let inserter = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        conn.execute(
            "INSERT INTO event (quest_id, session_id, ts, kind, payload) \
             VALUES (?1, NULL, 999999, 'note', '{\"text\":\"only this\"}')",
            [&quest_id],
        )
        .unwrap();
    });
    let mut cmd = env.cmd();
    let assert = cmd
        .env("Q_FOLLOW_ITERATIONS", "6")
        .args(["events", "evt", "-n", "0", "-f", "--json"])
        .assert()
        .success();
    inserter.join().unwrap();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let rows: Vec<serde_json::Value> = out
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // History is not replayed; only the row inserted after start shows up.
    assert_eq!(rows.len(), 1, "{out}");
    assert_eq!(rows[0]["payload"]["text"], "only this");
}

#[test]
fn events_follow_survives_a_closed_pipe() {
    // The initial page is empty, so the first write happens on the second
    // poll — after the reader has gone away — and must hit EPIPE, not fail.
    let (env, id, _master) = events_env("evt");
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("q"))
        .env("Q_DB", env.dir.path().join("q.db"))
        .env("Q_CONFIG", env.dir.path().join("config.toml"))
        .env("Q_FIXTURE", env.dir.path().join("tmux.json"))
        .env("Q_FOLLOW_ITERATIONS", "6")
        .env_remove("Q_QUEST")
        .args(["events", "evt", "--follow", "-n", "0"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    std::thread::sleep(std::time::Duration::from_millis(300));
    env.seed_event(&id, None, "note", r#"{"text":"into the void"}"#);
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ------------------------------------------------ agent self-report (bd-8lz.2.5)

/// A Quest with its master session, and a command pre-wired with the env a
/// Quest pane would carry (`Q_QUEST`, `Q_SESSION`).
struct Pane {
    env: Env,
    quest_id: String,
    session_id: String,
}

impl Pane {
    fn new() -> Pane {
        let env = Env::new();
        let created = env.new_quest("alpha");
        Pane {
            quest_id: created["quest"]["id"].as_str().unwrap().to_string(),
            session_id: created["session"]["id"].as_str().unwrap().to_string(),
            env,
        }
    }

    fn cmd(&self) -> Command {
        let mut cmd = self.env.cmd();
        cmd.env("Q_QUEST", &self.quest_id)
            .env("Q_SESSION", &self.session_id);
        cmd
    }

    fn json(&self, args: &[&str]) -> serde_json::Value {
        let mut cmd = self.cmd();
        let assert = cmd.args(args).arg("--json").assert().success();
        json_of(&assert)
    }

    /// `(kind, session_id, payload)` of every event, oldest first.
    fn events(&self) -> Vec<(String, Option<String>, serde_json::Value)> {
        self.env
            .conn()
            .prepare("SELECT kind, session_id, payload FROM event WHERE quest_id = ?1 ORDER BY id")
            .unwrap()
            .query_map([&self.quest_id], |r| {
                let payload: Option<String> = r.get(2)?;
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    payload
                        .map(|p| serde_json::from_str(&p).unwrap())
                        .unwrap_or(serde_json::Value::Null),
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn last_event(&self) -> (String, Option<String>, serde_json::Value) {
        self.events().pop().unwrap()
    }
}

fn error_json(assert: &assert_cmd::assert::Assert) -> serde_json::Value {
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    serde_json::from_str(stderr.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {stderr}"))
}

#[test]
fn phase_updates_the_session_and_logs_an_event() {
    let pane = Pane::new();
    pane.cmd()
        .args(["phase", "implementing"])
        .assert()
        .success()
        .stdout("phase set: implementing\n");

    let phase: String = pane
        .env
        .conn()
        .query_row(
            "SELECT phase FROM session WHERE id = ?1",
            [&pane.session_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(phase, "implementing");
    let (kind, session, payload) = pane.last_event();
    assert_eq!(kind, "phase");
    assert_eq!(session.as_deref(), Some(pane.session_id.as_str()));
    assert_eq!(payload, serde_json::json!({ "text": "implementing" }));

    let out = pane.json(&["phase", "reviewing"]);
    assert_eq!(out["phase"], "reviewing");
    assert_eq!(out["session_id"], pane.session_id.as_str());
    assert_eq!(out["quest_id"], pane.quest_id.as_str());
}

#[test]
fn phase_requires_a_session() {
    let pane = Pane::new();
    let assert = pane
        .cmd()
        .env_remove("Q_SESSION")
        .args(["phase", "planning", "--json"])
        .assert()
        .code(1);
    assert!(
        error_json(&assert)["error"]
            .as_str()
            .unwrap()
            .contains("Q_SESSION")
    );
    assert!(pane.events().iter().all(|(k, _, _)| k != "phase"));
}

#[test]
fn self_report_needs_a_quest_from_env_or_flag() {
    let pane = Pane::new();
    let assert = pane
        .cmd()
        .env_remove("Q_QUEST")
        .env_remove("Q_SESSION")
        .args(["note", "hello", "--json"])
        .assert()
        .code(1);
    assert!(
        error_json(&assert)["error"]
            .as_str()
            .unwrap()
            .contains("--quest")
    );

    // `--quest` resolves through the usual target rules (slug here) and works
    // without a session: the event is attributed to nobody.
    let out = {
        let mut cmd = pane.env.cmd();
        let assert = cmd
            .args(["note", "from outside", "--quest", "alpha", "--json"])
            .assert()
            .success();
        json_of(&assert)
    };
    assert_eq!(out["quest_id"], pane.quest_id.as_str());
    assert!(out["session_id"].is_null());
    let (kind, session, _) = pane.last_event();
    assert_eq!(kind, "note");
    assert_eq!(session, None);
}

#[test]
fn stale_session_env_is_an_error() {
    let pane = Pane::new();
    let assert = pane
        .cmd()
        .env("Q_SESSION", "s-dead")
        .args(["note", "hello", "--json"])
        .assert()
        .code(1);
    assert_eq!(error_json(&assert)["code"], "not_found");
}

#[test]
fn note_payload_carries_blocker_only_when_set() {
    let pane = Pane::new();
    pane.cmd()
        .args(["note", "all good"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"^note #\d+ all good\n$").unwrap());
    let (kind, session, payload) = pane.last_event();
    assert_eq!(kind, "note");
    assert_eq!(session.as_deref(), Some(pane.session_id.as_str()));
    assert_eq!(payload, serde_json::json!({ "text": "all good" }));

    pane.cmd()
        .args(["note", "--blocker", "DB is locked"])
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"^note #\d+ \[blocker\] DB is locked\n$").unwrap());
    let (_, _, payload) = pane.last_event();
    assert_eq!(
        payload,
        serde_json::json!({ "text": "DB is locked", "blocker": true })
    );

    let out = pane.json(&["note", "x", "--blocker"]);
    assert_eq!(out["blocker"], true);
    assert_eq!(out["text"], "x");
    assert!(out["event_id"].is_i64());
}

#[test]
fn link_add_detects_the_kind_and_is_idempotent() {
    let pane = Pane::new();
    let url = "https://github.com/acme/api/pull/42";
    pane.cmd()
        .args(["link", "add", url, "--title", "Fix it"])
        .assert()
        .success()
        .stdout(format!("link #1 pr {url} — Fix it\n"));
    let (kind, session, payload) = pane.last_event();
    assert_eq!(kind, "link.added");
    assert_eq!(session.as_deref(), Some(pane.session_id.as_str()));
    assert_eq!(
        payload,
        serde_json::json!({ "id": 1, "kind": "pr", "ref": url })
    );

    // Same ref again: the existing row, no new event.
    let out = pane.json(&["link", "add", url]);
    assert_eq!(out["created"], false);
    assert_eq!(out["link"]["id"], 1);
    assert_eq!(out["link"]["title"], "Fix it");
    assert_eq!(
        pane.events()
            .iter()
            .filter(|(k, _, _)| k == "link.added")
            .count(),
        1
    );
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);

    let out = pane.json(&["link", "add", "https://app.productive.io/1-acme/tasks/123"]);
    assert_eq!(out["created"], true);
    assert_eq!(out["link"]["kind"], "task");
    assert_eq!(out["link"]["session_id"], pane.session_id.as_str());
    let out = pane.json(&["link", "add", "bd-8lz.2.5"]);
    assert_eq!(out["link"]["kind"], "beads");
    let out = pane.json(&["link", "add", "https://example.com/docs"]);
    assert_eq!(out["link"]["kind"], "url");
}

#[test]
fn link_add_detects_worktrees_and_files_by_absolute_path() {
    let pane = Pane::new();
    let wt = pane.env.work("wt");
    std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
    let out = pane.json(&["link", "add", wt.to_str().unwrap()]);
    assert_eq!(out["link"]["kind"], "worktree");
    assert_eq!(out["link"]["ref"], wt.to_str().unwrap());

    // Relative to the cwd, stored absolute.
    let out_dir = pane.env.work("out");
    std::fs::write(out_dir.join("report.md"), "# hi").unwrap();
    let mut cmd = pane.cmd();
    cmd.current_dir(&out_dir);
    let assert = cmd
        .args(["link", "add", "report.md", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["link"]["kind"], "artifact");
    assert_eq!(
        out["link"]["ref"],
        out_dir.join("report.md").to_str().unwrap()
    );
}

#[test]
fn link_add_asks_for_a_kind_it_cannot_detect() {
    let pane = Pane::new();
    let assert = pane
        .cmd()
        .args(["link", "add", "feat/some-branch", "--json"])
        .assert()
        .code(1);
    let err = error_json(&assert);
    assert_eq!(err["code"], "invalid");
    assert!(err["error"].as_str().unwrap().contains("--kind"), "{err}");
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 0);

    let out = pane.json(&["link", "add", "feat/some-branch", "--kind", "branch"]);
    assert_eq!(out["link"]["kind"], "branch");
    assert_eq!(out["link"]["ref"], "feat/some-branch");
}

#[test]
fn link_rm_deletes_and_logs() {
    let pane = Pane::new();
    let id = pane.json(&["link", "add", "https://example.com"])["link"]["id"]
        .as_i64()
        .unwrap();
    pane.cmd()
        .args(["link", "rm", &id.to_string()])
        .assert()
        .success()
        .stdout(format!("link #{id} removed\n"));
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 0);
    let (kind, _, payload) = pane.last_event();
    assert_eq!(kind, "link.removed");
    assert_eq!(
        payload,
        serde_json::json!({ "id": id, "kind": "url", "ref": "https://example.com" })
    );

    let assert = pane
        .cmd()
        .args(["link", "rm", "999", "--json"])
        .assert()
        .code(1);
    assert_eq!(error_json(&assert)["code"], "not_found");
}

#[test]
fn link_rm_refuses_another_quests_link() {
    let pane = Pane::new();
    let other = pane.env.new_quest("beta")["quest"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut cmd = pane.env.cmd();
    let assert = cmd
        .args([
            "link",
            "add",
            "https://example.com",
            "--quest",
            &other,
            "--json",
        ])
        .assert()
        .success();
    let id = json_of(&assert)["link"]["id"].as_i64().unwrap();
    pane.cmd()
        .args(["link", "rm", &id.to_string()])
        .assert()
        .code(1);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);
}

#[test]
fn links_lists_grouped_by_kind_or_as_json() {
    let pane = Pane::new();
    pane.cmd()
        .args(["links"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("no links on"));

    pane.json(&["link", "add", "https://example.com/b"]);
    pane.json(&[
        "link",
        "add",
        "https://github.com/acme/api/pull/1",
        "--title",
        "One",
    ]);
    pane.json(&["link", "add", "https://example.com/a"]);

    pane.cmd().args(["links"]).assert().success().stdout(
        "pr\n  #2 https://github.com/acme/api/pull/1 — One\nurl\n  #1 https://example.com/b\n  #3 https://example.com/a\n",
    );

    let out = pane.json(&["links"]);
    let arr = out.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[1]["kind"], "pr");
    assert_eq!(arr[1]["title"], "One");

    // Positional target, from outside any pane.
    let mut cmd = pane.env.cmd();
    let assert = cmd.args(["links", "alpha", "--json"]).assert().success();
    assert_eq!(json_of(&assert).as_array().unwrap().len(), 3);
}

#[test]
fn artifact_add_stores_the_note_in_meta() {
    let pane = Pane::new();
    let out_dir = pane.env.work("out");
    let file = out_dir.join("report.html");
    std::fs::write(&file, "<h1/>").unwrap();

    let mut cmd = pane.cmd();
    cmd.current_dir(&out_dir);
    cmd.args(["artifact", "add", "report.html", "--note", "the report"])
        .assert()
        .success()
        .stdout(format!("link #1 artifact {}\n", file.to_str().unwrap()));

    let out = pane.json(&["links"]);
    let link = &out.as_array().unwrap()[0];
    assert_eq!(link["kind"], "artifact");
    assert_eq!(link["ref"], file.to_str().unwrap());
    assert_eq!(link["meta"], serde_json::json!({ "note": "the report" }));

    let (kind, session, payload) = pane.last_event();
    assert_eq!(kind, "artifact.added");
    assert_eq!(session.as_deref(), Some(pane.session_id.as_str()));
    assert_eq!(
        payload,
        serde_json::json!({
            "id": 1,
            "kind": "artifact",
            "ref": file.to_str().unwrap(),
            "note": "the report",
        })
    );

    // A file that does not exist yet is accepted (artifacts are promised
    // before they are written) as long as its directory does.
    let later = out_dir.join("later.md");
    let out = pane.json(&["artifact", "add", later.to_str().unwrap()]);
    assert_eq!(out["link"]["ref"], later.to_str().unwrap());
    assert!(out["link"]["meta"].is_null());
}

#[test]
fn artifact_add_rejects_urls_and_missing_directories() {
    let pane = Pane::new();
    for path in ["https://example.com/report.html", "/nope/deeper/later.md"] {
        let assert = pane
            .cmd()
            .args(["artifact", "add", path, "--json"])
            .assert()
            .code(1);
        assert_eq!(error_json(&assert)["code"], "invalid", "{path}");
    }
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 0);
}

#[test]
fn artifact_add_fills_a_missing_note_but_keeps_an_existing_one() {
    let pane = Pane::new();
    let file = pane.env.work("out").join("r.md");
    let path = file.to_str().unwrap();
    pane.json(&["artifact", "add", path]);
    pane.cmd()
        .args(["artifact", "add", path, "--note", "first"])
        .assert()
        .success()
        .stdout(predicate::str::ends_with("(already linked; note set)\n"));
    let out = pane.json(&["artifact", "add", path, "--note", "second"]);
    assert_eq!(out["created"], false);
    assert_eq!(out["link"]["meta"]["note"], "first");
    pane.cmd()
        .args(["artifact", "add", path, "--note", "second"])
        .assert()
        .success()
        .stdout(predicate::str::ends_with(
            "(already linked; kept existing note)\n",
        ));
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);
    assert_eq!(
        pane.events()
            .iter()
            .filter(|(k, _, _)| k == "artifact.added")
            .count(),
        1
    );
}

#[test]
fn paths_expand_a_leading_tilde() {
    let pane = Pane::new();
    let home = pane.env.work("home");
    std::fs::create_dir_all(home.join("out")).unwrap();
    let mut cmd = pane.cmd();
    let assert = cmd
        .env("HOME", &home)
        .args(["artifact", "add", "~/out/report.md", "--json"])
        .assert()
        .success();
    assert_eq!(
        json_of(&assert)["link"]["ref"],
        home.join("out/report.md").to_str().unwrap()
    );

    let mut cmd = pane.cmd();
    let assert = cmd
        .env("HOME", &home)
        .args(["link", "add", "~/out/report.md", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    // Same file already linked as an artifact: the row is reused.
    assert_eq!(out["created"], false);
    assert_eq!(out["link"]["kind"], "artifact");
}

#[test]
fn link_add_normalises_pr_urls_and_dedupes_across_kinds() {
    let pane = Pane::new();
    let canonical = "https://github.com/acme/api/pull/42";
    let out = pane.json(&[
        "link",
        "add",
        "http://www.github.com/acme/api/pull/42/files?diff=split#r1",
    ]);
    assert_eq!(out["link"]["kind"], "pr");
    assert_eq!(out["link"]["ref"], canonical);
    let out = pane.json(&[
        "link",
        "add",
        "https://github.com/acme/api/pull/42#issuecomment-1",
    ]);
    assert_eq!(out["created"], false);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);

    // Added as a plain url first, autodetected later: still one row.
    let docs = "https://example.com/docs";
    pane.json(&["link", "add", docs, "--kind", "url"]);
    let out = pane.json(&["link", "add", docs]);
    assert_eq!(out["created"], false);
    assert_eq!(out["link"]["kind"], "url");
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 2);
    // An explicit different kind is a deliberate second row.
    let out = pane.json(&["link", "add", docs, "--kind", "brain"]);
    assert_eq!(out["created"], true);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 3);
}

#[test]
fn link_add_sets_a_missing_title_but_keeps_an_existing_one() {
    let pane = Pane::new();
    let url = "https://example.com";
    pane.json(&["link", "add", url]);
    pane.cmd()
        .args(["link", "add", url, "--title", "Docs"])
        .assert()
        .success()
        .stdout(format!(
            "link #1 url {url} — Docs (already linked; title set)\n"
        ));
    pane.cmd()
        .args(["link", "add", url, "--title", "Other"])
        .assert()
        .success()
        .stdout(format!(
            "link #1 url {url} — Docs (already linked; kept existing title)\n"
        ));
    let out = pane.json(&["links"]);
    assert_eq!(out[0]["title"], "Docs");
    assert_eq!(
        pane.events()
            .iter()
            .filter(|(k, _, _)| k == "link.added")
            .count(),
        1
    );
}

#[test]
fn links_is_read_only_and_ignores_the_pane_session() {
    let pane = Pane::new();
    let other = pane.env.new_quest("beta");
    let other_id = other["quest"]["id"].as_str().unwrap();
    let mut cmd = pane.env.cmd();
    cmd.args(["link", "add", "https://example.com", "--quest", other_id])
        .assert()
        .success();

    // From alpha's pane, listing beta works…
    let out = pane.json(&["links", "beta"]);
    assert_eq!(out.as_array().unwrap().len(), 1);
    // …but writing to beta from alpha's session does not.
    let assert = pane
        .cmd()
        .args(["note", "x", "--quest", "beta", "--json"])
        .assert()
        .code(1);
    assert_eq!(error_json(&assert)["code"], "invalid");
}

#[test]
fn ended_session_env_is_rejected_for_writes() {
    let pane = Pane::new();
    pane.env
        .conn()
        .execute(
            "UPDATE session SET status = 'ended' WHERE id = ?1",
            [&pane.session_id],
        )
        .unwrap();
    let assert = pane.cmd().args(["note", "late", "--json"]).assert().code(1);
    let err = error_json(&assert);
    assert_eq!(err["code"], "invalid");
    assert!(err["error"].as_str().unwrap().contains("ended"), "{err}");
    assert!(pane.events().iter().all(|(k, _, _)| k != "note"));
    // Reads still work.
    pane.cmd().args(["links"]).assert().success();
}

// ------------------------------------------------ PostToolUse auto-capture

/// A Claude Code `PostToolUse` payload for a Bash call.
fn bash_payload(cwd: &std::path::Path, command: &str, stdout: &str) -> String {
    serde_json::json!({
        "session_id": "claude-1",
        "cwd": cwd,
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command, "description": "x" },
        "tool_response": { "stdout": stdout, "stderr": "", "interrupted": false },
    })
    .to_string()
}

fn post_tool_use(cmd: &mut Command, payload: &str) {
    cmd.args(["hook", "post-tool-use"])
        .write_stdin(payload.to_string())
        .assert()
        .success()
        .stdout("");
}

#[test]
fn post_tool_use_captures_a_pr_url_once() {
    let pane = Pane::new();
    let cwd = pane.env.work("alpha");
    let pr = "https://github.com/acme/api/pull/42";
    let payload = bash_payload(&cwd, "gh pr create", &format!("{pr}/files\n{pr}\n"));

    post_tool_use(&mut pane.cmd(), &payload);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);
    let (kind, session, event) = pane.last_event();
    assert_eq!(kind, "link.added");
    assert_eq!(session.as_deref(), Some(pane.session_id.as_str()));
    assert_eq!(
        event,
        serde_json::json!({ "id": 1, "kind": "pr", "ref": pr, "auto": true })
    );
    let links = pane.json(&["links"]);
    assert_eq!(links[0]["kind"], "pr");
    assert_eq!(links[0]["ref"], pr);
    assert_eq!(links[0]["session_id"], pane.session_id.as_str());

    // The same output again: no second row, no second event.
    post_tool_use(&mut pane.cmd(), &payload);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);
    assert_eq!(
        pane.events()
            .iter()
            .filter(|(k, _, _)| k == "link.added")
            .count(),
        1
    );
}

#[test]
fn post_tool_use_captures_worktrees_and_beads() {
    let pane = Pane::new();
    let cwd = pane.env.work("alpha");
    post_tool_use(
        &mut pane.cmd(),
        &bash_payload(&cwd, "git worktree add .worktrees/x -b feat/x", ""),
    );
    post_tool_use(
        &mut pane.cmd(),
        &bash_payload(&cwd, "bd create 'thing' -l repo:x", "Created bd-8lz.9\n"),
    );
    let links = pane.json(&["links"]);
    let refs: Vec<(String, String)> = links
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["kind"].as_str().unwrap().to_string(),
                l["ref"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(
        refs,
        vec![
            (
                "worktree".to_string(),
                cwd.join(".worktrees/x").to_string_lossy().into_owned()
            ),
            ("branch".to_string(), "feat/x".to_string()),
            ("beads".to_string(), "bd-8lz.9".to_string()),
        ]
    );
}

#[test]
fn post_tool_use_captures_written_artifacts() {
    let pane = Pane::new();
    let cwd = pane.env.work("alpha");
    let file = cwd.join("output").join("report.html");
    let payload = serde_json::json!({
        "session_id": "claude-1",
        "cwd": cwd,
        "tool_name": "Write",
        "tool_input": { "file_path": file, "content": "<h1>hi</h1>" },
        "tool_response": { "filePath": file, "success": true },
    })
    .to_string();
    post_tool_use(&mut pane.cmd(), &payload);

    let links = pane.json(&["links"]);
    assert_eq!(links[0]["kind"], "artifact");
    assert_eq!(links[0]["ref"], file.to_str().unwrap());
    assert_eq!(links[0]["meta"]["note"], "auto-captured (Write)");
    let (kind, _, event) = pane.last_event();
    assert_eq!(kind, "artifact.added");
    assert_eq!(event["auto"], true);
    assert_eq!(event["note"], "auto-captured (Write)");

    // Source files are not artifacts.
    let src = cwd.join("src").join("x.rs");
    let payload = serde_json::json!({
        "cwd": cwd,
        "tool_name": "Write",
        "tool_input": { "file_path": src, "content": "" },
        "tool_response": { "filePath": src, "success": true },
    })
    .to_string();
    post_tool_use(&mut pane.cmd(), &payload);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);
}

#[test]
fn post_tool_use_without_a_session_env_captures_at_quest_level() {
    let pane = Pane::new();
    let cwd = pane.env.work("alpha");
    let pr = "https://github.com/acme/api/pull/7";
    let mut cmd = pane.env.cmd();
    cmd.env("Q_QUEST", &pane.quest_id)
        .env_remove("Q_SESSION")
        .env_remove("TMUX_PANE");
    post_tool_use(&mut cmd, &bash_payload(&cwd, "gh pr create", pr));

    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 1);
    assert_eq!(
        pane.env
            .count("SELECT count(*) FROM link WHERE session_id IS NULL"),
        1
    );
    let (kind, session, _) = pane.last_event();
    assert_eq!(kind, "link.added");
    assert_eq!(session, None);
}

#[test]
fn post_tool_use_is_a_noop_outside_a_quest_and_never_creates_the_database() {
    // A live database, but no Q_QUEST: untouched.
    let pane = Pane::new();
    let cwd = pane.env.work("alpha");
    let payload = bash_payload(&cwd, "gh pr view", "https://github.com/acme/api/pull/1");
    let mut cmd = pane.env.cmd();
    cmd.env("Q_SESSION", &pane.session_id);
    post_tool_use(&mut cmd, &payload);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 0);

    // A session that does not exist named in the env: nothing captured.
    let mut cmd = pane.env.cmd();
    cmd.env("Q_QUEST", &pane.quest_id)
        .env("Q_SESSION", "s-nope");
    post_tool_use(&mut cmd, &payload);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 0);

    // A session from another quest named in the env: nothing captured.
    let other = pane.env.new_quest("beta");
    let mut cmd = pane.env.cmd();
    cmd.env("Q_QUEST", other["quest"]["id"].as_str().unwrap())
        .env("Q_SESSION", &pane.session_id);
    post_tool_use(&mut cmd, &payload);
    assert_eq!(pane.env.count("SELECT count(*) FROM link"), 0);

    // No database at all: Q_QUEST set, but the file must not appear.
    let env = Env::new();
    let mut cmd = env.cmd();
    cmd.env("Q_QUEST", "q-nope").env("Q_SESSION", "s-nope");
    post_tool_use(&mut cmd, &payload);
    assert!(!env.dir.path().join("q.db").exists());

    // Garbage on stdin is fine too.
    post_tool_use(&mut pane.cmd(), "not json");
}

// ------------------------------------------------------------------- q spawn

/// The pane of `<session>:<window>`, from the fixture.
fn window_of(fixture: &serde_json::Value, session: &str, window: &str) -> serde_json::Value {
    let panes = fixture["panes"].as_array().unwrap();
    let mut found = panes
        .iter()
        .filter(|p| p["session_name"] == session && p["window_name"] == window);
    let pane = found
        .next()
        .unwrap_or_else(|| panic!("no pane in `{session}:{window}`: {fixture}"));
    assert!(found.next().is_none(), "more than one `{session}:{window}`");
    pane.clone()
}

/// The payload of the newest event of `kind`, for a Quest.
fn last_payload(env: &Env, quest_id: &str, kind: &str) -> serde_json::Value {
    let text: String = env
        .conn()
        .query_row(
            "SELECT payload FROM event WHERE quest_id = ?1 AND kind = ?2 ORDER BY id DESC LIMIT 1",
            [quest_id, kind],
            |r| r.get(0),
        )
        .unwrap_or_else(|e| panic!("no `{kind}` event: {e}"));
    serde_json::from_str(&text).unwrap()
}

#[test]
fn spawn_opens_a_worker_window_and_records_the_session() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    let cwd = created["quest"]["cwd"].as_str().unwrap().to_string();

    let out = env.json(&["spawn", "foo", "write the tests", "--label", "tests"]);
    assert_eq!(out["quest"]["id"], quest_id.as_str());
    assert_eq!(out["tmux_session"], "q-foo");
    assert_eq!(out["window"], "w1-tests");
    assert_eq!(out["attach"], "none");
    assert_eq!(out["session"]["role"], "worker");
    assert_eq!(out["session"]["label"], "tests");
    assert_eq!(out["session"]["status"], "starting");
    assert_eq!(out["session"]["first_prompt"], "write the tests");
    assert_eq!(out["session"]["tmux_session"], "q-foo");
    let session_id = out["session"]["id"].as_str().unwrap().to_string();

    // The window lives in the Quest's session, next to the master.
    let fixture = env.fixture();
    assert_eq!(fixture["attached"], serde_json::Value::Null);
    let pane = window_of(&fixture, "q-foo", "w1-tests");
    assert_eq!(pane["window_index"], 1);
    assert_eq!(window_of(&fixture, "q-foo", "master")["window_index"], 0);
    assert_eq!(out["session"]["tmux_pane"], pane["pane_id"]);
    assert_ne!(pane["pane_id"], created["session"]["tmux_pane"]);

    // Same env as the master, but `Q_ROLE=worker` and its own `Q_SESSION`.
    assert_eq!(pane["env"]["Q_QUEST"], quest_id.as_str());
    assert_eq!(pane["env"]["Q_SESSION"], session_id.as_str());
    assert_eq!(pane["env"]["Q_ROLE"], "worker");
    assert_eq!(
        pane["env"]["Q_MACHINE"],
        window_of(&fixture, "q-foo", "master")["env"]["Q_MACHINE"]
    );
    assert!(pane["env"]["Q_DB"].is_string());
    assert!(pane["env"]["Q_CONFIG"].is_string());
    // Claude is named after the label, not the window.
    assert_eq!(pane["command"], "claude -n foo/tests -- 'write the tests'");
    // No `--dir`, so the worker starts in the Quest's own directory.
    assert_eq!(pane["cwd"], cwd.as_str());

    let conn = env.conn();
    let (q_id, role, label, status, prompt): (String, String, String, String, String) = conn
        .query_row(
            "SELECT quest_id, role, label, status, first_prompt FROM session WHERE id = ?1",
            [&session_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        (
            q_id.as_str(),
            role.as_str(),
            label.as_str(),
            status.as_str(),
            prompt.as_str()
        ),
        (
            quest_id.as_str(),
            "worker",
            "tests",
            "starting",
            "write the tests"
        )
    );

    let kinds = event_kinds(&env, &quest_id);
    assert_eq!(kinds, vec!["quest.created", "session.spawn"]);
    let payload = last_payload(&env, &quest_id, "session.spawn");
    assert_eq!(payload["label"], "tests");
    assert_eq!(payload["window"], "w1-tests");
    assert_eq!(payload["role"], "worker");
    assert_eq!(payload["cwd"], cwd.as_str());
    assert_eq!(payload["prompt"], "write the tests");
    let session_of_event: String = conn
        .query_row(
            "SELECT session_id FROM event WHERE quest_id = ?1 AND kind = 'session.spawn'",
            [&quest_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session_of_event, session_id);

    // `q show` lists the worker beside the master.
    let shown = env.json(&["show", "foo"]);
    assert_eq!(shown["live_sessions"], 2);
    let labels: Vec<&str> = shown["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"master") && labels.contains(&"tests"),
        "{labels:?}"
    );
}

#[test]
fn spawn_prints_a_human_one_liner() {
    let env = Env::new();
    env.new_quest("foo");
    env.cmd()
        .args(["spawn", "foo", "go", "--label", "tests"])
        .assert()
        .success()
        .stdout(predicate::str::contains("spawned s-"))
        .stdout(predicate::str::contains("tmux q-foo:w1-tests"))
        .stdout(predicate::str::contains("q enter foo --session tests"));
    env.cmd()
        .args(["spawn", "foo", "go", "--label", "other", "-q"])
        .assert()
        .success()
        .stdout("");
}

#[test]
fn workers_are_numbered_in_order_and_a_number_is_never_reused() {
    let env = Env::new();
    env.new_quest("foo");
    assert_eq!(
        env.json(&["spawn", "foo", "a", "--label", "tests"])["window"],
        "w1-tests"
    );
    let second = env.json(&["spawn", "foo", "b", "--label", "migration"]);
    assert_eq!(second["window"], "w2-migration");
    assert_eq!(
        window_of(&env.fixture(), "q-foo", "w2-migration")["window_index"],
        2
    );

    // The first worker's pane disappears; the sweep ends its session, which
    // frees the label — but not the number.
    let mut fixture = env.fixture();
    let panes: Vec<serde_json::Value> = fixture["panes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["window_name"] != "w1-tests")
        .cloned()
        .collect();
    fixture["panes"] = serde_json::json!(panes);
    env.write_fixture(fixture);

    let third = env.json(&["spawn", "foo", "c", "--label", "tests"]);
    assert_eq!(third["window"], "w3-tests");
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE label = 'tests' AND status = 'ended'"),
        1
    );
}

#[test]
fn spawn_refuses_a_label_that_is_already_live() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["spawn", "foo", "a", "--label", "tests"]);
    let assert = env
        .cmd()
        .args(["spawn", "foo", "b", "--label", "tests", "--json"])
        .assert()
        .code(1);
    let err: serde_json::Value =
        serde_json::from_str(&String::from_utf8(assert.get_output().stderr.clone()).unwrap())
            .unwrap();
    assert_eq!(err["code"], "conflict");
    assert!(
        err["error"].as_str().unwrap().contains("already live"),
        "{err}"
    );
    // Nothing was opened and nothing was recorded.
    assert_eq!(env.count("SELECT count(*) FROM session"), 2);
    assert_eq!(env.fixture()["panes"].as_array().unwrap().len(), 2);

    // `master` is window 0's label, so it is reserved outright.
    env.cmd()
        .args(["spawn", "foo", "b", "--label", "master"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("reserved"));
}

#[test]
fn spawn_validates_the_label_and_the_prompt() {
    let env = Env::new();
    env.new_quest("foo");
    for bad in ["Tests", "with space", "under_score", "double--dash"] {
        env.cmd()
            .args(["spawn", "foo", "go", "--label", bad])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("invalid label"));
    }
    env.cmd()
        .args(["spawn", "foo", "   ", "--label", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("needs a prompt"));
    assert_eq!(env.count("SELECT count(*) FROM session"), 1);
}

#[test]
fn spawn_needs_an_active_quest_with_a_live_tmux_session() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["close", "foo", "-f"]);
    env.cmd()
        .args(["spawn", "foo", "go", "--label", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("is finished"))
        .stderr(predicate::str::contains("q resume foo"));

    // Active, but the tmux session is gone: nothing to open a window in.
    let env = Env::new();
    env.new_quest("bar");
    env.write_fixture(serde_json::json!({ "panes": [] }));
    env.cmd()
        .args(["spawn", "bar", "go", "--label", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no tmux session `q-bar`"))
        .stderr(predicate::str::contains("q resume bar"));

    env.cmd()
        .args(["spawn", "nope", "go", "--label", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not found"));
}

#[test]
fn spawn_takes_a_directory_and_a_workflow_of_its_own() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["new", "--name", "foo", "--dir", work.to_str().unwrap()])
        .args(["--workflow", "orchestrator", "-d", "-q"])
        .assert()
        .success();
    let other = env.work("worktree");

    // Both inherited from the Quest unless overridden.
    let inherited = env.json(&["spawn", "foo", "a", "--label", "tests"]);
    assert_eq!(inherited["session"]["workflow"], "orchestrator");
    let quest_id = inherited["quest"]["id"].as_str().unwrap().to_string();
    assert_eq!(
        last_payload(&env, &quest_id, "session.spawn")["cwd"],
        work.to_str().unwrap()
    );
    assert_eq!(
        window_of(&env.fixture(), "q-foo", "w1-tests")["cwd"],
        work.to_str().unwrap()
    );

    let out = env.json(&[
        "spawn",
        "foo",
        "b",
        "--label",
        "migration",
        "--workflow",
        "review",
        "--dir",
        other.to_str().unwrap(),
    ]);
    assert_eq!(out["session"]["workflow"], "review");
    assert_eq!(
        last_payload(&env, &quest_id, "session.spawn")["cwd"],
        other.to_str().unwrap()
    );
    assert_eq!(
        window_of(&env.fixture(), "q-foo", "w2-migration")["cwd"],
        other.to_str().unwrap()
    );
    // The Quest's own cwd is untouched by `--dir`.
    assert_eq!(env.json(&["show", "foo"])["cwd"], work.to_str().unwrap());

    env.cmd()
        .args([
            "spawn",
            "foo",
            "c",
            "--label",
            "docs",
            "--dir",
            "/no/such/dir",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no such directory"));
}

#[test]
fn spawn_selects_the_new_window_only_from_inside_the_quests_session() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let master_pane = created["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();
    // A second Quest is a second real tmux session to stand in.
    let elsewhere = env.new_quest("bar");
    let elsewhere_pane = elsewhere["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();

    // Outside tmux: the window is opened, but no client is moved.
    assert_eq!(
        env.json(&["spawn", "foo", "a", "--label", "outside"])["attach"],
        "none"
    );
    assert_eq!(env.fixture()["selected"], serde_json::Value::Null);

    // Inside another tmux session: still not ours to switch.
    let mut cmd = env.cmd();
    let assert = cmd
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .env("TMUX_PANE", &elsewhere_pane)
        .args(["spawn", "foo", "b", "--label", "elsewhere", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["attach"], "none");
    assert_eq!(env.fixture()["selected"], serde_json::Value::Null);

    // From the master's own pane: the client follows the worker. `new-window -d`
    // never moves it on its own, so the selection is this step's alone.
    let mut cmd = env.cmd();
    let assert = cmd
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .env("TMUX_PANE", &master_pane)
        .args(["spawn", "foo", "c", "--label", "inside", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    // `select`, not `switch`: the client stays in the session it is already in
    // and only the active window changes.
    assert_eq!(out["attach"], "select");
    assert_eq!(out["window"], "w3-inside");
    let worker_pane = out["session"]["tmux_pane"].as_str().unwrap().to_string();
    assert_eq!(env.fixture()["selected"], worker_pane.as_str());
    // Selecting a window is not attaching: no client changed sessions.
    assert_eq!(env.fixture()["attached"], serde_json::Value::Null);

    // ... unless told not to.
    let mut cmd = env.cmd();
    let assert = cmd
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .env("TMUX_PANE", &master_pane)
        .args(["spawn", "foo", "d", "--label", "quiet", "--no-attach"])
        .arg("--json")
        .assert()
        .success();
    assert_eq!(json_of(&assert)["attach"], "none");
    assert_eq!(
        env.fixture()["selected"],
        worker_pane.as_str(),
        "--no-attach moved the client anyway"
    );
}

#[test]
fn spawn_falls_back_to_q_quest_when_the_pane_id_is_missing() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    let mut cmd = env.cmd();
    let assert = cmd
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .env("Q_QUEST", &quest_id)
        .args(["spawn", "foo", "a", "--label", "tests", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["attach"], "select");
}

#[test]
fn enter_reaches_a_worker_by_label_through_its_pane() {
    let env = Env::new();
    env.new_quest("foo");
    let spawned = env.json(&["spawn", "foo", "write the tests", "--label", "tests"]);
    let worker_pane = spawned["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();

    // The command `q spawn` printed has to work: the worker's window is
    // `w1-tests`, but it is addressed by pane id, so the label is enough.
    let entered = env.json(&["enter", "foo", "--session", "tests"]);
    assert_eq!(entered["tmux_session"], "q-foo");
    assert_eq!(entered["window"], "w1-tests");
    assert_eq!(entered["session"]["id"], spawned["session"]["id"]);
    assert_eq!(entered["attach"], "exec");
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo", worker_pane.as_str()])
    );
    assert_eq!(env.fixture()["selected"], worker_pane.as_str());

    // A rename of the window leaves the label addressable.
    let mut fixture = env.fixture();
    for pane in fixture["panes"].as_array_mut().unwrap() {
        if pane["pane_id"] == worker_pane.as_str() {
            pane["window_name"] = serde_json::json!("renamed");
        }
    }
    env.write_fixture(fixture);
    let again = env.json(&["enter", "foo", "--session", "tests"]);
    assert_eq!(again["window"], "renamed");
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo", worker_pane.as_str()])
    );

    // Without `--session` it is still the master.
    let master = env.json(&["enter", "foo"]);
    assert_eq!(master["window"], "master");
}

#[test]
fn a_spawn_whose_window_never_opens_leaves_no_session_behind() {
    let env = Env::new();
    env.new_quest("foo");
    let mut fixture = env.fixture();
    fixture["fail_new_window"] = serde_json::json!("no space left for windows");
    env.write_fixture(fixture);

    env.cmd()
        .args(["spawn", "foo", "go", "--label", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no space left for windows"));

    // The row is inserted before the window (the `SessionStart` hook resolves
    // `$Q_SESSION` the moment Claude starts), so it has to be taken back.
    assert_eq!(env.count("SELECT count(*) FROM session"), 1);
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE role = 'worker'"),
        0
    );
    assert_eq!(env.fixture()["panes"].as_array().unwrap().len(), 1);
    let quest_id = env.json(&["show", "foo"])["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(event_kinds(&env, &quest_id), vec!["quest.created"]);

    // The next spawn works, and the failed one did not consume a number.
    let mut fixture = env.fixture();
    fixture.as_object_mut().unwrap().remove("fail_new_window");
    env.write_fixture(fixture);
    assert_eq!(
        env.json(&["spawn", "foo", "go", "--label", "tests"])["window"],
        "w1-tests"
    );
}

/// A worker row inserted but never given a pane — a `q spawn` killed between
/// the insert and `update_session_pane`. `age` seconds ago.
fn seed_pending_worker(env: &Env, quest_id: &str, label: &str, age: i64) -> String {
    let started = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - age;
    let id = format!("s-pending-{label}");
    env.conn()
        .execute(
            "INSERT INTO session (id, quest_id, role, label, tmux_session, tmux_pane,
                                  status, started_at, updated_at)
             VALUES (?1, ?2, 'worker', ?3, 'q-foo', '', 'starting', ?4, ?4)",
            rusqlite::params![&id, quest_id, label, started],
        )
        .unwrap();
    id
}

#[test]
fn the_sweep_ends_a_row_whose_window_never_opened_once_the_grace_has_passed() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();

    // Still inside the grace: the window may yet appear, so the row is left be.
    seed_pending_worker(&env, &quest_id, "fresh", 0);
    assert_eq!(env.json(&["show", "foo"])["live_sessions"], 2);

    // Past it, nothing is ever going to fill the pane in. Without this the row
    // holds its label forever: `q enter` sends you to `q resume` and `q resume`
    // sends you back.
    let stale = seed_pending_worker(&env, &quest_id, "stale", 60);
    let shown = env.json(&["show", "foo"]);
    assert_eq!(shown["live_sessions"], 2);
    let ended = shown["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == stale.as_str())
        .unwrap();
    assert_eq!(ended["status"], "ended");
    assert!(ended["ended_at"].is_i64());

    let reason: String = env
        .conn()
        .query_row(
            "SELECT json_extract(payload, '$.reason') FROM event
             WHERE kind = 'session.end' AND session_id = ?1",
            [&stale],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(reason, "never_started");

    // The label is free again, and no window was touched to free it.
    assert_eq!(env.fixture()["panes"].as_array().unwrap().len(), 1);
    assert_eq!(
        env.json(&["spawn", "foo", "go", "--label", "stale"])["session"]["label"],
        "stale"
    );
}

#[test]
fn enter_refuses_a_session_whose_window_never_opened() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    // Fresh, so the sweep leaves it live and `q enter` is the one answering.
    seed_pending_worker(&env, &quest_id, "tests", 0);

    let assert = env
        .cmd()
        .args(["enter", "foo", "--session", "tests", "--json"])
        .assert()
        .code(1)
        .stdout("");
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    let message = parsed["error"].as_str().unwrap();
    assert!(message.contains("`tests`"), "{message}");
    assert!(message.contains("has no pane yet"), "{message}");

    // An empty tmux target means "the active window" — attaching would have
    // landed on the master while claiming to be the worker.
    let fixture = env.fixture();
    assert_eq!(fixture["attached"], serde_json::Value::Null);
    assert_eq!(fixture["selected"], serde_json::Value::Null);

    // The master is still reachable.
    assert_eq!(env.json(&["enter", "foo"])["window"], "master");
}

/// The other half of `enter_refuses_a_session_whose_window_never_opened`: an
/// empty tmux target is "whatever is current", so `kill`, `peek`, `send` and
/// `reset` against a row whose window never opened would kill, page or type
/// into the terminal `q` is itself running in. Every one of them refuses.
#[test]
fn kill_peek_send_and_reset_refuse_a_session_whose_window_never_opened() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    // Fresh, so the sweep leaves it live; idle, so the send/reset gate is not
    // the thing doing the refusing.
    let pending = seed_pending_worker(&env, &quest_id, "tests", 0);
    env.set_status(&pending, "idle", None);
    let before = env.fixture();

    for args in [
        vec!["kill", "foo/tests", "-f"],
        vec!["peek", "foo/tests"],
        vec!["send", "foo/tests", "hello", "--force"],
        vec!["reset", "foo/tests"],
    ] {
        let message = env.json_err(&args)["error"].as_str().unwrap().to_string();
        assert!(
            message.contains("has no pane"),
            "`q {}` said {message:?}",
            args.join(" ")
        );
    }

    // Nothing was killed, typed into or otherwise touched — the master window
    // is exactly as it was.
    assert_eq!(env.fixture()["panes"], before["panes"]);
    // And the row is still live: refusing is not the same as ending it (the
    // sweep does that, once the grace has passed).
    assert_eq!(env.status_of(&pending), "idle");
}

/// R2-3: the confirmation names the pane it is about to kill, so for a row
/// whose window never opened it asked about "tmux window of pane )" and only
/// refused once the answer was in. The refusal belongs in front of the
/// question, the way the TUI's openers do it.
#[test]
fn kill_refuses_a_pane_less_session_before_it_asks_to_confirm() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    let pending = seed_pending_worker(&env, &quest_id, "tests", 0);
    env.set_status(&pending, "idle", None);

    // No `-f`: the confirmation is the next thing that would happen.
    let message = env.json_err(&["kill", "foo/tests"])["error"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(message.contains("has no pane"), "{message}");
    assert_eq!(env.status_of(&pending), "idle");
}

/// N-4: an argument that can never be valid is rejected before the target is
/// resolved — and therefore before the liveness sweep writes anything.
#[test]
fn peek_and_send_reject_a_bad_argument_before_they_resolve_the_target() {
    let env = Env::new();
    env.new_quest("foo");

    let lines = env.json_err(&["peek", "nope", "--lines", "0"]);
    assert_eq!(lines["error"], "--lines must be at least 1");
    let empty = env.json_err(&["send", "nope", "   "]);
    assert_eq!(empty["error"], "nothing to send");

    // The valid-argument path still resolves and still says the target is the
    // thing that is wrong.
    let missing = env.json_err(&["peek", "nope"]);
    assert!(
        missing["error"].as_str().unwrap().contains("nope"),
        "{missing}"
    );
}

#[test]
fn spawn_help_only_lists_the_implemented_flags() {
    let assert = q().args(["spawn", "--help"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for flag in ["--label", "--workflow", "--dir", "--no-attach"] {
        assert!(
            out.contains(flag),
            "`{flag}` missing from `q spawn --help`:\n{out}"
        );
    }
}

// ------------------------------- q sessions / peek / send / kill (bd-8lz.3.2)

impl Env {
    /// Forces a session row where the hooks would have put it.
    fn set_status(&self, session_id: &str, status: &str, claude_pid: Option<i64>) {
        self.conn()
            .execute(
                "UPDATE session SET status = ?1, claude_pid = COALESCE(?2, claude_pid) WHERE id = ?3",
                rusqlite::params![status, claude_pid, session_id],
            )
            .unwrap();
    }

    /// The identity a `SessionStart` hook would have recorded.
    fn set_claude_session_id(&self, session_id: &str, claude_session_id: &str) {
        self.conn()
            .execute(
                "UPDATE session SET claude_session_id = ?1 WHERE id = ?2",
                rusqlite::params![claude_session_id, session_id],
            )
            .unwrap();
    }

    fn status_of(&self, session_id: &str) -> String {
        self.conn()
            .query_row(
                "SELECT status FROM session WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    /// A row the `Notification` hook parked on a prompt.
    fn set_waiting(&self, session_id: &str, waiting_for: &str) {
        self.conn()
            .execute(
                "UPDATE session SET status = 'waiting', waiting_for = ?1 WHERE id = ?2",
                rusqlite::params![waiting_for, session_id],
            )
            .unwrap();
    }

    /// One `<pid>.json` in the stubbed Claude session registry.
    fn registry(&self, pid: i64, json: &str) {
        let dir = self.dir.path().join("registry");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(format!("{pid}.json")), json).unwrap();
    }

    /// What `send_keys` has written into a pane, per the tmux fixture.
    fn buffer(&self, pane_id: &str) -> String {
        self.fixture()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane_id"] == pane_id)
            .unwrap_or_else(|| panic!("no pane {pane_id}"))["buffer"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Every bracketed paste into a pane, in order, per the tmux fixture.
    fn pastes(&self, pane_id: &str) -> Vec<String> {
        self.fixture()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane_id"] == pane_id)
            .unwrap_or_else(|| panic!("no pane {pane_id}"))["pastes"]
            .as_array()
            .map(|a| a.iter().map(|v| v.as_str().unwrap().to_string()).collect())
            .unwrap_or_default()
    }

    /// The `{"error": ..., "code": ...}` of a command that must fail.
    fn json_err(&self, args: &[&str]) -> serde_json::Value {
        let mut cmd = self.cmd();
        let assert = cmd.args(args).arg("--json").assert().failure();
        error_json(&assert)
    }
}

/// A Quest with an idle worker: the state `q send` is designed for.
struct Fleet {
    env: Env,
    quest_id: String,
    master_id: String,
    worker_id: String,
    worker_pane: String,
}

impl Fleet {
    fn new() -> Fleet {
        let env = Env::new();
        let created = env.new_quest("alpha");
        let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
        let master_id = created["session"]["id"].as_str().unwrap().to_string();
        let worker = env.json(&["spawn", "alpha", "write the tests", "--label", "tests"]);
        let worker_id = worker["session"]["id"].as_str().unwrap().to_string();
        let worker_pane = worker["session"]["tmux_pane"].as_str().unwrap().to_string();
        // Claude came up in both windows: `SessionStart` would say idle.
        env.set_status(&master_id, "idle", Some(1001));
        env.set_status(&worker_id, "idle", Some(1002));
        Fleet {
            env,
            quest_id,
            master_id,
            worker_id,
            worker_pane,
        }
    }
}

#[test]
fn sessions_lists_the_fleet_and_one_quest() {
    let fleet = Fleet::new();
    let other = fleet.env.new_quest("beta");
    let other_master = other["session"]["id"].as_str().unwrap().to_string();

    // No target: every live session of every active Quest, master first.
    let rows = fleet.env.json(&["sessions"]);
    let names: Vec<(&str, &str)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["quest_slug"].as_str().unwrap(),
                r["label"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        names,
        [("beta", "master"), ("alpha", "master"), ("alpha", "tests"),]
    );
    // The shape carries the whole session row plus its Quest.
    let worker = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "tests")
        .unwrap();
    assert_eq!(worker["id"], fleet.worker_id.as_str());
    assert_eq!(worker["role"], "worker");
    assert_eq!(worker["status"], "idle");
    assert_eq!(worker["tmux_pane"], fleet.worker_pane.as_str());
    assert_eq!(worker["machine"], rows[0]["machine"]);
    assert!(worker["quest_slug"] == "alpha");

    // A target: only that Quest's sessions, and no QUEST column is needed.
    let alpha = fleet.env.json(&["sessions", "alpha"]);
    assert_eq!(alpha.as_array().unwrap().len(), 2);
    assert!(
        alpha
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["quest_slug"] == "alpha")
    );
    assert_eq!(other_master.len(), fleet.master_id.len());

    let text = String::from_utf8(
        fleet
            .env
            .cmd()
            .args(["sessions"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(text.starts_with("QUEST  LABEL"), "{text}");
    assert!(text.contains("LAST PROMPT"), "{text}");
    let scoped = String::from_utf8(
        fleet
            .env
            .cmd()
            .args(["sessions", "alpha"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(scoped.starts_with("LABEL"), "{scoped}");
    assert!(!scoped.contains("beta"), "{scoped}");
}

#[test]
fn sessions_hides_ended_rows_from_the_fleet_but_keeps_them_in_a_quest() {
    let fleet = Fleet::new();
    fleet.env.json(&["kill", "alpha/tests", "-f"]);

    // Fleet view: only what is running.
    let live: Vec<String> = fleet
        .env
        .json(&["sessions"])
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["label"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(live, ["master"]);

    // Quest view: the ended worker is still there, after the live rows.
    let rows = fleet.env.json(&["sessions", "alpha"]);
    let shape: Vec<(&str, &str)> = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["label"].as_str().unwrap(), r["status"].as_str().unwrap()))
        .collect();
    assert_eq!(shape, [("master", "idle"), ("tests", "ended")]);

    // `--all` brings finished Quests and their ended sessions back.
    fleet.env.json(&["close", "alpha", "-f"]);
    assert_eq!(fleet.env.json(&["sessions"]).as_array().unwrap().len(), 0);
    assert_eq!(
        fleet
            .env
            .json(&["sessions", "--all"])
            .as_array()
            .unwrap()
            .len(),
        2
    );
    fleet
        .env
        .cmd()
        .args(["sessions"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no sessions"));
}

#[test]
fn peek_returns_the_panes_capture() {
    let fleet = Fleet::new();
    for line in ["one", "two", "three"] {
        fleet.env.json(&["send", "alpha/tests", line]);
    }

    let out = fleet.env.json(&["peek", "alpha/tests"]);
    assert_eq!(out["session"], fleet.worker_id.as_str());
    assert_eq!(out["quest"], "alpha");
    assert_eq!(out["label"], "tests");
    assert_eq!(out["pane"], fleet.worker_pane.as_str());
    assert_eq!(out["lines"], 40);
    assert_eq!(out["text"], "one\ntwo\nthree");

    // `--lines` is a tail, and the human rendering is the raw capture.
    assert_eq!(
        fleet.env.json(&["peek", "alpha/tests", "--lines", "2"])["text"],
        "two\nthree"
    );
    fleet
        .env
        .cmd()
        .args(["peek", "alpha/tests"])
        .assert()
        .success()
        .stdout("one\ntwo\nthree\n");
    assert_eq!(
        fleet.env.json_err(&["peek", "alpha/tests", "--lines", "0"])["code"],
        "invalid"
    );
}

#[test]
fn peek_and_send_refuse_an_ended_session() {
    let fleet = Fleet::new();
    fleet.env.json(&["kill", "alpha/tests", "-f"]);
    for args in [
        vec!["peek", "alpha/tests"],
        vec!["send", "alpha/tests", "hello"],
    ] {
        let err = fleet.env.json_err(&args);
        assert!(
            err["error"].as_str().unwrap().contains("has ended"),
            "{err}"
        );
    }
}

#[test]
fn send_writes_into_the_pane_when_the_session_is_idle() {
    let fleet = Fleet::new();
    let out = fleet.env.json(&["send", "alpha/tests", "carry on"]);
    assert_eq!(out["session"], fleet.worker_id.as_str());
    assert_eq!(out["quest"], "alpha");
    assert_eq!(out["text"], "carry on");
    assert_eq!(out["forced"], false);
    assert_eq!(out["status"], "idle");
    // No registry entry for pid 1002: the database's `idle` stands alone.
    assert_eq!(out["registry"]["verdict"], "unknown");

    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "carry on\n");
    let payload = last_payload(&fleet.env, &fleet.quest_id, "session.send");
    assert_eq!(payload["text"], "carry on");
    assert_eq!(payload["forced"], false);
    let session_of_event: String = fleet
        .env
        .conn()
        .query_row(
            "SELECT session_id FROM event WHERE quest_id = ?1 AND kind = 'session.send'",
            [&fleet.quest_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(session_of_event, fleet.worker_id);

    fleet
        .env
        .cmd()
        .args(["send", "alpha/tests", "again"])
        .assert()
        .success()
        .stdout(predicate::str::contains("sent to alpha/tests"));
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "carry on\nagain\n");

    assert_eq!(
        fleet.env.json_err(&["send", "alpha/tests", "   "])["code"],
        "invalid"
    );
}

#[test]
fn send_is_refused_when_the_database_says_the_session_is_not_idle() {
    let fleet = Fleet::new();
    for status in ["busy", "waiting", "starting"] {
        fleet.env.set_status(&fleet.worker_id, status, None);
        let err = fleet.env.json_err(&["send", "alpha/tests", "hello"]);
        assert_eq!(err["code"], "conflict", "{err}");
        let msg = err["error"].as_str().unwrap();
        assert!(msg.contains(status), "{msg}");
        assert!(msg.contains("--force"), "{msg}");
        assert_eq!(fleet.env.buffer(&fleet.worker_pane), "");
    }
    // Nothing was sent, so nothing was logged.
    assert_eq!(
        fleet
            .env
            .count("SELECT count(*) FROM event WHERE kind = 'session.send'"),
        0
    );
}

#[test]
fn send_is_refused_when_the_registry_says_the_session_is_waiting() {
    let fleet = Fleet::new();
    // The database is stale: the `Notification` hook never fired.
    fleet.env.registry(
        1002,
        r#"{"pid":1002,"name":"alpha/tests","status":"waiting","waitingFor":"permission_prompt"}"#,
    );
    let err = fleet.env.json_err(&["send", "alpha/tests", "yes"]);
    assert_eq!(err["code"], "conflict");
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("permission_prompt"), "{msg}");
    assert!(msg.contains("--force"), "{msg}");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "");

    // A registry that says busy blocks too; one that says idle agrees.
    fleet.env.registry(1002, r#"{"pid":1002,"status":"busy"}"#);
    assert_eq!(
        fleet.env.json_err(&["send", "alpha/tests", "yes"])["code"],
        "conflict"
    );
    fleet.env.registry(1002, r#"{"pid":1002,"status":"idle"}"#);
    let out = fleet.env.json(&["send", "alpha/tests", "yes"]);
    assert_eq!(out["registry"]["verdict"], "idle");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "yes\n");

    // Unparseable is no information at all, not a refusal.
    fleet.env.registry(1002, "half-writ");
    fleet.env.json(&["send", "alpha/tests", "still fine"]);
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "yes\nstill fine\n");
}

/// A registry entry found by pid can belong to another Claude: pids are
/// recycled, and a file outlives the process it described. `q` asks about a
/// session it named itself (`claude -n <slug>/<label>`) and — once a hook has
/// run — about one exact session id; an entry matching neither is no evidence,
/// in either direction.
#[test]
fn a_registry_entry_for_another_session_is_neither_believed_nor_a_refusal() {
    let fleet = Fleet::new();
    // Someone else's session, sitting on a permission prompt. Believing it
    // would refuse a send the database has every right to allow.
    fleet.env.registry(
        1002,
        r#"{"pid":1002,"name":"beta/other","status":"waiting","waitingFor":"permission_prompt"}"#,
    );
    let out = fleet.env.json(&["send", "alpha/tests", "carry on"]);
    assert_eq!(out["registry"]["verdict"], "unknown");
    assert_eq!(out["registry"]["reason"], "entry names another session");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "carry on\n");

    // The other direction is the dangerous one: a foreign `idle` must not
    // unlock a row that no hook ever moved off `starting`.
    fleet.env.set_status(&fleet.worker_id, "starting", None);
    fleet
        .env
        .registry(1002, r#"{"pid":1002,"name":"beta/other","status":"idle"}"#);
    let err = fleet.env.json_err(&["send", "alpha/tests", "hello"]);
    assert_eq!(err["code"], "conflict");
    assert!(err["error"].as_str().unwrap().contains("starting"), "{err}");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "carry on\n");

    // With a session id on the row, the name matching is not enough: this
    // entry is a different Claude in the same pane's pid.
    fleet.env.set_claude_session_id(&fleet.worker_id, "s-mine");
    fleet.env.registry(
        1002,
        r#"{"pid":1002,"name":"alpha/tests","sessionId":"s-theirs","status":"idle"}"#,
    );
    assert_eq!(
        fleet.env.json_err(&["send", "alpha/tests", "hello"])["code"],
        "conflict"
    );
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "carry on\n");

    // The session's own entry is believed, and unlocks the `starting` row.
    fleet.env.registry(
        1002,
        r#"{"pid":1002,"name":"alpha/tests","sessionId":"s-mine","status":"idle"}"#,
    );
    let out = fleet.env.json(&["send", "alpha/tests", "hello"]);
    assert_eq!(out["registry"]["verdict"], "idle");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "carry on\nhello\n");
}

#[test]
fn force_sends_past_both_gates_and_records_that_it_did() {
    let fleet = Fleet::new();
    fleet.env.set_status(&fleet.worker_id, "busy", None);
    let out = fleet.env.json(&["send", "alpha/tests", "stop", "--force"]);
    assert_eq!(out["forced"], true);
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "stop\n");
    assert_eq!(
        last_payload(&fleet.env, &fleet.quest_id, "session.send")["forced"],
        true
    );

    fleet
        .env
        .registry(1002, r#"{"status":"waiting","waitingFor":"idle_prompt"}"#);
    fleet.env.set_status(&fleet.worker_id, "idle", None);
    fleet
        .env
        .cmd()
        .args(["send", "alpha/tests", "go", "--force"])
        .assert()
        .success()
        .stdout(predicate::str::contains("forced past"))
        .stdout(predicate::str::contains("idle_prompt"));
}

/// A newline typed into a TUI is Enter, so multi-line text has to arrive as
/// one bracketed paste instead. Verified against tmux 3.6b: `paste-buffer -p`
/// wraps the bytes in `ESC[200~` / `ESC[201~` when the pane's application
/// asked for bracketed paste, and sends them bare when it did not.
#[test]
fn multi_line_text_is_pasted_in_one_piece_rather_than_typed() {
    let fleet = Fleet::new();
    let out = fleet
        .env
        .json(&["send", "alpha/tests", "first line\nsecond line"]);
    assert_eq!(out["pasted"], true);
    assert_eq!(out["text"], "first line\nsecond line");
    assert_eq!(
        fleet.env.pastes(&fleet.worker_pane),
        ["first line\nsecond line"]
    );
    assert_eq!(
        fleet.env.buffer(&fleet.worker_pane),
        "first line\nsecond line\n"
    );
    assert_eq!(
        last_payload(&fleet.env, &fleet.quest_id, "session.send")["pasted"],
        true
    );

    // A single line stays on the plain send-keys path.
    let out = fleet.env.json(&["send", "alpha/tests", "one line"]);
    assert_eq!(out["pasted"], false);
    assert_eq!(
        fleet.env.pastes(&fleet.worker_pane),
        ["first line\nsecond line"]
    );
}

/// `\n` is not the only hazard: tmux rewrites a bare `\r` into one, and every
/// other control byte typed into a TUI is the key it stands for (ESC leaves the
/// prompt, Tab completes). Anything non-printable takes the paste path.
#[test]
fn carriage_returns_escapes_and_tabs_are_pasted_rather_than_typed() {
    let fleet = Fleet::new();
    let hazards = ["aaa\rbbb", "before\u{1b}[Aafter", "tab\there"];
    for text in hazards {
        let out = fleet.env.json(&["send", "alpha/tests", text]);
        assert_eq!(out["pasted"], true, "{text:?}");
        assert_eq!(out["text"], text);
        assert_eq!(
            last_payload(&fleet.env, &fleet.quest_id, "session.send")["pasted"],
            true
        );
    }
    assert_eq!(fleet.env.pastes(&fleet.worker_pane), hazards);

    // A trailing `\r` is a line ending, not text: it is trimmed, and what is
    // left is printable.
    let out = fleet.env.json(&["send", "alpha/tests", "plain\r"]);
    assert_eq!(out["pasted"], false);
    assert_eq!(out["text"], "plain");
}

#[test]
fn text_that_starts_with_a_dash_is_text_and_not_a_flag() {
    let fleet = Fleet::new();
    let out = fleet
        .env
        .json(&["send", "alpha/tests", "-y --force is text"]);
    assert_eq!(out["text"], "-y --force is text");
    assert_eq!(out["forced"], false);
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "-y --force is text\n");
}

/// The event log is an index, not a transcript: a long prompt is clipped the
/// same way `session.prompt` clips one.
#[test]
fn a_long_send_is_truncated_in_the_event_log_but_not_in_the_pane() {
    let fleet = Fleet::new();
    let long = "x".repeat(500);
    fleet.env.json(&["send", "alpha/tests", &long]);
    let stored = last_payload(&fleet.env, &fleet.quest_id, "session.send")["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(stored.chars().count(), 200);
    assert!(stored.ends_with('…'), "{stored}");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), format!("{long}\n"));
}

/// Without the `SessionStart` hook the row never leaves `starting`, so Claude's
/// own registry is the only evidence the session is up and between turns.
#[test]
fn a_starting_row_the_registry_calls_idle_can_be_sent_to() {
    let fleet = Fleet::new();
    fleet.env.set_status(&fleet.worker_id, "starting", None);
    // No hook ran at all: no pid on the row either, and the error says where
    // to look rather than leaving `--force` as the only way through.
    fleet
        .env
        .conn()
        .execute(
            "UPDATE session SET claude_pid = NULL WHERE id = ?1",
            [&fleet.worker_id],
        )
        .unwrap();
    let err = fleet.env.json_err(&["send", "alpha/tests", "hello"]);
    assert_eq!(err["code"], "conflict");
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("q doctor"), "{msg}");

    fleet
        .env
        .set_status(&fleet.worker_id, "starting", Some(1002));
    fleet.env.registry(1002, r#"{"pid":1002,"status":"idle"}"#);
    let out = fleet.env.json(&["send", "alpha/tests", "hello"]);
    assert_eq!(out["status"], "starting");
    assert_eq!(out["registry"]["verdict"], "idle");
    assert_eq!(fleet.env.buffer(&fleet.worker_pane), "hello\n");
}

/// The registry is the second opinion on a row the hooks left behind: the
/// listing shows it beside the row's own status rather than instead of it.
#[test]
fn sessions_shows_the_registry_when_it_contradicts_the_row() {
    let fleet = Fleet::new();
    let rows = fleet.env.json(&["sessions", "alpha"]);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["registry"].is_null())
    );

    fleet.env.registry(
        1002,
        r#"{"pid":1002,"status":"waiting","waitingFor":"permission_prompt"}"#,
    );
    let rows = fleet.env.json(&["sessions", "alpha"]);
    let worker = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["label"] == "tests")
        .unwrap();
    assert_eq!(worker["status"], "idle");
    assert_eq!(worker["registry"], "waiting: permission_prompt");
    let text = String::from_utf8(
        fleet
            .env
            .cmd()
            .args(["sessions", "alpha"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(text.contains("REG"), "{text}");
    assert!(text.contains("permission_prompt"), "{text}");

    // A registry that agrees adds no column.
    fleet.env.registry(1002, r#"{"pid":1002,"status":"idle"}"#);
    let rows = fleet.env.json(&["sessions", "alpha"]);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["registry"].is_null())
    );

    // Agreeing in different words is still agreeing: the hooks fold Claude's
    // `permission_prompt` into `permission`, the registry quotes it raw.
    fleet.env.set_waiting(&fleet.worker_id, "permission");
    fleet.env.registry(
        1002,
        r#"{"pid":1002,"status":"waiting","waitingFor":"permission_prompt"}"#,
    );
    let rows = fleet.env.json(&["sessions", "alpha"]);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["registry"].is_null()),
        "{rows}"
    );

    // An entry that belongs to another session is not a second opinion at all.
    fleet.env.set_status(&fleet.worker_id, "idle", None);
    fleet
        .env
        .registry(1002, r#"{"pid":1002,"name":"beta/other","status":"busy"}"#);
    let rows = fleet.env.json(&["sessions", "alpha"]);
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|r| r["registry"].is_null()),
        "{rows}"
    );
}

#[test]
fn kill_ends_the_worker_and_removes_its_window() {
    let fleet = Fleet::new();
    let out = fleet.env.json(&["kill", "alpha/tests", "-f"]);
    assert_eq!(out["session"]["id"], fleet.worker_id.as_str());
    assert_eq!(out["session"]["status"], "ended");
    assert_eq!(out["already_ended"], false);
    assert_eq!(out["pane_killed"], true);
    assert!(out["session"]["ended_at"].is_i64());

    assert_eq!(fleet.env.status_of(&fleet.worker_id), "ended");
    // The window is gone; the master's is not.
    let panes = fleet.env.fixture()["panes"].as_array().unwrap().clone();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0]["window_name"], "master");

    let payload = last_payload(&fleet.env, &fleet.quest_id, "session.end");
    assert_eq!(payload["reason"], "killed");
    assert_eq!(payload["pane_killed"], true);
    assert!(
        event_kinds(&fleet.env, &fleet.quest_id).contains(&"session.end".to_string()),
        "{:?}",
        event_kinds(&fleet.env, &fleet.quest_id)
    );

    // Killing again is a no-op, not a second event.
    let again = fleet.env.json(&["kill", "alpha/tests", "-f"]);
    assert_eq!(again["already_ended"], true);
    assert_eq!(
        fleet
            .env
            .count("SELECT count(*) FROM event WHERE kind = 'session.end'"),
        1
    );
}

#[test]
fn kill_refuses_the_master_even_with_force() {
    let fleet = Fleet::new();
    for args in [
        vec!["kill", "alpha/master"],
        vec!["kill", "alpha/master", "-f"],
    ] {
        let err = fleet.env.json_err(&args);
        assert_eq!(err["code"], "invalid", "{err}");
        let msg = err["error"].as_str().unwrap();
        assert!(msg.contains("q close alpha"), "{msg}");
    }
    assert_eq!(fleet.env.status_of(&fleet.master_id), "idle");
    assert_eq!(fleet.env.fixture()["panes"].as_array().unwrap().len(), 2);
}

#[test]
fn kill_asks_before_it_acts_unless_forced() {
    let fleet = Fleet::new();
    // Not a terminal, and `--json` refuses to ask at all.
    let err = fleet.env.json_err(&["kill", "alpha/tests"]);
    assert!(err["error"].as_str().unwrap().contains("-f"), "{err}");
    assert_eq!(fleet.env.status_of(&fleet.worker_id), "idle");
}

#[test]
fn a_session_target_resolves_by_id_bare_label_and_quest_fragment() {
    let fleet = Fleet::new();

    // A session id, from anywhere.
    assert_eq!(
        fleet.env.json(&["peek", &fleet.worker_id])["label"],
        "tests"
    );
    // A Quest fragment plus a label.
    assert_eq!(fleet.env.json(&["peek", "alp/tests"])["quest"], "alpha");

    // A bare label needs `$Q_QUEST` — or a fleet-wide unique match.
    let mut cmd = fleet.env.cmd();
    let assert = cmd
        .env("Q_QUEST", &fleet.quest_id)
        .args(["peek", "tests", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["session"], fleet.worker_id.as_str());
    assert_eq!(fleet.env.json(&["peek", "tests"])["quest"], "alpha");

    // An unknown label lists what the Quest does have.
    let err = fleet.env.json_err(&["peek", "alpha/nope"]);
    assert_eq!(err["code"], "not_found");
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("live: master, tests"), "{msg}");
    assert_eq!(err["code"], "not_found");
    assert_eq!(fleet.env.json_err(&["peek", "nope"])["code"], "not_found");
}

#[test]
fn an_ambiguous_bare_label_lists_the_candidates() {
    let fleet = Fleet::new();
    // A second Quest with a worker of the same label.
    fleet.env.new_quest("beta");
    fleet
        .env
        .json(&["spawn", "beta", "and here too", "--label", "tests"]);

    let err = fleet.env.json_err(&["peek", "tests"]);
    assert_eq!(err["code"], "ambiguous");
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("alpha/tests"), "{msg}");
    assert!(msg.contains("beta/tests"), "{msg}");

    // `$Q_QUEST` disambiguates, and so does an explicit Quest.
    let mut cmd = fleet.env.cmd();
    let assert = cmd
        .env("Q_QUEST", &fleet.quest_id)
        .args(["peek", "tests", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["quest"], "alpha");
    assert_eq!(fleet.env.json(&["peek", "beta/tests"])["quest"], "beta");
}

// -------------------------------------------------------------------- q beads

/// The fixture `bd` (SPEC §13): canned output per subcommand, plus the log
/// every call appends to. A subcommand with no file armed counts as "`bd` is
/// unavailable", which is how the missing-`bd` path is exercised.
impl Env {
    fn bd_log(&self) -> std::path::PathBuf {
        self.dir.path().join("bd.log")
    }

    /// One line per fixture `bd` call, in order.
    fn bd_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.bd_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Records every `bd` call this command makes.
    fn with_bd_log(&self, cmd: &mut Command) {
        cmd.env("Q_FIXTURE_BD_LOG", self.bd_log());
    }

    /// `bd create` answers with `epic`.
    fn with_bd_create(&self, cmd: &mut Command, epic: &str) {
        let path = self.dir.path().join("bd-create.json");
        std::fs::write(&path, serde_json::json!({ "id": epic }).to_string()).unwrap();
        cmd.env("Q_FIXTURE_BD_CREATE", path);
        self.with_bd_log(cmd);
    }

    /// `bd list` answers with `issues`.
    fn with_bd_list(&self, cmd: &mut Command, issues: &serde_json::Value) {
        let path = self.dir.path().join("bd-list.json");
        std::fs::write(&path, issues.to_string()).unwrap();
        cmd.env("Q_FIXTURE_BD", path);
        self.with_bd_log(cmd);
    }

    /// `bd create` is killed mid-write, so `q` has to recover the epic id
    /// from `bd list` the way a real timed-out write does.
    fn with_bd_create_timeout(&self, cmd: &mut Command) {
        cmd.env("Q_FIXTURE_BD_CREATE_TIMEOUT", "1");
        self.with_bd_log(cmd);
    }

    /// The progress cache file `q` keeps beside the database.
    fn cache_file(&self, quest_id: &str) -> std::path::PathBuf {
        self.dir
            .path()
            .join("cache")
            .join(format!("beads-{quest_id}.json"))
    }

    /// `bd update --add-label` succeeds.
    fn with_bd_relabel(&self, cmd: &mut Command) {
        let path = self.dir.path().join("bd-relabel");
        std::fs::write(&path, "ok").unwrap();
        cmd.env("Q_FIXTURE_BD_RELABEL", path);
        self.with_bd_log(cmd);
    }

    /// `bd close` succeeds.
    fn with_bd_close(&self, cmd: &mut Command) {
        let path = self.dir.path().join("bd-close");
        std::fs::write(&path, "ok").unwrap();
        cmd.env("Q_FIXTURE_BD_CLOSE", path);
        self.with_bd_log(cmd);
    }

    /// A Quest whose epic came from the fixture `bd`.
    fn quest_with_epic(&self, slug: &str, epic: &str) -> serde_json::Value {
        let work = self.work(slug);
        let mut cmd = self.cmd();
        self.with_bd_create(&mut cmd, epic);
        let assert = cmd
            .args(["new", "--name", slug, "--dir", work.to_str().unwrap()])
            .args(["--repo", "quest", "-d", "--json"])
            .assert()
            .success();
        json_of(&assert)
    }
}

/// Issues for the fixture `bd list`, all labelled for `quest_id`.
fn bd_issues(quest_id: &str, statuses: &[(&str, &str)]) -> serde_json::Value {
    statuses
        .iter()
        .map(|(id, status)| {
            serde_json::json!({
                "id": id,
                "title": *id,
                "status": status,
                "issue_type": if *id == "bd-epic" { "epic" } else { "task" },
                "labels": [format!("quest:{quest_id}"), "repo:quest"],
            })
        })
        .collect()
}

/// The argv the fixture `bd` logs for a listing of one Quest — the same one
/// `RealBd` runs, `--all -n 0` included.
fn bd_list_call(quest_id: &str) -> String {
    format!("list -l quest:{quest_id} --all -n 0 --no-pager --json")
}

#[test]
fn new_creates_the_beads_epic_and_stores_it() {
    let env = Env::new();
    let work = env.work("repo");
    let mut cmd = env.cmd();
    env.with_bd_create(&mut cmd, "bd-7fx");
    let assert = cmd
        .args(["new", "--name", "epic-quest", "--goal", "ship it"])
        .args(["--dir", work.to_str().unwrap(), "--repo", "quest"])
        .args(["-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    let id = out["quest"]["id"].as_str().unwrap().to_string();
    assert_eq!(out["quest"]["beads_epic"], "bd-7fx");
    assert_eq!(out["quest"]["beads_repo"], "quest");

    // Title and labels are exactly what SPEC §13 asks for.
    assert_eq!(
        env.bd_calls(),
        vec![format!(
            "create epic-quest: ship it --type epic -l repo:quest,quest:{id} --json"
        )]
    );
    assert!(event_kinds(&env, &id).contains(&"beads.epic".to_string()));
}

#[test]
fn new_without_a_goal_titles_the_epic_with_the_slug() {
    let env = Env::new();
    let quest = env.quest_with_epic("bare", "bd-1");
    let id = quest["quest"]["id"].as_str().unwrap();
    assert_eq!(
        env.bd_calls(),
        vec![format!(
            "create bare --type epic -l repo:quest,quest:{id} --json"
        )]
    );
}

#[test]
fn no_beads_skips_bd_entirely() {
    let env = Env::new();
    let work = env.work("repo");
    let mut cmd = env.cmd();
    env.with_bd_create(&mut cmd, "bd-7fx");
    let assert = cmd
        .args(["new", "--name", "plain", "--dir", work.to_str().unwrap()])
        .args(["--no-beads", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert!(out["quest"]["beads_epic"].is_null());
    assert!(env.bd_calls().is_empty(), "{:?}", env.bd_calls());
}

#[test]
fn a_missing_bd_warns_on_stderr_and_still_creates_the_quest() {
    let env = Env::new();
    let work = env.work("repo");
    // No `Q_FIXTURE_BD_CREATE`: the fixture `bd` is unavailable.
    let mut cmd = env.cmd();
    env.with_bd_log(&mut cmd);
    let assert = cmd
        .args(["new", "--name", "beadless", "--dir", work.to_str().unwrap()])
        .args(["-d", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("warning: no beads epic"))
        .stderr(predicate::str::contains("q set beadless beads_epic"));
    let out = json_of(&assert);
    assert!(out["quest"]["beads_epic"].is_null());
    // It really did try.
    assert_eq!(env.bd_calls().len(), 1);
}

#[test]
fn the_repo_label_falls_back_to_the_git_root_then_to_the_config() {
    let env = Env::new();
    // A git checkout: the label is its root's basename, not the cwd's.
    let root = env.work("some-repo");
    let inited = std::process::Command::new("git")
        .args(["-C", root.to_str().unwrap(), "init", "-q"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if inited {
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let mut cmd = env.cmd();
        env.with_bd_create(&mut cmd, "bd-1");
        let out = json_of(
            &cmd.args([
                "new",
                "--name",
                "from-git",
                "--dir",
                nested.to_str().unwrap(),
            ])
            .args(["-d", "--json"])
            .assert()
            .success(),
        );
        assert_eq!(out["quest"]["beads_repo"], "some-repo");
    }

    // No git root above a bare temp directory: the configured default.
    let plain = env.work("plain");
    let mut cmd = env.cmd();
    env.with_bd_create(&mut cmd, "bd-2");
    let out = json_of(
        &cmd.args([
            "new",
            "--name",
            "from-config",
            "--dir",
            plain.to_str().unwrap(),
        ])
        .args(["-d", "--json"])
        .assert()
        .success(),
    );
    let label = out["quest"]["beads_repo"].as_str().unwrap();
    // `TMPDIR` may itself sit inside a checkout, so accept either the default
    // or whatever git root that would be — never nothing.
    assert!(!label.is_empty(), "{out}");
}

#[test]
fn show_and_list_report_beads_progress() {
    let env = Env::new();
    let quest = env.quest_with_epic("tracked", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let issues = bd_issues(
        &id,
        &[
            ("bd-epic", "open"),
            ("bd-1", "closed"),
            ("bd-2", "in_progress"),
            ("bd-3", "blocked"),
            ("bd-4", "open"),
        ],
    );

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let out = json_of(&cmd.args(["show", "tracked", "--json"]).assert().success());
    // The epic does not count against itself: four issues, not five.
    assert_eq!(out["progress"]["total"], 4);
    assert_eq!(out["progress"]["closed"], 1);
    assert_eq!(out["progress"]["open"], 1);
    assert_eq!(out["progress"]["in_progress"], 1);
    assert_eq!(out["progress"]["blocked"], 1);

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let assert = cmd.args(["show", "tracked"]).assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("bd-epic (repo quest)"), "{human}");
    assert!(human.contains("1/4 closed"), "{human}");

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let assert = cmd.args(["list"]).assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("BEADS"), "{human}");
    assert!(human.contains("1/4"), "{human}");

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let out = quests_of(&cmd.args(["list", "--json"]).assert().success());
    assert_eq!(out[0]["progress"]["total"], 4);
}

#[test]
fn a_quest_without_an_epic_has_no_progress() {
    let env = Env::new();
    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &serde_json::json!([]));
    cmd.args(["new", "--name", "plain", "--no-beads", "-d"])
        .assert()
        .success();

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &bd_issues("q-other", &[("bd-1", "open")]));
    let out = json_of(&cmd.args(["show", "plain", "--json"]).assert().success());
    assert!(out["progress"].is_null(), "{out}");
    // `--no-beads` skipped the create, and a Quest with no epic is never
    // counted — so `bd` was never run at all.
    assert!(env.bd_calls().is_empty(), "{:?}", env.bd_calls());

    let assert = env.cmd().args(["show", "plain"]).assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("beads    -"), "{human}");
}

#[test]
fn progress_falls_back_to_the_cache_when_bd_stops_answering() {
    let env = Env::new();
    let quest = env.quest_with_epic("cached", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let issues = bd_issues(&id, &[("bd-1", "closed"), ("bd-2", "open")]);

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let out = json_of(&cmd.args(["show", "cached", "--json"]).assert().success());
    assert_eq!(out["progress"]["total"], 2);

    // `bd` is gone; the last reading is still what a listing shows.
    let out = quests_of(&env.cmd().args(["list", "--json"]).assert().success());
    assert_eq!(out[0]["progress"]["total"], 2);
    assert_eq!(out[0]["progress"]["closed"], 1);

    // A fresh cache means `bd` is not asked again at all.
    let before = env.bd_calls().len();
    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    cmd.args(["show", "cached", "--json"]).assert().success();
    assert_eq!(env.bd_calls().len(), before);
}

#[test]
fn close_epic_closes_the_epic_through_bd() {
    let env = Env::new();
    env.quest_with_epic("closing", "bd-epic");

    let mut cmd = env.cmd();
    env.with_bd_close(&mut cmd);
    let out = json_of(
        &cmd.args(["close", "closing", "-f", "--close-epic", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["epic_closed"], true);
    assert_eq!(out["quest"]["state"], "finished");
    assert!(
        env.bd_calls()
            .contains(&"close bd-epic --reason quest closed".to_string()),
        "{:?}",
        env.bd_calls()
    );
}

#[test]
fn close_without_close_epic_leaves_bd_alone() {
    let env = Env::new();
    env.quest_with_epic("untouched", "bd-epic");
    let mut cmd = env.cmd();
    env.with_bd_close(&mut cmd);
    let out = json_of(
        &cmd.args(["close", "untouched", "-f", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["epic_closed"], false);
    assert!(
        !env.bd_calls().iter().any(|c| c.starts_with("close")),
        "{:?}",
        env.bd_calls()
    );
}

#[test]
fn a_failing_close_epic_warns_but_still_closes_the_quest() {
    let env = Env::new();
    env.quest_with_epic("stubborn", "bd-epic");
    // No `Q_FIXTURE_BD_CLOSE`: `bd close` is unavailable.
    let mut cmd = env.cmd();
    env.with_bd_log(&mut cmd);
    let assert = cmd
        .args(["close", "stubborn", "-f", "--close-epic", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("`bd close bd-epic` failed"));
    let out = json_of(&assert);
    assert_eq!(out["epic_closed"], false);
    assert_eq!(out["quest"]["state"], "finished");
}

#[test]
fn close_epic_on_a_quest_without_one_warns() {
    let env = Env::new();
    env.cmd()
        .args(["new", "--name", "bare", "--no-beads", "-d"])
        .assert()
        .success();
    let mut cmd = env.cmd();
    env.with_bd_close(&mut cmd);
    cmd.args(["close", "bare", "-f", "--close-epic"])
        .assert()
        .success()
        .stderr(predicate::str::contains("has no beads epic"));
    assert!(env.bd_calls().is_empty(), "{:?}", env.bd_calls());
}

#[test]
fn set_links_and_unlinks_the_beads_epic() {
    let env = Env::new();
    env.cmd()
        .args(["new", "--name", "linkable", "--no-beads", "-d"])
        .assert()
        .success();

    let out = env.json(&["set", "linkable", "beads_epic", "bd-9"]);
    assert_eq!(out["quest"]["beads_epic"], "bd-9");
    assert_eq!(out["key"], "beads_epic");

    // An epic `bd` cannot be asked about is still shown, without counts.
    let assert = env.cmd().args(["show", "linkable"]).assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("bd-9 · progress unavailable"), "{human}");
    let out = env.json(&["show", "linkable"]);
    assert!(out["progress"].is_null(), "{out}");

    let out = env.json(&["set", "linkable", "beads_epic", ""]);
    assert!(out["quest"]["beads_epic"].is_null(), "{out}");

    env.cmd()
        .args(["set", "linkable", "beads_epic", "not a bd id"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid beads epic id"));
}

#[test]
fn blocked_is_derived_from_dependencies_not_from_a_status() {
    let env = Env::new();
    let quest = env.quest_with_epic("depending", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    // The real shape: every child of a live epic carries a `parent-child`
    // dependency on it, which is not being blocked. Only `blocks` is.
    let dep = |id: &str, kind: &str, on: &str| serde_json::json!({ "issue_id": id, "depends_on_id": on, "type": kind });
    let issues = serde_json::json!([
        {
            "id": "bd-epic", "status": "open", "issue_type": "epic",
            "labels": [format!("quest:{id}")],
        },
        {
            "id": "bd-1", "status": "open", "issue_type": "task",
            "labels": [format!("quest:{id}")],
            "dependencies": [dep("bd-1", "parent-child", "bd-epic")],
        },
        {
            "id": "bd-2", "status": "open", "issue_type": "task",
            "labels": [format!("quest:{id}")],
            "dependencies": [
                dep("bd-2", "parent-child", "bd-epic"),
                dep("bd-2", "blocks", "bd-1"),
            ],
        },
    ]);

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let out = json_of(&cmd.args(["show", "depending", "--json"]).assert().success());
    assert_eq!(out["progress"]["total"], 2);
    assert_eq!(out["progress"]["blocked"], 1, "{out}");
    // The overlay leaves both issues in their own status bucket.
    assert_eq!(out["progress"]["open"], 2);
}

#[test]
fn a_timed_out_create_looks_for_the_epic_before_giving_up() {
    let env = Env::new();
    let work = env.work("repo");
    let mut cmd = env.cmd();
    // The write is killed mid-flight, so `q` cannot know whether it committed.
    env.with_bd_create_timeout(&mut cmd);
    // The tracker holds an epic, but not one labelled for this Quest — so
    // there is nothing to adopt and the Quest is created without one.
    let list = env.dir.path().join("bd-list.json");
    std::fs::write(
        &list,
        serde_json::json!([
            {"id": "bd-someone-else", "status": "open", "issue_type": "epic",
             "labels": ["quest:q-other"]},
        ])
        .to_string(),
    )
    .unwrap();
    let assert = cmd
        .env("Q_FIXTURE_BD", &list)
        .args([
            "new",
            "--name",
            "recovered",
            "--dir",
            work.to_str().unwrap(),
        ])
        .args(["--repo", "quest", "-d", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("warning: no beads epic"))
        .stderr(predicate::str::contains("did not finish within"));
    let out = json_of(&assert);
    let id = out["quest"]["id"].as_str().unwrap().to_string();
    assert!(out["quest"]["beads_epic"].is_null(), "{out}");
    // It did try, and it did go looking afterwards.
    assert_eq!(
        env.bd_calls(),
        vec![
            format!("create recovered --type epic -l repo:quest,quest:{id} --json"),
            bd_list_call(&id),
        ]
    );
}

#[test]
fn the_repo_flag_is_validated() {
    let env = Env::new();
    let work = env.work("repo");
    // A comma would silently mint a second label.
    env.cmd()
        .args(["new", "--name", "evil", "--dir", work.to_str().unwrap()])
        .args(["--repo", "evil,repo:other", "-d"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid beads repo label"));
    // So would whitespace, differently.
    env.cmd()
        .args(["new", "--name", "spaced", "--dir", work.to_str().unwrap()])
        .args(["--repo", "two words", "-d"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid beads repo label"));
    // Nothing was created by either.
    let out = quests_of(&env.cmd().args(["list", "--json"]).assert().success());
    assert_eq!(out.as_array().unwrap().len(), 0, "{out}");
}

#[test]
fn repo_and_no_beads_together_are_a_usage_error() {
    let env = Env::new();
    let work = env.work("repo");
    let mut cmd = env.cmd();
    env.with_bd_create(&mut cmd, "bd-1");
    cmd.args(["new", "--name", "both", "--dir", work.to_str().unwrap()])
        .args(["--repo", "quest", "--no-beads", "-d"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--no-beads"));
    assert!(env.bd_calls().is_empty(), "{:?}", env.bd_calls());
}

#[test]
fn set_fixes_a_wrong_repo_label() {
    let env = Env::new();
    let quest = env.quest_with_epic("mislabelled", "bd-epic");
    assert_eq!(quest["quest"]["beads_repo"], "quest");

    let out = env.json(&["set", "mislabelled", "beads_repo", "other-repo"]);
    assert_eq!(out["quest"]["beads_repo"], "other-repo");
    assert_eq!(out["key"], "beads_repo");

    let assert = env.cmd().args(["show", "mislabelled"]).assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("bd-epic (repo other-repo)"), "{human}");

    // The same validation `--repo` gets.
    env.cmd()
        .args(["set", "mislabelled", "beads_repo", "a,b"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid beads repo label"));
}

#[test]
fn set_beads_repo_moves_the_label_on_the_epic_too() {
    let env = Env::new();
    env.quest_with_epic("relabelled", "bd-epic");

    let mut cmd = env.cmd();
    env.with_bd_relabel(&mut cmd);
    let out = json_of(
        &cmd.args(["set", "relabelled", "beads_repo", "other-repo", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["quest"]["beads_repo"], "other-repo");
    assert_eq!(out["epic_relabelled"], true, "{out}");
    // One write, so the epic never carries both labels or neither.
    assert!(
        env.bd_calls().contains(
            &"update bd-epic --remove-label repo:quest --add-label repo:other-repo".to_string()
        ),
        "{:?}",
        env.bd_calls()
    );
    assert!(
        event_kinds(&env, out["quest"]["id"].as_str().unwrap())
            .contains(&"beads.epic_relabelled".to_string())
    );

    // Setting the same value again repairs a label that drifted, so there is
    // nothing to remove — only the label to put back.
    let mut cmd = env.cmd();
    env.with_bd_relabel(&mut cmd);
    cmd.args(["set", "relabelled", "beads_repo", "other-repo", "--json"])
        .assert()
        .success();
    assert!(
        env.bd_calls()
            .contains(&"update bd-epic --add-label repo:other-repo".to_string()),
        "{:?}",
        env.bd_calls()
    );
}

#[test]
fn a_bd_that_will_not_relabel_names_the_command_to_run_by_hand() {
    let env = Env::new();
    env.quest_with_epic("stuck-label", "bd-epic");
    // No `Q_FIXTURE_BD_RELABEL`: `bd update` is unavailable. `q set` still
    // stores the column — the write already happened.
    let mut cmd = env.cmd();
    env.with_bd_log(&mut cmd);
    let assert = cmd
        .args(["set", "stuck-label", "beads_repo", "other-repo", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "bd update bd-epic --remove-label repo:quest --add-label repo:other-repo",
        ));
    let out = json_of(&assert);
    assert_eq!(out["quest"]["beads_repo"], "other-repo");
    assert_eq!(out["epic_relabelled"], false, "{out}");
}

#[test]
fn clearing_beads_repo_says_the_epic_keeps_its_label() {
    let env = Env::new();
    env.quest_with_epic("cleared", "bd-epic");
    let mut cmd = env.cmd();
    env.with_bd_relabel(&mut cmd);
    let out = json_of(
        &cmd.args(["set", "cleared", "beads_repo", "", "--json"])
            .assert()
            .success()
            .stderr(predicate::str::contains(
                "epic bd-epic still carries repo:quest",
            )),
    );
    assert!(out["quest"]["beads_repo"].is_null(), "{out}");
    // Nothing was written to the tracker: q does not strip a label nobody
    // asked it to strip.
    assert!(
        !env.bd_calls().iter().any(|c| c.starts_with("update")),
        "{:?}",
        env.bd_calls()
    );
}

#[test]
fn rm_names_the_epic_it_orphans() {
    let env = Env::new();
    env.quest_with_epic("throwaway", "bd-epic");
    let assert = env.cmd().args(["rm", "throwaway", "-f"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("bd-epic is left open"), "{out}");
    assert!(out.contains("bd close bd-epic"), "{out}");
    // And in the payload, for anything reading it.
    env.quest_with_epic("throwaway2", "bd-e2");
    let out = env.json(&["rm", "throwaway2", "-f"]);
    assert_eq!(out["orphaned_epic"], "bd-e2");
    // A Quest with no epic has none to orphan.
    env.cmd()
        .args(["new", "--name", "beadless", "--no-beads", "-d"])
        .assert()
        .success();
    let out = env.json(&["rm", "beadless", "-f"]);
    assert!(out["orphaned_epic"].is_null(), "{out}");
}

#[test]
fn a_second_close_epic_neither_writes_nor_logs_twice() {
    let env = Env::new();
    let quest = env.quest_with_epic("twice", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();

    let mut cmd = env.cmd();
    env.with_bd_close(&mut cmd);
    let assert = cmd
        .args(["close", "twice", "-f", "--close-epic"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("epic bd-epic closed"), "{out}");

    // Again: the Quest is finished and the epic is a row in a shared tracker,
    // so there is nothing left to do.
    let mut cmd = env.cmd();
    env.with_bd_close(&mut cmd);
    let assert = cmd
        .args(["close", "twice", "-f", "--close-epic", "--json"])
        .assert()
        .success()
        .stderr(predicate::str::contains("was already closed"));
    assert_eq!(json_of(&assert)["epic_closed"], false);
    assert_eq!(
        env.bd_calls()
            .iter()
            .filter(|c| c.starts_with("close bd-epic"))
            .count(),
        1,
        "{:?}",
        env.bd_calls()
    );
    assert_eq!(
        event_kinds(&env, &id)
            .iter()
            .filter(|k| *k == "beads.epic_closed")
            .count(),
        1
    );
}

#[test]
fn close_epic_on_an_already_finished_quest_says_what_it_did() {
    let env = Env::new();
    env.quest_with_epic("late", "bd-epic");
    env.cmd().args(["close", "late", "-f"]).assert().success();

    let mut cmd = env.cmd();
    env.with_bd_close(&mut cmd);
    let assert = cmd
        .args(["close", "late", "-f", "--close-epic"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("is already finished"), "{out}");
    assert!(out.contains("epic bd-epic closed"), "{out}");
}

#[test]
fn a_quiet_listing_does_not_ask_bd_anything() {
    let env = Env::new();
    let quest = env.quest_with_epic("silent", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let before = env.bd_calls().len();
    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &bd_issues(&id, &[("bd-1", "open")]));
    let assert = cmd.args(["--quiet", "list"]).assert().success();
    assert!(assert.get_output().stdout.is_empty());
    // Nothing is printed, so nothing is worth a `bd` call.
    assert_eq!(env.bd_calls().len(), before, "{:?}", env.bd_calls());
}

#[test]
fn the_brief_and_show_agree_on_the_progress() {
    let env = Env::new();
    let quest = env.quest_with_epic("agreeing", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    // The epic is in the payload, and so is another Quest's issue.
    let mut issues = bd_issues(
        &id,
        &[
            ("bd-epic", "open"),
            ("bd-1", "closed"),
            ("bd-2", "open"),
            ("bd-3", "in_progress"),
        ],
    );
    issues.as_array_mut().unwrap().push(serde_json::json!({
        "id": "bd-9", "title": "not ours", "status": "open",
        "issue_type": "task", "labels": ["quest:q-somebody-else", "repo:quest"],
    }));

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let assert = cmd.args(["show", "agreeing"]).assert().success();
    let shown = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let summary = shown
        .lines()
        .find(|l| l.contains("bd-epic (repo"))
        .and_then(|l| l.split_once(" · "))
        .map(|(_, rest)| rest.trim().to_string())
        .expect(&shown);
    assert_eq!(summary, "1/3 closed · 1 open · 1 in progress");

    // The brief renders the same numbers from the same call — it used to
    // count the epic as open work and tally the payload its own way.
    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &issues);
    let assert = cmd.args(["brief", "agreeing"]).assert().success();
    let brief = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        brief.contains(&format!("- **progress**: {summary}")),
        "the brief must print `q show`'s summary, got:\n{brief}"
    );
    // The epic is named as the epic, never listed as work; nor is anyone
    // else's issue.
    assert!(!brief.contains("[open] `bd-epic`"), "{brief}");
    assert!(!brief.contains("bd-9"), "{brief}");
    assert!(brief.contains("[open] `bd-2`"), "{brief}");
    assert!(brief.contains("[in_progress] `bd-3`"), "{brief}");
    assert!(!brief.contains("[closed] `bd-1`"), "{brief}");
}

#[test]
fn rm_deletes_the_progress_cache_with_the_quest() {
    let env = Env::new();
    let quest = env.quest_with_epic("disposable", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();

    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &bd_issues(&id, &[("bd-1", "closed")]));
    cmd.args(["show", "disposable", "--json"])
        .assert()
        .success();
    assert!(env.cache_file(&id).exists(), "nothing was cached");

    env.cmd()
        .args(["rm", "disposable", "-f"])
        .assert()
        .success();
    assert!(
        !env.cache_file(&id).exists(),
        "the cache outlived the quest it belonged to"
    );
}

#[test]
fn the_brief_asks_bd_the_same_question_the_listing_does() {
    let env = Env::new();
    let quest = env.quest_with_epic("consistent", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &bd_issues(&id, &[("bd-1", "open")]));
    cmd.args(["brief", "consistent"]).assert().success();
    // `--all -n 0`: a brief truncated at bd's default 50 rows, or one hiding
    // closed issues, would contradict `q show`.
    assert!(
        env.bd_calls().contains(&bd_list_call(&id)),
        "{:?}",
        env.bd_calls()
    );
}

#[test]
fn the_brief_names_the_epic_and_the_labels_agents_must_use() {
    let env = Env::new();
    let quest = env.quest_with_epic("briefed", "bd-epic");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let mut cmd = env.cmd();
    env.with_bd_list(&mut cmd, &bd_issues(&id, &[("bd-1", "open")]));
    let assert = cmd.args(["brief", "briefed"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        out.contains(&format!("-l repo:quest,quest:{id}")),
        "the brief must spell out the labels:\n{out}"
    );
    assert!(out.contains("**epic**: `bd-epic`"), "{out}");
    assert!(out.contains("[open] `bd-1`"), "{out}");
}

// -------------------------------------------------- q reset (bd-8lz.3.3)

impl Env {
    fn set_ctx(&self, session_id: &str, pct: i64) {
        self.conn()
            .execute(
                "UPDATE session SET ctx_pct = ?1, ctx_updated_at = 2 WHERE id = ?2",
                rusqlite::params![pct, session_id],
            )
            .unwrap();
    }

    /// The payload of the newest event of `kind`, for a Quest.
    fn last_event(&self, quest_id: &str, kind: &str) -> serde_json::Value {
        self.conn()
            .query_row(
                "SELECT payload FROM event WHERE quest_id = ?1 AND kind = ?2 \
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![quest_id, kind],
                |r| {
                    let raw: Option<String> = r.get(0)?;
                    Ok(raw.map_or(serde_json::Value::Null, |p| {
                        serde_json::from_str(&p).unwrap()
                    }))
                },
            )
            .unwrap_or_else(|e| panic!("no `{kind}` event for {quest_id}: {e}"))
    }
}

/// The follow-up `q reset` types after `/clear`; mirrors the const in
/// `src/commands/reset.rs`, which is the authority.
const FOLLOW_UP: &str = "Nastavi rad na questu prema briefu.";

#[test]
fn reset_clears_and_then_types_the_follow_up() {
    let fleet = Fleet::new();
    fleet.env.set_ctx(&fleet.master_id, 61);
    let master_pane = fleet.env.json(&["peek", "alpha/master"])["pane"]
        .as_str()
        .unwrap()
        .to_string();

    let out = fleet.env.json(&["reset", "alpha/master", "--delay", "0"]);
    assert_eq!(out["action"], "reset");
    assert_eq!(out["strategy"], "clear");
    assert_eq!(out["ctx_pct"], 61);
    assert_eq!(out["session"], fleet.master_id.as_str());

    // Both keystrokes landed, in order, and each with its Enter.
    let buffer = fleet.env.buffer(&master_pane);
    let clear = buffer.find("/clear").expect(&buffer);
    let follow = buffer.find(FOLLOW_UP).expect(&buffer);
    assert!(clear < follow, "{buffer}");

    let payload = fleet.env.last_event(&fleet.quest_id, "session.reset");
    assert_eq!(payload["strategy"], "clear");
    assert_eq!(payload["keys"], "/clear");
    assert_eq!(payload["ctx_pct"], 61);
    assert_eq!(payload["scheduled"], true);
    assert_eq!(payload["follow_up"], FOLLOW_UP);
    // No real Claude under the fixture, so no `session.brief_injected` ever
    // arrives and the wait is bounded to nothing.
    assert_eq!(payload["brief_injected"], false);
}

#[test]
fn reset_compact_sends_the_quest_goal_as_the_focus() {
    let env = Env::new();
    let work = env.work("repo");
    let created = env.json(&[
        "new",
        "--name",
        "alpha",
        "--goal",
        "make the  backfill\nidempotent",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    let master_id = created["session"]["id"].as_str().unwrap().to_string();
    let pane = created["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();
    env.set_status(&master_id, "idle", Some(1001));

    let out = env.json(&["reset", "alpha/master", "--strategy", "compact"]);
    assert_eq!(out["strategy"], "compact");
    // `/compact` also leaves an empty window behind, so it gets the same
    // follow-up as `/clear` — otherwise the master idles there forever.
    let buffer = env.buffer(&pane);
    let compact = buffer
        .find("/compact make the backfill idempotent")
        .expect(&buffer);
    let follow = buffer.find(FOLLOW_UP).expect(&buffer);
    assert!(compact < follow, "{buffer}");

    let payload = env.last_event(&quest_id, "session.reset");
    assert_eq!(payload["focus"], "make the backfill idempotent");
    assert_eq!(payload["keys"], "/compact make the backfill idempotent");
    assert_eq!(payload["follow_up"], FOLLOW_UP);
    // No `--delay`: a manual reset.
    assert_eq!(payload["scheduled"], false);
}

#[test]
fn reset_without_a_goal_sends_a_bare_compact() {
    let fleet = Fleet::new();
    let pane = fleet.env.json(&["peek", "alpha/master"])["pane"]
        .as_str()
        .unwrap()
        .to_string();
    fleet
        .env
        .json(&["reset", "alpha/master", "--strategy", "compact"]);
    let buffer = fleet.env.buffer(&pane);
    assert!(buffer.starts_with("/compact\n"), "{buffer}");
    assert!(buffer.contains(FOLLOW_UP), "{buffer}");
    assert_eq!(
        fleet.env.last_event(&fleet.quest_id, "session.reset")["focus"],
        serde_json::Value::Null
    );
}

#[test]
fn the_default_strategy_comes_from_the_config() {
    let fleet = Fleet::new();
    let pane = fleet.env.json(&["peek", "alpha/master"])["pane"]
        .as_str()
        .unwrap()
        .to_string();
    fleet
        .env
        .cmd()
        .args(["config", "set", "context.reset_strategy", "compact"])
        .assert()
        .success();
    assert_eq!(
        fleet.env.json(&["reset", "alpha/master", "--delay", "0"])["strategy"],
        "compact"
    );
    assert!(fleet.env.buffer(&pane).contains("/compact"));
}

#[test]
fn a_scheduled_reset_of_a_busy_session_is_a_skip_and_a_manual_one_is_an_error() {
    let fleet = Fleet::new();
    let pane = fleet.env.json(&["peek", "alpha/master"])["pane"]
        .as_str()
        .unwrap()
        .to_string();

    for status in ["busy", "waiting", "starting"] {
        fleet.env.set_status(&fleet.master_id, status, None);

        // The scheduled path: nobody reads its exit code, so a skip is a
        // success that leaves a trail.
        let out = fleet.env.json(&["reset", "alpha/master", "--delay", "0"]);
        assert_eq!(out["action"], "skipped", "{status}");
        let reason = out["reason"].as_str().unwrap();
        assert!(reason.contains(status), "{reason}");
        let payload = fleet
            .env
            .last_event(&fleet.quest_id, "session.reset_skipped");
        assert_eq!(payload["status"], status);
        assert_eq!(payload["scheduled"], true);

        // Typed by hand, the same refusal is an error.
        let err = fleet.env.json_err(&["reset", "alpha/master"]);
        assert_eq!(err["code"], "conflict", "{status}");
        assert!(err["error"].as_str().unwrap().contains(status), "{err}");
    }
    // Nothing was ever typed into the pane.
    assert_eq!(fleet.env.buffer(&pane), "");
}

#[test]
fn a_reset_is_skipped_when_the_registry_says_the_session_is_waiting() {
    let fleet = Fleet::new();
    let pane = fleet.env.json(&["peek", "alpha/master"])["pane"]
        .as_str()
        .unwrap()
        .to_string();
    fleet.env.registry(
        1001,
        r#"{"pid":1001,"status":"waiting","waitingFor":"permission_prompt"}"#,
    );
    let out = fleet.env.json(&["reset", "alpha/master", "--delay", "0"]);
    assert_eq!(out["action"], "skipped");
    assert!(
        out["reason"]
            .as_str()
            .unwrap()
            .contains("permission_prompt"),
        "{out}"
    );
    assert_eq!(fleet.env.buffer(&pane), "");

    // A registry that agrees the session is idle lets it through.
    fleet.env.registry(1001, r#"{"pid":1001,"status":"idle"}"#);
    assert_eq!(
        fleet.env.json(&["reset", "alpha/master", "--delay", "0"])["action"],
        "reset"
    );
}

#[test]
fn a_reset_of_an_ended_session_goes_through_the_same_gate() {
    let fleet = Fleet::new();
    fleet.env.json(&["kill", "alpha/tests", "-f"]);
    // A session that ended between scheduling and waking is a skip, not a
    // failure the detached process could do anything about.
    let out = fleet.env.json(&["reset", "alpha/tests", "--delay", "0"]);
    assert_eq!(out["action"], "skipped");
    assert!(out["reason"].as_str().unwrap().contains("ended"), "{out}");
    // By hand it is an error, like every other non-idle state.
    let err = fleet.env.json_err(&["reset", "alpha/tests"]);
    assert_eq!(err["code"], "conflict");
    assert!(err["error"].as_str().unwrap().contains("ended"), "{err}");
}

#[test]
fn reset_help_lists_its_flags() {
    let assert = q().args(["reset", "--help"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for flag in ["--delay", "--strategy"] {
        assert!(out.contains(flag), "`{flag}` missing:\n{out}");
    }
}

#[test]
fn new_can_opt_a_quest_out_of_auto_reset() {
    let env = Env::new();
    let work = env.work("repo");
    let out = env.json(&[
        "new",
        "--name",
        "alpha",
        "--no-auto-reset",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);
    assert_eq!(out["quest"]["auto_reset"], false);
    // Without the flag the column stays NULL and follows the config.
    let plain = env.new_quest("beta");
    assert_eq!(plain["quest"]["auto_reset"], serde_json::Value::Null);
}

impl Env {
    /// `q reset` in its own process, with a generous poll budget, so a marker
    /// event can be seeded while it waits.
    fn reset_in_background(
        &self,
        session_id: &str,
        extra: &[&str],
    ) -> std::thread::JoinHandle<std::process::Output> {
        let mut args: Vec<String> = ["reset", session_id, "--delay", "0", "--json"]
            .iter()
            .map(|a| a.to_string())
            .collect();
        args.extend(extra.iter().map(|a| a.to_string()));
        let db = self.dir.path().join("q.db");
        let config = self.dir.path().join("config.toml");
        let fixture = self.dir.path().join("tmux.json");
        let registry = self.dir.path().join("registry");
        std::thread::spawn(move || {
            std::process::Command::new(assert_cmd::cargo::cargo_bin("q"))
                .args(&args)
                .env("Q_DB", db)
                .env("Q_CONFIG", config)
                .env("Q_FIXTURE", fixture)
                .env("Q_CLAUDE_SESSIONS_DIR", registry)
                .env("Q_RESET_ITERATIONS", "40")
                .env_remove("Q_QUEST")
                .output()
                .unwrap()
        })
    }
}

/// `session.brief_injected` — not `session.start` — is the signal that the
/// fresh brief is on its way back to Claude: the `SessionStart` hook writes
/// `session.start` before rendering it and the marker after.
#[test]
fn a_reset_waits_for_the_brief_marker_before_the_follow_up() {
    let fleet = Fleet::new();
    let quest_id = fleet.quest_id.clone();
    let pane = fleet.env.json(&["peek", "alpha/master"])["pane"]
        .as_str()
        .unwrap()
        .to_string();
    let reset = fleet.env.reset_in_background(&fleet.master_id, &[]);

    // The order a real hook writes in: the start event lands early, the brief
    // marker only once `brief::render` has returned.
    std::thread::sleep(std::time::Duration::from_millis(400));
    fleet.env.seed_event(
        &quest_id,
        Some(&fleet.master_id),
        "session.start",
        r#"{"source":"clear"}"#,
    );
    // While only `session.start` is in, the follow-up must not have been typed.
    assert!(!fleet.env.buffer(&pane).contains(FOLLOW_UP));
    fleet.env.seed_event(
        &quest_id,
        Some(&fleet.master_id),
        "session.brief_injected",
        r#"{"source":"clear","brief":true}"#,
    );

    let out = reset.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {out:?}"));
    assert_eq!(parsed["action"], "reset");
    assert_eq!(parsed["detail"]["brief_injected"], true);
    assert_eq!(
        fleet.env.last_event(&quest_id, "session.reset")["brief_injected"],
        true
    );
    assert!(fleet.env.buffer(&pane).contains(FOLLOW_UP));
}

/// The marker of the window that was just thrown away must not be mistaken
/// for the new one's: only events appended after the keystroke count.
#[test]
fn a_brief_marker_from_before_the_reset_does_not_confirm_it() {
    let fleet = Fleet::new();
    fleet.env.seed_event(
        &fleet.quest_id,
        Some(&fleet.master_id),
        "session.start",
        r#"{"source":"clear"}"#,
    );
    fleet.env.seed_event(
        &fleet.quest_id,
        Some(&fleet.master_id),
        "session.brief_injected",
        r#"{"source":"clear","brief":true}"#,
    );
    let out = fleet.env.json(&["reset", "alpha/master", "--delay", "0"]);
    assert_eq!(out["action"], "reset");
    assert_eq!(out["detail"]["brief_injected"], false);
}

#[test]
fn a_compact_waits_for_its_own_source() {
    let fleet = Fleet::new();
    let quest_id = fleet.quest_id.clone();
    let reset = fleet
        .env
        .reset_in_background(&fleet.master_id, &["--strategy", "compact"]);

    std::thread::sleep(std::time::Duration::from_millis(400));
    // A `clear` marker is the wrong window: `/compact` fires
    // `SessionStart(source=compact)`.
    fleet.env.seed_event(
        &quest_id,
        Some(&fleet.master_id),
        "session.brief_injected",
        r#"{"source":"clear","brief":true}"#,
    );
    fleet.env.seed_event(
        &quest_id,
        Some(&fleet.master_id),
        "session.brief_injected",
        r#"{"source":"compact","brief":true}"#,
    );

    let out = reset.join().unwrap();
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["strategy"], "compact");
    assert_eq!(parsed["detail"]["brief_injected"], true);
}

#[test]
fn a_scheduled_reset_that_fails_leaves_an_event_and_exits_zero() {
    let fleet = Fleet::new();
    // A tmux fixture q cannot read. The failure lands after the target has
    // resolved, so there is a session to record it against.
    std::fs::write(fleet.env.dir.path().join("tmux.json"), "not json").unwrap();

    fleet
        .env
        .cmd()
        .args(["reset", "alpha/master", "--delay", "0", "--json"])
        .assert()
        .success()
        .stdout("");
    let payload = fleet
        .env
        .last_event(&fleet.quest_id, "session.reset_failed");
    assert_eq!(payload["stage"], "run");
    assert_eq!(payload["scheduled"], true);
    assert!(!payload["error"].as_str().unwrap().is_empty(), "{payload}");

    // Typed by hand the same failure is an error, with a message.
    fleet
        .env
        .cmd()
        .args(["reset", "alpha/master"])
        .assert()
        .failure()
        .stderr(predicate::str::is_empty().not());
}

// ------------------------------------------------- q name (bd-8lz.3.5, SPEC §10)

impl Env {
    /// A file with a canned `claude -p` answer, wired in through
    /// `$Q_FIXTURE_CLAUDE_NAME`. No file at all = `claude` unavailable.
    fn canned_name(&self, answer: &str) -> std::path::PathBuf {
        let path = self.dir.path().join("claude-name");
        std::fs::write(&path, answer).unwrap();
        path
    }

    fn name_json(&self, canned: Option<&std::path::Path>, args: &[&str]) -> serde_json::Value {
        let mut cmd = self.cmd();
        if let Some(path) = canned {
            cmd.env("Q_FIXTURE_CLAUDE_NAME", path);
        }
        let assert = cmd.args(args).arg("--json").assert().success();
        json_of(&assert)
    }

    fn cached_names(&self) -> Vec<(String, String)> {
        self.conn()
            .prepare("SELECT slug, source FROM name_cache ORDER BY input_hash")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    fn quest_row(&self, slug: &str) -> serde_json::Value {
        self.conn()
            .query_row(
                "SELECT slug, name_source, name_input_hash FROM quest WHERE slug = ?1",
                [slug],
                |r| {
                    Ok(serde_json::json!({
                        "slug": r.get::<_, String>(0)?,
                        "name_source": r.get::<_, String>(1)?,
                        "name_input_hash": r.get::<_, Option<String>>(2)?,
                    }))
                },
            )
            .unwrap()
    }

    fn pending_rename(&self, session_id: &str) -> Option<String> {
        self.conn()
            .query_row(
                "SELECT pending_rename FROM session WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap()
    }
}

/// A Quest with a prompt, so the heuristic has something to fall back on.
fn quest_with_prompt(env: &Env, slug: &str) -> serde_json::Value {
    let work = env.work(slug);
    let assert = env
        .cmd()
        .args(["new", "--name", slug, "--dir", work.to_str().unwrap()])
        .args(["--prompt", "retry the cdc backfill", "-d", "--json"])
        .assert()
        .success();
    json_of(&assert)
}

#[test]
fn name_auto_proposes_a_valid_model_answer_and_caches_it() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    let canned = env.canned_name("cdc-backfill\n");

    let out = env.name_json(Some(&canned), &["name", "foo", "--auto"]);
    assert_eq!(out["proposal"]["slug"], "cdc-backfill");
    assert_eq!(out["proposal"]["source"], "claude");
    assert_eq!(out["proposal"]["cached"], false);
    assert_eq!(out["applied"], false);
    assert_eq!(out["current"], "foo");
    // A proposal alone never renames.
    assert_eq!(
        env.quest_row("foo")["name_input_hash"],
        serde_json::Value::Null
    );
    assert_eq!(
        env.cached_names(),
        [("cdc-backfill".to_string(), "claude".to_string())]
    );

    // The same input answers from the cache, without the model.
    let again = env.name_json(None, &["name", "foo", "--auto"]);
    assert_eq!(again["proposal"]["slug"], "cdc-backfill");
    assert_eq!(again["proposal"]["cached"], true);
    assert_eq!(again["proposal"]["source"], "claude");

    // `--refresh` asks again and overwrites the cache.
    let fresh = env.canned_name("cdc-restore");
    let refreshed = env.name_json(Some(&fresh), &["name", "foo", "--auto", "--refresh"]);
    assert_eq!(refreshed["proposal"]["slug"], "cdc-restore");
    assert_eq!(refreshed["proposal"]["cached"], false);
    assert_eq!(
        env.cached_names(),
        [("cdc-restore".to_string(), "claude".to_string())]
    );
}

#[test]
fn name_auto_falls_back_to_the_heuristic_without_caching_it() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");

    // An answer that is not a slug.
    let canned = env.canned_name("Sure! How about `cdc backfill`?");
    let out = env.name_json(Some(&canned), &["name", "foo", "--auto"]);
    assert_eq!(out["proposal"]["slug"], "retry-the-cdc-backfill");
    assert_eq!(out["proposal"]["source"], "heuristic");
    assert!(
        env.cached_names().is_empty(),
        "a rejected answer was cached"
    );

    // No `claude` at all is the same story.
    let out = env.name_json(None, &["name", "foo", "--auto"]);
    assert_eq!(out["proposal"]["source"], "heuristic");
    assert!(env.cached_names().is_empty());
}

#[test]
fn name_auto_prints_the_heuristic_marker_in_human_output() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    env.cmd()
        .args(["name", "foo", "--auto"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "retry-the-cdc-backfill (heuristic)",
        ));
}

#[test]
fn the_heuristic_prefers_the_git_branch_of_the_quests_directory() {
    let env = Env::new();
    let work = env.work("repo");
    for args in [
        vec!["init", "-q", "-b", "feat/cdc-backfill"],
        vec!["config", "user.email", "q@example.com"],
        vec!["config", "user.name", "q"],
        // `rev-parse --abbrev-ref HEAD` needs a HEAD to speak of.
        vec!["commit", "-q", "--allow-empty", "-m", "root"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&work)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    env.cmd()
        .args(["new", "--name", "foo", "--dir", work.to_str().unwrap()])
        .args(["--prompt", "retry the cdc backfill", "-d"])
        .assert()
        .success();

    let out = env.name_json(None, &["name", "foo", "--auto"]);
    assert_eq!(out["proposal"]["slug"], "feat-cdc-backfill");
    assert_eq!(out["proposal"]["source"], "heuristic");
}

#[test]
fn name_apply_renames_the_quest_and_tells_every_idle_claude_session() {
    let fleet = Fleet::new();
    let env = &fleet.env;
    fleet.env.registry(1001, r#"{"status":"idle"}"#);
    fleet.env.registry(1002, r#"{"status":"idle"}"#);
    let master_pane: String = env
        .conn()
        .query_row(
            "SELECT tmux_pane FROM session WHERE id = ?1",
            [&fleet.master_id],
            |r| r.get(0),
        )
        .unwrap();
    let canned = env.canned_name("cdc-backfill");

    let out = env.name_json(
        Some(&canned),
        &["name", "alpha", "--auto", "--apply", "--force"],
    );
    assert_eq!(out["applied"], true);
    assert_eq!(out["current"], "cdc-backfill");
    assert_eq!(out["renamed"]["changed"], true);
    assert_eq!(out["renamed"]["from"], "alpha");
    assert_eq!(out["renamed"]["tmux_session"], "q-cdc-backfill");
    assert_eq!(out["quest"]["name_source"], "auto");
    assert_eq!(
        out["quest"]["name_input_hash"],
        out["proposal"]["input_hash"]
    );

    // The tmux session followed, and so did the session rows.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-cdc-backfill'"),
        2
    );
    assert!(
        env.fixture()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| { p["session_name"] == "q-cdc-backfill" })
    );
    assert!(event_kinds(env, &fleet.quest_id).contains(&"name.changed".to_string()));

    // Both live sessions were told their new Claude name.
    // Sorted: the order sessions come back in is the database's business.
    let mut told: Vec<&str> = out["renamed"]["told"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    told.sort_unstable();
    assert_eq!(told, ["master", "tests"]);
    assert!(out["renamed"]["pending"].as_array().unwrap().is_empty());
    assert_eq!(env.buffer(&master_pane), "/rename cdc-backfill/master\n");
    assert_eq!(
        env.buffer(&fleet.worker_pane),
        "/rename cdc-backfill/tests\n"
    );
    assert_eq!(env.pending_rename(&fleet.master_id), None);
    assert_eq!(env.pending_rename(&fleet.worker_id), None);
}

#[test]
fn name_apply_holds_the_rename_of_a_session_that_is_not_idle() {
    let fleet = Fleet::new();
    let env = &fleet.env;
    env.set_status(&fleet.worker_id, "busy", None);
    let canned = env.canned_name("cdc-backfill");

    let out = env.name_json(
        Some(&canned),
        &["name", "alpha", "--auto", "--apply", "--force"],
    );
    assert_eq!(out["current"], "cdc-backfill");
    assert_eq!(out["renamed"]["told"], serde_json::json!(["master"]));
    assert_eq!(out["renamed"]["pending"], serde_json::json!(["tests"]));
    assert_eq!(env.buffer(&fleet.worker_pane), "");
    assert_eq!(
        env.pending_rename(&fleet.worker_id).as_deref(),
        Some("cdc-backfill/tests")
    );
    assert_eq!(env.pending_rename(&fleet.master_id), None);
    // The held send shows up in the human line too.
    env.cmd()
        .args(["rename", "cdc-backfill", "back-again"])
        .assert()
        .success()
        .stdout(predicate::str::contains("/rename held for tests"));
}

#[test]
fn name_apply_steps_aside_when_the_proposal_is_another_quests_slug() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    env.new_quest("cdc-backfill");
    let canned = env.canned_name("cdc-backfill");

    let out = env.name_json(
        Some(&canned),
        &["name", "foo", "--auto", "--apply", "--force"],
    );
    assert_eq!(out["current"], "cdc-backfill-2");
    assert_eq!(out["proposal"]["slug"], "cdc-backfill");
}

#[test]
fn name_apply_steps_aside_when_a_tmux_session_already_holds_the_proposal() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    // A tmux session with no Quest row behind it — a leftover from a Quest that
    // was removed. `rename::apply` refuses to move onto one, so the proposal
    // has to step aside before it gets there.
    let mut fixture = env.fixture();
    fixture["panes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "pane_id": "%99",
            "session_name": "q-cdc-backfill",
            "window_name": "master",
        }));
    env.write_fixture(fixture);
    let canned = env.canned_name("cdc-backfill");

    let out = env.name_json(
        Some(&canned),
        &["name", "foo", "--auto", "--apply", "--force"],
    );
    assert_eq!(out["proposal"]["slug"], "cdc-backfill");
    assert_eq!(out["current"], "cdc-backfill-2");
}

/// A rename that failed on the machine (tmux went away) is logged — the caller
/// is normally a detached child whose stderr goes to `/dev/null` (round-1
/// review, medium #6) — but the hash is *not* stamped: the answer is already in
/// the cache, so the retry is free, and stamping would make a transient failure
/// permanent (round-2 review, low #3).
#[test]
fn a_failed_apply_logs_it_and_leaves_the_hash_unstamped_for_a_retry() {
    let env = Env::new();
    let quest = quest_with_prompt(&env, "foo");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let mut fixture = env.fixture();
    fixture["fail_rename_session"] = serde_json::json!("tmux server went away");
    env.write_fixture(fixture);
    let canned = env.canned_name("cdc-backfill");

    let assert = env
        .cmd()
        .env("Q_FIXTURE_CLAUDE_NAME", &canned)
        .args(["name", "foo", "--auto", "--apply", "--force", "--json"])
        .assert()
        .failure();
    assert!(
        error_json(&assert)["error"]
            .as_str()
            .unwrap()
            .contains("tmux server went away")
    );

    let stored = env.quest_row("foo");
    assert_eq!(stored["slug"], "foo");
    assert!(stored["name_input_hash"].is_null(), "{stored}");
    assert!(event_kinds(&env, &id).contains(&"name.failed".to_string()));
    // The answer stays in the cache, so a later retry does not pay for it.
    assert_eq!(
        env.cached_names(),
        vec![("cdc-backfill".to_string(), "claude".to_string())]
    );
}

/// The other half of round-2 review low #3: a heuristic proposal has nothing in
/// the cache to retry from, so a failed apply stamps the hash rather than
/// letting every following `Stop` hook call the model again.
#[test]
fn a_failed_apply_of_a_heuristic_name_stamps_the_hash() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    let mut fixture = env.fixture();
    fixture["fail_rename_session"] = serde_json::json!("tmux server went away");
    env.write_fixture(fixture);

    // No canned answer at all = `claude` unavailable, so the proposal is the
    // heuristic one.
    env.cmd()
        .args(["name", "foo", "--auto", "--apply", "--force", "--json"])
        .assert()
        .failure();

    let stored = env.quest_row("foo");
    assert_eq!(stored["slug"], "foo");
    assert!(stored["name_input_hash"].is_string(), "{stored}");
    assert!(env.cached_names().is_empty());
}

#[test]
fn name_apply_records_the_input_hash_even_when_the_slug_does_not_change() {
    let env = Env::new();
    quest_with_prompt(&env, "cdc-backfill");
    let canned = env.canned_name("cdc-backfill");

    let out = env.name_json(
        Some(&canned),
        &["name", "cdc-backfill", "--auto", "--apply", "--force"],
    );
    assert_eq!(out["applied"], true);
    assert_eq!(out["renamed"]["changed"], false);
    assert_eq!(out["current"], "cdc-backfill");
    let stored = env.quest_row("cdc-backfill");
    assert_eq!(stored["name_input_hash"], out["proposal"]["input_hash"]);
    // No rename happened, so nothing was logged and nobody was told.
    assert!(
        !event_kinds(&env, out["quest"]["id"].as_str().unwrap())
            .contains(&"name.changed".to_string())
    );
}

#[test]
fn name_without_auto_reports_how_the_quest_got_its_name() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    let out = env.name_json(None, &["name", "foo"]);
    assert_eq!(out["slug"], "foo");
    assert_eq!(out["name_source"], "manual");
    assert_eq!(out["stored_input_hash"], serde_json::Value::Null);
    assert_eq!(out["input_hash"].as_str().unwrap().len(), 64);
    assert_eq!(out["stale"], true);
}

#[test]
fn name_detach_records_the_argv_it_would_have_run_and_returns_at_once() {
    let env = Env::new();
    let quest = quest_with_prompt(&env, "foo");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();
    let spawns = env.dir.path().join("spawns.jsonl");
    let assert = env
        .cmd()
        .env("Q_NO_DETACH", &spawns)
        .args([
            "name", "foo", "--auto", "--apply", "--detach", "--force", "--json",
        ])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["detached"], true);
    // The child is handed the id, not whatever the user typed: it resolves
    // nothing itself and its stderr goes nowhere.
    let expected = serde_json::json!(["name", id, "--auto", "--apply", "--force"]);
    assert_eq!(out["args"], expected);
    let recorded: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&spawns).unwrap().trim()).unwrap();
    assert_eq!(recorded["args"], expected);
    // Nothing was renamed: the child never ran.
    assert_eq!(
        env.quest_row("foo")["name_input_hash"],
        serde_json::Value::Null
    );
}

#[test]
fn name_detach_refuses_a_quest_that_does_not_exist() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    let spawns = env.dir.path().join("spawns.jsonl");
    let assert = env
        .cmd()
        .env("Q_NO_DETACH", &spawns)
        .args(["name", "nope", "--auto", "--apply", "--detach", "--json"])
        .assert()
        .failure();
    assert_eq!(error_json(&assert)["code"], "not_found");
    assert!(!spawns.exists(), "a detached child was still spawned");
}

#[test]
fn name_apply_leaves_a_hand_picked_name_alone_unless_forced() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    let canned = env.canned_name("cdc-backfill");
    let assert = env
        .cmd()
        .env("Q_FIXTURE_CLAUDE_NAME", &canned)
        .args(["name", "foo", "--auto", "--apply", "--json"])
        .assert()
        .failure();
    assert_eq!(error_json(&assert)["code"], "conflict");
    assert_eq!(env.quest_row("foo")["name_source"], "manual");
    // Proposing without applying is always allowed.
    let out = env.name_json(Some(&canned), &["name", "foo", "--auto"]);
    assert_eq!(out["proposal"]["slug"], "cdc-backfill");
    assert_eq!(out["applied"], false);
    // And `--force` hands it over.
    let out = env.name_json(
        Some(&canned),
        &["name", "foo", "--auto", "--apply", "--force"],
    );
    assert_eq!(out["current"], "cdc-backfill");
    assert_eq!(out["quest"]["name_source"], "auto");
}

#[test]
fn name_flags_that_only_make_sense_with_auto_are_rejected() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    for flag in ["--apply", "--refresh", "--detach"] {
        env.cmd().args(["name", "foo", flag]).assert().code(2);
    }
}

#[test]
fn name_needs_a_quest_that_exists() {
    let env = Env::new();
    quest_with_prompt(&env, "foo");
    let err = env.json_err(&["name", "nope", "--auto"]);
    assert_eq!(err["code"], "not_found");
}

// ---------------------------------------------------------------- q remote

/// The scripted ssh (SPEC §15): a JSON file of canned answers per alias, plus
/// a log every call appends to. With `Q_FIXTURE` set the fixture backend is
/// always the one that answers, so no test can reach a real host — an alias
/// the script does not name simply fails, like an unknown one.
impl Env {
    fn write_config(&self, text: &str) {
        std::fs::write(self.dir.path().join("config.toml"), text).unwrap();
    }

    /// `[[remotes]]` for each `(name, ssh alias)`.
    fn with_remotes(&self, remotes: &[(&str, &str)]) {
        let mut text = String::from("[machine]\nname = \"laptop\"\n");
        for (name, alias) in remotes {
            text.push_str(&format!(
                "\n[[remotes]]\nname = \"{name}\"\nssh = \"{alias}\"\n"
            ));
        }
        self.write_config(&text);
    }

    fn ssh_log(&self) -> std::path::PathBuf {
        self.dir.path().join("ssh.log")
    }

    /// One line per fixture ssh call: the alias then the argv, tab separated.
    fn ssh_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.ssh_log())
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// Arms the scripted ssh and records every call it makes.
    fn with_ssh(&self, cmd: &mut Command, hosts: serde_json::Value) {
        let path = self.dir.path().join("ssh.json");
        std::fs::write(&path, serde_json::json!({ "hosts": hosts }).to_string()).unwrap();
        cmd.env("Q_FIXTURE_SSH", path)
            .env("Q_FIXTURE_SSH_LOG", self.ssh_log());
    }

    /// The rows of `q list --json`, whatever else the envelope carries.
    fn quests(&self, args: &[&str]) -> serde_json::Value {
        self.json(args)["quests"].clone()
    }

    /// `q list --json` with the scripted ssh in place.
    fn list(&self, hosts: serde_json::Value) -> assert_cmd::assert::Assert {
        let mut cmd = self.cmd();
        self.with_ssh(&mut cmd, hosts);
        cmd.args(["list", "--json"]).assert()
    }
}

/// A real `q list --json` from a second, independent sandbox — exactly what a
/// remote machine would send back. Generated rather than hand-written so the
/// wire format cannot drift away from the one this test claims to read.
fn remote_listing(machine: &str, slug: &str) -> String {
    let far = Env::new();
    far.write_config(&format!("[machine]\nname = \"{machine}\"\n"));
    let work = far.work("repo");
    far.cmd()
        .args(["new", "--name", slug, "--no-beads", "-d", "--json"])
        .args(["--dir", work.to_str().unwrap()])
        .assert()
        .success();
    let assert = far.cmd().args(["list", "--json"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(parsed["quests"].as_array().unwrap().len(), 1, "{parsed}");
    assert_eq!(parsed["quests"][0]["machine"], machine);
    out.trim().to_string()
}

fn stderr_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stderr.clone()).unwrap()
}

#[test]
fn every_configured_remote_is_asked_for_its_listing() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host"), ("box", "box-host")]);
    let assert = env
        .list(serde_json::json!({
            "ws-host": { "stdout": "[]" },
            "box-host": { "stdout": "[]" },
        }))
        .success();
    assert_eq!(stderr_of(&assert), "", "a healthy round says nothing");

    let calls = env.ssh_calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.contains(&"ws-host\tq\tlist\t--json\t--no-remote".to_string()));
    assert!(calls.contains(&"box-host\tq\tlist\t--json\t--no-remote".to_string()));
}

#[test]
fn without_remotes_configured_no_ssh_runs_at_all() {
    let env = Env::new();
    env.list(serde_json::json!({})).success();
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());
    assert!(!env.ssh_log().exists());
}

#[test]
fn no_remote_skips_the_fan_out() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": "[]" } }),
    );
    cmd.args(["list", "--json", "--no-remote"])
        .assert()
        .success();
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());

    // The very same command without the guard does reach out.
    env.list(serde_json::json!({ "ws-host": { "stdout": "[]" } }))
        .success();
    assert_eq!(env.ssh_calls().len(), 1);
}

#[test]
fn a_machine_filter_asks_only_that_remote() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host"), ("box", "box-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({
            "ws-host": { "stdout": "[]" },
            "box-host": { "stdout": "[]" },
        }),
    );
    cmd.args(["list", "--json", "--machine", "box"])
        .assert()
        .success();
    assert_eq!(env.ssh_calls().len(), 1);
    assert!(env.ssh_calls()[0].starts_with("box-host\t"));
}

#[test]
fn a_remote_that_times_out_is_marked_unreachable_and_the_listing_still_succeeds() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let work = env.work("repo");
    env.cmd()
        .args(["new", "--name", "local-one", "--no-beads", "-d"])
        .args(["--dir", work.to_str().unwrap()])
        .assert()
        .success();

    let assert = env
        .list(serde_json::json!({ "ws-host": { "timeout": true } }))
        .success();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("⚠ unreachable"), "{stderr}");
    assert!(stderr.contains("ws"), "{stderr}");
    // The local listing is untouched by the remote being down.
    let listed = quests_of(&assert);
    assert_eq!(listed[0]["slug"], "local-one");
}

#[test]
fn an_unreachable_remote_still_shows_its_last_known_quests() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let payload = remote_listing("ws", "over-there");

    let good = env
        .list(serde_json::json!({ "ws-host": { "stdout": payload } }))
        .success();
    assert_eq!(stderr_of(&good), "");

    // Same command, host now dead: the cached response stands in.
    let down = env
        .list(serde_json::json!({ "ws-host": { "timeout": true } }))
        .success();
    let stderr = stderr_of(&down);
    assert!(stderr.contains("⚠ unreachable"), "{stderr}");
    assert!(stderr.contains("1 cached quest"), "{stderr}");
}

#[test]
fn a_remote_answering_with_something_unreadable_is_incompatible() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    for stdout in ["not json at all", "", "[{\"id\":\"q-1\"}]"] {
        let assert = env
            .list(serde_json::json!({ "ws-host": { "stdout": stdout } }))
            .success();
        let stderr = stderr_of(&assert);
        assert!(stderr.contains("⚠ incompatible"), "`{stdout}` → {stderr}");
    }
}

#[test]
fn a_remote_without_q_installed_is_unreachable_with_its_own_message() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let assert = env
        .list(serde_json::json!({
            "ws-host": { "exit": 127, "stderr": "bash: q: command not found" }
        }))
        .success();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("⚠ unreachable"), "{stderr}");
    assert!(stderr.contains("command not found"), "{stderr}");
}

/// SPEC §16's listing filters have to travel: a remote answering its own
/// default listing would contradict the `--all`/`--state` actually asked for
/// the moment bd-8lz.5.2 merges the rows.
#[test]
fn the_listing_filters_are_forwarded_to_the_remote() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": "[]" } }),
    );
    cmd.args(["list", "--json", "--all", "--state", "finished"])
        .assert()
        .success();
    assert_eq!(
        env.ssh_calls(),
        ["ws-host\tq\tlist\t--json\t--no-remote\t--all\t--state\tfinished"]
    );
}

/// `q list -q` prints nothing, so it must not pay for a round that can cost the
/// whole remote deadline.
#[test]
fn a_quiet_listing_does_not_reach_out_at_all() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": "[]" } }),
    );
    let assert = cmd.args(["list", "-q"]).assert().success();
    assert_eq!(
        String::from_utf8(assert.get_output().stdout.clone()).unwrap(),
        ""
    );
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());

    // `--json` still prints, so it still asks.
    env.list(serde_json::json!({ "ws-host": { "stdout": "[]" } }))
        .success();
    assert_eq!(env.ssh_calls().len(), 1);
}

/// The far end's own `[machine] name` is not the name this side files it under:
/// `remotes[].name` is what `--machine` takes and what the machine column shows.
#[test]
fn a_remote_that_calls_itself_something_else_is_cached_verbatim_under_its_config_name() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    // The box knows itself as `workstation`; the config reaches it as `ws`.
    let payload = remote_listing("workstation", "over-there");
    env.list(serde_json::json!({ "ws-host": { "stdout": payload } }))
        .success();

    let down = env
        .list(serde_json::json!({ "ws-host": { "timeout": true } }))
        .success();
    let stderr = stderr_of(&down);
    assert!(stderr.contains("ws ⚠ unreachable"), "{stderr}");
    assert!(stderr.contains("1 cached quest"), "{stderr}");
}

#[test]
fn a_remote_the_script_does_not_know_never_reaches_a_real_host() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let assert = env.list(serde_json::json!({})).success();
    assert!(stderr_of(&assert).contains("⚠ unreachable"));
}

// ------------------------------------------- q list: the merged listing (5.2)

/// The rows of `q list --json`, and the machines it asked, with the scripted
/// ssh in place.
impl Env {
    fn listing(&self, hosts: serde_json::Value) -> serde_json::Value {
        json_of(&self.list(hosts).success())
    }

    /// The row of `slug` in a listing envelope.
    fn row_of<'a>(listing: &'a serde_json::Value, slug: &str) -> &'a serde_json::Value {
        listing["quests"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["slug"] == slug)
            .unwrap_or_else(|| panic!("no row `{slug}` in {listing}"))
    }

    /// The machine entry called `name`.
    fn machine_of<'a>(listing: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
        listing["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == name)
            .unwrap_or_else(|| panic!("no machine `{name}` in {listing}"))
    }
}

/// SPEC §15: one listing, both machines, with a machine column.
#[test]
fn a_remote_machines_quests_join_the_listing() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");
    let payload = remote_listing("ws", "over-there");

    let listing = env.listing(serde_json::json!({ "ws-host": { "stdout": payload } }));
    let slugs: Vec<&str> = listing["quests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap())
        .collect();
    assert!(slugs.contains(&"here"), "{listing}");
    assert!(slugs.contains(&"over-there"), "{listing}");

    // Local and remote rows are told apart by `source`, not by guesswork.
    assert_eq!(
        Env::row_of(&listing, "here")["source"],
        serde_json::json!({ "kind": "local" })
    );
    assert_eq!(
        Env::row_of(&listing, "over-there")["source"],
        serde_json::json!({ "kind": "remote", "stale": false })
    );
    assert_eq!(Env::row_of(&listing, "over-there")["machine"], "ws");

    // The same listing for a human, with the machine column of SPEC §15.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } }),
    );
    let assert = cmd.arg("list").assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("MACHINE"), "{human}");
    assert!(human.contains("over-there"), "{human}");
    assert!(human.contains("ws"), "{human}");
}

/// The rows a remote sent are re-emitted exactly as they arrived: a field a
/// newer `q` over there knows and this one does not must survive.
#[test]
fn a_remote_row_is_re_emitted_verbatim() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut payload: serde_json::Value =
        serde_json::from_str(&remote_listing("workstation", "over-there")).unwrap();
    payload["quests"][0]["something_from_the_future"] = serde_json::json!({ "a": 1 });
    let payload = payload.to_string();

    let listing = env.listing(serde_json::json!({ "ws-host": { "stdout": payload } }));
    let row = Env::row_of(&listing, "over-there");
    assert_eq!(
        row["something_from_the_future"],
        serde_json::json!({ "a": 1 })
    );
    // …except `machine`, which is `remotes[].name` rather than what the far
    // end calls itself, and `source`, which only this side can know.
    assert_eq!(row["machine"], "ws");
    assert_eq!(row["source"]["kind"], "remote");
}

/// A machine that is down keeps its rows, marked — in the column and in the
/// row's `source` — and says so on stderr (SPEC §15).
#[test]
fn an_unreachable_remotes_cached_rows_are_shown_and_marked_stale() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let payload = remote_listing("ws", "over-there");
    env.list(serde_json::json!({ "ws-host": { "stdout": payload } }))
        .success();

    let listing = env.listing(serde_json::json!({ "ws-host": { "timeout": true } }));
    assert_eq!(
        Env::row_of(&listing, "over-there")["source"],
        serde_json::json!({ "kind": "remote", "stale": true })
    );
    let machine = Env::machine_of(&listing, "ws");
    assert_eq!(machine["status"], "unreachable");
    assert!(
        machine["reason"].as_str().unwrap().contains("5s"),
        "{machine}"
    );
    assert_eq!(machine["stale"], true);
    assert_eq!(machine["quests"], 1);
    assert!(machine["fetched_at"].is_i64(), "{machine}");

    // And in the table, where a stale row sits next to live ones.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "timeout": true } }),
    );
    let assert = cmd.arg("list").assert().success();
    let human = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(human.contains("ws \u{26a0} stale"), "{human}");
    assert!(stderr_of(&assert).contains("⚠ unreachable"), "{assert:?}");
}

/// The one thing rows cannot say: a machine that is down and has nothing
/// cached contributes none, and "no answer" must not read as "no Quests".
#[test]
fn a_machine_that_is_down_with_no_cache_is_still_in_the_machines_array() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let listing = env.listing(serde_json::json!({ "ws-host": { "timeout": true } }));
    assert!(
        listing["quests"].as_array().unwrap().is_empty(),
        "{listing}"
    );

    let machine = Env::machine_of(&listing, "ws");
    assert_eq!(machine["status"], "unreachable");
    assert_eq!(machine["stale"], false);
    assert_eq!(machine["fetched_at"], serde_json::Value::Null);
    assert_eq!(machine["quests"], 0);
    // Flattened: bd-8lz.5.1 nested this as `{"status": {"status": …}}`.
    assert!(machine["status"].is_string(), "{machine}");
}

#[test]
fn a_remote_on_an_unreadable_version_is_incompatible_in_the_machines_array() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let listing = env.listing(serde_json::json!({ "ws-host": { "stdout": "not json" } }));
    let machine = Env::machine_of(&listing, "ws");
    assert_eq!(machine["status"], "incompatible");
    assert!(machine["reason"].as_str().unwrap().contains("cannot read"));
}

/// `--machine` narrows the rows *and* the roster, to a remote and to this one.
#[test]
fn a_machine_filter_narrows_the_merged_listing_both_ways() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");
    let payload = remote_listing("ws", "over-there");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload.clone() } }),
    );
    let listing = json_of(
        &cmd.args(["list", "--json", "--machine", "ws"])
            .assert()
            .success(),
    );
    let rows = listing["quests"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listing}");
    assert_eq!(rows[0]["slug"], "over-there");
    assert_eq!(listing["machines"].as_array().unwrap().len(), 1);
    assert_eq!(listing["machines"][0]["name"], "ws");

    // Pinned to this machine: no ssh at all, and only local rows.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let listing = json_of(
        &cmd.args(["list", "--json", "--machine", "laptop"])
            .assert()
            .success(),
    );
    let rows = listing["quests"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listing}");
    assert_eq!(rows[0]["slug"], "here");
    assert_eq!(listing["machines"].as_array().unwrap().len(), 1);
    assert_eq!(listing["machines"][0]["kind"], "local");
}

/// The recursion guard is also what makes the far end's answer a *local*
/// listing: no rows but its own, and one machine entry.
#[test]
fn no_remote_yields_only_local_rows() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");
    let payload = remote_listing("ws", "over-there");
    // Cache a good answer first, so `--no-remote` could show it if it tried.
    env.list(serde_json::json!({ "ws-host": { "stdout": payload } }))
        .success();

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": "[]" } }),
    );
    let listing = json_of(
        &cmd.args(["list", "--json", "--no-remote"])
            .assert()
            .success(),
    );
    let rows = listing["quests"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{listing}");
    assert_eq!(rows[0]["slug"], "here");
    assert_eq!(rows[0]["source"], serde_json::json!({ "kind": "local" }));
    assert_eq!(listing["machines"].as_array().unwrap().len(), 1);
    assert_eq!(listing["machines"][0]["kind"], "local");
}

// -------------------------------------------------- q enter over ssh (5.2)

/// The ssh log line an attach writes: `attach`, the alias, then the argv.
fn attach_calls(env: &Env) -> Vec<String> {
    env.ssh_calls()
        .into_iter()
        .filter(|line| line.starts_with("attach"))
        .collect()
}

/// SPEC §15: `q enter` on a Quest that runs elsewhere is
/// `ssh -t <alias> tmux attach -t q-<slug>`.
#[test]
fn entering_a_remote_quest_attaches_over_ssh() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let payload = remote_listing("ws", "over-there");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let assert = cmd
        .args(["enter", "over-there", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["machine"], "ws");
    assert_eq!(out["remote"], true);
    assert_eq!(out["tmux_session"], "q-over-there");
    assert_eq!(out["quest"]["slug"], "over-there");

    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t=q-over-there"]
    );
}

/// `[tmux] iterm_cc` (SPEC §15 / §20), and only outside tmux.
#[test]
fn iterm_control_mode_is_used_only_outside_tmux() {
    let env = Env::new();
    env.write_config(
        "[machine]\nname = \"laptop\"\n\n[tmux]\niterm_cc = true\n\n\
         [[remotes]]\nname = \"ws\"\nssh = \"ws-host\"\n",
    );
    let payload = remote_listing("ws", "over-there");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload.clone() } }),
    );
    cmd.args(["enter", "over-there"]).assert().success();
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\t-CC\tattach\t-t\t=q-over-there"]
    );

    // Inside tmux a control-mode client would be a nested one, which iTerm2
    // cannot host: the plain attach is the only thing that works.
    std::fs::remove_file(env.ssh_log()).unwrap();
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [], "in_tmux": true }));
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    cmd.args(["enter", "over-there"]).assert().success();
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t=q-over-there"]
    );
}

/// A local Quest is entered without ever dialling out, and a name that is on
/// neither machine is still a plain "not found".
#[test]
fn entering_looks_locally_first_and_reports_a_typo_as_a_typo() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } }),
    );
    cmd.args(["enter", "here"]).assert().success();
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } }),
    );
    let assert = cmd.args(["enter", "nowhere", "--json"]).assert().code(1);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("not_found"), "{err}");
    assert!(err.contains("nowhere"), "{err}");
    assert!(attach_calls(&env).is_empty());
}

/// `--no-remote` breaks the recursion, so it also stops `q enter` from
/// looking anywhere but here.
#[test]
fn entering_under_no_remote_never_asks_another_machine() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } }),
    );
    cmd.args(["enter", "over-there", "--no-remote"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("over-there"));
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());
}

/// A window inside a remote Quest needs that machine's session rows, which is
/// bd-8lz.5.3's proxying; until then it is refused rather than mis-attached.
#[test]
fn a_session_label_on_a_remote_quest_is_refused_for_now() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } }),
    );
    cmd.args(["enter", "over-there", "--session", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not supported on ws"));
    assert!(attach_calls(&env).is_empty());
}
