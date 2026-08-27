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

use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::Ctx;
use crate::cli::QuestState as StateFilter;
use crate::commands::QuestView;
use crate::config::{Config, Remote};
use crate::db::Db;
use crate::error::QError;
use crate::model::now;
use crate::output;

/// SPEC §15: one fan-out round waits at most this long per remote. Not a config
/// key — SPEC §20 names none, and a listing that can block longer than this is
/// not a listing.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// What a remote is always asked for. `--no-remote` is the recursion guard:
/// without it the remote would fan out to *its* remotes, us included.
pub const LIST_ARGV: [&str; 4] = ["q", "list", "--json", "--no-remote"];

/// The remote's `q list`, filtered the same way this one is. The flags have to
/// travel: a remote answering its own default listing would silently contradict
/// a `--all` or `--state` the user actually asked for.
pub fn list_argv(all: bool, state: Option<StateFilter>) -> Vec<String> {
    let mut argv: Vec<String> = LIST_ARGV.iter().map(|a| (*a).to_string()).collect();
    if all {
        argv.push("--all".to_string());
    }
    if let Some(state) = state {
        argv.push("--state".to_string());
        argv.push(state_flag(state).to_string());
    }
    argv
}

/// The `--state` value clap would have parsed, spelled back out.
fn state_flag(state: StateFilter) -> &'static str {
    match state {
        StateFilter::Active => "active",
        StateFilter::Idle => "idle",
        StateFilter::Finished => "finished",
    }
}

/// The envelope keys of `q list --json` (SPEC §15 / §16), named here because
/// both the emitter and this parser have to agree on them.
pub const QUESTS: &str = "quests";
pub const MACHINES: &str = "machines";
/// The key holding a row's machine — `remotes[].name`, stamped by whoever
/// merged the row rather than trusted from the far end.
pub const MACHINE: &str = "machine";
/// The key holding a row's provenance; see [`crate::commands::Origin`].
pub const SOURCE: &str = "source";

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

/// `Send + Sync` because the fan-out shares one client across threads — and
/// because the TUI's [`Poller`] holds the same client from its own thread.
pub trait Ssh: Send + Sync {
    /// `ssh <alias> <argv…>`, given up on after `timeout`.
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome;

    /// `ssh -t <alias> <argv…>`, interactively: SPEC §15's remote `q enter`.
    /// Replaces this process, exactly as a local `tmux attach` does, so on
    /// success it does not return.
    fn attach(&self, alias: &str, argv: &[String]) -> anyhow::Result<()>;

    /// [`Ssh::attach`] for a caller that needs its process back — the TUI's
    /// `[ui] return_after_detach`. Runs as a child and returns when the far
    /// end's tmux client detaches.
    fn attach_child(&self, alias: &str, argv: &[String]) -> anyhow::Result<()>;
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
/// deadline. `ConnectTimeout` is ssh's own budget for the TCP handshake, set to
/// *half* the deadline so the common failure (host down, unroutable) gives up
/// well inside the round rather than burning the whole budget.
fn ssh_argv(alias: &str, argv: &[&str], timeout: Duration) -> Vec<String> {
    let mut out = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={}", (timeout.as_secs() / 2).max(1)),
        alias.to_string(),
    ];
    out.extend(argv.iter().map(|a| (*a).to_string()));
    out
}

/// The argv of an interactive `ssh`. No `BatchMode` and no `ConnectTimeout`,
/// unlike [`ssh_argv`]: there *is* someone at the keyboard here, a passphrase
/// prompt is the point rather than a hang, and an attach has no deadline.
/// `-t` forces a tty, without which the far end's `tmux attach` refuses.
pub fn attach_argv(alias: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec!["-t".to_string(), alias.to_string()];
    out.extend(argv.iter().cloned());
    out
}

/// ssh hands the remote command to a shell, so an argument that is more than
/// one shell word has to arrive as one. Everything `q` sends is a slug or a
/// flag, but `[tmux] session_prefix` is free-form config and ends up inside
/// the tmux target.
pub fn sh_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "=_-./:@%+,".contains(c));
    if plain {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', r"'\''"))
}

fn attach_failed(alias: &str, e: &std::io::Error) -> QError {
    match e.kind() {
        std::io::ErrorKind::NotFound => QError::Other("ssh not found on PATH".to_string()),
        _ => QError::Other(format!("cannot ssh to `{alias}`: {e}")),
    }
}

pub struct RealSsh;

/// How often the deadline is checked while the child runs.
const POLL: Duration = Duration::from_millis(20);

/// Most a single stream may buffer. A remote is expected to send a listing, not
/// a stream: past this the read end is dropped, which stops a hostile or broken
/// far end from spending our memory for the whole deadline.
const MAX_OUTPUT: u64 = 1 << 20;

impl Ssh for RealSsh {
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome {
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_argv(alias, argv, timeout));
        run_with_deadline(cmd, timeout)
    }

    fn attach(&self, alias: &str, argv: &[String]) -> anyhow::Result<()> {
        use std::os::unix::process::CommandExt;
        let e = Command::new("ssh").args(attach_argv(alias, argv)).exec();
        Err(attach_failed(alias, &e).into())
    }

    fn attach_child(&self, alias: &str, argv: &[String]) -> anyhow::Result<()> {
        // Inherited stdio on purpose: the ssh *is* the terminal until the far
        // end detaches, and this call blocks for exactly that long.
        let status = Command::new("ssh")
            .args(attach_argv(alias, argv))
            .status()
            .map_err(|e| attach_failed(alias, &e))?;
        if !status.success() {
            return Err(QError::Other(format!("`ssh {alias}` exited with {status}")).into());
        }
        Ok(())
    }
}

