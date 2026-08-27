//! Remote machines (SPEC §15). Every `[[remotes]]` entry is asked
//! `ssh <alias> q list --json --no-remote`, in parallel, with a deadline; the
//! last good answer is cached so a machine that is down still shows its
//! Quests, marked stale, instead of dropping out of the listing.
//!
//! ssh goes through the [`Ssh`] trait exactly as tmux goes through
//! [`crate::tmux::Tmux`]: with `$Q_FIXTURE` set the fixture backend answers and
//! no test can reach a real host.
//!
//! This module is the plumbing only. Merging these rows into `q list` and the
//! TUI (bd-8lz.5.2) and proxying commands over ssh (bd-8lz.5.3) build on top.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::commands::QuestView;
use crate::config::Remote;
use crate::db::Db;
use crate::error::QError;
use crate::model::now;
use crate::output;

/// SPEC §15: one fan-out round waits at most this long per remote. Not a config
/// key — SPEC §20 names none, and a listing that can block longer than this is
/// not a listing.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// What a remote is asked for. `--no-remote` is the recursion guard: without it
/// the remote would fan out to *its* remotes, us included.
pub const LIST_ARGV: [&str; 4] = ["q", "list", "--json", "--no-remote"];

/// SPEC §15's marker for a machine that did not answer.
pub const UNREACHABLE: &str = "⚠ unreachable";
/// The same, for a machine that answered with something this `q` cannot read.
pub const INCOMPATIBLE: &str = "⚠ incompatible";

// --------------------------------------------------------------------- ssh

/// One `ssh` invocation's ending. A remote being down is an ordinary outcome
/// here, not an error: the fan-out degrades, it never fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshOutcome {
    Done {
        code: Option<i32>,
        stdout: String,
        stderr: String,
    },
    /// The deadline passed and the child was killed.
    TimedOut,
    /// ssh could not be started at all.
    Failed(String),
}

/// `Send + Sync` because the fan-out shares one client across threads.
pub trait Ssh: Send + Sync {
    /// `ssh <alias> <argv…>`, given up on after `timeout`.
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome;
}

/// `FixtureSsh` whenever `$Q_FIXTURE` is set, else the real thing — the same
/// gate `tmux` and `bd` use, so a test can never shell out to a real host by
/// forgetting a second variable.
pub fn ssh() -> Box<dyn Ssh> {
    match std::env::var_os("Q_FIXTURE") {
        Some(p) if !p.is_empty() => Box::new(FixtureSsh),
        _ => Box::new(RealSsh),
    }
}

/// The argv handed to `ssh`, built once and shared with the fixture log so a
/// test asserts on the command line `q` would really have run.
///
/// `BatchMode=yes` because nobody is at the keyboard: a host wanting a password
/// or a passphrase must fail fast rather than hold the listing until the
/// deadline. `ConnectTimeout` is ssh's own budget for the TCP handshake — the
/// deadline below covers the whole call, this only makes the common failure
/// (host down) return sooner.
fn ssh_argv(alias: &str, argv: &[&str], timeout: Duration) -> Vec<String> {
    let mut out = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", timeout.as_secs().max(1)),
        alias.to_string(),
    ];
    out.extend(argv.iter().map(|a| (*a).to_string()));
    out
}

pub struct RealSsh;

/// How often the deadline is checked while the child runs.
const POLL: Duration = Duration::from_millis(20);

