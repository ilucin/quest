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
    ///
    /// `local` is this machine's name, and it is spelled out rather than left
    /// implicit: `q-bd5f (alpha), q-82b8 (alpha) on ws` reads as though `on ws`
    /// covered the whole list. Every candidate carrying its machine is the only
    /// version of that line nobody has to parse twice.
    fn label(&self, local: Option<&str>) -> String {
        let head = format!("{} ({})", self.quest.id, self.quest.slug);
        match (self.machine, local) {
            (Some(machine), _) => format!("{head} on {machine}"),
            (None, Some(local)) => format!("{head} on {local}"),
            (None, None) => head,
        }
    }
}

/// Which machines a `q enter` target is resolved against, for the messages
/// that have to name them.
#[derive(Debug, Clone, Copy)]
struct Scope<'a> {
    /// This machine's name, when more than one machine is in play — `None` for
    /// a single-machine `q`, whose ambiguity line stays the local one.
    local: Option<&'a str>,
    /// The `--machine` this invocation was pinned to, if any.
    pinned: Option<&'a str>,
}

/// SPEC §16's rungs, in order: exact id, exact slug, unique prefix, unique
/// substring. A single array because [`resolve_target`] and
/// [`uncontested_local`] must ask the same question — the cheap check decides
/// whether the expensive one is worth running, so a rung the two disagreed
/// about would be a rung where `q enter` guesses.
const LADDER: [fn(&Quest, &str) -> bool; 4] = [
    |q, t| q.id == t,
    |q, t| q.slug == t,
    |q, t| q.id.starts_with(t) || q.slug.starts_with(t),
    |q, t| q.id.contains(t) || q.slug.contains(t),
];

/// How many of [`LADDER`]'s rungs are *exact* — the ones a Quest can only
/// match by being the thing that was typed.
const EXACT: usize = 2;

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
///
/// That applies to the *exact* rungs too: a Quest id is 16 bits and unique only
/// per machine, so an id that is exact on two machines is a genuine ambiguity
/// rather than a reason to prefer the local one. Which candidates this is
/// handed is [`run`]'s decision, and it is where the cost of asking the other
/// machines is weighed — see [`uncontested_local`].
fn resolve_target<'a>(
    local: &'a [Quest],
    results: &'a [RemoteResult],
    target: &str,
    scope: Scope<'_>,
) -> anyhow::Result<Candidate<'a>> {
    if target.is_empty() {
        return Err(not_found("", scope).into());
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

    for rule in LADDER {
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
                    candidates: matches.iter().map(|c| c.label(scope.local)).collect(),
                }
                .into());
            }
        }
    }
    Err(not_found(target, scope).into())
}

/// The local Quest `target` names exactly — but only when the last round's
/// cache says no remote could be naming the same thing.
///
/// `q enter` is the most-used command there is, and the cross-machine ladder
/// makes every invocation pay a fan-out round to rule out a collision that
/// almost never exists: ~150 ms against a healthy remote, the full 5 s deadline
/// against a dead one, where a local attach used to be a single database read.
/// So the `remote_cache` is asked first. It already holds every remote's last
/// listing, and reading it costs no ssh.
///
/// Suspicion is exactly "a cached remote row that would match at this rung or
/// an earlier one". A remote row further down the ladder cannot beat an exact
/// hit, so it is no reason to dial out; one at the same rung — or, against an
/// exact *slug*, one whose *id* is the target — is the collision that must be
/// reported, and that case takes the live round, so the ambiguity error is
/// built from fresh rows rather than from a cache that may have moved on.
/// A local target that is ambiguous *here* also declines: the full ladder is
/// what says so.
///
/// **The accepted gap.** A cold cache means no suspicion, so a genuine
/// collision with a remote Quest this machine has never listed enters the local
/// Quest silently. Closing it means an ssh on every `q enter`, which is the
/// cost this exists to avoid; a single `q list` (or any TUI tick) is enough to
/// teach the cache about the other machine.
fn uncontested_local<'a>(ctx: &Ctx, local: &'a [Quest], target: &str) -> Option<&'a Quest> {
    if target.is_empty() {
        return None;
    }
    let (rung, rule) = LADDER[..EXACT]
        .iter()
        .enumerate()
        .find(|(_, rule)| local.iter().any(|q| rule(q, target)))?;
    let mut hits = local.iter().filter(|q| rule(q, target));
    let quest = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    let contested = remote::cached_quests(ctx)
        .iter()
        .any(|q| LADDER[..=rung].iter().any(|r| r(&q.view.quest, target)));
    (!contested).then_some(quest)
}

