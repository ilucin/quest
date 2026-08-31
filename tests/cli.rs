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
        .env("Q_CLAUDE_SETTINGS", claude_dir(dir).join("settings.json"))
        // Pin the skill file explicitly too, exactly as `tests/skill.rs` does:
        // no skill/doctor test may ever write to the real `~/.claude`, and a
        // faked `$HOME` alone leaves that one `dirs`/platform change away.
        .env("Q_CLAUDE_SKILL", claude_dir(dir).join("skills/q/SKILL.md"));
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
        "sessions", "peek", "send", "reset", "kill", "tpl",
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
    std::fs::write(
        &path,
        "[machine]\nname = \"laptop\"\n\n[[remotes]]\nname = \"ws\"\nssh = \"ws-host\"\n",
    )
    .unwrap();
    cmd.args(["--machine", "ws", "config", "set", "ui.mouse", "false"])
        .assert()
        .success();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("name = \"laptop\""), "{text}");
    assert!(text.contains("mouse = false"), "{text}");
}

/// `--machine` is validated wherever it is given, including on the commands
/// that tolerate a broken config — swallowing it is how `q doctor` came to
/// answer about machines that do not exist (bd-8lz.5.4 review F2).
#[test]
fn machine_flag_is_validated_even_on_the_lenient_commands() {
    for args in [
        vec!["--machine", "nope", "config", "set", "ui.mouse", "false"],
        vec!["--machine", "nope", "config", "path"],
        vec!["--machine", "nope", "doctor"],
    ] {
        let mut cmd = q();
        std::fs::write(config_path(&cmd), "[machine]\nname = \"laptop\"\n").unwrap();
        cmd.args(&args)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("known machines: laptop"));
    }
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

    /// A user workflow file (SPEC §11), written straight into the sandboxed
    /// config directory — `Q_CONFIG` points at `config.toml` in here, so
    /// `workflows/` beside it is the directory `q` reads. Returns its path.
    fn workflow(&self, name: &str, body: &str) -> std::path::PathBuf {
        let dir = self.dir.path().join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn workflow_dir(&self) -> std::path::PathBuf {
        self.dir.path().join("workflows")
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
    // `--workflow` is checked against the registry (SPEC §11), so the name has
    // to be one that exists; a user file is the half a built-in cannot prove.
    env.workflow("tdd", "# tdd\n\nred, green, refactor.\n");
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
    // The pane runs the login shell (SPEC §6 v2); Claude is typed into it.
    assert_eq!(pane["command"], serde_json::Value::Null);
    assert_eq!(pane["buffer"], "claude -n foo/master\n");
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
        pane_of(&env.fixture(), "q-foo")["buffer"],
        format!(
            "{}\n",
            r#"claude -n foo/master -- 'fix it; don'\''t '\''break'\'' it'"#
        )
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
        pane_of(&env.fixture(), "q-foo")["buffer"],
        "claude -n foo/master -- 'from stdin'\n"
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
        pane_of(&env.fixture(), "q-foo")["buffer"],
        "claude -n foo/master -- 'from a file'\n"
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
        "--template",
        "--brain",
    ] {
        assert!(
            out.contains(flag),
            "`{flag}` missing from `q new --help`:\n{out}"
        );
    }
    assert!(
        !out.contains("--from-brief"),
        "`--from-brief` is not implemented yet:\n{out}"
    );
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

/// `doctor_bare` plus q's hooks and agent skill installed, so a test about
/// some other check is not drowned in hook or skill failures.
fn doctor(names: &[&str]) -> TestCmd {
    let cmd = doctor_bare(names);
    install_hooks(&cmd);
    install_skill(&cmd);
    cmd
}

/// `q skill install` into the sandboxed `~/.claude` (HOME is faked by
/// `sandbox`), so `q doctor`'s skill check reports it installed.
fn install_skill(cmd: &TestCmd) {
    let mut installer = Command::cargo_bin("q").unwrap();
    sandbox(&mut installer, cmd.dir.path());
    installer.args(["skill", "install"]).assert().success();
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
            "claude wrapper",
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
            "skill",
            "statusline chain",
            "orphan sessions",
            "fleet names",
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
// ------------------------------------------- doctor: remotes (bd-8lz.5.4)

/// `[[remotes]]` in this command's config, plus a scripted ssh — SPEC §19's
/// remote checks, without a host anywhere near them. The ssh log doubles as
/// the proof that a run made no ssh call at all.
fn doctor_remotes(
    cmd: &mut TestCmd,
    remotes: &[(&str, &str)],
    hosts: serde_json::Value,
) -> std::path::PathBuf {
    let mut config = String::from("[machine]\nname = \"laptop\"\n");
    for (name, alias) in remotes {
        config.push_str(&format!(
            "\n[[remotes]]\nname = \"{name}\"\nssh = \"{alias}\"\n"
        ));
    }
    std::fs::write(cmd.dir.path().join("config.toml"), config).unwrap();
    let script = cmd.dir.path().join("ssh.json");
    std::fs::write(&script, serde_json::json!({ "hosts": hosts }).to_string()).unwrap();
    let log = cmd.dir.path().join("ssh.log");
    cmd.env("Q_FIXTURE_SSH", &script)
        .env("Q_FIXTURE_SSH_LOG", &log);
    log
}

fn ssh_log_lines(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A `q --version` answer for one alias, next to a `ssh -G` answer.
fn host(version: serde_json::Value, options: &str) -> serde_json::Value {
    serde_json::json!({
        "version": version,
        "options": { "stdout": options },
    })
}

const MUX_ON: &str = "controlmaster auto\ncontrolpath /tmp/cm-%r@%h:%p\ncontrolpersist 600\n";
const MUX_OFF: &str = "controlmaster false\ncontrolpersist no\n";

/// The common case is `remotes = []`, and it must cost nothing: no ssh, no
/// check lines, no wait.
#[test]
fn doctor_without_remotes_makes_no_ssh_call() {
    let mut cmd = doctor(&["claude"]);
    let log = doctor_remotes(&mut cmd, &[], serde_json::json!({}));
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);

    assert!(ssh_log_lines(&log).is_empty(), "{:?}", ssh_log_lines(&log));
    let names: Vec<&str> = parsed["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(!names.iter().any(|n| n.starts_with("remote ")), "{names:?}");
    assert!(
        !names.iter().any(|n| n.starts_with("ssh multiplexing")),
        "{names:?}"
    );
}

/// SPEC §19's probe, verbatim: `ssh <alias> q --version`.
#[test]
fn doctor_probes_every_remote_with_q_version() {
    let mut cmd = doctor(&["claude"]);
    let log = doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host"), ("box", "box-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
            "box-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);

    let mut calls = ssh_log_lines(&log);
    calls.sort();
    assert_eq!(
        calls,
        [
            "box-host\tq\t--version",
            "options\tbox-host",
            "options\tws-host",
            "ws-host\tq\t--version",
        ],
        "{calls:?}"
    );
    for name in ["remote ws", "remote box"] {
        assert_eq!(check(&parsed, name)["status"], "ok", "{parsed}");
    }
    let detail = check(&parsed, "remote ws")["detail"].as_str().unwrap();
    assert!(detail.contains("q 0.1.0 (wire 1)"), "{detail}");
    assert!(detail.contains("ssh ws-host"), "{detail}");
    assert_eq!(parsed["ok"], true);
}

/// `q --version` is what the far end will be asked, so it is what this `q`
/// prints — wire tag included, or a remote could never be told apart from an
/// older one.
#[test]
fn the_version_banner_carries_the_wire_version() {
    let assert = q().arg("--version").assert().success();
    let banner = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(banner.contains("(wire "), "{banner}");

    // …and bare `q --json` reports the same two facts, each in its own field
    // rather than one string a script would have to re-parse.
    let assert = q().arg("--json").assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    let wire = parsed["wire"].as_u64().expect("a wire number");
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert!(
        banner.contains(&format!("(wire {wire})")),
        "{banner} vs {parsed}"
    );

    // `q --version --json` honours `--json` too — clap owned `--version` and
    // printed the plain banner before any `q` code ran (bd-8lz.9); now `q`
    // owns it, so the same two facts come back as a document, not a banner.
    let assert = q().args(["--version", "--json"]).assert().success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(parsed["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(parsed["wire"], wire);
}

/// A host that is asleep or off the network is not a broken setup and not one
/// this machine can repair, so it warns and the report still passes — a
/// scripted `q doctor` must not flap with a laptop's lid (bd-8lz.5.4 F4).
#[test]
fn doctor_warns_about_an_unreachable_remote() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(
                serde_json::json!({ "exit": 255, "stderr": "ssh: connect to host ws-host port 22: No route to host" }),
                MUX_OFF,
            ),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    assert!(
        remote["detail"].as_str().unwrap().contains("No route"),
        "{parsed}"
    );
    // The fix is about the connection, not about `q`.
    let hint = remote["fix_hint"].as_str().unwrap();
    assert!(hint.contains("ssh ws-host"), "{hint}");
    assert!(!hint.contains("upgrade"), "{hint}");
}

/// A remote that never answers must cost the deadline and not a second more —
/// the whole point of bd-8lz.5.1's hard deadline. And the line must not blame
/// the network for it: only this one probe hung, and `q list` may be talking
/// to the same host perfectly (bd-8lz.5.4 D4).
#[test]
fn doctor_gives_up_on_a_silent_remote_at_the_deadline() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            // Far longer than the 5 s deadline: the fixture waits the deadline
            // out and answers `TimedOut`, exactly as the real backend does.
            "ws-host": host(serde_json::json!({ "delay_ms": 600_000 }), MUX_OFF),
        }),
    );
    let started = std::time::Instant::now();
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "doctor waited {elapsed:?} on one dead remote"
    );
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    let detail = remote["detail"].as_str().unwrap();
    assert!(detail.contains("no answer"), "{detail}");
    assert!(detail.contains("q --version"), "{detail}");
    // The fix asks about the probe, not about host, network and key.
    let hint = remote["fix_hint"].as_str().unwrap();
    assert!(hint.contains("`ssh ws-host q --version` answers"), "{hint}");
    assert!(!hint.contains("~/.ssh/config"), "{hint}");
}

/// The case the real `ws` is: up, ssh fine, and no `q` on it. Before the probe
/// this read `unreachable`, which sent the user looking at the network.
#[test]
fn doctor_tells_a_reachable_host_without_q_apart_from_a_dead_one() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(
                serde_json::json!({ "exit": 127, "stderr": "zsh: command not found: q" }),
                MUX_ON,
            ),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "fail");
    assert!(
        remote["detail"]
            .as_str()
            .unwrap()
            .contains("no `q` on PATH"),
        "{parsed}"
    );
    assert!(
        remote["fix_hint"]
            .as_str()
            .unwrap()
            .contains("install q on ws"),
        "{parsed}"
    );
}

/// The debt bd-8lz.5.3 left: a `q` too old to understand `--expect` answers
/// with clap's exit 2 and no explanation. Its `--version` has no wire tag —
/// but so does every `q` up to and including bd-8lz.5.3's own `main`, which
/// this one drives end to end and which `q list` reports as `ok`. A banner
/// cannot tell those apart, so doctor advises and does not condemn: failing
/// here would make doctor contradict `q list` about the same host
/// (bd-8lz.5.4 review F3).
#[test]
fn doctor_advises_rather_than_fails_a_remote_that_reports_no_wire() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0" }), MUX_ON),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    let detail = remote["detail"].as_str().unwrap();
    assert!(detail.contains("q 0.1.0"), "{detail}");
    assert!(detail.contains("no wire tag"), "{detail}");
    assert!(detail.contains("--expect"), "{detail}");
    assert!(
        remote["fix_hint"]
            .as_str()
            .unwrap()
            .contains("upgrade `q` on ws"),
        "{parsed}"
    );
}

/// A tag that is there and is not a number is a broken answer, not an ancient
/// `q`; reading it as "older than everything" told a far end claiming
/// `(wire 4294967296)` to upgrade (bd-8lz.5.4 review F5).
#[test]
fn doctor_warns_about_a_wire_tag_it_cannot_read() {
    for tag in ["4294967296", "-1", "99999999999999999999"] {
        let mut cmd = doctor(&["claude"]);
        doctor_remotes(
            &mut cmd,
            &[("ws", "ws-host")],
            serde_json::json!({
                "ws-host": host(
                    serde_json::json!({ "stdout": format!("q 0.1.0 (wire {tag})") }),
                    MUX_ON,
                ),
            }),
        );
        let assert = cmd.args(["doctor", "--json"]).assert().success();
        let parsed = json_of(&assert);
        let remote = check(&parsed, "remote ws");
        assert_eq!(remote["status"], "warn", "{parsed}");
        let detail = remote["detail"].as_str().unwrap();
        assert!(detail.contains(&format!("(wire {tag})")), "{detail}");
        assert!(!detail.contains("needs wire"), "{detail}");
    }
}

/// A tag that is present, readable and below the floor is the strongest thing
/// a version banner can say — and still advice, not a verdict. Nothing in `q`
/// consults the wire before talking to a remote, so failing here would condemn
/// a host `q list` and every proxied command go on using: the live test found
/// exactly that, a far end reporting `wire 0` that serves perfectly
/// (bd-8lz.5.4 D2).
#[test]
fn doctor_advises_rather_than_fails_a_remote_below_the_wire_floor() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.0.1 (wire 0)" }), MUX_ON),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    let detail = remote["detail"].as_str().unwrap();
    assert!(detail.contains("older than the wire 1"), "{detail}");
    assert!(detail.contains("may fail"), "{detail}");
    assert_eq!(remote["fix_hint"], "upgrade `q` on ws");
    assert_eq!(parsed["ok"], true, "{parsed}");
}

/// D1: a `q` whose `--version` fails and whose every other verb works. The
/// probe failed; that is what the line says, and the report still passes —
/// `q list` is using this host in the very same session (see
/// `doctor_and_list_agree_about_every_far_end_state`).
#[test]
fn doctor_advises_rather_than_fails_a_q_whose_version_probe_failed() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(
                serde_json::json!({ "exit": 3, "stderr": "boom: cannot start" }),
                MUX_ON,
            ),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    let detail = remote["detail"].as_str().unwrap();
    assert!(detail.contains("exited 3"), "{detail}");
    assert!(detail.contains("boom: cannot start"), "{detail}");
    assert!(detail.contains("not necessarily anything else"), "{detail}");
    assert_eq!(parsed["ok"], true, "{parsed}");
}

/// D3, said out loud: the probe is `q --version`, and a green line reports
/// what it saw rather than promising the remote serves. Doctor cannot see the
/// difference from a banner, so it does not pretend to.
#[test]
fn a_green_remote_line_says_only_what_the_probe_saw() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "ok");
    let detail = remote["detail"].as_str().unwrap();
    assert!(detail.contains("`q --version`"), "{detail}");
    assert!(detail.contains("q list"), "{detail}");
}

/// The other direction: the far end is newer than this `q`. Not a failure —
/// the listing parse ignores fields it has never heard of — and the fix is on
/// this machine.
#[test]
fn doctor_warns_when_the_remote_q_is_newer_than_this_one() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 9.9.9 (wire 99)" }), MUX_ON),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    assert!(
        remote["detail"].as_str().unwrap().contains("wire 99"),
        "{parsed}"
    );
    assert!(
        remote["fix_hint"].as_str().unwrap().contains("laptop"),
        "{parsed}"
    );
}

/// A login shell that prints a banner, a wrapper that swallows `--version`: an
/// answer that is not a version is not proof of an old `q`, so it warns.
#[test]
fn doctor_warns_about_an_unreadable_version_answer() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(
                serde_json::json!({ "stdout": "Welcome to ws! Have a nice day.\n" }),
                MUX_ON,
            ),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    assert!(
        remote["detail"].as_str().unwrap().contains("Welcome to ws"),
        "{parsed}"
    );
}

/// A second `q` against the same sandbox and the same scripted ssh — for the
/// one test that has to ask two commands about the same host in the same
/// state.
fn same_sandbox(cmd: &TestCmd) -> Command {
    let mut second = Command::cargo_bin("q").unwrap();
    sandbox(&mut second, cmd.dir.path());
    second
        .env("PATH", bin_dir(cmd))
        .env("Q_FIXTURE_SSH", cmd.dir.path().join("ssh.json"))
        .env("Q_FIXTURE_SSH_LOG", cmd.dir.path().join("ssh.log"));
    second
}

/// **The coherence rule this bead commits to**, pinned state by state.
///
/// `q doctor` and `q list` ask the same host different questions — the probe
/// is `ssh <alias> q --version`, the listing is `ssh <alias> q list --json
/// --no-remote --all` — so they *can* disagree, and the live two-machine test
/// found three states where they did. The rule that survives is
/// one-directional:
///
/// **doctor never fails a host `q list` is willing to use.** The only remote
/// `Fail` is "ssh got there and there is no `q` on PATH", which `q list` also
/// refuses (`incompatible`).
///
/// The converse does **not** hold and cannot: the last row is a far end whose
/// `q --version` is perfect and whose `q list --json` is garbage. Doctor
/// cannot see that from a banner, so it stays green — and its green line says
/// what it actually saw rather than promising the remote serves.
#[test]
fn doctor_and_list_agree_about_every_far_end_state() {
    let listing = remote_listing("ws", "over-there");
    let serves = serde_json::json!({ "stdout": listing });
    let unreachable = serde_json::json!({
        "exit": 255,
        "stderr": "ssh: connect to host ws-host port 22: No route to host",
    });
    let no_q = serde_json::json!({ "exit": 127, "stderr": "zsh:1: command not found: q" });

    // (what the far end is, what `q list` gets, what `q --version` gets,
    //  doctor's verdict, `q list`'s status)
    let table: [(&str, serde_json::Value, serde_json::Value, &str, &str); 8] = [
        (
            "healthy",
            serves.clone(),
            serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }),
            "ok",
            "ok",
        ),
        // D1, live on the real `ws`: `q --version` exits 3 and every other
        // verb serves.
        (
            "a q whose --version fails",
            serves.clone(),
            serde_json::json!({ "exit": 3, "stderr": "boom: cannot start" }),
            "warn",
            "ok",
        ),
        // D2, one `MIN_REMOTE_WIRE` bump away from being live.
        (
            "a readable wire below the floor",
            serves.clone(),
            serde_json::json!({ "stdout": "q 0.1.0 (wire 0)" }),
            "warn",
            "ok",
        ),
        (
            "no wire tag at all",
            serves.clone(),
            serde_json::json!({ "stdout": "q 0.1.0" }),
            "warn",
            "ok",
        ),
        (
            "a wire newer than this one",
            serves.clone(),
            serde_json::json!({ "stdout": "q 9.9.9 (wire 99)" }),
            "warn",
            "ok",
        ),
        // The one failure — and `q list` refuses this host too.
        (
            "no q on PATH there",
            no_q.clone(),
            no_q,
            "fail",
            "incompatible",
        ),
        (
            "ssh cannot get there",
            unreachable.clone(),
            unreachable,
            "warn",
            "unreachable",
        ),
        // D3, the documented one-way gap: a banner cannot show this.
        (
            "a perfect banner over a garbage listing",
            serde_json::json!({ "stdout": "not json at all\n" }),
            serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }),
            "ok",
            "incompatible",
        ),
    ];

    for (what, list_answer, version, want_doctor, want_list) in table {
        let mut cmd = doctor(&["claude"]);
        let mut answer = list_answer;
        answer["version"] = version;
        answer["options"] = serde_json::json!({ "stdout": MUX_ON });
        doctor_remotes(
            &mut cmd,
            &[("ws", "ws-host")],
            serde_json::json!({ "ws-host": answer }),
        );

        let assert = cmd.args(["doctor", "--json"]).assert();
        let report = json_of(&assert);
        let remote = check(&report, "remote ws");
        assert_eq!(remote["status"], want_doctor, "{what}: {report}");
        // Nothing else in this sandbox fails, so the report's verdict is the
        // remote line's — and so is the exit code.
        assert_eq!(report["ok"], want_doctor != "fail", "{what}: {report}");

        let assert = same_sandbox(&cmd)
            .args(["list", "--json"])
            .assert()
            .success();
        let machines = json_of(&assert);
        let ws = machines["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["name"] == "ws")
            .unwrap_or_else(|| panic!("{what}: no ws entry in {machines}"))
            .clone();
        assert_eq!(ws["status"], want_list, "{what}: {machines}");

        // The rule, asserted rather than described.
        if want_doctor == "fail" {
            assert_ne!(
                ws["status"], "ok",
                "{what}: doctor failed a host `q list` uses"
            );
        }
        // …and the one place the converse fails, kept honest in the output.
        if want_doctor == "ok" && want_list != "ok" {
            assert_eq!(what, "a perfect banner over a garbage listing");
            let detail = remote["detail"].as_str().unwrap();
            assert!(
                detail.contains("only `q list` can show it serves"),
                "{detail}"
            );
        }
    }
}

/// SPEC §23 #6. A warning, never a failure, and `q` never touches the file.
#[test]
fn doctor_warns_when_ssh_multiplexing_is_missing() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_OFF),
        }),
    );
    // Warn only: the whole report still passes.
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let mux = check(&parsed, "ssh multiplexing ws");
    assert_eq!(mux["status"], "warn");
    let hint = mux["fix_hint"].as_str().unwrap();
    assert!(hint.contains("ControlMaster auto"), "{hint}");
    assert!(hint.contains("ControlPersist"), "{hint}");
    assert!(hint.contains("ws-host"), "{hint}");
}

#[test]
fn doctor_is_happy_with_a_multiplexed_remote() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let mux = check(&parsed, "ssh multiplexing ws");
    assert_eq!(mux["status"], "ok");
    assert!(
        mux["detail"]
            .as_str()
            .unwrap()
            .contains("ControlPersist 600"),
        "{parsed}"
    );
    assert!(mux["fix_hint"].is_null(), "{parsed}");
}

/// A `ControlMaster` with `ControlPersist no` looks configured and reuses
/// nothing: every mux dies with the command that opened it.
#[test]
fn doctor_warns_about_a_control_master_that_never_persists() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(
                serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }),
                "controlmaster auto\ncontrolpath /tmp/cm\ncontrolpersist no\n",
            ),
        }),
    );
    let assert = cmd.args(["doctor", "--json"]).assert().success();
    let parsed = json_of(&assert);
    let mux = check(&parsed, "ssh multiplexing ws");
    assert_eq!(mux["status"], "warn");
    assert!(
        mux["fix_hint"]
            .as_str()
            .unwrap()
            .contains("ControlPersist 10m"),
        "{parsed}"
    );
}

// ------------------------------- doctor: --machine (bd-8lz.5.4 review F1/F2)

/// `--machine` now decides which remotes are probed, so an unknown one is a
/// user error here exactly as it is for `q list`. It used to be swallowed by
/// `Ctx::lenient`, which produced a **green** report about a machine that does
/// not exist — no remote lines, no error, exit 0.
#[test]
fn doctor_refuses_a_machine_that_does_not_exist() {
    let mut cmd = doctor(&["claude"]);
    let log = doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    cmd.args(["--machine", "bogus", "doctor", "--json"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("known machines: laptop, ws"));
    assert!(ssh_log_lines(&log).is_empty(), "{:?}", ssh_log_lines(&log));
}

/// The other invocation `q list` refuses outright and doctor used to accept.
#[test]
fn doctor_refuses_a_remote_machine_together_with_no_remote() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    cmd.args(["--machine", "ws", "--no-remote", "doctor", "--json"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("--no-remote"));
}

/// A broken config is what doctor is *for*, so it stays lenient about the
/// file — but `--machine` names something only the config could define, so it
/// says it cannot resolve one rather than quietly answering about another
/// machine.
#[test]
fn doctor_still_diagnoses_a_broken_config_but_not_under_machine() {
    let mut cmd = doctor(&["claude"]);
    std::fs::write(cmd.dir.path().join("config.toml"), "[machine\n").unwrap();
    let assert = cmd.args(["doctor", "--json"]).assert().code(1);
    assert_eq!(check(&json_of(&assert), "config")["status"], "fail");

    let mut cmd = doctor(&["claude"]);
    std::fs::write(cmd.dir.path().join("config.toml"), "[machine\n").unwrap();
    cmd.args(["--machine", "ws", "doctor", "--json"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("config is unreadable"));
}

/// `--machine <a remote>` narrows the probe to that remote, and the "the fix
/// is on this machine" hint still names the machine `q` is *running on* — it
/// used to name `--machine`'s target, i.e. the one that is already newer.
#[test]
fn doctor_under_machine_probes_only_it_and_still_names_the_local_machine() {
    let mut cmd = doctor(&["claude"]);
    let log = doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host"), ("box", "box-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 9.9.9 (wire 99)" }), MUX_ON),
            "box-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    let assert = cmd
        .args(["--machine", "ws", "doctor", "--json"])
        .assert()
        .success();
    let parsed = json_of(&assert);

    let names: Vec<&str> = parsed["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .filter(|n| n.starts_with("remote "))
        .collect();
    assert_eq!(names, ["remote ws"], "{parsed}");
    let lines = ssh_log_lines(&log);
    assert!(
        lines.iter().all(|l| l.contains("ws-host")),
        "the other remote was probed anyway: {lines:?}"
    );

    let remote = check(&parsed, "remote ws");
    assert_eq!(remote["status"], "warn");
    assert_eq!(remote["fix_hint"], "upgrade `q` on laptop");
}

/// `--machine <this machine>` is valid and means "no remotes" — the same
/// contract `q list` has, and it must still cost no ssh.
#[test]
fn doctor_under_this_machines_own_name_makes_no_ssh_call() {
    let mut cmd = doctor(&["claude"]);
    let log = doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_ON),
        }),
    );
    let assert = cmd
        .args(["--machine", "laptop", "doctor", "--json"])
        .assert()
        .success();
    assert!(ssh_log_lines(&log).is_empty(), "{:?}", ssh_log_lines(&log));
    let parsed = json_of(&assert);
    let names: Vec<&str> = parsed["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(!names.iter().any(|n| n.starts_with("remote ")), "{names:?}");
}

