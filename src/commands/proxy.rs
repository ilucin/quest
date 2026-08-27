//! SPEC §15's generic remote dispatch: *every command that resolves a Quest on
//! a remote machine is proxied over ssh with the same arguments, and
//! `--no-remote` breaks the recursion.*
//!
//! One mechanism, one table. [`route`] is a pure `match` over [`Command`] that
//! says what a command aims at — a Quest, a session, or a creation — and how it
//! travels; [`dispatch`] is the only place that turns that into an ssh. No
//! command implements remoteness itself, so there is exactly one answer to
//! "what happens when the target is elsewhere" and it is auditable in one
//! screen.
//!
//! # The forwarded command line
//!
//! What travels is **this process's own argv**, not a reconstruction of it:
//! `q` at the front (SPEC §15's `ssh <alias> q …`), `--machine <remote>`
//! dropped (it named a machine *this* `q` knows, and on the far end it would be
//! an unknown machine), and `--no-remote` inserted. Everything else — `--json`,
//! `-q`, free text with spaces and quotes in it — is passed through untouched,
//! and quoting happens once, at the ssh boundary
//! ([`crate::remote::ssh_argv`]), never here.
//!
//! Two things about that line are not "the same arguments":
//!
//! * **The target is pinned to what it resolved to here** ([`pin`]). The
//!   fragment the user typed is resolved on *this* machine, against a listing
//!   that may be a cached one; sending the fragment on would have the far end
//!   resolve it a second time, against different data, with nothing checking
//!   that the two agreed. What travels is the Quest id — exact, and unique on
//!   the machine that is about to act on it — so the Quest the user was shown
//!   is the Quest that is acted on.
//! * **The guard is inserted where the far end will read it as a flag**
//!   ([`forward`]), not blindly appended: a line with a `--` in it makes
//!   everything after the separator positional over there, and an appended
//!   `--no-remote` would land in the far end's free text.
//!
//! Both are decided by re-parsing the candidate line with `q`'s own clap
//! definition — the same reading the far end will give it — rather than by
//! guessing at argv positions.
//!
//! # Why the recursion cannot happen
//!
//! Two independent guards, either of which is sufficient:
//!
//! 1. [`dispatch`] returns immediately unless [`remote::targets`] is non-empty,
//!    and `targets` is empty under `--no-remote` (it checks
//!    [`Ctx::remote_enabled`]). A `q` running with the flag has no remote to
//!    dial.
//! 2. [`locate::quest`] resolves against the same `targets`, so under
//!    `--no-remote` it can only ever return a local Quest, and a local Quest is
//!    never proxied.
//!
//! And [`forward`] appends `--no-remote` unconditionally, so a proxied
//! invocation is always a `q` of kind 1.
//!
//! # Streaming
//!
//! Nothing here streams. A proxied command runs to completion behind a deadline
//! ([`remote::PROXY_TIMEOUT`]), its stdout and stderr are relayed once it has,
//! and its exit code is this process's. `q events --follow` is therefore
//! refused rather than proxied — see [`Refusal::Follow`].
//!
//! # Destructive commands
//!
//! A `q close` / `q rm` / `q kill` whose target is elsewhere is confirmed
//! **here**, where the terminal is, and then sent with [`CONFIRMED`] — never
//! with `-f`. The two are not the same word: `-f` on `q rm` also authorises
//! killing a tmux session that is still running, which is a *second* question
//! the far end would otherwise never get to ask. `--confirmed` answers the
//! `[y/N]` and buys nothing else, so a proxied command is exactly as
//! destructive as the same command typed on that machine, and no more.

use std::io::{ErrorKind, Write};

use clap::Parser;

use crate::Ctx;
use crate::cli::{ArtifactAction, Cli, Command, LinkAction};
use crate::commands::locate::{self, Located};
use crate::commands::{confirm, enter, flush_warnings, new};
use crate::config::Remote;
use crate::error::QError;
use crate::output;
use crate::remote::{self, SshOutcome};

/// How the far end's `q` is invoked. SPEC §15 spells it as a bare `q`, so it is
/// whatever that machine's login shell finds on `PATH` — the same assumption
/// the listing fan-out already makes ([`remote::LIST_ARGV`]).
pub const REMOTE_Q: &str = "q";

/// The recursion breaker (SPEC §15), appended to every proxied invocation.
pub const NO_REMOTE: &str = "--no-remote";

/// ssh's own exit code when it could not run the command at all. `q` never
/// exits with it, so it is read as a connection failure rather than relayed as
/// the far end's answer.
const SSH_FAILED: i32 = 255;

/// The word a proxied confirmation sends instead of `-f`: the human has
/// answered the `[y/N]`, and that is all it says. See the module docs.
pub const CONFIRMED: &str = "--confirmed";

/// Why a command is not proxied at all. The reason is rendered with the
/// machine and Quest that were actually asked for, so an escape hatch it
/// prints is one the user can paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// `q events --follow`.
    ///
    /// A tail is a stream, and a stream over ssh is a choice between two bad
    /// ends. Without `-t` there is no pty, so killing the local ssh leaves the
    /// far `q` polling for ever with nobody reading it — bd-8lz.5.7's orphan,
    /// made unbounded. With `-t` the far end gets a SIGHUP on disconnect (which
    /// is right) but also a terminal: line endings become `\r\n` and `--json`
    /// stops being valid JSON on stdout, which is the one thing a proxied
    /// command must not do. Neither is worth a silent choice, so the snapshot
    /// is proxied, the tail is refused, and the escape hatch is printed.
    ///
    /// That escape hatch is `ssh -t`, deliberately: a human at a keyboard has a
    /// terminal, so the pty objection does not apply to them — and it is the
    /// `-t` that makes the far `q` die with the connection instead of becoming
    /// the very orphan this refusal exists to avoid.
    Follow,
    /// `q artifact add`.
    Artifact,
    /// `q phase`.
    Phase,
}

