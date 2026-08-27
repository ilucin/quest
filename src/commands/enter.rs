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
pub fn remote_target(
    ctx: &Ctx,
    remote: &Remote,
    slug: &str,
    tmux_prefix: Option<&str>,
) -> RemoteTarget {
    let prefix = tmux_prefix.unwrap_or(remote::DEFAULT_TMUX_PREFIX);
    let tmux_session = format!("{prefix}{slug}");
    let mut argv = vec!["tmux".to_string()];
    if ctx.config.tmux.iterm_cc && !ctx.tmux().in_tmux() {
        argv.push("-CC".to_string());
    }
    argv.push("attach".to_string());
    argv.push("-t".to_string());
    argv.push(crate::tmux::exact(&tmux_session));
    RemoteTarget {
        machine: remote.name.clone(),
        alias: remote.ssh.clone(),
        tmux_session,
        argv,
    }
}

/// A Quest a `q enter` target could mean, and the machine it runs on.
#[derive(Debug, Clone, Copy)]
struct Candidate<'a> {
    quest: &'a Quest,
    /// `None` for this machine.
    machine: Option<&'a str>,
    /// That machine's tmux prefix; see [`remote_target`].
    tmux_prefix: Option<&'a str>,
}

impl Candidate<'_> {
    /// How the Quest is named when more than one matched.
    fn label(&self) -> String {
        let head = format!("{} ({})", self.quest.id, self.quest.slug);
        match self.machine {
            Some(machine) => format!("{head} on {machine}"),
            None => head,
        }
    }
}

/// The exact rungs of SPEC §16 against this machine's Quests: the one lookup
/// worth doing before any ssh.
///
/// A Quest whose id or slug is *exactly* what was typed is not a guess, so it
/// is entered without dialling out. Everything below exact is a guess, which is
/// why the fuzzy rungs wait for the remotes (see [`resolve_target`]).
fn exact_local<'a>(local: &'a [Quest], target: &str) -> Option<&'a Quest> {
    local
        .iter()
        .find(|q| q.id == target)
        .or_else(|| local.iter().find(|q| q.slug == target))
}

/// SPEC §16 target resolution across **every** machine in the listing: exact
/// id, exact slug, then unique prefix, then unique substring, with more than
/// one match at a rung an error rather than a guess.
///
/// The rungs are walked across the whole candidate set rather than machine by
/// machine. Running the local ladder to exhaustion first would let a local
/// *prefix* hit shadow a remote *exact slug* hit — the user types the name of a
/// Quest that exists and lands in a different Quest on a different machine,
/// with nothing said. Ambiguity across machines is reported the same way
/// ambiguity on one is: as a list of what matched, with where.
fn resolve_target<'a>(
    local: &'a [Quest],
    results: &'a [RemoteResult],
    target: &str,
) -> anyhow::Result<Candidate<'a>> {
    if target.is_empty() {
        return Err(QError::NotFound("quest ``".to_string()).into());
    }
    // Local first, so the order candidates are listed in is this machine's
    // Quests then the remotes in config order.
    let mut all: Vec<Candidate<'a>> = local
        .iter()
        .map(|quest| Candidate {
            quest,
            machine: None,
            tmux_prefix: None,
        })
        .collect();
    all.extend(results.iter().flat_map(|r| {
        r.quests.iter().map(move |q| Candidate {
            quest: &q.view.quest,
            machine: Some(r.name.as_str()),
            tmux_prefix: r.tmux_prefix.as_deref(),
        })
    }));

    let ladder: [fn(&Quest, &str) -> bool; 4] = [
        |q, t| q.id == t,
        |q, t| q.slug == t,
        |q, t| q.id.starts_with(t) || q.slug.starts_with(t),
        |q, t| q.id.contains(t) || q.slug.contains(t),
    ];
    for rule in ladder {
        let matches: Vec<Candidate<'a>> = all
            .iter()
            .copied()
            .filter(|c| rule(c.quest, target))
            .collect();
        match matches.len() {
            0 => continue,
            1 => return Ok(matches[0]),
            _ => {
                return Err(QError::Ambiguous {
                    target: target.to_string(),
                    candidates: matches.iter().map(Candidate::label).collect(),
                }
                .into());
            }
        }
    }
    Err(QError::NotFound(format!("quest `{target}`")).into())
}