impl Ssh for RealSsh {
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome {
        let spawned = Command::new("ssh")
            .args(ssh_argv(alias, argv, timeout))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return SshOutcome::Failed("ssh not found on PATH".to_string());
            }
            Err(e) => return SshOutcome::Failed(format!("cannot run ssh: {e}")),
        };

        // Drained on their own threads: a child that fills a pipe would block
        // forever, and this call has a deadline to keep.
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let (ending, stdout, stderr) = std::thread::scope(|scope| {
            let out = scope.spawn(move || drain(out_pipe));
            let err = scope.spawn(move || drain(err_pipe));
            let deadline = Instant::now() + timeout;
            let ending = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break Ending::Exited(status.code()),
                    Ok(None) if Instant::now() >= deadline => {
                        // Killing the child closes both pipes, so the drains end.
                        let _ = child.kill();
                        let _ = child.wait();
                        break Ending::TimedOut;
                    }
                    Ok(None) => std::thread::sleep(POLL),
                    Err(e) => break Ending::Broken(format!("cannot wait for ssh: {e}")),
                }
            };
            (
                ending,
                out.join().unwrap_or_default(),
                err.join().unwrap_or_default(),
            )
        });
        match ending {
            Ending::Exited(code) => SshOutcome::Done {
                code,
                stdout,
                stderr,
            },
            Ending::TimedOut => SshOutcome::TimedOut,
            Ending::Broken(e) => SshOutcome::Failed(e),
        }
    }
}

/// How the child run ended, before its output is folded back in.
enum Ending {
    Exited(Option<i32>),
    TimedOut,
    Broken(String),
}

fn drain(pipe: Option<impl Read>) -> String {
    let Some(mut pipe) = pipe else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

// ----------------------------------------------------------------- fixture

/// A scripted `ssh`, driven by two files so a test needs no host:
///
/// | variable | meaning |
/// |---|---|
/// | `Q_FIXTURE_SSH` | JSON: alias → the canned answer below |
/// | `Q_FIXTURE_SSH_LOG` | appended one line per call, `alias` then the argv, tab separated |
///
/// An alias the script does not name fails like an unknown host, so a fan-out
/// that reaches further than the test meant shows up as unreachable rather
/// than as a silent success.
pub struct FixtureSsh;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SshScript {
    #[serde(default)]
    pub hosts: BTreeMap<String, SshHost>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SshHost {
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    #[serde(default)]
    pub exit: i32,
    /// Answer as if the deadline had passed, without waiting for it.
    #[serde(default)]
    pub timeout: bool,
    /// ssh itself could not be started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail: Option<String>,
    /// Wait this long before answering — how a test makes a fan-out that is
    /// not parallel take N times as long as one that is.
    #[serde(default)]
    pub delay_ms: u64,
}

impl Ssh for FixtureSsh {
    fn run(&self, alias: &str, argv: &[&str], _timeout: Duration) -> SshOutcome {
        log(alias, argv);
        let mut script = script();
        let Some(host) = script.hosts.remove(alias) else {
            return SshOutcome::Failed(format!("no fixture host `{alias}`"));
        };
        if host.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(host.delay_ms));
        }
        if let Some(msg) = host.fail {
            return SshOutcome::Failed(msg);
        }
        if host.timeout {
            return SshOutcome::TimedOut;
        }
        SshOutcome::Done {
            code: Some(host.exit),
            stdout: host.stdout,
            stderr: host.stderr,
        }
    }
}

