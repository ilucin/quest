//! `q enter` — attach to a Quest's tmux session, master window first (SPEC §6).

use std::io::Write;

use crate::Ctx;
use crate::commands::new::MASTER;
use crate::commands::{attach_mode, flush_warnings, live, sweep_quiet};
use crate::config::Remote;
use crate::error::QError;
use crate::model::{Quest, QuestState, Session};
use crate::output;
use crate::remote::{self, RemoteResult};
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
/// The session name is built from *this* machine's `[tmux] session_prefix`.
/// The far end's prefix is not knowable from a listing row, and SPEC §15 names
/// the target literally as `q-<slug>`.
pub fn remote_target(ctx: &Ctx, remote: &Remote, slug: &str) -> RemoteTarget {
    let tmux_session = session_name(&ctx.config, slug);
    let mut argv = vec!["tmux".to_string()];
    if ctx.config.tmux.iterm_cc && !ctx.tmux().in_tmux() {
        argv.push("-CC".to_string());
    }
    argv.push("attach".to_string());
    argv.push("-t".to_string());
    argv.push(remote::sh_quote(&crate::tmux::exact(&tmux_session)));
    RemoteTarget {
        machine: remote.name.clone(),
        alias: remote.ssh.clone(),
        tmux_session,
        argv,
    }
}

/// The Quest `target` names among the rows the remotes sent, by the same rule
/// [`crate::db::Db::resolve_quest`] uses locally: exact id, exact slug, then
/// prefix, then substring, with more than one match an error rather than a
/// guess.
fn resolve_remote<'a>(
    results: &'a [RemoteResult],
    target: &str,
) -> anyhow::Result<(&'a str, &'a Quest)> {
    let all: Vec<(&str, &Quest)> = results
        .iter()
        .flat_map(|r| {
            r.quests
                .iter()
                .map(move |q| (r.name.as_str(), &q.view.quest))
        })
        .collect();
    if let Some(hit) = all.iter().find(|(_, q)| q.id == target) {
        return Ok(*hit);
    }
    if let Some(hit) = all.iter().find(|(_, q)| q.slug == target) {
        return Ok(*hit);
    }
    for rule in [
        |q: &Quest, t: &str| q.id.starts_with(t) || q.slug.starts_with(t),
        |q: &Quest, t: &str| q.id.contains(t) || q.slug.contains(t),
    ] {
        let matches: Vec<(&str, &Quest)> = all
            .iter()
            .copied()
            .filter(|(_, q)| rule(q, target))
            .collect();
        match matches.len() {
            0 => continue,
            1 => return Ok(matches[0]),
            _ => {
                return Err(QError::Ambiguous {
                    target: target.to_string(),
                    candidates: matches
                        .into_iter()
                        .map(|(machine, q)| format!("{} ({}) on {machine}", q.id, q.slug))
                        .collect(),
                }
                .into());
            }
        }
    }
    Err(QError::NotFound(format!("quest `{target}`")).into())
}

/// Whether an error is "no such Quest here" — the one local failure that is
/// worth asking the other machines about.
fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<QError>().map(QError::code) == Some("not_found")
}

/// `q enter` on a Quest that is not in this database: ask the remotes, and
/// hand the terminal to the one that has it (SPEC §15).
///
/// `local` is the error the local lookup gave, returned unchanged when there
/// are no remotes to ask or none of them has it — a `q enter typo` must read
/// as a typo, not as a report about ssh.
fn enter_remote(
    ctx: &Ctx,
    target: &str,
    label: Option<&str>,
    local: anyhow::Error,
) -> anyhow::Result<()> {
    if !ctx.remote_enabled() || remote::targets(ctx).is_empty() {
        return Err(local);
    }
    // `--all`: a Quest that is finished over there has to be *found* before it
    // can be refused, exactly as a local one is.
    let results = remote::fetch_all(ctx, true, None);
    remote::warn_unreachable(ctx, &results);
    flush_warnings(ctx);

    let (machine, quest) = match resolve_remote(&results, target) {
        Ok(found) => found,
        // Nothing matched anywhere: the local error is still the true one.
        Err(e) if is_not_found(&e) => return Err(local),
        Err(e) => return Err(e),
    };
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished on {machine}; run `q resume {}` there",
            quest.slug, quest.slug
        ))
        .into());
    }
    // SPEC §15's remote attach is the tmux session, not a window inside it:
    // picking a window needs that machine's session rows, which is the
    // proxying bd-8lz.5.3 adds.
    if let Some(label) = label {
        return Err(QError::Other(format!(
            "--session {label} is not supported on {machine} yet;              `q enter {}` lands in its master",
            quest.slug
        ))
        .into());
    }
    let remote = remote::find(&ctx.config.remotes, machine)?;
    let found = remote_target(ctx, remote, &quest.slug);

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "machine": found.machine,
                "ssh": found.alias,
                "tmux_session": found.tmux_session,
                "remote": true,
                "argv": found.argv,
            }),
            || {
                format!(
                    "attaching to {}:{} over ssh",
                    found.machine, found.tmux_session
                )
            },
        )?;
    }
    // The attach replaces this process, so nothing buffered survives it.
    std::io::stdout().flush()?;
    ctx.ssh().attach(&found.alias, &found.argv)
}

