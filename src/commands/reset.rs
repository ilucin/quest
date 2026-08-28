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
//!   scheduling and waking the user may well have typed something. Every
//!   failure there is a `session.reset_failed` event and exit 0 — nobody is
//!   reading its exit code, and its stderr goes to /dev/null.

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

/// Typed after `/clear` or `/compact`, once the `SessionStart` hook has
/// re-injected the brief: the brief is complete, so the master only needs to
/// be told to go on. Without it the fresh window sits idle forever.
pub const FOLLOW_UP: &str = "Nastavi rad na questu prema briefu.";

/// The delay a scheduled reset is spawned with (SPEC §8).
const SCHEDULED_DELAY: u64 = 2;

/// A backstop against scheduling twice in the same breath. The real guard is
/// the `ctx_updated_at` freshness check in `schedule`; this only covers the
/// window before the reset has written anything at all.
const COOLDOWN: Duration = Duration::from_secs(30);

/// How long a reset waits for the fresh brief before sending the follow-up
/// anyway — a missing hook must not strand the master silently. `/compact`
/// summarises the transcript through a model first, so it gets much longer.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(15);
const COMPACT_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(250);

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

    /// The keystrokes that start the new context window. `/compact` is given
    /// the Quest goal as its focus; `/clear` needs nothing (SPEC §8).
    fn keys(self, goal: Option<&str>) -> (String, Option<String>) {
        match self {
            Strategy::Clear => ("/clear".to_string(), None),
            Strategy::Compact => match focus_of(goal) {
                Some(focus) => (format!("/compact {focus}"), Some(focus)),
                None => ("/compact".to_string(), None),
            },
        }
    }

    fn timeout(self) -> Duration {
        match self {
            Strategy::Clear => CLEAR_TIMEOUT,
            Strategy::Compact => COMPACT_TIMEOUT,
        }
    }

    /// `[context] reset_strategy`. Validated on load, so an unknown spelling
    /// can only come from a config `q` itself rejected — fall back to `clear`.
    pub fn from_config(config: &Config) -> Strategy {
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
    match attempt(ctx, args, scheduled) {
        Ok(code) => Ok(code),
        // Nobody reads the scheduled path's exit code, and its stderr goes to
        // /dev/null: a failure that appends no event leaves no trace at all.
        Err(e) if scheduled => {
            record_failure(ctx, args.session, &e);
            Ok(0)
        }
        Err(e) => Err(e),
    }
}

/// Best effort: re-resolves the target purely to have somewhere to log. A
/// failure that was itself the resolution failing has no Quest to belong to.
fn record_failure(ctx: &Ctx, session: &str, error: &anyhow::Error) {
    let Ok(db) = ctx.db() else { return };
    let Ok(found) = target::resolve(ctx, session) else {
        return;
    };
    let _ = db.append_event(
        &found.quest.id,
        Some(&found.session.id),
        "session.reset_failed",
        &serde_json::json!({
            "stage": "run",
            "error": error.to_string(),
            "scheduled": true,
        }),
    );
}