/// A missing or unreadable script is an empty one — every alias then fails.
fn script() -> SshScript {
    std::env::var_os("Q_FIXTURE_SSH")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Appended, never rewritten: the fan-out logs from several threads at once,
/// and one `write` per line under `O_APPEND` keeps them from interleaving.
fn log(alias: &str, argv: &[&str]) {
    let Some(path) = std::env::var_os("Q_FIXTURE_SSH_LOG") else {
        return;
    };
    use std::io::Write;
    let line = format!("{alias}\t{}\n", argv.join("\t"));
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

// ------------------------------------------------------------------ results

/// How a remote answered this round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "reason")]
pub enum RemoteStatus {
    Ok,
    /// No answer: down, unroutable, no `q` there, or slower than [`TIMEOUT`].
    Unreachable(String),
    /// An answer this `q` cannot read — a `q` too old or too new at the far end.
    Incompatible(String),
}

impl RemoteStatus {
    pub fn is_ok(&self) -> bool {
        matches!(self, RemoteStatus::Ok)
    }

    /// The listing marker (SPEC §15), or `None` when all is well.
    pub fn marker(&self) -> Option<&'static str> {
        match self {
            RemoteStatus::Ok => None,
            RemoteStatus::Unreachable(_) => Some(UNREACHABLE),
            RemoteStatus::Incompatible(_) => Some(INCOMPATIBLE),
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            RemoteStatus::Ok => None,
            RemoteStatus::Unreachable(r) | RemoteStatus::Incompatible(r) => Some(r),
        }
    }
}

/// One remote's Quests and how they were come by. Never an error: a machine
/// that is down contributes a row saying so, not a failed command.
#[derive(Debug, Serialize)]
pub struct RemoteResult {
    /// `remotes[].name` — the value of a Quest's `machine` column over there.
    pub name: String,
    pub ssh: String,
    pub status: RemoteStatus,
    /// This round's answer, or the last cached one when the round failed.
    pub quests: Vec<QuestView>,
    /// True when `quests` came out of the cache rather than off the wire.
    pub stale: bool,
    /// When `quests` were fetched. `None` when there is nothing to show at all.
    pub fetched_at: Option<i64>,
}

impl RemoteResult {
    /// `ws ⚠ unreachable (host is down)` — one line for a human.
    pub fn note(&self) -> Option<String> {
        let marker = self.status.marker()?;
        let mut line = format!("{} {marker}", self.name);
        if let Some(reason) = self.status.reason().filter(|r| !r.is_empty()) {
            line.push_str(&format!(" ({reason})"));
        }
        if self.stale {
            line.push_str(&format!(", showing {} cached quest(s)", self.quests.len()));
        }
        Some(line)
    }
}

// ----------------------------------------------------------------- fan-out

/// Every remote this invocation should ask, in config order. Empty means no
/// ssh at all: `--no-remote`, no `[[remotes]]`, or a `--machine` that is not
/// one of them (the local machine included).
pub fn targets(ctx: &Ctx) -> Vec<&Remote> {
    if !ctx.remote_enabled() {
        return Vec::new();
    }
    match ctx.machine_filter() {
        // A listing pinned to one machine has no use for the others.
        Some(machine) => ctx
            .config
            .remotes
            .iter()
            .filter(|r| r.name == machine)
            .collect(),
        None => ctx.config.remotes.iter().collect(),
    }
}

/// Ask every remote at once and fold in the cache. The whole round takes about
/// as long as the slowest remote, and at most [`TIMEOUT`].
pub fn fetch_all(ctx: &Ctx) -> Vec<RemoteResult> {
    let targets = targets(ctx);
    if targets.is_empty() {
        return Vec::new();
    }
    let answers = fan_out(ctx.ssh(), &targets, TIMEOUT);
    resolve(ctx.db().ok(), &targets, answers, now())
}

/// Buffers one line per unhappy remote onto the `Ctx` (see [`Ctx::warn`]), so
/// the caller decides where it goes.
pub fn warn_unreachable(ctx: &Ctx, results: &[RemoteResult]) {
    for note in results.iter().filter_map(RemoteResult::note) {
        ctx.warn(format!("warning: {note}"));
    }
}

/// One thread per remote, all joined before this returns; the answers come back
/// in `targets` order. A thread that panics counts as unreachable — a broken
/// remote must not take the command down with it.
fn fan_out(
    ssh: &dyn Ssh,
    targets: &[&Remote],
    timeout: Duration,
) -> Vec<Result<Vec<QuestView>, RemoteStatus>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|remote| {
                let alias = remote.ssh.as_str();
                scope.spawn(move || interpret(ssh.run(alias, &LIST_ARGV, timeout), timeout))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(RemoteStatus::Unreachable(
                        "the ssh call panicked".to_string(),
                    ))
                })
            })
            .collect()
    })
}

