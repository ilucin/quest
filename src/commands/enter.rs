//! `q enter` — attach to a Quest's tmux session, master window first (SPEC §6).

use std::io::Write;

use crate::Ctx;
use crate::commands::locate::{self, Located};
use crate::commands::new::MASTER;
use crate::commands::{attach_mode, flush_warnings, live, sweep_quiet};
use crate::config::Remote;
use crate::error::QError;
use crate::model::{Quest, QuestState, Session};
use crate::output;
use crate::remote;
use crate::tmux::{session_name, window_of};

/// Where an attach would land. Every error building one is a reason not to
/// attach at all, which is why the TUI (bd-8lz.4.3) resolves through here
/// rather than growing a second attach path.
pub struct Target {
    pub tmux_session: String,
    /// The `q` session the attach lands in — proven live by `resolve`.
    pub session: Session,
    /// The pane that *is* the session's identity (SPEC §6).
    pub pane: String,
    /// What tmux calls that pane's window, for the message; falls back to the
    /// `q` session label when tmux cannot say.
    pub window: String,
}

/// The tmux session and pane `label` (default: the master) names in `quest`.
pub fn resolve(ctx: &Ctx, quest: &Quest, label: Option<&str>) -> anyhow::Result<Target> {
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished; run `q resume {}`",
            quest.slug, quest.slug
        ))
        .into());
    }
    let tmux_session = session_name(&ctx.config, &quest.slug);
    if !ctx.tmux().has_session(&tmux_session)? {
        return Err(QError::Tmux(format!(
            "no tmux session `{tmux_session}`; run `q resume {}`",
            quest.slug
        ))
        .into());
    }

    let sessions = ctx.db()?.list_sessions_by_quest(&quest.id)?;
    let wanted = label.unwrap_or(MASTER);
    let session = match live(&sessions).find(|s| s.label == wanted) {
        Some(session) => session,
        // The tmux session can outlive its master window; attaching would then
        // land on whatever window is left instead of the Quest's master.
        None if label.is_none() => {
            return Err(QError::Other(format!(
                "master session of {} ended; run `q resume {}`",
                quest.slug, quest.slug
            ))
            .into());
        }
        None => {
            let known: Vec<&str> = live(&sessions).map(|s| s.label.as_str()).collect();
            let live_labels = if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            };
            return Err(QError::NotFound(format!(
                "session `{wanted}` in quest {} (live: {live_labels})",
                quest.slug
            ))
            .into());
        }
    };
    // A row inserted by a spawn that then died has no pane. tmux reads an
    // empty target as "whatever is active", so entering it would land on the
    // master while claiming to be the worker. The sweep ends such a row a few
    // seconds in; until then, say so.
    if session.tmux_pane.is_empty() {
        return Err(QError::Other(format!(
            "session `{wanted}` of {} has no pane yet; it never finished starting",
            quest.slug
        ))
        .into());
    }
    // The pane is the session's identity (SPEC §6); the window name is only
    // ever reported, and tmux is the one that knows it.
    let pane = session.tmux_pane.clone();
    let window = window_of(ctx.tmux(), &pane).unwrap_or_else(|| session.label.clone());
    Ok(Target {
        tmux_session,
        session: session.clone(),
        pane,
        window,
    })
}

/// Where a remote attach would land (SPEC §15). Built here rather than at the
/// call site so `q enter` and the TUI hand ssh the same command line.
pub struct RemoteTarget {
    pub machine: String,
    pub alias: String,
    pub tmux_session: String,
    /// The command run on the far end, after `ssh -t <alias>`.
    pub argv: Vec<String>,
}

