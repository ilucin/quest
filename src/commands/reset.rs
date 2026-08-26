//! `q reset <session> [--delay N] [--strategy clear|compact]` — hand a session
//! a fresh context window (SPEC §8), plus the `Stop`-hook trigger that
//! schedules it once the master crosses the threshold.
//!
//! Two halves that never run in the same process:
//!
//! * `maybe_schedule` runs *inside* the `Stop` hook. It must return in
//!   milliseconds, so all it does is decide, log `session.reset_scheduled`,
//!   and hand a detached `q reset <session> --delay 2` to the OS.
//! * `run` is that detached process. The delay exists so Claude has finished
//!   settling after the turn; the idle gate is then re-taken, because between
//!   scheduling and waking the user may well have typed something.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::Ctx;
use crate::config::Config;
use crate::db::Db;
use crate::db::event::{EventFilter, KindPattern};
use crate::error::QError;
use crate::model::{Session, SessionRole, SessionStatus};
use crate::output;
use crate::{commands::sweep_quiet, commands::target};

/// Typed after `/clear`, once the `SessionStart` hook has re-injected the
/// brief: the brief is complete, so the master only needs to be told to go on.
pub const FOLLOW_UP: &str = "Nastavi rad na questu prema briefu.";

/// The delay a scheduled reset is spawned with (SPEC §8).
const SCHEDULED_DELAY: u64 = 2;

/// How long a scheduled reset suppresses the next one for the same session.
/// The cheapest guard against double scheduling that still works: `ctx_pct`
/// only drops once the statusline refreshes after the `/clear`, so between the
/// reset and that refresh every `Stop` would otherwise schedule again.
const COOLDOWN: Duration = Duration::from_secs(120);

/// How long `clear` waits for `SessionStart(source=clear)` before sending the
/// follow-up anyway — a missing hook must not strand the master silently.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(15);
const CLEAR_POLL: Duration = Duration::from_millis(250);

/// Cap on the `/compact` focus line, which is a tmux keystroke payload.
const FOCUS_CHARS: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Clear,
    Compact,
}

impl Strategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Strategy::Clear => "clear",
            Strategy::Compact => "compact",
        }
    }

    /// `[context] reset_strategy`. Validated on load, so an unknown spelling
    /// can only come from a config `q` itself rejected — fall back to `clear`.
    fn from_config(config: &Config) -> Strategy {
        match config.context.reset_strategy.as_str() {
            "compact" => Strategy::Compact,
            _ => Strategy::Clear,
        }
    }
}