/// Hand the terminal to the machine `found` runs on (SPEC §15).
fn enter_remote(
    ctx: &Ctx,
    found: Candidate<'_>,
    machine: &str,
    label: Option<&str>,
) -> anyhow::Result<()> {
    let quest = found.quest;
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
            "--session {label} is not supported on {machine} yet; `q enter {}` lands in its master",
            quest.slug
        ))
        .into());
    }
    let remote = remote::find(&ctx.config.remotes, machine)?;
    let target = remote_target(ctx, remote, &quest.slug, found.tmux_prefix);

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "machine": target.machine,
                "ssh": target.alias,
                "tmux_session": target.tmux_session,
                "remote": true,
                "argv": target.argv,
            }),
            || {
                format!(
                    "attaching to {}:{} over ssh",
                    target.machine, target.tmux_session
                )
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
    let local = ctx.db()?.list_quests(true)?;
    // An exact hit here is entered without ever dialling out.
    if let Some(quest) = exact_local(&local, target) {
        return enter_local(ctx, quest, label);
    }
    // Anything less than exact has to be held against the other machines
    // before it is acted on: a local *fragment* match must not shadow a Quest
    // whose name is exactly what was typed, wherever that Quest runs.
    let results = if ctx.remote_enabled() && !remote::targets(ctx).is_empty() {
        // `--all`: a Quest that is finished over there has to be *found*
        // before it can be refused, exactly as a local one is.
        let results = remote::fetch_all(ctx, true, None);
        remote::warn_unreachable(ctx, &results);
        flush_warnings(ctx);
        results
    } else {
        Vec::new()
    };

    let found = resolve_target(&local, &results, target)?;
    match found.machine {
        Some(machine) => enter_remote(ctx, found, machine, label),
        None => enter_local(ctx, found.quest, label),
    }
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
        let target = remote_target(&ctx, &ws(), "alpha", None);
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
        let target = remote_target(&ctx, &ws(), "alpha", None);
        assert_eq!(target.tmux_session, "q-alpha");
        // What the far end reported does.
        let target = remote_target(&ctx, &ws(), "alpha", Some("work_"));
        assert_eq!(target.tmux_session, "work_alpha");
        assert_eq!(target.argv.last().unwrap(), "=work_alpha");
    }

    /// `[tmux] iterm_cc` — and only outside tmux, because a control-mode
    /// client inside a tmux session is a nested one iTerm2 cannot host.
    #[test]
    fn iterm_control_mode_is_asked_for_only_outside_tmux() {
        let (ctx, _dir) = with_tmux(true, false);
        assert_eq!(
            remote_target(&ctx, &ws(), "alpha", None).argv,
            ["tmux", "-CC", "attach", "-t", "=q-alpha"]
        );

        let (ctx, _dir) = with_tmux(true, true);
        assert_eq!(
            remote_target(&ctx, &ws(), "alpha", None).argv,
            ["tmux", "attach", "-t", "=q-alpha"]
        );
    }

    /// A far-end prefix with a space is still one shell word when ssh sends it
    /// — and the argv `--json` shows is the unquoted one.
    #[test]
    fn a_session_prefix_with_a_space_still_arrives_as_one_argument() {
        let (ctx, _dir) = with_tmux(false, false);
        let target = remote_target(&ctx, &ws(), "alpha", Some("my quests/"));
        assert_eq!(target.argv.last().unwrap(), "=my quests/alpha");
        assert_eq!(
            crate::remote::attach_argv(&target.alias, &target.argv)
                .last()
                .unwrap(),
            "'=my quests/alpha'"
        );
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
            tmux_prefix: Some("q-".to_string()),
        }
    }

    fn machine_of<'a>(found: &Candidate<'a>) -> &'a str {
        found.machine.unwrap_or("<local>")
    }

    #[test]
    fn a_remote_quest_is_resolved_by_the_same_rule_as_a_local_one() {
        let results = [result("ws", &["cdc-backfill"]), result("box", &["other"])];
        let id = results[0].quests[0].view.quest.id.clone();

        assert_eq!(
            machine_of(&resolve_target(&[], &results, "cdc-backfill").unwrap()),
            "ws"
        );
        assert_eq!(
            machine_of(&resolve_target(&[], &results, &id).unwrap()),
            "ws"
        );
        // Prefix, then substring.
        assert_eq!(
            resolve_target(&[], &results, "cdc").unwrap().quest.slug,
            "cdc-backfill"
        );
        assert_eq!(
            resolve_target(&[], &results, "backfill")
                .unwrap()
                .quest
                .slug,
            "cdc-backfill"
        );
        assert_eq!(
            machine_of(&resolve_target(&[], &results, "oth").unwrap()),
            "box"
        );

        for empty in ["", "nope"] {
            let e = resolve_target(&[], &results, empty).unwrap_err();
            assert_eq!(
                e.downcast_ref::<QError>().map(QError::code),
                Some("not_found"),
                "{empty}"
            );
        }
    }

    /// Two machines can hold Quests whose names both match; guessing between
    /// them is exactly what the local resolver refuses to do.
    #[test]
    fn a_fragment_that_matches_on_two_machines_names_both() {
        let results = [result("ws", &["cdc-one"]), result("box", &["cdc-two"])];
        let e = resolve_target(&[], &results, "cdc").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
        let said = e.to_string();
        assert!(said.contains("on ws") && said.contains("on box"), "{said}");
    }

    /// The ladder is walked across machines, not machine by machine: a local
    /// *prefix* hit must not shadow a remote Quest whose slug is exactly what
    /// was typed.
    #[test]
    fn an_exact_slug_anywhere_beats_a_local_fragment() {
        let local = [Quest::new("cdc-backfill-v2", "/tmp", "laptop")];
        let results = [result("ws", &["cdc-backfill"])];

        // `exact_local` is what `run` checks before any ssh: the local Quest
        // is only a prefix match, so it does not qualify.
        assert!(exact_local(&local, "cdc-backfill").is_none());
        assert_eq!(
            exact_local(&local, "cdc-backfill-v2").map(|q| q.slug.as_str()),
            Some("cdc-backfill-v2")
        );

        let found = resolve_target(&local, &results, "cdc-backfill").unwrap();
        assert_eq!(found.quest.slug, "cdc-backfill");
        assert_eq!(machine_of(&found), "ws");

        // A fragment that matches on both is ambiguous rather than local-wins.
        let e = resolve_target(&local, &results, "cdc-back").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
        let said = e.to_string();
        assert!(said.contains("on ws"), "{said}");
        // The local candidate is named without a machine suffix, exactly as
        // `db.resolve_quest` names it.
        assert!(said.contains("(cdc-backfill-v2)"), "{said}");
    }

    /// With no remotes in the picture the ladder is the local one, unchanged.
    #[test]
    fn without_remotes_the_ladder_is_purely_local() {
        let local = [
            Quest::new("alpha", "/tmp", "laptop"),
            Quest::new("alpine", "/tmp", "laptop"),
        ];
        assert_eq!(
            resolve_target(&local, &[], "alpha").unwrap().quest.slug,
            "alpha"
        );
        assert!(
            resolve_target(&local, &[], "alpha")
                .unwrap()
                .machine
                .is_none()
        );
        assert_eq!(
            resolve_target(&local, &[], "alpi").unwrap().quest.slug,
            "alpine"
        );
        let e = resolve_target(&local, &[], "alp").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
    }
}