/// The human report keeps doctor's ✓/✗ shape for the remote lines too.
#[test]
fn doctor_human_output_marks_the_remote_checks() {
    let mut cmd = doctor(&["claude"]);
    doctor_remotes(
        &mut cmd,
        &[("ws", "ws-host")],
        serde_json::json!({
            "ws-host": host(serde_json::json!({ "stdout": "q 0.1.0 (wire 1)" }), MUX_OFF),
        }),
    );
    cmd.arg("doctor")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "✓ remote ws q 0.1.0 (wire 1) · ssh ws-host",
        ))
        .stdout(predicate::str::contains("⚠ ssh multiplexing ws"));
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
    install_skill(&cmd);

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
    install_skill(&cmd);
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
    install_skill(&cmd);
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
    // which answers with nothing of its own here.
    env.with_remotes(&[("elsewhere", "elsewhere-host")]);
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "elsewhere-host": { "stdout": "[]" } }),
    );
    let listing = json_of(
        &cmd.args(["--machine", "elsewhere", "list", "--json"])
            .assert()
            .success(),
    );
    assert!(
        listing["quests"].as_array().unwrap().is_empty(),
        "{listing}"
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
fn rename_moves_the_whole_fleet_of_tmux_sessions_and_rows() {
    let env = Env::new();
    env.new_quest("foo");
    // A worker lives in its own tmux session `q-foo+tests` (SPEC §6 v2).
    env.json(&["spawn", "foo", "write the tests", "--label", "tests"]);
    assert!(
        env.fixture()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["session_name"] == "q-foo+tests"),
        "the worker session was not opened"
    );

    let renamed = env.json(&["rename", "foo", "bar"]);
    assert_eq!(renamed["tmux_session"], "q-bar");

    // Both tmux sessions followed the slug.
    let names: Vec<String> = env.fixture()["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["session_name"].as_str().unwrap().to_string())
        .collect();
    assert!(names.contains(&"q-bar".to_string()), "{names:?}");
    assert!(names.contains(&"q-bar+tests".to_string()), "{names:?}");
    assert!(!names.iter().any(|n| n.starts_with("q-foo")), "{names:?}");

    // Both rows were remapped.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-bar'"),
        1
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-bar+tests'"),
        1
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session LIKE 'q-foo%'"),
        0
    );
}

