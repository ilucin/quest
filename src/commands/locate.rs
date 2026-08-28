//! Where a `<quest>` target lives (SPEC §15, §16).
//!
//! One resolver for every command that takes a Quest, across every machine the
//! listing covers. It was `q enter`'s (bd-8lz.5.2) and is unchanged in
//! behaviour — extracted here so the generic remote dispatch of
//! [`crate::commands::proxy`] resolves by exactly the same rules, including the
//! cache-first shortcut that keeps the everyday local command a database read.
//!
//! Nothing here writes to a terminal: an unreachable remote is buffered on the
//! `Ctx` (see [`Ctx::warn`]) and the caller decides where it goes.

use crate::Ctx;
use crate::error::QError;
use crate::model::Quest;
use crate::remote::{self, RemoteResult};

/// A Quest a target could mean, and the machine it runs on.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub quest: &'a Quest,
    /// `None` for this machine.
    pub machine: Option<&'a str>,
    /// That machine's tmux prefix; see [`crate::commands::enter::remote_target`].
    pub tmux_prefix: Option<&'a str>,
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

/// A resolved Quest, owned, with the machine it was found on.
#[derive(Debug, Clone)]
pub struct Located {
    pub quest: Quest,
    /// `None` when the Quest is this machine's own.
    pub machine: Option<String>,
    /// The far end's `[tmux] session_prefix`, when it reported one.
    pub tmux_prefix: Option<String>,
}

impl Located {
    fn local(quest: Quest) -> Located {
        Located {
            quest,
            machine: None,
            tmux_prefix: None,
        }
    }
}

/// Which machines a target is resolved against, for the messages that have to
/// name them.
#[derive(Debug, Clone, Copy)]
pub struct Scope<'a> {
    /// This machine's name, when more than one machine is in play — `None` for
    /// a single-machine `q`, whose ambiguity line stays the local one.
    pub local: Option<&'a str>,
    /// The `--machine` this invocation was pinned to, if any.
    pub pinned: Option<&'a str>,
}

/// SPEC §16's rungs, in order: exact id, exact slug, unique prefix, unique
/// substring. A single array because [`resolve_target`] and
/// [`uncontested_local`] must ask the same question — the cheap check decides
/// whether the expensive one is worth running, so a rung the two disagreed
/// about would be a rung where the resolver guesses.
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
/// handed is [`quest`]'s decision, and it is where the cost of asking the other
/// machines is weighed — see [`uncontested_local`].
pub fn resolve_target<'a>(
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
/// almost never exists: ~150 ms against a healthy remote, the full deadline
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
/// collision with a remote Quest this machine has never listed resolves to the
/// local Quest silently. Closing it means an ssh on every command, which is the
/// cost this exists to avoid; a single `q list` (or any TUI tick) is enough to
/// teach the cache about the other machine.
pub fn uncontested_local<'a>(ctx: &Ctx, local: &'a [Quest], target: &str) -> Option<&'a Quest> {
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

/// The Quest `target` names, wherever it runs (SPEC §15).
///
/// Cache-first, then a live fan-out only when the cache gives a reason to
/// suspect a collision — see [`uncontested_local`]. `--machine` scopes the
/// candidate set exactly as it scopes `q list`: pinned to a remote, this
/// machine's Quests are not candidates at all, and the refusal says where it
/// looked.
///
/// An unreachable remote is buffered as a warning rather than printed; call
/// [`crate::commands::flush_warnings`] once the caller owns the terminal.
pub fn quest(ctx: &Ctx, target: &str) -> anyhow::Result<Located> {
    let local_name = ctx.config.machine.name.as_str();
    let covers_local = ctx.machine_filter().is_none_or(|m| m == local_name);
    let local = if covers_local {
        ctx.db()?.list_quests(true)?
    } else {
        Vec::new()
    };

    if remote::targets(ctx).is_empty() {
        // Nothing to ask, so nothing an ssh could add: the ladder is the local
        // one and the common `<slug>` stays a single database read.
        let scope = Scope {
            local: None,
            pinned: ctx.machine_filter(),
        };
        return resolve_target(&local, &[], target, scope).map(|c| Located::local(c.quest.clone()));
    }

    if let Some(quest) = uncontested_local(ctx, &local, target) {
        return Ok(Located::local(quest.clone()));
    }

    // `--all`: a Quest that is finished over there has to be *found* before it
    // can be refused, exactly as a local one is.
    let results = remote::fetch_all(ctx, true, None);
    remote::warn_unreachable(ctx, &results);

    let scope = Scope {
        local: covers_local.then_some(local_name),
        pinned: ctx.machine_filter(),
    };
    let found = resolve_target(&local, &results, target, scope)?;
    Ok(Located {
        quest: found.quest.clone(),
        machine: found.machine.map(str::to_string),
        tmux_prefix: found.tmux_prefix.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::QuestView;
    use crate::config::{Config, Remote};
    use crate::db::Db;
    use crate::remote::{RemoteQuest, RemoteStatus};

    /// A `Ctx` with one remote configured and no ssh that could answer.
    fn fresh() -> Ctx {
        let mut config = Config::default();
        config.machine.name = "laptop".to_string();
        config.remotes = vec![Remote {
            name: "ws".to_string(),
            ssh: "ws-host".to_string(),
        }];
        Ctx::for_tests(
            config,
            Db::open_in_memory().unwrap(),
            Box::new(crate::tmux::FixtureTmux::new(std::path::PathBuf::from(
                "/nonexistent/tmux.json",
            ))),
        )
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

    /// The scope of a two-machine resolution, with no `--machine` given.
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

        // The local Quest is not in the candidate set at all: `quest` leaves it
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

    /// The everyday target naming a Quest that runs here: the cache already
    /// holds the other machine's listing and nothing in it could mean this
    /// Quest, so there is nothing an ssh could add.
    #[test]
    fn an_exact_local_match_with_an_uncontested_cache_needs_no_round() {
        let ctx = fresh();
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
    /// the accepted gap.
    #[test]
    fn a_cold_cache_is_not_a_suspicion() {
        let ctx = fresh();
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
        let ctx = fresh();
        let local = [Quest::new("here", "/tmp", "laptop")];
        cache(&ctx, "ws", &[Quest::new("here", "/tmp", "ws")]);
        assert!(uncontested_local(&ctx, &local, "here").is_none());

        // An exact id on both machines.
        let ctx = fresh();
        let local = [Quest::new("here", "/tmp", "laptop")];
        let mut collides = Quest::new("over-there", "/tmp", "ws");
        collides.id = local[0].id.clone();
        cache(&ctx, "ws", &[collides]);
        assert!(uncontested_local(&ctx, &local, &local[0].id).is_none());

        // And an *earlier* rung: the local hit is an exact slug, but a remote
        // Quest's id is that same string — the id rung is walked first, so the
        // remote one would win outright.
        let ctx = fresh();
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
        let ctx = fresh();
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
        let ctx = fresh();
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
        let ctx = fresh();
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