pub fn run(ctx: &Ctx, target: &str, label: Option<&str>) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    // Local first: a Quest this machine runs is entered without ever dialling
    // out, and a name that is ambiguous *here* is a mistake to report rather
    // than a reason to widen the search.
    let quest = match db.resolve_quest(target) {
        Ok(quest) => quest,
        Err(e) if is_not_found(&e) => return enter_remote(ctx, target, label, e),
        Err(e) => return Err(e),
    };
    let found = resolve(ctx, &quest, label)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::QuestView;
    use crate::config::Config;
    use crate::db::Db;
    use crate::remote::{RemoteQuest, RemoteStatus};

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

    /// SPEC §15: `ssh -t <alias> tmux attach -t q-<slug>`.
    #[test]
    fn the_remote_attach_is_the_spec_command() {
        let (ctx, _dir) = with_tmux(false, false);
        let target = remote_target(&ctx, &ws(), "alpha");
        assert_eq!(target.machine, "ws");
        assert_eq!(target.alias, "ws-host");
        assert_eq!(target.tmux_session, "q-alpha");
        // `=` for the same reason the local attach uses it: without it tmux
        // matches `-t` by prefix and `q-a` would resolve to `q-alpha`.
        assert_eq!(target.argv, ["tmux", "attach", "-t", "=q-alpha"]);
        assert_eq!(
            crate::remote::attach_argv(&target.alias, &target.argv),
            ["-t", "ws-host", "tmux", "attach", "-t", "=q-alpha"]
        );
    }

    /// `[tmux] iterm_cc` — and only outside tmux, because a control-mode
    /// client inside a tmux session is a nested one iTerm2 cannot host.
    #[test]
    fn iterm_control_mode_is_asked_for_only_outside_tmux() {
        let (ctx, _dir) = with_tmux(true, false);
        assert_eq!(
            remote_target(&ctx, &ws(), "alpha").argv,
            ["tmux", "-CC", "attach", "-t", "=q-alpha"]
        );

        let (ctx, _dir) = with_tmux(true, true);
        assert_eq!(
            remote_target(&ctx, &ws(), "alpha").argv,
            ["tmux", "attach", "-t", "=q-alpha"]
        );
    }

    /// The tmux target is one shell word on the far end, whatever
    /// `[tmux] session_prefix` is.
    #[test]
    fn a_session_prefix_with_a_space_still_arrives_as_one_argument() {
        let (mut ctx, _dir) = with_tmux(false, false);
        ctx.config.tmux.session_prefix = "my quests/".to_string();
        let target = remote_target(&ctx, &ws(), "alpha");
        assert_eq!(target.argv.last().unwrap(), "'=my quests/alpha'");
    }

    fn result(name: &str, slugs: &[&str]) -> RemoteResult {
        RemoteResult {
            name: name.to_string(),
            ssh: format!("{name}-host"),
            status: RemoteStatus::Ok,
            quests: slugs
                .iter()
                .map(|slug| {
                    let view = QuestView::new(Quest::new(slug, "/tmp", name), &[]);
                    let raw = serde_json::to_value(&view).unwrap();
                    RemoteQuest { view, raw }
                })
                .collect(),
            stale: false,
            fetched_at: Some(1),
        }
    }

    #[test]
    fn a_remote_quest_is_resolved_by_the_same_rule_as_a_local_one() {
        let results = [result("ws", &["cdc-backfill"]), result("box", &["other"])];
        let id = results[0].quests[0].view.quest.id.clone();

        assert_eq!(resolve_remote(&results, "cdc-backfill").unwrap().0, "ws");
        assert_eq!(resolve_remote(&results, &id).unwrap().0, "ws");
        // Prefix, then substring.
        assert_eq!(
            resolve_remote(&results, "cdc").unwrap().1.slug,
            "cdc-backfill"
        );
        assert_eq!(
            resolve_remote(&results, "backfill").unwrap().1.slug,
            "cdc-backfill"
        );
        assert_eq!(resolve_remote(&results, "oth").unwrap().0, "box");

        let e = resolve_remote(&results, "nope").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found")
        );
    }

    /// Two machines can hold Quests whose names both match; guessing between
    /// them is exactly what the local resolver refuses to do.
    #[test]
    fn a_fragment_that_matches_on_two_machines_names_both() {
        let results = [result("ws", &["cdc-one"]), result("box", &["cdc-two"])];
        let e = resolve_remote(&results, "cdc").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
        let said = e.to_string();
        assert!(said.contains("on ws") && said.contains("on box"), "{said}");
    }
}