#[test]
fn rename_leaves_a_worker_whose_target_name_is_already_taken() {
    let env = Env::new();
    env.new_quest("foo");
    let spawned = env.json(&["spawn", "foo", "go", "--label", "tests"]);
    let worker_pane = spawned["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();

    // A hand-made `q-bar+tests` already exists: the worker's rename target
    // collides, so its tmux session must be left where it is and its row must
    // NOT be remapped — remapping onto a name another session owns would make
    // the next sweep orphan the live worker (correctness review #1).
    let mut fixture = env.fixture();
    let mut clash = fixture["panes"].as_array().unwrap()[0].clone();
    clash["pane_id"] = serde_json::json!("%900");
    clash["session_name"] = serde_json::json!("q-bar+tests");
    fixture["panes"].as_array_mut().unwrap().push(clash);
    env.write_fixture(fixture);

    let renamed = env.json(&["rename", "foo", "bar"]);
    assert_eq!(renamed["tmux_session"], "q-bar");

    // The master still moved; the colliding worker row kept its old name.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-bar'"),
        1
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-foo+tests'"),
        1
    );
    // The live worker pane still lives under its old session name, reachable.
    let worker = pane_of(&env.fixture(), "q-foo+tests");
    assert_eq!(worker["pane_id"], worker_pane.as_str());

    // F1: the stranded worker is reported on the rename itself, not silently
    // left behind — a `stranded` entry with the tmux session, the reason, and
    // the owning session row id.
    let stranded = renamed["stranded"].as_array().expect("stranded array");
    assert_eq!(stranded.len(), 1, "{renamed}");
    assert_eq!(stranded[0]["tmux_session"], "q-foo+tests");
    assert_eq!(stranded[0]["reason"], "target tmux session already exists");
    let session_id = env
        .conn()
        .query_row(
            "SELECT id FROM session WHERE tmux_session = 'q-foo+tests'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    assert_eq!(stranded[0]["session"], session_id);

    // F1 (doctor): the mismatch the rename left behind is what `q doctor`'s
    // fleet-names check now flags — the worker's row names slug `foo` while its
    // Quest is `bar`, and its pane is still alive under the old name. (The
    // environment checks fail under this bare `Env`, so the report is read
    // without asserting the exit code.)
    let mut doctor_cmd = env.cmd();
    let assert = doctor_cmd.args(["doctor", "--json"]).assert();
    let report = json_of(&assert);
    let fleet = report["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "fleet names")
        .unwrap_or_else(|| panic!("no fleet-names check: {report}"));
    assert_eq!(fleet["status"], "warn", "{report}");
    assert!(
        fleet["detail"].as_str().unwrap().contains("bar/tests"),
        "{report}"
    );

    // F2: closing the Quest tears the stranded worker down too — the R7 union
    // of the prefix scan and every row's recorded tmux_session, so it does not
    // leak under the old slug.
    env.json(&["close", "bar", "-f"]);
    assert!(
        !env.fixture()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["session_name"] == "q-foo+tests"),
        "the stranded worker leaked: {:?}",
        env.fixture()["panes"]
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

/// A Quest made without an epic (`--no-beads`, or the TUI's bare `n`) gets one
/// later with `beads_epic new`, titled from the slug and goal it has by then;
/// a second `new` is refused rather than minting a twin.
#[test]
fn set_beads_epic_new_creates_the_epic_once() {
    let env = Env::new();
    let work = env.work("bare");
    env.cmd()
        .args(["new", "--name", "bare", "--dir", work.to_str().unwrap()])
        .args(["--no-beads", "-d"])
        .assert()
        .success();
    env.json(&["set", "bare", "goal", "ship it"]);

    let mut cmd = env.cmd();
    env.with_bd_create(&mut cmd, "bd-late");
    let out = json_of(
        &cmd.args(["set", "bare", "beads_epic", "new", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["value"], "bd-late");
    assert_eq!(out["quest"]["beads_epic"], "bd-late");
    assert!(
        env.bd_calls()
            .iter()
            .any(|c| c.contains("create bare: ship it --type epic")),
        "{:?}",
        env.bd_calls()
    );

    let mut cmd = env.cmd();
    env.with_bd_create(&mut cmd, "bd-twin");
    cmd.args(["set", "bare", "beads_epic", "new"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already has epic bd-late"));
}

/// The epic is titled `<slug>: <goal>`, so a new goal is written to it too.
#[test]
fn set_goal_retitles_the_epic() {
    let env = Env::new();
    env.quest_with_epic("retitle-me", "bd-epic");
    let mut cmd = env.cmd();
    let ok = env.dir.path().join("bd-retitle");
    std::fs::write(&ok, "ok").unwrap();
    cmd.env("Q_FIXTURE_BD_RETITLE", ok);
    env.with_bd_log(&mut cmd);
    cmd.args(["set", "retitle-me", "goal", "ship it"])
        .assert()
        .success();
    assert!(
        env.bd_calls()
            .iter()
            .any(|c| c.contains("update bd-epic --title retitle-me: ship it")),
        "{:?}",
        env.bd_calls()
    );
}

#[test]
fn set_clears_goal_and_workflow_with_an_empty_value() {
    let env = Env::new();
    env.new_quest("foo");
    env.json(&["set", "foo", "goal", "ship it"]);
    env.json(&["set", "foo", "workflow", "solo"]);

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
    use std::os::unix::process::ExitStatusExt;
    let env = Env::new();
    env.new_quest("piped");
    // The read end is closed before `q` writes, so the write hits a gone
    // reader. `main` restores SIG_DFL for SIGPIPE, so that terminates `q` by
    // signal (the Unix norm) rather than surfacing as an error.
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
    // Killed by SIGPIPE (signal 13 on Linux and macOS), not an error exit,
    // and — the point — nothing on stderr: no "Broken pipe", no panic.
    assert_eq!(
        out.status.signal(),
        Some(13),
        "expected termination by SIGPIPE, got {:?}; stderr:\n{}",
        out.status,
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
    use std::os::unix::process::ExitStatusExt;
    // The initial page is empty, so the first write happens on the second
    // poll — after the reader has gone away. `main` restores SIG_DFL for
    // SIGPIPE, so that write terminates `q` by signal rather than failing.
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
    // Killed by SIGPIPE (signal 13 on Linux and macOS), not an error exit,
    // and nothing on stderr: no "Broken pipe", no panic.
    assert_eq!(
        out.status.signal(),
        Some(13),
        "expected termination by SIGPIPE, got {:?}; stderr:\n{}",
        out.status,
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
fn links_enriches_a_pr_from_the_fixture_and_caches_it() {
    let pane = Pane::new();
    let url = "https://github.com/acme/api/pull/42";
    pane.json(&["link", "add", url]);

    // The fixture stands in for `gh pr view`; `enrich::FixtureFetcher` reads it
    // because `Q_FIXTURE` is already set for the sandbox.
    let gh = pane.env.dir.path().join("gh-pr.json");
    std::fs::write(
        &gh,
        r#"{"title":"Fix the backfill","state":"OPEN","isDraft":false,
            "reviewDecision":"APPROVED",
            "statusCheckRollup":[{"status":"COMPLETED","conclusion":"SUCCESS"}]}"#,
    )
    .unwrap();

    let out = {
        let mut cmd = pane.cmd();
        cmd.env("Q_FIXTURE_GH_PR", &gh);
        let assert = cmd.args(["links", "--json"]).assert().success();
        json_of(&assert)
    };
    assert_eq!(out[0]["title"], "Fix the backfill");
    assert_eq!(out[0]["meta"]["state"], "open");
    assert_eq!(out[0]["meta"]["status"], "approved");
    assert_eq!(out[0]["meta"]["ci"], "passing");
    assert!(out[0]["enriched_at"].is_i64(), "cache stamp written: {out}");

    // Second call is a cache hit: even with the fixture removed the row still
    // reads back enriched rather than reverting to the bare ref.
    std::fs::remove_file(&gh).unwrap();
    let again = pane.json(&["links"]);
    assert_eq!(again[0]["title"], "Fix the backfill");
    assert_eq!(again[0]["meta"]["ci"], "passing");
}

#[test]
fn links_degrades_to_the_bare_ref_when_enrichment_is_unavailable() {
    let pane = Pane::new();
    let url = "https://github.com/acme/api/pull/7";
    pane.json(&["link", "add", url]);
    // No `Q_FIXTURE_GH_PR`, so the fixture fetcher returns nothing: the command
    // still succeeds and shows the bare ref, unenriched.
    let out = pane.json(&["links"]);
    assert_eq!(out[0]["ref"], url);
    assert!(out[0]["title"].is_null(), "no title without a fetch: {out}");
    assert!(out[0]["enriched_at"].is_null(), "no cache stamp: {out}");
}

#[test]
fn link_add_collapses_task_deep_link_and_plain_form_to_one_row() {
    let pane = Pane::new();
    let deep = "https://app.productive.io/1-acme/tasks?filter=1&task/123";
    let plain = "https://app.productive.io/1-acme/tasks/123";
    let canonical = plain;

    let out = pane.json(&["link", "add", deep]);
    assert_eq!(out["created"], true);
    assert_eq!(out["link"]["kind"], "task");
    assert_eq!(out["link"]["ref"], canonical);

    // The plain form of the same task is the existing row, not a new one.
    let out = pane.json(&["link", "add", plain]);
    assert_eq!(out["created"], false);
    assert_eq!(out["link"]["ref"], canonical);

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
fn spawn_opens_a_worker_session_and_records_the_session() {
    let env = Env::new();
    let created = env.new_quest("foo");
    let quest_id = created["quest"]["id"].as_str().unwrap().to_string();
    let cwd = created["quest"]["cwd"].as_str().unwrap().to_string();

    let out = env.json(&["spawn", "foo", "write the tests", "--label", "tests"]);
    assert_eq!(out["quest"]["id"], quest_id.as_str());
    // The worker has its own tmux session `q-<slug>+<label>` (SPEC §6 v2).
    assert_eq!(out["tmux_session"], "q-foo+tests");
    assert_eq!(out["attach"], "none");
    assert_eq!(out["launched"], true);
    assert_eq!(out["session"]["role"], "worker");
    assert_eq!(out["session"]["label"], "tests");
    assert_eq!(out["session"]["status"], "starting");
    assert_eq!(out["session"]["first_prompt"], "write the tests");
    assert_eq!(out["session"]["tmux_session"], "q-foo+tests");
    let session_id = out["session"]["id"].as_str().unwrap().to_string();

    // The worker's own session holds one pane; the main session is untouched.
    let fixture = env.fixture();
    assert_eq!(fixture["attached"], serde_json::Value::Null);
    let pane = pane_of(&fixture, "q-foo+tests");
    assert_eq!(pane["window_index"], 0);
    assert_eq!(pane_of(&fixture, "q-foo")["window_index"], 0);
    assert_eq!(out["session"]["tmux_pane"], pane["pane_id"]);
    assert_ne!(pane["pane_id"], created["session"]["tmux_pane"]);

    // Same env as the master, but `Q_ROLE=worker` and its own `Q_SESSION`.
    assert_eq!(pane["env"]["Q_QUEST"], quest_id.as_str());
    assert_eq!(pane["env"]["Q_SESSION"], session_id.as_str());
    assert_eq!(pane["env"]["Q_ROLE"], "worker");
    assert_eq!(
        pane["env"]["Q_MACHINE"],
        pane_of(&fixture, "q-foo")["env"]["Q_MACHINE"]
    );
    assert!(pane["env"]["Q_DB"].is_string());
    assert!(pane["env"]["Q_CONFIG"].is_string());
    // The pane runs the login shell; Claude is typed into it, named after the
    // label (SPEC §6 v2).
    assert_eq!(pane["command"], serde_json::Value::Null);
    assert_eq!(pane["buffer"], "claude -n foo/tests -- 'write the tests'\n");
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
    assert_eq!(payload["tmux_session"], "q-foo+tests");
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
        .stdout(predicate::str::contains("tmux q-foo+tests"))
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
    // Auto workers carry the number in their label now (SPEC §6 v2 — no windows).
    assert_eq!(env.json(&["spawn", "foo"])["session"]["label"], "w1");
    assert_eq!(env.json(&["spawn", "foo"])["session"]["label"], "w2");

    // The first worker's whole tmux session disappears; the sweep ends its row,
    // which frees the label — but not the number.
    let mut fixture = env.fixture();
    let panes: Vec<serde_json::Value> = fixture["panes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["session_name"] != "q-foo+w1")
        .cloned()
        .collect();
    fixture["panes"] = serde_json::json!(panes);
    env.write_fixture(fixture);

    let third = env.json(&["spawn", "foo"]);
    assert_eq!(third["session"]["label"], "w3");
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE label = 'w1' AND status = 'ended'"),
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
fn spawn_validates_the_label_and_a_blank_prompt_is_a_bare_worker() {
    let env = Env::new();
    env.new_quest("foo");
    for bad in ["Tests", "with space", "under_score", "double--dash"] {
        env.cmd()
            .args(["spawn", "foo", "go", "--label", bad])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("invalid label"));
    }
    // A rejected label opened nothing: only the master row is there.
    assert_eq!(env.count("SELECT count(*) FROM session"), 1);

    // A blank prompt is no error now — it is a bare interactive worker with no
    // first prompt (SPEC §6).
    let out = env.json(&["spawn", "foo", "   ", "--label", "tests"]);
    assert!(out["session"]["first_prompt"].is_null());
    assert_eq!(env.count("SELECT count(*) FROM session"), 2);
}

#[test]
fn spawn_without_a_label_or_prompt_makes_an_auto_bare_worker() {
    let env = Env::new();
    env.new_quest("foo");
    // No `--label`, no prompt: the label is `w<n>`, the worker gets its own
    // `q-foo+w1` session, and Claude is typed in with no `--` prompt.
    let out = env.json(&["spawn", "foo"]);
    assert_eq!(out["session"]["label"], "w1");
    assert_eq!(out["tmux_session"], "q-foo+w1");
    assert!(out["session"]["first_prompt"].is_null());
    let buffer = pane_of(&env.fixture(), "q-foo+w1")["buffer"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(buffer.contains("claude -n foo/w1"), "{buffer}");
    assert!(!buffer.contains(" -- "), "{buffer}");

    // The next auto worker is `w2`.
    let out = env.json(&["spawn", "foo"]);
    assert_eq!(out["session"]["label"], "w2");
}

#[test]
fn spawn_here_reads_the_quest_from_the_pane_and_spawns_a_bare_worker() {
    let env = Env::new();
    env.new_quest("foo");
    let master_pane = window_of(&env.fixture(), "q-foo", "master")["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    // A pane in the Quest opens a bare worker in its own session and lands on it.
    env.cmd()
        .args(["spawn-here", &master_pane])
        .assert()
        .success()
        .stdout(predicate::str::contains("spawned w1 in foo"));
    assert_eq!(env.count("SELECT count(*) FROM session"), 2);
    assert_eq!(
        env.fixture()["selected"],
        pane_of(&env.fixture(), "q-foo+w1")["pane_id"]
    );

    // A pane that is not any Quest's is a friendly no-op — nothing spawned.
    env.cmd()
        .args(["spawn-here", "%999"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not in a Quest"));
    assert_eq!(env.count("SELECT count(*) FROM session"), 2);
}

#[test]
fn spawn_here_honors_json() {
    let env = Env::new();
    env.new_quest("foo");
    let master_pane = window_of(&env.fixture(), "q-foo", "master")["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = env.json(&["spawn-here", &master_pane]);
    assert_eq!(out["spawned"], true);
    assert_eq!(out["quest"]["slug"], "foo");
    assert_eq!(out["session"]["label"], "w1");

    // The no-op case is JSON too, not a bare println.
    let out = env.json(&["spawn-here", "%999"]);
    assert_eq!(out["spawned"], false);
}

#[test]
fn a_master_leaves_a_user_prefix_binding_alone() {
    let env = Env::new();
    // The user has bound prefix+N to something of their own.
    env.write_fixture(serde_json::json!({
        "prefix_keys": { "N": "next-window" }
    }));
    env.new_quest("foo");
    // The master did not clobber it, so prefix+N is not wired to spawn-here.
    assert_eq!(env.fixture()["prefix_keys"]["N"], "next-window");
}

#[test]
fn a_master_claims_a_free_prefix_key() {
    let env = Env::new();
    env.new_quest("foo");
    // Nothing held prefix+N, so the master took it for spawn-here.
    let bound = env.fixture()["prefix_keys"]["N"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(bound.contains("spawn-here"), "{bound}");
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
        pane_of(&env.fixture(), "q-foo+tests")["cwd"],
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
        pane_of(&env.fixture(), "q-foo+migration")["cwd"],
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
fn spawn_enters_the_worker_session_only_with_enter() {
    let env = Env::new();
    env.new_quest("foo");

    // Default: the worker session is opened, but no client is moved — a worker
    // is its own tmux session (SPEC §6 v2), so there is no window to select.
    assert_eq!(
        env.json(&["spawn", "foo", "a", "--label", "plain"])["attach"],
        "none"
    );
    assert_eq!(env.fixture()["selected"], serde_json::Value::Null);
    assert_eq!(env.fixture()["attached"], serde_json::Value::Null);

    // `--enter` inside tmux switches the client to the worker's own session.
    let mut cmd = env.cmd();
    let assert = cmd
        .env("TMUX", "/tmp/tmux-0/default,1,0")
        .args(["spawn", "foo", "b", "--label", "here", "--enter", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["attach"], "switch");
    let worker_pane = out["session"]["tmux_pane"].as_str().unwrap().to_string();
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo+here", worker_pane.as_str()])
    );
    assert_eq!(env.fixture()["selected"], worker_pane.as_str());
}

#[test]
fn spawn_shell_opens_a_bare_shell_off_without_claude() {
    let env = Env::new();
    env.new_quest("foo");
    let out = env.json(&["spawn", "foo", "--label", "review", "--shell"]);
    assert_eq!(out["tmux_session"], "q-foo+review");
    assert_eq!(out["launched"], false);
    assert_eq!(out["session"]["status"], "off");
    // The pane is a login shell and nothing was typed into it — no Claude.
    let pane = pane_of(&env.fixture(), "q-foo+review");
    assert_eq!(pane["command"], serde_json::Value::Null);
    assert_eq!(pane["buffer"], "");
    assert_eq!(pane["current_command"], "zsh");
    // The one-liner says it is a shell only.
    env.cmd()
        .args(["spawn", "foo", "--label", "other", "--shell"])
        .assert()
        .success()
        .stdout(predicate::str::contains("shell only"));
}

#[test]
fn enter_reaches_a_worker_by_label_through_its_own_session() {
    let env = Env::new();
    env.new_quest("foo");
    let spawned = env.json(&["spawn", "foo", "write the tests", "--label", "tests"]);
    let worker_pane = spawned["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();

    // The command `q spawn` printed has to work: the worker lives in its own
    // tmux session `q-foo+tests`, addressed by pane id.
    let entered = env.json(&["enter", "foo", "--session", "tests"]);
    assert_eq!(entered["tmux_session"], "q-foo+tests");
    assert_eq!(entered["window"], "tests");
    assert_eq!(entered["session"]["id"], spawned["session"]["id"]);
    assert_eq!(entered["attach"], "exec");
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo+tests", worker_pane.as_str()])
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
        serde_json::json!(["q-foo+tests", worker_pane.as_str()])
    );

    // Without `--session` it is still the master.
    let master = env.json(&["enter", "foo"]);
    assert_eq!(master["window"], "master");
    assert_eq!(master["tmux_session"], "q-foo");
}

#[test]
fn enter_reaches_a_live_worker_when_the_main_session_is_gone() {
    let env = Env::new();
    env.new_quest("foo");
    let spawned = env.json(&["spawn", "foo", "write the tests", "--label", "tests"]);
    let worker_pane = spawned["session"]["tmux_pane"]
        .as_str()
        .unwrap()
        .to_string();

    // Drop the main tmux session `q-foo`, keeping the worker's own alive: this
    // is exactly the state `q resume` re-adopts a live worker in. `q enter` must
    // gate on the worker's own session, not the (now absent) main (SPEC §6 v2).
    let mut fixture = env.fixture();
    fixture["panes"]
        .as_array_mut()
        .unwrap()
        .retain(|p| p["session_name"] != "q-foo");
    env.write_fixture(fixture);

    let entered = env.json(&["enter", "foo", "--session", "tests"]);
    assert_eq!(entered["tmux_session"], "q-foo+tests");
    assert_eq!(entered["session"]["id"], spawned["session"]["id"]);
    assert_eq!(
        env.fixture()["attached"],
        serde_json::json!(["q-foo+tests", worker_pane.as_str()])
    );

    // The master, whose session is gone, points at `q resume`.
    env.cmd()
        .args(["enter", "foo"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("q resume foo"));
}

#[test]
fn a_spawn_whose_session_never_opens_leaves_no_session_behind() {
    let env = Env::new();
    env.new_quest("foo");
    let mut fixture = env.fixture();
    fixture["fail_new_session"] = serde_json::json!("no space left for sessions");
    env.write_fixture(fixture);

    env.cmd()
        .args(["spawn", "foo", "go", "--label", "tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("no space left for sessions"));

    // The row is inserted before the session (the `SessionStart` hook resolves
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
    fixture.as_object_mut().unwrap().remove("fail_new_session");
    env.write_fixture(fixture);
    let out = env.json(&["spawn", "foo", "go", "--label", "tests"]);
    assert_eq!(out["tmux_session"], "q-foo+tests");
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
    for flag in ["--label", "--workflow", "--dir", "--shell", "--enter"] {
        assert!(
            out.contains(flag),
            "`{flag}` missing from `q spawn --help`:\n{out}"
        );
    }
}

// ------------------------------- q start / stop / prompt (bd-v1d.3, SPEC §6 v2)

#[test]
fn start_launches_claude_in_a_shell_pane_and_refuses_a_non_shell_one() {
    let env = Env::new();
    env.new_quest("alpha");
    let w = env.json(&["spawn", "alpha", "--label", "w1", "--shell"]);
    assert_eq!(w["session"]["status"], "off");

    let started = env.json(&["start", "alpha/w1"]);
    assert_eq!(started["status"], "starting");
    let buffer = pane_of(&env.fixture(), "q-alpha+w1")["buffer"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(buffer.contains("claude -n alpha/w1"), "{buffer}");
    assert_eq!(
        env.status_of(w["session"]["id"].as_str().unwrap()),
        "starting"
    );

    // The row is already `starting`, so a second `q start` is refused on status
    // before it can double-type `claude` into the boot window (correctness #2).
    env.cmd()
        .args(["start", "alpha/w1"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("already up or starting"));
    // …unless forced.
    env.cmd()
        .args(["start", "alpha/w1", "--force"])
        .assert()
        .success();
}

#[test]
fn start_refuses_a_non_shell_pane_on_an_off_row() {
    let env = Env::new();
    env.new_quest("alpha");
    let w = env.json(&["spawn", "alpha", "--label", "w1", "--shell"]);
    let pane = w["session"]["tmux_pane"].as_str().unwrap().to_string();

    // The row is `off`, so the status gate passes; but the pane is running
    // something that is not a shell (a vim, a build), so `launch` refuses.
    let mut fixture = env.fixture();
    for p in fixture["panes"].as_array_mut().unwrap() {
        if p["pane_id"] == pane.as_str() {
            p["current_command"] = serde_json::json!("vim");
        }
    }
    env.write_fixture(fixture);

    env.cmd()
        .args(["start", "alpha/w1"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a shell"));
    env.cmd()
        .args(["start", "alpha/w1", "--force"])
        .assert()
        .success();
}

#[test]
fn start_uses_the_stored_prompt_and_an_override_updates_it() {
    let env = Env::new();
    env.new_quest("alpha");
    env.json(&["spawn", "alpha", "do the thing", "--label", "w1", "--shell"]);

    // No prompt arg: the stored first prompt is embedded.
    let started = env.json(&["start", "alpha/w1"]);
    assert!(
        started["command"]
            .as_str()
            .unwrap()
            .contains("-- 'do the thing'"),
        "{started}"
    );

    // Stop, then re-start with an override: the stored prompt is replaced.
    let out = env.json(&["start", "alpha/w1", "other thing", "--force"]);
    assert!(
        out["command"]
            .as_str()
            .unwrap()
            .contains("-- 'other thing'")
    );
    assert_eq!(env.json(&["prompt", "alpha/w1"])["prompt"], "other thing");
}

#[test]
fn prompt_prints_the_stored_first_prompt() {
    let env = Env::new();
    env.new_quest("alpha");
    env.json(&["spawn", "alpha", "write the tests", "--label", "tests"]);
    env.cmd()
        .args(["prompt", "alpha/tests"])
        .assert()
        .success()
        .stdout(predicate::str::contains("write the tests"));
    assert_eq!(
        env.json(&["prompt", "alpha/tests"])["prompt"],
        "write the tests"
    );
}

#[test]
fn stop_types_exit_and_is_idle_gated() {
    let fleet = Fleet::new();
    let env = &fleet.env;

    // Busy: refused (nothing typed). The pane is still running Claude, so the
    // sweep does not touch the row.
    env.set_status(&fleet.worker_id, "busy", None);
    env.cmd()
        .args(["stop", "alpha/tests"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not idle"));
    assert_eq!(env.buffer(&fleet.worker_pane), "");

    // Idle: `/exit` goes in.
    env.set_status(&fleet.worker_id, "idle", None);
    env.json(&["stop", "alpha/tests"]);
    assert_eq!(env.buffer(&fleet.worker_pane), "/exit\n");
}

#[test]
fn stop_clears_a_stray_input_line_before_typing_exit() {
    let fleet = Fleet::new();
    let env = &fleet.env;
    env.set_status(&fleet.worker_id, "idle", None);

    // The user typed a few chars into Claude and walked away (no Enter, so no
    // trailing newline). `/exit` appended to it would submit `half typed/exit`
    // as an ordinary message and Claude would never leave (correctness #4).
    let mut fixture = env.fixture();
    for pane in fixture["panes"].as_array_mut().unwrap() {
        if pane["pane_id"] == fleet.worker_pane.as_str() {
            pane["buffer"] = serde_json::json!("half typed");
        }
    }
    env.write_fixture(fixture);

    env.json(&["stop", "alpha/tests"]);
    // The `C-u` killed the stray line, so `/exit` lands alone at column 0.
    assert_eq!(env.buffer(&fleet.worker_pane), "/exit\n");
}

#[test]
fn send_refuses_an_off_session_unless_shell() {
    let env = Env::new();
    env.new_quest("alpha");
    let w = env.json(&["spawn", "alpha", "--label", "w1", "--shell"]);
    let pane = w["session"]["tmux_pane"].as_str().unwrap().to_string();

    // Off: a plain send would land in zsh, so it is refused.
    env.cmd()
        .args(["send", "alpha/w1", "ls -la"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("off"));
    assert_eq!(env.buffer(&pane), "");

    // `--shell` types into the shell.
    env.json(&["send", "alpha/w1", "ls -la", "--shell"]);
    assert_eq!(env.buffer(&pane), "ls -la\n");
}

#[test]
fn close_kills_the_whole_fleet_including_a_rowless_pane() {
    let env = Env::new();
    env.new_quest("alpha");
    env.json(&["spawn", "alpha", "go", "--label", "tests"]);
    // A worker pane with no session row — a crash between insert and pane.
    let mut fixture = env.fixture();
    fixture["panes"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "pane_id": "%99",
            "session_name": "q-alpha+ghost",
            "window_name": "ghost",
            "window_index": 0,
            "current_command": "zsh",
        }));
    env.write_fixture(fixture);

    env.json(&["close", "alpha", "-f"]);
    // Every `q-alpha*` session is gone, the ghost included; a sibling is safe.
    let sessions: Vec<String> = env.fixture()["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["session_name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !sessions.iter().any(|s| s.starts_with("q-alpha")),
        "{sessions:?}"
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE status != 'ended'"),
        0
    );
}

#[test]
fn rm_force_kills_the_fleet_when_the_main_is_already_gone() {
    let env = Env::new();
    env.new_quest("alpha");
    env.json(&["spawn", "alpha", "go", "--label", "tests"]);
    // The main tmux session vanished; only the worker's own session is left.
    let mut fixture = env.fixture();
    let panes: Vec<serde_json::Value> = fixture["panes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["session_name"] != "q-alpha")
        .cloned()
        .collect();
    fixture["panes"] = serde_json::json!(panes);
    env.write_fixture(fixture);

    env.json(&["rm", "alpha", "-f"]);
    // The worker session was killed too, and the Quest is gone.
    assert!(
        env.fixture()["panes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| !p["session_name"].as_str().unwrap().starts_with("q-alpha")),
        "{:?}",
        env.fixture()["panes"]
    );
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
}

#[test]
fn resume_readopts_a_worker_when_the_main_is_gone() {
    let env = Env::new();
    env.new_quest("alpha");
    let worker = env.json(&["spawn", "alpha", "go", "--label", "tests"]);
    let worker_id = worker["session"]["id"].as_str().unwrap().to_string();

    // The main tmux session is gone; the worker's own session lives on.
    let mut fixture = env.fixture();
    let panes: Vec<serde_json::Value> = fixture["panes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["session_name"] != "q-alpha")
        .cloned()
        .collect();
    fixture["panes"] = serde_json::json!(panes);
    env.write_fixture(fixture);

    // Resume brings up a fresh master; the worker row is re-adopted, not ended.
    let out = env.json(&["resume", "alpha", "-d"]);
    assert_eq!(out["tmux_session"], "q-alpha");
    assert_ne!(
        env.status_of(&worker_id),
        "ended",
        "the worker was re-adopted"
    );
    // A fresh master session exists again, plus the surviving worker.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE role = 'master' AND status != 'ended'"),
        1
    );
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

    /// Empty a pane's send-keys buffer in the fixture. `q new`/`q spawn` now
    /// type the `claude …` launch command into the pane (SPEC §6 v2); a test
    /// about a *later* send starts from the fresh pane Claude left behind.
    fn clear_buffer(&self, pane_id: &str) {
        let mut fixture = self.fixture();
        for pane in fixture["panes"].as_array_mut().unwrap() {
            if pane["pane_id"] == pane_id {
                pane["buffer"] = serde_json::json!("");
            }
        }
        self.write_fixture(fixture);
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
        let master_pane = created["session"]["tmux_pane"]
            .as_str()
            .unwrap()
            .to_string();
        let worker = env.json(&["spawn", "alpha", "write the tests", "--label", "tests"]);
        let worker_id = worker["session"]["id"].as_str().unwrap().to_string();
        let worker_pane = worker["session"]["tmux_pane"].as_str().unwrap().to_string();
        // Claude came up in both sessions: `SessionStart` would say idle, and it
        // took over the pane — so the launch command it was typed with is behind
        // it; a test about a later send starts from a fresh buffer.
        env.set_status(&master_id, "idle", Some(1001));
        env.set_status(&worker_id, "idle", Some(1002));
        env.clear_buffer(&master_pane);
        env.clear_buffer(&worker_pane);
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

/// v2 (SPEC §6): an `off` row is a live shell with no Claude — `q reset` has
/// nothing to reset, so it skips (scheduled) or refuses (manual), and the
/// status surfaces verbatim in `q sessions`.
#[test]
fn an_off_session_renders_as_off_and_is_never_reset() {
    let fleet = Fleet::new();
    fleet.env.set_status(&fleet.master_id, "off", None);

    let rows = fleet.env.json(&["sessions"]);
    let master = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["quest_slug"] == "alpha" && r["label"] == "master")
        .unwrap();
    assert_eq!(master["status"], "off");

    // The scheduled path skips and logs; the manual path is a conflict.
    let out = fleet.env.json(&["reset", "alpha/master", "--delay", "0"]);
    assert_eq!(out["action"], "skipped");
    assert!(out["reason"].as_str().unwrap().contains("off"), "{out}");
    let err = fleet.env.json_err(&["reset", "alpha/master"]);
    assert_eq!(err["code"], "conflict");
    assert!(err["error"].as_str().unwrap().contains("off"), "{err}");
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

    // The whole fleet followed (SPEC §6 v2): the main row to `q-cdc-backfill`
    // and the worker row to its own `q-cdc-backfill+tests`.
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-cdc-backfill'"),
        1
    );
    assert_eq!(
        env.count("SELECT count(*) FROM session WHERE tmux_session = 'q-cdc-backfill+tests'"),
        1
    );
    let sessions: Vec<String> = env.fixture()["panes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["session_name"].as_str().unwrap().to_string())
        .collect();
    assert!(
        sessions.contains(&"q-cdc-backfill".to_string()),
        "{sessions:?}"
    );
    assert!(
        sessions.contains(&"q-cdc-backfill+tests".to_string()),
        "{sessions:?}"
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

/// A real `q list --json --all` from a second, independent sandbox running its
/// own config — the exact envelope that machine would send back, `machines`
/// entry and tmux prefix included.
fn far_listing(config: &str, quests: &[(&str, bool)]) -> String {
    let far = Env::new();
    far.write_config(config);
    for (slug, finished) in quests {
        let work = far.work(slug);
        far.cmd()
            .args(["new", "--name", slug, "--no-beads", "-d", "--json"])
            .args(["--dir", work.to_str().unwrap()])
            .assert()
            .success();
        if *finished {
            far.cmd().args(["close", slug, "-f"]).assert().success();
        }
    }
    let assert = far
        .cmd()
        .args(["list", "--json", "--all"])
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    assert_eq!(
        parsed["quests"].as_array().unwrap().len(),
        quests.len(),
        "{parsed}"
    );
    out.trim().to_string()
}

/// The slugs of a listing envelope, in the order the table shows them.
fn slugs_of(listing: &serde_json::Value) -> Vec<String> {
    listing["quests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap().to_string())
        .collect()
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
    assert!(calls.contains(&"ws-host\tq\tlist\t--json\t--no-remote\t--all".to_string()));
    assert!(calls.contains(&"box-host\tq\tlist\t--json\t--no-remote\t--all".to_string()));
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

/// A host that answers is not unreachable, whatever it answers: the shell got
/// as far as looking for `q`, so what is wrong is that machine's `q`
/// (bd-8lz.5.4). `q doctor` is where that becomes a fix line.
#[test]
fn a_remote_without_q_installed_is_incompatible_with_its_own_message() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let assert = env
        .list(serde_json::json!({
            "ws-host": { "exit": 127, "stderr": "bash: q: command not found" }
        }))
        .success();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("⚠ incompatible"), "{stderr}");
    assert!(stderr.contains("no `q` on PATH"), "{stderr}");
    assert!(stderr.contains("command not found"), "{stderr}");
}

/// ssh's own failure code, on the other hand, says nothing came back at all.
#[test]
fn a_host_ssh_could_not_reach_is_unreachable() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let assert = env
        .list(serde_json::json!({
            "ws-host": { "exit": 255, "stderr": "ssh: Could not resolve hostname ws-host" }
        }))
        .success();
    let stderr = stderr_of(&assert);
    assert!(stderr.contains("⚠ unreachable"), "{stderr}");
    assert!(stderr.contains("Could not resolve"), "{stderr}");
}

/// SPEC §16's listing filters deliberately do NOT travel: there is one cache
/// row per remote and it has to serve every invocation, so the wire request is
/// always the whole listing and the filters are applied on arrival.
#[test]
fn the_remote_is_always_asked_for_the_whole_listing() {
    let env = Env::new();
    for args in [
        vec!["list", "--json"],
        vec!["list", "--json", "--all"],
        vec!["list", "--json", "--state", "finished"],
        vec!["list", "--json", "--all", "--state", "idle"],
    ] {
        let env2 = Env::new();
        env2.with_remotes(&[("ws", "ws-host")]);
        let mut cmd = env2.cmd();
        env2.with_ssh(
            &mut cmd,
            serde_json::json!({ "ws-host": { "stdout": "[]" } }),
        );
        cmd.args(&args).assert().success();
        assert_eq!(
            env2.ssh_calls(),
            ["ws-host\tq\tlist\t--json\t--no-remote\t--all"],
            "{args:?}"
        );
    }
    let _ = env;
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

    // Quoted, because ssh hands the words to the far end's LOGIN SHELL before
    // tmux ever sees them: zsh reads a bare `=q-over-there` as an equals
    // expansion, fails it, and aborts the line with tmux unrun.
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t'=q-over-there'"]
    );
    // …while the argv `--json` reports is the command as `q` means it, with
    // nothing a consumer would have to un-quote.
    assert_eq!(
        out["argv"],
        serde_json::json!(["tmux", "attach", "-t", "=q-over-there"])
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
        ["attach\tws-host\ttmux\t-CC\tattach\t-t\t'=q-over-there'"]
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
        ["attach\tws-host\ttmux\tattach\t-t\t'=q-over-there'"]
    );
}

/// A local Quest is entered locally, and a name that is on neither machine is
/// still a plain "not found" rather than a story about ssh.
///
/// A target that is *exact* here and uncontested in the cache is entered
/// without dialling out at all (see
/// `an_uncontested_cache_enters_a_local_quest_with_no_ssh`); anything else —
/// a fragment, a typo — is resolved across machines, because an id or a slug is
/// unique only per machine. What must not happen either way is an attach over
/// ssh for a Quest that lives here.
#[test]
fn entering_looks_across_machines_and_reports_a_typo_as_a_typo() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } }),
    );
    let entered = json_of(&cmd.args(["enter", "here", "--json"]).assert().success());
    assert_eq!(entered["tmux_session"], "q-here");
    assert_eq!(entered["attach"], "exec");
    // Cold cache, exact local hit: nothing to be suspicious of, so no ssh runs.
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
    // A target that is not exact here *is* asked across machines before it is
    // refused: an exact slug on `ws` would have beaten a local fragment (S1).
    assert_eq!(
        env.ssh_calls(),
        ["ws-host\tq\tlist\t--json\t--no-remote\t--all"]
    );
    assert!(attach_calls(&env).is_empty());
}

/// `q enter` is the most-used command, and with `[[remotes]]` configured the
/// cross-machine ladder made every one of them pay a fan-out round. The cache
/// is consulted instead: it already holds the other machine's listing, and when
/// nothing in it could mean this target, the local attach costs no ssh at all.
#[test]
fn an_uncontested_cache_enters_a_local_quest_with_no_ssh() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");
    let payload = remote_listing("ws", "over-there");

    // One `q list` fills the cache — the round every enter used to repeat.
    env.list(serde_json::json!({ "ws-host": { "stdout": payload.clone() } }))
        .success();
    assert_eq!(env.ssh_calls().len(), 1);
    std::fs::remove_file(env.ssh_log()).unwrap();

    // The remote is still scripted and healthy; it is simply not asked.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let entered = json_of(&cmd.args(["enter", "here", "--json"]).assert().success());
    assert_eq!(entered["tmux_session"], "q-here");
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());
}

/// With no `[[remotes]]` at all there is nothing to ask, so the ladder stays
/// the single database read it always was.
#[test]
fn entering_without_remotes_never_leaves_this_machine() {
    let env = Env::new();
    env.write_config("[machine]\nname = \"laptop\"\n");
    env.new_quest("here");

    let mut cmd = env.cmd();
    env.with_ssh(&mut cmd, serde_json::json!({}));
    let entered = json_of(&cmd.args(["enter", "here", "--json"]).assert().success());
    assert_eq!(entered["tmux_session"], "q-here");
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());
}

/// A Quest id is 16 bits and unique only per machine, so the same id can name
/// a different Quest on each. Entering the local one without a word is the
/// guess SPEC §16 refuses everywhere else — and the candidates say which
/// machine each is on, so `on ws` cannot be read as covering both.
///
/// The cache is what raises the suspicion, and the error is then built from a
/// **live** round rather than from the cached rows: it is a refusal to act, so
/// it is made on fresh data. The accepted cost of not asking on every enter is
/// pinned first, in `a_collision_the_cache_has_never_seen_is_entered_locally`.
#[test]
fn an_id_that_is_exact_on_both_machines_is_ambiguous() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let id = env.new_quest("here")["quest"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // The far end's listing, with its row forced onto the same id.
    let payload = remote_listing("ws", "over-there");
    let mut far: serde_json::Value = serde_json::from_str(&payload).unwrap();
    far["quests"][0]["id"] = serde_json::json!(id);
    let payload = far.to_string();

    // The cache learns about `ws` — one `q list`, or any TUI tick.
    env.list(serde_json::json!({ "ws-host": { "stdout": payload.clone() } }))
        .success();
    std::fs::remove_file(env.ssh_log()).unwrap();

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let assert = cmd.args(["enter", &id, "--json"]).assert().code(1);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("ambiguous"), "{err}");
    assert!(err.contains("(here) on laptop"), "{err}");
    assert!(err.contains("(over-there) on ws"), "{err}");
    // Fresh rows, not the cached ones the suspicion came from.
    assert_eq!(
        env.ssh_calls(),
        ["ws-host\tq\tlist\t--json\t--no-remote\t--all"]
    );
    assert!(attach_calls(&env).is_empty());
}

/// The trade-off, stated as a test so nobody discovers it in the field: with a
/// cache that has never seen the colliding remote Quest, `q enter <id>` takes
/// the local one and says nothing. Closing that means an ssh on **every**
/// `q enter`, which is the cost cache-first exists to avoid — and one `q list`
/// is enough to turn the same command into the ambiguity error above.
#[test]
fn a_collision_the_cache_has_never_seen_is_entered_locally() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let id = env.new_quest("here")["quest"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let payload = remote_listing("ws", "over-there");
    let mut far: serde_json::Value = serde_json::from_str(&payload).unwrap();
    far["quests"][0]["id"] = serde_json::json!(id);
    let payload = far.to_string();

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload.clone() } }),
    );
    let entered = json_of(&cmd.args(["enter", &id, "--json"]).assert().success());
    assert_eq!(entered["tmux_session"], "q-here");
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());

    // …and the very next round teaches the cache, after which it is ambiguous.
    env.list(serde_json::json!({ "ws-host": { "stdout": payload.clone() } }))
        .success();
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let assert = cmd.args(["enter", &id, "--json"]).assert().code(1);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("ambiguous"), "{err}");
}

/// `--machine` scopes `q enter` as it scopes `q list`: pinned to a remote, a
/// local Quest is not a candidate at all, and the refusal names the machine it
/// looked on rather than attaching here and claiming to have gone there.
#[test]
fn entering_honours_the_machine_flag() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("local-alpha");
    let payload = remote_listing("ws", "over-there");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload.clone() } }),
    );
    let assert = cmd
        .args(["enter", "local-alpha", "--machine", "ws", "--json"])
        .assert()
        .code(1);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("not_found"), "{err}");
    assert!(err.contains("`local-alpha` on ws"), "{err}");
    assert!(attach_calls(&env).is_empty(), "{:?}", env.ssh_calls());

    // The remote Quest is still entered through the same flag.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload.clone() } }),
    );
    let out = json_of(
        &cmd.args(["enter", "over-there", "--machine", "ws", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["machine"], "ws");

    // Pinned the other way: no ssh, and a remote Quest is out of reach.
    std::fs::remove_file(env.ssh_log()).unwrap();
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let assert = cmd
        .args(["enter", "over-there", "--machine", "laptop", "--json"])
        .assert()
        .code(1);
    let err = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(err.contains("`over-there` on laptop"), "{err}");
    assert!(env.ssh_calls().is_empty(), "{:?}", env.ssh_calls());
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

/// The cache is one row per remote, holding whatever the last successful round
/// returned — and `q enter` and every TUI round ask for `--all`. Replaying that
/// row under a plain `q list` used to leak finished remote Quests into the
/// default listing, and to answer `--state active` with a finished row.
///
/// bd-8lz.5.1's standing constraint: the merge must not filter remote rows
/// differently from local ones.
#[test]
fn a_stale_remotes_rows_are_filtered_by_this_invocations_flags() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("here");
    let payload = far_listing(
        "[machine]\nname = \"ws\"\n",
        &[("live-there", false), ("done-there", true)],
    );

    // One `--all` round fills the cache — as `q enter` and every TUI tick do.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    cmd.args(["list", "--json", "--all"]).assert().success();

    // Machine now down. The cached rows stand in, filtered by THIS invocation.
    let down = serde_json::json!({ "ws-host": { "timeout": true } });
    let listing = env.listing(down.clone());
    assert!(
        !slugs_of(&listing).contains(&"done-there".to_string()),
        "a finished remote Quest leaked into the default listing: {listing}"
    );
    assert!(slugs_of(&listing).contains(&"live-there".to_string()));
    assert_eq!(
        Env::row_of(&listing, "live-there")["source"],
        serde_json::json!({ "kind": "remote", "stale": true })
    );

    // `--state` is exact for a cached remote row, exactly as it is for a
    // local one.
    for (state, want) in [("active", "live-there"), ("finished", "done-there")] {
        let mut cmd = env.cmd();
        env.with_ssh(&mut cmd, down.clone());
        let listing = json_of(
            &cmd.args(["list", "--json", "--state", state])
                .assert()
                .success(),
        );
        let slugs = slugs_of(&listing);
        assert!(
            slugs.contains(&want.to_string()),
            "--state {state}: {listing}"
        );
        for row in listing["quests"].as_array().unwrap() {
            assert_eq!(row["display_state"], state, "--state {state}: {listing}");
        }
    }

    // …and `--all` still shows both.
    let mut cmd = env.cmd();
    env.with_ssh(&mut cmd, down);
    let listing = json_of(&cmd.args(["list", "--json", "--all"]).assert().success());
    let slugs = slugs_of(&listing);
    assert!(slugs.contains(&"live-there".to_string()), "{listing}");
    assert!(slugs.contains(&"done-there".to_string()), "{listing}");
}

/// The same rule for rows that came off the wire this round.
#[test]
fn a_fresh_remotes_rows_are_filtered_like_local_ones() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let payload = far_listing(
        "[machine]\nname = \"ws\"\n",
        &[("live-there", false), ("done-there", true)],
    );
    let listing = env.listing(serde_json::json!({ "ws-host": { "stdout": payload } }));
    assert_eq!(slugs_of(&listing), ["live-there"], "{listing}");
}

/// `machines[].quests` counts the rows that claim that machine, not the bucket
/// they arrived in — an envelope whose counts contradict its own rows is worse
/// than either answer.
#[test]
fn the_machines_array_counts_the_rows_that_claim_each_machine() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    // A *local* Quest filed under a remote's name. `q --machine ws new` used
    // to make one of these (the bd-8lz.5.3 bug — it now creates on `ws`), but
    // the row is still reachable: a `[[remotes]]` renamed after the fact
    // leaves exactly this behind, and the envelope has to add up either way.
    env.new_quest("mislabelled");
    env.conn()
        .execute(
            "UPDATE quest SET machine = 'ws' WHERE slug = 'mislabelled'",
            [],
        )
        .unwrap();
    let payload = remote_listing("ws", "over-there");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let listing = json_of(
        &cmd.args(["list", "--json", "--machine", "ws"])
            .assert()
            .success(),
    );
    let rows = listing["quests"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{listing}");
    assert!(rows.iter().all(|r| r["machine"] == "ws"), "{listing}");
    assert_eq!(Env::machine_of(&listing, "ws")["quests"], 2, "{listing}");
}

/// `--machine <remote>` says "that machine only" and `--no-remote` says "no
/// machine but this one". Together they can only produce an empty answer, and
/// an empty answer about a machine reads as a fact about that machine.
#[test]
fn a_remote_machine_filter_with_no_remote_is_refused() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let assert = env
        .cmd()
        .args(["--machine", "ws", "--no-remote", "list", "--json"])
        .assert()
        .code(1);
    let said = error_json(&assert)["error"].as_str().unwrap().to_string();
    assert!(said.contains("ws"), "{said}");
    assert!(said.contains("--no-remote"), "{said}");
    assert!(env.ssh_calls().is_empty());

    // This machine's own name is not a remote, so the pair is fine.
    env.cmd()
        .args(["--machine", "laptop", "--no-remote", "list", "--json"])
        .assert()
        .success();
}

/// SPEC §16's ladder is walked across machines, not machine by machine: a
/// local *fragment* match must not shadow a Quest whose slug is exactly what
/// was typed, wherever it runs.
#[test]
fn an_exact_slug_on_a_remote_beats_a_local_fragment_match() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("cdc-backfill-v2");
    let payload = remote_listing("ws", "cdc-backfill");

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload.clone() } }),
    );
    let out = json_of(
        &cmd.args(["enter", "cdc-backfill", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["remote"], true, "{out}");
    assert_eq!(out["machine"], "ws");
    assert_eq!(out["quest"]["slug"], "cdc-backfill");

    // A fragment that matches on both machines is ambiguous, and says where.
    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let assert = cmd.args(["enter", "cdc-back", "--json"]).assert().code(1);
    let err = error_json(&assert);
    assert_eq!(err["code"], "ambiguous");
    let said = err["error"].as_str().unwrap();
    assert!(said.contains("on ws"), "{said}");
    assert!(said.contains("cdc-backfill-v2"), "{said}");
}

/// The tmux session belongs to the machine that runs the Quest: its prefix is
/// its own config, not this one's.
#[test]
fn the_remote_attach_uses_the_far_ends_tmux_prefix() {
    let env = Env::new();
    env.write_config(
        "[machine]\nname = \"laptop\"\n\n[tmux]\nsession_prefix = \"local-\"\n\n\
         [[remotes]]\nname = \"ws\"\nssh = \"ws-host\"\n",
    );
    let payload = far_listing(
        "[machine]\nname = \"ws\"\n\n[tmux]\nsession_prefix = \"work_\"\n",
        &[("over-there", false)],
    );

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let out = json_of(
        &cmd.args(["enter", "over-there", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["tmux_session"], "work_over-there", "{out}");
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t'=work_over-there'"]
    );
}

/// A remote too old to report its prefix falls back to SPEC §15's literal.
#[test]
fn a_remote_that_never_said_its_prefix_gets_the_spec_default() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    // A bare array: what a bd-8lz.5.1 `q` sends, with no `machines` at all.
    let payload: serde_json::Value =
        serde_json::from_str(&remote_listing("ws", "over-there")).unwrap();
    let payload = payload["quests"].to_string();

    let mut cmd = env.cmd();
    env.with_ssh(
        &mut cmd,
        serde_json::json!({ "ws-host": { "stdout": payload } }),
    );
    let out = json_of(
        &cmd.args(["enter", "over-there", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["tmux_session"], "q-over-there", "{out}");
}

/// A window inside a remote Quest needs that machine's session rows, and this
/// machine has none of them — so the attach runs the far end's own `q enter`
/// over ssh (SPEC §15's generic rule, applied to an attach), with the
/// recursion guard on it.
#[test]
fn a_session_label_on_a_remote_quest_runs_that_machines_own_enter() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut cmd = env.cmd();
    let hosts = serde_json::json!({ "ws-host": { "stdout": remote_listing("ws", "over-there") } });
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.with_ssh(&mut cmd, hosts);
    let out = json_of(
        &cmd.args(["enter", "over-there", "--session", "tests", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["machine"], "ws", "{out}");
    assert_eq!(out["session"], "tests", "{out}");
    // D3: pinned like every other proxied line — the id this end resolved, and
    // the identity it resolved to, never the fragment or the slug.
    assert_eq!(
        out["argv"],
        serde_json::json!([
            "q",
            "enter",
            id,
            "--session",
            "tests",
            "--expect",
            expect,
            "--no-remote"
        ]),
        "{out}"
    );
    assert_eq!(
        attach_calls(&env),
        [format!(
            "attach\tws-host\tq\tenter\t{id}\t--session\ttests\
             \t--expect\t{expect}\t--no-remote"
        )]
    );
}

// ------------------------------------------------------- q remote dispatch
//
// SPEC §15's generic rule: every command that resolves a Quest on a remote
// machine is proxied over ssh with the same arguments, and `--no-remote`
// breaks the recursion (bd-8lz.5.3).

impl Env {
    /// A host that answers the listing fan-out with a real far-end listing and
    /// every proxied command with `proxied`.
    fn two_faced(&self, slug: &str, proxied: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "ws-host": { "stdout": remote_listing("ws", slug), "proxied": proxied },
        })
    }

    /// Run `args` with the scripted ssh armed.
    fn over_ssh(&self, hosts: serde_json::Value, args: &[&str]) -> assert_cmd::assert::Assert {
        let mut cmd = self.cmd();
        self.with_ssh(&mut cmd, hosts);
        cmd.args(args).assert()
    }

    /// The same, with the `[y/N]` answered.
    ///
    /// A test has no terminal, so without a scripted answer the only half of a
    /// confirmation the suite can execute is the abort — and the *yes* is the
    /// half in which something is actually destroyed. `Q_FIXTURE_CONFIRM`
    /// stands in for stdin (and only for stdin: the question is still asked out
    /// loud, so these tests read exactly what the human would).
    fn answering(
        &self,
        hosts: serde_json::Value,
        answer: &str,
        args: &[&str],
    ) -> assert_cmd::assert::Assert {
        let mut cmd = self.cmd();
        self.with_ssh(&mut cmd, hosts);
        cmd.env("Q_FIXTURE_CONFIRM", answer);
        cmd.args(args).assert()
    }

    /// Every ssh call that was not the listing fan-out — the proxied ones.
    fn proxy_calls(&self) -> Vec<String> {
        self.ssh_calls()
            .into_iter()
            .filter(|line| !line.contains("\tlist\t") && !line.starts_with("attach"))
            .collect()
    }
}

/// A remote answer that succeeds and says so.
fn ok_reply() -> serde_json::Value {
    serde_json::json!({ "stdout": "done over there\n" })
}

/// The id the far end's own listing gives its Quest.
///
/// It is what a proxied command puts on the wire in place of whatever fragment
/// was typed here: the target is resolved once, on the machine with the human,
/// and the far end is told the identity rather than asked to resolve a
/// fragment a second time against a database this end may have a stale picture
/// of (bd-8lz.5.3 review B2).
fn far_id(hosts: &serde_json::Value, alias: &str) -> String {
    let listing: serde_json::Value =
        serde_json::from_str(hosts[alias]["stdout"].as_str().expect("a listing")).expect("json");
    listing["quests"][0]["id"]
        .as_str()
        .expect("an id")
        .to_string()
}

/// The identity that travels beside the pinned target: `<id>.<created_at>`.
///
/// The id alone is not an identity across time — it is 16 bits and it is freed
/// on delete, so a later `q new` over there can draw the id of a Quest this
/// end still has in its cache. The creation time is what tells the two apart,
/// and the far end refuses a command whose id no longer means the Quest that
/// was confirmed (bd-8lz.5.3 D1).
fn far_expect(hosts: &serde_json::Value, alias: &str) -> String {
    let listing: serde_json::Value =
        serde_json::from_str(hosts[alias]["stdout"].as_str().expect("a listing")).expect("json");
    format!(
        "{}.{}",
        listing["quests"][0]["id"].as_str().expect("an id"),
        listing["quests"][0]["created_at"]
            .as_i64()
            .expect("a created_at"),
    )
}

/// Every command SPEC §16 gives a `<quest>` (or a `<quest>/<label>`) reaches
/// the machine that runs it, with its own arguments and the recursion guard.
#[test]
fn every_command_that_resolves_a_remote_quest_is_proxied_with_the_guard() {
    // `{id}` is the identity the target resolved to here — see [`far_id`].
    for (args, sent) in [
        (vec!["show", "over-there"], "q\tshow\t{id}"),
        (vec!["brief", "over-there"], "q\tbrief\t{id}"),
        (vec!["links", "over-there"], "q\tlinks\t{id}"),
        (vec!["events", "over-there"], "q\tevents\t{id}"),
        (vec!["sessions", "over-there"], "q\tsessions\t{id}"),
        (vec!["name", "over-there"], "q\tname\t{id}"),
        (
            vec!["rename", "over-there", "renamed"],
            "q\trename\t{id}\trenamed",
        ),
        (
            vec!["set", "over-there", "goal", "ship it"],
            "q\tset\t{id}\tgoal\t'ship it'",
        ),
        (
            vec!["spawn", "over-there", "--label", "tests", "run the suite"],
            "q\tspawn\t{id}\t--label\ttests\t'run the suite'",
        ),
        (vec!["close", "over-there", "-f"], "q\tclose\t{id}\t-f"),
        (vec!["rm", "over-there", "-f"], "q\trm\t{id}\t-f"),
        (vec!["resume", "over-there", "-d"], "q\tresume\t{id}\t-d"),
        (vec!["peek", "over-there/master"], "q\tpeek\t{id}/master"),
        (
            vec!["send", "over-there/master", "carry on"],
            "q\tsend\t{id}/master\t'carry on'",
        ),
        (vec!["reset", "over-there/master"], "q\treset\t{id}/master"),
        (
            vec!["kill", "over-there/tests", "-f"],
            "q\tkill\t{id}/tests\t-f",
        ),
        (
            vec!["note", "a note", "--quest", "over-there"],
            "q\tnote\t'a note'\t--quest\t{id}",
        ),
        (
            vec!["link", "add", "https://x/1", "--quest", "over-there"],
            "q\tlink\tadd\t'https://x/1'\t--quest\t{id}",
        ),
    ] {
        let env = Env::new();
        env.with_remotes(&[("ws", "ws-host")]);
        let hosts = env.two_faced("over-there", ok_reply());
        let id = far_id(&hosts, "ws-host");
        let expect = far_expect(&hosts, "ws-host");
        env.over_ssh(hosts, &args).success();
        assert_eq!(
            env.proxy_calls(),
            [format!(
                "ws-host\t{}\t--expect\t{expect}\t--no-remote",
                sent.replace("{id}", &id)
            )],
            "{args:?}"
        );
    }
}

/// **D5** — the seam bd-8lz.5.3 left and bd-8lz.5.4 closes. A `q` too old for
/// the hidden globals every proxied line carries rejects them with clap's exit
/// 2, and what the user saw was an argument they never typed and the far-end
/// binary's own usage. Every fact needed to name the real cause is already in
/// this process, so it names it — with no version round trip on the happy path.
#[test]
fn a_far_end_too_old_for_the_pin_is_named_as_such() {
    // Both shapes a real pre-`--expect` `q` produced: clap names the unknown
    // flag, or — where the subcommand's positionals are full — its value.
    for said in [
        "error: unexpected argument '--expect' found\n",
        "error: unexpected argument 'q-e85c.1787879708' found\nUsage: q-active spawn …\n",
    ] {
        let env = Env::new();
        env.with_remotes(&[("ws", "ws-host")]);
        let mut hosts = env.two_faced("over-there", ok_reply());
        // The second shape names *this* line's own `--expect` value.
        let expect = far_expect(&hosts, "ws-host");
        let said = said.replace("q-e85c.1787879708", &expect);
        hosts["ws-host"]["proxied"] = serde_json::json!({ "exit": 2, "stderr": said });

        let assert = env.over_ssh(hosts, &["brief", "over-there"]).code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(stderr.contains("`q` on ws is too old"), "{stderr}");
        assert!(stderr.contains("q doctor"), "{stderr}");
        assert!(stderr.contains("upgrade `q` on ws"), "{stderr}");
        // Clap's own line survives as evidence, not as the explanation.
        assert!(stderr.contains("unexpected argument"), "{stderr}");
        // …and never as a usage error the user could act on.
        assert!(!stderr.contains("Usage: q-active"), "{stderr}");
    }
}

/// The other half of D5: a usage error the *user* made still belongs to the
/// user. Only the words this end put on the line are read as version skew.
#[test]
fn a_far_end_rejecting_the_users_own_argument_is_relayed_unchanged() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({
        "exit": 2,
        "stderr": "error: unexpected argument '--bogus' found\n",
    });
    let assert = env
        .over_ssh(env.two_faced("over-there", reply), &["brief", "over-there"])
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("'--bogus'"), "{stderr}");
    assert!(!stderr.contains("too old"), "{stderr}");
}

/// **D6** — the listing already said "no `q` on PATH there"; two lines later
/// the proxy handed the user zsh's own message and exit 127. It says it in
/// this program's words now, and in the same words `q doctor` uses.
#[test]
fn a_proxied_command_against_a_q_less_remote_says_so_itself() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({ "exit": 127, "stderr": "zsh:1: command not found: q\n" });
    let assert = env
        .over_ssh(
            env.two_faced("over-there", reply),
            &["brief", "over-there", "--json"],
        )
        .code(1)
        .stdout("");
    let error = error_json(&assert);
    let said = error["error"].as_str().unwrap();
    assert!(said.contains("no `q` on PATH there"), "{said}");
    assert!(said.contains("install q on ws"), "{said}");
    assert!(said.contains("command not found"), "{said}");
}

/// The far end's streams are this command's streams, and its exit code is this
/// process's (CLAUDE.md: human output to stdout, errors to stderr).
#[test]
fn a_proxied_command_relays_both_streams_and_its_exit_code() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({
        "stdout": "the pane, verbatim\n",
        "stderr": "error: not idle\n",
        "exit": 1,
    });
    env.over_ssh(
        env.two_faced("over-there", reply),
        &["peek", "over-there/master"],
    )
    .code(1)
    .stdout("the pane, verbatim\n")
    .stderr(predicate::str::contains("error: not idle"));
}

/// `--json` still means one JSON document on stdout: the far end already wrote
/// one and it is relayed rather than rebuilt.
#[test]
fn a_proxied_json_command_is_still_valid_json_on_stdout() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({ "stdout": "{\"quest\":{\"slug\":\"over-there\"}}\n" });
    let hosts = env.two_faced("over-there", reply);
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    let assert = env
        .over_ssh(hosts, &["show", "over-there", "--json"])
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["slug"], "over-there", "{out}");
    // `--json` travelled: the far end has to know to answer in kind.
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\tshow\t{id}\t--json\t--expect\t{expect}\t--no-remote"
        )]
    );
}