pub struct Args<'a> {
    pub session: &'a str,
    /// `Some` marks the scheduled path — the one nobody is watching. A skip
    /// there is a normal outcome (exit 0); a manual `q reset` that cannot act
    /// says so with exit 1.
    pub delay: Option<u64>,
    pub strategy: Option<Strategy>,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<u8> {
    let scheduled = args.delay.is_some();
    if let Some(secs) = args.delay.filter(|d| *d > 0) {
        std::thread::sleep(Duration::from_secs(secs));
    }
    // Resolved after the sleep: the session may have ended while we waited.
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, args.session)?;
    let strategy = args
        .strategy
        .unwrap_or_else(|| Strategy::from_config(&ctx.config));
    let session = &found.session;
    let db = ctx.db()?;

    let (verdict, refusal) = found.idle_gate(ctx);
    if let Some(reason) = refusal {
        db.append_event(
            &found.quest.id,
            Some(&session.id),
            "session.reset_skipped",
            &serde_json::json!({
                "reason": reason,
                "strategy": strategy.as_str(),
                "status": session.status,
                "ctx_pct": session.ctx_pct,
                "scheduled": scheduled,
            }),
        )?;
        if !scheduled {
            return Err(QError::Conflict(format!("{} is not idle: {reason}", found.name())).into());
        }
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({
                    "session": session.id,
                    "quest": found.quest.slug,
                    "label": session.label,
                    "action": "skipped",
                    "reason": reason,
                    "strategy": strategy.as_str(),
                    "registry": verdict,
                }),
                || format!("skipped reset of {}: {reason}", found.name()),
            )?;
        }
        return Ok(0);
    }

    let ctx_pct = session.ctx_pct;
    let mut payload = serde_json::json!({
        "strategy": strategy.as_str(),
        "ctx_pct": ctx_pct,
        "scheduled": scheduled,
    });
    let tmux = ctx.tmux();
    match strategy {
        Strategy::Clear => {
            let baseline = db.last_event_id(&found.quest.id)?;
            tmux.send_keys(&session.tmux_pane, "/clear", true)?;
            let confirmed = await_clear(db, &found.quest.id, &session.id, baseline)?;
            payload["cleared"] = serde_json::json!(confirmed);
            payload["follow_up"] = serde_json::json!(FOLLOW_UP);
            tmux.send_keys(&session.tmux_pane, FOLLOW_UP, true)?;
        }
        Strategy::Compact => {
            let focus = focus_of(found.quest.goal.as_deref());
            let keys = match &focus {
                Some(text) => format!("/compact {text}"),
                None => "/compact".to_string(),
            };
            payload["focus"] = serde_json::json!(focus);
            payload["keys"] = serde_json::json!(keys);
            tmux.send_keys(&session.tmux_pane, &keys, true)?;
        }
    }
    db.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.reset",
        &payload,
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": session.id,
                "quest": found.quest.slug,
                "label": session.label,
                "pane": session.tmux_pane,
                "action": "reset",
                "strategy": strategy.as_str(),
                "ctx_pct": ctx_pct,
                "registry": verdict,
                "detail": payload,
            }),
            || {
                format!(
                    "reset {} via /{} (ctx {})",
                    found.name(),
                    strategy.as_str(),
                    ctx_pct.map_or("?".to_string(), |p| format!("{p}%"))
                )
            },
        )?;
    }
    Ok(0)
}

/// The `Stop`-hook half (SPEC §8) — the whole of q's presence in that handler.
/// Silent and best effort: a hook must never fail, and never block, so all
/// this does is decide, log, and hand the work to a detached process.
pub fn maybe_schedule(db: &Db, session: &Session) {
    schedule(db, session, &Config::load().unwrap_or_default(), &detach);
}

/// `maybe_schedule` with its two impure edges — the config and the launcher —
/// injected, so the decision is testable without a real fork. Returns the
/// command line it scheduled, `None` when it decided against.
fn schedule(
    db: &Db,
    session: &Session,
    config: &Config,
    launch: &dyn Fn(&[String]) -> bool,
) -> Option<Vec<String>> {
    if session.role != SessionRole::Master {
        return None;
    }
    // The row `stop` just wrote, not the one it was handed: a `Notification`
    // racing this turn may have moved the session to `waiting`, and typing
    // `/clear` at a permission prompt would answer it (SPEC §8).
    let row = db.get_session(&session.id).ok().flatten()?;
    if row.status != SessionStatus::Idle {
        return None;
    }
    let quest = db.get_quest(&session.quest_id).ok().flatten()?;
    if !quest.auto_reset.unwrap_or(config.context.auto_reset) {
        return None;
    }
    let threshold = quest
        .ctx_reset_pct
        .unwrap_or(config.context.master_reset_pct);
    let ctx_pct = row.ctx_pct.filter(|p| *p >= threshold)?;
    if recently_scheduled(db, &quest.id, &row.id) {
        return None;
    }

    let strategy = Strategy::from_config(config);
    let argv = argv(&row.id, strategy)?;
    let spawned = launch(&argv);
    let _ = db.append_event(
        &quest.id,
        Some(&row.id),
        "session.reset_scheduled",
        &serde_json::json!({
            "ctx_pct": ctx_pct,
            "threshold": threshold,
            "strategy": strategy.as_str(),
            "delay": SCHEDULED_DELAY,
            "argv": argv,
            "spawned": spawned,
        }),
    );
    Some(argv)
}

