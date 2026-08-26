//! The TUI (SPEC §17): tab shell, event loop and terminal lifecycle.
//!
//! Split so the interesting parts need no terminal:
//! * [`keys`] turns crossterm events into an `Input` alphabet,
//! * [`app`] is the pure key → state machine,
//! * [`layout`] holds the responsive arithmetic,
//! * `render` draws an `App` into any ratatui backend (`TestBackend` included),
//! * only `run` below talks to a real terminal.
//!
//! The tab bodies (`quests`, `sessions`, `templates`, `events`) are stubs that
//! later beads fill in against the seams they already expose.

pub mod app;
pub mod events;
pub mod keys;
pub mod layout;
pub mod quests;
pub mod sessions;
pub mod templates;

use std::io::{self, Stdout, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, poll, read as read_event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::Ctx;

use app::{Action, App, Tab};

/// Bare `q` (SPEC §16). Enters the alternate screen, runs the loop, and
/// restores the terminal on every exit path — including a panic.
pub fn run(ctx: &Ctx) -> anyhow::Result<()> {
    let mut app = App::new(&ctx.config, ctx.machine());
    let tick = Duration::from_secs(app.tick_secs);
    let mouse = app.mouse;

    let (guard, mut terminal) = enter(mouse)?;
    let result = event_loop(ctx, &mut terminal, &mut app, tick);
    // Explicit so the terminal is back to normal before `main` prints an error
    // into it; a panic or a `?` above drops the guard just the same.
    drop(guard);
    result
}

// ------------------------------------------------------------ terminal state

/// What has actually been switched on, so the guard and the panic hook undo
/// exactly that much — from either, in either order.
static RAW_ON: AtomicBool = AtomicBool::new(false);
static ALT_ON: AtomicBool = AtomicBool::new(false);
static MOUSE_ON: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

/// The terminal-level effects the lifecycle performs, behind a trait so
/// `arm`'s failure paths, the guard and `restore`'s undo order are testable
/// without a tty — the half of this module a `TestBackend` cannot reach.
trait TermIo {
    fn raw(&mut self, on: bool) -> io::Result<()>;
    fn alt(&mut self, on: bool) -> io::Result<()>;
    fn mouse(&mut self, on: bool) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
}

impl<T: TermIo + ?Sized> TermIo for &mut T {
    fn raw(&mut self, on: bool) -> io::Result<()> {
        (**self).raw(on)
    }
    fn alt(&mut self, on: bool) -> io::Result<()> {
        (**self).alt(on)
    }
    fn mouse(&mut self, on: bool) -> io::Result<()> {
        (**self).mouse(on)
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        (**self).show_cursor()
    }
    fn flush(&mut self) -> io::Result<()> {
        (**self).flush()
    }
}

/// The real terminal. Stateless, so the panic hook can build one.
#[derive(Debug)]
struct Stdio;

impl TermIo for Stdio {
    fn raw(&mut self, on: bool) -> io::Result<()> {
        if on {
            enable_raw_mode()
        } else {
            disable_raw_mode()
        }
    }

    fn alt(&mut self, on: bool) -> io::Result<()> {
        let mut out = io::stdout();
        if on {
            execute!(out, EnterAlternateScreen)
        } else {
            execute!(out, LeaveAlternateScreen)
        }
    }

    fn mouse(&mut self, on: bool) -> io::Result<()> {
        let mut out = io::stdout();
        if on {
            execute!(out, EnableMouseCapture)
        } else {
            execute!(out, DisableMouseCapture)
        }
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }
}

/// Switches the terminal back however the TUI ends — `?`, break, or unwind.
#[derive(Debug)]
struct Guard<T: TermIo> {
    io: T,
}

impl<T: TermIo> Drop for Guard<T> {
    fn drop(&mut self) {
        restore_with(&mut self.io);
    }
}

fn enter(mouse: bool) -> anyhow::Result<(Guard<Stdio>, Terminal<CrosstermBackend<Stdout>>)> {
    install_hook();
    let guard = arm(Stdio, mouse)?;
    // `Terminal::new` asks the backend for its size — a fallible ioctl. The
    // guard is armed by now, so failing here still restores.
    let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    Ok((guard, terminal))
}

/// Switch the terminal into TUI mode, handing back the guard that switches it
/// off again.
///
/// Every step after the first is an exit path of its own — raw mode is already
/// on and the caller has nothing to drop yet — so a failure undoes the whole
/// sequence here rather than leaving the shell in the alternate screen.
fn arm<T: TermIo>(mut io: T, mouse: bool) -> anyhow::Result<Guard<T>> {
    match arm_steps(&mut io, mouse) {
        Ok(()) => Ok(Guard { io }),
        Err(e) => {
            restore_with(&mut io);
            Err(e.into())
        }
    }
}

/// Each flag is set *before* the call that applies it, so a step that failed
/// half-written is still undone.
fn arm_steps<T: TermIo>(io: &mut T, mouse: bool) -> io::Result<()> {
    RAW_ON.store(true, Ordering::SeqCst);
    io.raw(true)?;
    ALT_ON.store(true, Ordering::SeqCst);
    io.alt(true)?;
    if mouse {
        MOUSE_ON.store(true, Ordering::SeqCst);
        io.mouse(true)?;
    }
    Ok(())
}

/// A terminal left in raw mode is the worst way for this to fail, so the hook
/// goes in before raw mode does.
fn install_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
}