/// A far end that exits non-zero under `--json` sends a `{"error": …}` on
/// stderr, and that is what a `--json` caller here gets — unchanged.
#[test]
fn a_proxied_json_failure_keeps_the_far_ends_error_object() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({
        "stderr": "{\"error\":\"not found: session `nope`\",\"code\":\"not_found\"}\n",
        "exit": 1,
    });
    let assert = env
        .over_ssh(
            env.two_faced("over-there", reply),
            &["peek", "over-there/nope", "--json"],
        )
        .code(1)
        .stdout("");
    assert_eq!(error_json(&assert)["code"], "not_found");
}

/// The recursion guard. A `q` running with `--no-remote` has no remote to ask,
/// so it can neither resolve a Quest elsewhere nor dial one — which is exactly
/// what every proxied invocation carries.
#[test]
fn no_remote_stops_the_proxy_dead() {
    for args in [
        vec!["show", "over-there"],
        vec!["peek", "over-there/master"],
        vec!["close", "over-there", "-f"],
        vec!["spawn", "over-there", "--label", "t", "p"],
        vec!["new", "--name", "here", "--no-beads", "-d"],
    ] {
        let env = Env::new();
        env.with_remotes(&[("ws", "ws-host")]);
        let mut full = vec!["--no-remote"];
        full.extend(args.iter().copied());
        env.over_ssh(env.two_faced("over-there", ok_reply()), &full);
        assert!(
            env.ssh_calls().is_empty(),
            "{args:?}: {:?}",
            env.ssh_calls()
        );
    }
}

/// A session id or a bare `<label>` is unique only per machine (SPEC §16), so
/// neither can name a session elsewhere: they stay here, and never cost an ssh.
#[test]
fn a_bare_label_or_a_session_id_never_leaves_this_machine() {
    for target in ["master", "s-0001"] {
        let env = Env::new();
        env.with_remotes(&[("ws", "ws-host")]);
        env.over_ssh(env.two_faced("over-there", ok_reply()), &["peek", target])
            .code(1)
            .stderr(predicate::str::contains("not found"));
        assert!(
            env.ssh_calls().is_empty(),
            "{target}: {:?}",
            env.ssh_calls()
        );
    }
}

/// The commands that cannot honestly travel are refused with a reason and a
/// way forward, not proxied into something that would be wrong over there.
#[test]
fn a_command_that_cannot_travel_is_refused_with_a_reason() {
    for (args, said) in [
        (vec!["events", "over-there", "--follow"], "--follow"),
        (
            vec![
                "artifact",
                "add",
                "/tmp/report.html",
                "--quest",
                "over-there",
            ],
            "absolute path",
        ),
        (
            vec!["phase", "building", "--quest", "over-there"],
            "$Q_SESSION",
        ),
    ] {
        let env = Env::new();
        env.with_remotes(&[("ws", "ws-host")]);
        env.over_ssh(env.two_faced("over-there", ok_reply()), &args)
            .code(1)
            .stderr(predicate::str::contains(said))
            .stderr(predicate::str::contains("ws"));
        assert!(env.proxy_calls().is_empty(), "{args:?}");
    }
    // The escape hatch the `--follow` refusal prints is one a human can paste:
    // the real alias and the real Quest, and `ssh -t`, whose pty is what keeps
    // the far `q` from outliving the connection (bd-8lz.5.3 review S2).
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.over_ssh(
        env.two_faced("over-there", ok_reply()),
        &["events", "over-there", "--follow"],
    )
    .code(1)
    .stderr(predicate::str::contains(
        "`ssh -t ws-host q events over-there -f --no-remote`",
    ));

    // The snapshot, which is what `--follow` is refused in favour of, travels.
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced("over-there", ok_reply());
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.over_ssh(hosts, &["events", "over-there"]).success();
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\tevents\t{id}\t--expect\t{expect}\t--no-remote"
        )]
    );
}

/// The confirmation belongs to the machine with the human. The far end's stdin
/// is `/dev/null`, so its own prompt could only abort — so it is asked here,
/// and what travels is `--confirmed`: the answer to the question, and nothing
/// else. See the next test for why that is not `-f`.
#[test]
fn a_destructive_command_is_confirmed_here_and_travels_as_confirmed() {
    // Answered "no" — nothing is sent to run.
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.answering(
        env.two_faced("over-there", ok_reply()),
        "n",
        &["close", "over-there"],
    )
    .code(1)
    .stderr(predicate::str::contains("aborted"));
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());

    // No terminal and nothing scripted is a "no" as well.
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.over_ssh(
        env.two_faced("over-there", ok_reply()),
        &["close", "over-there"],
    )
    .code(1)
    .stderr(predicate::str::contains("aborted"));
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());

    // Answered "yes" — it travels, once, with `--confirmed` and never `-f`.
    // `q kill` asks the far end first whether its session is still live (see
    // `nothing_to_kill`); a canned answer it cannot read leaves the question
    // exactly where it was, which is what the second line here is.
    for (args, sent) in [
        (vec!["close", "over-there"], "q\tclose\t{id}\t--confirmed"),
        (vec!["rm", "over-there"], "q\trm\t{id}\t--confirmed"),
        (
            vec!["kill", "over-there/tests"],
            "q\tkill\t{id}/tests\t--confirmed",
        ),
    ] {
        let env = Env::new();
        env.with_remotes(&[("ws", "ws-host")]);
        let hosts = env.two_faced("over-there", ok_reply());
        let id = far_id(&hosts, "ws-host");
        let expect = far_expect(&hosts, "ws-host");
        let killing = args[0] == "kill";
        env.answering(hosts, "y", &args).success();
        let calls = env.proxy_calls();
        let ran = calls.last().expect("the command travelled");
        assert_eq!(
            *ran,
            format!(
                "ws-host\t{}\t--expect\t{expect}\t--no-remote",
                sent.replace("{id}", &id)
            ),
            "{args:?}"
        );
        assert_eq!(
            calls.len(),
            if killing { 2 } else { 1 },
            "{args:?}: {calls:?}"
        );
        assert!(!ran.contains("\t-f"), "{args:?}: {calls:?}");
    }

    // `-f` given by the user is the user's own word and still travels as `-f`:
    // the prompt is skipped here, and the far end gets the powers `-f` has
    // there — which is exactly what the same line typed on that machine does.
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced("over-there", ok_reply());
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.over_ssh(hosts, &["close", "over-there", "-f"])
        .success();
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\tclose\t{id}\t-f\t--expect\t{expect}\t--no-remote"
        )]
    );
}

/// B1. Locally, `-f` on `q rm` carries two meanings: skip the prompt, *and*
/// override the refusal to delete a Quest whose tmux session is still running.
/// A proxied `q rm` answers the first question only, so `--confirmed` must not
/// buy the second — otherwise the identical command is strictly more
/// destructive against a remote Quest than against a local one.
#[test]
fn confirmed_answers_the_question_and_buys_nothing_else() {
    // A live tmux session: the hard refusal, which is not a confirmation at all.
    let env = Env::new();
    env.new_quest("foo");
    env.cmd()
        .env("Q_FIXTURE_CONFIRM", "y")
        .args(["rm", "foo", "--confirmed"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("still runs in tmux session q-foo"));
    assert_eq!(env.count("SELECT count(*) FROM quest"), 1);

    // `-f` is what authorises that, and it still does.
    env.cmd().args(["rm", "foo", "-f"]).assert().success();
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);

    // With no session left there is only the question, and `--confirmed`
    // answers it.
    let env = Env::new();
    env.new_quest("foo");
    env.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));
    env.cmd()
        .args(["rm", "foo", "--confirmed"])
        .assert()
        .success();
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
}

/// B2. The prompt is built from the Quest that was resolved **here**; the far
/// end must act on that Quest and not re-resolve the fragment against its own
/// database, which may have moved on since the listing this end read.
///
/// So what travels is the identity, and the fragment the user typed is not on
/// the wire at all: the Quest named in the question is the Quest that dies.
#[test]
fn a_destructive_command_names_the_quest_it_destroys() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced("over-there", ok_reply());
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    // A fragment, not the slug — the shape that resolves differently on two
    // databases.
    env.answering(hosts, "y", &["rm", "over"])
        .success()
        .stderr(predicate::str::contains(
            "remove quest (and all of its history) over-there on ws?",
        ));
    let calls = env.proxy_calls();
    assert_eq!(
        calls,
        [format!(
            "ws-host\tq\trm\t{id}\t--confirmed\t--expect\t{expect}\t--no-remote"
        )]
    );
    assert!(!calls[0].contains("\tover\t"), "{calls:?}");
}

/// A far end, as it really is: its own `q`, its own database, one Quest.
/// Returned with the Quest's real identity, so a test can hand it a line that
/// was pinned to a *different* one.
fn far_end(slug: &str) -> (Env, String, i64) {
    let far = Env::new();
    far.write_config("[machine]\nname = \"ws\"\n");
    let work = far.work("repo");
    far.cmd()
        .args(["new", "--name", slug, "--no-beads", "-d", "--json"])
        .args(["--dir", work.to_str().unwrap()])
        .assert()
        .success();
    let listing = far.json(&["list"]);
    let id = listing["quests"][0]["id"]
        .as_str()
        .expect("an id")
        .to_string();
    let created = listing["quests"][0]["created_at"]
        .as_i64()
        .expect("a stamp");
    (far, id, created)
}

/// D1. A Quest id is 16 bits and is **freed on delete**, so a `q new` over
/// there can draw the id of a Quest this end still has in its cache. Both ends
/// then agree on the id and mean different Quests — and pinning the id alone
/// is what let a confirmed `q rm` destroy the one the human never saw named.
///
/// So the identity that travels is the id *and* the Quest's creation time, and
/// the far end refuses a command whose id no longer means the Quest it was
/// confirmed against. Fail closed: nothing is deleted, and the fragment is
/// never fallen back to.
#[test]
fn a_reused_id_is_refused_by_the_far_end_instead_of_acted_on() {
    let (far, id, created) = far_end("innocent");
    // No live pane over there, so the only thing between `q rm` and the row is
    // the question — and the identity.
    far.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));

    // This machine's picture: the same id, under the Quest that held it before
    // it was freed and drawn again.
    let stale_created = created - 3600;
    let mut stale = far.json(&["list"]);
    stale["quests"][0]["slug"] = serde_json::json!("victim");
    stale["quests"][0]["created_at"] = serde_json::json!(stale_created);
    let hosts = serde_json::json!({
        "ws-host": { "stdout": stale.to_string(), "proxied": ok_reply() },
    });

    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.answering(hosts, "y", &["rm", "victim"])
        .success()
        .stderr(predicate::str::contains(
            "remove quest (and all of its history) victim on ws?",
        ));
    let expect = format!("{id}.{stale_created}");
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\trm\t{id}\t--confirmed\t--expect\t{expect}\t--no-remote"
        )]
    );

    // Now run that exact line where it was headed. The id resolves — to a
    // Quest nobody confirmed — and the far end says so instead of deleting it.
    far.cmd()
        .args(["rm", &id, "--confirmed", "--no-remote"])
        .args(["--expect", &expect])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing"))
        .stderr(predicate::str::contains("innocent"));
    assert_eq!(far.count("SELECT count(*) FROM quest"), 1);

    // …and the same line, pinned to the Quest that is really there, goes
    // through: the check refuses a mismatch, not the command.
    far.cmd()
        .args(["rm", &id, "--confirmed", "--no-remote"])
        .args(["--expect", &format!("{id}.{created}")])
        .assert()
        .success();
    assert_eq!(far.count("SELECT count(*) FROM quest"), 0);
}

/// The identity is checked on **every** proxied command, not only the ones
/// that destroy something: `q send` typed into an agent in the wrong Quest is
/// its own kind of damage, and a read is a lie about which machine holds what.
#[test]
fn a_reused_id_is_refused_for_a_command_that_only_reads() {
    let (far, id, created) = far_end("innocent");
    let wrong = format!("{id}.{}", created - 3600);
    far.cmd()
        .args(["show", &id, "--no-remote", "--expect", &wrong])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing"));
    // The same command with the right identity is just `q show`.
    far.cmd()
        .args(["show", &id, "--no-remote"])
        .args(["--expect", &format!("{id}.{created}")])
        .assert()
        .success()
        .stdout(predicate::str::contains("innocent"));
}