fn attempt(ctx: &Ctx, args: &Args, scheduled: bool) -> anyhow::Result<u8> {
    // Resolved after the sleep: the session may have ended while we waited.
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, args.session)?;
    let strategy = args
        .strategy
        .unwrap_or_else(|| Strategy::from_config(&ctx.config));
    let session = &found.session;
    let db = ctx.db()?;
    // A row whose window never opened has an empty pane, and every `send_keys`
    // below would then land on whatever window is current — `/clear` typed into
    // the caller's own terminal. The gate cannot catch it: a `starting` row that
    // a hook marked idle passes.
    found.require_pane()?;
    // The same cooldown `Z` takes, for the same reason: a `q reset` typed by
    // hand on top of a reset the `Stop` hook has just scheduled runs two
    // children, and the second clears the context window the first just made
    // (SPEC §8). The scheduled path is exempt — it *is* that reset.
    if !scheduled && recently_scheduled(db, &found.quest.id, &session.id) {
        return Err(QError::Conflict(format!(
            "{} already has a reset scheduled; wait for it",
            found.name()
        ))
        .into());
    }

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
    let (keys, focus) = strategy.keys(found.quest.goal.as_deref());
    let mut payload = serde_json::json!({
        "strategy": strategy.as_str(),
        "ctx_pct": ctx_pct,
        "scheduled": scheduled,
        "keys": keys,
        "focus": focus,
        "follow_up": FOLLOW_UP,
    });
    // Both strategies end in a fresh, empty window, and Claude will sit there
    // idle forever unless it is told to go on (SPEC §8) — so the follow-up is
    // typed either way, once the `SessionStart` hook has injected the brief.
    let tmux = ctx.tmux();
    let baseline = db.last_event_id(&found.quest.id)?;
    tmux.send_keys(&session.tmux_pane, &keys, true)?;
    let injected = await_brief(db, &found.quest.id, &session.id, baseline, strategy)?;
    payload["brief_injected"] = serde_json::json!(injected);
    tmux.send_keys(&session.tmux_pane, FOLLOW_UP, true)?;
    db.append_event(
        &found.quest.id,
        Some(&session.id),
        "session.reset",
        &payload,
    )?;
    // One reset executed = one notification; the `session.reset` event above is
    // the single edge, so no extra de-dupe is needed (SPEC §20).
    crate::notify::emit(
        &ctx.config.notify,
        crate::notify::runner().as_ref(),
        crate::notify::Kind::Reset,
        &format!("{} · reset", found.quest.slug),
        &format!("{} got a fresh context", session.label),
    );

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
    launch: &dyn Fn(&[String]) -> Launch,
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
    // The reading has to postdate the last reset. `/clear` empties the window
    // immediately but `ctx_pct` only drops when the statusline next refreshes,
    // so acting on an older reading resets the same session over and over
    // (SPEC §8) — and it is the only thing that would.
    if !reading_is_fresh(row.ctx_updated_at, last_reset_ts(db, &quest.id, &row.id)) {
        return None;
    }
    if recently_scheduled(db, &quest.id, &row.id) {
        return None;
    }

    let strategy = Strategy::from_config(config);
    let argv = argv(&row.id, strategy, SCHEDULED_DELAY)?;
    let mut payload = serde_json::json!({
        "ctx_pct": ctx_pct,
        "threshold": threshold,
        "strategy": strategy.as_str(),
        "delay": SCHEDULED_DELAY,
        "argv": argv,
    });
    // A reset that was never handed to the OS is not scheduled: it must not
    // start a cooldown, or a transient fork failure would leave the master
    // sitting on a full context window until it expires.
    let outcome = launch(&argv);
    if outcome == Launch::Failed {
        payload["stage"] = serde_json::json!("spawn");
        let _ = db.append_event(&quest.id, Some(&row.id), "session.reset_failed", &payload);
        return None;
    }
    payload["spawned"] = serde_json::json!(outcome == Launch::Spawned);
    let _ = db.append_event(
        &quest.id,
        Some(&row.id),
        "session.reset_scheduled",
        &payload,
    );
    Some(argv)
}

/// A `ctx_pct` reading is only worth acting on when it was taken after the
/// last reset. A session that has never been reset has nothing to be stale
/// against; one that has, and whose reading has no timestamp at all, does.
fn reading_is_fresh(ctx_updated_at: Option<i64>, last_reset: Option<i64>) -> bool {
    match last_reset {
        None => true,
        Some(last) => ctx_updated_at.is_some_and(|ts| ts > last),
    }
}

/// When this session was last reset, or had one scheduled — the point after
/// which a `ctx_pct` reading is believable. `None` when it never was.
fn last_reset_ts(db: &Db, quest_id: &str, session_id: &str) -> Option<i64> {
    let filter = EventFilter {
        kinds: vec![
            KindPattern::Exact("session.reset".to_string()),
            KindPattern::Exact("session.reset_scheduled".to_string()),
        ],
        session_id: Some(session_id.to_string()),
    };
    db.list_events_latest(quest_id, &filter, 1)
        .ok()?
        .first()
        .map(|e| e.ts)
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
    events.first().is_some_and(|e| e.ts >= cutoff)
}