/// SPEC §15: `ssh -t <alias> tmux attach -t q-<slug>`, or `tmux -CC` under
/// `[tmux] iterm_cc` when we are not already inside tmux.
///
/// `-CC` is iTerm2's control mode, and it is conditional for a reason tmux
/// enforces itself: a control-mode client inside a tmux session is a nested
/// one, which iTerm2 cannot host — so inside tmux the plain attach is not a
/// fallback but the only thing that works.
///
/// `tmux_prefix` is the **far end's** `[tmux] session_prefix`, off its own
/// `machines` entry (see [`crate::remote::Listing`]). This machine's prefix is
/// not it: the two are independent config files, and a laptop set to `quest-`
/// would otherwise attach to a session name that does not exist on a
/// workstation still using `q-`. SPEC §15's literal `q-<slug>` is the fallback
/// for a remote whose `q` is too old to report one.
///
/// The argv is the command as `q` means it, unquoted: quoting is what
/// [`crate::remote::attach_argv`] does on the way into ssh, so what `--json`
/// shows here is an argv a consumer can read.
/// `label` picks a window inside that tmux session, and it cannot be done from
/// here: the pane that *is* the session's identity (SPEC §6) lives in the far
/// end's database, and this machine has none of its session rows. So that case
/// runs the far end's **own** `q enter` over ssh — the generic rule of SPEC §15
/// applied to an attach rather than to a captured command — and that `q`
/// resolves the label, picks the pane and attaches, exactly as it would for
/// someone sitting at that machine. `--no-remote` goes with it, so the far end
/// cannot bounce the attach onwards.
///
/// That line is pinned exactly as [`crate::commands::proxy`] pins the ones it
/// forwards, and for the same reason: the target travels as the Quest **id**
/// this machine resolved, and the identity it resolved to travels with it
/// (`--expect`), so a far end whose picture has moved on — a rename, or an id
/// drawn again after a delete — refuses rather than handing the terminal to an
/// agent in some other Quest.
///
/// The trade is that `[tmux] iterm_cc` is then *that* machine's setting rather
/// than this one's, which is the honest reading of it: the `q` that runs the
/// attach is the one whose config decides how it attaches.
pub fn remote_target(
    ctx: &Ctx,
    remote: &Remote,
    quest: &Quest,
    tmux_prefix: Option<&str>,
    label: Option<&str>,
) -> RemoteTarget {
    use crate::commands::proxy;
    let prefix = tmux_prefix.unwrap_or(remote::DEFAULT_TMUX_PREFIX);
    let tmux_session = format!("{prefix}{}", quest.slug);
    let argv = match label {
        None => attach_command(ctx, &tmux_session),
        Some(label) => vec![
            proxy::REMOTE_Q.to_string(),
            "enter".to_string(),
            quest.id.clone(),
            "--session".to_string(),
            label.to_string(),
            proxy::EXPECT.to_string(),
            proxy::identity(quest),
            proxy::NO_REMOTE.to_string(),
        ],
    };
    RemoteTarget {
        machine: remote.name.clone(),
        alias: remote.ssh.clone(),
        tmux_session,
        argv,
    }
}

/// The far end's `tmux attach` for a session name that is already known — what
/// [`remote_target`] derives from a slug, and what `q new --machine` uses
/// instead, since the machine that created the Quest reported the session name
/// itself and nothing here has to guess a prefix.
pub fn attach_command(ctx: &Ctx, tmux_session: &str) -> Vec<String> {
    let mut argv = vec!["tmux".to_string()];
    if ctx.config.tmux.iterm_cc && !ctx.tmux().in_tmux() {
        argv.push("-CC".to_string());
    }
    argv.push("attach".to_string());
    argv.push("-t".to_string());
    argv.push(crate::tmux::exact(tmux_session));
    argv
}

/// Hand the terminal to the machine `found` runs on (SPEC §15).
fn enter_remote(
    ctx: &Ctx,
    found: &Located,
    machine: &str,
    label: Option<&str>,
) -> anyhow::Result<()> {
    let quest = &found.quest;
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished on {machine}; run `q resume {}` there",
            quest.slug, quest.slug
        ))
        .into());
    }
    let remote = remote::find(&ctx.config.remotes, machine)?;
    let target = remote_target(ctx, remote, quest, found.tmux_prefix.as_deref(), label);

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "machine": target.machine,
                "ssh": target.alias,
                "tmux_session": target.tmux_session,
                "session": label,
                "remote": true,
                "argv": target.argv,
            }),
            || match label {
                Some(label) => format!(
                    "attaching to {}:{} ({label}) over ssh",
                    target.machine, target.tmux_session
                ),
                None => format!(
                    "attaching to {}:{} over ssh",
                    target.machine, target.tmux_session
                ),
            },
        )?;
    }
    // The attach replaces this process, so nothing buffered survives it.
    std::io::stdout().flush()?;
    ctx.ssh().attach(&target.alias, &target.argv)
}

/// Attach to a Quest in this machine's own tmux.
fn enter_local(ctx: &Ctx, quest: &Quest, label: Option<&str>) -> anyhow::Result<()> {
    let found = resolve(ctx, quest, label)?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": found.session,
                "tmux_session": found.tmux_session,
                "window": found.window,
                "attach": attach_mode(ctx, true),
            }),
            || format!("attaching to {}:{}", found.tmux_session, found.window),
        )?;
    }
    // A real attach replaces this process, so nothing buffered survives it.
    std::io::stdout().flush()?;
    ctx.tmux().attach(&found.tmux_session, Some(&found.pane))
}