/// Run `cmd` to completion or to `timeout`, whichever comes first — and return
/// no later than `timeout` either way.
///
/// The deadline has to be hard, because `q list` is also a TUI tick. That rules
/// out joining the pipe drains: a pipe's write end can outlive the child that
/// was handed it, and under ssh multiplexing (which SPEC §23 #6 recommends) it
/// routinely does — the mux master keeps a dup of the client's stderr, so
/// killing the client leaves our read blocked with nobody left to close it. So
/// the drains are detached threads writing into shared buffers, and a drain
/// that has not finished by the deadline is simply abandoned: it holds nothing
/// but its own capped buffer and ends by itself when the fd finally closes.
fn run_with_deadline(mut cmd: Command, timeout: Duration) -> SshOutcome {
    let spawned = cmd
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
    let deadline = Instant::now() + timeout;

    // Drained off-thread: a child that fills a pipe would block forever, and
    // this call has a deadline to keep.
    let (done_tx, done) = mpsc::channel();
    let out = drain(child.stdout.take(), done_tx.clone());
    let err = drain(child.stderr.take(), done_tx);

    let ending = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ending::Exited(status.code()),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Ending::TimedOut;
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                // Nothing else reaps this child: `Child::drop` neither kills
                // nor waits, so without this it stays a live ssh and a zombie.
                let _ = child.kill();
                let _ = child.wait();
                break Ending::Broken(format!("cannot wait for ssh: {e}"));
            }
        }
    };

    match ending {
        // Only this arm uses the output, so only this arm waits for it — and
        // never past the deadline the child already respected.
        Ending::Exited(code) => {
            await_drains(&done, deadline);
            SshOutcome::Done {
                code,
                stdout: taken(&out),
                stderr: taken(&err),
            }
        }
        Ending::TimedOut => SshOutcome::TimedOut,
        Ending::Broken(e) => SshOutcome::Failed(e),
    }
}

/// Wait for both drains, giving up at `deadline` and leaving them running.
fn await_drains(done: &mpsc::Receiver<()>, deadline: Instant) {
    for _ in 0..2 {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || done.recv_timeout(left).is_err() {
            return;
        }
    }
}

/// How the child run ended, before its output is folded back in.
enum Ending {
    Exited(Option<i32>),
    TimedOut,
    Broken(String),
}

/// Shared with the detached reader, which appends between reads rather than at
/// the end, so abandoning it still yields everything that had arrived.
type Buffer = Arc<Mutex<Vec<u8>>>;

fn drain(pipe: Option<impl Read + Send + 'static>, done: mpsc::Sender<()>) -> Buffer {
    let buf: Buffer = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&buf);
    std::thread::spawn(move || {
        if let Some(pipe) = pipe {
            let mut pipe = pipe.take(MAX_OUTPUT);
            let mut chunk = [0u8; 8 * 1024];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => lock(&into).extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }
        let _ = done.send(());
    });
    buf
}

fn taken(buf: &Buffer) -> String {
    String::from_utf8_lossy(&lock(buf)).into_owned()
}

/// Poison is not news here: the reader only ever appends, so the bytes that did
/// arrive are still exactly the bytes that arrived.
fn lock(buf: &Buffer) -> std::sync::MutexGuard<'_, Vec<u8>> {
    buf.lock().unwrap_or_else(|e| e.into_inner())
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
    /// not parallel take N times as long as one that is. A delay longer than
    /// the deadline times out, exactly as the real backend would.
    #[serde(default)]
    pub delay_ms: u64,
}