/// `not found: quest `alpha`` — and, under `--machine`, which machine it was
/// looked for on, so the flag never narrows the search silently.
fn not_found(target: &str, scope: Scope<'_>) -> QError {
    match scope.pinned {
        Some(machine) => QError::NotFound(format!("quest `{target}` on {machine}")),
        None => QError::NotFound(format!("quest `{target}`")),
    }
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
    // `--machine` scopes `q enter` exactly as it scopes `q list`: it says
    // *which machine's* Quests are candidates. Without this the flag was
    // accepted and ignored, and `q enter <local quest> --machine ws` attached
    // to the local Quest while claiming to have gone to `ws`.
    let local_name = ctx.config.machine.name.as_str();
    let covers_local = ctx.machine_filter().is_none_or(|m| m == local_name);
    let local = if covers_local {
        ctx.db()?.list_quests(true)?
    } else {
        Vec::new()
    };

    let asking = !remote::targets(ctx).is_empty();
    if !asking {
        // Nothing to ask, so nothing an ssh could add: the ladder is the local
        // one and the common `q enter <slug>` stays a single database read.
        let scope = Scope {
            local: None,
            pinned: ctx.machine_filter(),
        };
        let found = resolve_target(&local, &[], target, scope)?;
        return enter_local(ctx, found.quest, label);
    }

    // Cache-first: the everyday `q enter <a quest that runs here>` stays a
    // database read even with remotes configured, and only a cached row that
    // could mean the same thing buys the round-trip (see `uncontested_local`).
    if let Some(quest) = uncontested_local(ctx, &local, target) {
        return enter_local(ctx, quest, label);
    }

    // `--all`: a Quest that is finished over there has to be *found* before it
    // can be refused, exactly as a local one is.
    let results = remote::fetch_all(ctx, true, None);
    remote::warn_unreachable(ctx, &results);
    flush_warnings(ctx);

    let scope = Scope {
        local: covers_local.then_some(local_name),
        pinned: ctx.machine_filter(),
    };
    let found = resolve_target(&local, &results, target, scope)?;
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

    /// The scope of a two-machine `q enter`, with no `--machine` given.
    fn cross() -> Scope<'static> {
        Scope {
            local: Some("laptop"),
            pinned: None,
        }
    }

    /// The scope of a `q` with no remotes at all.
    fn solo() -> Scope<'static> {
        Scope {
            local: None,
            pinned: None,
        }
    }

    #[test]
    fn a_remote_quest_is_resolved_by_the_same_rule_as_a_local_one() {
        let results = [result("ws", &["cdc-backfill"]), result("box", &["other"])];
        let id = results[0].quests[0].view.quest.id.clone();

        assert_eq!(
            machine_of(&resolve_target(&[], &results, "cdc-backfill", cross()).unwrap()),
            "ws"
        );
        assert_eq!(
            machine_of(&resolve_target(&[], &results, &id, cross()).unwrap()),
            "ws"
        );
        // Prefix, then substring.
        assert_eq!(
            resolve_target(&[], &results, "cdc", cross())
                .unwrap()
                .quest
                .slug,
            "cdc-backfill"
        );
        assert_eq!(
            resolve_target(&[], &results, "backfill", cross())
                .unwrap()
                .quest
                .slug,
            "cdc-backfill"
        );
        assert_eq!(
            machine_of(&resolve_target(&[], &results, "oth", cross()).unwrap()),
            "box"
        );

        for empty in ["", "nope"] {
            let e = resolve_target(&[], &results, empty, cross()).unwrap_err();
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
        let e = resolve_target(&[], &results, "cdc", cross()).unwrap_err();
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

        let found = resolve_target(&local, &results, "cdc-backfill", cross()).unwrap();
        assert_eq!(found.quest.slug, "cdc-backfill");
        assert_eq!(machine_of(&found), "ws");

        // The local Quest is still reachable by its own exact slug.
        let found = resolve_target(&local, &results, "cdc-backfill-v2", cross()).unwrap();
        assert_eq!(found.quest.slug, "cdc-backfill-v2");
        assert!(found.machine.is_none());

        // A fragment that matches on both is ambiguous rather than local-wins.
        let e = resolve_target(&local, &results, "cdc-back", cross()).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
        let said = e.to_string();
        assert!(said.contains("on ws"), "{said}");
        // Both candidates carry a machine, so `on ws` cannot be read as
        // covering the whole list.
        assert!(said.contains("(cdc-backfill-v2) on laptop"), "{said}");
    }

    /// A Quest id is unique only per machine, so an id that is exact on two of
    /// them is a genuine ambiguity — not a reason to quietly prefer the local
    /// Quest, which is what an "exact locally, skip the ssh" shortcut did.
    #[test]
    fn an_id_that_is_exact_on_two_machines_is_ambiguous() {
        let results = [result("ws", &["live-there"])];
        let id = results[0].quests[0].view.quest.id.clone();
        let mut collides = Quest::new("local-idle", "/tmp", "laptop");
        collides.id = id.clone();
        let local = [collides];

        let e = resolve_target(&local, &results, &id, cross()).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
        let said = e.to_string();
        assert!(said.contains("(local-idle) on laptop"), "{said}");
        assert!(said.contains("(live-there) on ws"), "{said}");

        // An exact *slug* on both machines is the same story.
        let local = [Quest::new("live-there", "/tmp", "laptop")];
        let e = resolve_target(&local, &results, "live-there", cross()).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
    }

    /// `--machine` scopes the ladder: a local Quest is not a candidate for a
    /// target pinned to a remote, and the refusal names the machine it looked
    /// on rather than claiming the Quest does not exist anywhere.
    #[test]
    fn a_pinned_machine_narrows_the_candidates_and_the_refusal() {
        let local = [Quest::new("local-alpha", "/tmp", "laptop")];
        let results = [result("ws", &["live-there"])];
        let pinned = Scope {
            local: None,
            pinned: Some("ws"),
        };

        // The local Quest is not in the candidate set at all: `run` leaves it
        // out, and the message says where it did look.
        let e = resolve_target(&[], &results, "local-alpha", pinned).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found")
        );
        assert!(e.to_string().contains("`local-alpha` on ws"), "{e}");

        // Pinned the other way, the remote rows are the ones left out.
        let pinned = Scope {
            local: None,
            pinned: Some("laptop"),
        };
        let e = resolve_target(&local, &[], "live-there", pinned).unwrap_err();
        assert!(e.to_string().contains("`live-there` on laptop"), "{e}");
        assert_eq!(
            resolve_target(&local, &[], "local-alpha", pinned)
                .unwrap()
                .quest
                .slug,
            "local-alpha"
        );
    }

    /// A remote's listing in `remote_cache`, as the last fan-out left it.
    fn cache(ctx: &Ctx, name: &str, quests: &[Quest]) {
        let views: Vec<QuestView> = quests
            .iter()
            .map(|q| QuestView::new(q.clone(), &[]))
            .collect();
        ctx.db()
            .unwrap()
            .put_remote_cache(name, &serde_json::to_string(&views).unwrap(), 1)
            .unwrap();
    }

    /// The everyday `q enter <a quest that runs here>`: the cache already holds
    /// the other machine's listing and nothing in it could mean this Quest, so
    /// there is nothing an ssh could add.
    #[test]
    fn an_exact_local_match_with_an_uncontested_cache_needs_no_round() {
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        cache(&ctx, "ws", &[Quest::new("over-there", "/tmp", "ws")]);

        assert_eq!(
            uncontested_local(&ctx, &local, "here").unwrap().slug,
            "here"
        );
        assert_eq!(
            uncontested_local(&ctx, &local, &local[0].id).unwrap().slug,
            "here"
        );
        // Only the *exact* rungs qualify: a fragment can be beaten by an exact
        // hit on another machine (S1), so it still has to ask.
        assert!(uncontested_local(&ctx, &local, "her").is_none());
        assert!(uncontested_local(&ctx, &local, "").is_none());
        assert!(uncontested_local(&ctx, &local, "nowhere").is_none());
    }

    /// A cache that has never heard of the other machine reports no suspicion —
    /// the accepted gap. `q enter` takes the local Quest rather than paying an
    /// ssh to discover there was nothing to find.
    #[test]
    fn a_cold_cache_is_not_a_suspicion() {
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        assert_eq!(
            uncontested_local(&ctx, &local, "here").unwrap().slug,
            "here"
        );
    }

    /// D3, from the cache: a cached remote row that would match at the same
    /// rung is the collision, and it buys the live round that reports it.
    #[test]
    fn a_cached_row_that_could_mean_the_same_quest_forces_the_round() {
        // An exact slug on both machines.
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        cache(&ctx, "ws", &[Quest::new("here", "/tmp", "ws")]);
        assert!(uncontested_local(&ctx, &local, "here").is_none());

        // An exact id on both machines.
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        let mut collides = Quest::new("over-there", "/tmp", "ws");
        collides.id = local[0].id.clone();
        cache(&ctx, "ws", &[collides]);
        assert!(uncontested_local(&ctx, &local, &local[0].id).is_none());

        // And an *earlier* rung: the local hit is an exact slug, but a remote
        // Quest's id is that same string — the id rung is walked first, so the
        // remote one would win outright.
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        let mut named = Quest::new("over-there", "/tmp", "ws");
        named.id = "here".to_string();
        cache(&ctx, "ws", &[named]);
        assert!(uncontested_local(&ctx, &local, "here").is_none());
    }

    /// A cached remote row that only matches further down the ladder cannot
    /// beat an exact hit, so it is not worth an ssh.
    #[test]
    fn a_cached_row_further_down_the_ladder_is_not_a_collision() {
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        cache(&ctx, "ws", &[Quest::new("here-too", "/tmp", "ws")]);
        assert_eq!(
            uncontested_local(&ctx, &local, "here").unwrap().slug,
            "here"
        );
    }

    /// Ambiguous *here* is the full ladder's story to tell — it is the thing
    /// that lists candidates — so the shortcut declines.
    #[test]
    fn a_target_that_is_ambiguous_locally_still_takes_the_long_way() {
        let (ctx, _dir) = with_tmux(false, false);
        let mut twin = Quest::new("elsewhere", "/tmp", "laptop");
        let first = Quest::new("here", "/tmp", "laptop");
        twin.id = first.id.clone();
        let id = first.id.clone();
        let local = [first, twin];
        assert!(uncontested_local(&ctx, &local, &id).is_none());
    }

    /// `--no-remote` and a `--machine` naming this machine leave no targets, so
    /// there is no cache to consult and nothing to be suspicious of.
    #[test]
    fn with_nothing_to_ask_the_cache_is_not_even_read() {
        let (ctx, _dir) = with_tmux(false, false);
        let local = [Quest::new("here", "/tmp", "laptop")];
        cache(&ctx, "ws", &[Quest::new("here", "/tmp", "ws")]);
        let ctx = ctx.with_no_remote(true);
        assert_eq!(
            uncontested_local(&ctx, &local, "here").unwrap().slug,
            "here"
        );
    }

    /// With no remotes in the picture the ladder is the local one, unchanged —
    /// candidates and all, so a single-machine `q` never grows an `on laptop`
    /// it has no use for.
    #[test]
    fn without_remotes_the_ladder_is_purely_local() {
        let local = [
            Quest::new("alpha", "/tmp", "laptop"),
            Quest::new("alpine", "/tmp", "laptop"),
        ];
        assert_eq!(
            resolve_target(&local, &[], "alpha", solo())
                .unwrap()
                .quest
                .slug,
            "alpha"
        );
        assert!(
            resolve_target(&local, &[], "alpha", solo())
                .unwrap()
                .machine
                .is_none()
        );
        assert_eq!(
            resolve_target(&local, &[], "alpi", solo())
                .unwrap()
                .quest
                .slug,
            "alpine"
        );
        let e = resolve_target(&local, &[], "alp", solo()).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous")
        );
        let said = e.to_string();
        assert!(!said.contains(" on "), "{said}");
    }
}