/// True when this session already has a `session.reset_scheduled` inside
/// `COOLDOWN`. One event is enough to look at: they only ever get appended.
fn recently_scheduled(db: &Db, quest_id: &str, session_id: &str) -> bool {
    let filter = EventFilter {
        kinds: vec![KindPattern::Exact("session.reset_scheduled".to_string())],
        session_id: Some(session_id.to_string()),
    };
    let Ok(events) = db.list_events_latest(quest_id, &filter, 1) else {
        // A database that cannot be read is not a licence to schedule.
        return true;
    };
    let cutoff = crate::model::now() - COOLDOWN.as_secs() as i64;
    events.last().is_some_and(|e| e.ts >= cutoff)
}

/// The detached command line, or `None` when this binary's own path is
/// unknowable (nothing to re-invoke).
fn argv(session_id: &str, strategy: Strategy) -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?;
    Some(vec![
        exe.to_string_lossy().into_owned(),
        "reset".to_string(),
        session_id.to_string(),
        "--delay".to_string(),
        SCHEDULED_DELAY.to_string(),
        "--strategy".to_string(),
        strategy.as_str().to_string(),
        "--quiet".to_string(),
    ])
}

/// Spawns `argv` fully detached — its own process group, no streams — and does
/// not wait: the hook returns while the reset sleeps out its delay.
///
/// Never under `$Q_FIXTURE` or `$Q_NO_DETACH`: tests assert on the
/// `session.reset_scheduled` payload's `argv` instead of racing a real child.
fn detach(argv: &[String]) -> bool {
    if env_set("Q_FIXTURE") || env_set("Q_NO_DETACH") {
        return false;
    }
    let Some((program, rest)) = argv.split_first() else {
        return false;
    };
    use std::os::unix::process::CommandExt;
    Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok()
}

fn env_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