/// What the far end said, turned into either Quests or a reason it is not
/// usable. Nothing here can panic: garbage, a non-zero exit and JSON from
/// another version of `q` all land on a status.
fn interpret(outcome: SshOutcome, timeout: Duration) -> Result<Vec<QuestView>, RemoteStatus> {
    match outcome {
        SshOutcome::TimedOut => Err(RemoteStatus::Unreachable(format!(
            "no answer within {}s",
            timeout.as_secs()
        ))),
        SshOutcome::Failed(e) => Err(RemoteStatus::Unreachable(e)),
        SshOutcome::Done {
            code,
            stdout,
            stderr,
        } => {
            if code != Some(0) {
                return Err(RemoteStatus::Unreachable(exit_reason(code, &stderr)));
            }
            parse(&stdout).map_err(RemoteStatus::Incompatible)
        }
    }
}

fn exit_reason(code: Option<i32>, stderr: &str) -> String {
    let said = output::first_line(stderr, 120);
    // ssh hands back the remote command's exit status, so `c` is `q`'s code
    // when the connection got that far and ssh's own (255) when it did not.
    match (code, said.is_empty()) {
        (Some(c), false) => format!("ssh exited {c}: {said}"),
        (Some(c), true) => format!("ssh exited {c}"),
        (None, false) => format!("ssh was killed: {said}"),
        (None, true) => "ssh was killed by a signal".to_string(),
    }
}

/// `q list --json` is an array of [`QuestView`]. Unknown fields are ignored, so
/// a newer `q` at the far end still parses; a missing required one does not.
pub fn parse(stdout: &str) -> Result<Vec<QuestView>, String> {
    let text = stdout.trim();
    if text.is_empty() {
        return Err("empty response".to_string());
    }
    serde_json::from_str(text).map_err(|e| format!("cannot read `q list --json`: {e}"))
}

/// Pairs each answer with its remote, writing the good ones to the cache and
/// falling back to it for the rest.
fn resolve(
    db: Option<&Db>,
    targets: &[&Remote],
    answers: Vec<Result<Vec<QuestView>, RemoteStatus>>,
    ts: i64,
) -> Vec<RemoteResult> {
    targets
        .iter()
        .zip(answers)
        .map(|(remote, answer)| match answer {
            Ok(quests) => {
                store(db, &remote.name, &quests, ts);
                RemoteResult {
                    name: remote.name.clone(),
                    ssh: remote.ssh.clone(),
                    status: RemoteStatus::Ok,
                    quests,
                    stale: false,
                    fetched_at: Some(ts),
                }
            }
            Err(status) => {
                let cached = load(db, &remote.name);
                RemoteResult {
                    name: remote.name.clone(),
                    ssh: remote.ssh.clone(),
                    status,
                    stale: cached.is_some(),
                    fetched_at: cached.as_ref().map(|(_, at)| *at),
                    quests: cached.map(|(q, _)| q).unwrap_or_default(),
                }
            }
        })
        .collect()
}

/// Best effort in both directions: a cache that cannot be written or read costs
/// staleness, never the listing.
fn store(db: Option<&Db>, name: &str, quests: &[QuestView], ts: i64) {
    let Some(db) = db else { return };
    if let Ok(payload) = serde_json::to_string(quests) {
        let _ = db.put_remote_cache(name, &payload, ts);
    }
}

fn load(db: Option<&Db>, name: &str) -> Option<(Vec<QuestView>, i64)> {
    let cached = db?.get_remote_cache(name).ok().flatten()?;
    let quests = parse(&cached.payload).ok()?;
    Some((quests, cached.fetched_at))
}

/// The configured remote called `name`, for the commands that dispatch to one
/// (bd-8lz.5.3).
pub fn find<'a>(remotes: &'a [Remote], name: &str) -> anyhow::Result<&'a Remote> {
    remotes.iter().find(|r| r.name == name).ok_or_else(|| {
        QError::NotFound(format!("remote `{name}` (see `[[remotes]]` in the config)")).into()
    })
}

#[cfg(test)]
pub(crate) mod stub {
    use super::{Ssh, SshOutcome};
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::time::Duration;

    /// What every `Ctx::for_tests` gets unless it asks for something else: an
    /// ssh that refuses, so a test reaching a remote by accident says so
    /// instead of shelling out.
    pub(crate) struct NoSsh;