impl Refusal {
    /// The reason, for the machine (`alias` as `ssh` spells it) and Quest that
    /// were asked for.
    fn why(self, alias: &str, quest: &str) -> String {
        match self {
            Refusal::Follow => format!(
                "`--follow` tails a live log and does not travel over ssh; \
                 run it without `--follow` for a snapshot, or tail it there with \
                 `ssh -t {alias} q events {quest} -f --no-remote`"
            ),
            Refusal::Artifact => "an artifact is stored by absolute path, and that path is on \
                                  this machine; add it on the machine that runs the Quest"
                .to_string(),
            Refusal::Phase => "`q phase` reports for the session it runs in ($Q_SESSION), and \
                               a session belongs to the machine that runs it"
                .to_string(),
        }
    }
}

/// What a command aims at, for the purpose of deciding where it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aim<'a> {
    /// A `<quest>` target (SPEC §16), resolved across every machine.
    Quest(&'a str),
    /// A `<session>` target. Only the `<quest>/<label>` spelling can name one
    /// on another machine — see [`session_quest`].
    Session(&'a str),
    /// `q new --machine <remote>`: nothing is resolved, a Quest is created over
    /// there (SPEC §15).
    Create,
}

impl<'a> Aim<'a> {
    /// The `<label>` half of a `<session>` target, when this aim is one. The
    /// quest half is [`session_quest`]'s.
    fn session_label(self) -> Option<&'a str> {
        match self {
            Aim::Session(target) => target
                .split_once('/')
                .map(|(_, label)| label)
                .filter(|label| !label.is_empty()),
            Aim::Quest(_) | Aim::Create => None,
        }
    }
}

/// How a command travels once its target turns out to be elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passage {
    /// Sent as it stands.
    Proxy,
    /// Asked about **here**, where the terminal is, then sent with
    /// [`CONFIRMED`]. The far end's stdin is `/dev/null`, so its own prompt
    /// could only abort; the confirmation is a property of having a human, not
    /// of the machine that does the work. The verb is the question's, and the
    /// noun is whatever the target actually names — a Quest, or a session.
    Confirm(&'static str),
    /// Sent with `-d`, then the terminal is handed to the far end's tmux —
    /// `q resume`, which is `q new --machine` with an existing Quest.
    ThenEnter,
    /// Never proxied. See [`Refusal`].
    Refuse(Refusal),
}

/// The whole decision table (SPEC §15, §16). `None` is "this command can only
/// ever mean this machine".
///
/// Deliberately not proxied, and why:
/// * `q list` merges remote rows instead (bd-8lz.5.2).
/// * `q enter` hands the terminal to the far end's tmux instead (bd-8lz.5.2).
/// * `q sessions` with no Quest is the local fleet; making the fleet
///   multi-machine is SPEC §17's, not this bead's. With a Quest it is proxied,
///   which is what makes `q peek`/`q send` on that machine usable.
/// * `q hook`, `q doctor`, `q config`, the TUI: machine-local by definition.
/// * `q brief`/`q links`/`q note`/`q link` with no `--quest` fall back to
///   `$Q_QUEST`, which is this machine's pane environment.
pub fn route(command: &Command) -> Option<(Aim<'_>, Passage)> {
    use Passage::{Proxy, Refuse, ThenEnter};
    Some(match command {
        Command::New { .. } => (Aim::Create, Proxy),

        Command::Show { quest } => (Aim::Quest(quest), Proxy),
        Command::Rename { quest, .. } => (Aim::Quest(quest), Proxy),
        Command::Name { quest, .. } => (Aim::Quest(quest), Proxy),
        Command::Set { quest, .. } => (Aim::Quest(quest), Proxy),
        Command::Spawn { quest, .. } => (Aim::Quest(quest), Proxy),
        Command::Sessions {
            quest: Some(quest), ..
        } => (Aim::Quest(quest), Proxy),
        Command::Brief {
            quest: Some(quest), ..
        } => (Aim::Quest(quest), Proxy),
        Command::Links {
            quest: Some(quest), ..
        } => (Aim::Quest(quest), Proxy),
        Command::Events {
            quest: Some(quest),
            follow,
            ..
        } => (
            Aim::Quest(quest),
            if *follow {
                Refuse(Refusal::Follow)
            } else {
                Proxy
            },
        ),
        Command::Close { quest, force, .. } => (Aim::Quest(quest), asked(*force, "close quest")),
        Command::Rm { quest, force } => (
            Aim::Quest(quest),
            asked(*force, "remove quest (and all of its history)"),
        ),
        Command::Resume { quest, detach, .. } => {
            (Aim::Quest(quest), if *detach { Proxy } else { ThenEnter })
        }

        Command::Peek { session, .. } => (Aim::Session(session), Proxy),
        Command::Send { session, .. } => (Aim::Session(session), Proxy),
        Command::Reset { session, .. } => (Aim::Session(session), Proxy),
        Command::Kill { session, force } => (Aim::Session(session), asked(*force, "kill session")),

        Command::Note {
            quest: Some(quest), ..
        } => (Aim::Quest(quest), Proxy),
        // A relative `<ref>` would be made absolute against the far end's login
        // directory rather than this one — the common case is a URL, so the
        // command travels and the caveat is documented rather than refused.
        Command::Link {
            action:
                LinkAction::Add {
                    quest: Some(quest), ..
                }
                | LinkAction::Rm {
                    quest: Some(quest), ..
                },
        } => (Aim::Quest(quest), Proxy),
        Command::Artifact {
            action: ArtifactAction::Add {
                quest: Some(quest), ..
            },
        } => (Aim::Quest(quest), Refuse(Refusal::Artifact)),
        Command::Phase {
            quest: Some(quest), ..
        } => (Aim::Quest(quest), Refuse(Refusal::Phase)),

        _ => return None,
    })
}

/// `-f` already given means the human has answered; otherwise ask here.
fn asked(force: bool, verb: &'static str) -> Passage {
    if force {
        Passage::Proxy
    } else {
        Passage::Confirm(verb)
    }
}