/// An id the far end no longer has at all is its own plain not-found — the
/// answer it would have given anyway, not a second kind of error.
#[test]
fn an_identity_for_an_id_that_is_gone_is_a_not_found() {
    let (far, id, created) = far_end("innocent");
    far.write_fixture(serde_json::json!({ "next_pane": 1, "panes": [] }));
    far.cmd()
        .args(["rm", &id, "-f", "--no-remote"])
        .assert()
        .success();
    far.cmd()
        .args(["show", &id, "--no-remote"])
        .args(["--expect", &format!("{id}.{created}")])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not found"));
}

/// Fail closed. An identity this `q` cannot read, or one on a command that
/// resolves no Quest at all, is an error — never a command that quietly runs
/// unchecked.
#[test]
fn an_identity_that_cannot_be_checked_is_refused() {
    let (far, id, _created) = far_end("innocent");
    far.cmd()
        .args(["show", &id, "--no-remote", "--expect", "nonsense"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not a quest identity"));
    far.cmd()
        .args(["list", "--no-remote", "--expect", "q-1234.1"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("resolves none"));
}

/// D3. `q enter <remote> --session <label>` runs the far end's own `q enter`
/// over ssh, so it is a proxied command like any other: it travels pinned to
/// the id, and the far end checks the identity before it hands over a
/// terminal. Before this it sent the resolved *slug*, and a stale listing put
/// the user in a different Quest's agent.
#[test]
fn a_remote_enter_by_label_is_checked_on_the_far_end() {
    let (far, id, created) = far_end("attach2");
    far.cmd()
        .args(["enter", &id, "--session", "w1", "--no-remote"])
        .args(["--expect", &format!("{id}.{}", created - 60)])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing"));
    // With the identity it really has, the check is silent and the far end
    // gets on with resolving the label.
    far.cmd()
        .args(["enter", &id, "--session", "w1", "--no-remote"])
        .args(["--expect", &format!("{id}.{created}")])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("not found: session `w1`"));
}

/// D4. Deleting a Quest leaves its beads epic open in a tracker other people
/// share, and the local prompt says so **before** the answer. The epic is a
/// column of the Quest row, so it rides in with the listing and the proxied
/// question can say it too.
#[test]
fn a_proxied_rm_warns_about_the_epic_it_will_orphan() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let mut listing: serde_json::Value =
        serde_json::from_str(&remote_listing("ws", "over-there")).unwrap();
    listing["quests"][0]["beads_epic"] = serde_json::json!("bd-7");
    let hosts = serde_json::json!({
        "ws-host": { "stdout": listing.to_string(), "proxied": ok_reply() },
    });
    env.answering(hosts, "n", &["rm", "over-there"])
        .code(1)
        .stderr(predicate::str::contains(
            "over-there on ws (beads epic bd-7 stays open)?",
        ));
    // No epic, no parenthetical.
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.answering(
        env.two_faced("over-there", ok_reply()),
        "n",
        &["rm", "over-there"],
    )
    .code(1)
    .stderr(predicate::str::contains(
        "remove quest (and all of its history) over-there on ws?",
    ));
}

/// D5. `q kill` on a session the sweep has already ended asks nothing on the
/// machine that runs it — the question would be about work that is not going
/// to happen. This end cannot see the far end's session rows, so it asks the
/// far end, on the connection the kill is about to use anyway.
#[test]
fn a_proxied_kill_asks_nothing_about_a_session_that_is_already_over() {
    let ended = serde_json::json!([
        { "label": "master", "status": "idle" },
        { "label": "w1", "status": "ended" },
    ]);
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced(
        "over-there",
        serde_json::json!({ "stdout": ended.to_string() }),
    );
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    // No `Q_FIXTURE_CONFIRM`: a question here would abort, and this succeeds.
    env.over_ssh(hosts, &["kill", "over-there/w1"])
        .success()
        .stderr(predicate::str::contains("kill session").not());
    assert_eq!(
        env.proxy_calls(),
        [
            format!(
                "ws-host\tq\tsessions\t{id}\t--all\t--json\t--no-remote\
                 \t--expect\t{expect}"
            ),
            format!("ws-host\tq\tkill\t{id}/w1\t--expect\t{expect}\t--no-remote"),
        ]
    );
    // …and nothing was answered on the human's behalf: the far end still gets
    // to ask, and to abort, if this end read it wrong.
    assert!(!env.proxy_calls()[1].contains("--confirmed"));

    // A session that is still live is still confirmed here, exactly as before.
    let live = serde_json::json!([{ "label": "w1", "status": "busy" }]);
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced(
        "over-there",
        serde_json::json!({ "stdout": live.to_string() }),
    );
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.answering(hosts, "y", &["kill", "over-there/w1"])
        .success()
        .stderr(predicate::str::contains(
            "kill session over-there/w1 on ws?",
        ));
    assert_eq!(
        env.proxy_calls().last().unwrap(),
        &format!("ws-host\tq\tkill\t{id}/w1\t--confirmed\t--expect\t{expect}\t--no-remote")
    );
}

/// A `q kill` prompt names the session, which is the only thing that dies —
/// not the Quest that contains it. And the master is refused before the
/// question, exactly as `kill::guard_master` refuses it locally: a human must
/// not be asked to authorise a kill `q` will not perform.
#[test]
fn a_remote_kill_asks_about_the_session_and_never_about_the_master() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.answering(
        env.two_faced("over-there", ok_reply()),
        "n",
        &["kill", "over-there/tests"],
    )
    .code(1)
    .stderr(predicate::str::contains(
        "kill session over-there/tests on ws?",
    ));

    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.answering(
        env.two_faced("over-there", ok_reply()),
        "y",
        &["kill", "over-there/master"],
    )
    .code(1)
    .stderr(predicate::str::contains(
        "is the master of quest over-there",
    ))
    .stderr(predicate::str::contains("q close over-there"));
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());
}

/// S1. A `--` makes everything after it positional over there, so the guard
/// cannot ride on the end of the line: it goes where the far end reads it as
/// the flag it is. Without this, a command that is perfectly legal on the
/// machine that runs it fails when proxied, blaming an argument the user never
/// typed.
#[test]
fn a_separator_in_the_line_does_not_swallow_the_guard() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced("over-there", ok_reply());
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.over_ssh(
        hosts,
        &["note", "--quest", "over-there", "--", "-- a dashed note"],
    )
    .success();
    // Before the separator, so the far end reads it as a flag; the free text is
    // still one shell word and still says what it said.
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\tnote\t--quest\t{id}\t--expect\t{expect}\t--no-remote\
             \t--\t'-- a dashed note'"
        )]
    );
}

/// …and the far end, running that exact line, does what the user asked — with
/// no `--no-remote` anywhere in the note.
#[test]
fn the_line_a_separator_produces_is_one_the_far_end_accepts() {
    let far = Env::new();
    far.new_quest("over-there");
    far.cmd()
        .args(["note", "--quest", "over-there", "--no-remote"])
        .args(["--", "-- a dashed note"])
        .assert()
        .success();
    let events = far.json(&["events", "over-there"]);
    let notes: Vec<&str> = events
        .as_array()
        .expect("events")
        .iter()
        .filter(|e| e["kind"] == "note")
        .filter_map(|e| e["payload"]["text"].as_str())
        .collect();
    assert_eq!(notes, ["-- a dashed note"], "{events}");
}

/// SPEC §15: `q new --machine ws …` → `ssh <alias> q new … -d`. The Quest is
/// created **on that machine** — before bd-8lz.5.3 this made a local Quest
/// merely labelled `ws`, indistinguishable from a real remote row.
#[test]
fn new_on_a_remote_creates_it_there_and_not_here() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let payload = serde_json::json!({
        "quest": { "id": "q-1234", "slug": "over-there", "machine": "ws" },
        "session": { "id": "s-0001" },
        "tmux_session": "q-over-there",
        "attach": "none",
    });
    let hosts = serde_json::json!({
        "ws-host": { "stdout": serde_json::to_string(&payload).unwrap() },
    });
    let assert = env
        .over_ssh(
            hosts,
            &[
                "--machine",
                "ws",
                "new",
                "--name",
                "over-there",
                "--goal",
                "make it idempotent",
                "--no-beads",
                "-d",
                "--json",
            ],
        )
        .success();

    // `-d` and `--json` are added; `--machine` does not travel (over there it
    // would name a machine that `q` has never heard of).
    assert_eq!(
        env.ssh_calls(),
        [
            "ws-host\tq\tnew\t--name\tover-there\t--goal\t'make it idempotent'\
             \t--no-beads\t-d\t--json\t--no-remote"
        ]
    );
    let out = json_of(&assert);
    assert_eq!(out["machine"], "ws", "{out}");
    assert_eq!(out["remote"], true, "{out}");
    assert_eq!(out["quest"]["slug"], "over-there", "{out}");
    assert_eq!(out["attach"], "none", "{out}");

    // Nothing was written here. The old bug's row would be right there.
    assert_eq!(env.count("SELECT COUNT(*) FROM quest"), 0);
}

/// …and then enter (SPEC §15), unless `-d` said not to. The session name comes
/// from the machine that created it, so nothing here guesses a tmux prefix.
#[test]
fn new_on_a_remote_attaches_to_the_machine_that_created_it() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let payload = serde_json::json!({
        "quest": { "id": "q-1234", "slug": "over-there", "machine": "ws" },
        "tmux_session": "work_over-there",
    });
    let hosts = serde_json::json!({
        "ws-host": { "stdout": serde_json::to_string(&payload).unwrap() },
    });
    env.over_ssh(
        hosts,
        &[
            "--machine",
            "ws",
            "new",
            "--name",
            "over-there",
            "--no-beads",
        ],
    )
    .success()
    .stdout(predicate::str::contains("created quest over-there on ws"));
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t'=work_over-there'"]
    );
}

/// A `q new` that fails over there fails here, with what it said.
#[test]
fn a_remote_creation_that_fails_says_what_the_far_end_said() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = serde_json::json!({
        "ws-host": {
            "stderr": "{\"error\":\"slug `over-there` is taken\",\"code\":\"conflict\"}\n",
            "exit": 1,
        },
    });
    env.over_ssh(
        hosts,
        &["--machine", "ws", "new", "--name", "over-there", "-d"],
    )
    .code(1)
    .stderr(predicate::str::contains("slug `over-there` is taken"))
    .stderr(predicate::str::contains("on ws"));
    assert_eq!(env.count("SELECT COUNT(*) FROM quest"), 0);
}

/// `q resume` has no terminal at the far end to attach to, so it travels with
/// `-d` and the attach is made from here afterwards.
#[test]
fn resume_on_a_remote_runs_detached_there_and_attaches_from_here() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced("over-there", ok_reply());
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.over_ssh(hosts, &["resume", "over-there"]).success();
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\tresume\t{id}\t-d\t--expect\t{expect}\t--no-remote"
        )]
    );
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t'=q-over-there'"]
    );
}

/// A resume that failed over there is not followed by an attach to a tmux
/// session that does not exist.
#[test]
fn a_remote_resume_that_failed_does_not_attach() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({ "stderr": "error: still running\n", "exit": 1 });
    env.over_ssh(
        env.two_faced("over-there", reply),
        &["resume", "over-there"],
    )
    .code(1);
    assert!(attach_calls(&env).is_empty());
}

/// The ladder is the same one `q enter` walks (SPEC §16), across machines: a
/// target that matches on two of them is listed, not guessed at.
///
/// …and the cache-first shortcut is the same one too, gap included: until a
/// listing has taught this machine that `ws` holds a Quest by that name, the
/// exact local hit is taken with no ssh at all.
#[test]
fn a_target_that_matches_on_two_machines_is_ambiguous_for_every_command() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.new_quest("over-there");
    let hosts = env.two_faced("over-there", ok_reply());

    // The accepted gap (bd-8lz.5.2): a cold cache is not a suspicion.
    env.over_ssh(hosts.clone(), &["show", "over-there", "--json"])
        .success();
    assert!(env.proxy_calls().is_empty());

    // One listing is enough to teach it, and then the collision is reported.
    env.over_ssh(hosts.clone(), &["list", "--json"]).success();
    let assert = env
        .over_ssh(hosts, &["show", "over-there", "--json"])
        .code(1);
    let err = error_json(&assert);
    assert_eq!(err["code"], "ambiguous", "{err}");
    let said = err["error"].as_str().unwrap();
    assert!(
        said.contains("on laptop") && said.contains("on ws"),
        "{said}"
    );
    assert!(env.proxy_calls().is_empty());
}

/// The user-visible lie this bead closes: a Quest `q list` happily shows used
/// to answer `not found` to everything but `q enter`.
#[test]
fn a_quest_the_listing_shows_is_no_longer_not_found() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = env.two_faced("over-there", ok_reply());
    let listing = json_of(&env.over_ssh(hosts.clone(), &["list", "--json"]).success());
    assert_eq!(slugs_of(&listing), ["over-there"], "{listing}");
    env.over_ssh(hosts, &["show", "over-there"]).success();

    // A target that is on no machine still is not found, and says where it
    // looked when `--machine` narrowed that.
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let assert = env
        .over_ssh(
            env.two_faced("over-there", ok_reply()),
            &["--machine", "ws", "show", "nowhere", "--json"],
        )
        .code(1);
    let err = error_json(&assert);
    assert_eq!(err["code"], "not_found", "{err}");
    assert!(err["error"].as_str().unwrap().contains("on ws"), "{err}");
}

/// A remote that cannot be reached at the moment of the proxy is an error
/// about that machine, not a relayed exit code — ssh's own 255 says nothing
/// about a command that never ran.
#[test]
fn a_remote_that_drops_the_connection_is_reported_as_unreachable() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({
        "stderr": "ssh: connect to host ws-host port 22: Connection refused\n",
        "exit": 255,
    });
    env.over_ssh(env.two_faced("over-there", reply), &["show", "over-there"])
        .code(1)
        .stderr(predicate::str::contains("cannot reach ws"))
        .stderr(predicate::str::contains("Connection refused"));
}

/// Free text carrying spaces, quotes, `$` and `=` is one argv word here and
/// one shell word over there. Quoting happens at the single ssh boundary, so
/// the far end's login shell cannot expand or split it.
#[test]
fn free_text_reaches_the_far_end_as_one_shell_word() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let text = "run `make test` for $USER 'now' FOO=1 ~x";
    let hosts = env.two_faced("over-there", ok_reply());
    let id = far_id(&hosts, "ws-host");
    let expect = far_expect(&hosts, "ws-host");
    env.over_ssh(hosts, &["send", "over-there/master", text])
        .success();
    assert_eq!(
        env.proxy_calls(),
        [format!(
            "ws-host\tq\tsend\t{id}/master\t\
             'run `make test` for $USER '\\''now'\\'' FOO=1 ~x'\
             \t--expect\t{expect}\t--no-remote"
        )]
    );
}

/// `q resume <remote> --json` is one document, not two: the far end's answer
/// with the attach folded into it.
#[test]
fn a_remote_resume_reports_both_halves_as_one_json_document() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let reply = serde_json::json!({
        "stdout": "{\"quest\":{\"slug\":\"over-there\"},\"tmux_session\":\"q-over-there\"}\n",
    });
    let assert = env
        .over_ssh(
            env.two_faced("over-there", reply),
            &["resume", "over-there", "--json"],
        )
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["slug"], "over-there", "{out}");
    assert_eq!(out["machine"], "ws", "{out}");
    assert_eq!(out["remote"], true, "{out}");
    assert_eq!(
        out["argv"],
        serde_json::json!(["tmux", "attach", "-t", "=q-over-there"]),
        "{out}"
    );
}

// ---------------------------------------------------------------------------
// `q tpl` — template CRUD (SPEC §11, bd-8lz.6.1)
// ---------------------------------------------------------------------------

/// One `q tpl add` with every field set, so the tests below have something
/// whose round trip is worth checking.
fn add_full_template(env: &Env, name: &str, work: &std::path::Path) {
    env.cmd()
        .args(["tpl", "add", name])
        .args(["--description", "the Monday routine"])
        .args(["--goal", "tidy {{arg.repo}} on {{date}}"])
        .args(["--prompt", "start with the lint report"])
        .args(["--workflow", "routine", "--repo", "work"])
        .args(["--cwd", work.to_str().unwrap()])
        .args(["--tag", "routine", "--tag", "weekly", "--brain"])
        .assert()
        .success();
}