pub fn run(ctx: &Ctx, target: &str, label: Option<&str>) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    // `--machine` scopes `q enter` exactly as it scopes `q list`: it says
    // *which machine's* Quests are candidates. Resolution — including the
    // cache-first shortcut that keeps the everyday local `q enter` a single
    // database read — is [`locate::quest`]'s, shared with the generic proxy.
    let found = locate::quest(ctx, target);
    flush_warnings(ctx);
    let found = found?;
    match found.machine.clone() {
        Some(machine) => enter_remote(ctx, &found, &machine, label),
        None => enter_local(ctx, &found.quest, label),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;

    /// A `Ctx` whose tmux is a fixture that answers `in_tmux` the way the test
    /// asks — the one thing `-CC` turns on.
    fn with_tmux(iterm_cc: bool, inside_tmux: bool) -> (Ctx, tempfile::TempDir) {
        let mut config = Config::default();
        config.machine.name = "laptop".to_string();
        config.tmux.iterm_cc = iterm_cc;
        config.remotes = vec![Remote {
            name: "ws".to_string(),
            ssh: "ws-host".to_string(),
        }];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux.json");
        let state = crate::tmux::FixtureState {
            in_tmux: Some(inside_tmux),
            ..Default::default()
        };
        std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
        let tmux = Box::new(crate::tmux::FixtureTmux::new(path));
        (
            Ctx::for_tests(config, Db::open_in_memory().unwrap(), tmux),
            dir,
        )
    }

    fn ws() -> Remote {
        Remote {
            name: "ws".to_string(),
            ssh: "ws-host".to_string(),
        }
    }

    fn alpha() -> Quest {
        Quest::new("alpha", "/tmp", "ws")
    }

    /// SPEC §15: `ssh -t <alias> tmux attach -t q-<slug>`.
    #[test]
    fn the_remote_attach_is_the_spec_command() {
        let (ctx, _dir) = with_tmux(false, false);
        let target = remote_target(&ctx, &ws(), &alpha(), None, None);
        assert_eq!(target.machine, "ws");
        assert_eq!(target.alias, "ws-host");
        assert_eq!(target.tmux_session, "q-alpha");
        // `=` for the same reason the local attach uses it: without it tmux
        // matches `-t` by prefix and `q-a` would resolve to `q-alpha`.
        assert_eq!(target.argv, ["tmux", "attach", "-t", "=q-alpha"]);
        // …and it is quoted on the way into ssh, because the far end's login
        // shell sees the words before tmux does: zsh reads a bare `=q-alpha`
        // as an equals expansion and never runs tmux at all.
        assert_eq!(
            crate::remote::attach_argv(&target.alias, &target.argv),
            ["-t", "ws-host", "tmux", "attach", "-t", "'=q-alpha'"]
        );
    }

    /// The session name belongs to the machine that runs the Quest: SPEC §15's
    /// `q-` only until that machine says otherwise.
    #[test]
    fn the_tmux_prefix_is_the_far_ends_not_this_machines() {
        let (mut ctx, _dir) = with_tmux(false, false);
        ctx.config.tmux.session_prefix = "quest-".to_string();
        // This machine's prefix does not reach across the wire.
        let target = remote_target(&ctx, &ws(), &alpha(), None, None);
        assert_eq!(target.tmux_session, "q-alpha");
        // What the far end reported does.
        let target = remote_target(&ctx, &ws(), &alpha(), Some("work_"), None);
        assert_eq!(target.tmux_session, "work_alpha");
        assert_eq!(target.argv.last().unwrap(), "=work_alpha");
    }

    /// `[tmux] iterm_cc` — and only outside tmux, because a control-mode
    /// client inside a tmux session is a nested one iTerm2 cannot host.
    #[test]
    fn iterm_control_mode_is_asked_for_only_outside_tmux() {
        let (ctx, _dir) = with_tmux(true, false);
        assert_eq!(
            remote_target(&ctx, &ws(), &alpha(), None, None).argv,
            ["tmux", "-CC", "attach", "-t", "=q-alpha"]
        );

        let (ctx, _dir) = with_tmux(true, true);
        assert_eq!(
            remote_target(&ctx, &ws(), &alpha(), None, None).argv,
            ["tmux", "attach", "-t", "=q-alpha"]
        );
    }

    /// A far-end prefix with a space is still one shell word when ssh sends it
    /// — and the argv `--json` shows is the unquoted one.
    #[test]
    fn a_session_prefix_with_a_space_still_arrives_as_one_argument() {
        let (ctx, _dir) = with_tmux(false, false);
        let target = remote_target(&ctx, &ws(), &alpha(), Some("my quests/"), None);
        assert_eq!(target.argv.last().unwrap(), "=my quests/alpha");
        assert_eq!(
            crate::remote::attach_argv(&target.alias, &target.argv)
                .last()
                .unwrap(),
            "'=my quests/alpha'"
        );
    }
}