/// The `<quest>` part of a `<session>` target, when it has one.
///
/// SPEC §16 lets a session be named three ways, and only one of them survives a
/// machine boundary. A session id and a bare `<label>` are resolved against
/// *this* database (and `$Q_QUEST`), and both are unique only per machine — a
/// bare label on another machine is exactly the guess `q` refuses to make
/// elsewhere. So only `<quest>/<label>` can point at a remote session, and the
/// other two stay local, where they already mean something.
fn session_quest(target: &str) -> Option<&str> {
    target
        .split_once('/')
        .map(|(quest, _)| quest)
        .filter(|q| !q.is_empty())
}

/// Run `command` on the machine its target lives on, or say this machine is the
/// one. `Some(code)` is the far end's exit code and means the command is done.
///
/// The first line is one of the two recursion guards (see the module docs): no
/// remote to ask, no proxy — which is also why a `q` with no `[[remotes]]`
/// configured pays nothing at all for any of this.
pub fn dispatch(ctx: &Ctx, command: &Command) -> anyhow::Result<Option<u8>> {
    if remote::targets(ctx).is_empty() {
        return Ok(None);
    }
    // Guard 1 of the module docs, stated where it is relied on: `targets` is
    // built from `Ctx::remote_enabled`, so getting past the line above means
    // this invocation did not carry `--no-remote`.
    debug_assert!(ctx.remote_enabled(), "--no-remote must leave no targets");
    let Some((aim, passage)) = route(command) else {
        return Ok(None);
    };

    let target = match aim {
        Aim::Create => return create(ctx, command),
        Aim::Quest(target) => target,
        // A session id or a bare label names a session in this database; only
        // `<quest>/<label>` can name one on another machine.
        Aim::Session(target) => match session_quest(target) {
            Some(quest) => quest,
            None => return Ok(None),
        },
    };

    let found = locate::quest(ctx, target);
    // Whatever the fan-out has to say about a machine that did not answer
    // belongs on the terminal before the command's own output — and before the
    // error, when resolution failed because a machine was down.
    flush_warnings(ctx);
    let found = found?;
    let Some(machine) = found.machine.clone() else {
        return Ok(None);
    };
    let remote = remote::find(&ctx.config.remotes, &machine)?;

    // What the far end is told to act on: the identity this resolved to here,
    // not the fragment that was typed. See [`pin`].
    let raw = raw_args();
    let pinned = pin(&raw, aim, &found.quest.id);
    let deadline = deadline(command);

    match passage {
        Passage::Refuse(why) => Err(QError::Other(format!(
            "{} runs on {machine}: {}",
            found.quest.slug,
            why.why(&remote.ssh, &found.quest.slug)
        ))
        .into()),
        Passage::Proxy => send(
            ctx,
            remote,
            pinned.as_ref().unwrap_or(&raw),
            &[],
            false,
            deadline,
        )
        .map(|(code, _)| Some(code)),
        Passage::Confirm(verb) => {
            // A subject the far end could re-read differently is the whole of
            // B2: a confirmation that names one Quest while the command
            // destroys another is worse than no confirmation at all.
            let Some(pinned) = pinned else {
                return Err(unpinnable(&found.quest.slug, &machine));
            };
            let subject = subject(&found, aim);
            // The master is refused *before* the question, exactly as
            // `kill::guard_master` refuses it before `q kill`'s own prompt:
            // asking a human to authorise something that cannot happen is not
            // a confirmation, it is a trap.
            guard_remote_master(&found, aim)?;
            confirm(ctx, &format!("{verb} {subject} on {machine}?"))?;
            let extra: &[&str] = if raw.iter().any(|a| a == CONFIRMED) {
                &[]
            } else {
                &[CONFIRMED]
            };
            send(ctx, remote, &pinned, extra, false, deadline).map(|(code, _)| Some(code))
        }
        // `-d` because there is no terminal at the far end to attach to; the
        // attach is this machine's, once the Quest is back up over there.
        Passage::ThenEnter => {
            // Under `--json` the far end's document is held back rather than
            // relayed: the attach is part of the same answer, and two JSON
            // documents on one stdout is not `--json`.
            let (code, held) = send(
                ctx,
                remote,
                pinned.as_ref().unwrap_or(&raw),
                &["-d"],
                ctx.json,
                deadline,
            )?;
            if code != 0 {
                write_out(std::io::stdout(), &held)?;
                return Ok(Some(code));
            }
            enter_after(ctx, &found, &machine, held).map(|()| Some(0))
        }
    }
}

/// What a confirmation is *about*: the Quest, or — for `q kill` — the session
/// inside it, which is the only thing that is going to die.
fn subject(found: &Located, aim: Aim<'_>) -> String {
    match aim.session_label() {
        Some(label) => format!("{}/{label}", found.quest.slug),
        None => found.quest.slug.clone(),
    }
}

/// `kill::guard_master`, for a session that lives elsewhere.
///
/// The far end runs the real guard (it can see the row's `role`); this one
/// reads the reserved label out of the target, which is the spelling every
/// master answers to, and refuses before a human is asked to authorise a kill
/// that `q kill` would never perform.
fn guard_remote_master(found: &Located, aim: Aim<'_>) -> anyhow::Result<()> {
    let Some(label) = aim.session_label() else {
        return Ok(());
    };
    if label != new::MASTER {
        return Ok(());
    }
    Err(QError::Invalid(format!(
        "{}/{label} is the master of quest {}; run `q close {}` to end the whole Quest",
        found.quest.slug, found.quest.slug, found.quest.slug
    ))
    .into())
}

/// A target this process cannot restate as an identity is refused rather than
/// destroyed on a guess — see [`pin`]. Unreachable for every argv `q` accepts;
/// it is here because "unreachable" and "destroys the wrong Quest" are one
/// mistake apart.
fn unpinnable(slug: &str, machine: &str) -> anyhow::Error {
    QError::Other(format!(
        "cannot say which quest on {machine} this names ({slug}); \
         run it there with `q --no-remote`"
    ))
    .into()
}