/// Today as `{{date}}` renders it. Read on **both** sides of the command
/// under test, never only afterwards: a single reading turns any date
/// assertion into a once-a-year midnight flake.
fn today_iso() -> String {
    chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

/// A file inside the sandbox, for the import and editor fixtures.
fn write_file(env: &Env, name: &str, body: &str) -> std::path::PathBuf {
    let path = env.dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn tpl_add_then_show_round_trips_every_field() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    let assert = env
        .cmd()
        .args(["tpl", "show", "weekly-hygiene", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["name"], "weekly-hygiene");
    assert_eq!(out["description"], "the Monday routine");
    assert_eq!(out["goal"], "tidy {{arg.repo}} on {{date}}");
    assert_eq!(out["master_prompt"], "start with the lint report");
    assert_eq!(out["workflow"], "routine");
    assert_eq!(out["beads_repo"], "work");
    assert_eq!(out["cwd"], work.to_str().unwrap());
    assert_eq!(out["create_brain"], true);
    assert_eq!(out["tags"], serde_json::json!(["routine", "weekly"]));
    assert_eq!(out["run_count"], 0);
    assert_eq!(out["last_run_at"], serde_json::Value::Null);
    assert!(out["id"].as_str().unwrap().starts_with("t-"), "{out}");
}

#[test]
fn tpl_list_is_empty_before_anything_is_added() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no templates"));
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    assert_eq!(json_of(&assert), serde_json::json!([]));
}

#[test]
fn tpl_list_names_every_template_alphabetically() {
    let env = Env::new();
    for name in ["weekly-hygiene", "deps-audit"] {
        env.cmd().args(["tpl", "add", name]).assert().success();
    }
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    let listed = json_of(&assert);
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["deps-audit", "weekly-hygiene"]);

    env.cmd()
        .args(["tpl", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("NAME"))
        .stdout(predicate::str::contains("deps-audit"))
        .stdout(predicate::str::contains("RUNS"));
}

#[test]
fn tpl_add_rejects_a_name_that_is_not_kebab_case() {
    let env = Env::new();
    for bad in ["Weekly", "weekly_hygiene", "weekly-", "weekly--x"] {
        let assert = env
            .cmd()
            .args(["tpl", "add", bad, "--json"])
            .assert()
            .failure();
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        assert_eq!(parsed["code"], "invalid", "{bad}: {parsed}");
        assert!(
            parsed["error"].as_str().unwrap().contains("template name"),
            "{bad}: {parsed}"
        );
    }
}

#[test]
fn tpl_add_refuses_a_name_that_is_already_taken() {
    let env = Env::new();
    env.cmd().args(["tpl", "add", "routine"]).assert().success();
    let assert = env
        .cmd()
        .args(["tpl", "add", "routine", "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "conflict", "{parsed}");
    assert!(
        parsed["error"].as_str().unwrap().contains("q tpl edit"),
        "{parsed}"
    );
}

/// A placeholder no `--arg` could ever fill is a typo, and the moment to say so
/// is when it is written, not when a routine is halfway through starting.
#[test]
fn tpl_add_refuses_a_placeholder_nothing_can_fill() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "bad", "--goal", "{{today}}"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown placeholder `{{today}}`"))
        .stderr(predicate::str::contains("{{arg.<key>}}"));
    env.cmd()
        .args(["tpl", "add", "bad", "--prompt", "{{arg.}}"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("master_prompt"));
    // Nothing was stored on the way to either failure.
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    assert_eq!(json_of(&assert), serde_json::json!([]));
}

#[test]
fn tpl_add_refuses_a_cwd_that_is_not_a_directory() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", "/definitely/not/here"])
        .assert()
        .failure()
        // One prefix and one mention of the path: the underlying error used
        // to be wrapped whole, so this read "not found: routine: cwd
        // `/definitely/not/here`: not found: no such directory:
        // /definitely/not/here" (bd-8lz.6.2).
        .stderr(predicate::str::contains(
            "not found: routine: cwd `/definitely/not/here`: no such directory",
        ))
        .stderr(predicate::str::contains("not found: no such directory").not());
}

#[test]
fn tpl_show_resolves_a_fragment_and_lists_the_candidates_when_it_cannot() {
    let env = Env::new();
    for name in ["weekly-hygiene", "weekly-report", "deps-audit"] {
        env.cmd().args(["tpl", "add", name]).assert().success();
    }
    let assert = env
        .cmd()
        .args(["tpl", "show", "audit", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["name"], "deps-audit");

    let assert = env
        .cmd()
        .args(["tpl", "show", "weekly", "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "ambiguous", "{parsed}");
    let message = parsed["error"].as_str().unwrap();
    assert!(message.contains("weekly-hygiene"), "{message}");
    assert!(message.contains("weekly-report"), "{message}");
}

#[test]
fn tpl_show_of_an_unknown_name_says_what_there_is_instead() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "deps-audit"])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "nope", "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found", "{parsed}");
    assert!(
        parsed["error"].as_str().unwrap().contains("deps-audit"),
        "{parsed}"
    );
}

#[test]
fn tpl_edit_with_flags_patches_only_what_was_given() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    env.cmd()
        .args(["tpl", "edit", "weekly", "--description", "changed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("updated template weekly-hygiene"));

    let assert = env
        .cmd()
        .args(["tpl", "show", "weekly", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["description"], "changed");
    // Untouched by a flag nobody gave.
    assert_eq!(out["goal"], "tidy {{arg.repo}} on {{date}}");
    assert_eq!(out["workflow"], "routine");
    assert_eq!(out["create_brain"], true);
}

#[test]
fn tpl_edit_with_a_blank_flag_clears_the_field() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    env.cmd()
        .args(["tpl", "edit", "weekly"])
        .args(["--goal", "", "--workflow", "", "--tag", ""])
        .arg("--no-brain")
        .assert()
        .success();

    let assert = env
        .cmd()
        .args(["tpl", "show", "weekly", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    for field in ["goal", "workflow", "tags"] {
        assert_eq!(out[field], serde_json::Value::Null, "{field}: {out}");
    }
    assert_eq!(out["create_brain"], false);
    assert_eq!(out["description"], "the Monday routine");
}

/// The editor is stubbed, exactly like tmux and `bd`: no test may launch one.
#[test]
fn tpl_edit_without_flags_round_trips_the_toml_through_the_editor() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    let edited = write_file(
        &env,
        "edited.toml",
        "[[template]]\nname = \"weekly-hygiene\"\ndescription = \"edited\"\n\
         goal = \"tidy up on {{date}}\"\ntags = [\"weekly\"]\n",
    );
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &edited)
        .args(["tpl", "edit", "weekly-hygiene"])
        .assert()
        .success();

    let assert = env
        .cmd()
        .args(["tpl", "show", "weekly-hygiene", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["description"], "edited");
    assert_eq!(out["goal"], "tidy up on {{date}}");
    // Everything the edited file left out is now blank — the file is the whole
    // definition, not a patch.
    assert_eq!(out["workflow"], serde_json::Value::Null);
    assert_eq!(out["master_prompt"], serde_json::Value::Null);
    assert_eq!(out["create_brain"], false);
    assert_eq!(out["tags"], serde_json::json!(["weekly"]));
}

#[test]
fn tpl_edit_reports_what_the_editor_saved_and_changes_nothing_when_it_is_broken() {
    let env = Env::new();
    env.cmd().args(["tpl", "add", "routine"]).assert().success();

    let broken = write_file(&env, "broken.toml", "[[template]\nname = ?\n");
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &broken)
        .args(["tpl", "edit", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid TOML"));

    let empty = write_file(&env, "empty.toml", "# nothing here\n");
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &empty)
        .args(["tpl", "edit", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no [[template]]"));

    let two = write_file(
        &env,
        "two.toml",
        "[[template]]\nname = \"a\"\n\n[[template]]\nname = \"b\"\n",
    );
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &two)
        .args(["tpl", "edit", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("q tpl import"));

    env.cmd()
        .env("Q_FIXTURE_EDITOR_FAIL", "1")
        .args(["tpl", "edit", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("editor"));

    // Still exactly the one template it was.
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    let rows = json_of(&assert);
    assert_eq!(rows.as_array().unwrap().len(), 1, "{rows}");
    assert_eq!(rows[0]["name"], "routine");
}

#[test]
fn tpl_edit_can_rename_but_not_onto_a_name_that_exists() {
    let env = Env::new();
    for name in ["routine", "other"] {
        env.cmd().args(["tpl", "add", name]).assert().success();
    }
    let clash = write_file(&env, "clash.toml", "[[template]]\nname = \"other\"\n");
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &clash)
        .args(["tpl", "edit", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));

    let renamed = write_file(&env, "renamed.toml", "[[template]]\nname = \"renamed\"\n");
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &renamed)
        .args(["tpl", "edit", "routine"])
        .assert()
        .success();
    env.cmd()
        .args(["tpl", "show", "renamed"])
        .assert()
        .success();
}

#[test]
fn tpl_rm_asks_first_and_removes_on_yes() {
    let env = Env::new();
    env.cmd().args(["tpl", "add", "routine"]).assert().success();

    // No scripted answer: the question is refused, and nothing is removed.
    env.cmd()
        .args(["tpl", "rm", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("aborted"));
    env.cmd()
        .args(["tpl", "show", "routine"])
        .assert()
        .success();

    env.cmd()
        .env("Q_FIXTURE_CONFIRM", "y")
        .args(["tpl", "rm", "routine"])
        .assert()
        .success()
        .stderr(predicate::str::contains("remove template routine?"))
        .stdout(predicate::str::contains("removed template routine"));
    env.cmd()
        .args(["tpl", "show", "routine"])
        .assert()
        .failure();
}

/// A template is a definition; the Quests made from it are history and outlive
/// it, with only their `template_id` cleared.
#[test]
fn tpl_rm_unlinks_the_quests_that_came_from_the_template() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", work.to_str().unwrap()])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "run", "routine", "-d", "--json"])
        .assert()
        .success();
    let quest_id = json_of(&assert)["quest"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let assert = env
        .cmd()
        .args(["tpl", "rm", "routine", "-f", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["unlinked_quests"], 1);

    let template_id: Option<String> = env
        .conn()
        .query_row(
            "SELECT template_id FROM quest WHERE id = ?1",
            [&quest_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(template_id, None);
}

#[test]
fn tpl_run_creates_a_quest_from_the_template_and_counts_the_run() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    let before = today_iso();
    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly", "--arg", "repo=work", "-d", "--json"])
        .assert()
        .success();
    let after = today_iso();
    let out = json_of(&assert);

    // Read on both sides of the command: a suite that computes "today" only
    // afterwards fails once a year, at midnight.
    let goal = out["quest"]["goal"].as_str().unwrap().to_string();
    assert!(
        goal == format!("tidy work on {before}") || goal == format!("tidy work on {after}"),
        "{goal}"
    );
    assert_eq!(out["quest"]["cwd"], work.to_str().unwrap());
    assert_eq!(out["quest"]["workflow"], "routine");
    assert_eq!(out["quest"]["template_id"], out["template"]["id"]);
    assert_eq!(out["session"]["first_prompt"], "start with the lint report");
    assert_eq!(out["attach"], "none");
    assert_eq!(out["template"]["run_count"], 1);
    assert!(out["template"]["last_run_at"].is_i64(), "{out}");

    // Every run counts.
    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly", "--arg", "repo=work", "-d", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["template"]["run_count"], 2);
}

#[test]
fn tpl_run_prints_a_human_one_liner_naming_the_template() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", work.to_str().unwrap()])
        .assert()
        .success();
    env.cmd()
        .args(["tpl", "run", "routine", "-d"])
        .assert()
        .success()
        .stdout(predicate::str::contains("from template routine"))
        .stdout(predicate::str::contains("q enter "));
}

/// A NULL `cwd` means "wherever the run happens" (SPEC §11).
#[test]
fn tpl_run_without_a_template_cwd_uses_the_current_directory() {
    let env = Env::new();
    let work = env.work("elsewhere");
    env.cmd().args(["tpl", "add", "routine"]).assert().success();
    let assert = env
        .cmd()
        .current_dir(&work)
        .args(["tpl", "run", "routine", "-d", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["quest"]["cwd"], work.to_str().unwrap());
}

#[test]
fn tpl_run_refuses_to_start_with_an_unfilled_placeholder() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly", "-d", "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "invalid", "{parsed}");
    let message = parsed["error"].as_str().unwrap();
    assert!(message.contains("goal"), "{message}");
    assert!(message.contains("no --arg for `repo`"), "{message}");

    // Nothing was created, and the run was not counted.
    let quests = quests_of(&env.cmd().args(["list", "--json"]).assert().success());
    assert_eq!(quests.as_array().unwrap().len(), 0, "{quests}");
    let assert = env
        .cmd()
        .args(["tpl", "show", "weekly", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["run_count"], 0);
}

#[test]
fn tpl_run_rejects_an_arg_that_is_not_a_pair() {
    let env = Env::new();
    env.cmd().args(["tpl", "add", "routine"]).assert().success();
    for bad in ["nope", "=1"] {
        env.cmd()
            .args(["tpl", "run", "routine", "--arg", bad, "-d"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("--arg"));
    }
    env.cmd()
        .args([
            "tpl", "run", "routine", "--arg", "a=1", "--arg", "a=2", "-d",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("twice"));
}

#[test]
fn tpl_export_and_import_round_trip_a_template() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);

    let assert = env.cmd().args(["tpl", "export"]).assert().success();
    let toml = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(toml.contains("[[template]]"), "{toml}");
    assert!(toml.contains("name = \"weekly-hygiene\""), "{toml}");
    // The history is the database's; a file must not be able to rewrite it.
    assert!(!toml.contains("run_count"), "{toml}");
    assert!(!toml.contains("last_run_at"), "{toml}");

    // Into a second, empty q.
    let other = Env::new();
    let file = write_file(&other, "in.toml", &toml);
    other
        .cmd()
        .args(["tpl", "import", file.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("weekly-hygiene"));

    let before = json_of(
        &env.cmd()
            .args(["tpl", "show", "weekly", "--json"])
            .assert()
            .success(),
    );
    let after = json_of(
        &other
            .cmd()
            .args(["tpl", "show", "weekly", "--json"])
            .assert()
            .success(),
    );
    for field in [
        "name",
        "description",
        "cwd",
        "workflow",
        "goal",
        "master_prompt",
        "beads_repo",
        "create_brain",
        "tags",
    ] {
        assert_eq!(before[field], after[field], "{field}");
    }
}

#[test]
fn tpl_export_of_one_name_is_the_same_document_as_all_of_them() {
    let env = Env::new();
    for name in ["deps-audit", "routine"] {
        env.cmd().args(["tpl", "add", name]).assert().success();
    }
    let assert = env
        .cmd()
        .args(["tpl", "export", "deps-audit"])
        .assert()
        .success();
    let toml = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(toml.contains("deps-audit"), "{toml}");
    assert!(!toml.contains("routine"), "{toml}");

    let assert = env
        .cmd()
        .args(["tpl", "export", "--json"])
        .assert()
        .success();
    let doc = json_of(&assert);
    let names: Vec<&str> = doc["template"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["deps-audit", "routine"]);
}

#[test]
fn tpl_import_refuses_an_existing_name_unless_replace_is_given() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "routine", "--goal", "the old goal"])
        .assert()
        .success();
    let file = write_file(
        &env,
        "in.toml",
        "[[template]]\nname = \"routine\"\ngoal = \"the new goal\"\n",
    );

    let assert = env
        .cmd()
        .args(["tpl", "import", file.to_str().unwrap(), "--json"])
        .assert()
        .failure();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "conflict", "{parsed}");
    assert!(
        parsed["error"].as_str().unwrap().contains("--replace"),
        "{parsed}"
    );
    let assert = env
        .cmd()
        .args(["tpl", "show", "routine", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["goal"], "the old goal");

    let assert = env
        .cmd()
        .args([
            "tpl",
            "import",
            file.to_str().unwrap(),
            "--replace",
            "--json",
        ])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["replaced"], serde_json::json!(["routine"]));
    assert_eq!(out["added"], serde_json::json!([]));
    let assert = env
        .cmd()
        .args(["tpl", "show", "routine", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["goal"], "the new goal");
}

/// Run stats are history, not definition: `--replace` overwrites what the
/// template *is* and keeps the record of how often it has been used.
#[test]
fn tpl_import_replace_keeps_the_run_stats_and_the_id() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", work.to_str().unwrap()])
        .assert()
        .success();
    env.cmd()
        .args(["tpl", "run", "routine", "-d"])
        .assert()
        .success();
    let before = json_of(
        &env.cmd()
            .args(["tpl", "show", "routine", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(before["run_count"], 1);

    let file = write_file(
        &env,
        "in.toml",
        "[[template]]\nname = \"routine\"\ngoal = \"replaced\"\n",
    );
    env.cmd()
        .args(["tpl", "import", file.to_str().unwrap(), "--replace"])
        .assert()
        .success();

    let after = json_of(
        &env.cmd()
            .args(["tpl", "show", "routine", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(after["goal"], "replaced");
    assert_eq!(after["run_count"], before["run_count"]);
    assert_eq!(after["last_run_at"], before["last_run_at"]);
    assert_eq!(after["id"], before["id"]);
    // The definition really was replaced, not merged.
    assert_eq!(after["cwd"], serde_json::Value::Null);
}

#[test]
fn tpl_import_is_all_or_nothing() {
    let env = Env::new();
    let file = write_file(
        &env,
        "in.toml",
        "[[template]]\nname = \"good\"\n\n[[template]]\nname = \"Bad Name\"\n",
    );
    env.cmd()
        .args(["tpl", "import", file.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("template name"));
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    assert_eq!(
        json_of(&assert),
        serde_json::json!([]),
        "a half import landed"
    );

    let twice = write_file(
        &env,
        "twice.toml",
        "[[template]]\nname = \"same\"\n\n[[template]]\nname = \"same\"\n",
    );
    env.cmd()
        .args(["tpl", "import", twice.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("twice"));
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    assert_eq!(json_of(&assert), serde_json::json!([]));
}

#[test]
fn tpl_import_reports_a_file_it_cannot_use() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "import", "/definitely/not/here.toml"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("cannot read"));

    let empty = write_file(&env, "empty.toml", "# nothing\n");
    env.cmd()
        .args(["tpl", "import", empty.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no [[template]]"));

    let unknown = write_file(
        &env,
        "unknown.toml",
        "[[template]]\nname = \"a\"\nrun_count = 9\n",
    );
    env.cmd()
        .args(["tpl", "import", unknown.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run_count"));
}

#[test]
fn tpl_import_reads_stdin_for_a_dash() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "import", "-", "--json"])
        .write_stdin("[[template]]\nname = \"piped\"\n")
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "piped", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["name"], "piped");
}

#[test]
fn tpl_from_builds_a_template_out_of_a_quest() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "cdc-backfill",
            "--goal",
            "make it idempotent",
        ])
        .args([
            "--workflow",
            "orchestrator",
            "--dir",
            work.to_str().unwrap(),
        ])
        .args(["--prompt", "read the CDC docs first", "-d"])
        .assert()
        .success();

    env.cmd()
        .args(["tpl", "from", "cdc-backfill", "cdc-routine"])
        .assert()
        .success()
        .stdout(predicate::str::contains("created template cdc-routine"));

    let assert = env
        .cmd()
        .args(["tpl", "show", "cdc-routine", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["goal"], "make it idempotent");
    assert_eq!(out["cwd"], work.to_str().unwrap());
    assert_eq!(out["workflow"], "orchestrator");
    assert_eq!(out["master_prompt"], "read the CDC docs first");
    assert_eq!(out["run_count"], 0);
}

#[test]
fn tpl_from_reports_a_quest_and_a_name_it_cannot_use() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "alpha",
            "--dir",
            work.to_str().unwrap(),
            "-d",
        ])
        .assert()
        .success();
    env.cmd()
        .args(["tpl", "from", "nope", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("quest `nope`"));

    env.cmd().args(["tpl", "add", "routine"]).assert().success();
    env.cmd()
        .args(["tpl", "from", "alpha", "routine"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

/// A template is a row in this machine's database, so nothing about `q tpl`
/// reaches for ssh even with a remote configured (SPEC §15, `proxy::route`).
#[test]
fn tpl_is_never_proxied_to_a_remote() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.cmd().args(["tpl", "add", "routine"]).assert().success();
    env.cmd().args(["tpl", "list"]).assert().success();
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());
}

#[test]
fn tpl_is_silent_under_quiet() {
    let env = Env::new();
    env.cmd()
        .args(["--quiet", "tpl", "add", "routine"])
        .assert()
        .success()
        .stdout("");
    env.cmd()
        .args(["--quiet", "tpl", "list"])
        .assert()
        .success()
        .stdout("");
    env.cmd()
        .args(["--quiet", "tpl", "show", "routine"])
        .assert()
        .success()
        .stdout("");
}

// --------------------------------------------------- bd-8lz.6.1 review fixes

/// A relative `--cwd` has to pin the directory it was typed in, exactly as
/// `q new --dir .` does. Stored raw it pinned nothing: the template behaved
/// like a NULL `cwd` and the routine ran wherever it was invoked from.
#[test]
fn tpl_pins_a_relative_cwd_to_the_directory_it_was_typed_in() {
    let env = Env::new();
    let here = env.work("here");
    let there = env.work("there");

    env.cmd()
        .current_dir(&here)
        .args(["tpl", "add", "rel", "--cwd", "."])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "rel", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["cwd"], here.to_str().unwrap());

    // Run from somewhere else, it still lands in the directory it was pinned to.
    let assert = env
        .cmd()
        .current_dir(&there)
        .args(["tpl", "run", "rel", "-d", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["quest"]["cwd"], here.to_str().unwrap());

    // `q tpl edit --cwd` resolves the same way.
    env.cmd()
        .current_dir(&there)
        .args(["tpl", "edit", "rel", "--cwd", "."])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "rel", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["cwd"], there.to_str().unwrap());
}

/// A `cwd` is checked when it is *set* and when it is *run*, and at no other
/// time: an edit that never touched it must not be refused because of it.
#[test]
fn a_stale_cwd_blocks_a_run_and_nothing_else() {
    let env = Env::new();
    let gone = env.work("gone");
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", gone.to_str().unwrap()])
        .args(["--description", "before"])
        .assert()
        .success();
    std::fs::remove_dir_all(&gone).unwrap();

    env.cmd()
        .args(["tpl", "edit", "routine", "--description", "after"])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "routine", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["description"], "after");
    assert_eq!(out["cwd"], gone.to_str().unwrap());

    // Running it is where the directory has to be there — and the message
    // names the template and the directory.
    let assert = env
        .cmd()
        .args(["tpl", "run", "routine", "-d", "--json"])
        .assert()
        .failure();
    let message = error_json(&assert)["error"].as_str().unwrap().to_string();
    assert!(message.contains("template `routine`"), "{message}");
    assert!(message.contains(gone.to_str().unwrap()), "{message}");

    // Setting a bad one is still refused, naming the field and the template.
    env.cmd()
        .args(["tpl", "edit", "routine", "--cwd", "/definitely/not/here"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("routine: cwd"));
}

/// `q tpl export | ssh <alias> q tpl import -`: a definition travels to a
/// machine that has not checked its repository out yet, and one absent
/// directory must not fail the whole all-or-nothing import.
#[test]
fn an_import_never_checks_a_directory_the_far_machine_may_not_have() {
    let env = Env::new();
    let file = write_file(
        &env,
        "in.toml",
        "[[template]]\nname = \"a\"\n\n[[template]]\nname = \"b\"\n\
         cwd = \"/Users/someone-else/Code/work\"\n",
    );
    env.cmd()
        .args(["tpl", "import", file.to_str().unwrap()])
        .assert()
        .success();
    let assert = env.cmd().args(["tpl", "list", "--json"]).assert().success();
    assert_eq!(json_of(&assert).as_array().unwrap().len(), 2);
    let assert = env
        .cmd()
        .args(["tpl", "show", "b", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["cwd"], "/Users/someone-else/Code/work");
}

/// Every `q tpl` subcommand but `run` names a definition that is *this*
/// machine's row, so a `--machine <other>` on it is refused rather than
/// ignored — it used to create nothing remote while suggesting it had.
/// `q tpl run --machine` is the exception and is tested separately.
#[test]
fn tpl_refuses_a_machine_it_cannot_reach_rather_than_ignoring_it() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.cmd().args(["tpl", "add", "routine"]).assert().success();

    for action in [
        vec!["tpl", "list"],
        vec!["tpl", "edit", "routine", "--description", "x"],
        vec!["tpl", "export", "routine"],
    ] {
        env.cmd()
            .args(["--machine", "ws"])
            .args(action)
            .assert()
            .failure()
            .stderr(predicate::str::contains("q tpl export"));
    }
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());

    // This machine's own name is not a remote.
    env.cmd()
        .args(["--machine", "laptop", "tpl", "list"])
        .assert()
        .success();
}

/// A far-end `q tpl run --json` reply: the same envelope the real command emits,
/// with the two fields the proxy reads back — `quest.slug` and `tmux_session`.
fn tpl_run_reply(slug: &str) -> serde_json::Value {
    serde_json::json!({
        "template": { "name": "weekly" },
        "quest": { "id": "q-1234", "slug": slug, "machine": "ws" },
        "session": { "id": "s-0001" },
        "tmux_session": format!("q-{slug}"),
        "attach": "none",
    })
}

/// SPEC §15: `q tpl run <name> --machine ws` reaches ws, which reads **its own**
/// template of that name and creates the Quest there — never here. `-d --json`
/// are added and `--machine` does not travel, exactly as `q new --machine` does.
#[test]
fn tpl_run_on_a_remote_runs_it_there_and_not_here() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = serde_json::json!({
        "ws-host": { "proxied": { "stdout": tpl_run_reply("weekly-run").to_string() } },
    });
    let assert = env
        .over_ssh(
            hosts,
            &[
                "--machine",
                "ws",
                "tpl",
                "run",
                "weekly",
                "--arg",
                "k=v",
                "-d",
                "--json",
            ],
        )
        .success();

    // Built, not forwarded: the far end's line is `q tpl run weekly --arg k=v`
    // with `-d --json --no-remote` — the user's own `-d`/`--json` subsumed, and
    // `--machine` gone (over there it would name a machine `q` has never heard).
    assert_eq!(
        env.proxy_calls(),
        ["ws-host\tq\ttpl\trun\tweekly\t--arg\t'k=v'\t-d\t--json\t--no-remote"]
    );
    let out = json_of(&assert);
    assert_eq!(out["machine"], "ws", "{out}");
    assert_eq!(out["remote"], true, "{out}");
    assert_eq!(out["quest"]["slug"], "weekly-run", "{out}");

    // Nothing was written here.
    assert_eq!(env.count("SELECT COUNT(*) FROM quest"), 0);
}

/// …and then enter (SPEC §15), unless `-d` said not to. The tmux session name
/// comes from the machine that created it.
#[test]
fn tpl_run_on_a_remote_attaches_to_the_machine_that_created_it() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = serde_json::json!({
        "ws-host": { "proxied": { "stdout": tpl_run_reply("weekly-run").to_string() } },
    });
    env.over_ssh(hosts, &["--machine", "ws", "tpl", "run", "weekly"])
        .success()
        .stdout(predicate::str::contains("created quest weekly-run on ws"));
    assert_eq!(
        attach_calls(&env),
        ["attach\tws-host\ttmux\tattach\t-t\t'=q-weekly-run'"]
    );
}

/// A `q tpl run` that fails over there (e.g. ws has no template by that name)
/// fails here, with what the far end said and the machine it ran on.
#[test]
fn a_remote_tpl_run_that_fails_says_what_the_far_end_said() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let hosts = serde_json::json!({
        "ws-host": {
            "proxied": {
                "stderr": "{\"error\":\"no template `weekly`\",\"code\":\"not_found\"}\n",
                "exit": 1,
            },
        },
    });
    env.over_ssh(hosts, &["--machine", "ws", "tpl", "run", "weekly", "-d"])
        .code(1)
        .stderr(predicate::str::contains("no template `weekly`"))
        .stderr(predicate::str::contains("on ws"));
    assert_eq!(env.count("SELECT COUNT(*) FROM quest"), 0);
}

/// `--machine <this machine>` is not a remote, so `q tpl run` runs locally with
/// no ssh at all — the template is read here and the Quest is created here.
#[test]
fn tpl_run_with_the_local_machine_name_runs_here() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    let work = env.work("repo");
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", work.to_str().unwrap()])
        .assert()
        .success();
    let mut cmd = env.cmd();
    env.with_ssh(&mut cmd, serde_json::json!({}));
    cmd.args([
        "--machine",
        "laptop",
        "tpl",
        "run",
        "routine",
        "-d",
        "--json",
    ])
    .assert()
    .success();
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());
    assert_eq!(env.count("SELECT COUNT(*) FROM quest"), 1);
}

/// SPEC §4's third `name_source`: a routine run three times is three
/// recognisable rows in `q list`, not three model-invented names.
#[test]
fn a_templated_quest_is_named_after_its_template() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "tpl",
            "add",
            "weekly-hygiene",
            "--cwd",
            work.to_str().unwrap(),
        ])
        .assert()
        .success();

    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly-hygiene", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["slug"], "weekly-hygiene");
    assert_eq!(out["quest"]["name_source"], "template");

    // Run again: the slug steps aside the way an auto one does rather than
    // failing on the first run's row.
    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly-hygiene", "-d", "--json"])
        .assert()
        .success();
    let again = json_of(&assert);
    assert_eq!(again["quest"]["slug"], "weekly-hygiene-2");
    assert_eq!(again["quest"]["name_source"], "template");
}

/// The point of `name_source = 'template'`: `naming::schedule` gates on
/// `auto`, so the master's first `Stop` hook must not queue an LLM rename over
/// the name the template just gave the Quest.
#[test]
fn the_stop_hook_schedules_no_rename_for_a_templated_quest() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "tpl",
            "add",
            "weekly-hygiene",
            "--cwd",
            work.to_str().unwrap(),
        ])
        .assert()
        .success();
    let templated = json_of(
        &env.cmd()
            .args(["tpl", "run", "weekly-hygiene", "-d", "--json"])
            .assert()
            .success(),
    );
    let auto = json_of(
        &env.cmd()
            .args(["new", "--dir", work.to_str().unwrap(), "-d", "--json"])
            .assert()
            .success(),
    );

    let spawns = env.dir.path().join("spawns.jsonl");
    let stop = |quest: &serde_json::Value| {
        env.cmd()
            .env("Q_NO_DETACH", &spawns)
            .env("Q_QUEST", quest["quest"]["id"].as_str().unwrap())
            .env("Q_SESSION", quest["session"]["id"].as_str().unwrap())
            .args(["hook", "stop"])
            .write_stdin("{}")
            .assert()
            .success();
    };

    stop(&templated);
    assert!(
        !spawns.exists(),
        "a templated Quest was queued for an LLM rename: {}",
        std::fs::read_to_string(&spawns).unwrap_or_default()
    );
    // The same hook does schedule one for an auto-named Quest, so the
    // assertion above is about the gate and not about the fixture.
    stop(&auto);
    let recorded = std::fs::read_to_string(&spawns).unwrap();
    assert!(recorded.contains("--auto"), "{recorded}");
}

/// `q new` accepts `{{…}}` in a goal or a prompt, so capturing one must not be
/// the single place that refuses it.
#[test]
fn tpl_from_captures_a_quest_whose_text_looks_like_a_template() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "new",
            "--name",
            "mustache",
            "--goal",
            "render {{user.name}}",
        ])
        .args(["--dir", work.to_str().unwrap()])
        .args(["--prompt", "fix the {{handlebars}} block", "-d"])
        .assert()
        .success();

    env.cmd()
        .args(["tpl", "from", "mustache", "mustache-tpl"])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "mustache-tpl", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["goal"], "render {{{{user.name}}}}");
    assert_eq!(out["master_prompt"], "fix the {{{{handlebars}}}} block");

    // And it runs back out as exactly what the Quest said.
    let assert = env
        .cmd()
        .args(["tpl", "run", "mustache-tpl", "-d", "--json"])
        .assert()
        .success();
    let run = json_of(&assert);
    assert_eq!(run["quest"]["goal"], "render {{user.name}}");
    assert_eq!(
        run["session"]["first_prompt"],
        "fix the {{handlebars}} block"
    );
}

#[test]
fn a_doubled_brace_is_how_a_template_carries_a_literal_one() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["tpl", "add", "lit", "--cwd", work.to_str().unwrap()])
        .args(["--goal", "literal {{{{arg.x}}}} on {{date}}"])
        .assert()
        .success();
    let before = today_iso();
    let assert = env
        .cmd()
        .args(["tpl", "run", "lit", "-d", "--json"])
        .assert()
        .success();
    let after = today_iso();
    let goal = json_of(&assert)["quest"]["goal"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        goal == format!("literal {{{{arg.x}}}} on {before}")
            || goal == format!("literal {{{{arg.x}}}} on {after}"),
        "{goal}"
    );
}

/// The module doc claims every offending key at once; that has to be true per
/// run, not per field.
#[test]
fn tpl_run_names_every_field_it_cannot_fill_in_one_error() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "both", "--goal", "g {{arg.a}}"])
        .args(["--prompt", "p {{arg.b}}"])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "run", "both", "-d", "--json"])
        .assert()
        .failure();
    let message = error_json(&assert)["error"].as_str().unwrap().to_string();
    assert!(message.contains("goal: no --arg for `a`"), "{message}");
    assert!(
        message.contains("master_prompt: no --arg for `b`"),
        "{message}"
    );
}

/// A mistyped `--arg` is otherwise only ever caught by leaving some *other*
/// key unfilled.
#[test]
fn tpl_run_warns_about_an_arg_no_placeholder_uses() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["tpl", "add", "routine", "--cwd", work.to_str().unwrap()])
        .args(["--goal", "tidy {{arg.k}}"])
        .assert()
        .success();
    env.cmd()
        .args([
            "tpl", "run", "routine", "--arg", "k=1", "--arg", "typoo=2", "-d",
        ])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "template routine has no placeholder for --arg `typoo`",
        ));
}

/// `insert` and `import` trim a name; the editor rename path has to agree.
#[test]
fn tpl_edit_trims_a_renamed_name_as_add_and_import_do() {
    let env = Env::new();
    env.cmd().args(["tpl", "add", "hy"]).assert().success();
    let renamed = write_file(
        &env,
        "renamed.toml",
        "[[template]]\nname = \"  hy-renamed  \"\n",
    );
    env.cmd()
        .env("Q_FIXTURE_EDITOR", &renamed)
        .args(["tpl", "edit", "hy"])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "hy-renamed", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["name"], "hy-renamed");
}