/// Waits for the `SessionStart(source=clear)` the `/clear` should trigger, so
/// the follow-up prompt lands on a Claude that already has the fresh brief.
/// `false` when it never arrived — the follow-up is sent regardless.
fn await_clear(db: &Db, quest_id: &str, session_id: &str, after: i64) -> anyhow::Result<bool> {
    let filter = EventFilter {
        kinds: vec![KindPattern::Exact("session.start".to_string())],
        session_id: Some(session_id.to_string()),
    };
    for _ in 0..clear_polls() {
        std::thread::sleep(CLEAR_POLL);
        let fresh = db.list_events_after(quest_id, after, &filter, 16)?;
        if fresh.iter().any(|e| {
            e.payload
                .as_ref()
                .and_then(|p| p["source"].as_str())
                .is_some_and(|s| s == "clear")
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How many times `await_clear` polls. Under `$Q_FIXTURE` no real Claude can
/// ever fire the hook, so the wait is bounded by `$Q_RESET_ITERATIONS`
/// (default 0 — proceed straight to the follow-up) the way `q events --follow`
/// is bounded by `$Q_FOLLOW_ITERATIONS`.
fn clear_polls() -> u32 {
    if env_set("Q_FIXTURE") {
        return std::env::var("Q_RESET_ITERATIONS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
    }
    (CLEAR_TIMEOUT.as_millis() / CLEAR_POLL.as_millis()) as u32
}

/// The Quest goal as a single short line, for `/compact <focus>`. `None` for a
/// Quest without one, which sends a bare `/compact`.
fn focus_of(goal: Option<&str>) -> Option<String> {
    let flat = goal?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string();
    if flat.is_empty() {
        return None;
    }
    if flat.chars().count() <= FOCUS_CHARS {
        return Some(flat);
    }
    let mut out: String = flat.chars().take(FOCUS_CHARS - 1).collect();
    out.push('…');
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::model::{Quest, SessionStatus};

    /// A Quest with an idle master over the threshold — what `Stop` sees when
    /// a reset is due.
    fn fleet(ctx_pct: Option<u8>, status: SessionStatus) -> (Db, Quest, Session) {
        let db = Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("alpha", "/tmp", "laptop"))
            .unwrap();
        let mut row = Session::new(&quest.id, SessionRole::Master, "master", "q-alpha", "%1");
        row.status = status;
        row.ctx_pct = ctx_pct;
        let session = db.insert_session(&row).unwrap();
        (db, quest, session)
    }

    /// Records whether the launcher was reached without ever forking.
    struct Launcher(Cell<u32>);

    impl Launcher {
        fn new() -> Launcher {
            Launcher(Cell::new(0))
        }

        fn calls(&self) -> u32 {
            self.0.get()
        }

        fn as_fn(&self) -> impl Fn(&[String]) -> bool + '_ {
            |_argv| {
                self.0.set(self.0.get() + 1);
                true
            }
        }
    }

    fn scheduled_events(db: &Db, quest_id: &str) -> Vec<crate::model::Event> {
        db.list_events_by_kinds(quest_id, &["session.reset_scheduled"], 10)
            .unwrap()
    }

    #[test]
    fn an_idle_master_over_the_threshold_is_scheduled_once() {
        let (db, quest, session) = fleet(Some(40), SessionStatus::Idle);
        let launcher = Launcher::new();
        let config = Config::default();
        let argv = schedule(&db, &session, &config, &launcher.as_fn()).unwrap();
        assert_eq!(argv[1..4], ["reset", session.id.as_str(), "--delay"]);
        assert_eq!(launcher.calls(), 1);

        let events = scheduled_events(&db, &quest.id);
        assert_eq!(events.len(), 1);
        let payload = events[0].payload.as_ref().unwrap();
        assert_eq!(payload["ctx_pct"], 40);
        assert_eq!(payload["threshold"], 35);
        assert_eq!(payload["strategy"], "clear");
        assert_eq!(payload["delay"], SCHEDULED_DELAY);
        assert_eq!(payload["spawned"], true);
        assert_eq!(payload["argv"], serde_json::json!(argv));

        // The cooldown keeps the next `Stop` from scheduling again while
        // `ctx_pct` still reads high.
        assert!(schedule(&db, &session, &config, &launcher.as_fn()).is_none());
        assert_eq!(launcher.calls(), 1);
        assert_eq!(scheduled_events(&db, &quest.id).len(), 1);
    }

    #[test]
    fn nothing_is_scheduled_below_the_threshold_or_without_a_reading() {
        for ctx_pct in [None, Some(0), Some(34)] {
            let (db, quest, session) = fleet(ctx_pct, SessionStatus::Idle);
            let launcher = Launcher::new();
            assert!(
                schedule(&db, &session, &Config::default(), &launcher.as_fn()).is_none(),
                "{ctx_pct:?}"
            );
            assert_eq!(launcher.calls(), 0);
            assert!(scheduled_events(&db, &quest.id).is_empty());
        }
        // The threshold is inclusive.
        let (db, _, session) = fleet(Some(35), SessionStatus::Idle);
        assert!(schedule(&db, &session, &Config::default(), &|_| true).is_some());
    }

    #[test]
    fn a_worker_is_never_reset_however_full_it_is() {
        let (db, quest, mut session) = fleet(Some(99), SessionStatus::Idle);
        session.role = SessionRole::Worker;
        assert!(schedule(&db, &session, &Config::default(), &|_| true).is_none());
        assert!(scheduled_events(&db, &quest.id).is_empty());
    }

    #[test]
    fn a_session_that_is_not_idle_is_left_alone() {
        // `Stop` writes `idle` first, so this is the race where that write was
        // dropped or a `Notification` overtook it.
        for status in [
            SessionStatus::Waiting,
            SessionStatus::Busy,
            SessionStatus::Starting,
            SessionStatus::Ended,
        ] {
            let (db, quest, session) = fleet(Some(90), status);
            assert!(
                schedule(&db, &session, &Config::default(), &|_| true).is_none(),
                "{status}"
            );
            assert!(scheduled_events(&db, &quest.id).is_empty(), "{status}");
        }
    }

    #[test]
    fn auto_reset_off_in_the_config_or_on_the_quest_stops_it() {
        let mut off = Config::default();
        off.context.auto_reset = false;

        let (db, _, session) = fleet(Some(90), SessionStatus::Idle);
        assert!(schedule(&db, &session, &off, &|_| true).is_none());

        // A Quest that says `on` overrides a config that says off, and vice
        // versa: NULL is the only value that follows the config.
        let patched = db
            .update_quest(
                &session.quest_id,
                &crate::db::quest::QuestPatch {
                    auto_reset: Some(Some(true)),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(patched.auto_reset, Some(true));
        assert!(schedule(&db, &session, &off, &|_| true).is_some());

        let (db, _, session) = fleet(Some(90), SessionStatus::Idle);
        db.update_quest(
            &session.quest_id,
            &crate::db::quest::QuestPatch {
                auto_reset: Some(Some(false)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(schedule(&db, &session, &Config::default(), &|_| true).is_none());
    }

    #[test]
    fn the_quest_threshold_and_the_config_strategy_are_both_honoured() {
        let (db, quest, session) = fleet(Some(50), SessionStatus::Idle);
        let mut config = Config::default();
        config.context.master_reset_pct = 80;
        config.context.reset_strategy = "compact".to_string();
        assert!(schedule(&db, &session, &config, &|_| true).is_none());

        db.update_quest(
            &quest.id,
            &crate::db::quest::QuestPatch {
                ctx_reset_pct: Some(Some(45)),
                ..Default::default()
            },
        )
        .unwrap();
        let argv = schedule(&db, &session, &config, &|_| true).unwrap();
        assert!(argv.contains(&"compact".to_string()), "{argv:?}");
        let payload = scheduled_events(&db, &quest.id)[0].payload.clone().unwrap();
        assert_eq!(payload["threshold"], 45);
        assert_eq!(payload["strategy"], "compact");
    }

    #[test]
    fn the_strategy_comes_from_the_config_and_falls_back_to_clear() {
        let mut config = Config::default();
        assert_eq!(Strategy::from_config(&config), Strategy::Clear);
        config.context.reset_strategy = "compact".to_string();
        assert_eq!(Strategy::from_config(&config), Strategy::Compact);
        // Only reachable via a config `validate` would have rejected.
        config.context.reset_strategy = "nuke".to_string();
        assert_eq!(Strategy::from_config(&config), Strategy::Clear);
    }

    #[test]
    fn the_focus_is_one_short_line_or_nothing() {
        assert_eq!(focus_of(None), None);
        assert_eq!(focus_of(Some("   ")), None);
        assert_eq!(
            focus_of(Some("  make the\n backfill\tidempotent ")),
            Some("make the backfill idempotent".to_string())
        );
        let long = "ž".repeat(FOCUS_CHARS + 50);
        let focus = focus_of(Some(&long)).unwrap();
        assert_eq!(focus.chars().count(), FOCUS_CHARS);
        assert!(focus.ends_with('…'));
    }

    #[test]
    fn the_scheduled_argv_re_invokes_this_binary() {
        let argv = argv("s-0001", Strategy::Compact).unwrap();
        assert_eq!(
            argv[1..],
            [
                "reset",
                "s-0001",
                "--delay",
                "2",
                "--strategy",
                "compact",
                "--quiet"
            ]
        );
        // The test binary, not `q`, but it is this process's own path.
        assert_eq!(
            std::path::Path::new(&argv[0]),
            std::env::current_exe().unwrap()
        );
    }
}
