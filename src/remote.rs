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

/// The deadline on a **proxied** command (SPEC §15, bd-8lz.5.3). Far longer
/// than [`TIMEOUT`]: a listing that blocks is a broken listing, but a
/// `q spawn` over there starts tmux and Claude, and a user who typed the
/// command is willing to wait for it. Still bounded — a `q` that never returns
/// must not leave a terminal wedged with no way back but Ctrl-C.
///
/// The far end is *not* killed when this passes (bd-8lz.5.7): the ssh client
/// is, and what that leaves behind on the remote is that bead's problem.
pub const PROXY_TIMEOUT: Duration = Duration::from_secs(60);

/// The cap on ssh's own `ConnectTimeout`; see [`ssh_argv`]. A host that has not
/// completed a handshake in this long is down, whatever the command's deadline.
pub const CONNECT_MAX: Duration = Duration::from_secs(5);

/// ssh's own exit code when it could not run the command at all — a refused
/// connection, an unknown host, a rejected key. `q` never exits with it, so it
/// is read as *the host did not answer* rather than as the far end's verdict.
pub const SSH_FAILED: i32 = 255;

/// What a POSIX shell exits when the command it was given is not on `PATH`.
/// The far end answered, so the machine is up; what is missing is `q`.
pub const NO_COMMAND: i32 = 127;

/// The **remote wire version** of this build: the contract two `q`s speak to
/// each other over ssh (SPEC §15). Bumped whenever that contract changes in a
/// way the other end has to know about — a new hidden global on a proxied
/// line, a change to the `q list --json` envelope, a change to what a
/// confirmation means. It is *not* the crate version: several wire-breaking
/// changes have already shipped inside `0.1.0`, so a semver floor could not
/// tell any of them apart.
///
/// Wire 1 is the contract as of bd-8lz.5.3: the `q list --json` envelope
/// (`{"quests":…,"machines":…}`) and a proxied command line carrying
/// `--expect <id>.<created_at>`, `--confirmed` and `--no-remote`.
///
/// **This number is a diagnostic signal, not a gate.** Nothing in `q` refuses
/// to talk to a remote over it: [`fetch_all`], [`interpret`] and
/// [`crate::commands::proxy::dispatch`] never read it, and its only reader is
/// [`crate::doctor`], which puts it in a report line. Enforcing it would mean
/// a `q --version` round trip in front of every proxied command *and* refusing
/// remotes that demonstrably work, so it is deliberately left advisory. A bead
/// that wants a real compatibility gate has to build one — raising
/// [`MIN_REMOTE_WIRE`] on its own changes only what `q doctor` prints.
pub const WIRE: u32 = 1;

/// The oldest far-end wire `q doctor` calls compatible. Raise it and doctor
/// *warns* about a remote whose tag is below it; lower it and doctor stops.
/// It changes nothing else — not even the report's exit code: see [`WIRE`], no
/// command consults the wire before talking to a remote, so a wire verdict is
/// a prediction about a command doctor never ran and cannot be a failure
/// without contradicting the listing about the same host (bd-8lz.5.4 D2).
///
/// A far end that reports **no** tag is outside this comparison entirely, and
/// no value here can bring it in. No tag means "older than wire tagging",
/// which covers every `q` up to and including bd-8lz.5.3 — a range holding
/// both binaries this one drives perfectly and binaries that reject
/// `--expect`. `q doctor` warns about those rather than judging them.
pub const MIN_REMOTE_WIRE: u32 = 1;

/// A floor above this build's own wire would make `q` refuse a remote running
/// the very same binary. Checked at compile time so a bead that bumps one
/// constant and forgets the other cannot ship.
const _: () = assert!(MIN_REMOTE_WIRE <= WIRE);

/// What `q --version` prints — `0.1.0 (wire 1)`. The crate version for a
/// human, the wire tag for the other end of an ssh (SPEC §19: *every remote
/// reachable and version compatible*).
///
/// The wire tag is spelled out because `concat!` takes only literals; the
/// `the_version_banner_carries_this_builds_wire` test keeps it and [`WIRE`]
/// from drifting apart.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (wire 1)");

/// SPEC §19's reachability probe. `q` rather than an absolute path for the
/// same reason [`LIST_ARGV`] uses one: it is whatever that machine's login
/// shell finds.
pub const VERSION_ARGV: [&str; 2] = ["q", "--version"];

/// The budget for one `ssh <alias> q --version`. The listing's deadline: a
/// remote that cannot print its version in five seconds is not a remote this
/// `q` can list from either.
pub const PROBE_TIMEOUT: Duration = TIMEOUT;

/// The budget for one `ssh -G <alias>`. Much shorter than [`PROBE_TIMEOUT`]
/// because `-G` never opens a connection — it only resolves `~/.ssh/config` —
/// so anything slow there is a DNS lookup, not a host being down.
pub const OPTIONS_TIMEOUT: Duration = Duration::from_secs(2);

/// What a remote is always asked for. `--no-remote` is the recursion guard:
/// without it the remote would fan out to *its* remotes, us included; `--all`
/// is why the cache works — see [`list_argv`].
pub const LIST_ARGV: [&str; 5] = ["q", "list", "--json", "--no-remote", "--all"];

/// The remote's `q list` — always the **whole** listing, never this
/// invocation's `--all`/`--state`.
///
/// The filters deliberately do not travel. There is one cache row per remote
/// and it has to serve every invocation: a payload fetched under `--all` (which
/// `q enter` and the TUI poller always ask for) replayed under a plain `q list`
/// would leak finished Quests, and a payload fetched under `--state active`
/// replayed under `--all` would hide live ones. Stamping the cache with its
/// flags only turns that into a cache that is almost never usable.
///
/// So the wire request is unconditional and the filtering happens on arrival,
/// through the very same predicate local rows go through
/// ([`crate::commands::listed`]) — which is also the only way fresh and stale
/// remote rows can be filtered identically, and the only way remote rows can be
/// filtered identically to local ones (bd-8lz.5.1's standing constraint).
pub fn list_argv() -> Vec<String> {
    LIST_ARGV.iter().map(|a| (*a).to_string()).collect()
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
/// The key on a `machines` entry holding that machine's `[tmux]
/// session_prefix` — the only place the far end's tmux session names are
/// knowable from here (SPEC §15).
pub const TMUX_PREFIX: &str = "tmux_prefix";
/// SPEC §15's tmux session prefix, and the fallback for a remote whose `q` is
/// too old to report its own.
pub const DEFAULT_TMUX_PREFIX: &str = "q-";

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
    /// The far end sent more than [`MAX_OUTPUT`] on stdout and the read end was
    /// dropped. Its own outcome, because what ssh then reports is a broken
    /// pipe — `exited 255`, which says nothing about what happened.
    TooLarge,
    /// ssh could not be started at all.
    Failed(String),
}

/// `Send + Sync` because the fan-out shares one client across threads — and
/// because the TUI's [`Poller`] holds the same client from its own thread.
pub trait Ssh: Send + Sync {
    /// `ssh <alias> <argv…>`, given up on after `timeout`.
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome;