fn restore() {
    restore_with(&mut Stdio);
}

/// Undoes exactly what is on, and nothing when nothing is: safe from the panic
/// hook and the guard, in either order.
fn restore_with<T: TermIo>(io: &mut T) {
    let mouse = MOUSE_ON.swap(false, Ordering::SeqCst);
    let alt = ALT_ON.swap(false, Ordering::SeqCst);
    let raw = RAW_ON.swap(false, Ordering::SeqCst);
    if !(mouse || alt || raw) {
        return;
    }
    // First, and whatever else is on: ratatui hides the cursor on every draw,
    // and `LeaveAlternateScreen` restores the screen buffer, not DECTCEM
    // visibility — that is a global attribute. Without this a panic leaves an
    // invisible cursor behind, which reads as a hung shell.
    let _ = io.show_cursor();
    if mouse {
        let _ = io.mouse(false);
    }
    if alt {
        let _ = io.alt(false);
    }
    if raw {
        let _ = io.raw(false);
    }
    let _ = io.flush();
}

// ------------------------------------------------------------------ the loop

/// Next tick deadline. A fixed period from the last one so the tick does not
/// drift by however long the refresh took, resynced to now when we fell more
/// than a whole period behind (a slow refresh, a suspended process) so a
/// backlog never fires as a burst of catch-up ticks.
fn advance_tick(last: Instant, now: Instant, tick: Duration) -> Instant {
    let next = last + tick;
    if now.saturating_duration_since(next) >= tick {
        now
    } else {
        next
    }
}

/// Reduce a crossterm event to a state transition, plus whether anything the
/// renderer can see changed. The second half is not `action != Action::None`:
/// switching tabs returns `Action::None` and very much needs a redraw, while a
/// mouse report the alphabet drops needs none.
fn apply_event(app: &mut App, ev: Event) -> (Action, bool) {
    match ev {
        Event::Key(key) if key.kind != KeyEventKind::Release => match keys::normalize(key) {
            Some(input) => (app.handle(input), true),
            None => (Action::None, false),
        },
        Event::Mouse(m) if app.mouse => match keys::normalize_mouse(m) {
            Some(input) => (app.handle_mouse(input), true),
            None => (Action::None, false),
        },
        Event::Resize(w, h) => {
            app.set_size(w, h);
            (Action::None, true)
        }
        _ => (Action::None, false),
    }
}