/// How long the far end gets. [`remote::PROXY_TIMEOUT`] for everything except a
/// command that was *asked* to wait: `q reset --delay N` sleeps N seconds over
/// there before it does anything, so the deadline has to clear the sleep or the
/// command it schedules is always reported as one that may still be running.
fn deadline(command: &Command) -> std::time::Duration {
    match command {
        Command::Reset {
            delay: Some(delay), ..
        } => remote::PROXY_TIMEOUT + std::time::Duration::from_secs(*delay),
        _ => remote::PROXY_TIMEOUT,
    }
}

/// Hand the terminal to the far end once a `q resume` has brought the Quest
/// back up there (SPEC §15's "…, then enter").
///
/// `held` is the far end's own `--json` document, kept back so the attach can
/// be folded into it: what a `--json` caller gets is one object describing both
/// halves. A far end whose answer does not parse (an older or newer `q`) has
/// its bytes relayed untouched instead, and the attach is not reported —
/// mangling its document would be worse than saying less about ours.
fn enter_after(ctx: &Ctx, found: &Located, machine: &str, held: String) -> anyhow::Result<()> {
    let remote = remote::find(&ctx.config.remotes, machine)?;
    let target = enter::remote_target(
        ctx,
        remote,
        &found.quest.slug,
        found.tmux_prefix.as_deref(),
        None,
    );
    let attach = serde_json::json!({
        "machine": target.machine,
        "ssh": target.alias,
        "tmux_session": target.tmux_session,
        "remote": true,
        "argv": target.argv,
    });
    if ctx.json {
        match serde_json::from_str::<serde_json::Value>(held.trim()) {
            Ok(serde_json::Value::Object(mut resumed)) => {
                for (key, value) in attach.as_object().expect("built as an object") {
                    resumed.insert(key.clone(), value.clone());
                }
                output::emit(true, &serde_json::Value::Object(resumed), String::new)?;
            }
            _ => write_out(std::io::stdout(), &held)?,
        }
    } else if !ctx.quiet {
        output::emit(false, &attach, || {
            format!(
                "attaching to {}:{} over ssh",
                target.machine, target.tmux_session
            )
        })?;
    }
    // The attach replaces this process, so nothing buffered survives it.
    std::io::stdout().flush()?;
    ctx.ssh().attach(&target.alias, &target.argv)
}

/// `raw`, over ssh to `remote`, with `extra` and the recursion guard put where
/// the far end reads them as flags. Relays the far end's streams and hands back
/// its exit code — and, under `hold_stdout`, its stdout instead of writing it,
/// for the one caller that has something to add to the same answer
/// ([`enter_after`]).
fn send(
    ctx: &Ctx,
    remote: &Remote,
    raw: &[String],
    extra: &[&str],
    hold_stdout: bool,
    deadline: std::time::Duration,
) -> anyhow::Result<(u8, String)> {
    let argv = forward(raw, ctx.machine_filter(), extra);
    relay(ctx, remote, &argv, hold_stdout, deadline)
}

/// Run `argv` on `remote` and become its result: stdout to stdout, stderr to
/// stderr (CLAUDE.md), exit code propagated.
///
/// Verbatim in both directions. Under `--json` the far end has already written
/// a JSON document to its stdout and a `{"error": …}` to its stderr, and
/// re-rendering either here could only make them less true.
fn relay(
    ctx: &Ctx,
    remote: &Remote,
    argv: &[String],
    hold_stdout: bool,
    deadline: std::time::Duration,
) -> anyhow::Result<(u8, String)> {
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    let outcome = ctx.ssh().run(&remote.ssh, &borrowed, deadline);
    match outcome {
        SshOutcome::Done {
            code,
            stdout,
            stderr,
        } => {
            match code {
                // ssh's own failure code, not the far end's: `q` never exits
                // 255, so relaying it as an answer would be a lie about a
                // command that never ran.
                Some(SSH_FAILED) | None => {
                    return Err(unreachable_remote(&remote.name, code, &stderr));
                }
                Some(_) => {}
            }
            if !hold_stdout {
                write_out(std::io::stdout(), &stdout)?;
            }
            write_out(std::io::stderr(), &stderr)?;
            // Only 0..=255 can be an exit code; a `q` exits 0, 1 or 2.
            let code = u8::try_from(code.unwrap_or(1)).unwrap_or(1);
            Ok((code, if hold_stdout { stdout } else { String::new() }))
        }
        SshOutcome::TimedOut => Err(QError::Other(format!(
            "`{}` did not finish within {}s on {}; it may still be running there",
            argv.join(" "),
            deadline.as_secs(),
            remote.name
        ))
        .into()),
        // The cap was sized for a listing and now bounds every relayed command,
        // so it says how big it is rather than leaving the user to guess what
        // "more" means.
        SshOutcome::TooLarge => Err(QError::Other(format!(
            "{} sent more than the {} MiB `q` will relay; run it there \
             (`ssh -t {} q … --no-remote`), or ask for less of it",
            remote.name,
            remote::MAX_OUTPUT >> 20,
            remote.ssh
        ))
        .into()),
        SshOutcome::Failed(e) => {
            Err(QError::Other(format!("cannot reach {}: {e}", remote.name)).into())
        }
    }
}

fn unreachable_remote(machine: &str, code: Option<i32>, stderr: &str) -> anyhow::Error {
    let said = output::first_line(stderr, 200);
    let detail = if said.is_empty() {
        String::new()
    } else {
        format!(": {said}")
    };
    match code {
        Some(_) => QError::Other(format!("cannot reach {machine}{detail}")).into(),
        None => QError::Other(format!("ssh to {machine} was killed by a signal{detail}")).into(),
    }
}

