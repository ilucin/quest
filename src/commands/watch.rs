//! `q watch [--interval N]` — the headless equivalent of the TUI tick (SPEC §6).
//!
//! Between hooks, a pane that dies fires nothing: only the liveness sweep can
//! notice it and end the row. The TUI runs that sweep on its clock; `q watch`
//! runs the very same sweep ([`tmux::sweep`], the one every command already
//! runs via [`crate::commands::sweep_quiet`]) on a loop with no terminal, and
//! turns each end it discovers into an `ended` notification. Waiting and reset
//! notifications are fired by the hooks themselves; watch only owns the ends a
//! hook can never see.
//!
//! No TUI, no tmux client required, and — unless `--json` — no output. Ctrl-C
//! (SIGINT) or SIGTERM stops the loop between ticks and exits 0, never a stack
//! dump.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::Ctx;
use crate::model::Session;
use crate::notify::{self, Kind, Runner};
use crate::tmux;

/// Set from the signal handler; the loop and its sleep both watch it.
static STOP: AtomicBool = AtomicBool::new(false);

pub fn run(ctx: &Ctx, interval: Option<u64>) -> anyhow::Result<u8> {
    // `[ui] tick_local` is the same clock the TUI's local sweep runs on; never
    // below a second, so a bad `--interval 0` cannot spin.
    let period = Duration::from_secs(interval.unwrap_or(ctx.config.ui.tick_local).max(1));
    STOP.store(false, Ordering::SeqCst);
    install_signals();

    let runner = notify::runner();
    while !STOP.load(Ordering::SeqCst) {
        tick(ctx, runner.as_ref());
        interruptible_sleep(period);
    }
    Ok(0)
}

/// One sweep-and-notify pass. Public within the crate so a test drives a single
/// tick against a fixture database without ever entering the loop.
pub fn tick(ctx: &Ctx, runner: &dyn Runner) {
    let Ok(db) = ctx.db() else { return };
    // The shared sweep, reused rather than reimplemented: it ends orphaned rows
    // and hands back exactly the sessions it ended.
    let Ok(ended) = tmux::sweep(db, ctx.tmux(), &ctx.config) else {
        return;
    };
    for session in &ended {
        let (title, body) = ended_text(ctx, session);
        notify::emit(&ctx.config.notify, runner, Kind::Ended, &title, &body);
        if ctx.json {
            let _ = crate::output::emit(
                true,
                &serde_json::json!({
                    "event": "ended",
                    "session": session.id,
                    "label": session.label,
                    "quest": session.quest_id,
                }),
                String::new,
            );
        }
    }
}

fn ended_text(ctx: &Ctx, session: &Session) -> (String, String) {
    let slug = ctx
        .db()
        .ok()
        .and_then(|db| db.get_quest(&session.quest_id).ok().flatten())
        .map(|q| q.slug)
        .unwrap_or_else(|| session.quest_id.clone());
    (
        format!("{slug} · ended"),
        format!("{} ended", session.label),
    )
}

/// Sleep the period in short slices so a signal is noticed promptly.
fn interruptible_sleep(period: Duration) {
    const SLICE: Duration = Duration::from_millis(100);
    let deadline = Instant::now() + period;
    while Instant::now() < deadline {
        if STOP.load(Ordering::SeqCst) {
            return;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(left.min(SLICE));
    }
}

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

/// Catch SIGINT / SIGTERM so the loop unwinds cleanly instead of the default
/// disposition tearing the process down. Idempotent.
fn install_signals() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        for sig in [libc::SIGINT, libc::SIGTERM] {
            // Safety: a zeroed sigaction with our handler filled in.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as usize;
                action.sa_flags = 0;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(sig, &action, std::ptr::null_mut());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::{Quest, Session, SessionRole, SessionStatus};
    use crate::notify::CaptureRunner;
    use crate::tmux::{FixtureState, FixtureTmux};
    use tempfile::TempDir;

    /// A quest with one live session whose pane is absent from tmux, so the
    /// sweep must end it.
    fn fixture() -> (TempDir, Ctx) {
        let dir = TempDir::new().unwrap();
        let db = Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("watch-me", "/tmp", "laptop"))
            .unwrap();
        let mut row = Session::new(&quest.id, SessionRole::Master, "master", "q-watchme", "%7");
        row.status = SessionStatus::Idle;
        db.insert_session(&row).unwrap();

        let tmux_path = dir.path().join("tmux.json");
        let tmux = FixtureTmux::new(&tmux_path);
        // No panes -> the pane %7 is gone -> the sweep ends the row.
        tmux.save(&FixtureState {
            panes: vec![],
            ..FixtureState::default()
        })
        .unwrap();

        let ctx = Ctx::for_tests(Config::default(), db, Box::new(tmux));
        (dir, ctx)
    }

    #[test]
    fn one_tick_ends_an_orphan_and_notifies_then_returns() {
        let (_dir, ctx) = fixture();
        let runner = CaptureRunner::new();
        // A single bounded tick — no loop, so the test cannot hang.
        tick(&ctx, &runner);

        let calls = runner.calls();
        assert_eq!(calls.len(), 1, "one ended notification");
        assert_eq!(calls[0].channel, "macos");
        assert!(calls[0].title.contains("watch-me"));
        assert!(calls[0].body.contains("ended"));

        // The row is actually ended now; a second tick finds no new orphan and
        // notifies nothing.
        let runner = CaptureRunner::new();
        tick(&ctx, &runner);
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn interruptible_sleep_returns_at_once_when_stop_is_set() {
        STOP.store(true, Ordering::SeqCst);
        let started = Instant::now();
        interruptible_sleep(Duration::from_secs(30));
        assert!(started.elapsed() < Duration::from_secs(1));
        STOP.store(false, Ordering::SeqCst);
    }
}