impl Ssh for FixtureSsh {
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome {
        log(alias, argv);
        let mut script = script();
        let Some(host) = script.hosts.remove(alias) else {
            return SshOutcome::Failed(format!("no fixture host `{alias}`"));
        };
        let delay = Duration::from_millis(host.delay_ms);
        if delay > timeout {
            std::thread::sleep(timeout);
            return SshOutcome::TimedOut;
        }
        if !delay.is_zero() {
            std::thread::sleep(delay);
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

    /// Recorded, never run: a fixture that `exec`ed would replace the test.
    fn attach(&self, alias: &str, argv: &[String]) -> anyhow::Result<()> {
        log_attach("attach", alias, argv);
        Ok(())
    }

    fn attach_child(&self, alias: &str, argv: &[String]) -> anyhow::Result<()> {
        log_attach("attach-child", alias, argv);
        Ok(())
    }
}

/// A missing or unreadable script is an empty one — every alias then fails.
fn script() -> SshScript {
    std::env::var_os("Q_FIXTURE_SSH")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// An attach, in the same log: `attach`, the alias, then the remote argv.
/// Marked rather than shaped like a `run` line so a test can tell the two
/// apart, and so exec-vs-child — the `[ui] return_after_detach` split — is on
/// the record.
fn log_attach(kind: &str, alias: &str, argv: &[String]) {
    let mut parts = vec![alias.to_string()];
    parts.extend(argv.iter().cloned());
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    log(kind, &parts);
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
///
/// Internally tagged, so it flattens into whatever carries it:
/// `{"status": "ok"}` / `{"status": "unreachable", "reason": "…"}` rather than
/// bd-8lz.5.1's nested `{"status": {"status": …}}` (bd-8lz.5.2 owns the
/// `--json` contract).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum RemoteStatus {
    Ok,
    /// No answer: down, unroutable, no `q` there, or slower than [`TIMEOUT`].
    Unreachable {
        reason: String,
    },
    /// An answer this `q` cannot read — a `q` too old or too new at the far end.
    Incompatible {
        reason: String,
    },
}

impl RemoteStatus {
    pub fn unreachable(reason: impl Into<String>) -> RemoteStatus {
        RemoteStatus::Unreachable {
            reason: reason.into(),
        }
    }

    pub fn incompatible(reason: impl Into<String>) -> RemoteStatus {
        RemoteStatus::Incompatible {
            reason: reason.into(),
        }
    }

    /// The listing marker (SPEC §15), or `None` when all is well.
    pub fn marker(&self) -> Option<&'static str> {
        match self {
            RemoteStatus::Ok => None,
            RemoteStatus::Unreachable { .. } => Some(UNREACHABLE),
            RemoteStatus::Incompatible { .. } => Some(INCOMPATIBLE),
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            RemoteStatus::Ok => None,
            RemoteStatus::Unreachable { reason } | RemoteStatus::Incompatible { reason } => {
                Some(reason)
            }
        }
    }
}

/// One Quest as a remote sent it: the parsed view this `q` renders and sorts
/// by, next to the object it arrived in.
///
/// Both, because they answer different questions. `view` is what a listing is
/// built from; `raw` is what `--json` re-emits, so a field a newer `q` at the
/// far end knows and this one does not survives the trip (it would be dropped
/// by re-serializing `view`).
#[derive(Debug)]
pub struct RemoteQuest {
    pub view: QuestView,
    pub raw: serde_json::Value,
}

/// One remote's Quests and how they were come by. Never an error: a machine
/// that is down contributes a row saying so, not a failed command.
#[derive(Debug)]
pub struct RemoteResult {
    /// `remotes[].name` — the value of a Quest's `machine` column over there.
    pub name: String,
    pub ssh: String,
    pub status: RemoteStatus,
    /// This round's answer, or the last cached one when the round failed.
    pub quests: Vec<RemoteQuest>,
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

/// Ask every remote at once, with this invocation's own listing filters, and
/// fold in the cache. The whole round takes about as long as the slowest
/// remote, and at most [`TIMEOUT`].
pub fn fetch_all(ctx: &Ctx, all: bool, state: Option<StateFilter>) -> Vec<RemoteResult> {
    let targets = targets(ctx);
    if targets.is_empty() {
        return Vec::new();
    }
    let argv = list_argv(all, state);
    let answers = fan_out(ctx.ssh(), &targets, &argv, TIMEOUT);
    resolve(ctx.db().ok(), &targets, answers, now())
}

// ----------------------------------------------------------------- polling

/// One completed fan-out round on its way from the poller thread to the UI
/// thread. Opaque on purpose: turning it into [`RemoteResult`]s needs the
/// cache, and the database belongs to the thread that owns the `Ctx`.
pub struct Round {
    targets: Vec<Remote>,
    answers: Vec<Result<Answer, RemoteStatus>>,
}

/// The TUI's remote tick (SPEC §17: `[ui] tick_remote`, 10 s, against 2 s
/// locally).
///
/// One worker thread running one round at a time. The UI thread never waits on
/// ssh — it looks in between frames with [`Poller::take`] and picks up whatever
/// has finished — so a remote that is slow or dead costs a round, never a
/// frame and never the local tick.
///
/// Single-threaded by design rather than by convenience: an ssh whose pipe is
/// held open past the deadline leaves an abandoned drain holding its buffer
/// until the far fd closes, which under `ControlPersist` is tens of seconds
/// later. Rounds that could overlap would pile those up without bound; here a
/// round cannot start until the last one has returned.
pub struct Poller {
    rounds: mpsc::Receiver<Round>,
    /// Also the shutdown signal: dropping the `Poller` closes it, and the
    /// worker exits at the end of the round it is in.
    nudges: mpsc::SyncSender<()>,
}

impl Poller {
    /// Start polling, or `None` when there is nothing to poll — no remotes,
    /// `--no-remote`, or a `--machine` that is not one of them.
    ///
    /// The round is asked for the *whole* listing (`--all`), unlike the CLI's,
    /// because the TUI's `f` toggle filters rows it already has rather than
    /// re-fetching: a keypress must not have to wait for a fan-out.
    pub fn spawn(ctx: &Ctx, every: Duration) -> Option<Poller> {
        let targets: Vec<Remote> = targets(ctx).into_iter().cloned().collect();
        if targets.is_empty() {
            return None;
        }
        let ssh = ctx.ssh_shared();
        let argv = list_argv(true, None);
        let (send_round, rounds) = mpsc::channel();
        let (nudges, wake) = mpsc::sync_channel(1);
        // Detached: a round holds for at most `TIMEOUT`, and joining it would
        // put that on the TUI's exit path for nothing.
        std::thread::spawn(move || {
            poll_loop(ssh.as_ref(), &targets, &argv, every, &send_round, &wake)
        });
        Some(Poller { rounds, nudges })
    }

    /// The newest round that has finished since the last look, if any. Never
    /// blocks; older rounds are dropped rather than replayed.
    pub fn take(&self) -> Option<Round> {
        let mut latest = None;
        while let Ok(round) = self.rounds.try_recv() {
            latest = Some(round);
        }
        latest
    }

    /// Ask for a round now (the TUI's `x`). Coalesced: a nudge that arrives
    /// while one is queued or running is dropped, so holding `x` down cannot
    /// queue a fan-out per keypress.
    pub fn nudge(&self) {
        let _ = self.nudges.try_send(());
    }
}

fn poll_loop(
    ssh: &dyn Ssh,
    targets: &[Remote],
    argv: &[String],
    every: Duration,
    send_round: &mpsc::Sender<Round>,
    wake: &mpsc::Receiver<()>,
) {
    loop {
        let borrowed: Vec<&Remote> = targets.iter().collect();
        let answers = fan_out(ssh, &borrowed, argv, TIMEOUT);
        let round = Round {
            targets: targets.to_vec(),
            answers,
        };
        // The receiver is gone: the TUI has exited and this thread with it.
        if send_round.send(round).is_err() {
            return;
        }
        match wake.recv_timeout(every) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// A polled round, folded through the cache exactly as [`fetch_all`] folds a
/// synchronous one.
pub fn resolve_round(ctx: &Ctx, round: Round) -> Vec<RemoteResult> {
    let targets: Vec<&Remote> = round.targets.iter().collect();
    resolve(ctx.db().ok(), &targets, round.answers, now())
}

/// Every machine name `--machine` may name: this one, plus the remotes. Used to
/// turn a typo into an error instead of an empty listing.
pub fn known_machines(config: &Config) -> Vec<&str> {
    let mut names = vec![config.machine.name.as_str()];
    names.extend(config.remotes.iter().map(|r| r.name.as_str()));
    names
}

/// `--machine <name>`: well-formed *and* a machine this `q` knows about. A name
/// that is neither the local machine nor a configured remote can only be a
/// typo, and answering it with "no quests" would read as a fact about that
/// machine rather than as the mistake it is.
pub fn validate_target(config: &Config, name: &str) -> anyhow::Result<()> {
    crate::config::validate_machine_name(name)?;
    let known = known_machines(config);
    if known.contains(&name) {
        return Ok(());
    }
    Err(QError::NotFound(format!(
        "machine `{name}` — known machines: {}",
        known.join(", ")
    ))
    .into())
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
    argv: &[String],
    timeout: Duration,
) -> Vec<Result<Answer, RemoteStatus>> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let argv = argv.as_slice();
    std::thread::scope(|scope| {
        let handles: Vec<_> = targets
            .iter()
            .map(|remote| {
                let alias = remote.ssh.as_str();
                scope.spawn(move || interpret(ssh.run(alias, argv, timeout), timeout))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join().unwrap_or_else(|_| {
                    Err(RemoteStatus::unreachable(
                        "the ssh call panicked".to_string(),
                    ))
                })
            })
            .collect()
    })
}

/// One remote's answer: the Quests it sent and the bytes they arrived in. The
/// bytes are what the cache keeps — re-serializing the parsed views would drop
/// every field a newer `q` at the far end knows and this one does not.
///
/// `pub` because the TUI's [`Poller`] carries a whole round back from its own
/// thread before the database on the UI thread turns it into [`RemoteResult`]s.
#[derive(Debug)]
pub struct Answer {
    quests: Vec<RemoteQuest>,
    raw: String,
}

/// What the far end said, turned into either an [`Answer`] or a reason it is
/// not usable. Nothing here can panic: garbage, a non-zero exit and JSON from
/// another version of `q` all land on a status.
fn interpret(outcome: SshOutcome, timeout: Duration) -> Result<Answer, RemoteStatus> {
    match outcome {
        SshOutcome::TimedOut => Err(RemoteStatus::unreachable(format!(
            "no answer within {}s",
            timeout.as_secs()
        ))),
        SshOutcome::Failed(e) => Err(RemoteStatus::unreachable(e)),
        SshOutcome::Done {
            code,
            stdout,
            stderr,
        } => {
            if code != Some(0) {
                return Err(RemoteStatus::unreachable(exit_reason(code, &stderr)));
            }
            let quests = parse(&stdout).map_err(RemoteStatus::incompatible)?;
            Ok(Answer {
                quests,
                raw: stdout.trim().to_string(),
            })
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

/// The far end's `q list --json`, row by row.
///
/// The document is the envelope `q list --json` emits — `{"quests": [...],
/// "machines": [...]}` — and a bare array is accepted as well, which is what a
/// `q` from before the envelope sends and what its cache rows still hold. Only
/// `quests` is read: a remote's own `machines` describes *its* fan-out, and
/// under `--no-remote` it never has one.
///
/// Each row is kept twice (see [`RemoteQuest`]): parsed, and verbatim. Unknown
/// fields are ignored by the parse and survive in the verbatim copy, so a newer
/// `q` at the far end still lists; a missing required field does not parse.
pub fn parse(stdout: &str) -> Result<Vec<RemoteQuest>, String> {
    let text = stdout.trim();
    if text.is_empty() {
        return Err("empty response".to_string());
    }
    let document: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("cannot read `q list --json`: {e}"))?;
    let rows = match &document {
        serde_json::Value::Array(rows) => rows,
        serde_json::Value::Object(map) => match map.get(QUESTS).and_then(|q| q.as_array()) {
            Some(rows) => rows,
            None => return Err("`q list --json` has no `quests` array".to_string()),
        },
        _ => return Err("`q list --json` is neither an array nor an object".to_string()),
    };
    rows.iter()
        .map(|raw| {
            serde_json::from_value(raw.clone())
                .map(|view| RemoteQuest {
                    view,
                    raw: raw.clone(),
                })
                .map_err(|e| format!("cannot read a quest in `q list --json`: {e}"))
        })
        .collect()
}

/// Pairs each answer with its remote, writing the good ones to the cache and
/// falling back to it for the rest.
fn resolve(
    db: Option<&Db>,
    targets: &[&Remote],
    answers: Vec<Result<Answer, RemoteStatus>>,
    ts: i64,
) -> Vec<RemoteResult> {
    targets
        .iter()
        .zip(answers)
        .map(|(remote, answer)| match answer {
            Ok(mut answer) => {
                store(db, &remote.name, &answer.raw, ts);
                attribute(&mut answer.quests, &remote.name);
                RemoteResult {
                    name: remote.name.clone(),
                    ssh: remote.ssh.clone(),
                    status: RemoteStatus::Ok,
                    quests: answer.quests,
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

/// `remotes[].name` wins over the `[machine] name` the far end reports.
///
/// The two can disagree — a box configured as `workstation` reached through a
/// remote called `ws`. The config name is the one the user types into
/// `--machine` and the one this `q` shows in the machine column, so every row
/// is stamped with it; the remote's own idea of its name is not carried over.
fn attribute(quests: &mut [RemoteQuest], name: &str) {
    for quest in quests {
        if quest.view.quest.machine != name {
            quest.view.quest.machine = name.to_string();
        }
        // The verbatim row is what `--json` re-emits, so it is stamped too:
        // a consumer must never see two names for one machine.
        if let Some(row) = quest.raw.as_object_mut() {
            row.insert(
                MACHINE.to_string(),
                serde_json::Value::String(name.to_string()),
            );
        }
    }
}

/// Best effort in both directions: a cache that cannot be written or read costs
/// staleness, never the listing. The payload is stored exactly as it arrived —
/// see [`Answer`].
fn store(db: Option<&Db>, name: &str, payload: &str, ts: i64) {
    let Some(db) = db else { return };
    let _ = db.put_remote_cache(name, payload, ts);
}

fn load(db: Option<&Db>, name: &str) -> Option<(Vec<RemoteQuest>, i64)> {
    let cached = db?.get_remote_cache(name).ok().flatten()?;
    let mut quests = parse(&cached.payload).ok()?;
    attribute(&mut quests, name);
    Some((quests, cached.fetched_at))
}

/// The configured remote called `name`, for the commands that dispatch to one.
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

        fn attach(&self, alias: &str, _: &[String]) -> anyhow::Result<()> {
            Err(no_ssh(alias))
        }

        fn attach_child(&self, alias: &str, _: &[String]) -> anyhow::Result<()> {
            Err(no_ssh(alias))
        }
    }

    fn no_ssh(alias: &str) -> anyhow::Error {
        crate::error::QError::Other(format!(
            "this test has no ssh (pass one with `Ctx::with_ssh`): {alias}"
        ))
        .into()
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
        attaches: Vec<(String, String, Vec<String>)>,
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

        /// Every attach, as `(kind, alias, argv)` — `kind` is `exec` or
        /// `child`, the `[ui] return_after_detach` split.
        pub(crate) fn attaches(&self) -> Vec<(String, String, Vec<String>)> {
            self.state.lock().unwrap().attaches.clone()
        }

        fn record_attach(&self, kind: &str, alias: &str, argv: &[String]) {
            self.state.lock().unwrap().attaches.push((
                kind.to_string(),
                alias.to_string(),
                argv.to_vec(),
            ));
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

        /// Recorded, never run: a stub that `exec`ed would replace the test.
        fn attach(&self, alias: &str, argv: &[String]) -> anyhow::Result<()> {
            self.record_attach("exec", alias, argv);
            Ok(())
        }

        fn attach_child(&self, alias: &str, argv: &[String]) -> anyhow::Result<()> {
            self.record_attach("child", alias, argv);
            Ok(())
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
        payload_from("ws", slug)
    }

    /// The same, from a machine that calls itself `machine`.
    fn payload_from(machine: &str, slug: &str) -> String {
        let view = QuestView::new(Quest::new(slug, "/tmp", machine), &[]);
        serde_json::to_string(&[view]).unwrap()
    }

    /// A remote's answer, the way `interpret` would have built it.
    fn answer(raw: &str) -> Answer {
        Answer {
            quests: parse(raw).unwrap(),
            raw: raw.to_string(),
        }
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
                // Half the deadline, so an unroutable host gives up inside it.
                "ConnectTimeout=2",
                "ws",
                "q",
                "list",
                "--json",
                "--no-remote"
            ]
        );
        // However short the deadline, ssh is never told to wait zero seconds.
        assert!(
            ssh_argv("ws", &LIST_ARGV, Duration::from_millis(1))
                .contains(&"ConnectTimeout=1".to_string())
        );
    }

    #[test]
    fn the_listing_filters_travel_to_the_remote() {
        assert_eq!(list_argv(false, None), LIST_ARGV);
        assert_eq!(
            list_argv(true, Some(StateFilter::Idle)),
            [
                "q",
                "list",
                "--json",
                "--no-remote",
                "--all",
                "--state",
                "idle"
            ]
        );
        // Every filter this `q` accepts has a spelling the far end accepts.
        for state in [
            StateFilter::Active,
            StateFilter::Idle,
            StateFilter::Finished,
        ] {
            let argv = list_argv(false, Some(state));
            assert_eq!(argv[..4], LIST_ARGV);
            assert_eq!(argv[4], "--state");
        }
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

        let answers = fan_out(&ssh, &targets, &list_argv(false, None), TIMEOUT);

        assert_eq!(answers.len(), 3);
        assert!(answers.iter().all(|a| a.is_ok()));
        // Overlap, not the wall clock: a loaded CI runner may take as long as
        // it likes, but three calls cannot be in flight at once unless they
        // really did run in parallel.
        assert_eq!(ssh.peak(), 3, "the calls did not overlap");
        // The answers keep config order whatever order they arrived in.
        let slugs: Vec<&str> = answers
            .iter()
            .map(|a| a.as_ref().unwrap().quests[0].view.quest.slug.as_str())
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
            // An object without the envelope key.
            "{\"rows\": []}",
            // A row that is not a Quest.
            "[{\"id\": \"q-1\"}]",
            "{\"quests\": [{\"id\": \"q-1\"}]}",
            "42",
            "",
            "   \n",
        ] {
            let status = interpret(ok(stdout.to_string()), TIMEOUT).unwrap_err();
            assert_eq!(status.marker(), Some(INCOMPATIBLE), "accepted `{stdout}`");
        }
    }

    /// Both shapes of the wire format: the envelope `q list --json` emits now,
    /// and the bare array a `q` from before it sends — which is also what its
    /// cache rows still hold.
    #[test]
    fn the_envelope_and_the_bare_array_both_parse() {
        let array = payload("one");
        let enveloped = format!("{{\"quests\": {array}, \"machines\": []}}");
        for text in [array.as_str(), enveloped.as_str(), "{\"quests\": []}"] {
            let quests = parse(text).unwrap_or_else(|e| panic!("{text} → {e}"));
            assert!(quests.len() <= 1);
        }
        assert_eq!(parse(&enveloped).unwrap()[0].view.quest.slug, "one");
    }

    #[test]
    fn a_newer_remote_with_extra_fields_still_parses() {
        let mut value: serde_json::Value = serde_json::from_str(&payload("one")).unwrap();
        value[0]["something_from_the_future"] = serde_json::json!("hello");
        // And a field this q knows but an older one never sent.
        value[0].as_object_mut().unwrap().remove("progress");
        let quests = parse(&value.to_string()).unwrap();
        assert_eq!(quests[0].view.quest.slug, "one");
        assert_eq!(quests[0].view.progress, None);
    }

    #[test]
    fn a_good_round_is_cached_and_a_failed_one_reads_it_back() {
        let db = Db::open_in_memory().unwrap();
        let remotes = [remote("ws")];
        let targets: Vec<&Remote> = remotes.iter().collect();

        let fresh = resolve(Some(&db), &targets, vec![Ok(answer(&payload("one")))], 1000);
        assert_eq!(fresh[0].status, RemoteStatus::Ok);
        assert!(!fresh[0].stale);
        assert_eq!(fresh[0].fetched_at, Some(1000));
        assert_eq!(fresh[0].quests[0].view.quest.slug, "one");
        assert_eq!(fresh[0].note(), None);

        let down = resolve(
            Some(&db),
            &targets,
            vec![Err(RemoteStatus::unreachable("host is down".to_string()))],
            2000,
        );
        assert!(down[0].stale, "the cache was not used");
        assert_eq!(
            down[0].fetched_at,
            Some(1000),
            "the stale timestamp is kept"
        );
        assert_eq!(down[0].quests[0].view.quest.slug, "one");
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
            vec![Err(RemoteStatus::unreachable("nope".to_string()))],
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
        let out = resolve(None, &targets, vec![Ok(answer(&payload("one")))], 1);
        assert_eq!(out[0].quests.len(), 1);
        let out = resolve(
            None,
            &targets,
            vec![Err(RemoteStatus::unreachable("x".to_string()))],
            1,
        );
        assert!(out[0].quests.is_empty());
    }

    fn ctx_with(remotes: &[Remote], ssh: std::sync::Arc<dyn Ssh>) -> Ctx {
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
        let ctx = ctx_with(&[], Arc::new(stub::NoSsh));
        assert!(targets(&ctx).is_empty());
        assert!(fetch_all(&ctx, false, None).is_empty());
    }

    #[test]
    fn no_remote_skips_the_fan_out() {
        let remotes = [remote("ws")];
        let ctx = ctx_with(&remotes, Arc::new(stub::NoSsh)).with_no_remote(true);
        assert!(targets(&ctx).is_empty());
        assert!(fetch_all(&ctx, false, None).is_empty());

        // The same config without the guard does reach for the remote.
        let ctx = ctx_with(&remotes, Arc::new(stub::NoSsh));
        assert_eq!(names(&fetch_all(&ctx, false, None)), ["ws"]);
    }

    #[test]
    fn a_machine_filter_narrows_the_fan_out_to_that_one_remote() {
        let remotes = [remote("ws"), remote("box")];
        let ssh = || {
            Arc::new(StubSsh::new(&[
                ("ws-host", ok(payload("one"))),
                ("box-host", ok(payload("two"))),
            ])) as Arc<dyn Ssh>
        };
        assert_eq!(
            names(&fetch_all(&ctx_with(&remotes, ssh()), false, None)),
            ["ws", "box"]
        );
        assert_eq!(
            names(&fetch_all(
                &ctx_with(&remotes, ssh()).with_machine(Some("box")),
                false,
                None
            )),
            ["box"]
        );
        // The local machine is not a remote: nothing to ask.
        let ctx = ctx_with(&remotes, Arc::new(stub::NoSsh)).with_machine(Some("laptop"));
        assert!(fetch_all(&ctx, false, None).is_empty());
    }

    #[test]
    fn one_dead_remote_does_not_spoil_the_round() {
        let remotes = [remote("up"), remote("down")];
        let ssh = StubSsh::new(&[
            ("up-host", ok(payload("one"))),
            ("down-host", SshOutcome::TimedOut),
        ]);
        let results = fetch_all(&ctx_with(&remotes, Arc::new(ssh)), false, None);
        assert_eq!(results[0].status, RemoteStatus::Ok);
        assert_eq!(results[0].quests[0].view.quest.slug, "one");
        assert_eq!(results[1].status.marker(), Some(UNREACHABLE));
        assert!(results[1].quests.is_empty());
    }

    // ------------------------------------------- the real child-process path

    /// `/bin/sh -c` stands in for ssh: it spawns, writes to both streams, exits
    /// and forks exactly like the real thing, with no host anywhere near it.
    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    /// Short enough that a test never waits on it, long enough that a busy
    /// runner still gets there.
    const SHORT: Duration = Duration::from_millis(300);
    /// What "the deadline held" is judged against. Far above [`SHORT`] so a
    /// loaded runner cannot fail these, and far below the 30 s the grandchildren
    /// below live for, so a deadline that is not enforced cannot pass them.
    const PATIENCE: Duration = Duration::from_secs(10);

    #[test]
    fn a_command_that_finishes_is_reported_with_both_its_streams() {
        let outcome = run_with_deadline(sh("printf hello; printf oops >&2; exit 3"), TIMEOUT);
        assert_eq!(
            outcome,
            SshOutcome::Done {
                code: Some(3),
                stdout: "hello".to_string(),
                stderr: "oops".to_string(),
            }
        );
    }

    #[test]
    fn a_binary_that_is_not_there_is_a_failure_rather_than_a_panic() {
        let outcome = run_with_deadline(Command::new("q-no-such-binary-anywhere"), TIMEOUT);
        assert_eq!(
            outcome,
            SshOutcome::Failed("ssh not found on PATH".to_string())
        );
    }

    #[test]
    fn a_child_that_will_not_finish_is_killed_at_the_deadline() {
        let started = Instant::now();
        assert_eq!(
            run_with_deadline(sh("sleep 30"), SHORT),
            SshOutcome::TimedOut
        );
        assert!(started.elapsed() < PATIENCE, "{:?}", started.elapsed());
    }

    /// The regression this whole shape exists for. Under ssh multiplexing the
    /// mux master keeps a dup of the client's stderr, so killing the client
    /// leaves our pipe open with nobody left to close it, and a drain that is
    /// joined unconditionally never returns. A grandchild that inherits the
    /// pipes and outlives its parent is that exact shape, without a host.
    #[test]
    fn a_pipe_held_open_after_the_kill_cannot_outlast_the_deadline() {
        let started = Instant::now();
        assert_eq!(
            run_with_deadline(sh("sleep 30 & sleep 30"), SHORT),
            SshOutcome::TimedOut
        );
        let elapsed = started.elapsed();
        assert!(
            elapsed < PATIENCE,
            "the deadline was not enforced: {elapsed:?}"
        );
    }

    /// The same pipe, but the child exits by itself: what it did say comes
    /// back, and the fd still held open does not hold the call past the
    /// deadline either.
    #[test]
    fn a_child_that_exits_leaving_its_pipe_open_still_answers_in_time() {
        let started = Instant::now();
        let outcome = run_with_deadline(sh("sleep 30 & printf hello"), SHORT);
        let elapsed = started.elapsed();
        assert!(
            elapsed < PATIENCE,
            "the deadline was not enforced: {elapsed:?}"
        );
        match outcome {
            SshOutcome::Done { code, stdout, .. } => {
                assert_eq!(code, Some(0));
                assert_eq!(stdout, "hello");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_remote_that_floods_the_pipe_is_capped_rather_than_buffered_whole() {
        let flood = MAX_OUTPUT * 4;
        let outcome = run_with_deadline(sh(&format!("yes | head -c {flood}")), TIMEOUT);
        match outcome {
            SshOutcome::Done { stdout, .. } => {
                assert!(!stdout.is_empty());
                assert!(stdout.len() as u64 <= MAX_OUTPUT, "{} bytes", stdout.len());
            }
            other => panic!("{other:?}"),
        }
    }

    // ------------------------------------------------------ names and caching

    #[test]
    fn a_machine_that_is_neither_this_one_nor_a_remote_is_an_error() {
        let mut config = Config::default();
        config.machine.name = "laptop".to_string();
        config.remotes = vec![remote("ws")];

        validate_target(&config, "laptop").unwrap();
        validate_target(&config, "ws").unwrap();
        assert_eq!(known_machines(&config), ["laptop", "ws"]);

        let e = validate_target(&config, "bogus").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found")
        );
        let said = e.to_string();
        assert!(said.contains("laptop") && said.contains("ws"), "{said}");
        // The shape is still checked, and first.
        assert!(validate_target(&config, "Not Valid").is_err());
    }

    /// `remotes[].name` is authoritative: the far end's own `[machine] name` is
    /// replaced by the one this config reaches it under, fresh and cached alike.
    #[test]
    fn a_remote_that_calls_itself_something_else_is_still_listed_under_its_config_name() {
        let db = Db::open_in_memory().unwrap();
        let remotes = [remote("ws")];
        let targets: Vec<&Remote> = remotes.iter().collect();

        let raw = payload_from("workstation", "over-there");
        let fresh = resolve(Some(&db), &targets, vec![Ok(answer(&raw))], 1000);
        assert_eq!(fresh[0].quests[0].view.quest.machine, "ws");

        let down = resolve(
            Some(&db),
            &targets,
            vec![Err(RemoteStatus::unreachable("down".to_string()))],
            2000,
        );
        assert!(down[0].stale);
        assert_eq!(down[0].quests[0].view.quest.machine, "ws");
    }

    #[test]
    fn the_cache_keeps_the_response_verbatim_so_a_newer_remote_loses_nothing() {
        let db = Db::open_in_memory().unwrap();
        let remotes = [remote("ws")];
        let targets: Vec<&Remote> = remotes.iter().collect();

        let mut value: serde_json::Value = serde_json::from_str(&payload("one")).unwrap();
        value[0]["something_from_the_future"] = serde_json::json!("hello");
        let raw = value.to_string();
        resolve(Some(&db), &targets, vec![Ok(answer(&raw))], 1000);

        let cached = db.get_remote_cache("ws").unwrap().unwrap();
        assert_eq!(cached.payload, raw, "the payload was re-serialized");
        let back: serde_json::Value = serde_json::from_str(&cached.payload).unwrap();
        assert_eq!(back[0]["something_from_the_future"], "hello");
    }

    // ------------------------------------------------------- attach & polling

    #[test]
    fn an_interactive_ssh_asks_for_a_tty_and_nothing_else() {
        // No `BatchMode`, no `ConnectTimeout`: there is someone at the
        // keyboard, and an attach has no deadline.
        assert_eq!(
            attach_argv("ws-host", &["tmux".to_string(), "attach".to_string()]),
            ["-t", "ws-host", "tmux", "attach"]
        );
    }

    #[test]
    fn an_argument_that_is_more_than_one_shell_word_is_quoted() {
        assert_eq!(sh_quote("=q-alpha"), "=q-alpha");
        assert_eq!(sh_quote("tmux"), "tmux");
        assert_eq!(sh_quote("=q alpha"), "'=q alpha'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("; rm -rf /"), "'; rm -rf /'");
    }

    #[test]
    fn an_attach_is_recorded_rather_than_run_by_the_stub() {
        let ssh = StubSsh::new(&[]);
        let argv = vec!["tmux".to_string(), "attach".to_string()];
        ssh.attach("ws-host", &argv).unwrap();
        ssh.attach_child("ws-host", &argv).unwrap();
        assert_eq!(
            ssh.attaches(),
            [
                ("exec".to_string(), "ws-host".to_string(), argv.clone()),
                ("child".to_string(), "ws-host".to_string(), argv),
            ]
        );
    }

    #[test]
    fn a_poller_has_nothing_to_poll_without_remotes() {
        let ctx = ctx_with(&[], Arc::new(stub::NoSsh));
        assert!(Poller::spawn(&ctx, Duration::from_millis(1)).is_none());

        let remotes = [remote("ws")];
        let ctx = ctx_with(&remotes, Arc::new(stub::NoSsh)).with_no_remote(true);
        assert!(Poller::spawn(&ctx, Duration::from_millis(1)).is_none());

        // Pinned to the local machine: there is no remote in the listing.
        let ctx = ctx_with(&remotes, Arc::new(stub::NoSsh)).with_machine(Some("laptop"));
        assert!(Poller::spawn(&ctx, Duration::from_millis(1)).is_none());
    }

    /// The property the TUI depends on: rounds arrive without the caller ever
    /// waiting on ssh, and two of them are never in flight at once — an
    /// abandoned pipe drain outlives its round, so overlapping rounds would
    /// pile them up (SPEC §23 #6).
    #[test]
    fn the_poller_delivers_rounds_and_never_overlaps_two() {
        let stub = Arc::new(
            StubSsh::new(&[
                ("ws-host", ok(payload("one"))),
                ("box-host", ok(payload("two"))),
            ])
            .with_delay(Duration::from_millis(20)),
        );
        let remotes = [remote("ws"), remote("box")];
        let ctx = ctx_with(&remotes, stub.clone() as Arc<dyn Ssh>);
        let poller = Poller::spawn(&ctx, Duration::from_millis(1)).expect("two remotes to poll");

        let deadline = Instant::now() + PATIENCE;
        let mut rounds = 0;
        while rounds < 3 && Instant::now() < deadline {
            if let Some(round) = poller.take() {
                let results = resolve_round(&ctx, round);
                assert_eq!(names(&results), ["ws", "box"]);
                assert_eq!(results[0].quests[0].view.quest.slug, "one");
                rounds += 1;
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        assert_eq!(rounds, 3, "the poller stopped delivering");
        // Two remotes are asked at once; two *rounds* never are.
        assert_eq!(stub.peak(), 2, "rounds overlapped");
        drop(poller);
    }

    /// `x` in the TUI nudges the poller. Coalesced: a nudge that arrives while
    /// one is already queued is dropped, so holding the key down cannot queue
    /// a fan-out per keypress.
    #[test]
    fn nudges_are_coalesced_rather_than_queued() {
        let stub = Arc::new(StubSsh::new(&[("ws-host", ok(payload("one")))]));
        let remotes = [remote("ws")];
        let ctx = ctx_with(&remotes, stub.clone() as Arc<dyn Ssh>);
        // Long enough that nothing here is the periodic tick.
        let poller = Poller::spawn(&ctx, Duration::from_secs(600)).expect("a remote to poll");

        // The round every poller opens with.
        let deadline = Instant::now() + PATIENCE;
        while poller.take().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        for _ in 0..20 {
            poller.nudge();
        }
        std::thread::sleep(Duration::from_millis(200));
        let mut extra = 0;
        while poller.take().is_some() {
            extra += 1;
        }
        // At least the nudge was honoured, and nowhere near twenty of them:
        // the queue holds one, plus at most the round already running.
        assert!((1..=2).contains(&extra), "{extra} rounds for twenty nudges");
        assert!(stub.calls().len() <= 3, "{:?}", stub.calls());
        drop(poller);
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