/// The detached command line, or `None` when this binary's own path is
/// unknowable (nothing to re-invoke).
fn argv(session_id: &str, strategy: Strategy, delay: u64) -> Option<Vec<String>> {
    let exe = std::env::current_exe().ok()?;
    Some(vec![
        exe.to_string_lossy().into_owned(),
        "reset".to_string(),
        session_id.to_string(),
        "--delay".to_string(),
        delay.to_string(),
        "--strategy".to_string(),
        strategy.as_str().to_string(),
        "--quiet".to_string(),
    ])
}

/// A reset asked for by hand — the TUI's `Z` (SPEC §17) — handed to the same
/// detached `q reset` the `Stop` hook uses.
///
/// Detached for the same reason the hook detaches: [`attempt`] waits for the
/// fresh brief (up to [`COMPACT_TIMEOUT`]), and a caller that must not block
/// cannot afford that. The TUI's event loop is exactly such a caller — a
/// synchronous `Z` would freeze the whole UI for the length of the wait.
///
/// The idle gate is taken *here* as well as in the child, so a session that is
/// mid-turn is refused in front of the user rather than silently in a process
/// nobody is watching. `--delay 0` marks the child as the scheduled path, so
/// what it cannot do lands as a `session.reset_skipped` / `session.reset_failed`
/// event instead of an exit code nobody reads.
///
/// Returns the command line that was scheduled.
pub fn spawn_detached(
    ctx: &Ctx,
    found: &target::Target,
    strategy: Strategy,
) -> anyhow::Result<Vec<String>> {
    spawn_detached_with(ctx, found, strategy, &detach)
}

/// [`spawn_detached`] with its one impure edge injected, the way [`schedule`]
/// takes it — so a test can prove what was decided without forking anything.
/// Without this, `current_exe()` inside a unit test is the *test binary*, and
/// every `Z` test really did spawn a copy of it.
fn spawn_detached_with(
    ctx: &Ctx,
    found: &target::Target,
    strategy: Strategy,
    launch: &dyn Fn(&[String]) -> Launch,
) -> anyhow::Result<Vec<String>> {
    // `require_live` covers both halves: an ended row, and a row whose window
    // never opened (an empty pane is "the current window" to tmux, so a reset
    // would type `/clear` into whatever the caller is looking at).
    found.require_live()?;
    let (_, refusal) = found.idle_gate(ctx);
    if let Some(reason) = refusal {
        return Err(QError::Conflict(format!("{} is not idle: {reason}", found.name())).into());
    }
    // The same cooldown the AUTO path takes. Without it, a `Z` landing on a
    // session the `Stop` hook has just scheduled a `--delay 2` reset for runs
    // two children: this one clears now, the other wakes two seconds later,
    // re-takes an idle gate the fresh `SessionStart` has just satisfied, and
    // clears the new context window plus its follow-up (SPEC §8).
    if recently_scheduled(ctx.db()?, &found.quest.id, &found.session.id) {
        return Err(QError::Conflict(format!(
            "{} already has a reset scheduled; wait for it",
            found.name()
        ))
        .into());
    }

    let argv = argv(&found.session.id, strategy, 0)
        .ok_or_else(|| QError::Other("cannot find this q binary to re-invoke".to_string()))?;
    let mut payload = serde_json::json!({
        "ctx_pct": found.session.ctx_pct,
        "strategy": strategy.as_str(),
        "delay": 0,
        "manual": true,
        "argv": argv,
    });
    let outcome = launch(&argv);
    if outcome == Launch::Failed {
        let mut failed = payload.clone();
        failed["stage"] = serde_json::json!("spawn");
        let _ = ctx.db()?.append_event(
            &found.quest.id,
            Some(&found.session.id),
            "session.reset_failed",
            &failed,
        );
        return Err(QError::Other(format!("cannot start `q reset {}`", found.name())).into());
    }
    // Same key the auto path records: under `$Q_NO_DETACH`/`$Q_FIXTURE` nothing
    // was handed to the OS, and the event must not claim otherwise.
    payload["spawned"] = serde_json::json!(outcome == Launch::Spawned);
    ctx.db()?.append_event(
        &found.quest.id,
        Some(&found.session.id),
        "session.reset_scheduled",
        &payload,
    )?;
    Ok(argv)
}