/// A scripted backup and restore of an empty database is a no-op, not a
/// failure. A file that never mentions `template` still is one.
#[test]
fn an_export_of_an_empty_database_imports_again() {
    let env = Env::new();
    let assert = env.cmd().args(["tpl", "export"]).assert().success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let file = write_file(&env, "backup.toml", &text);
    let assert = env
        .cmd()
        .args(["tpl", "import", file.to_str().unwrap(), "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["added"], serde_json::json!([]));
    assert_eq!(out["replaced"], serde_json::json!([]));

    let nothing = write_file(&env, "nothing.toml", "# not a template file\n");
    env.cmd()
        .args(["tpl", "import", nothing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no [[template]]"));
}

/// The dangerous half of "all or nothing": a replace that already ran an
/// UPDATE before the file's next entry turned out to be bad.
#[test]
fn tpl_import_replace_rolls_back_a_definition_it_already_changed() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "keeper", "--goal", "original"])
        .args(["--description", "first"])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "show", "keeper", "--json"])
        .assert()
        .success();
    let before = json_of(&assert);

    let file = write_file(
        &env,
        "replace.toml",
        "[[template]]\nname = \"keeper\"\ngoal = \"clobbered\"\n\n\
         [[template]]\nname = \"Bad Name\"\n",
    );
    env.cmd()
        .args(["tpl", "import", file.to_str().unwrap(), "--replace"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("template name"));

    let assert = env
        .cmd()
        .args(["tpl", "show", "keeper", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert), before, "the replace was not rolled back");
}

/// The half of `q tpl edit`'s round trip a user sees: the editor opens on the
/// template's current definition. Without this assertion `from_editor` could
/// pass `""` and every other `tpl edit` test would still pass.
#[test]
fn tpl_edit_opens_the_editor_on_the_current_definition() {
    let env = Env::new();
    let work = env.work("repo");
    add_full_template(&env, "weekly-hygiene", &work);
    let seen = env.dir.path().join("seen.toml");
    let reply = write_file(
        &env,
        "reply.toml",
        "[[template]]\nname = \"weekly-hygiene\"\ndescription = \"edited\"\n",
    );

    env.cmd()
        .env("Q_FIXTURE_EDITOR_SEEN", &seen)
        .env("Q_FIXTURE_EDITOR", &reply)
        .args(["tpl", "edit", "weekly-hygiene"])
        .assert()
        .success();

    let buffer = std::fs::read_to_string(&seen).unwrap();
    for expected in [
        "[[template]]",
        "name = \"weekly-hygiene\"",
        "the Monday routine",
        "tidy {{arg.repo}} on {{date}}",
        "start with the lint report",
        "routine",
        "weekly",
        work.to_str().unwrap(),
    ] {
        assert!(buffer.contains(expected), "`{expected}` missing:\n{buffer}");
    }
}

/// SPEC §16's `q new --template`: the definition fills the blanks and a typed
/// flag always wins. Not a synonym for `q tpl run`, which takes the whole
/// definition and has no `--name` / `--goal`.
#[test]
fn new_template_fills_the_blanks_and_a_flag_always_wins() {
    let env = Env::new();
    let from_template = env.work("template-dir");
    let from_flag = env.work("flag-dir");
    env.cmd()
        .args([
            "tpl",
            "add",
            "starter",
            "--cwd",
            from_template.to_str().unwrap(),
        ])
        .args(["--goal", "the template goal", "--prompt", "template prompt"])
        .args(["--workflow", "routine"])
        .assert()
        .success();

    let assert = env
        .cmd()
        .args(["new", "--template", "starter", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["goal"], "the template goal");
    assert_eq!(out["quest"]["cwd"], from_template.to_str().unwrap());
    assert_eq!(out["quest"]["workflow"], "routine");
    assert_eq!(out["quest"]["slug"], "starter");
    assert_eq!(out["quest"]["name_source"], "template");
    assert_eq!(out["session"]["first_prompt"], "template prompt");

    let assert = env
        .cmd()
        .args(["tpl", "show", "starter", "--json"])
        .assert()
        .success();
    let template = json_of(&assert);
    assert_eq!(out["quest"]["template_id"], template["id"]);
    // A Quest made from a definition counts as a run of it, whichever command
    // asked for it.
    assert_eq!(template["run_count"], 1);

    let assert = env
        .cmd()
        .args(["new", "--template", "starter", "--name", "typed"])
        .args(["--goal", "mine", "--dir", from_flag.to_str().unwrap()])
        .args(["--prompt", "my prompt", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["goal"], "mine");
    assert_eq!(out["quest"]["cwd"], from_flag.to_str().unwrap());
    assert_eq!(out["quest"]["slug"], "typed");
    assert_eq!(out["quest"]["name_source"], "manual");
    assert_eq!(out["session"]["first_prompt"], "my prompt");
    assert_eq!(out["quest"]["template_id"], template["id"]);
    let assert = env
        .cmd()
        .args(["tpl", "show", "starter", "--json"])
        .assert()
        .success();
    assert_eq!(json_of(&assert)["run_count"], 2);
}

#[test]
fn new_template_reports_a_template_it_cannot_use() {
    let env = Env::new();
    env.cmd()
        .args(["tpl", "add", "needs-arg", "--goal", "tidy {{arg.repo}}"])
        .assert()
        .success();
    env.cmd()
        .args(["new", "--template", "nope", "-d"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("template `nope`"));
    // A form has nowhere to type `--arg` and neither has `q new`; the command
    // that can is named.
    env.cmd()
        .args(["new", "--template", "needs-arg", "-d"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("q tpl run needs-arg --arg"));
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
}

/// The brief is read by the master agent, for which `t-f5b1` says nothing.
#[test]
fn brief_names_the_template_a_quest_came_from() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args([
            "tpl",
            "add",
            "weekly-hygiene",
            "--cwd",
            work.to_str().unwrap(),
        ])
        .assert()
        .success();
    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly-hygiene", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    let slug = out["quest"]["slug"].as_str().unwrap().to_string();
    let id = out["template"]["id"].as_str().unwrap().to_string();

    let assert = env.cmd().args(["brief", &slug]).assert().success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        text.contains(&format!("- **template**: weekly-hygiene ({id})")),
        "{text}"
    );

    // The definition can go; the Quest and its brief stay.
    env.cmd()
        .args(["tpl", "rm", "weekly-hygiene", "-f"])
        .assert()
        .success();
    let assert = env.cmd().args(["brief", &slug]).assert().success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(text.contains("- **template**: -"), "{text}");
}

/// A template names a row in *this* machine's database, so `--template` with
/// a remote `--machine` cannot mean what it says.
#[test]
fn new_template_is_refused_for_a_remote_machine() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    env.cmd().args(["tpl", "add", "starter"]).assert().success();
    env.cmd()
        .args(["--machine", "ws", "new", "--template", "starter", "-d"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("q tpl export starter | ssh ws"));
    assert!(env.proxy_calls().is_empty(), "{:?}", env.proxy_calls());
}

// ---------------------------------------------------------------- q workflow
//
// SPEC §11's workflow files. `Q_CONFIG` points at `config.toml` inside the
// sandbox, so `workflows/` beside it is the directory every one of these
// reads and writes — no test touches the real `~/.config/q`, and none of them
// launches a real editor (`Q_FIXTURE` gates that; see `src/editor.rs`).

const BUILTINS: [&str; 5] = ["orchestrator", "research", "review", "routine", "solo"];

/// `Env::cmd` with an "editor" that saves `body`, and optionally records the
/// buffer it was handed.
fn with_editor(env: &Env, body: &str, seen: Option<&std::path::Path>) -> Command {
    let reply = write_file(env, "editor-reply.md", body);
    let mut cmd = env.cmd();
    cmd.env("Q_FIXTURE_EDITOR", &reply);
    if let Some(seen) = seen {
        cmd.env("Q_FIXTURE_EDITOR_SEEN", seen);
    }
    cmd
}

#[test]
fn workflow_list_reports_the_five_builtins() {
    let env = Env::new();
    let assert = env.cmd().args(["workflow", "list"]).assert().success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(text.starts_with("NAME"), "{text}");
    for name in BUILTINS {
        assert!(text.contains(name), "`{name}` missing:\n{text}");
    }
    assert!(text.contains("builtin"), "{text}");

    let rows = env.json(&["workflow", "list"]);
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 5, "{rows:#?}");
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, BUILTINS, "sorted, and exactly the five");
    for row in rows {
        assert_eq!(row["source"], "builtin");
        assert_eq!(row["path"], serde_json::Value::Null);
        assert!(row["chars"].as_u64().unwrap() > 500);
    }
    // Only `solo` has no worker section — it is the one master, no workers.
    let worker: Vec<(&str, bool)> = rows
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap(),
                r["has_worker_section"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        worker,
        [
            ("orchestrator", true),
            ("research", true),
            ("review", true),
            ("routine", true),
            ("solo", false),
        ]
    );
}

