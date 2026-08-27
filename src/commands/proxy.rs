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
//! an unknown machine), and `--no-remote` appended. Everything else — `--json`,
//! `-q`, free text with spaces and quotes in it — is passed through untouched,
//! and quoting happens once, at the ssh boundary
//! ([`crate::remote::ssh_argv`]), never here.
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
//! refused rather than proxied — see [`FOLLOW`].

use std::io::{ErrorKind, Write};

use crate::Ctx;
use crate::cli::{ArtifactAction, Command, LinkAction};
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

/// Why `--follow` does not travel.
///
/// A tail is a stream, and a stream over ssh is a choice between two bad ends.
/// Without `-t` there is no pty, so killing the local ssh leaves the far `q`
/// polling for ever with nobody reading it — bd-8lz.5.7's orphan, made
/// unbounded. With `-t` the far end gets a SIGHUP on disconnect (which is
/// right) but also a terminal: line endings become `\r\n` and `--json` stops
/// being valid JSON on stdout, which is the one thing a proxied command must
/// not do. Neither is worth a silent choice, so the snapshot is proxied, the
/// tail is refused, and the escape hatch is printed.
const FOLLOW: &str = "`--follow` tails a live log and does not travel over ssh; \
                      run it without `--follow` for a snapshot, or tail it there with \
                      `ssh <alias> q events <quest> -f --no-remote`";

/// Why `q artifact add` does not travel.
const ARTIFACT: &str = "an artifact is stored by absolute path, and that path is on this \
                        machine; add it on the machine that runs the Quest";

/// Why `q phase` does not travel.
const PHASE: &str = "`q phase` reports for the session it runs in ($Q_SESSION), and a \
                     session belongs to the machine that runs it";

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

/// How a command travels once its target turns out to be elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Passage {
    /// Sent as it stands.
    Proxy,
    /// Asked about **here**, where the terminal is, then sent with `-f`. The
    /// far end's stdin is `/dev/null`, so its own prompt could only abort; the
    /// confirmation is a property of having a human, not of the machine that
    /// does the work.
    Confirm(&'static str),
    /// Sent with `-d`, then the terminal is handed to the far end's tmux —
    /// `q resume`, which is `q new --machine` with an existing Quest.
    ThenEnter,
    /// Never proxied. The reason is the message.
    Refuse(&'static str),
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
            if *follow { Refuse(FOLLOW) } else { Proxy },
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
        } => (Aim::Quest(quest), Refuse(ARTIFACT)),
        Command::Phase {
            quest: Some(quest), ..
        } => (Aim::Quest(quest), Refuse(PHASE)),

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

    match passage {
        Passage::Refuse(why) => {
            Err(QError::Other(format!("{} runs on {machine}: {why}", found.quest.slug)).into())
        }
        Passage::Proxy => send(ctx, &machine, &[], false).map(|(code, _)| Some(code)),
        Passage::Confirm(verb) => {
            confirm(ctx, &format!("{verb} {} on {machine}?", found.quest.slug))?;
            send(ctx, &machine, &["-f"], false).map(|(code, _)| Some(code))
        }
        // `-d` because there is no terminal at the far end to attach to; the
        // attach is this machine's, once the Quest is back up over there.
        Passage::ThenEnter => {
            // Under `--json` the far end's document is held back rather than
            // relayed: the attach is part of the same answer, and two JSON
            // documents on one stdout is not `--json`.
            let (code, held) = send(ctx, &machine, &["-d"], ctx.json)?;
            if code != 0 {
                write_out(std::io::stdout(), &held)?;
                return Ok(Some(code));
            }
            enter_after(ctx, &found, &machine, held).map(|()| Some(0))
        }
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

/// This invocation, over ssh to `machine`, with `extra` appended before the
/// recursion guard. Relays the far end's streams and hands back its exit code —
/// and, under `hold_stdout`, its stdout instead of writing it, for the one
/// caller that has something to add to the same answer ([`enter_after`]).
fn send(
    ctx: &Ctx,
    machine: &str,
    extra: &[&str],
    hold_stdout: bool,
) -> anyhow::Result<(u8, String)> {
    let remote = remote::find(&ctx.config.remotes, machine)?;
    let argv = forward(&raw_args(), ctx.machine_filter(), extra);
    relay(ctx, remote, &argv, hold_stdout)
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
) -> anyhow::Result<(u8, String)> {
    let borrowed: Vec<&str> = argv.iter().map(String::as_str).collect();
    let outcome = ctx.ssh().run(&remote.ssh, &borrowed, remote::PROXY_TIMEOUT);
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
            remote::PROXY_TIMEOUT.as_secs(),
            remote.name
        ))
        .into()),
        SshOutcome::TooLarge => Err(QError::Other(format!(
            "{} sent more output than `q` will relay; run it there",
            remote.name
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
pub fn forward(raw: &[String], machine: Option<&str>, extra: &[&str]) -> Vec<String> {
    debug_assert!(
        !raw.iter().any(|a| a == NO_REMOTE),
        "a --no-remote invocation must never reach the proxy"
    );
    let mut out = vec![REMOTE_Q.to_string()];
    let joined = machine.map(|m| format!("--machine={m}"));
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
    out.extend(extra.iter().map(|e| (*e).to_string()));
    out.push(NO_REMOTE.to_string());
    out
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
            return Err(QError::Other(format!("{} sent an unreadable answer", remote.name)).into());
        }
        SshOutcome::Failed(e) => {
            return Err(QError::Other(format!("cannot reach {}: {e}", remote.name)).into());
        }
    };
    let payload: serde_json::Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        QError::Other(format!(
            "cannot read `q new --json` from {}: {e}",
            remote.name
        ))
    })?;
    let field = |key: &str, at: &serde_json::Value| -> anyhow::Result<String> {
        at.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| {
                QError::Other(format!(
                    "`q new --json` from {} has no `{key}`",
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

fn create_failed(machine: &str, code: Option<i32>, stderr: &str) -> anyhow::Error {
    if code == Some(SSH_FAILED) || code.is_none() {
        return unreachable_remote(machine, code, stderr);
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
        assert_eq!(passage("events alpha --follow"), Passage::Refuse(FOLLOW));
        assert_eq!(passage("events alpha"), Passage::Proxy);
        assert_eq!(
            passage("artifact add /p --quest alpha"),
            Passage::Refuse(ARTIFACT)
        );
        assert_eq!(passage("phase hi --quest alpha"), Passage::Refuse(PHASE));
        assert_eq!(passage("resume alpha"), Passage::ThenEnter);
        assert_eq!(passage("resume alpha -d"), Passage::Proxy);
        assert_eq!(passage("close alpha"), Passage::Confirm("close quest"));
        assert_eq!(passage("close alpha -f"), Passage::Proxy);
        assert_eq!(passage("kill alpha/t"), Passage::Confirm("kill session"));
        assert_eq!(passage("kill alpha/t -f"), Passage::Proxy);
        assert!(matches!(passage("rm alpha"), Passage::Confirm(_)));
        assert_eq!(passage("rm alpha -f"), Passage::Proxy);
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