    /// `ssh -G <alias>`: the options ssh itself would use for that alias, with
    /// every `Host` pattern, `Match` block, `Include` and per-host override
    /// already applied. Asked rather than parsed out of `~/.ssh/config`,
    /// because only ssh knows which of those actually won (SPEC §23 #6).
    ///
    /// It opens no connection, so it answers for a host that is down.
    fn options(&self, alias: &str, timeout: Duration) -> SshOutcome;

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
/// well inside the round rather than burning the whole budget — and capped at
/// [`CONNECT_MAX`], because a proxied command's deadline ([`PROXY_TIMEOUT`]) is
/// generous about how long the far end may *work* and says nothing about how
/// long a dead host may take to answer the phone.
fn ssh_argv(alias: &str, argv: &[&str], timeout: Duration) -> Vec<String> {
    let mut out = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!(
            "ConnectTimeout={}",
            (timeout.as_secs() / 2).clamp(1, CONNECT_MAX.as_secs())
        ),
        alias.to_string(),
    ];
    out.extend(remote_command(argv.iter().map(|a| (*a).to_string())));
    out
}

/// The argv of an interactive `ssh`. No `BatchMode` and no `ConnectTimeout`,
/// unlike [`ssh_argv`]: there *is* someone at the keyboard here, a passphrase
/// prompt is the point rather than a hang, and an attach has no deadline.
/// `-t` forces a tty, without which the far end's `tmux attach` refuses.
pub fn attach_argv(alias: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec!["-t".to_string(), alias.to_string()];
    out.extend(remote_command(argv.iter().cloned()));
    out
}

/// The remote command as the far end's shell must receive it — every word
/// quoted at this one boundary rather than by each caller.
///
/// ssh does not take an argv: it joins everything after the alias with spaces
/// and hands the string to the remote user's **login shell**. So quoting is a
/// property of *sending* a command, not of building one, and it is applied
/// here so no caller can forget it. Only the words after the alias go through
/// this — ssh's own options (`-t`, `-o …`, the alias) are exec'd by us.
fn remote_command(argv: impl Iterator<Item = String>) -> Vec<String> {
    argv.map(|a| sh_quote(&a)).collect()
}

/// One shell word, safe in **any** remote login shell.
///
/// Conservative on purpose: only strictly alphanumerics and `-_./` pass
/// through unquoted, and everything else is single-quoted. The plain set is
/// not "what bash leaves alone" — the far end's shell is whatever that user
/// runs, and zsh (the macOS default) expands things bash does not. `=q-alpha`,
/// the tmux exact-match target of SPEC §15, is the case that proves it: zsh's
/// *equals expansion* rewrites a word starting with `=` to the path of that
/// command and aborts the line when there is none, so an unquoted target never
/// reaches tmux at all. `~` is the same story. A word this errs on costs two
/// quote characters; a word it lets through costs the command.
pub fn sh_quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c));
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
///
/// It bounds a proxied command's output too (SPEC §15), which is why the
/// refusal names it — `q peek --lines 20000` against a very long pane is the
/// one everyday command that can reach it.
pub const MAX_OUTPUT: u64 = 1 << 20;

impl Ssh for RealSsh {
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome {
        let mut cmd = Command::new("ssh");
        cmd.args(ssh_argv(alias, argv, timeout));
        run_with_deadline(cmd, timeout)
    }

    fn options(&self, alias: &str, timeout: Duration) -> SshOutcome {
        let mut cmd = Command::new("ssh");
        cmd.args(["-G", alias]);
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
            // A capped stdout is truncated JSON at best, and the exit status
            // that comes with it is ssh's broken-pipe 255. Only stdout is
            // checked: a remote that floods *stderr* has still sent a listing.
            if capped(&out) {
                return SshOutcome::TooLarge;
            }
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
                    Ok(n) => lock(into.as_ref()).extend_from_slice(&chunk[..n]),
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
    String::from_utf8_lossy(&lock(buf.as_ref())).into_owned()
}

/// Whether the drain hit its cap: `Read::take` stops at exactly [`MAX_OUTPUT`]
/// bytes, so a buffer that long is one the far end was still writing into.
fn capped(buf: &Buffer) -> bool {
    lock(buf.as_ref()).len() as u64 >= MAX_OUTPUT
}

/// Poison is not news behind any of this module's locks: a drain only ever
/// appends (so the bytes that did arrive are still exactly the bytes that
/// arrived), and the poller's slot holds one round the UI can do without. A
/// panicking helper thread must never also freeze the caller.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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
    /// The answer to a call that is **not** the listing fan-out — a command
    /// proxied to this host (SPEC §15, bd-8lz.5.3).
    ///
    /// One host plays two roles in a single proxied invocation: it is asked for
    /// its listing (so the target can be resolved to it at all) and then asked
    /// to run the command. Without this a test could only script one of them,
    /// and a proxied command that must fail would have to fail the listing
    /// first — at which point the Quest is never found and the proxy never
    /// happens. Absent, both roles get the same canned answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxied: Option<Box<SshHost>>,
    /// The answer to `ssh <alias> q --version` — SPEC §19's reachability probe
    /// (bd-8lz.5.4). A third role for the same host, for the same reason
    /// [`SshHost::proxied`] is a second one: `q doctor` asks for the version
    /// and `q list` asks for the listing, and a test needs to script both.
    /// Absent, the probe gets the base answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<Box<SshHost>>,
    /// The answer to `ssh -G <alias>`. Absent, the alias resolves to ssh's
    /// defaults — `controlmaster false`, no multiplexing — which is what a
    /// `~/.ssh/config` with no `ControlMaster` in it produces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Box<SshHost>>,
}

impl Ssh for FixtureSsh {
    fn run(&self, alias: &str, argv: &[&str], timeout: Duration) -> SshOutcome {
        log_run(alias, argv);
        let mut script = script();
        let Some(host) = script.hosts.remove(alias) else {
            return SshOutcome::Failed(format!("no fixture host `{alias}`"));
        };
        // The listing fan-out, the version probe and a proxied command are
        // three different questions to the same host; see [`SshHost::proxied`].
        let host = match (host.version, host.proxied) {
            (Some(version), _) if argv == VERSION_ARGV => *version,
            (_, Some(proxied)) if argv != LIST_ARGV => *proxied,
            _ => SshHost {
                version: None,
                proxied: None,
                ..host
            },
        };
        answer(host, timeout)
    }