#[test]
fn workflow_show_prints_the_markdown_and_its_worker_half() {
    let env = Env::new();
    let assert = env
        .cmd()
        .args(["workflow", "show", "orchestrator"])
        .assert()
        .success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(text.starts_with("# orchestrator"), "{text}");
    assert!(text.contains("q spawn"), "{text}");
    assert!(text.contains("## worker"), "{text}");

    let out = env.json(&["workflow", "show", "orchestrator"]);
    assert_eq!(out["name"], "orchestrator");
    assert_eq!(out["source"], "builtin");
    assert_eq!(out["for"], "master");
    assert_eq!(out["has_worker_section"], true);
    assert_eq!(out["whole_file"], true);
    // stdout is the body with `emit`'s newline; the payload carries the file.
    assert_eq!(out["body"].as_str().unwrap().trim_end(), text.trim_end());

    // `--worker` is what a worker's brief would actually be handed.
    let out = env.json(&["workflow", "show", "orchestrator", "--worker"]);
    assert_eq!(out["for"], "worker");
    assert_eq!(out["whole_file"], false);
    let body = out["body"].as_str().unwrap();
    assert!(
        body.contains("Do **only** the stage you were given"),
        "{body}"
    );
    assert!(!body.contains("## worker"), "{body}");
    assert!(!body.contains("Choosing the pipeline"), "{body}");

    // A workflow with no worker section hands a worker the whole file, and
    // says so out loud — `whole_file` in `--json` is not visible to a human.
    let out = env.json(&["workflow", "show", "solo", "--worker"]);
    assert_eq!(out["has_worker_section"], false);
    assert_eq!(out["whole_file"], true);
    assert!(out["body"].as_str().unwrap().starts_with("# solo"));
    let assert = env
        .cmd()
        .args(["workflow", "show", "solo", "--worker"])
        .assert()
        .success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("defines no `## worker` section"),
        "{stderr}"
    );
    // The note is on stderr, so the body still pipes clean.
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(stdout.starts_with("# solo"), "{stdout}");
    // A workflow that *does* define one says nothing.
    let assert = env
        .cmd()
        .args(["workflow", "show", "orchestrator", "--worker"])
        .assert()
        .success();
    assert!(assert.get_output().stderr.is_empty());

    // `show` gates on `--quiet` the way `q tpl show` does.
    env.cmd()
        .args(["workflow", "show", "solo", "-q"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
}

/// `q workflow rm <builtin>` used to ask "remove workflow review?" and only
/// then refuse. Nothing is going to be removed, so nothing is asked.
#[test]
fn removing_a_builtin_or_an_unknown_workflow_is_refused_without_asking() {
    let env = Env::new();
    for (name, code, needle) in [
        ("review", "invalid", "is a built-in workflow"),
        ("ghost", "not_found", "unknown workflow `ghost`"),
    ] {
        let assert = env
            .cmd()
            .args(["workflow", "rm", name, "--json"])
            .assert()
            .code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        assert_eq!(parsed["code"], code, "{parsed}");
        assert!(
            parsed["error"].as_str().unwrap().contains(needle),
            "{parsed}"
        );
        // Not "aborted": the confirm never happened.
        let assert = env.cmd().args(["workflow", "rm", name]).assert().code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(
            !stderr.contains("[y/N]"),
            "{name} was asked about: {stderr}"
        );
        assert!(stderr.contains(needle), "{name}: {stderr}");
    }
}

/// `require_opt` trimmed before it checked; the row stored the raw flag. A
/// Quest could be created whose every brief then reported a workflow that
/// "could not be read", and `"   "` set a column no `is_empty()` filter caught.
#[test]
fn a_padded_workflow_flag_is_stored_exactly_as_it_was_checked() {
    let env = Env::new();
    let work = env.work("repo");

    let out = env.json(&[
        "new",
        "--name",
        "padded",
        "--workflow",
        " solo ",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);
    assert_eq!(out["quest"]["workflow"], "solo");
    let md = env.json(&["brief", "padded"])["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(md.contains("Workflow **`solo`** (builtin)"), "{md}");
    assert!(!md.contains("could not be read"), "{md}");
    assert!(md.contains("- **workflow**: solo"), "{md}");

    // Whitespace-only is "unset" to the check, so it must be unset in the row.
    let out = env.json(&[
        "new",
        "--name",
        "blank",
        "--workflow",
        "   ",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);
    assert_eq!(out["quest"]["workflow"], serde_json::Value::Null);
    let md = env.json(&["brief", "blank"])["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(md.contains("No workflow set"), "{md}");
    assert!(md.contains("- **workflow**: -"), "{md}");

    // The three doors agree: `q set` already trimmed, and `q spawn` now does.
    assert_eq!(
        env.json(&["set", "blank", "workflow", " solo "])["quest"]["workflow"],
        "solo"
    );
    let out = env.json(&[
        "spawn",
        "blank",
        "go",
        "--label",
        "w",
        "--workflow",
        " routine ",
    ]);
    assert_eq!(out["session"]["workflow"], "routine");
}

/// `q spawn --workflow`'s help promises "default: the Quest's". The flag was
/// validated and stored, and then section 3 read the Quest's anyway.
#[test]
fn a_worker_spawned_with_its_own_workflow_is_briefed_with_it() {
    let env = Env::new();
    let work = env.work("repo");
    env.cmd()
        .args(["new", "--name", "split", "--dir", work.to_str().unwrap()])
        .args(["--workflow", "orchestrator", "-d", "-q"])
        .assert()
        .success();

    env.json(&[
        "spawn",
        "split",
        "go",
        "--label",
        "own",
        "--workflow",
        "research",
    ]);
    let md = env.json(&["brief", "split", "--session", "own"])["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(md.contains("Workflow **`research`** (builtin)"), "{md}");
    assert!(!md.contains("Workflow **`orchestrator`**"), "{md}");

    // A worker with no `--workflow` still reads the Quest's.
    env.json(&["spawn", "split", "go", "--label", "plain"]);
    let md = env.json(&["brief", "split", "--session", "plain"])["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(md.contains("Workflow **`orchestrator`** (builtin)"), "{md}");

    // And the master's own brief is the Quest's, as it always was.
    let md = env.json(&["brief", "split"])["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(md.contains("Workflow **`orchestrator`** (builtin)"), "{md}");
}

/// A `## worker` inside a fenced code block is a workflow documenting the
/// convention. It used to carve the section — leaking master-only prose to the
/// worker and losing the real section.
#[test]
fn a_worker_heading_inside_a_fence_does_not_carve_the_worker_section() {
    let env = Env::new();
    let work = env.work("repo");
    env.workflow(
        "fence",
        "# fence\n\nMaster-only: DO-NOT-LEAK\n\n```markdown\n## worker\ndocumentation of the convention\n```\n\nMore master-only: SECOND-SECRET\n\n## worker\n\nreal worker text\n",
    );

    let out = env.json(&["workflow", "show", "fence", "--worker"]);
    let body = out["body"].as_str().unwrap();
    assert_eq!(body.trim(), "real worker text", "{body}");
    assert!(!body.contains("SECOND-SECRET"), "{body}");

    // `has_worker_section` is the same question, asked by `list` and `show`.
    env.workflow(
        "faux",
        "# faux\n\n```md\n## worker\nnot a heading\n```\n\ntail\n",
    );
    let rows = env.json(&["workflow", "list"]);
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "faux")
        .unwrap();
    assert_eq!(row["has_worker_section"], false, "{row}");

    // And the worker's brief is the section, not the master's copy.
    env.cmd()
        .args(["new", "--name", "fenced", "--dir", work.to_str().unwrap()])
        .args(["--workflow", "fence", "-d", "-q"])
        .assert()
        .success();
    env.json(&["spawn", "fenced", "go", "--label", "w"]);
    let md = env.json(&["brief", "fenced", "--session", "w"])["markdown"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(md.contains("real worker text"), "{md}");
    assert!(!md.contains("SECOND-SECRET"), "{md}");
    assert!(!md.contains("DO-NOT-LEAK"), "{md}");
}

#[test]
fn workflow_show_of_an_unknown_name_lists_the_known_ones() {
    let env = Env::new();
    let assert = env
        .cmd()
        .args(["workflow", "show", "orchestartor", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
    let msg = parsed["error"].as_str().unwrap();
    assert!(msg.contains("orchestartor"), "{msg}");
    for name in BUILTINS {
        assert!(msg.contains(name), "`{name}` missing from `{msg}`");
    }

    // A malformed name is a different failure: the grammar, not the shelf.
    let assert = env
        .cmd()
        .args(["workflow", "show", "Not A Name", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "invalid");
    assert!(
        parsed["error"].as_str().unwrap().contains("workflow name"),
        "{parsed}"
    );
}

#[test]
fn workflow_add_writes_a_file_next_to_the_config_and_lists_as_user() {
    let env = Env::new();
    let body = write_file(&env, "body.md", "# triage\n\nlook at the queue.\n");

    let out = env.json(&[
        "workflow",
        "add",
        "triage",
        "--file",
        body.to_str().unwrap(),
    ]);
    assert_eq!(out["name"], "triage");
    assert_eq!(out["source"], "user");
    assert_eq!(out["action"], "created");
    assert_eq!(out["has_worker_section"], false);
    let path = env.workflow_dir().join("triage.md");
    assert_eq!(out["path"], path.to_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "# triage\n\nlook at the queue.\n"
    );

    let rows = env.json(&["workflow", "list"]);
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "triage")
        .unwrap();
    assert_eq!(row["source"], "user");
    assert_eq!(row["summary"], "look at the queue.");
    assert_eq!(rows.as_array().unwrap().len(), 6);

    // Adding it twice is a conflict, and it points at the command that works.
    let assert = env
        .cmd()
        .args([
            "workflow",
            "add",
            "triage",
            "--file",
            body.to_str().unwrap(),
        ])
        .args(["--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "conflict");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("q workflow edit triage"),
        "{parsed}"
    );
}

#[test]
fn workflow_add_reads_stdin_and_refuses_an_empty_body() {
    let env = Env::new();
    env.cmd()
        .args(["workflow", "add", "piped", "--file", "-"])
        .write_stdin("# piped\n\nfrom a pipe.\n")
        .assert()
        .success();
    let out = env.json(&["workflow", "show", "piped"]);
    assert_eq!(out["source"], "user");
    assert!(out["body"].as_str().unwrap().contains("from a pipe."));

    let blank = write_file(&env, "blank.md", "   \n\n");
    env.cmd()
        .args([
            "workflow",
            "add",
            "empty",
            "--file",
            blank.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("would be empty"));
    assert!(!env.workflow_dir().join("empty.md").exists());
}

/// `q workflow add` with no `--file` opens `$EDITOR` on a skeleton that shows
/// both halves of the SPEC §11 split.
#[test]
fn workflow_add_opens_an_editor_on_a_skeleton() {
    let env = Env::new();
    let seen = env.dir.path().join("seen.md");
    with_editor(
        &env,
        "# triage\n\nmine.\n\n## worker\n\nyours.\n",
        Some(&seen),
    )
    .args(["workflow", "add", "triage"])
    .assert()
    .success();

    let buffer = std::fs::read_to_string(&seen).unwrap();
    assert!(buffer.starts_with("# triage\n"), "{buffer}");
    assert!(buffer.contains("## worker"), "{buffer}");

    let out = env.json(&["workflow", "show", "triage"]);
    assert_eq!(out["has_worker_section"], true);
    assert!(out["body"].as_str().unwrap().contains("yours."));

    // An editor that failed writes nothing.
    env.cmd()
        .env("Q_FIXTURE_EDITOR_FAIL", "1")
        .args(["workflow", "add", "other"])
        .assert()
        .failure();
    assert!(!env.workflow_dir().join("other.md").exists());
}

#[test]
fn workflow_add_refuses_a_builtin_name_and_points_at_edit() {
    let env = Env::new();
    let assert = env
        .cmd()
        .args(["workflow", "add", "orchestrator", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "conflict");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("q workflow edit orchestrator"),
        "{parsed}"
    );
    assert!(!env.workflow_dir().join("orchestrator.md").exists());

    for bad in ["Not A Name", "double--dash", "../escape"] {
        env.cmd()
            .args(["workflow", "add", bad])
            .assert()
            .failure()
            .stderr(predicate::str::contains("workflow name"));
    }
}

/// SPEC §11: editing a built-in copies it into the config directory first, and
/// the copy shadows it from then on.
#[test]
fn workflow_edit_of_a_builtin_copies_it_and_rm_reveals_it_again() {
    let env = Env::new();
    let seen = env.dir.path().join("seen.md");
    let out = json_of(
        &with_editor(&env, "# solo\n\nMY OWN SOLO.\n", Some(&seen))
            .args(["workflow", "edit", "solo", "--json"])
            .assert()
            .success(),
    );
    assert_eq!(out["source"], "shadow");
    assert_eq!(out["action"], "copied and updated");

    // The editor opened on the built-in's own text, not an empty buffer.
    let buffer = std::fs::read_to_string(&seen).unwrap();
    assert!(buffer.starts_with("# solo\n"), "{buffer}");
    assert!(buffer.contains("One master, no workers"), "{buffer}");

    let shown = env.json(&["workflow", "show", "solo"]);
    assert_eq!(shown["source"], "shadow");
    assert!(shown["body"].as_str().unwrap().contains("MY OWN SOLO."));
    let rows = env.json(&["workflow", "list"]);
    assert_eq!(rows.as_array().unwrap().len(), 5, "still five names");
    let row = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "solo")
        .unwrap();
    assert_eq!(row["source"], "shadow");

    // Removing the copy brings the built-in back, untouched.
    let out = env.json(&["workflow", "rm", "solo", "-f"]);
    assert_eq!(out["reveals_builtin"], true);
    assert!(!env.workflow_dir().join("solo.md").exists());
    let shown = env.json(&["workflow", "show", "solo"]);
    assert_eq!(shown["source"], "builtin");
    assert!(
        shown["body"]
            .as_str()
            .unwrap()
            .contains("One master, no workers")
    );
}

#[test]
fn workflow_edit_replaces_a_user_file_from_a_file() {
    let env = Env::new();
    env.workflow("triage", "# triage\n\nold.\n");
    let next = write_file(&env, "next.md", "# triage\n\nnew.\n");
    let out = env.json(&[
        "workflow",
        "edit",
        "triage",
        "--file",
        next.to_str().unwrap(),
    ]);
    assert_eq!(out["action"], "updated");
    assert_eq!(out["source"], "user");
    assert!(
        env.json(&["workflow", "show", "triage"])["body"]
            .as_str()
            .unwrap()
            .contains("new.")
    );

    env.cmd()
        .args(["workflow", "edit", "nope", "--file", next.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown workflow `nope`"));
}

#[test]
fn workflow_rm_asks_first_and_refuses_a_builtin_that_is_not_shadowed() {
    let env = Env::new();
    env.workflow("triage", "# triage\n\nx.\n");

    // No terminal and no scripted answer: refused, and the file survives.
    env.cmd()
        .args(["workflow", "rm", "triage"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("aborted (use -f)"));
    assert!(env.workflow_dir().join("triage.md").exists());

    // The scripted `yes` is the half that actually deletes.
    env.cmd()
        .env("Q_FIXTURE_CONFIRM", "y")
        .args(["workflow", "rm", "triage"])
        .assert()
        .success()
        .stderr(predicate::str::contains("remove workflow triage?"));
    assert!(!env.workflow_dir().join("triage.md").exists());

    // A built-in with no file behind it is not a delete at all.
    let assert = env
        .cmd()
        .args(["workflow", "rm", "solo", "-f", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "invalid");
    assert!(
        parsed["error"].as_str().unwrap().contains("built-in"),
        "{parsed}"
    );

    let assert = env
        .cmd()
        .args(["workflow", "rm", "nope", "-f", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
}

/// SPEC §11: the master may change its own workflow mid-Quest, and it is an
/// event — the same write `q set <quest> workflow` makes.
#[test]
fn workflow_set_changes_the_quest_and_emits_an_event() {
    let env = Env::new();
    let quest = env.new_quest("foo");
    let id = quest["quest"]["id"].as_str().unwrap().to_string();

    let out = env.json(&["workflow", "set", "foo", "orchestrator"]);
    assert_eq!(out["key"], "workflow");
    assert_eq!(out["value"], "orchestrator");
    assert_eq!(out["quest"]["workflow"], "orchestrator");

    let payloads: Vec<String> = env
        .conn()
        .prepare("SELECT payload FROM event WHERE quest_id = ?1 AND kind = 'quest.updated'")
        .unwrap()
        .query_map([&id], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(payloads.len(), 1, "{payloads:?}");
    assert!(payloads[0].contains("orchestrator"), "{payloads:?}");

    // A blank name clears it, exactly as `q set` does.
    assert_eq!(
        env.json(&["workflow", "set", "foo", ""])["quest"]["workflow"],
        serde_json::Value::Null
    );

    // And an unknown one is refused before anything is written.
    let assert = env
        .cmd()
        .args(["workflow", "set", "foo", "nope", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
    assert_eq!(
        env.json(&["show", "foo"])["workflow"],
        serde_json::Value::Null
    );
}

/// The four places a workflow name is accepted, each refusing an unknown one
/// with the list — and each accepting a user file (SPEC §11).
#[test]
fn every_command_that_takes_a_workflow_checks_it_against_the_registry() {
    let env = Env::new();
    let work = env.work("repo");
    env.workflow("triage", "# triage\n\nmine.\n");

    for args in [
        vec!["new", "--name", "a", "--workflow", "nope", "-d"],
        vec!["tpl", "add", "t", "--workflow", "nope"],
    ] {
        let assert = env.cmd().args(&args).args(["--json"]).assert().code(1);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
        assert_eq!(parsed["code"], "not_found", "{args:?}: {parsed}");
        assert!(
            parsed["error"].as_str().unwrap().contains("triage"),
            "{args:?}: the list is offered: {parsed}"
        );
    }
    // The refused `q new` created nothing at all.
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
    assert_eq!(env.count("SELECT count(*) FROM template"), 0);

    // A user file is accepted everywhere a built-in is.
    let out = env.json(&[
        "new",
        "--name",
        "a",
        "--workflow",
        "triage",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);
    assert_eq!(out["quest"]["workflow"], "triage");
    env.json(&["tpl", "add", "t", "--workflow", "triage"]);
    env.json(&["set", "a", "workflow", "solo"]);

    let assert = env
        .cmd()
        .args(["spawn", "a", "go", "--label", "w", "--workflow", "nope"])
        .args(["--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
    assert_eq!(
        env.count("SELECT count(*) FROM session"),
        1,
        "no worker row"
    );

    let out = env.json(&["spawn", "a", "go", "--label", "w", "--workflow", "review"]);
    assert_eq!(out["session"]["workflow"], "review");
}

/// A template's workflow is checked when it is **set**, never on an unrelated
/// write — the rule `cwd` already follows — and again when the template is
/// **run**, which goes through `q new`.
#[test]
fn a_templates_workflow_is_checked_when_set_and_when_run() {
    let env = Env::new();
    let work = env.work("repo");
    env.workflow("triage", "# triage\n\nmine.\n");
    env.json(&[
        "tpl",
        "add",
        "weekly",
        "--workflow",
        "triage",
        "--cwd",
        work.to_str().unwrap(),
    ]);

    // The file goes away; an edit that never touches `workflow` still works.
    std::fs::remove_file(env.workflow_dir().join("triage.md")).unwrap();
    let out = env.json(&["tpl", "edit", "weekly", "--description", "the routine"]);
    assert_eq!(out["description"], "the routine");
    assert_eq!(out["workflow"], "triage");

    // Running it is where it has to exist.
    let assert = env
        .cmd()
        .args(["tpl", "run", "weekly", "-d", "--json"])
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stderr.trim()).unwrap();
    assert_eq!(parsed["code"], "not_found");
    assert!(
        parsed["error"]
            .as_str()
            .unwrap()
            .contains("unknown workflow `triage`"),
        "{parsed}"
    );
    assert_eq!(env.count("SELECT count(*) FROM quest"), 0);
    assert_eq!(
        env.json(&["tpl", "show", "weekly"])["run_count"],
        0,
        "a run that did not happen is not counted"
    );

    // Put it back and the routine runs.
    env.workflow("triage", "# triage\n\nmine.\n");
    let out = env.json(&["tpl", "run", "weekly", "-d"]);
    assert_eq!(out["quest"]["workflow"], "triage");
}

/// SPEC §9 section 3 end to end: the brief carries the workflow's **markdown**,
/// and a worker's carries only its `## worker` half.
#[test]
fn the_brief_renders_the_workflow_markdown_for_each_role() {
    let env = Env::new();
    let work = env.work("repo");
    env.json(&[
        "new",
        "--name",
        "foo",
        "--workflow",
        "orchestrator",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);

    let assert = env.cmd().args(["brief", "foo"]).assert().success();
    let master = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        master.contains("Workflow **`orchestrator`** (builtin)"),
        "{master}"
    );
    assert!(master.contains("Choosing the pipeline"), "{master}");
    assert!(
        master.contains("Do **only** the stage you were given"),
        "{master}"
    );
    // The workflow's own headings never pose as the brief's.
    assert!(!master.contains("\n## worker\n"), "{master}");
    assert!(master.contains("\n#### worker\n"), "{master}");

    let assert = env
        .cmd()
        .args(["brief", "foo", "--for", "worker"])
        .assert()
        .success();
    let worker = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        worker.contains("Do **only** the stage you were given"),
        "{worker}"
    );
    assert!(!worker.contains("Choosing the pipeline"), "{worker}");

    // A user file shadows the built-in here too.
    env.workflow("orchestrator", "# orchestrator\n\nMY OWN.\n");
    let assert = env.cmd().args(["brief", "foo"]).assert().success();
    let shadowed = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(shadowed.contains("MY OWN."), "{shadowed}");
    assert!(!shadowed.contains("Choosing the pipeline"), "{shadowed}");
    assert!(shadowed.contains("(user (shadows builtin))"), "{shadowed}");
}

/// A workflow file that vanished under a Quest is stated in the brief, never
/// silently dropped: a master cannot tell "no workflow" from "yours is gone".
#[test]
fn a_brief_says_when_the_quests_workflow_cannot_be_read() {
    let env = Env::new();
    let work = env.work("repo");
    env.workflow("triage", "# triage\n\nmine.\n");
    env.json(&[
        "new",
        "--name",
        "foo",
        "--workflow",
        "triage",
        "--dir",
        work.to_str().unwrap(),
        "-d",
    ]);
    std::fs::remove_file(env.workflow_dir().join("triage.md")).unwrap();

    let assert = env.cmd().args(["brief", "foo"]).assert().success();
    let text = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(text.contains("could not be read"), "{text}");
    assert!(text.contains("unknown workflow `triage`"), "{text}");
    assert!(
        text.contains("## 4. Beads"),
        "the brief is still whole:\n{text}"
    );
}

/// Workflows are files in *this* machine's config directory, so `--machine`
/// cannot mean another one — the shape `q tpl` already refuses.
#[test]
fn workflow_files_are_refused_for_a_remote_machine_but_set_travels() {
    let env = Env::new();
    env.with_remotes(&[("ws", "ws-host")]);
    for args in [
        vec!["workflow", "list"],
        vec!["workflow", "show", "solo"],
        vec!["workflow", "rm", "solo", "-f"],
    ] {
        env.cmd()
            .args(["--machine", "ws"])
            .args(&args)
            .assert()
            .failure()
            .stderr(predicate::str::contains("q workflow add <name> --file -"));
    }
}

#[test]
fn help_lists_the_workflow_command() {
    let assert = q().arg("--help").assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("workflow"), "{out}");

    let assert = q().args(["workflow", "--help"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    for sub in ["list", "show", "add", "edit", "rm", "set"] {
        assert!(out.contains(sub), "`{sub}` missing:\n{out}");
    }
}

/// A closed downstream reader (`q … | head`) must stay silent on stdout paths:
/// no "error: Broken pipe" on stderr and no Rust panic. Restoring the default
/// SIGPIPE disposition at the top of `main` (see `src/main.rs`) makes a write
/// to a gone reader terminate the process the way every Unix tool does, so a
/// large `workflow show` cut short by `head` unwinds nothing and prints no
/// error. `head` closes the pipe after one line, and the body is far past any
/// pipe buffer, so the closure lands mid-write.
#[test]
fn a_closed_downstream_pipe_prints_no_error_and_does_not_panic() {
    let env = Env::new();
    let big = "lorem ipsum dolor sit amet, consectetur adipiscing elit\n".repeat(30_000);
    env.workflow("big", &big);

    let q = assert_cmd::cargo::cargo_bin("q");
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("'{}' workflow show big | head -1", q.display()))
        .env("Q_DB", env.dir.path().join("q.db"))
        .env("Q_CONFIG", env.dir.path().join("config.toml"))
        .env("Q_FIXTURE", env.dir.path().join("tmux.json"))
        .env("Q_CLAUDE_SESSIONS_DIR", env.dir.path().join("registry"))
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("Q_QUEST")
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.to_lowercase().contains("broken pipe"),
        "a closed pipe surfaced as an error:\n{stderr}"
    );
    assert!(
        !stderr.contains("panicked"),
        "a closed pipe unwound the process:\n{stderr}"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("lorem ipsum"),
        "`head` never saw the first line:\n{stdout}"
    );
}

/// Guards the fix itself: the default SIGPIPE disposition must be restored in
/// `main`, since every stdout path — including any future direct `println!` —
/// relies on it rather than on a per-call BrokenPipe check.
#[test]
fn main_restores_the_default_sigpipe_disposition() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    assert!(
        src.contains("libc::signal(libc::SIGPIPE, libc::SIG_DFL)"),
        "main() no longer restores SIG_DFL for SIGPIPE"
    );
}

// ----------------------------------------------------------- brain integration

/// `<root>/sessions/<slug>/<slug>.md`, the session-note path convention.
fn brain_note(root: &std::path::Path, slug: &str) -> std::path::PathBuf {
    root.join("sessions").join(slug).join(format!("{slug}.md"))
}

/// `q new --brain` writes the session note with `tags: [session]` and the YAML
/// block, and records the slug as the Quest's `brain_session`.
#[test]
fn new_brain_writes_the_session_note_and_records_the_session() {
    let env = Env::new();
    let work = env.work("bq");
    let root = env.dir.path().join("brain");
    let assert = env
        .cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["new", "--name", "bq", "--dir", work.to_str().unwrap()])
        .args(["--brain", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert_eq!(out["quest"]["brain_session"], "bq");
    let id = out["quest"]["id"].as_str().unwrap();

    let body = std::fs::read_to_string(brain_note(&root, "bq")).unwrap();
    assert!(body.starts_with("---\ntags: [session]\n"), "{body}");
    assert!(body.contains(&format!("quest: {id}\n")), "{body}");
    assert!(body.contains("machine: "), "{body}");
    assert!(
        body.contains(&format!("cwd: {}\n", work.to_str().unwrap())),
        "{body}"
    );
    assert!(body.contains("created: "), "{body}");
    assert!(body.contains("# bq\n"), "{body}");

    // The event is recorded too.
    let kinds = event_kinds(&env, id);
    assert!(kinds.iter().any(|k| k == "brain.session"), "{kinds:?}");
}

/// Neither the default nor `--no-brain` writes a note or sets a session, even
/// with a brain root configured.
#[test]
fn new_without_brain_writes_no_note() {
    let env = Env::new();
    let root = env.dir.path().join("brain");

    let plain = env.new_quest("plain");
    assert!(plain["quest"]["brain_session"].is_null());
    assert!(!brain_note(&root, "plain").exists());

    let work = env.work("nb");
    let assert = env
        .cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["new", "--name", "nb", "--dir", work.to_str().unwrap()])
        .args(["--no-brain", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    assert!(out["quest"]["brain_session"].is_null());
    assert!(!brain_note(&root, "nb").exists());
}

/// A template stored with `create_brain` (`q tpl add --brain`) creates the
/// brain session when instantiated through `q tpl run` (bd-8lz.7.9): the note
/// is written under the brain root and recorded as the Quest's session. This
/// is the one construction the TUI Templates tab shares too (`instantiate_with`).
#[test]
fn tpl_run_creates_the_brain_session_when_the_template_asks() {
    let env = Env::new();
    let work = env.work("repo");
    let root = env.dir.path().join("brain");
    env.cmd()
        .args(["tpl", "add", "brainy", "--cwd", work.to_str().unwrap()])
        .arg("--brain")
        .assert()
        .success();

    let assert = env
        .cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["tpl", "run", "brainy", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    let slug = out["quest"]["slug"].as_str().unwrap().to_string();
    assert_eq!(out["quest"]["brain_session"], slug, "{out}");
    assert!(
        brain_note(&root, &slug).exists(),
        "no session note for {slug}"
    );
}

/// A template without `create_brain` writes no note and leaves the Quest
/// without a brain session, even with a brain root configured.
#[test]
fn tpl_run_without_create_brain_writes_no_note() {
    let env = Env::new();
    let work = env.work("repo");
    let root = env.dir.path().join("brain");
    env.cmd()
        .args(["tpl", "add", "plainy", "--cwd", work.to_str().unwrap()])
        .assert()
        .success();

    let assert = env
        .cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["tpl", "run", "plainy", "-d", "--json"])
        .assert()
        .success();
    let out = json_of(&assert);
    let slug = out["quest"]["slug"].as_str().unwrap().to_string();
    assert!(out["quest"]["brain_session"].is_null(), "{out}");
    assert!(
        !brain_note(&root, &slug).exists(),
        "unexpected note for {slug}"
    );
}

/// `q link add` appends the reference into the session note's YAML block when
/// `[brain] sync_links` is on (the default) and the Quest has a brain session.
#[test]
fn link_add_syncs_into_the_brain_note_when_enabled() {
    let env = Env::new();
    let work = env.work("lq");
    let root = env.dir.path().join("brain");
    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["new", "--name", "lq", "--dir", work.to_str().unwrap()])
        .args(["--brain", "-d", "--json"])
        .assert()
        .success();

    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args([
            "link",
            "add",
            "https://example.com/x",
            "--quest",
            "lq",
            "--json",
        ])
        .assert()
        .success();

    let body = std::fs::read_to_string(brain_note(&root, "lq")).unwrap();
    let fm_end = body.find("\n---\n\n").unwrap();
    assert!(
        body[..fm_end].contains("url: https://example.com/x"),
        "link not synced into the YAML block:\n{body}"
    );
}

/// `[brain] sync_links = false` leaves the note untouched on `q link add`.
#[test]
fn link_add_does_not_sync_when_sync_links_is_off() {
    let env = Env::new();
    let work = env.work("lq");
    let root = env.dir.path().join("brain");
    env.cmd()
        .args(["config", "set", "brain.sync_links", "false"])
        .assert()
        .success();
    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["new", "--name", "lq", "--dir", work.to_str().unwrap()])
        .args(["--brain", "-d", "--json"])
        .assert()
        .success();

    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args([
            "link",
            "add",
            "https://example.com/x",
            "--quest",
            "lq",
            "--json",
        ])
        .assert()
        .success();

    let body = std::fs::read_to_string(brain_note(&root, "lq")).unwrap();
    assert!(!body.contains("example.com"), "synced despite off:\n{body}");
}

/// With no brain session on the Quest, `q link add` never touches the brain —
/// no note is created.
#[test]
fn link_add_does_not_sync_without_a_brain_session() {
    let env = Env::new();
    let root = env.dir.path().join("brain");
    env.new_quest("plain");
    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args([
            "link",
            "add",
            "https://example.com/x",
            "--quest",
            "plain",
            "--json",
        ])
        .assert()
        .success();
    assert!(!root.join("sessions").exists());
}

/// `q link add --kind brain <slug>` enables an existing session on a Quest that
/// had none: the slug becomes its `brain_session`.
#[test]
fn link_add_kind_brain_enables_an_existing_session() {
    let env = Env::new();
    env.new_quest("plain");
    let out = env.json(&[
        "link",
        "add",
        "my-session",
        "--kind",
        "brain",
        "--quest",
        "plain",
    ]);
    assert_eq!(out["link"]["kind"], "brain");

    let bs: Option<String> = env
        .conn()
        .query_row(
            "SELECT brain_session FROM quest WHERE slug = ?1",
            ["plain"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(bs.as_deref(), Some("my-session"));
}

/// `q close --summarize` hands the brief to the stubbed `claude`, then records
/// the path it returns as `summarized_to:` on the session note.
#[test]
fn close_summarize_invokes_claude_and_records_summarized_to() {
    let env = Env::new();
    let work = env.work("cq");
    let root = env.dir.path().join("brain");
    // The stub `claude` "wrote" this note and prints its path.
    let claude_out = env.dir.path().join("claude.out");
    std::fs::write(&claude_out, "knowledge/cq-lessons.md\n").unwrap();
    let claude_log = env.dir.path().join("claude.log");

    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["new", "--name", "cq", "--dir", work.to_str().unwrap()])
        .args(["--brain", "-d", "--json"])
        .assert()
        .success();

    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .env("Q_FIXTURE_CLAUDE", &claude_out)
        .env("Q_FIXTURE_CLAUDE_LOG", &claude_log)
        .env("Q_FIXTURE_CONFIRM", "y")
        .args(["close", "cq", "--force", "--summarize"])
        .assert()
        .success();

    // The brief reached claude.
    let logged = std::fs::read_to_string(&claude_log).unwrap();
    assert!(
        logged.contains("# Quest"),
        "brief not sent to claude:\n{logged}"
    );
    assert!(logged.contains("`cq`"), "{logged}");

    // The note records where the summary landed.
    let body = std::fs::read_to_string(brain_note(&root, "cq")).unwrap();
    assert!(
        body.contains("summarized_to: knowledge/cq-lessons.md"),
        "{body}"
    );
}

/// `q close --summarize` degrades gracefully when `claude` is unavailable: the
/// Quest still closes and the note carries no `summarized_to`.
#[test]
fn close_summarize_skips_gracefully_when_claude_is_unavailable() {
    let env = Env::new();
    let work = env.work("cq");
    let root = env.dir.path().join("brain");
    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .args(["new", "--name", "cq", "--dir", work.to_str().unwrap()])
        .args(["--brain", "-d", "--json"])
        .assert()
        .success();

    // No Q_FIXTURE_CLAUDE → the stub reports claude unavailable.
    env.cmd()
        .env("Q_BRAIN_ROOT", &root)
        .env("Q_FIXTURE_CONFIRM", "y")
        .args(["close", "cq", "--force", "--summarize"])
        .assert()
        .success();

    let closed: String = env
        .conn()
        .query_row("SELECT state FROM quest WHERE slug = ?1", ["cq"], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(closed, "finished");
    let body = std::fs::read_to_string(brain_note(&root, "cq")).unwrap();
    assert!(!body.contains("summarized_to"), "{body}");
}

#[test]
fn completions_zsh_emits_script() {
    // No DB or config is set up here, yet the command must still succeed: it is
    // generated straight off the clap command tree (SPEC §21).
    let assert = q().args(["completions", "zsh"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(out.contains("#compdef q"), "zsh header missing:\n{out}");
    assert!(
        out.contains("_q"),
        "zsh completion function missing:\n{out}"
    );
}

#[test]
fn completions_bash_emits_script() {
    let assert = q().args(["completions", "bash"]).assert().success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        out.contains("_q()"),
        "bash completion function missing:\n{out}"
    );
    assert!(
        out.contains("complete "),
        "bash complete registration missing:\n{out}"
    );
}

#[test]
fn completions_reject_unknown_shell() {
    q().args(["completions", "tcsh"]).assert().failure();
}

// ---------------------------------------- M3: cwd follows the main shell (bd-v1d.5)

/// Point the main session's pane at `path`, running `command` (a shell after a
/// `cd`, or Claude's version string while it is up), the way tmux reports it.
fn set_main_pane(env: &Env, slug: &str, command: &str, path: &std::path::Path) {
    let mut fixture = env.fixture();
    let session = format!("q-{slug}");
    let pane = fixture["panes"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|p| p["session_name"].as_str() == Some(session.as_str()))
        .expect("main pane in the fixture");
    pane["current_command"] = serde_json::json!(command);
    pane["current_path"] = serde_json::json!(path.to_str().unwrap());
    env.write_fixture(fixture);
}

/// Run a read that sweeps (so cwd-follow runs), then read the Quest cwd back.
fn swept_cwd(env: &Env, slug: &str) -> String {
    env.cmd().args(["show", slug]).assert().success();
    env.conn()
        .query_row("SELECT cwd FROM quest WHERE slug = ?1", [slug], |r| {
            r.get(0)
        })
        .unwrap()
}

fn cwd_changed_count(env: &Env) -> i64 {
    env.count("SELECT count(*) FROM event WHERE kind = 'quest.cwd_changed'")
}

#[test]
fn cwd_follows_the_main_shell_across_a_shell_edge() {
    let env = Env::new();
    env.new_quest("foo");
    let a = env.work("foo");
    let x = env.work("x");

    // The master exits Claude to a shell in `a`: the first sweep only seeds the
    // baseline, it does not rewrite the (already `a`) cwd.
    set_main_pane(&env, "foo", "zsh", &a);
    assert_eq!(swept_cwd(&env, "foo"), a.to_str().unwrap());
    assert_eq!(cwd_changed_count(&env), 0);

    // A `cd` to `x` in that shell is a real edge across two sweeps.
    set_main_pane(&env, "foo", "zsh", &x);
    assert_eq!(swept_cwd(&env, "foo"), x.to_str().unwrap());
    assert_eq!(cwd_changed_count(&env), 1);

    // Idempotent: sitting in `x` is not another edge.
    assert_eq!(swept_cwd(&env, "foo"), x.to_str().unwrap());
    assert_eq!(cwd_changed_count(&env), 1);
}

#[test]
fn cwd_does_not_follow_while_claude_is_up() {
    let env = Env::new();
    env.new_quest("foo");
    let a = env.work("foo");
    let x = env.work("x");

    // Claude is up in the main pane — a non-shell command, never followed even
    // as its frozen path drifts a -> x.
    set_main_pane(&env, "foo", "2.1.0", &a);
    assert_eq!(swept_cwd(&env, "foo"), a.to_str().unwrap());
    set_main_pane(&env, "foo", "2.1.0", &x);
    assert_eq!(swept_cwd(&env, "foo"), a.to_str().unwrap());
    assert_eq!(cwd_changed_count(&env), 0);
}

#[test]
fn q_cd_sticks_across_a_later_sweep() {
    let env = Env::new();
    env.new_quest("foo");
    let a = env.work("foo");
    let y = env.work("y");

    // Master at a shell in `a`; baseline seeded.
    set_main_pane(&env, "foo", "zsh", &a);
    assert_eq!(swept_cwd(&env, "foo"), a.to_str().unwrap());

    // `q cd` is the alias of `q set <quest> cwd`; the explicit move sticks even
    // though the shell is still sitting in `a`.
    env.cmd()
        .args(["cd", "foo", y.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(swept_cwd(&env, "foo"), y.to_str().unwrap());
}

#[test]
fn set_cwd_is_not_reverted_when_claude_exits() {
    let env = Env::new();
    env.new_quest("foo");
    let a = env.work("foo");
    let x = env.work("x");
    let y = env.work("y");

    // Master exits to a shell in `a`: baseline seeded to `a`.
    set_main_pane(&env, "foo", "zsh", &a);
    assert_eq!(swept_cwd(&env, "foo"), a.to_str().unwrap());

    // The shell `cd`s to `x` and Claude is launched there (a non-shell pane):
    // cwd-follow does not touch a non-shell pane, so `last_pane_path` stays `a`.
    set_main_pane(&env, "foo", "2.1.0", &x);

    // The master moves the Quest explicitly to `y` while Claude runs. `q set`
    // reseeds `last_pane_path` to the pane's current path (`x`), consuming the
    // otherwise-stale `a` baseline.
    env.cmd()
        .args(["set", "foo", "cwd", y.to_str().unwrap()])
        .assert()
        .success();
    assert_eq!(swept_cwd(&env, "foo"), y.to_str().unwrap());

    // Claude exits to a shell still sitting in `x`. Without the reseed the sweep
    // would read an `a` -> `x` edge and revert the explicit `y`; with it there
    // is no edge and the cwd stays `y`.
    set_main_pane(&env, "foo", "zsh", &x);
    assert_eq!(swept_cwd(&env, "foo"), y.to_str().unwrap());
    assert_eq!(cwd_changed_count(&env), 0);
}

#[test]
fn spawn_after_a_cd_opens_the_worker_in_the_new_cwd() {
    let env = Env::new();
    env.new_quest("foo");
    let a = env.work("foo");
    let x = env.work("x");

    // Master at a shell in `a`; baseline seeded by this sweep.
    set_main_pane(&env, "foo", "zsh", &a);
    swept_cwd(&env, "foo");

    // `cd x` in the shell — no `q show`/TUI/watch in between.
    set_main_pane(&env, "foo", "zsh", &x);

    // `q spawn` sweeps first, so it picks up the moved cwd and opens the worker
    // in `x`.
    let out = env.json(&["spawn", "foo", "--shell"]);
    let worker = out["tmux_session"].as_str().unwrap().to_string();
    let pane = pane_of(&env.fixture(), &worker);
    assert_eq!(pane["cwd"], x.to_str().unwrap());
    assert_eq!(swept_cwd(&env, "foo"), x.to_str().unwrap());
}

#[test]
fn cwd_follow_can_be_turned_off() {
    let env = Env::new();
    env.cmd()
        .args(["config", "set", "quest.follow_main_cwd", "false"])
        .assert()
        .success();
    env.new_quest("foo");
    let a = env.work("foo");
    let x = env.work("x");

    set_main_pane(&env, "foo", "zsh", &a);
    swept_cwd(&env, "foo");
    set_main_pane(&env, "foo", "zsh", &x);
    assert_eq!(swept_cwd(&env, "foo"), a.to_str().unwrap());
    assert_eq!(cwd_changed_count(&env), 0);
}