/// What became of the detached child. `Suppressed` is a decision, not a
/// failure: the reset was scheduled, the OS was simply not involved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Launch {
    Spawned,
    Suppressed,
    Failed,
}

/// Spawns `argv` fully detached — its own process group, no streams — and does
/// not wait: the hook returns while the reset sleeps out its delay.
///
/// Never under `$Q_FIXTURE` or `$Q_NO_DETACH`: tests assert on the
/// `session.reset_scheduled` payload's `argv` instead of racing a real child.
fn detach(argv: &[String]) -> Launch {
    if suppressed() {
        return Launch::Suppressed;
    }
    let Some((program, rest)) = argv.split_first() else {
        return Launch::Failed;
    };
    use std::os::unix::process::CommandExt;
    let spawned = Command::new(program)
        .args(rest)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .is_ok();
    if spawned {
        Launch::Spawned
    } else {
        Launch::Failed
    }
}

fn suppressed() -> bool {
    // No in-crate test may fork. `current_exe()` there is the *test binary*,
    // so a real launch spawns a copy of the whole suite with `reset …` on its
    // command line — which is exactly what the `Z` tests were doing. The
    // integration tests reach the same branch through `$Q_FIXTURE`.
    cfg!(test) || env_set("Q_FIXTURE") || env_set("Q_NO_DETACH")
}

fn env_set(key: &str) -> bool {
    std::env::var_os(key).is_some_and(|v| !v.is_empty())
}