fn event_loop(
    ctx: &Ctx,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    tick: Duration,
) -> anyhow::Result<()> {
    refresh_now(ctx, app);
    let mut last_tick = Instant::now();
    let mut dirty = true;

    loop {
        // Mouse capture is ANY-MOTION tracking (CSI ?1003h), so without this
        // an idle mouse crossing the window costs a full render pass per
        // report.
        if dirty {
            sync_now(ctx, app);
            terminal.draw(|frame| render(frame, app))?;
            dirty = false;
        }

        // Block until the next tick is due rather than spinning on a zero
        // timeout; a TUI that burns a core while idle is a bug.
        let timeout = tick.saturating_sub(last_tick.elapsed());
        let mut refresh_due = false;
        if poll(timeout)? {
            let (action, changed) = apply_event(app, read_event()?);
            dirty |= changed;
            match action {
                Action::Quit => break,
                Action::Refresh => refresh_due = true,
                Action::None => {}
            }
        }

        let now = Instant::now();
        if now.saturating_duration_since(last_tick) >= tick {
            last_tick = advance_tick(last_tick, now, tick);
            app.tick();
            refresh_due = true;
        }

        // One call site, so `x` on a due tick cannot refresh twice.
        if refresh_due {
            refresh_now(ctx, app);
            dirty = true;
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

fn refresh_now(ctx: &Ctx, app: &mut App) {
    let result = refresh(ctx, app);
    report_refresh(app, result);
}

/// The redraw preamble: bring the active tab's per-selection data in line with
/// the selection before it is drawn. Cheap by design — a no-op unless a key
/// moved the selection since the last frame — so moving down a list costs one
/// indexed query rather than the whole reload a tick does.
///
/// A failure only *sets* the error: it must not clear a reload failure that is
/// the actual reason the screen is stale.
fn sync_now(ctx: &Ctx, app: &mut App) {
    let result = match app.tab {
        Tab::Quests => quests::sync(ctx, app),
        _ => Ok(()),
    };
    if let Err(e) = result {
        app.refresh_error = Some(format!("refresh failed: {e:#}"));
    }
}

/// A failed reload belongs in the status bar, not in `main`'s error path: a
/// transient `SQLITE_BUSY` from another `q` process or from a hook handler
/// holding the write lock must not drop the user out of the TUI. Returning
/// `()` is the point — there is no `?` for the loop to take.
///
/// It lands in `refresh_error`, not in `status`: `refresh_now` runs on every
/// tick, so clearing `status` here would wipe whatever a keypress had just
/// put there, within a tick and without the user having touched anything.
fn report_refresh(app: &mut App, result: anyhow::Result<()>) {
    app.refresh_error = match result {
        Ok(()) => None,
        Err(e) => Some(format!("refresh failed: {e:#}")),
    };
}

/// Reload the active tab. The tabs own their loading; the shell only decides
/// when — which is what keeps `App::handle` free of I/O.
fn refresh(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    match app.tab {
        Tab::Quests => quests::refresh(ctx, app),
        Tab::Sessions => sessions::refresh(ctx, app),
        Tab::Templates => templates::refresh(ctx, app),
        Tab::Events => events::refresh(ctx, app),
    }
}

// ------------------------------------------------------------------ the view

/// Draw the whole shell. Backend-agnostic on purpose: the rendering tests run
/// this against `TestBackend` and assert the buffer.
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    app.set_size(area.width, area.height);
    let chrome = layout::chrome(area);

    render_header(frame, chrome.header, app);
    match app.tab {
        Tab::Quests => quests::render(frame, chrome.body, app),
        Tab::Sessions => sessions::render(frame, chrome.body, app),
        Tab::Templates => templates::render(frame, chrome.body, app),
        Tab::Events => events::render(frame, chrome.body, app),
    }
    render_status(frame, chrome.status, app);

    if app.help {
        render_help(frame, area, app.tab);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 {
        app.tab_bar_width = 0;
        return;
    }
    let machine = format!(" {} ", app.machine);
    let [bar, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(layout::right_segment(
            area.width,
            layout::width(&machine) as u16,
        )),
    ])
    .areas(area);
    // Hit-testing must agree with what was drawn, not with what would have
    // been drawn on an unbounded row.
    app.tab_bar_width = bar.width;

    let mut spans = vec![Span::raw(" ")];
    for (i, (tab, label, _, _)) in app::tab_layout().into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(app::TAB_SEP).dim());
        }
        let style = if tab == app.tab {
            Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), bar);
    frame.render_widget(
        Paragraph::new(Line::from(
            Span::raw(layout::truncate(&machine, right.width as usize)).bold(),
        )),
        right,
    );
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let hint = " ? help · x refresh · q quit ";
    // A reload that failed outranks a keypress's feedback: it is the reason
    // what is on screen may be stale.
    let left = match (app.refresh_error.as_deref(), app.current_status()) {
        (Some(e), _) => format!(" {e}"),
        (None, Some(status)) => format!(" {status}"),
        (None, None) => format!(
            " {} · rows {} · tick {}s · mouse {}",
            app.machine,
            app.row_mode().lines(),
            app.tick_secs,
            if app.mouse { "on" } else { "off" },
        ),
    };
    // The filters are a mode, not a message: they lead the line and stay there
    // until they are turned off, whatever else has something to say.
    let filters = app.filters();
    let left = if filters.is_empty() {
        left
    } else {
        format!(" [{filters}]{left}")
    };

    let [l, r] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(layout::right_segment(
            area.width,
            layout::width(hint) as u16,
        )),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::raw(layout::truncate(
            &left,
            l.width as usize,
        )))),
        l,
    );
    frame.render_widget(
        Paragraph::new(Line::from(
            Span::raw(layout::truncate(hint, r.width as usize)).dim(),
        )),
        r,
    );
}