/// Relay one stream verbatim. A closed pipe (`q peek … | head`) is not an
/// error, exactly as it is not one in [`output::emit`].
fn write_out(mut to: impl Write, text: &str) -> anyhow::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    match to.write_all(text.as_bytes()).and_then(|()| to.flush()) {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

/// This process's arguments, without argv[0].
fn raw_args() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// The command line the far end must receive (see the module docs).
///
/// `machine` is `--machine`'s **value** as clap parsed it, and only that exact
/// spelling is dropped — `--machine ws` or `--machine=ws`, first occurrence
/// only. Scanning for the flag name alone would eat a `--machine=…` that clap
/// read as free text (`q send s -- --machine=ws`); matching the parsed value
/// means the only thing dropped is the flag that was really given. With no
/// `--machine` at all, nothing is dropped and the argv travels untouched.
///
/// `extra` and the recursion guard are **inserted**, not appended. A `--` in
/// the line makes everything after it positional over there, so a guard on the
/// end of `q note --quest x -- "-- text"` arrives as a second `<TEXT>` and the
/// far end rejects a command that is perfectly legal on the machine that runs
/// it. Where they go is decided by asking clap — the far end's own reading —
/// rather than by guessing: the last position whose parse still sees the guard
/// as the flag it is wins, and the end of the line is tried first so an
/// ordinary command travels exactly as it always did.
pub fn forward(raw: &[String], machine: Option<&str>, extra: &[&str]) -> Vec<String> {
    let kept = without_machine(raw, machine);
    let mut flags: Vec<String> = extra.iter().map(|e| (*e).to_string()).collect();
    flags.push(NO_REMOTE.to_string());
    for at in (0..=kept.len()).rev() {
        let candidate = splice(&kept, at, &flags);
        if guarded(&candidate) {
            return candidate;
        }
    }
    // No reading of this line has the guard as a flag. Unreachable for every
    // argv clap accepts here — the position before the subcommand always works,
    // `--no-remote` being global — so the end of the line is as good a last
    // resort as any, and the far end says what it did not understand.
    splice(&kept, kept.len(), &flags)
}

/// `raw` without the `--machine` that was really given.
fn without_machine(raw: &[String], machine: Option<&str>) -> Vec<String> {
    let joined = machine.map(|m| format!("--machine={m}"));
    let mut out = Vec::with_capacity(raw.len());
    let mut dropped = machine.is_none();
    let mut at = 0;
    while at < raw.len() {
        let arg = &raw[at];
        if !dropped {
            let machine = machine.expect("dropped is set when there is no --machine");
            if arg == "--machine" && raw.get(at + 1).is_some_and(|v| v == machine) {
                dropped = true;
                at += 2;
                continue;
            }
            if joined.as_deref() == Some(arg.as_str()) {
                dropped = true;
                at += 1;
                continue;
            }
        }
        out.push(arg.clone());
        at += 1;
    }
    out
}

/// `q`, then `kept` with `flags` inserted at `at`.
fn splice(kept: &[String], at: usize, flags: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(kept.len() + flags.len() + 1);
    out.push(REMOTE_Q.to_string());
    out.extend(kept[..at].iter().cloned());
    out.extend(flags.iter().cloned());
    out.extend(kept[at..].iter().cloned());
    out
}

/// Would the far end read this line as one carrying the recursion guard?
/// Answered by `q`'s own parser, so the answer is the far end's.
fn guarded(argv: &[String]) -> bool {
    Cli::try_parse_from(argv).is_ok_and(|cli| cli.no_remote)
}

/// The same line with its `<quest>` target replaced by the identity it resolved
/// to on this machine.
///
/// Resolution happens twice — once here, to find out which machine owns the
/// target, and once over there, when the far end runs the command. Those two
/// walk different databases: this one may be reading a `remote_cache` row left
/// by a listing that timed out at 5 s, while the command itself gets 60 s and
/// reaches a host that has since renamed, finished or created Quests. Sending
/// the fragment on lets the two disagree in silence, and a `q rm` is not a
/// place for that: the user confirms one Quest by name and another one dies.
///
/// So what travels is `found.quest.id` — exact, and unique on the machine that
/// is about to act on it — spliced into the token the target was typed in
/// (`--quest=alpha` included). Which token that is, is not guessed: each
/// candidate is re-parsed with `q`'s own clap definition and kept only when
/// [`route`] reads the new spelling as the target. Exactly one token changed,
/// and it is the one the target is read from, so nothing else in the line can
/// have moved.
///
/// `None` means no reading of this line names the identity — see
/// [`unpinnable`].
fn pin(raw: &[String], aim: Aim<'_>, id: &str) -> Option<Vec<String>> {
    let (target, want) = match aim {
        Aim::Create => return Some(raw.to_vec()),
        Aim::Quest(target) => (target, id.to_string()),
        Aim::Session(target) => (target, format!("{id}/{}", aim.session_label()?)),
    };
    if target == want {
        return Some(raw.to_vec());
    }
    for at in 0..raw.len() {
        let Some(rewritten) = restate(&raw[at], target, &want) else {
            continue;
        };
        let mut candidate = raw.to_vec();
        candidate[at] = rewritten;
        if names(&candidate, &want) {
            return Some(candidate);
        }
    }
    None
}

/// One argv word with `target` said as `want`: the bare word, or the value half
/// of a `--flag=value`. Anything else is not a spelling of this target.
fn restate(arg: &str, target: &str, want: &str) -> Option<String> {
    if arg == target {
        return Some(want.to_string());
    }
    let (flag, value) = arg.split_once('=')?;
    (flag.starts_with("--") && value == target).then(|| format!("{flag}={want}"))
}

/// Does this line name `want` as the Quest (or session) it acts on? Asked of
/// clap and [`route`], never of the string.
fn names(raw: &[String], want: &str) -> bool {
    let mut line = Vec::with_capacity(raw.len() + 1);
    line.push(REMOTE_Q.to_string());
    line.extend(raw.iter().cloned());
    let Ok(cli) = Cli::try_parse_from(line) else {
        return false;
    };
    let Some(command) = cli.command else {
        return false;
    };
    matches!(
        route(&command),
        Some((Aim::Quest(target) | Aim::Session(target), _)) if target == want
    )
}

// ------------------------------------------------------------------ create

/// `q new --machine ws …` → `ssh <alias> q new … -d`, then enter (SPEC §15).
///
/// The one command whose remote form is **built** rather than forwarded. It has
/// to be: the TUI's new-Quest form (SPEC §17) reaches the same path with no
/// argv behind it at all, and the machine select in that form was the other
/// half of this bead's bug — it labelled a *local* Quest with a remote's name.
/// One builder, so the CLI and the form cannot drift.
fn create(ctx: &Ctx, command: &Command) -> anyhow::Result<Option<u8>> {
    let Some(machine) = ctx.machine_filter() else {
        return Ok(None);
    };
    // `targets` being non-empty already proved this, but `find` is what turns
    // the name into an alias.
    let Ok(remote) = remote::find(&ctx.config.remotes, machine) else {
        return Ok(None);
    };
    let Command::New {
        name,
        goal,
        dir,
        workflow,
        repo,
        no_beads,
        prompt,
        prompt_file,
        no_auto_reset,
        detach,
    } = command
    else {
        return Ok(None);
    };
    // A relative `--dir` is the `link add` relative-`<ref>` caveat again (see
    // `route`): it is made absolute against the far end's login directory, not
    // this one. The common case is an absolute path or none at all, and the
    // far end refuses a directory it does not have, so the command travels and
    // the caveat is documented rather than refused.
    //
    // `--prompt-file -` reads *this* machine's stdin, so it is resolved here
    // and travels as text.
    let prompt = new::resolve_prompt(prompt.as_deref(), prompt_file.as_deref())?;
    let args = new::Args {
        name: name.as_deref(),
        goal: goal.as_deref(),
        dir: dir.as_deref(),
        workflow: workflow.as_deref(),
        repo: repo.as_deref(),
        no_beads: *no_beads,
        prompt: prompt.as_deref(),
        prompt_file: None,
        no_auto_reset: *no_auto_reset,
        detach: *detach,
        machine: Some(machine),
        template: None,
    };
    let created = create_remote(ctx, remote, &args)?;
    report_created(ctx, &created, !args.detach)?;
    if args.detach {
        return Ok(Some(0));
    }
    let argv = enter::attach_command(ctx, &created.tmux_session);
    std::io::stdout().flush()?;
    ctx.ssh().attach(&remote.ssh, &argv).map(|()| Some(0))
}

/// A Quest as the machine that created it reported it back.
pub struct CreatedRemote {
    pub machine: String,
    /// The far end's whole `q new --json` payload, re-emitted rather than
    /// rebuilt: a field a newer `q` over there knows and this one does not
    /// survives the trip.
    pub payload: serde_json::Value,
    pub slug: String,
    /// The tmux session **that machine** named it, so nothing here has to guess
    /// its `[tmux] session_prefix`.
    pub tmux_session: String,
}

/// Create the Quest on `remote` (SPEC §15). Shared by the CLI and the TUI's
/// new-Quest form.
///
/// Always `-d` and always `--json` on the wire: `-d` because there is no
/// terminal at the far end to attach to (that is this machine's job,
/// afterwards), and `--json` because the answer has to be *read* — the slug and
/// the tmux session name are how the attach that follows finds the Quest.
pub fn create_remote(
    ctx: &Ctx,
    remote: &Remote,
    args: &new::Args,
) -> anyhow::Result<CreatedRemote> {
    if args.template.is_some() {
        return Err(QError::Invalid(
            "a template lives in the database of the machine that holds it; \
             run the template on that machine"
                .to_string(),
        )
        .into());
    }
    let argv = new::remote_argv(args);
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    let outcome = ctx.ssh().run(&remote.ssh, &borrowed, remote::PROXY_TIMEOUT);
    let stdout = match outcome {
        SshOutcome::Done {
            code: Some(0),
            stdout,
            ..
        } => stdout,
        SshOutcome::Done { code, stderr, .. } => {
            return Err(create_failed(&remote.name, code, &stderr));
        }
        SshOutcome::TimedOut => {
            return Err(QError::Other(format!(
                "`q new` did not finish within {}s on {}; check `q list` before retrying",
                remote::PROXY_TIMEOUT.as_secs(),
                remote.name
            ))
            .into());
        }
        SshOutcome::TooLarge => {
            return Err(QError::Other(format!(
                "{} sent an unreadable answer{MAY_EXIST}",
                remote.name
            ))
            .into());
        }
        SshOutcome::Failed(e) => {
            return Err(QError::Other(format!("cannot reach {}: {e}", remote.name)).into());
        }
    };
    // Exit 0 over there means the Quest exists over there, whatever this end
    // can make of the answer.
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        QError::Other(format!(
            "cannot read `q new --json` from {}: {e}{MAY_EXIST}",
            remote.name
        ))
    })?;
    let field = |key: &str, at: &serde_json::Value| -> anyhow::Result<String> {
        at.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                QError::Other(format!(
                    "`q new --json` from {} has no `{key}`{MAY_EXIST}",
                    remote.name
                ))
                .into()
            })
    };
    let slug = field(
        "slug",
        payload.get("quest").unwrap_or(&serde_json::Value::Null),
    )?;
    let tmux_session = field("tmux_session", &payload)?;
    Ok(CreatedRemote {
        machine: remote.name.to_string(),
        payload,
        slug,
        tmux_session,
    })
}