/// Waits until the fresh brief is actually on its way back to Claude, so the
/// follow-up prompt lands on a Claude that has it.
///
/// The marker is `session.brief_injected`, which the `SessionStart` hook
/// appends *after* `brief::render` returns — `session.start` is written before
/// it, and rendering shells out to `bd`/`brain`, so it can be seconds behind.
/// Only events newer than `after` count, so the previous window's start cannot
/// be mistaken for this one's. `false` when it never arrived — the follow-up is
/// sent regardless, rather than strand the master.
fn await_brief(
    db: &Db,
    quest_id: &str,
    session_id: &str,
    after: i64,
    strategy: Strategy,
) -> anyhow::Result<bool> {
    let filter = EventFilter {
        kinds: vec![KindPattern::Exact("session.brief_injected".to_string())],
        session_id: Some(session_id.to_string()),
    };
    for _ in 0..polls(strategy) {
        std::thread::sleep(POLL);
        let fresh = db.list_events_after(quest_id, after, &filter, 16)?;
        if fresh.iter().any(|e| {
            e.payload
                .as_ref()
                .and_then(|p| p["source"].as_str())
                .is_some_and(|s| s == strategy.as_str())
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How many times `await_brief` polls. Under `$Q_FIXTURE` no real Claude can
/// ever fire the hook, so the wait is bounded by `$Q_RESET_ITERATIONS`
/// (default 0 — proceed straight to the follow-up) the way `q events --follow`
/// is bounded by `$Q_FOLLOW_ITERATIONS`.
fn polls(strategy: Strategy) -> u32 {
    if env_set("Q_FIXTURE") {
        return std::env::var("Q_RESET_ITERATIONS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
    }
    (strategy.timeout().as_millis() / POLL.as_millis()) as u32
}

/// The Quest goal as a single short line, for `/compact <focus>`. `None` for a
/// Quest without one, which sends a bare `/compact`.
fn focus_of(goal: Option<&str>) -> Option<String> {
    // The focus is a tmux keystroke payload, and a control character typed into
    // a TUI is a key of its own (ESC leaves the prompt, CR submits) — so they
    // count as whitespace here and collapse away with the rest.
    let flat = goal?
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
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
        row.ctx_updated_at = ctx_pct.map(|_| crate::model::now());
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

        fn as_fn(&self) -> impl Fn(&[String]) -> Launch + '_ {
            |_argv| {
                self.0.set(self.0.get() + 1);
                Launch::Spawned
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

        // The next `Stop` does not schedule again: `ctx_pct` still reads high
        // but the reading predates the reset.
        assert!(schedule(&db, &session, &config, &launcher.as_fn()).is_none());
        assert_eq!(launcher.calls(), 1);
        assert_eq!(scheduled_events(&db, &quest.id).len(), 1);
    }

    /// A `Ctx` over the same in-memory fleet, with a tmux fixture holding the
    /// master's pane. Nothing here touches the process environment or a real
    /// tmux server.
    fn manual_fleet(
        status: SessionStatus,
        pane: &str,
    ) -> (Ctx, tempfile::TempDir, Quest, target::Target) {
        let (db, quest, mut session) = fleet(Some(40), status);
        if session.tmux_pane != pane {
            db.update_session_pane(&session.id, pane).unwrap();
            session = db.get_session(&session.id).unwrap().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux.json");
        std::fs::write(&path, "{}").unwrap();
        let ctx = Ctx::for_tests(
            Config::default(),
            db,
            Box::new(crate::tmux::FixtureTmux::new(path)),
        );
        let found = target::Target {
            quest: quest.clone(),
            session,
        };
        (ctx, dir, quest, found)
    }

    /// N-2: `Z` racing the `Stop` hook's own `--delay 2` schedule ran two
    /// children. The manual one clears now; the auto one wakes two seconds
    /// later, re-takes an idle gate the fresh `SessionStart` has just
    /// satisfied, and clears the *new* context window plus its follow-up
    /// (SPEC §8). The manual path takes the same cooldown the auto path does.
    #[test]
    fn a_manual_reset_will_not_double_fire_on_top_of_a_scheduled_one() {
        let (ctx, _dir, quest, found) = manual_fleet(SessionStatus::Idle, "%1");
        let launcher = Launcher::new();
        // The `Stop` hook gets there first.
        assert!(
            schedule(
                ctx.db().unwrap(),
                &found.session,
                &ctx.config,
                &launcher.as_fn()
            )
            .is_some()
        );
        assert_eq!(launcher.calls(), 1);

        let error = spawn_detached_with(&ctx, &found, Strategy::Clear, &launcher.as_fn())
            .expect_err("the manual reset was let through on top of the scheduled one");
        assert!(
            format!("{error:#}").contains("already has a reset scheduled"),
            "{error:#}"
        );
        assert_eq!(launcher.calls(), 1, "a second child was launched");
        assert_eq!(scheduled_events(ctx.db().unwrap(), &quest.id).len(), 1);
    }

    /// And two `Z` presses in a row are the same race with the same answer.
    #[test]
    fn a_second_manual_reset_inside_the_cooldown_is_refused() {
        let (ctx, _dir, quest, found) = manual_fleet(SessionStatus::Idle, "%1");
        let launcher = Launcher::new();
        spawn_detached_with(&ctx, &found, Strategy::Clear, &launcher.as_fn()).expect("first Z");
        assert!(spawn_detached_with(&ctx, &found, Strategy::Clear, &launcher.as_fn()).is_err());
        assert_eq!(launcher.calls(), 1);
        assert_eq!(scheduled_events(ctx.db().unwrap(), &quest.id).len(), 1);
    }

    /// R2-4: the same race one keystroke removed. `Z` took the cooldown but a
    /// `q reset` typed by hand in another terminal did not, so it fired a
    /// second child on top of the one the `Stop` hook had just scheduled.
    #[test]
    fn a_hand_typed_reset_will_not_double_fire_on_a_scheduled_one() {
        let (ctx, _dir, quest, found) = manual_fleet(SessionStatus::Idle, "%1");
        let launcher = Launcher::new();
        // The `Stop` hook gets there first.
        assert!(
            schedule(
                ctx.db().unwrap(),
                &found.session,
                &ctx.config,
                &launcher.as_fn()
            )
            .is_some()
        );

        let args = Args {
            session: &found.session.id,
            delay: None,
            strategy: Some(Strategy::Clear),
        };
        let error = run(&ctx, &args).expect_err("the hand-typed reset was let through");
        assert!(
            format!("{error:#}").contains("already has a reset scheduled"),
            "{error:#}"
        );
        // Refused before anything was typed at the master, and without a
        // second `session.reset_scheduled` of its own.
        let db = ctx.db().unwrap();
        assert!(
            db.list_events_by_kinds(&quest.id, &["session.reset"], 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(scheduled_events(db, &quest.id).len(), 1);
    }

    /// And the cooldown must not refuse the child that event is *about*: the
    /// scheduled path is the reset the hook asked for.
    #[test]
    fn the_scheduled_child_is_not_refused_by_the_event_that_scheduled_it() {
        let (ctx, _dir, quest, found) = manual_fleet(SessionStatus::Busy, "%1");
        let db = ctx.db().unwrap();
        db.append_event(
            &quest.id,
            Some(&found.session.id),
            "session.reset_scheduled",
            &serde_json::json!({ "delay": SCHEDULED_DELAY }),
        )
        .unwrap();

        let args = Args {
            session: &found.session.id,
            delay: Some(0),
            strategy: Some(Strategy::Clear),
        };
        // `busy` stops it at the idle gate — the point is that it got that far
        // instead of being turned away by its own cooldown.
        assert_eq!(run(&ctx, &args).unwrap(), 0);
        assert_eq!(
            db.list_events_by_kinds(&quest.id, &["session.reset_skipped"], 10)
                .unwrap()
                .len(),
            1
        );
    }

    /// N-6: the manual payload records whether the OS was actually involved,
    /// exactly as the auto path does. Under a suppressed launch it must not
    /// claim a child that never ran.
    #[test]
    fn a_manual_schedule_records_whether_it_was_really_spawned() {
        for (outcome, want) in [(Launch::Spawned, true), (Launch::Suppressed, false)] {
            let (ctx, _dir, quest, found) = manual_fleet(SessionStatus::Idle, "%1");
            spawn_detached_with(&ctx, &found, Strategy::Clear, &|_| outcome).expect("Z");
            let events = scheduled_events(ctx.db().unwrap(), &quest.id);
            let payload = events[0].payload.as_ref().unwrap();
            assert_eq!(payload["manual"], serde_json::json!(true));
            assert_eq!(payload["spawned"], serde_json::json!(want), "{outcome:?}");
        }
    }

    /// B1: a row whose window never opened has an empty pane, and tmux reads an
    /// empty `-t` target as "whatever is current". `spawn_detached` used to
    /// refuse this for a *master* only — the `role ==` conjunct was a slip, and
    /// every worker `Z` went straight past it.
    #[test]
    fn a_reset_of_a_row_with_no_pane_is_refused_whatever_its_role() {
        for role in [SessionRole::Master, SessionRole::Worker] {
            let (ctx, _dir, quest, mut found) = manual_fleet(SessionStatus::Idle, "");
            found.session.role = role;
            let launcher = Launcher::new();
            let error = spawn_detached_with(&ctx, &found, Strategy::Clear, &launcher.as_fn())
                .expect_err("a pane-less row was reset");
            assert!(format!("{error:#}").contains("has no pane"), "{error:#}");
            assert_eq!(launcher.calls(), 0);
            assert!(scheduled_events(ctx.db().unwrap(), &quest.id).is_empty());

            // And the synchronous `q reset` path refuses it too, rather than
            // typing `/clear` into whatever window is current.
            let args = Args {
                session: &found.session.id,
                delay: None,
                strategy: Some(Strategy::Clear),
            };
            let error = run(&ctx, &args).expect_err("a pane-less row was reset");
            assert!(format!("{error:#}").contains("has no pane"), "{error:#}");
        }
    }

    #[test]
    fn only_a_reading_taken_after_the_last_reset_counts() {
        // Never reset: nothing to be stale against.
        assert!(reading_is_fresh(None, None));
        assert!(reading_is_fresh(Some(10), None));
        // The reading has to be strictly newer than the reset.
        assert!(reading_is_fresh(Some(101), Some(100)));
        assert!(!reading_is_fresh(Some(100), Some(100)));
        assert!(!reading_is_fresh(Some(99), Some(100)));
        // A reading whose age is unknown is not evidence of a full window.
        assert!(!reading_is_fresh(None, Some(100)));
    }

    #[test]
    fn a_failed_spawn_neither_counts_as_scheduled_nor_starts_a_cooldown() {
        let (db, quest, session) = fleet(Some(90), SessionStatus::Idle);
        let config = Config::default();
        assert!(schedule(&db, &session, &config, &|_| Launch::Failed).is_none());
        assert!(scheduled_events(&db, &quest.id).is_empty());

        let failed = db
            .list_events_by_kinds(&quest.id, &["session.reset_failed"], 10)
            .unwrap();
        assert_eq!(failed.len(), 1);
        let payload = failed[0].payload.as_ref().unwrap();
        assert_eq!(payload["stage"], "spawn");
        assert_eq!(payload["strategy"], "clear");
        assert!(payload["argv"].is_array(), "{payload}");
        assert!(payload["spawned"].is_null(), "{payload}");

        // Nothing was suppressed, so the very next `Stop` tries again.
        let launcher = Launcher::new();
        assert!(schedule(&db, &session, &config, &launcher.as_fn()).is_some());
        assert_eq!(launcher.calls(), 1);
    }

    #[test]
    fn a_suppressed_spawn_still_counts_as_scheduled() {
        let (db, quest, session) = fleet(Some(90), SessionStatus::Idle);
        assert!(
            schedule(&db, &session, &Config::default(), &|_| {
                Launch::Suppressed
            })
            .is_some()
        );
        let events = scheduled_events(&db, &quest.id);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload.as_ref().unwrap()["spawned"], false);
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
        assert!(schedule(&db, &session, &Config::default(), &|_| Launch::Spawned).is_some());
    }

    #[test]
    fn a_worker_is_never_reset_however_full_it_is() {
        let (db, quest, mut session) = fleet(Some(99), SessionStatus::Idle);
        session.role = SessionRole::Worker;
        assert!(schedule(&db, &session, &Config::default(), &|_| Launch::Spawned).is_none());
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
                schedule(&db, &session, &Config::default(), &|_| Launch::Spawned).is_none(),
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
        assert!(schedule(&db, &session, &off, &|_| Launch::Spawned).is_none());

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
        assert!(schedule(&db, &session, &off, &|_| Launch::Spawned).is_some());

        let (db, _, session) = fleet(Some(90), SessionStatus::Idle);
        db.update_quest(
            &session.quest_id,
            &crate::db::quest::QuestPatch {
                auto_reset: Some(Some(false)),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(schedule(&db, &session, &Config::default(), &|_| Launch::Spawned).is_none());
    }

    #[test]
    fn the_quest_threshold_and_the_config_strategy_are_both_honoured() {
        let (db, quest, session) = fleet(Some(50), SessionStatus::Idle);
        let mut config = Config::default();
        config.context.master_reset_pct = 80;
        config.context.reset_strategy = "compact".to_string();
        assert!(schedule(&db, &session, &config, &|_| Launch::Spawned).is_none());

        db.update_quest(
            &quest.id,
            &crate::db::quest::QuestPatch {
                ctx_reset_pct: Some(Some(45)),
                ..Default::default()
            },
        )
        .unwrap();
        let argv = schedule(&db, &session, &config, &|_| Launch::Spawned).unwrap();
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
        // Every control character is a keystroke of its own once typed into a
        // TUI, so none may reach send-keys.
        let focus = focus_of(Some("ship\u{1b}[A it\u{7}\u{0}now\r\n")).unwrap();
        assert_eq!(focus, "ship [A it now");
        assert!(!focus.chars().any(char::is_control), "{focus:?}");
        assert_eq!(focus_of(Some("\u{1b}\u{7}\r\n\t")), None);
        let long = "ž".repeat(FOCUS_CHARS + 50);
        let focus = focus_of(Some(&long)).unwrap();
        assert_eq!(focus.chars().count(), FOCUS_CHARS);
        assert!(focus.ends_with('…'));
    }

    #[test]
    fn the_scheduled_argv_re_invokes_this_binary() {
        let argv = argv("s-0001", Strategy::Compact, SCHEDULED_DELAY).unwrap();
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