fn render_help(frame: &mut Frame, area: Rect, tab: Tab) {
    let rows = app::help_rows(tab);
    let key_width = rows
        .iter()
        .map(|(k, _)| layout::width(k))
        .max()
        .unwrap_or(0);
    let inner_width = rows
        .iter()
        .map(|(_, d)| key_width + 2 + layout::width(d))
        .max()
        .unwrap_or(0);
    let box_area = layout::centered(area, (inner_width + 4) as u16, (rows.len() + 2) as u16);

    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            Line::from(vec![
                Span::raw(format!("{k:>key_width$}")).bold(),
                Span::raw("  "),
                Span::raw(*d),
            ])
        })
        .collect();

    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Keys ")
                .padding(ratatui::widgets::Padding::horizontal(1)),
        ),
        box_area,
    );
}

/// Shared body for the tab stubs, so all four say the same thing the same way.
pub(crate) fn placeholder(frame: &mut Frame, area: Rect, title: &str, bead: &str, app: &App) {
    if area.height == 0 {
        return;
    }
    let body = vec![
        Line::from(Span::raw(format!("{title} tab")).bold()),
        Line::from(""),
        Line::from(Span::raw(format!("not built yet — lands in {bead}")).dim()),
        Line::from(Span::raw(format!("machine {}", app.machine)).dim()),
    ];
    frame.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .padding(ratatui::widgets::Padding::uniform(1)),
        ),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tui::keys::{Input, MouseInput};
    use crossterm::event::{
        KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::backend::TestBackend;
    use std::sync::{Mutex, MutexGuard};

    fn draw(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn app() -> App {
        App::new(&Config::default(), "laptop")
    }

    // ------------------------------------------------------ terminal lifecycle
    //
    // The statics `arm`/`restore` drive are process-global, so these tests take
    // a lock and always leave them clear.

    static LIFECYCLE: Mutex<()> = Mutex::new(());

    fn lifecycle_lock() -> MutexGuard<'static, ()> {
        let guard = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
        clear_flags();
        guard
    }

    fn clear_flags() {
        RAW_ON.store(false, Ordering::SeqCst);
        ALT_ON.store(false, Ordering::SeqCst);
        MOUSE_ON.store(false, Ordering::SeqCst);
    }

    fn flags() -> (bool, bool, bool) {
        (
            RAW_ON.load(Ordering::SeqCst),
            ALT_ON.load(Ordering::SeqCst),
            MOUSE_ON.load(Ordering::SeqCst),
        )
    }

    /// Records the escape/termios steps instead of performing them, and can be
    /// told to fail at one of them.
    #[derive(Debug, Default)]
    struct FakeTerm {
        calls: Vec<&'static str>,
        fail_on: Option<&'static str>,
    }

    impl FakeTerm {
        fn failing_at(step: &'static str) -> FakeTerm {
            FakeTerm {
                calls: Vec::new(),
                fail_on: Some(step),
            }
        }

        fn step(&mut self, name: &'static str) -> io::Result<()> {
            self.calls.push(name);
            if self.fail_on == Some(name) {
                return Err(io::Error::other(name));
            }
            Ok(())
        }
    }

    impl TermIo for FakeTerm {
        fn raw(&mut self, on: bool) -> io::Result<()> {
            self.step(if on { "raw on" } else { "raw off" })
        }
        fn alt(&mut self, on: bool) -> io::Result<()> {
            self.step(if on { "alt on" } else { "alt off" })
        }
        fn mouse(&mut self, on: bool) -> io::Result<()> {
            self.step(if on { "mouse on" } else { "mouse off" })
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.step("cursor show")
        }
        fn flush(&mut self) -> io::Result<()> {
            self.step("flush")
        }
    }

    /// Everything `run` does to the terminal, end to end, with the guard
    /// dropped the way the loop returning drops it.
    #[test]
    fn the_guard_switches_back_exactly_what_arm_switched_on() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        {
            let _guard = arm(&mut term, true).expect("arm");
            assert_eq!(flags(), (true, true, true));
        }
        assert_eq!(
            term.calls,
            [
                "raw on",
                "alt on",
                "mouse on",
                "cursor show",
                "mouse off",
                "alt off",
                "raw off",
                "flush",
            ]
        );
        assert_eq!(flags(), (false, false, false));
    }

    #[test]
    fn mouse_capture_is_left_alone_when_the_config_says_so() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        {
            let _guard = arm(&mut term, false).expect("arm");
            assert_eq!(flags(), (true, true, false));
        }
        // Nothing is undone that was never switched on.
        assert_eq!(
            term.calls,
            [
                "raw on",
                "alt on",
                "cursor show",
                "alt off",
                "raw off",
                "flush"
            ]
        );
    }

    /// B1: raw mode and the alternate screen are on before the last two steps,
    /// and the caller has no guard to drop yet. A failure there used to return
    /// `Err` straight past `main`, leaving the shell in the alternate screen,
    /// in raw mode.
    #[test]
    fn a_failure_part_way_through_arm_leaves_the_terminal_clean() {
        let _lock = lifecycle_lock();
        for (mouse, step, want) in [
            (
                true,
                "raw on",
                vec!["raw on", "cursor show", "raw off", "flush"],
            ),
            (
                true,
                "alt on",
                vec![
                    "raw on",
                    "alt on",
                    "cursor show",
                    "alt off",
                    "raw off",
                    "flush",
                ],
            ),
            (
                true,
                "mouse on",
                vec![
                    "raw on",
                    "alt on",
                    "mouse on",
                    "cursor show",
                    "mouse off",
                    "alt off",
                    "raw off",
                    "flush",
                ],
            ),
            (
                false,
                "alt on",
                vec![
                    "raw on",
                    "alt on",
                    "cursor show",
                    "alt off",
                    "raw off",
                    "flush",
                ],
            ),
        ] {
            clear_flags();
            let mut term = FakeTerm::failing_at(step);
            let err = match arm(&mut term, mouse) {
                Ok(_) => panic!("arm must fail at {step}"),
                Err(e) => e,
            };
            assert!(err.to_string().contains(step), "{err} at {step}");
            assert_eq!(term.calls, want, "failing at {step}");
            assert_eq!(flags(), (false, false, false), "failing at {step}");
        }
    }

    /// B2: ratatui hides the cursor on every draw and `LeaveAlternateScreen`
    /// does not bring it back, so `restore` must — otherwise the panic hook
    /// hands back a shell that looks hung.
    #[test]
    fn restore_shows_the_cursor_before_anything_else() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();

        restore_with(&mut term);
        assert_eq!(term.calls[0], "cursor show");
        assert!(term.calls.contains(&"alt off"));
    }

    /// The panic hook and the guard both restore, in whichever order they run.
    #[test]
    fn restoring_twice_is_a_no_op_the_second_time() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);

        restore_with(&mut term);
        assert_eq!(flags(), (false, false, false));
        term.calls.clear();
        restore_with(&mut term);
        assert!(term.calls.is_empty(), "{:?}", term.calls);
        // And with nothing ever armed.
        restore_with(&mut term);
        assert!(term.calls.is_empty(), "{:?}", term.calls);
    }

    // ------------------------------------------------------------- the loop

    /// N2: a reload that fails is a status line, not the end of the session.
    #[test]
    fn a_failed_refresh_lands_in_the_status_bar() {
        let mut app = app();
        report_refresh(&mut app, Err(anyhow::anyhow!("database is locked")));
        assert_eq!(
            app.refresh_error.as_deref(),
            Some("refresh failed: database is locked")
        );
        let lines = draw(&mut app, 100, 10);
        assert!(
            lines
                .last()
                .unwrap()
                .contains("refresh failed: database is locked"),
            "{lines:?}"
        );

        // And the next reload that works takes the message away again — the
        // old "refreshing…" had nothing to clear it at all.
        report_refresh(&mut app, Ok(()));
        assert!(app.refresh_error.is_none());
        let lines = draw(&mut app, 100, 10);
        assert!(lines.last().unwrap().contains("rows 2"), "{lines:?}");
    }

    /// The seam bd-8lz.4.1 left for the tabs that load data: a tick's success
    /// must not wipe what a keypress put in the status bar.
    #[test]
    fn a_successful_reload_leaves_a_keypress_message_alone() {
        let mut app = app();
        app.say("close cdc-backfill: lands in bd-8lz.4.4");
        for _ in 0..3 {
            report_refresh(&mut app, Ok(()));
        }
        assert_eq!(app.status, "close cdc-backfill: lands in bd-8lz.4.4");
        let lines = draw(&mut app, 100, 10);
        assert!(lines.last().unwrap().contains("bd-8lz.4.4"), "{lines:?}");
        // A failure outranks it while it lasts, and does not destroy it.
        report_refresh(&mut app, Err(anyhow::anyhow!("locked")));
        let lines = draw(&mut app, 100, 10);
        assert!(
            lines.last().unwrap().contains("refresh failed"),
            "{lines:?}"
        );
        report_refresh(&mut app, Ok(()));
        let lines = draw(&mut app, 100, 10);
        assert!(lines.last().unwrap().contains("bd-8lz.4.4"), "{lines:?}");
    }

    #[test]
    fn the_tick_deadline_holds_its_period_and_resyncs_after_a_stall() {
        let tick = Duration::from_secs(2);
        let t0 = Instant::now();
        // On the grid, and a little late: the next deadline does not slide by
        // however long the refresh took.
        assert_eq!(advance_tick(t0, t0 + tick, tick), t0 + tick);
        assert_eq!(
            advance_tick(t0, t0 + tick + Duration::from_millis(300), tick),
            t0 + tick
        );
        // More than a whole period behind: resync instead of firing a burst of
        // catch-up ticks.
        let late = t0 + Duration::from_secs(30);
        assert_eq!(advance_tick(t0, late, tick), late);
    }

    #[test]
    fn only_events_that_change_something_ask_for_a_redraw() {
        let mut app = app();
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 3,
                row: 0,
                modifiers: KeyModifiers::NONE,
            })
        };
        // Mouse capture is ANY-MOTION tracking: motion reports arrive
        // constantly and must not cost a render pass.
        assert_eq!(
            apply_event(&mut app, mouse(MouseEventKind::Moved)),
            (Action::None, false)
        );
        // A key with no letter in the alphabet is just as free.
        let f1 = Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
        assert_eq!(apply_event(&mut app, f1), (Action::None, false));

        // A tab switch returns `Action::None` too, and absolutely needs a redraw.
        let tab = Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(apply_event(&mut app, tab), (Action::None, true));
        assert_eq!(app.tab, Tab::Sessions);
        assert_eq!(
            apply_event(&mut app, Event::Resize(80, 24)),
            (Action::None, true)
        );
        assert_eq!((app.width, app.height), (80, 24));

        // `x` is the one that also reloads.
        let x = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(apply_event(&mut app, x), (Action::Refresh, true));

        // With `[ui] mouse = false` the loop never even captures these.
        app.mouse = false;
        assert_eq!(
            apply_event(&mut app, mouse(MouseEventKind::Down(MouseButton::Left))),
            (Action::None, false)
        );
    }

    #[test]
    fn a_key_release_is_dropped_rather_than_firing_the_binding_twice() {
        let mut app = app();
        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = crossterm::event::KeyEventKind::Release;
        assert_eq!(
            apply_event(&mut app, Event::Key(release)),
            (Action::None, false)
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn tab_clicks_land_on_the_labels_actually_drawn() {
        let mut app = app();
        let lines = draw(&mut app, 100, 20);
        let header = &lines[0];
        for (tab, label, x, w) in app::tab_layout() {
            let drawn: String = header
                .chars()
                .skip(x as usize)
                .take(w as usize)
                .collect::<String>();
            assert_eq!(
                drawn, label,
                "{tab:?} is not drawn where hit-testing puts it"
            );
            assert_eq!(app::tab_at_column(x, app.tab_bar_width), Some(tab));
            assert_eq!(app::tab_at_column(x + w - 1, app.tab_bar_width), Some(tab));
        }
    }

    /// N5: the header splits the row, so the bar can be narrower than the
    /// labels `tab_layout` lays out. A click past the clip is on the machine
    /// name, not on the tab whose label was cut off there.
    #[test]
    fn a_click_past_the_clipped_tab_bar_selects_nothing() {
        let mut app = app();
        draw(&mut app, 50, 10);
        // `" laptop "` takes 8 of the 50 columns.
        assert_eq!(app.tab_bar_width, 42);
        let (events, _, x, w) = app::tab_layout().into_iter().last().unwrap();
        assert!(x < app.tab_bar_width, "Events starts inside the bar");
        assert!(x + w > app.tab_bar_width, "and is drawn clipped");

        // On the visible part of the clipped label: still Events.
        app.handle_mouse(MouseInput::Click {
            col: app.tab_bar_width - 1,
            row: 0,
        });
        assert_eq!(app.tab, events);

        // On the machine label — visually the clipped `4 Events` — nothing moves.
        app.tab = Tab::Quests;
        for col in [app.tab_bar_width, app.tab_bar_width + 1, x + w - 1] {
            app.handle_mouse(MouseInput::Click { col, row: 0 });
            assert_eq!(app.tab, Tab::Quests, "col {col}");
        }
    }

    /// N6: `[machine] name` has no length cap, and a fixed `Length` for the
    /// right-hand segment used to hand the tab bar zero columns.
    #[test]
    fn a_machine_name_longer_than_the_terminal_cannot_wipe_the_tab_bar() {
        let mut app = App::new(&Config::default(), &"m".repeat(300));
        let lines = draw(&mut app, 60, 10);
        assert_eq!(app.tab_bar_width, 20);
        assert!(lines[0].starts_with(" 1 Quests"), "{:?}", lines[0]);
        assert!(lines[0].contains('m'), "{:?}", lines[0]);
        // Clicks still land on the tabs that survived the clip.
        assert_eq!(
            app::tab_at_column(1, app.tab_bar_width),
            Some(Tab::Quests),
            "{:?}",
            lines[0]
        );
        assert_eq!(app::tab_at_column(39, app.tab_bar_width), None);
    }

    #[test]
    fn a_narrow_status_bar_still_shows_the_machine_readout() {
        let mut app = app();
        let lines = draw(&mut app, 24, 5);
        let status = lines.last().unwrap();
        assert!(status.contains("lap"), "{status:?}");
        assert!(status.contains("help"), "{status:?}");
    }

    /// N8: the chrome arithmetic and both `Layout` splits, swept over the
    /// sizes that actually break constraint solvers.
    #[test]
    fn every_terminal_size_renders_without_panicking() {
        for w in [1u16, 2, 20, 70, 100, 200] {
            for h in [1u16, 2, 3, 24] {
                for help in [false, true] {
                    for tab in Tab::ALL {
                        let mut app = app();
                        app.tab = tab;
                        app.help = help;
                        // The sweep itself is the assertion: `draw` panics on
                        // a widget written outside its area, and every band of
                        // the chrome is exercised at every breakpoint.
                        let lines = draw(&mut app, w, h);
                        // The chrome always wins the last line it was given:
                        // a body that overran would have taken it.
                        if h >= 2 && w >= 40 {
                            let status = lines.last().unwrap();
                            assert!(
                                status.contains("q quit") || help,
                                "{w}x{h} {tab:?} help={help}: {status:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_header_lists_every_tab_and_the_machine() {
        let mut app = app();
        let lines = draw(&mut app, 100, 20);
        let header = &lines[0];
        for (tab, label, _, _) in app::tab_layout() {
            assert!(header.contains(&label), "{tab:?} missing from {header:?}");
        }
        assert!(header.contains("laptop"), "{header:?}");
    }

    #[test]
    fn the_body_follows_the_selected_tab() {
        let mut app = app();
        // The Quests tab is built (bd-8lz.4.2); with an empty database it
        // draws its own empty state rather than the stub.
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("no open quests"), "{body}");

        app.handle(Input::Char('4'));
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("Events tab"), "{body}");
        assert!(!body.contains("no open quests"), "{body}");

        app.handle(Input::Char('1'));
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("no open quests"), "{body}");
        assert!(!body.contains("Events tab"), "{body}");
    }

    #[test]
    fn the_status_bar_reports_orientation() {
        let mut app = app();
        let lines = draw(&mut app, 100, 20);
        let status = lines.last().unwrap();
        assert!(status.contains("laptop"), "{status:?}");
        assert!(status.contains("rows 2"), "{status:?}");
        assert!(status.contains("tick 2s"), "{status:?}");
        assert!(status.contains("mouse on"), "{status:?}");
        assert!(status.contains("? help"), "{status:?}");
        assert!(status.contains("q quit"), "{status:?}");
    }

    #[test]
    fn the_status_bar_reports_the_narrow_row_mode() {
        let mut app = app();
        let lines = draw(&mut app, 60, 20);
        assert!(lines.last().unwrap().contains("rows 3"), "{lines:?}");
    }

    #[test]
    fn the_help_overlay_covers_the_body_and_lists_the_bindings() {
        let mut app = app();
        app.handle(Input::Char('?'));
        // Wide enough for the widest row, so the overlay spans the body.
        let rendered = draw(&mut app, 68, 26).join("\n");
        assert!(rendered.contains("Keys"), "{rendered}");
        for (key, desc) in app::help_rows(app.tab) {
            assert!(rendered.contains(key), "missing key {key}: {rendered}");
            assert!(rendered.contains(desc), "missing desc {desc}: {rendered}");
        }
        // `Clear` really wiped what was underneath.
        assert!(!rendered.contains("n starts one"), "{rendered}");

        // Dismissing it brings the body back.
        app.handle(Input::Esc);
        let rendered = draw(&mut app, 68, 26).join("\n");
        assert!(rendered.contains("n starts one"), "{rendered}");
        assert!(!rendered.contains("Keys"), "{rendered}");
    }

    /// The overlay is the shell's keys plus the active tab's own.
    #[test]
    fn the_help_overlay_follows_the_tab() {
        let mut app = app();
        app.help = true;
        let quests = draw(&mut app, 90, 30).join("\n");
        assert!(quests.contains("cycle the machine filter"), "{quests}");
        app.help = false;
        app.handle(Input::Char('4'));
        app.help = true;
        let events = draw(&mut app, 90, 30).join("\n");
        assert!(!events.contains("cycle the machine filter"), "{events}");
        assert!(events.contains("next / previous tab"), "{events}");
    }

    #[test]
    fn rendering_tracks_the_terminal_size() {
        let mut app = app();
        draw(&mut app, 120, 40);
        assert_eq!((app.width, app.height), (120, 40));
        assert_eq!(app.row_mode(), layout::RowMode::Two);
        draw(&mut app, 55, 12);
        assert_eq!((app.width, app.height), (55, 12));
        assert_eq!(app.row_mode(), layout::RowMode::Three);
    }

    #[test]
    fn a_terminal_too_small_for_the_chrome_still_renders() {
        let mut app = app();
        // One row: header only, no body and no status bar.
        let lines = draw(&mut app, 20, 1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("Quests"), "{lines:?}");
        // Two rows, and the help overlay on top of them.
        app.handle(Input::Char('?'));
        let lines = draw(&mut app, 20, 2);
        assert_eq!(lines.len(), 2);
    }
}