/// What is added to every `q new --machine` failure that could have happened
/// *after* the far end committed the Quest.
///
/// A `q new` that fails on the way there leaves nothing behind; one whose ssh
/// drops on the way back, or whose answer this `q` cannot read, may already
/// have created a Quest and a tmux session over there. Saying only "cannot
/// reach ws" invites a retry that creates a second one.
const MAY_EXIST: &str = "; the quest may already have been created there — check `q list` \
                         before retrying";

fn create_failed(machine: &str, code: Option<i32>, stderr: &str) -> anyhow::Error {
    if code == Some(SSH_FAILED) || code.is_none() {
        // The command ran; only the answer was lost.
        return QError::Other(format!(
            "{:#}{MAY_EXIST}",
            unreachable_remote(machine, code, stderr)
        ))
        .into();
    }
    // `q --json` puts `{"error": …}` on stderr; anything else is relayed as it
    // came.
    let said = serde_json::from_str::<serde_json::Value>(stderr.trim())
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| output::first_line(stderr, 200));
    QError::Other(format!("`q new` failed on {machine}: {said}")).into()
}

/// The one-liner (or payload) for a Quest that was created elsewhere.
fn report_created(ctx: &Ctx, created: &CreatedRemote, attaching: bool) -> anyhow::Result<()> {
    if !ctx.json && ctx.quiet {
        return Ok(());
    }
    let mut payload = created.payload.clone();
    if let Some(map) = payload.as_object_mut() {
        map.insert(
            "machine".to_string(),
            serde_json::Value::String(created.machine.clone()),
        );
        map.insert("remote".to_string(), serde_json::Value::Bool(true));
        map.insert(
            "attach".to_string(),
            serde_json::Value::String(if attaching { "exec" } else { "none" }.to_string()),
        );
    }
    output::emit(ctx.json, &payload, || {
        format!(
            "created quest {} on {} · tmux {} · run: q enter {}",
            created.slug, created.machine, created.tmux_session, created.slug
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    fn args(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_string).collect()
    }

    fn parse(line: &str) -> Cli {
        let mut argv = vec!["q".to_string()];
        argv.extend(args(line));
        Cli::try_parse_from(argv).expect(line)
    }

    /// SPEC §15: the same arguments, plus the recursion guard, with `q` at the
    /// front rather than this binary's path.
    #[test]
    fn the_forwarded_line_is_this_one_plus_the_guard() {
        assert_eq!(
            forward(&args("show alpha --json"), None, &[]),
            ["q", "show", "alpha", "--json", "--no-remote"]
        );
        assert_eq!(
            forward(&args("close alpha"), None, &["-f"]),
            ["q", "close", "alpha", "-f", "--no-remote"]
        );
    }

    /// `--machine` named a machine *this* `q` knows; over there it is an
    /// unknown machine and the command would refuse rather than run.
    #[test]
    fn the_machine_flag_does_not_travel() {
        assert_eq!(
            forward(&args("--machine ws show alpha"), Some("ws"), &[]),
            ["q", "show", "alpha", "--no-remote"]
        );
        assert_eq!(
            forward(&args("show alpha --machine=ws"), Some("ws"), &[]),
            ["q", "show", "alpha", "--no-remote"]
        );
    }

    /// Only the flag that was really given is dropped: free text that happens
    /// to read like one travels untouched.
    #[test]
    fn free_text_that_looks_like_the_machine_flag_survives() {
        // No `--machine` was parsed, so nothing is a candidate for dropping.
        let raw = args("send alpha/tests --machine=ws");
        assert_eq!(
            forward(&raw, None, &[]),
            ["q", "send", "alpha/tests", "--machine=ws", "--no-remote"]
        );
        // With one given, the *first* occurrence goes and the text stays.
        let raw = vec![
            "--machine".to_string(),
            "ws".to_string(),
            "send".to_string(),
            "alpha/tests".to_string(),
            "--machine=ws".to_string(),
        ];
        assert_eq!(
            forward(&raw, Some("ws"), &[]),
            ["q", "send", "alpha/tests", "--machine=ws", "--no-remote"]
        );
    }

    /// Text with spaces, quotes and `$` in it is one argv word here and one
    /// shell word over there — quoting is [`crate::remote`]'s, at the single
    /// ssh boundary, and nothing is pre-quoted on the way in.
    #[test]
    fn free_text_is_carried_as_one_word_and_quoted_only_at_the_boundary() {
        let raw = vec![
            "send".to_string(),
            "alpha/tests".to_string(),
            "run `make test` for $USER 'now'".to_string(),
        ];
        let argv = forward(&raw, None, &[]);
        assert_eq!(argv[3], "run `make test` for $USER 'now'");
        let sent = remote::sh_quote(&argv[3]);
        assert_eq!(sent, r#"'run `make test` for $USER '\''now'\'''"#);
    }

    #[test]
    fn every_quest_taking_command_has_a_decision() {
        for (line, want) in [
            ("show alpha", Some(Aim::Quest("alpha"))),
            ("rename alpha beta", Some(Aim::Quest("alpha"))),
            ("set alpha goal x", Some(Aim::Quest("alpha"))),
            ("name alpha", Some(Aim::Quest("alpha"))),
            ("rm alpha", Some(Aim::Quest("alpha"))),
            ("close alpha", Some(Aim::Quest("alpha"))),
            ("resume alpha", Some(Aim::Quest("alpha"))),
            ("spawn alpha --label t p", Some(Aim::Quest("alpha"))),
            ("sessions alpha", Some(Aim::Quest("alpha"))),
            ("brief alpha", Some(Aim::Quest("alpha"))),
            ("links alpha", Some(Aim::Quest("alpha"))),
            ("events alpha", Some(Aim::Quest("alpha"))),
            ("peek alpha/tests", Some(Aim::Session("alpha/tests"))),
            ("send alpha/tests hi", Some(Aim::Session("alpha/tests"))),
            ("reset alpha/tests", Some(Aim::Session("alpha/tests"))),
            ("kill alpha/tests", Some(Aim::Session("alpha/tests"))),
            ("note hi --quest alpha", Some(Aim::Quest("alpha"))),
            ("link add url --quest alpha", Some(Aim::Quest("alpha"))),
            ("artifact add /p --quest alpha", Some(Aim::Quest("alpha"))),
            ("phase hi --quest alpha", Some(Aim::Quest("alpha"))),
            ("new", Some(Aim::Create)),
            // Never proxied.
            ("list", None),
            ("enter alpha", None),
            ("sessions", None),
            ("brief", None),
            ("links", None),
            ("events", None),
            ("note hi", None),
            ("doctor", None),
        ] {
            let cli = parse(line);
            let got = route(cli.command.as_ref().expect(line)).map(|(aim, _)| aim);
            assert_eq!(got, want, "{line}");
        }
    }

    /// The commands that do not simply travel, and why.
    #[test]
    fn the_passages_that_are_not_a_plain_proxy() {
        let passage = |line: &str| {
            let cli = parse(line);
            route(cli.command.as_ref().unwrap()).unwrap().1
        };
        assert_eq!(
            passage("events alpha --follow"),
            Passage::Refuse(Refusal::Follow)
        );
        assert_eq!(passage("events alpha"), Passage::Proxy);
        assert_eq!(
            passage("artifact add /p --quest alpha"),
            Passage::Refuse(Refusal::Artifact)
        );
        assert_eq!(
            passage("phase hi --quest alpha"),
            Passage::Refuse(Refusal::Phase)
        );
        assert_eq!(passage("resume alpha"), Passage::ThenEnter);
        assert_eq!(passage("resume alpha -d"), Passage::Proxy);
        assert_eq!(passage("close alpha"), Passage::Confirm("close quest"));
        assert_eq!(passage("close alpha -f"), Passage::Proxy);
        assert_eq!(passage("kill alpha/t"), Passage::Confirm("kill session"));
        assert_eq!(passage("kill alpha/t -f"), Passage::Proxy);
        assert!(matches!(passage("rm alpha"), Passage::Confirm(_)));
        assert_eq!(passage("rm alpha -f"), Passage::Proxy);
    }

    /// A `--` makes everything after it positional over there, so the guard
    /// goes where the far end reads it as a flag. Every command with free
    /// trailing text can be spelled with one.
    #[test]
    fn the_guard_lands_before_a_separator_not_after_it() {
        for line in [
            "note --quest alpha -- --dashed",
            "send alpha/tests -- --help",
            "set alpha goal -- --not-a-flag",
            "spawn alpha --label t -- --dashed",
        ] {
            let raw = args(line);
            let sent = forward(&raw, None, &[]);
            let cli = Cli::try_parse_from(&sent).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(cli.no_remote, "{line}: {sent:?}");
            // The guard is the only thing added, and the text is untouched.
            let text: Vec<&String> = sent.iter().filter(|w| *w != NO_REMOTE).skip(1).collect();
            assert_eq!(
                text.into_iter().cloned().collect::<Vec<String>>(),
                raw,
                "{line}"
            );
        }
    }

    /// With no `--` in the line the guard still rides on the end, so an
    /// ordinary proxied command travels exactly as it always did.
    #[test]
    fn an_ordinary_line_still_carries_the_guard_at_the_end() {
        assert_eq!(
            forward(&args("show alpha --json"), None, &[]),
            ["q", "show", "alpha", "--json", "--no-remote"]
        );
    }

    /// The target that travels is the identity it resolved to here, in
    /// whichever token it was typed — the bare positional, the `<quest>` half
    /// of a session target, or a `--quest=` value.
    #[test]
    fn the_target_travels_as_the_identity_it_resolved_to() {
        for (line, want) in [
            ("rm alpha", vec!["rm", "q-1234"]),
            ("rename alpha beta", vec!["rename", "q-1234", "beta"]),
            ("kill alpha/tests", vec!["kill", "q-1234/tests"]),
            (
                "note hi --quest alpha",
                vec!["note", "hi", "--quest", "q-1234"],
            ),
            (
                "note hi --quest=alpha",
                vec!["note", "hi", "--quest=q-1234"],
            ),
        ] {
            let cli = parse(line);
            let command = cli.command.as_ref().expect(line);
            let (aim, _) = route(command).expect(line);
            let pinned = pin(&args(line), aim, "q-1234").unwrap_or_else(|| panic!("{line}"));
            assert_eq!(pinned, want, "{line}");
        }
    }

    /// Only the token the target was read from moves, however many other words
    /// happen to spell the same thing. The candidate is verified by re-parsing,
    /// so a value that merely looks like the target is never the one rewritten.
    #[test]
    fn a_word_that_only_looks_like_the_target_is_left_alone() {
        // `--label alpha` comes first and reads like the target.
        let line = "spawn --label alpha alpha go";
        let cli = parse(line);
        let (aim, _) = route(cli.command.as_ref().unwrap()).unwrap();
        assert_eq!(
            pin(&args(line), aim, "q-1234").unwrap(),
            ["spawn", "--label", "alpha", "q-1234", "go"]
        );

        // …and a machine that shares the target's name.
        let line = "rm ws --machine ws";
        let cli = parse(line);
        let (aim, _) = route(cli.command.as_ref().unwrap()).unwrap();
        assert_eq!(
            pin(&args(line), aim, "q-1234").unwrap(),
            ["rm", "q-1234", "--machine", "ws"]
        );
    }

    /// `q reset --delay N` is *asked* to wait N seconds over there, so the
    /// deadline has to clear the sleep — otherwise every scheduled reset is
    /// reported as one that may still be running.
    #[test]
    fn a_delayed_reset_gets_a_deadline_that_clears_its_own_sleep() {
        let deadline_of = |line: &str| deadline(parse(line).command.as_ref().unwrap());
        assert_eq!(deadline_of("reset alpha/t"), remote::PROXY_TIMEOUT);
        assert_eq!(
            deadline_of("reset alpha/t --delay 90"),
            remote::PROXY_TIMEOUT + std::time::Duration::from_secs(90)
        );
        assert_eq!(deadline_of("rm alpha"), remote::PROXY_TIMEOUT);
    }

    /// The refusal's escape hatch is one the user can paste: the real alias,
    /// the real Quest, and `ssh -t` — whose pty is what keeps the far `q` from
    /// outliving the connection, which is the whole reason `--follow` is
    /// refused.
    #[test]
    fn the_follow_refusal_prints_a_hatch_that_does_not_orphan() {
        let said = Refusal::Follow.why("ws-host", "over-there");
        assert!(
            said.contains("`ssh -t ws-host q events over-there -f --no-remote`"),
            "{said}"
        );
        assert!(
            !said.contains("<alias>") && !said.contains("<quest>"),
            "{said}"
        );
    }

    /// Only `<quest>/<label>` can name a session on another machine.
    #[test]
    fn a_bare_label_or_a_session_id_stays_on_this_machine() {
        assert_eq!(session_quest("alpha/tests"), Some("alpha"));
        assert_eq!(session_quest("tests"), None);
        assert_eq!(session_quest("s-0001"), None);
        assert_eq!(session_quest("/tests"), None);
    }
}