    impl Ssh for NoSsh {
        fn run(&self, _: &str, _: &[&str], _: Duration) -> SshOutcome {
            SshOutcome::Failed("this test has no ssh (pass one with `Ctx::with_ssh`)".to_string())
        }
    }

    /// A scriptable ssh that records what it was asked, and how many calls
    /// overlapped — which is how a test tells a parallel fan-out from a serial
    /// one without timing it.
    pub(crate) struct StubSsh {
        answers: BTreeMap<String, SshOutcome>,
        delay: Duration,
        state: Mutex<StubState>,
    }

    #[derive(Default)]
    struct StubState {
        calls: Vec<(String, Vec<String>)>,
        running: usize,
        peak: usize,
    }

    impl StubSsh {
        pub(crate) fn new(answers: &[(&str, SshOutcome)]) -> StubSsh {
            StubSsh {
                answers: answers
                    .iter()
                    .map(|(a, o)| ((*a).to_string(), o.clone()))
                    .collect(),
                delay: Duration::ZERO,
                state: Mutex::new(StubState::default()),
            }
        }

        /// Every call sleeps this long, so overlap is observable.
        pub(crate) fn with_delay(mut self, delay: Duration) -> StubSsh {
            self.delay = delay;
            self
        }

        pub(crate) fn calls(&self) -> Vec<(String, Vec<String>)> {
            self.state.lock().unwrap().calls.clone()
        }

        /// The highest number of calls that were in flight at once.
        pub(crate) fn peak(&self) -> usize {
            self.state.lock().unwrap().peak
        }
    }