    fn options(&self, alias: &str, timeout: Duration) -> SshOutcome {
        log("options", &[alias]);
        let host = script()
            .hosts
            .remove(alias)
            .and_then(|h| h.options)
            .map(|o| *o)
            // ssh answers `-G` for an alias it has never heard of too: an
            // unknown host is not an error, it is just the defaults.
            .unwrap_or_else(|| SshHost {
                stdout: DEFAULT_SSH_OPTIONS.to_string(),
                ..SshHost::default()
            });
        answer(host, timeout)
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

/// What `ssh -G` prints for an alias with nothing configured: multiplexing
/// off. macOS's OpenSSH says `false` rather than `no`, and omits
/// `controlpath` entirely when it is unset — both are what
/// [`parse_multiplexing`] has to cope with, so the fixture's default says it
/// the same way.
const DEFAULT_SSH_OPTIONS: &str = "controlmaster false\ncontrolpersist no\n";

/// One scripted answer, honoured the way the real backend would: a delay
/// longer than the deadline times out rather than being waited out.
fn answer(host: SshHost, timeout: Duration) -> SshOutcome {
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

/// A missing or unreadable script is an empty one — every alias then fails.
fn script() -> SshScript {
    std::env::var_os("Q_FIXTURE_SSH")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// A fan-out call, logged as the far end's shell would receive it — quoting
/// included, so a test pins the *sent* command rather than the argv on its way
/// to [`remote_command`].
fn log_run(alias: &str, argv: &[&str]) {
    let sent = remote_command(argv.iter().map(|a| (*a).to_string()));
    let sent: Vec<&str> = sent.iter().map(String::as_str).collect();
    log(alias, &sent);
}

/// An attach, in the same log: `attach`, the alias, then the remote argv as
/// sent. Marked rather than shaped like a `run` line so a test can tell the
/// two apart, and so exec-vs-child — the `[ui] return_after_detach` split — is
/// on the record.
fn log_attach(kind: &str, alias: &str, argv: &[String]) {
    let mut parts = vec![alias.to_string()];
    parts.extend(remote_command(argv.iter().cloned()));
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
    /// That machine's `[tmux] session_prefix`, as it reported it. `None` when
    /// it has not answered yet or its `q` is too old to say; SPEC §15's
    /// [`DEFAULT_TMUX_PREFIX`] is the fallback.
    pub tmux_prefix: Option<String>,
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

// ------------------------------------------------------------------- probe

/// What a far end's `q --version` said about the wire it speaks.
///
/// Three states, not two: a tag that reads as a number, no tag at all, and a
/// tag that is there and is not a number. The last two are kept apart because
/// they mean opposite things — one is an old `q`, the other is a broken answer
/// — and collapsing them once made `q doctor` tell a remote claiming
/// `(wire 4294967296)` that it was too old (bd-8lz.5.4 review F5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wire {
    /// A `(wire N)` tag this `q` could read.
    Speaks(u32),
    /// No tag at all — how every `q` from before the wire was numbered
    /// identifies itself. It means "older than wire tagging" and nothing more
    /// precise: the range covers both a `q` this one drives end to end and a
    /// `q` that answers `--expect` with clap's exit 2.
    Untagged,
    /// A tag is present and is not a `u32`: negative, overflowing, or not a
    /// number. Carries it verbatim so the report can quote it.
    Unreadable(String),
}

/// What a far end's `q --version` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteVersion {
    /// The crate version, `0.1.0` — for the human reading the report.
    pub semver: String,
    /// The wire the far end claims, as far as it can be read.
    pub wire: Wire,
}

impl RemoteVersion {
    /// `0.1.0 (wire 1)` / `0.1.0`, however the far end said it.
    pub fn label(&self) -> String {
        match &self.wire {
            Wire::Speaks(wire) => format!("q {} (wire {wire})", self.semver),
            Wire::Untagged => format!("q {}", self.semver),
            Wire::Unreadable(tag) => format!("q {} (wire {tag})", self.semver),
        }
    }
}

/// `q --version`, as this `q` prints it and as every `q` before [`WIRE`]
/// printed it: `q 0.1.0 (wire 1)`, `0.1.0 (wire 1)`, or a bare `q 0.1.0`.
///
/// `None` for anything that does not start with a version number — a login
/// shell's banner, a `command not found`, an ssh notice. Deliberately strict:
/// guessing a version out of arbitrary text is how a doctor reports a
/// compatible remote that is not one.
pub fn parse_version(out: &str) -> Option<RemoteVersion> {
    let line = out.lines().find(|l| !l.trim().is_empty())?.trim();
    // clap prints `<bin> <version>`; a wrapper may print the version alone.
    let word = line
        .split_whitespace()
        .find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    // Only the *first* word may be the binary's name, so a version appearing
    // later in a sentence ("error: q 1.2.3 is broken") is not read as one.
    if !line.starts_with(word) && line.split_whitespace().nth(1) != Some(word) {
        return None;
    }
    // `q 0.1.0(wire 1)`: a tag with no space before it is still a tag, and the
    // version is what precedes it — splitting on whitespace alone once left
    // `0.1.0(wire` as the version (bd-8lz.5.4 review F6).
    let semver = word.split('(').next().unwrap_or(word);
    Some(RemoteVersion {
        semver: semver.to_string(),
        wire: parse_wire(line),
    })
}

/// The `(wire N)` tag, read into its three states — see [`Wire`].
fn parse_wire(line: &str) -> Wire {
    let Some(after) = line.split("(wire ").nth(1) else {
        return Wire::Untagged;
    };
    let tag = after.split(')').next().unwrap_or(after).trim();
    match tag.parse::<u32>() {
        Ok(wire) => Wire::Speaks(wire),
        Err(_) => Wire::Unreadable(output::first_line(tag, 32)),
    }
}

/// Whether ssh would multiplex this alias, and why not when it would not
/// (SPEC §23 #6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Multiplexing {
    /// A master and a socket to keep it on: a second command reuses the first
    /// connection.
    On { persist: String },
    /// A master, but `ControlPersist no` — the mux lives only as long as the
    /// command that opened it, so nothing short-lived ever reuses one.
    NotPersisted,
    /// No `ControlMaster`, or one with `ControlPath none`.
    Off,
    /// `ssh -G` did not answer, so nothing here is known. Reported as such:
    /// reading silence as "off" would put a fix line under a config that may
    /// already be right.
    Unknown(String),
}

/// Read `ssh -G <alias>`'s answer.
///
/// The keys come back lowercased, one per line, already resolved through every
/// `Host` pattern, `Match` block and `Include` — which is the whole reason
/// this asks ssh instead of reading `~/.ssh/config`. `controlmaster` is
/// `false`/`no` when off (OpenSSH on macOS prints `false`), and `controlpath`
/// is omitted entirely rather than printed as `none` when it was never set.
pub fn parse_multiplexing(out: &str) -> Multiplexing {
    let value = |key: &str| -> Option<String> {
        out.lines().find_map(|line| {
            let (k, v) = line.trim().split_once(char::is_whitespace)?;
            k.eq_ignore_ascii_case(key).then(|| v.trim().to_string())
        })
    };
    let master = value("controlmaster").unwrap_or_default();
    let master_on = !matches!(
        master.to_ascii_lowercase().as_str(),
        "" | "no" | "false" | "none"
    );
    let path = value("controlpath").unwrap_or_default();
    let path_on = !matches!(path.to_ascii_lowercase().as_str(), "" | "none");
    if !master_on || !path_on {
        return Multiplexing::Off;
    }
    let persist = value("controlpersist").unwrap_or_default();
    match persist.to_ascii_lowercase().as_str() {
        "" | "no" | "false" | "none" => Multiplexing::NotPersisted,
        _ => Multiplexing::On { persist },
    }
}

/// One remote as SPEC §19 asks about it: what `q --version` said over there,
/// and what ssh would do for that alias.
#[derive(Debug)]
pub struct Probe {
    pub name: String,
    pub ssh: String,
    /// The raw outcome of `ssh <alias> q --version` — a status this `q` can
    /// diagnose is built from it in [`crate::doctor`], because the diagnosis
    /// (and its fix line) is doctor's business, not the transport's.
    pub version: SshOutcome,
    pub multiplexing: Multiplexing,
}

/// Ask every remote for its version and its ssh options, all at once — the
/// same fan-out `q list` uses, so there is one ssh path and one deadline
/// story (SPEC §19). Empty when there is nothing to ask: no `[[remotes]]`,
/// `--no-remote`, or a `--machine` naming this machine.
///
/// Bounded by `PROBE_TIMEOUT + OPTIONS_TIMEOUT` however many remotes there
/// are and however dead they all are.
pub fn probe_all(ctx: &Ctx) -> Vec<Probe> {
    let targets = targets(ctx);
    if targets.is_empty() {
        return Vec::new();
    }
    let ssh = ctx.ssh();
    scatter(&targets, |remote| {
        // `-G` first: it opens no connection, so it costs nothing next to the
        // probe and still answers when the probe times out.
        let options = ssh.options(&remote.ssh, OPTIONS_TIMEOUT);
        Probe {
            name: remote.name.clone(),
            ssh: remote.ssh.clone(),
            version: ssh.run(&remote.ssh, &VERSION_ARGV, PROBE_TIMEOUT),
            multiplexing: match options {
                SshOutcome::Done {
                    code: Some(0),
                    stdout,
                    ..
                } => parse_multiplexing(&stdout),
                // `ssh -G` that did not answer says nothing about the config;
                // reading "off" out of silence would be a guess.
                other => Multiplexing::Unknown(why(&other)),
            },
        }
    })
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
    let answers = fan_out(ctx.ssh(), &targets, &list_argv(), TIMEOUT);
    let mut results = resolve(ctx.db().ok(), &targets, answers, now());
    retain_listed(&mut results, all, state);
    results
}

/// Every remote's **last cached** listing, read straight out of `remote_cache`
/// with no ssh at all — what a command consults when it needs to know whether a
/// remote could be in play before deciding to pay for a fan-out (see
/// [`crate::commands::enter`]).
///
/// Respects `--no-remote` and `--machine` exactly as [`fetch_all`] does, and
/// carries no status: a remote that has never answered, or whose cached payload
/// no longer parses, simply contributes nothing. The rows are unfiltered — the
/// far end is always asked for the whole listing — so what comes back here is
/// what a `--all` round would have returned last time.
pub fn cached_quests(ctx: &Ctx) -> Vec<RemoteQuest> {
    let targets = targets(ctx);
    if targets.is_empty() {
        return Vec::new();
    }
    let Ok(db) = ctx.db() else {
        return Vec::new();
    };
    targets
        .iter()
        .filter_map(|remote| load(Some(db), &remote.name))
        .flat_map(|(listing, _)| listing.quests)
        .collect()
}

/// Apply this invocation's `--all`/`--state` to rows a remote sent, fresh or
/// cached alike. See [`list_argv`]: the far end always sends everything, so
/// this is where SPEC §16's filters are applied to a remote's rows — by the
/// same predicate that filtered the local ones.
pub fn retain_listed(results: &mut [RemoteResult], all: bool, state: Option<StateFilter>) {
    if all && state.is_none() {
        return;
    }
    for result in results {
        result
            .quests
            .retain(|q| crate::commands::listed(&q.view, all, state));
    }
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
    /// The last finished round, waiting to be picked up. A *slot*, not a
    /// queue: the UI wants the newest answer and nothing else, and a queue
    /// nobody drains is unbounded memory — the UI thread can be away for hours
    /// inside a [`crate::tui`] handoff.
    latest: Arc<Mutex<Option<Round>>>,
    /// Set while the UI thread has given the terminal away. The worker starts
    /// no round while it is on, so an attach that lasts four hours does not
    /// cost 1440 ssh connections to machines nobody is looking at.
    paused: Arc<std::sync::atomic::AtomicBool>,
    /// Dropped by the worker on its way out, so [`Poller::alive`] can tell a
    /// stopped clock from a quiet one.
    beacon: Arc<()>,
    /// Also the shutdown signal: dropping the `Poller` closes it, and the
    /// worker exits at the end of the round it is in.
    nudges: mpsc::SyncSender<()>,
}

impl Poller {
    /// Start polling, or `None` when there is nothing to poll — no remotes,
    /// `--no-remote`, or a `--machine` that is not one of them.
    ///
    /// The round asks for the whole listing (see [`list_argv`]); the TUI's `f`
    /// toggle filters rows it already has rather than re-fetching, so a
    /// keypress never waits for a fan-out.
    pub fn spawn(ctx: &Ctx, every: Duration) -> Option<Poller> {
        let targets: Vec<Remote> = targets(ctx).into_iter().cloned().collect();
        if targets.is_empty() {
            return None;
        }
        let ssh = ctx.ssh_shared();
        let argv = list_argv();
        let latest = Arc::new(Mutex::new(None));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let beacon = Arc::new(());
        let (nudges, wake) = mpsc::sync_channel(1);
        let worker = Worker {
            latest: latest.clone(),
            paused: paused.clone(),
            beacon: beacon.clone(),
        };
        // Detached: a round holds for at most `TIMEOUT`, and joining it would
        // put that on the TUI's exit path for nothing.
        std::thread::spawn(move || poll_loop(ssh.as_ref(), &targets, &argv, every, &worker, &wake));
        Some(Poller {
            latest,
            paused,
            beacon,
            nudges,
        })
    }

    /// The newest round that has finished since the last look, if any. Never
    /// blocks; older rounds were overwritten rather than queued.
    pub fn take(&self) -> Option<Round> {
        lock(&self.latest).take()
    }

    /// Ask for a round now (the TUI's `x`). Coalesced: a nudge that arrives
    /// while one is queued or running is dropped, so holding `x` down cannot
    /// queue a fan-out per keypress.
    pub fn nudge(&self) {
        let _ = self.nudges.try_send(());
    }

    /// Stop starting rounds. The one already in flight, if any, finishes.
    pub fn pause(&self) {
        self.paused.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Start again, and ask for a round straight away: whatever the UI was
    /// away doing, the listing it comes back to is older than it looks.
    pub fn resume(&self) {
        self.paused
            .store(false, std::sync::atomic::Ordering::SeqCst);
        self.nudge();
    }

    /// Whether the worker is still there. False once it has returned or
    /// panicked — the remote clock has stopped, and a TUI that kept showing
    /// the last chip would say nothing about it.
    pub fn alive(&self) -> bool {
        Arc::strong_count(&self.beacon) > 1
    }
}

/// The poller's half of the shared state.
struct Worker {
    latest: Arc<Mutex<Option<Round>>>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    /// Held for exactly as long as the worker runs, and read only through the
    /// `Arc`'s strong count; see [`Poller::alive`].
    #[allow(dead_code)]
    beacon: Arc<()>,
}

fn poll_loop(
    ssh: &dyn Ssh,
    targets: &[Remote],
    argv: &[String],
    every: Duration,
    worker: &Worker,
    wake: &mpsc::Receiver<()>,
) {
    loop {
        if !worker.paused.load(std::sync::atomic::Ordering::SeqCst) {
            let borrowed: Vec<&Remote> = targets.iter().collect();
            let answers = fan_out(ssh, &borrowed, argv, TIMEOUT);
            // Overwrites whatever the UI has not picked up: one round of
            // memory, and always the newest one.
            *lock(&worker.latest) = Some(Round {
                targets: targets.to_vec(),
                answers,
            });
        }
        match wake.recv_timeout(every) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The `Poller` is gone: the TUI has exited and this thread with it.
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(test)]
impl Round {
    /// A round as the poller would have delivered it, from what each remote
    /// would have printed. Built through [`interpret`], so a test drives the
    /// real parse — and, through [`resolve_round`], the real cache write.
    pub(crate) fn for_tests(answers: Vec<(Remote, SshOutcome)>) -> Round {
        let (targets, outcomes): (Vec<Remote>, Vec<SshOutcome>) = answers.into_iter().unzip();
        Round {
            answers: outcomes
                .into_iter()
                .map(|o| interpret(o, TIMEOUT))
                .collect(),
            targets,
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

/// One line on why an ssh call produced no usable answer.
fn why(outcome: &SshOutcome) -> String {
    match outcome {
        SshOutcome::TimedOut => "no answer in time".to_string(),
        SshOutcome::TooLarge => "the answer was too large".to_string(),
        SshOutcome::Failed(e) => e.clone(),
        SshOutcome::Done { code, stderr, .. } => exit_reason(*code, stderr),
    }
}

/// One thread per remote, all joined before this returns; the answers come back
/// in `targets` order. Every fan-out in `q` goes through here, so "parallel,
/// bounded, and a panicking remote does not take the command down" is decided
/// once — `f` must therefore be total.
fn scatter<T>(targets: &[&Remote], f: impl Fn(&Remote) -> T + Sync) -> Vec<T>
where
    T: Send + for<'a> From<PanickedRemote<'a>>,
{
    std::thread::scope(|scope| {
        let f = &f;
        let handles: Vec<_> = targets
            .iter()
            .map(|remote| scope.spawn(move || f(remote)))
            .collect();
        handles
            .into_iter()
            .zip(targets)
            .map(|(h, remote)| h.join().unwrap_or_else(|_| T::from(PanickedRemote(remote))))
            .collect()
    })
}

/// What a [`scatter`] worker that panicked leaves behind — a remote and no
/// answer. Each fan-out says for itself what that means.
struct PanickedRemote<'a>(&'a Remote);

impl From<PanickedRemote<'_>> for Result<Answer, RemoteStatus> {
    fn from(_: PanickedRemote<'_>) -> Result<Answer, RemoteStatus> {
        Err(RemoteStatus::unreachable("the ssh call panicked"))
    }
}

impl From<PanickedRemote<'_>> for Probe {
    fn from(remote: PanickedRemote<'_>) -> Probe {
        Probe {
            name: remote.0.name.clone(),
            ssh: remote.0.ssh.clone(),
            version: SshOutcome::Failed("the ssh call panicked".to_string()),
            multiplexing: Multiplexing::Unknown("the ssh call panicked".to_string()),
        }
    }
}

fn fan_out(
    ssh: &dyn Ssh,
    targets: &[&Remote],
    argv: &[String],
    timeout: Duration,
) -> Vec<Result<Answer, RemoteStatus>> {
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    let argv = argv.as_slice();
    scatter(targets, |remote| {
        interpret(ssh.run(&remote.ssh, argv, timeout), timeout)
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
    listing: Listing,
    raw: String,
}

/// A `q list --json` document, read.
#[derive(Debug, Default)]
pub struct Listing {
    pub quests: Vec<RemoteQuest>,
    /// The sender's `[tmux] session_prefix`, out of its own `machines` entry.
    /// `None` from a bare array or a `q` too old to report it.
    pub tmux_prefix: Option<String>,
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
        SshOutcome::TooLarge => Err(RemoteStatus::incompatible(format!(
            "`q list --json` was larger than {} MiB",
            MAX_OUTPUT >> 20
        ))),
        SshOutcome::Failed(e) => Err(RemoteStatus::unreachable(e)),
        SshOutcome::Done {
            code,
            stdout,
            stderr,
        } => {
            if code != Some(0) {
                return Err(nonzero(code, &stderr));
            }
            let listing = parse(&stdout).map_err(RemoteStatus::incompatible)?;
            Ok(Answer {
                listing,
                raw: stdout.trim().to_string(),
            })
        }
    }
}

/// A remote command that ran and failed, told apart from a connection that
/// never happened (bd-8lz.5.4).
///
/// This is where [`RemoteStatus::Incompatible`] finally diverges from
/// [`RemoteStatus::Unreachable`]. ssh reports its *own* failures as 255 and a
/// signalled command as no code at all; anything else is the far end's answer,
/// which means the host is up and it is that machine's `q` that is the
/// problem — absent (127, the shell could not find it) or too old to
/// understand the line it was sent (clap exits 2 on an unknown argument, which
/// is exactly what a pre-`--no-remote` `q` does with the listing request).
///
/// It costs no extra ssh: the exit code was already in hand.
fn nonzero(code: Option<i32>, stderr: &str) -> RemoteStatus {
    match code {
        Some(NO_COMMAND) => RemoteStatus::incompatible(format!(
            "reachable, but no `q` on PATH there ({})",
            exit_reason(code, stderr)
        )),
        Some(SSH_FAILED) | None => RemoteStatus::unreachable(exit_reason(code, stderr)),
        Some(_) => RemoteStatus::incompatible(exit_reason(code, stderr)),
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
pub fn parse(stdout: &str) -> Result<Listing, String> {
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
    let quests: Vec<RemoteQuest> = rows
        .iter()
        .map(|raw| {
            serde_json::from_value(raw.clone())
                .map(|view| RemoteQuest {
                    view,
                    raw: raw.clone(),
                })
                .map_err(|e| format!("cannot read a quest in `q list --json`: {e}"))
        })
        .collect::<Result<_, _>>()?;
    Ok(Listing {
        tmux_prefix: local_tmux_prefix(&document),
        quests,
    })
}

/// The sender's own tmux prefix: the `machines` entry it filed itself under.
/// A remote is asked with `--no-remote`, so its roster is exactly one entry —
/// but it is matched on `kind` rather than on position, because a future `q`
/// may put more there.
fn local_tmux_prefix(document: &serde_json::Value) -> Option<String> {
    document
        .get(MACHINES)?
        .as_array()?
        .iter()
        .find(|m| m.get("kind").and_then(|k| k.as_str()) == Some("local"))?
        .get(TMUX_PREFIX)?
        .as_str()
        .filter(|p| !p.is_empty())
        .map(str::to_string)
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
                attribute(&mut answer.listing.quests, &remote.name);
                RemoteResult {
                    name: remote.name.clone(),
                    ssh: remote.ssh.clone(),
                    status: RemoteStatus::Ok,
                    quests: answer.listing.quests,
                    stale: false,
                    fetched_at: Some(ts),
                    tmux_prefix: answer.listing.tmux_prefix,
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
                    tmux_prefix: cached.as_ref().and_then(|(l, _)| l.tmux_prefix.clone()),
                    quests: cached.map(|(l, _)| l.quests).unwrap_or_default(),
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

fn load(db: Option<&Db>, name: &str) -> Option<(Listing, i64)> {
    let cached = db?.get_remote_cache(name).ok().flatten()?;
    let mut listing = parse(&cached.payload).ok()?;
    attribute(&mut listing.quests, name);
    Some((listing, cached.fetched_at))
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

        fn options(&self, _: &str, _: Duration) -> SshOutcome {
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
        /// What `ssh -G <alias>` answers; the ssh defaults when unscripted.
        options: BTreeMap<String, SshOutcome>,
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
                options: BTreeMap::new(),
                delay: Duration::ZERO,
                state: Mutex::new(StubState::default()),
            }
        }

        /// Script `ssh -G` for an alias.
        #[allow(dead_code)]
        pub(crate) fn with_options(mut self, options: &[(&str, SshOutcome)]) -> StubSsh {
            self.options = options
                .iter()
                .map(|(a, o)| ((*a).to_string(), o.clone()))
                .collect();
            self
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

        fn options(&self, alias: &str, _: Duration) -> SshOutcome {
            self.state
                .lock()
                .unwrap()
                .calls
                .push((alias.to_string(), vec!["-G".to_string()]));
            self.options
                .get(alias)
                .cloned()
                .unwrap_or_else(|| SshOutcome::Done {
                    code: Some(0),
                    stdout: super::DEFAULT_SSH_OPTIONS.to_string(),
                    stderr: String::new(),
                })
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
            listing: parse(raw).unwrap(),
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
                "--no-remote",
                "--all"
            ]
        );
        // However short the deadline, ssh is never told to wait zero seconds.
        assert!(
            ssh_argv("ws", &LIST_ARGV, Duration::from_millis(1))
                .contains(&"ConnectTimeout=1".to_string())
        );
    }

    /// The filters do NOT travel — see [`list_argv`]. One cache row per remote
    /// has to serve every invocation, so the wire request is always the whole
    /// listing and SPEC §16's filters are applied on arrival.
    #[test]
    fn the_remote_is_always_asked_for_the_whole_listing() {
        assert_eq!(list_argv(), LIST_ARGV);
        assert!(list_argv().contains(&"--all".to_string()));
        assert!(!list_argv().iter().any(|a| a == "--state"));
    }

    /// …and the filters are applied to what comes back, by the same predicate
    /// that filtered the local rows.
    #[test]
    fn a_remotes_rows_are_filtered_on_arrival_fresh_or_cached() {
        let mixed = {
            let mut live = Quest::new("live-there", "/tmp", "ws");
            live.state = crate::model::QuestState::Active;
            let mut done = Quest::new("done-there", "/tmp", "ws");
            done.state = crate::model::QuestState::Finished;
            let views = [QuestView::new(live, &[]), QuestView::new(done, &[])];
            serde_json::to_string(&views).unwrap()
        };
        let slugs = |results: &[RemoteResult]| -> Vec<String> {
            results[0]
                .quests
                .iter()
                .map(|q| q.view.quest.slug.clone())
                .collect()
        };
        let remotes = [remote("ws")];
        let ssh = || Arc::new(StubSsh::new(&[("ws-host", ok(mixed.clone()))])) as Arc<dyn Ssh>;

        // Fresh.
        assert_eq!(
            slugs(&fetch_all(&ctx_with(&remotes, ssh()), false, None)),
            ["live-there"],
            "a finished remote Quest is not in the default listing"
        );
        assert_eq!(
            slugs(&fetch_all(&ctx_with(&remotes, ssh()), true, None)).len(),
            2
        );
        assert_eq!(
            slugs(&fetch_all(
                &ctx_with(&remotes, ssh()),
                false,
                Some(StateFilter::Finished)
            )),
            ["done-there"]
        );

        // …and cached, which is the case the cache row cannot answer on its
        // own: one `--all` round fills it, then the machine goes down and a
        // plain `q list` replays it. Same database, dead ssh.
        let ctx = ctx_with(&remotes, ssh());
        assert_eq!(slugs(&fetch_all(&ctx, true, None)).len(), 2);
        let ctx = ctx.with_ssh(Arc::new(stub::NoSsh));
        let cached = fetch_all(&ctx, false, None);
        assert!(cached[0].stale, "the cache was not used");
        assert_eq!(
            slugs(&cached),
            ["live-there"],
            "a cached row is filtered by THIS invocation's flags"
        );
        assert_eq!(slugs(&fetch_all(&ctx, true, None)).len(), 2);
        // No live session over there, so the unfinished one reads as idle —
        // exactly as the far end's own `q list --state idle` would report it.
        assert_eq!(
            slugs(&fetch_all(&ctx, false, Some(StateFilter::Idle))),
            ["live-there"]
        );
        assert!(slugs(&fetch_all(&ctx, false, Some(StateFilter::Active))).is_empty());
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

        let answers = fan_out(&ssh, &targets, &list_argv(), TIMEOUT);

        assert_eq!(answers.len(), 3);
        assert!(answers.iter().all(|a| a.is_ok()));
        // Overlap, not the wall clock: a loaded CI runner may take as long as
        // it likes, but three calls cannot be in flight at once unless they
        // really did run in parallel.
        assert_eq!(ssh.peak(), 3, "the calls did not overlap");
        // The answers keep config order whatever order they arrived in.
        let slugs: Vec<&str> = answers
            .iter()
            .map(|a| {
                a.as_ref().unwrap().listing.quests[0]
                    .view
                    .quest
                    .slug
                    .as_str()
            })
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
        let failed = |code, stderr: &str| {
            interpret(
                SshOutcome::Done {
                    code,
                    stdout: String::new(),
                    stderr: stderr.to_string(),
                },
                TIMEOUT,
            )
            .unwrap_err()
        };

        // The host answered — the shell got as far as looking for `q` — so it
        // is that machine's `q` that is the problem, not the machine
        // (bd-8lz.5.4).
        let status = failed(Some(NO_COMMAND), "bash: q: command not found\n");
        assert_eq!(status.marker(), Some(INCOMPATIBLE));
        assert!(status.reason().unwrap().contains("command not found"));
        assert!(status.reason().unwrap().contains("no `q` on PATH"));

        // Any other exit is the far end's `q` refusing the line it was sent —
        // clap answers an unknown argument with 2, which is what a `q` from
        // before `--no-remote` does with the listing request.
        let status = failed(Some(2), "error: unexpected argument '--no-remote'\n");
        assert_eq!(status.marker(), Some(INCOMPATIBLE));

        // ssh's own failure code, and a signalled command: nothing got there.
        assert_eq!(failed(Some(SSH_FAILED), "").marker(), Some(UNREACHABLE));
        assert_eq!(failed(None, "").marker(), Some(UNREACHABLE));
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
            let listing = parse(text).unwrap_or_else(|e| panic!("{text} → {e}"));
            assert!(listing.quests.len() <= 1);
            // Nothing said its prefix, so nothing is assumed.
            assert_eq!(listing.tmux_prefix, None);
        }
        assert_eq!(parse(&enveloped).unwrap().quests[0].view.quest.slug, "one");

        // The sender's own `machines` entry is where its tmux prefix comes
        // from — the only thing on the wire that can name a remote session.
        let with_prefix = format!(
            "{{\"quests\": {array}, \"machines\": [{{\"name\": \"ws\", \"kind\": \"local\", \"tmux_prefix\": \"work_\"}}]}}"
        );
        assert_eq!(
            parse(&with_prefix).unwrap().tmux_prefix,
            Some("work_".to_string())
        );
    }

    #[test]
    fn a_newer_remote_with_extra_fields_still_parses() {
        let mut value: serde_json::Value = serde_json::from_str(&payload("one")).unwrap();
        value[0]["something_from_the_future"] = serde_json::json!("hello");
        // And a field this q knows but an older one never sent.
        value[0].as_object_mut().unwrap().remove("progress");
        let listing = parse(&value.to_string()).unwrap();
        assert_eq!(listing.quests[0].view.quest.slug, "one");
        assert_eq!(listing.quests[0].view.progress, None);
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

    /// A tag that is there and is not a number is not an old `q` — reading it
    /// as one told a far end claiming `(wire 4294967296)` to upgrade
    /// (bd-8lz.5.4 review F5).
    #[test]
    fn a_wire_tag_that_is_not_a_number_is_kept_apart_from_no_tag_at_all() {
        let wire = |out: &str| parse_version(out).unwrap().wire;

        assert_eq!(
            wire("q 0.1.0 (wire 4294967296)"),
            Wire::Unreadable("4294967296".to_string())
        );
        assert_eq!(
            wire("q 0.1.0 (wire 99999999999999999999)"),
            Wire::Unreadable("99999999999999999999".to_string())
        );
        assert_eq!(
            wire("q 0.1.0 (wire -1)"),
            Wire::Unreadable("-1".to_string())
        );
        assert_eq!(wire("q 0.1.0 (wire x)"), Wire::Unreadable("x".to_string()));
        // …and none of them is `Untagged`, which is the whole point.
        assert_ne!(wire("q 0.1.0 (wire -1)"), Wire::Untagged);
        assert_eq!(wire("q 0.1.0"), Wire::Untagged);
        // The report quotes it back verbatim rather than dropping it.
        assert_eq!(
            parse_version("q 0.1.0 (wire -1)").unwrap().label(),
            "q 0.1.0 (wire -1)"
        );
    }

    /// [`WIRE`] and the tag `q --version` prints are two spellings of one
    /// number, and the far end reads the printed one.
    #[test]
    fn the_version_banner_carries_this_builds_wire() {
        assert!(
            VERSION.ends_with(&format!("(wire {WIRE})")),
            "`{VERSION}` does not carry wire {WIRE}"
        );
        assert!(VERSION.starts_with(env!("CARGO_PKG_VERSION")));
        // …and this `q` reads its own banner back, so the probe and the
        // printer can never drift apart.
        let mine = parse_version(&format!("q {VERSION}")).unwrap();
        assert_eq!(mine.wire, Wire::Speaks(WIRE));
        assert_eq!(mine.semver, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn a_remotes_version_is_read_from_however_it_says_it() {
        let wire = |out: &str| parse_version(out).map(|v| (v.semver, v.wire));

        // clap's own line, and a wrapper printing the version alone.
        assert_eq!(
            wire("q 0.4.0 (wire 3)\n"),
            Some(("0.4.0".to_string(), Wire::Speaks(3)))
        );
        assert_eq!(
            wire("0.4.0 (wire 3)"),
            Some(("0.4.0".to_string(), Wire::Speaks(3)))
        );
        // Every `q` from before the wire was numbered.
        assert_eq!(wire("q 0.1.0"), Some(("0.1.0".to_string(), Wire::Untagged)));
        // A tag with no space in front of it is still a tag, and the version
        // in front of it is still a version (review F6).
        assert_eq!(
            wire("q 0.1.0(wire 1)"),
            Some(("0.1.0".to_string(), Wire::Speaks(1)))
        );
        assert_eq!(
            parse_version("q 0.1.0(wire 1)").unwrap().label(),
            "q 0.1.0 (wire 1)"
        );

        // Nothing that is not a version is read as one.
        assert_eq!(wire(""), None);
        assert_eq!(wire("zsh: command not found: q"), None);
        assert_eq!(wire("Welcome to ws! Have a nice day."), None);
        assert_eq!(wire("error: q 1.2.3 is broken"), None);

        // A wire tag that is not a number is read as unreadable — never a
        // panic, and never confused with a `q` that printed no tag at all.
        assert_eq!(
            wire("q 0.1.0 (wire next)"),
            Some(("0.1.0".into(), Wire::Unreadable("next".to_string())))
        );

        // A banner ahead of the version is a banner, not a version: the far
        // end's rc files printing at us must not be read as a `q`.
        assert_eq!(wire("motd\nq 0.4.0 (wire 3)"), None);
    }

    /// `ssh -G` is the honest way to ask what ssh will do — every `Host`
    /// pattern, `Match` block and `Include` already resolved (SPEC §23 #6).
    #[test]
    fn multiplexing_is_read_out_of_what_ssh_itself_reports() {
        let on = "controlmaster auto\ncontrolpath /tmp/cm-%r@%h:%p\ncontrolpersist 600\n";
        assert_eq!(
            parse_multiplexing(on),
            Multiplexing::On {
                persist: "600".to_string()
            }
        );

        // OpenSSH on macOS says `false`, not `no`; and `controlpath` is simply
        // absent when it was never set.
        assert_eq!(
            parse_multiplexing("controlmaster false\ncontrolpersist no\n"),
            Multiplexing::Off
        );
        assert_eq!(parse_multiplexing(""), Multiplexing::Off);

        // A master with nowhere to put its socket multiplexes nothing.
        assert_eq!(
            parse_multiplexing("controlmaster auto\ncontrolpath none\n"),
            Multiplexing::Off
        );

        // Configured, and useless for short commands: the mux dies with the
        // command that opened it.
        assert_eq!(
            parse_multiplexing("controlmaster auto\ncontrolpath /tmp/cm\ncontrolpersist no\n"),
            Multiplexing::NotPersisted
        );

        // Other keys are ignored, and the whole line is what is read — a key
        // that merely contains "controlmaster" is not one.
        assert_eq!(
            parse_multiplexing("hostname box\nproxycommand controlmaster auto\n"),
            Multiplexing::Off
        );
    }

    /// The probes fan out exactly as the listing does; a remote that hangs
    /// costs one round, not one round per remote.
    #[test]
    fn every_remote_is_probed_at_once() {
        let remotes = [remote("ws"), remote("box"), remote("nas")];
        let answers: Vec<(&str, SshOutcome)> = remotes
            .iter()
            .map(|r| (r.ssh.as_str(), ok("q 0.1.0 (wire 1)\n".to_string())))
            .collect();
        let ssh = Arc::new(StubSsh::new(&answers).with_delay(Duration::from_millis(60)));
        let ctx = ctx_with(&remotes, ssh.clone() as Arc<dyn Ssh>);

        let probes = probe_all(&ctx);
        assert_eq!(
            probes.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["ws", "box", "nas"]
        );
        assert_eq!(ssh.peak(), 3, "the probes did not overlap");
        // SPEC §19's probe, and nothing else.
        assert!(
            ssh.calls()
                .iter()
                .filter(|(_, argv)| argv != &vec!["-G".to_string()])
                .all(|(_, argv)| argv == &VERSION_ARGV),
            "{:?}",
            ssh.calls()
        );
        // Unscripted `ssh -G` is the ssh defaults: no multiplexing.
        assert!(
            probes.iter().all(|p| p.multiplexing == Multiplexing::Off),
            "{probes:?}"
        );
    }

    #[test]
    fn nothing_is_probed_without_remotes_or_under_no_remote() {
        assert!(probe_all(&ctx_with(&[], Arc::new(stub::NoSsh))).is_empty());
        let ctx = ctx_with(&[remote("ws")], Arc::new(stub::NoSsh)).with_no_remote(true);
        assert!(probe_all(&ctx).is_empty());
    }

    /// A `ssh -G` that did not answer is not evidence of a missing
    /// `ControlMaster`.
    #[test]
    fn an_unanswerable_ssh_g_leaves_multiplexing_unknown() {
        let remotes = [remote("ws")];
        let ssh = Arc::new(
            StubSsh::new(&[("ws-host", ok("q 0.1.0 (wire 1)".to_string()))]).with_options(&[(
                "ws-host",
                SshOutcome::Failed("ssh not found on PATH".to_string()),
            )]),
        );
        let probes = probe_all(&ctx_with(&remotes, ssh));
        assert!(
            matches!(probes[0].multiplexing, Multiplexing::Unknown(_)),
            "{probes:?}"
        );
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

    /// The cap holds — and what comes back says so. Truncated JSON cannot be
    /// read, and the exit status that arrives with it is ssh's broken-pipe
    /// 255, which on its own reads as "the host is down".
    #[test]
    fn a_remote_that_floods_the_pipe_is_capped_and_says_why() {
        let flood = MAX_OUTPUT * 4;
        let outcome = run_with_deadline(sh(&format!("yes | head -c {flood}")), TIMEOUT);
        assert_eq!(outcome, SshOutcome::TooLarge);

        let status = interpret(outcome, TIMEOUT).unwrap_err();
        assert_eq!(status.marker(), Some(INCOMPATIBLE));
        let reason = status.reason().unwrap();
        assert!(reason.contains("larger than 1 MiB"), "{reason}");
    }

    /// A remote that only floods *stderr* has still sent a listing.
    #[test]
    fn a_flood_on_stderr_does_not_discard_a_good_answer() {
        let flood = MAX_OUTPUT * 2;
        let outcome = run_with_deadline(
            sh(&format!("yes >&2 | head -c {flood}; printf '[]'")),
            TIMEOUT,
        );
        match outcome {
            SshOutcome::Done { stdout, .. } => assert_eq!(stdout, "[]"),
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

    /// The far end's login shell sees these words before any program does,
    /// and it is not necessarily bash.
    #[test]
    fn an_argument_that_is_more_than_one_shell_word_is_quoted() {
        // SPEC §15's tmux target. zsh — the macOS default, and what `ws` runs
        // — applies *equals expansion* to a leading `=`: unquoted, the line
        // dies with `zsh:1: q-alpha not found` and tmux never runs at all.
        assert_eq!(sh_quote("=q-alpha"), "'=q-alpha'");
        // …and `~` is the same story, in every shell.
        assert_eq!(sh_quote("~/x"), "'~/x'");
        // A plain word stays plain: alphanumerics and `-_./`, nothing else.
        assert_eq!(sh_quote("tmux"), "tmux");
        assert_eq!(sh_quote("--no-remote"), "--no-remote");
        assert_eq!(sh_quote("q-alpha"), "q-alpha");
        assert_eq!(sh_quote("/usr/local/bin/q"), "/usr/local/bin/q");
        assert_eq!(sh_quote("=q alpha"), "'=q alpha'");
        assert_eq!(sh_quote("a'b"), r"'a'\''b'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("; rm -rf /"), "'; rm -rf /'");
        // Everything the old plain set let through, now quoted.
        for arg in ["=x", "a=b", "~x", "a%b", "a,b", "a+b", "a:b", "a@b"] {
            assert_eq!(sh_quote(arg), format!("'{arg}'"), "{arg}");
        }
    }

    /// Quoting happens once, at the ssh boundary, so no caller can forget it —
    /// and the fan-out's own flags are unaffected by it.
    #[test]
    fn the_remote_command_is_quoted_on_its_way_into_ssh() {
        assert_eq!(
            attach_argv("ws-host", &["tmux".to_string(), "=q-alpha".to_string()]),
            ["-t", "ws-host", "tmux", "'=q-alpha'"]
        );
        // ssh's own options are ours to exec, never the far end's to read.
        let argv = ssh_argv("ws", &LIST_ARGV, TIMEOUT);
        assert_eq!(argv[1], "BatchMode=yes");
        assert_eq!(&argv[5..], LIST_ARGV);
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

    /// SPEC §17's handoff: the UI thread can be inside a tmux attach for
    /// hours, and a poller left running would open an ssh connection per
    /// machine per tick for a screen nobody is looking at — retaining a round
    /// each time, because nothing is draining them.
    #[test]
    fn a_paused_poller_starts_no_rounds_and_keeps_only_the_newest() {
        let stub = Arc::new(StubSsh::new(&[("ws-host", ok(payload("one")))]));
        let remotes = [remote("ws")];
        let ctx = ctx_with(&remotes, stub.clone() as Arc<dyn Ssh>);
        let poller = Poller::spawn(&ctx, Duration::from_millis(1)).expect("a remote to poll");

        // Let it get going, then hand the terminal away.
        let deadline = Instant::now() + PATIENCE;
        while stub.calls().len() < 3 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(stub.calls().len() >= 3, "the poller never started");
        poller.pause();

        // The round in flight finishes; after that, nothing starts.
        let settled = loop {
            let before = stub.calls().len();
            std::thread::sleep(Duration::from_millis(50));
            if stub.calls().len() == before {
                break before;
            }
            assert!(Instant::now() < deadline, "the poller never stopped");
        };
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(stub.calls().len(), settled, "a paused poller kept dialling");

        // However many rounds ran, at most one was ever retained.
        assert!(poller.take().is_some(), "no round to pick up");
        assert!(
            poller.take().is_none(),
            "rounds were queued rather than kept"
        );

        // Coming back asks for one straight away.
        poller.resume();
        let deadline = Instant::now() + PATIENCE;
        while poller.take().is_none() {
            assert!(Instant::now() < deadline, "the poller did not resume");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(poller.alive());
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
