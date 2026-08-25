use assert_cmd::Command;
use predicates::prelude::*;

fn q() -> Command {
    let mut cmd = Command::cargo_bin("q").unwrap();
    // Never touch the real user state.
    cmd.env("Q_DB", ":memory:").env("Q_CONFIG", "/dev/null");
    cmd
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
    q().arg("--quiet").assert().success().stdout("");
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
}

#[test]
fn unknown_command_is_a_usage_error() {
    q().arg("definitely-not-a-command").assert().code(2);
}