    impl Ssh for StubSsh {
        fn run(&self, alias: &str, argv: &[&str], _: Duration) -> SshOutcome {
            {
                let mut state = self.state.lock().unwrap();
                state.calls.push((
                    alias.to_string(),
                    argv.iter().map(|a| a.to_string()).collect(),
                ));
                state.running += 1;
                state.peak = state.peak.max(state.running);
            }
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.state.lock().unwrap().running -= 1;
            self.answers
                .get(alias)
                .cloned()
                .unwrap_or_else(|| SshOutcome::Failed(format!("no stub host `{alias}`")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::stub::StubSsh;
    use super::*;
    use crate::model::Quest;

    fn remote(name: &str) -> Remote {
        Remote {
            name: name.to_string(),
            ssh: format!("{name}-host"),
        }
    }

    /// One Quest as a remote's `q list --json` would send it.
    fn payload(slug: &str) -> String {
        let view = QuestView::new(Quest::new(slug, "/tmp", "ws"), &[]);
        serde_json::to_string(&[view]).unwrap()
    }

    fn ok(stdout: String) -> SshOutcome {
        SshOutcome::Done {
            code: Some(0),
            stdout,
            stderr: String::new(),
        }
    }

    #[test]
    fn the_argv_is_the_spec_command_and_never_prompts() {
        let argv = ssh_argv("ws", &LIST_ARGV, TIMEOUT);
        assert_eq!(
            argv,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "ws",
                "q",
                "list",
                "--json",
                "--no-remote"
            ]
        );
    }

    #[test]
    fn every_remote_is_asked_at_once() {
        let remotes = [remote("a"), remote("b"), remote("c")];
        let targets: Vec<&Remote> = remotes.iter().collect();
        let ssh = StubSsh::new(&[
            ("a-host", ok(payload("one"))),
            ("b-host", ok(payload("two"))),
            ("c-host", ok(payload("three"))),
        ])
        .with_delay(Duration::from_millis(50));

        let started = Instant::now();
        let answers = fan_out(&ssh, &targets, TIMEOUT);
        let elapsed = started.elapsed();

        assert_eq!(answers.len(), 3);
        assert!(answers.iter().all(|a| a.is_ok()));
        assert_eq!(ssh.peak(), 3, "the calls did not overlap");
        assert!(
            elapsed < Duration::from_millis(150),
            "serial fan-out: {elapsed:?}"
        );
        // The answers keep config order whatever order they arrived in.
        let slugs: Vec<&str> = answers
            .iter()
            .map(|a| a.as_ref().unwrap()[0].quest.slug.as_str())
            .collect();
        assert_eq!(slugs, ["one", "two", "three"]);
        assert_eq!(ssh.calls()[0].1, LIST_ARGV);
    }

    #[test]
    fn a_timeout_is_unreachable_rather_than_an_error() {
        let status = interpret(SshOutcome::TimedOut, TIMEOUT).unwrap_err();
        assert_eq!(status.marker(), Some(UNREACHABLE));
        assert!(status.reason().unwrap().contains("5s"), "{status:?}");
    }

    #[test]
    fn a_failed_exit_reports_what_the_far_end_said() {
        let status = interpret(
            SshOutcome::Done {
                code: Some(127),
                stdout: String::new(),
                stderr: "bash: q: command not found\n".to_string(),
            },
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(status.marker(), Some(UNREACHABLE));
        assert!(status.reason().unwrap().contains("command not found"));

        // Killed by a signal: no code at all.
        let status = interpret(
            SshOutcome::Done {
                code: None,
                stdout: String::new(),
                stderr: String::new(),
            },
            TIMEOUT,
        )
        .unwrap_err();
        assert_eq!(status.marker(), Some(UNREACHABLE));
    }

    #[test]
    fn garbage_and_foreign_json_are_incompatible_not_a_panic() {
        for stdout in [
            "not json at all",
            "{\"quests\": []}",
            "[{\"id\": \"q-1\"}]",
            "",
            "   \n",
        ] {
            let status = interpret(ok(stdout.to_string()), TIMEOUT).unwrap_err();
            assert_eq!(status.marker(), Some(INCOMPATIBLE), "accepted `{stdout}`");
        }
    }

    #[test]
    fn a_newer_remote_with_extra_fields_still_parses() {
        let mut value: serde_json::Value = serde_json::from_str(&payload("one")).unwrap();
        value[0]["something_from_the_future"] = serde_json::json!("hello");
        // And a field this q knows but an older one never sent.
        value[0].as_object_mut().unwrap().remove("progress");
        let quests = parse(&value.to_string()).unwrap();
        assert_eq!(quests[0].quest.slug, "one");
        assert_eq!(quests[0].progress, None);
    }

    #[test]
    fn a_good_round_is_cached_and_a_failed_one_reads_it_back() {
        let db = Db::open_in_memory().unwrap();
        let remotes = [remote("ws")];
        let targets: Vec<&Remote> = remotes.iter().collect();

        let fresh = resolve(
            Some(&db),
            &targets,
            vec![Ok(parse(&payload("one")).unwrap())],
            1000,
        );
        assert_eq!(fresh[0].status, RemoteStatus::Ok);
        assert!(!fresh[0].stale);
        assert_eq!(fresh[0].fetched_at, Some(1000));
        assert_eq!(fresh[0].quests[0].quest.slug, "one");
        assert_eq!(fresh[0].note(), None);

        let down = resolve(
            Some(&db),
            &targets,
            vec![Err(RemoteStatus::Unreachable("host is down".to_string()))],
            2000,
        );
        assert!(down[0].stale, "the cache was not used");
        assert_eq!(
            down[0].fetched_at,
            Some(1000),
            "the stale timestamp is kept"
        );
        assert_eq!(down[0].quests[0].quest.slug, "one");
        let note = down[0].note().unwrap();
        assert!(
            note.contains(UNREACHABLE) && note.contains("host is down"),
            "{note}"
        );
    }

    #[test]
    fn a_remote_that_never_answered_shows_nothing_rather_than_failing() {
        let db = Db::open_in_memory().unwrap();
        let remotes = [remote("ws")];
        let targets: Vec<&Remote> = remotes.iter().collect();
        let out = resolve(
            Some(&db),
            &targets,
            vec![Err(RemoteStatus::Unreachable("nope".to_string()))],
            10,
        );
        assert!(out[0].quests.is_empty());
        assert!(!out[0].stale);
        assert_eq!(out[0].fetched_at, None);
    }

    #[test]
    fn without_a_database_the_fan_out_still_works() {
        let remotes = [remote("ws")];
        let targets: Vec<&Remote> = remotes.iter().collect();
        let out = resolve(None, &targets, vec![Ok(parse(&payload("one")).unwrap())], 1);
        assert_eq!(out[0].quests.len(), 1);
        let out = resolve(
            None,
            &targets,
            vec![Err(RemoteStatus::Unreachable("x".to_string()))],
            1,
        );
        assert!(out[0].quests.is_empty());
    }

    fn ctx_with(remotes: &[Remote], ssh: Box<dyn Ssh>) -> Ctx {
        let mut config = crate::config::Config::default();
        config.machine.name = "laptop".to_string();
        config.remotes = remotes.to_vec();
        let db = Db::open_in_memory().unwrap();
        let tmux = Box::new(crate::tmux::FixtureTmux::new(std::path::PathBuf::from(
            "/nonexistent/tmux.json",
        )));
        Ctx::for_tests(config, db, tmux).with_ssh(ssh)
    }

    fn names(results: &[RemoteResult]) -> Vec<&str> {
        results.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn no_remotes_configured_means_no_ssh_at_all() {
        // `NoSsh` fails every call, so a single invocation would show up as an
        // unreachable row rather than as nothing.
        let ctx = ctx_with(&[], Box::new(stub::NoSsh));
        assert!(targets(&ctx).is_empty());
        assert!(fetch_all(&ctx).is_empty());
    }

    #[test]
    fn no_remote_skips_the_fan_out() {
        let remotes = [remote("ws")];
        let ctx = ctx_with(&remotes, Box::new(stub::NoSsh)).with_no_remote(true);
        assert!(targets(&ctx).is_empty());
        assert!(fetch_all(&ctx).is_empty());

        // The same config without the guard does reach for the remote.
        let ctx = ctx_with(&remotes, Box::new(stub::NoSsh));
        assert_eq!(names(&fetch_all(&ctx)), ["ws"]);
    }

    #[test]
    fn a_machine_filter_narrows_the_fan_out_to_that_one_remote() {
        let remotes = [remote("ws"), remote("box")];
        let ssh = || {
            Box::new(StubSsh::new(&[
                ("ws-host", ok(payload("one"))),
                ("box-host", ok(payload("two"))),
            ])) as Box<dyn Ssh>
        };
        assert_eq!(names(&fetch_all(&ctx_with(&remotes, ssh()))), ["ws", "box"]);
        assert_eq!(
            names(&fetch_all(
                &ctx_with(&remotes, ssh()).with_machine(Some("box"))
            )),
            ["box"]
        );
        // The local machine is not a remote: nothing to ask.
        let ctx = ctx_with(&remotes, Box::new(stub::NoSsh)).with_machine(Some("laptop"));
        assert!(fetch_all(&ctx).is_empty());
    }

    #[test]
    fn one_dead_remote_does_not_spoil_the_round() {
        let remotes = [remote("up"), remote("down")];
        let ssh = StubSsh::new(&[
            ("up-host", ok(payload("one"))),
            ("down-host", SshOutcome::TimedOut),
        ]);
        let results = fetch_all(&ctx_with(&remotes, Box::new(ssh)));
        assert_eq!(results[0].status, RemoteStatus::Ok);
        assert_eq!(results[0].quests[0].quest.slug, "one");
        assert_eq!(results[1].status.marker(), Some(UNREACHABLE));
        assert!(results[1].quests.is_empty());
    }

    #[test]
    fn find_names_the_config_key_when_there_is_no_such_remote() {
        let remotes = [remote("ws")];
        assert_eq!(find(&remotes, "ws").unwrap().ssh, "ws-host");
        let e = find(&remotes, "nope").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found")
        );
        assert!(e.to_string().contains("remotes"), "{e}");
    }
}
