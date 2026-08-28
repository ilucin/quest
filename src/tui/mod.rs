//! The TUI (SPEC §17): tab shell, event loop and terminal lifecycle.
//!
//! Split so the interesting parts need no terminal:
//! * [`keys`] turns crossterm events into an `Input` alphabet,
//! * [`app`] is the pure key → state machine,
//! * [`form`] is the reusable modal input layer every prompt is built from,
//! * [`layout`] holds the responsive arithmetic,
//! * `render` draws an `App` into any ratatui backend (`TestBackend` included),
//! * only `run` below talks to a real terminal, and [`handoff`] is the one
//!   place that hands it to somebody else (tmux, a pager) and takes it back.
//!
//! The tab bodies (`quests`, `sessions`, `templates`, `events`) own their own
//! loading, keymap and rendering; the shell only decides when each runs.

pub mod app;
pub mod events;
pub mod form;
pub mod keys;
pub mod layout;
pub mod pager;
pub mod quests;
pub mod sessions;
mod signals;
pub mod templates;

use std::io::{self, Stdout, Write};
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::Show;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind, poll, read as read_event,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::Ctx;
use crate::brief;
use crate::commands::{enter, peek};
use crate::remote;

use app::{Action, App, Tab};

/// Bare `q` (SPEC §16). Enters the alternate screen, runs the loop, and
/// restores the terminal on every exit path — including a panic.
pub fn run(ctx: &Ctx) -> anyhow::Result<()> {
    let mut app = App::new(&ctx.config, ctx.machine());
    let tick = Duration::from_secs(app.tick_secs);
    let mouse = app.mouse;
    // SPEC §17's second clock. Started before the alternate screen so its
    // first round is already in flight while the first frame is drawn; it
    // never touches the terminal, and dropping it stops it.
    let poller = remote::Poller::spawn(ctx, Duration::from_secs(ctx.config.ui.tick_remote.max(1)));

    let (guard, mut terminal) = enter(mouse)?;
    let result = event_loop(ctx, &mut terminal, &mut app, tick, poller.as_ref());
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
/// Bracketed paste (CSI ?2004h). Not optional the way the mouse is: with it
/// off the terminal hands pasted bytes over as if they had been typed, so a
/// `ESC [ C` in a paste is an arrow key that walks a guarded action row off
/// `cancel` and the `CR` after it submits. With it on the same bytes arrive as
/// one `Event::Paste`, which is text and nothing else.
static PASTE_ON: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

/// The terminal-level effects the lifecycle performs, behind a trait so
/// `arm`'s failure paths, the guard and `restore`'s undo order are testable
/// without a tty — the half of this module a `TestBackend` cannot reach.
trait TermIo {
    fn raw(&mut self, on: bool) -> io::Result<()>;
    fn alt(&mut self, on: bool) -> io::Result<()>;
    fn mouse(&mut self, on: bool) -> io::Result<()>;
    fn paste(&mut self, on: bool) -> io::Result<()>;
    fn show_cursor(&mut self) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    /// One line on the terminal this process is about to give away for good.
    ///
    /// Only [`land`]'s exec shape uses it, and only after the TUI is off the
    /// screen: nothing will be drawn again, so the status bar is not a place
    /// to say anything. Written where the CLI writes the same lines — stderr,
    /// the way `q tpl run` flushes its warnings before it attaches.
    fn note(&mut self, line: &str) -> io::Result<()>;
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
    fn paste(&mut self, on: bool) -> io::Result<()> {
        (**self).paste(on)
    }
    fn show_cursor(&mut self) -> io::Result<()> {
        (**self).show_cursor()
    }
    fn flush(&mut self) -> io::Result<()> {
        (**self).flush()
    }
    fn note(&mut self, line: &str) -> io::Result<()> {
        (**self).note(line)
    }
}

/// The real terminal. Stateless, so the panic hook can build one.
#[derive(Debug)]
struct Stdio;

impl TermIo for Stdio {
    fn raw(&mut self, on: bool) -> io::Result<()> {
        if on {
            // Before crossterm changes it, so a signal handler has the
            // pristine line discipline to put back (bd-8lz.4.7).
            signals::save_termios();
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

    fn paste(&mut self, on: bool) -> io::Result<()> {
        let mut out = io::stdout();
        if on {
            execute!(out, EnableBracketedPaste)
        } else {
            execute!(out, DisableBracketedPaste)
        }
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(io::stdout(), Show)
    }

    fn flush(&mut self) -> io::Result<()> {
        io::stdout().flush()
    }

    fn note(&mut self, line: &str) -> io::Result<()> {
        writeln!(io::stderr(), "{line}")
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
    // Unconditional, unlike the mouse: this one is a safety property, not a
    // preference, and a terminal that does not know the sequence ignores it.
    PASTE_ON.store(true, Ordering::SeqCst);
    io.paste(true)?;
    if mouse {
        MOUSE_ON.store(true, Ordering::SeqCst);
        io.mouse(true)?;
    }
    Ok(())
}

/// A terminal left in raw mode is the worst way for this to fail, so the hook
/// — and the signal handlers that cover the other uncontrolled exit — go in
/// before raw mode does.
fn install_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });
    signals::install();
}

fn restore() {
    restore_with(&mut Stdio);
}

/// Undoes exactly what is on, and nothing when nothing is: safe from the panic
/// hook and the guard, in either order.
///
/// Each flag is cleared *after* its own undo, not all three up front: a signal
/// landing between the two would otherwise find `plan(false, false, false)`,
/// write nothing, and re-raise — leaving the process dead with ANY-MOTION
/// mouse tracking still armed, which `signals`' own module doc calls the worst
/// outcome there is. The cost is that the handler and this may both undo a
/// step; every one of them (`?25h`, the mouse-off run, `?1049l`, `tcsetattr`)
/// is idempotent, and the handler always dies, so no `Guard::drop` can follow
/// it.
fn restore_with<T: TermIo>(io: &mut T) {
    let mouse = MOUSE_ON.load(Ordering::SeqCst);
    let paste = PASTE_ON.load(Ordering::SeqCst);
    let alt = ALT_ON.load(Ordering::SeqCst);
    let raw = RAW_ON.load(Ordering::SeqCst);
    if !(mouse || paste || alt || raw) {
        return;
    }
    // First, and whatever else is on: ratatui hides the cursor on every draw,
    // and `LeaveAlternateScreen` restores the screen buffer, not DECTCEM
    // visibility — that is a global attribute. Without this a panic leaves an
    // invisible cursor behind, which reads as a hung shell.
    let _ = io.show_cursor();
    if mouse {
        let _ = io.mouse(false);
        MOUSE_ON.store(false, Ordering::SeqCst);
    }
    if paste {
        let _ = io.paste(false);
        PASTE_ON.store(false, Ordering::SeqCst);
    }
    if alt {
        let _ = io.alt(false);
        ALT_ON.store(false, Ordering::SeqCst);
    }
    if raw {
        let _ = io.raw(false);
        RAW_ON.store(false, Ordering::SeqCst);
    }
    let _ = io.flush();
}

// --------------------------------------------------------- handing it over

/// Leave TUI mode, let `body` own the terminal, then take it back and force a
/// full redraw.
///
/// The *one* mechanism: attaching to tmux and paging the brief are the same
/// problem — a child that needs an ordinary terminal — and a second copy of
/// this would be a second way to leak one. `body` is infallible from here;
/// what it reports is the caller's to put in the status bar, while an `Err`
/// out of `handoff` itself means the terminal did not come back and the TUI
/// has to end.
fn handoff<B, T, R>(
    io: &mut T,
    terminal: &mut Terminal<B>,
    mouse: bool,
    body: impl FnOnce() -> R,
) -> anyhow::Result<R>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    restore_with(io);
    let out = body();
    // Re-armed whatever `body` did: an attach that failed must not leave the
    // user staring at a shell with `q` still running and eating their keys.
    // `arm_steps` sets each flag before the call that applies it, so a failure
    // here still leaves the guard something to undo.
    arm_steps(io, mouse)?;
    // The alternate screen comes back blank while ratatui still believes it
    // holds the last frame, so without this the next diff would draw nothing.
    terminal.clear()?;
    Ok(out)
}

/// What an attach would land in: the Quest, the window inside it, and what to
/// call the pair in a message.
///
/// `o` on the Quests tab means the Quest's master; `⏎`/`o` on the Sessions tab
/// means *exactly that row's window* (SPEC §17), which is the same thing
/// `q enter --session <label>` means.
struct AttachWant {
    quest: crate::model::Quest,
    label: Option<String>,
    name: String,
    /// The exact `q` session row the attach must land in, when the caller had
    /// one in hand. `enter::resolve` finds a label among the Quest's *live*
    /// sessions, and labels are reused once a worker is gone — so with `a` on,
    /// selecting an ended `alpha/tests` and pressing `⏎` would attach to its
    /// live replacement, under a name identical to the one the row showed.
    /// `None` on the Quests tab, where the master is whichever row holds it.
    session: Option<String>,
    /// The machine the Quest runs on, when that is not this one (SPEC §15).
    /// A remote attach goes over ssh and never consults the local database.
    machine: Option<String>,
}

/// Nothing selected is `None`; a selection whose Quest has since been deleted
/// is an `Err`, which the caller puts in the status bar.
fn attach_want(ctx: &Ctx, app: &App) -> anyhow::Result<Option<AttachWant>> {
    match app.tab {
        Tab::Quests => {
            let machine = quests::selected_remote(app);
            Ok(quests::selected_quest(app).map(|quest| AttachWant {
                name: quest.slug.clone(),
                quest,
                label: None,
                session: None,
                machine,
            }))
        }
        Tab::Sessions => {
            let Some(selection) = sessions::selected(app) else {
                return Ok(None);
            };
            // By id, now: the row is as old as the last tick, and the Quest
            // the window belongs to is what `enter::resolve` is about to be
            // asked about.
            let quest = ctx.db()?.get_quest(&selection.quest)?.ok_or_else(|| {
                crate::error::QError::NotFound(format!("quest of {}", selection.name))
            })?;
            Ok(Some(AttachWant {
                quest,
                label: Some(selection.label),
                name: selection.name,
                session: Some(selection.session),
                // Every row on this tab is a local session: SPEC §15 keeps
                // each machine's sessions in its own database.
                machine: None,
            }))
        }
        _ => Ok(None),
    }
}

/// `o` on the Quests tab and `⏎` on the Sessions tab (SPEC §17): hand the
/// terminal to tmux.
///
/// Resolved through [`enter::resolve`] — the same check `q enter` makes — so a
/// finished Quest, a tmux session that is gone, a master window that ended and
/// a worker label that is not live all say the same thing here as they do on
/// the command line.
fn attach<B, T>(
    ctx: &Ctx,
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    let want = match attach_want(ctx, app) {
        Ok(Some(want)) => want,
        Ok(None) => return Ok(()),
        Err(e) => {
            app.say(format!("cannot enter: {e:#}"));
            return Ok(());
        }
    };
    if let Some(machine) = want.machine.clone() {
        return attach_remote(ctx, io, terminal, app, &want, &machine);
    }
    // The listing is as old as the last tick, and a window that died in the
    // meantime would otherwise be attached to as if it were live — `q enter`
    // sweeps first for the same reason. A sweep that fails is not reported
    // here: `resolve` is about to consult the same tmux and will say so.
    let _ = crate::commands::sweep_quiet(ctx);
    let target = match enter::resolve(ctx, &want.quest, want.label.as_deref()) {
        Ok(target) => target,
        Err(e) => {
            app.say(format!("cannot enter {}: {e:#}", want.name));
            return Ok(());
        }
    };
    // SPEC §17 is "attach to exactly that window". `enter::resolve` answers a
    // narrower question — which LIVE session of this Quest carries that label —
    // and a reused label makes the two different rows.
    if let Some(wanted) = &want.session
        && &target.session.id != wanted
    {
        app.say(format!(
            "{} is not that session any more (the label was reused); x to reload",
            want.name
        ));
        return Ok(());
    }
    if !ctx.config.ui.return_after_detach {
        // Hand the terminal back for good: outside tmux the attach replaces
        // this process and never returns, and inside it the client moves to
        // the Quest, leaving nothing here worth drawing.
        restore_with(io);
        ctx.tmux()
            .attach(&target.tmux_session, Some(&target.pane))?;
        app.should_quit = true;
        return Ok(());
    }
    let attached = handoff(io, terminal, app.mouse, || {
        ctx.tmux()
            .attach_child(&target.tmux_session, Some(&target.pane))
    })?;
    app.say(match attached {
        // Inside tmux the attach is a `switch-client`: the client moves to the
        // Quest and this process never lost anything, so "back from" would be
        // reporting a round trip that did not happen.
        Ok(()) if ctx.tmux().in_tmux() => format!("switched to {}", want.name),
        Ok(()) => format!("back from {}", want.name),
        Err(e) => format!("cannot enter {}: {e:#}", want.name),
    });
    Ok(())
}

/// `o` on a Quest that runs on another machine (SPEC §15): hand the terminal
/// to `ssh -t <alias> tmux attach -t q-<slug>` instead of to a local tmux.
///
/// The same two shapes as the local attach, and for the same reason: with
/// `[ui] return_after_detach` the ssh runs as a child and the TUI comes back
/// when the far end detaches; without it the terminal is given away for good.
/// Nothing here touches the database — a remote Quest's sessions, links and
/// events are on that machine (SPEC §15), and its id means nothing in this one.
fn attach_remote<B, T>(
    ctx: &Ctx,
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
    want: &AttachWant,
    machine: &str,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    // The row says which machine; the config says how to reach it. A remote
    // dropped from the config since the last round leaves rows behind.
    let remote = match remote::find(&ctx.config.remotes, machine) {
        Ok(remote) => remote,
        Err(e) => {
            app.say(format!("cannot enter {}: {e:#}", want.name));
            return Ok(());
        }
    };
    // Refused here as `enter::resolve` refuses it locally: a finished Quest
    // has no tmux session to attach to, over ssh or otherwise.
    if want.quest.state == crate::model::QuestState::Finished {
        app.say(format!(
            "{} is finished on {machine}; resume it there",
            want.name
        ));
        return Ok(());
    }
    // The prefix is the far end's, off the last round it answered; SPEC §15's
    // `q-` stands in until then.
    let target = enter::remote_target(
        ctx,
        remote,
        &want.quest,
        app.remote_tmux.get(machine).map(String::as_str),
        // The Quests tab's `o` is the master (SPEC §17); the Sessions tab is
        // still this machine's fleet only.
        None,
    );
    if !ctx.config.ui.return_after_detach {
        restore_with(io);
        ctx.ssh().attach(&target.alias, &target.argv)?;
        app.should_quit = true;
        return Ok(());
    }
    let attached = handoff(io, terminal, app.mouse, || {
        ctx.ssh().attach_child(&target.alias, &target.argv)
    })?;
    app.say(match attached {
        Ok(()) => format!("back from {machine}:{}", target.tmux_session),
        Err(e) => format!("cannot enter {} on {machine}: {e:#}", want.name),
    });
    Ok(())
}

/// Lines of pane a `p` captures. Far more than `q peek`'s default screenful:
/// the pager can scroll, so the useful bound is the transcript rather than the
/// terminal.
const PEEK_LINES: usize = 200;

/// `p` on the Sessions tab (SPEC §17): the pane, in the pager.
///
/// The same [`handoff`] the attach and the brief use, and the same
/// [`peek::capture`] `q peek` uses — captured *before* the handoff, so a
/// session that cannot be peeked at is a status message rather than a blank
/// screen with a pager on nothing.
fn peek_in_pager<B, T>(
    ctx: &Ctx,
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    let Some(selection) = sessions::selected(app) else {
        return Ok(());
    };
    let _ = crate::commands::sweep_quiet(ctx);
    let text = match crate::commands::target::resolve(ctx, &selection.session).and_then(|found| {
        Ok((
            found.name(),
            found.session.tmux_pane.clone(),
            peek::capture(ctx, &found, PEEK_LINES)?,
        ))
    }) {
        Ok((name, pane, text)) => format!("# {name} · pane {pane}\n\n{text}\n"),
        Err(e) => {
            app.say(format!("cannot peek at {}: {e:#}", selection.name));
            return Ok(());
        }
    };
    let paged = handoff(io, terminal, app.mouse, || pager::show(&text))?;
    if let Err(e) = paged {
        app.say(format!("cannot page the peek: {e:#}"));
    }
    Ok(())
}

/// `b` on the Quests tab (SPEC §17): the brief, in a pager.
fn brief_in_pager<B, T>(
    ctx: &Ctx,
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    let Some(quest) = quests::selected_quest(app) else {
        return Ok(());
    };
    // Rendered before the handoff: a brief that cannot be built is a status
    // message, not a reason to blank the screen and start a pager on nothing.
    //
    // Through the `Ctx`'s own `bd`, not `brief::render`'s discovered one: this
    // runs while the TUI still owns the alternate screen, and the discovered
    // client writes its progress notices to stderr (N-4).
    let markdown = match ctx.db().and_then(|db| {
        brief::render_with(
            db,
            &quest,
            &brief::Opts {
                workflows: ctx.workflows(),
                ..brief::Opts::default()
            },
            &brief::WithBd::new(ctx.bd()),
        )
    }) {
        Ok(markdown) => markdown,
        Err(e) => {
            app.say(format!("cannot brief {}: {e:#}", quest.slug));
            return Ok(());
        }
    };
    let paged = handoff(io, terminal, app.mouse, || pager::show(&markdown))?;
    if let Err(e) = paged {
        app.say(format!("cannot page the brief: {e:#}"));
    }
    Ok(())
}

/// `X` on the Templates tab (SPEC §11): the selected template's TOML, in a
/// pager — the same document `q tpl export <name>` writes, through the same
/// [`handoff`] the brief and the peek use.
fn export_in_pager<B, T>(
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    let Some(template) = templates::selected(app) else {
        return Ok(());
    };
    // Rendered before the handoff, like the brief: a definition that will not
    // serialize is a status message, not a blank screen with a pager on it.
    let toml = match crate::templates::render(std::slice::from_ref(&template)) {
        Ok(toml) => toml,
        Err(e) => {
            app.say(format!("cannot export {}: {e:#}", template.name));
            return Ok(());
        }
    };
    let paged = handoff(io, terminal, app.mouse, || pager::show(&toml))?;
    if let Err(e) = paged {
        app.say(format!("cannot page the export: {e:#}"));
    }
    Ok(())
}

/// This `q`'s own binary, for a proxied Quests key (SPEC §15, bd-8lz.5.8). The
/// current exe so `Q_DB`/`Q_CONFIG` and the version match what is running;
/// a bare `q` on `PATH` only if the exe cannot be found, which never happens in
/// practice but is not worth aborting a keypress over.
fn q_exe() -> std::path::PathBuf {
    std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("q"))
}

/// Run `q <args>` to completion with its output captured — the shape a paged
/// remote read wants. `Q_DB`/`Q_CONFIG` and every other var are inherited, so
/// the child resolves remotes exactly as this process would.
pub(super) fn spawn_q(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new(q_exe()).args(args).output()
}

/// What a captured `q` child had to say when it failed: its stderr as one line,
/// the `error: ` prefix `output::emit_error` writes stripped back off. Falls
/// back to stdout for a command that failed without a word on stderr.
pub(super) fn child_said(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let said = crate::output::first_line(stderr.trim(), 200);
    let said = said.strip_prefix("error: ").unwrap_or(&said).to_string();
    if said.is_empty() {
        crate::output::first_line(String::from_utf8_lossy(&out.stdout).trim(), 200)
    } else {
        said
    }
}

/// Proxy a Quests-tab key against the machine the selection lives on (SPEC §15,
/// bd-8lz.5.8) by running the real `q` binary, which carries the whole proxy
/// stack — target-pinning, the identity check, and a destructive command's
/// confirmation — rather than restating any of it here.
///
/// The selection is read now, not carried on the `Action`: a tick can have
/// reordered the listing between the keypress and here, and the loop reads
/// every hand-over the same way.
fn proxy_remote<B, T>(
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
    key: char,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    let (Some(machine), Some(quest), Some((sub, paged))) = (
        quests::selected_remote(app),
        quests::selected_quest(app),
        quests::proxied(key),
    ) else {
        return Ok(());
    };
    let slug = quest.slug;
    let args = [sub, slug.as_str(), "--machine", machine.as_str()];
    if paged {
        // A read: captured, then paged — the far end's human output scrolls in
        // the same pager the local brief and peek use.
        let out = spawn_q(&args)?;
        if !out.status.success() {
            app.say(format!("q {sub} {slug} on {machine}: {}", child_said(&out)));
            return Ok(());
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.trim().is_empty() {
            app.say(format!("{sub} of {slug} on {machine}: nothing to show"));
            return Ok(());
        }
        let paged = handoff(io, terminal, app.mouse, || pager::show(&text))?;
        if let Err(e) = paged {
            app.say(format!("cannot page {sub}: {e:#}"));
        }
    } else {
        // A write: the terminal is handed to the child so `q close`'s `[y/N]`
        // and `q resume`'s attach run against the real keyboard, exactly as
        // they would from a shell.
        let status = handoff(io, terminal, app.mouse, || {
            std::process::Command::new(q_exe()).args(args).status()
        })?;
        match status {
            Ok(s) if s.success() => app.say(format!(
                "{sub} {slug} on {machine} \u{b7} it updates at the next remote tick"
            )),
            Ok(s) => app.say(match s.code() {
                Some(code) => format!("q {sub} {slug} on {machine} exited {code}"),
                None => format!("q {sub} {slug} on {machine} was killed by a signal"),
            }),
            Err(e) => app.say(format!("cannot run q {sub}: {e}")),
        }
    }
    Ok(())
}

/// Land in the master a template run just made (SPEC §11: a routine runs from
/// the TUI in one keypress).
///
/// A no-op unless a run queued somewhere to go, so both paths that can produce
/// one — the bare `⏎` and the argument form — end the same way without the
/// loop having to know which it was. The two shapes are the attach's, for the
/// attach's reasons: with `[ui] return_after_detach` the tmux client runs as a
/// child and the TUI comes back; without it the terminal is given away for
/// good and this process stops drawing.
///
/// Whatever the run wanted to say rides along on the [`templates::Landing`]
/// and is said from here, on both shapes. It cannot be left on the status bar
/// for the next redraw: this function overwrites the status bar, and on the
/// exec shape there is no next redraw.
fn land<B, T>(
    ctx: &Ctx,
    io: &mut T,
    terminal: &mut Terminal<B>,
    app: &mut App,
    poller: Option<&remote::Poller>,
) -> anyhow::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
    T: TermIo,
{
    let Some(landing) = app.templates.take_landing() else {
        return Ok(());
    };
    // Taken only now: with nothing to land in there is no handoff, and pausing
    // the remote clock for every form submission would cost a fan-out per key.
    let _away = Away::new(poller);
    if !ctx.config.ui.return_after_detach {
        restore_with(io);
        // After the restore and before the attach, exactly where
        // `tpl::instantiate` flushes: the screen is the shell's again, and an
        // attach outside tmux `exec`s this process away.
        for warning in &landing.warnings {
            let _ = io.note(warning);
        }
        let _ = io.flush();
        ctx.tmux()
            .attach(&landing.tmux_session, Some(&landing.pane))?;
        app.should_quit = true;
        return Ok(());
    }
    let attached = handoff(io, terminal, app.mouse, || {
        ctx.tmux()
            .attach_child(&landing.tmux_session, Some(&landing.pane))
    })?;
    let said = match attached {
        Ok(()) if ctx.tmux().in_tmux() => format!("switched to {}", landing.name),
        Ok(()) => format!("back from {}", landing.name),
        Err(e) => format!("{} is running; cannot enter it: {e:#}", landing.name),
    };
    app.say(joined(&said, &landing.warnings));
    Ok(())
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
        // Text, never keys. Bracketed paste is armed precisely so this arm
        // exists: dropping it here would put the bytes back through the key
        // parser, which is what let a pasted `ESC [ C` arm a guarded action
        // row (N-1).
        Event::Paste(text) => (Action::None, app.paste(&text)),
        Event::Resize(w, h) => {
            app.set_size(w, h);
            (Action::None, true)
        }
        _ => (Action::None, false),
    }
}

/// The remote clock, stopped while the UI thread has given the terminal away.
///
/// A handoff blocks this thread for as long as the child runs — four hours in
/// a tmux session is ordinary — and a poller left running would spend that
/// opening an ssh connection per machine every `[ui] tick_remote` for a screen
/// nobody is looking at. Held as a guard so every exit path out of the
/// handoff, `?` included, starts it again.
struct Away<'a>(Option<&'a remote::Poller>);

impl<'a> Away<'a> {
    fn new(poller: Option<&'a remote::Poller>) -> Away<'a> {
        if let Some(poller) = poller {
            poller.pause();
        }
        Away(poller)
    }
}

impl Drop for Away<'_> {
    fn drop(&mut self) {
        if let Some(poller) = self.0 {
            // Resuming asks for a round straight away: whatever the user was
            // doing, what is on screen is older than it looks.
            poller.resume();
        }
    }
}

/// Fold a finished remote round into the state the renderer reads (SPEC §15).
/// Runs on the UI thread because the cache is a database write and the `Ctx`
/// owns the only connection; the ssh it is the answer to ran elsewhere.
fn absorb(ctx: &Ctx, app: &mut App, round: remote::Round) {
    let mut results = remote::resolve_round(ctx, round);
    let notes: Vec<String> = results
        .iter()
        .filter_map(remote::RemoteResult::note)
        .collect();
    app.remote_note = (!notes.is_empty()).then(|| notes.join(" \u{b7} "));
    // Kept rather than replaced: a machine that did not answer this round
    // still has the prefix it reported when it did, which is the same age as
    // the cached rows shown under it.
    for result in &results {
        if let Some(prefix) = &result.tmux_prefix {
            app.remote_tmux.insert(result.name.clone(), prefix.clone());
        }
    }
    app.quests.remote = crate::commands::remote_rows(&mut results);
}

fn event_loop(
    ctx: &Ctx,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    tick: Duration,
    poller: Option<&remote::Poller>,
) -> anyhow::Result<()> {
    refresh_now(ctx, app);
    let mut last_tick = Instant::now();
    let mut dirty = true;
    // Whether the poller's death has already been reported.
    let mut stopped = false;

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
                Action::Refresh => {
                    // `x` is "refresh now" (SPEC §17), the remotes included —
                    // coalesced, so holding the key cannot queue a fan-out per
                    // press. The answer lands in a later iteration; this one
                    // reloads what is already here.
                    if let Some(poller) = poller {
                        poller.nudge();
                    }
                    refresh_due = true;
                }
                // Both take the terminal away and give it back, so both need
                // the screen rebuilt — and after an attach the Quest has very
                // likely moved on, so the listing is reloaded too. The remote
                // clock stops for the duration: nobody is looking, and an
                // attach can last hours (see [`Away`]).
                Action::Attach => {
                    let away = Away::new(poller);
                    let out = attach(ctx, &mut Stdio, terminal, app);
                    drop(away);
                    out?;
                    refresh_due = true;
                }
                Action::Brief => {
                    let away = Away::new(poller);
                    let out = brief_in_pager(ctx, &mut Stdio, terminal, app);
                    drop(away);
                    out?;
                    dirty = true;
                }
                Action::Peek => {
                    let away = Away::new(poller);
                    let out = peek_in_pager(ctx, &mut Stdio, terminal, app);
                    drop(away);
                    out?;
                    dirty = true;
                }
                // Creating, renaming, closing or resuming all change the
                // listing, so the reload is part of the action.
                Action::Submit => {
                    submit(ctx, app);
                    // A template run submitted through its argument form
                    // leaves a master to land in; every other prompt does not,
                    // and `land` is a no-op for them.
                    land(ctx, &mut Stdio, terminal, app, poller)?;
                    refresh_due = true;
                }
                // SPEC §11's one keypress: make the Quest, then hand the
                // terminal to its master. The reload afterwards is what moves
                // `run_count` and the last-run age on the row.
                Action::Run => {
                    templates::run_now(ctx, app);
                    land(ctx, &mut Stdio, terminal, app, poller)?;
                    refresh_due = true;
                }
                Action::Export => {
                    let away = Away::new(poller);
                    let out = export_in_pager(&mut Stdio, terminal, app);
                    drop(away);
                    out?;
                    dirty = true;
                }
                // SPEC §15, bd-8lz.5.8: a Quests key on a remote row runs the
                // real `q` against the owning machine — a read is paged, a
                // write hands over the terminal. Both take the terminal away
                // and (for a write) can change the listing, so the row is
                // reloaded afterwards exactly as an attach is.
                Action::Proxy(key) => {
                    let away = Away::new(poller);
                    let out = proxy_remote(&mut Stdio, terminal, app, key);
                    drop(away);
                    out?;
                    refresh_due = true;
                }
                Action::None => {}
            }
        }

        // A round that finished while this loop was waiting for a key. Never
        // blocks: the ssh ran on the poller's thread, and all that is left is
        // the cache write and the merge.
        if let Some(round) = poller.and_then(remote::Poller::take) {
            absorb(ctx, app, round);
            refresh_due = true;
        }
        // A poller that has stopped would otherwise leave the last chip
        // standing for ever, which reads as a fact about the machines rather
        // than about `q`. Said once.
        if poller.is_some_and(|p| !p.alive()) && !stopped {
            stopped = true;
            app.remote_note = Some("remote polling stopped".to_string());
            dirty = true;
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

/// Run the open form (SPEC §17 `n` / `r` / `c` / `R`).
///
/// The modal is taken first and only put back on failure, so a form that
/// succeeded can never be submitted twice, and one that failed keeps every
/// field the user typed with the reason next to it. A failure is never fatal:
/// a Quest that would not close leaves the TUI exactly where it was.
///
/// Whatever the library wanted to say about `bd` comes back as data on the
/// `Ctx` and is put on screen from here. It cannot say it itself: `q new` and
/// `q close` reach these paths on a terminal they own, the TUI reaches them in
/// raw mode on the alternate screen, and a write there scrolls the pane and
/// leaves ratatui's diff renderer painting over garbage that never clears.
///
/// Dispatch lives here rather than in the tab so bd-8lz.4.5's Sessions prompts
/// have somewhere to hang; today every [`app::Prompt`] is a Quest prompt.
fn submit(ctx: &Ctx, app: &mut App) {
    let Some(mut modal) = app.modal.take() else {
        return;
    };
    let outcome = match &modal.prompt {
        app::Prompt::Send(_) | app::Prompt::Kill(_) | app::Prompt::Reset(_) => {
            sessions::submit(ctx, app, &modal.prompt, &modal.form)
        }
        prompt if prompt.is_template() => templates::submit(ctx, app, &modal.prompt, &modal.form),
        _ => quests::submit(ctx, app, &modal.prompt, &modal.form),
    };
    let warnings = ctx.take_warnings();
    match outcome {
        Ok(()) => {
            if !warnings.is_empty() {
                let said = app.status.clone();
                app.say(joined(&said, &warnings));
            }
        }
        Err(e) => {
            // The error leads: it is why the form is still up. The warnings
            // follow, because on the rollback path one of them names an epic
            // that outlived its Quest — and the id is what has to survive:
            // `form::render` truncates each line to the box budget exactly as
            // the status bar does, so at 120 columns the actionable *tail*
            // ("close it with `bd close bd-e9`") is cut. The id appears early
            // enough that it is not (N-5).
            modal.form.set_error(joined(&format!("{e:#}"), &warnings));
            app.modal = Some(modal);
        }
    }
}

/// `head`, then anything buffered, on one line.
fn joined(head: &str, rest: &[String]) -> String {
    std::iter::once(head)
        .chain(rest.iter().map(String::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

fn refresh_now(ctx: &Ctx, app: &mut App) {
    let result = refresh(ctx, app);
    report_refresh(app, result);
    // Nothing on the reload path warns today. Drained anyway: a warning left
    // in the buffer would surface against a later, unrelated action, and in a
    // process that runs for hours the buffer would only grow.
    let warnings = ctx.take_warnings();
    if !warnings.is_empty() {
        let said = app.status.clone();
        app.say(joined(&said, &warnings));
    }
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

    // Over the whole frame, whatever tab is behind it: this is what makes
    // "the keyboard is captured" and "there is a box on screen" the same
    // condition (`App::capturing`).
    if let Some(modal) = &app.modal {
        form::render(frame, area, &modal.form);
    }
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

/// The right-hand chrome: the keys worth advertising on this tab.
///
/// The two that take over the terminal lead on Quests — nothing else on screen
/// would say which key hands it to tmux or to a pager, and both are one
/// keystroke from a blank screen. `x` gives way to them there and stays in the
/// help overlay, which lists every binding: the segment is capped at two
/// thirds of the row (`layout::right_segment`), and a hint that outgrows that
/// is silently truncated from the right — losing `q quit`, the one key a stuck
/// user needs. `the_status_hint_fits_the_segment_it_is_given` pins that.
fn hint(app: &App) -> &'static str {
    // With a form up, `q` is a letter and `x` is a letter: advertising them
    // would be advertising keys that do not work. `⏎ ok` is not advertised
    // either — on a prompt that destroys or spawns something, Enter alone is
    // deliberately nothing (B2), and the box's own hint, which is the head of
    // this same bar, says what it wants instead.
    if app.modal.is_some() {
        return " Tab field · ←→ choose · Esc cancel ";
    }
    match app.tab {
        Tab::Quests => " ? help · o attach · b brief · q quit ",
        // The two that take over the terminal lead here too: `⏎` lands in the
        // agent's own window, `p` opens its pane in a pager.
        Tab::Sessions => " ? help · ⏎ enter · p peek · q quit ",
        // The tail's own two: what narrows it, and how to get back onto it
        // once the selection has been moved off the newest row.
        Tab::Events => " ? help · / kind · G tail · q quit ",
        // The one that takes over the terminal leads, as it does everywhere:
        // ⏎ runs the routine and lands in its master. `X` is next because
        // this is the tab where `x` and `X` mean different things.
        Tab::Templates => " ? help · ⏎ run · X export · q quit ",
    }
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let hint = hint(app);
    // A reload that failed outranks a keypress's feedback: it is the reason
    // what is on screen may be stale.
    let left = match (app.refresh_error.as_deref(), app.current_status()) {
        // A capture owns the head of the bar: the bar *is* the box, with its
        // cursor and match count, and `filters` deliberately keeps the
        // in-flight query out of the chips. Swapping the box for the error
        // would leave the keyboard armed and the list filtered with nothing on
        // screen saying either, so the error is appended instead.
        (error, Some(status)) if app.capturing() => match error {
            Some(e) => format!(" {status} · {e}"),
            None => format!(" {status}"),
        },
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
    let filters = app.chips();
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

/// The lifecycle flags are process-global, so every test that touches them —
/// here and in [`signals`] — takes this first and always leaves them clear.
/// Whether [`arm_steps`] actually turns bracketed paste on — asked of the
/// code rather than assumed, so a test can model what the terminal will hand
/// the app for a paste (`Event::Paste` with the mode on, raw key events
/// without it) instead of asserting the answer it hopes for.
#[cfg(test)]
pub(super) fn arms_bracketed_paste() -> bool {
    #[derive(Default)]
    struct Probe(bool);
    impl TermIo for Probe {
        fn raw(&mut self, _on: bool) -> io::Result<()> {
            Ok(())
        }
        fn alt(&mut self, _on: bool) -> io::Result<()> {
            Ok(())
        }
        fn mouse(&mut self, _on: bool) -> io::Result<()> {
            Ok(())
        }
        fn paste(&mut self, on: bool) -> io::Result<()> {
            self.0 |= on;
            Ok(())
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn note(&mut self, _line: &str) -> io::Result<()> {
            Ok(())
        }
    }
    // The flags are process-global, and the probe leaves them as it found
    // them: clear on the way in (the lock does that) and clear on the way out.
    let _lock = lifecycle_lock();
    let mut probe = Probe::default();
    let _ = arm_steps(&mut probe, false);
    restore_with(&mut probe);
    probe.0
}

#[cfg(test)]
pub(super) fn lifecycle_lock() -> std::sync::MutexGuard<'static, ()> {
    static LIFECYCLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = LIFECYCLE.lock().unwrap_or_else(|e| e.into_inner());
    RAW_ON.store(false, Ordering::SeqCst);
    ALT_ON.store(false, Ordering::SeqCst);
    MOUSE_ON.store(false, Ordering::SeqCst);
    PASTE_ON.store(false, Ordering::SeqCst);
    guard
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

    fn clear_flags() {
        RAW_ON.store(false, Ordering::SeqCst);
        ALT_ON.store(false, Ordering::SeqCst);
        MOUSE_ON.store(false, Ordering::SeqCst);
        PASTE_ON.store(false, Ordering::SeqCst);
    }

    fn flags() -> Flags {
        (
            RAW_ON.load(Ordering::SeqCst),
            ALT_ON.load(Ordering::SeqCst),
            MOUSE_ON.load(Ordering::SeqCst),
            PASTE_ON.load(Ordering::SeqCst),
        )
    }

    /// `(raw, alt, mouse, paste)`, the four terminal-state flags.
    type Flags = (bool, bool, bool, bool);

    /// Records the escape/termios steps instead of performing them, and can be
    /// told to fail at one of them — or to let a signal land after one.
    #[derive(Debug, Default)]
    struct FakeTerm {
        calls: Vec<&'static str>,
        /// Lines written to the terminal on the way out of TUI mode — the
        /// exec shape of `land` is the only thing that writes any.
        notes: Vec<String>,
        /// `(step, (raw, alt, mouse, paste))` as each step was issued, so a
        /// teardown that drops a flag too early is visible rather than merely
        /// narrow.
        snapshots: Vec<(&'static str, Flags)>,
        fail_on: Option<&'static str>,
        signal_after: Option<&'static str>,
    }

    impl FakeTerm {
        fn failing_at(step: &'static str) -> FakeTerm {
            FakeTerm {
                fail_on: Some(step),
                ..FakeTerm::default()
            }
        }

        fn step(&mut self, name: &'static str) -> io::Result<()> {
            self.calls.push(name);
            self.snapshots.push((name, flags()));
            if self.fail_on == Some(name) {
                return Err(io::Error::other(name));
            }
            if self.signal_after == Some(name) {
                // The real race: a SIGTERM delivered part way through the
                // guard's own teardown.
                signals::restore_from_signal();
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
        fn paste(&mut self, on: bool) -> io::Result<()> {
            self.step(if on { "paste on" } else { "paste off" })
        }
        fn show_cursor(&mut self) -> io::Result<()> {
            self.step("cursor show")
        }
        fn flush(&mut self) -> io::Result<()> {
            self.step("flush")
        }
        fn note(&mut self, line: &str) -> io::Result<()> {
            self.notes.push(line.to_string());
            Ok(())
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
            assert_eq!(flags(), (true, true, true, true));
        }
        assert_eq!(
            term.calls,
            [
                "raw on",
                "alt on",
                "paste on",
                "mouse on",
                "cursor show",
                "mouse off",
                "paste off",
                "alt off",
                "raw off",
                "flush",
            ]
        );
        assert_eq!(flags(), (false, false, false, false));
    }

    #[test]
    fn mouse_capture_is_left_alone_when_the_config_says_so() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        {
            let _guard = arm(&mut term, false).expect("arm");
            assert_eq!(flags(), (true, true, false, true));
        }
        // Nothing is undone that was never switched on.
        assert_eq!(
            term.calls,
            [
                "raw on",
                "alt on",
                "paste on",
                "cursor show",
                "paste off",
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
            // Bracketed paste is armed between the alternate screen and the
            // mouse, and its flag is set before its own call — so a failure
            // here still leaves `?2004l` in the teardown.
            (
                true,
                "paste on",
                vec![
                    "raw on",
                    "alt on",
                    "paste on",
                    "cursor show",
                    "paste off",
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
                    "paste on",
                    "mouse on",
                    "cursor show",
                    "mouse off",
                    "paste off",
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
            assert_eq!(flags(), (false, false, false, false), "failing at {step}");
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

    /// N1 (round 1): the three flags used to be swapped to `false` *before*
    /// any of the undo I/O, so a signal landing in that window found nothing
    /// armed, wrote nothing, and re-raised — killing the process with
    /// ANY-MOTION mouse tracking still on. Each flag now falls only once its
    /// own step has actually run.
    #[test]
    fn every_flag_outlives_the_step_that_undoes_it() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        term.snapshots.clear();

        restore_with(&mut term);
        assert_eq!(
            term.snapshots,
            [
                // (raw, alt, mouse, paste) as each step was issued.
                ("cursor show", (true, true, true, true)),
                ("mouse off", (true, true, true, true)),
                ("paste off", (true, true, false, true)),
                ("alt off", (true, true, false, false)),
                ("raw off", (true, false, false, false)),
                ("flush", (false, false, false, false)),
            ]
        );
        assert_eq!(flags(), (false, false, false, false));
    }

    /// The other half of N1: a signal really landing mid-teardown restores
    /// everything the guard has not reached, and the guard walking its own
    /// remaining steps on top of that is harmless — each one is idempotent.
    #[test]
    fn a_signal_part_way_through_the_teardown_undoes_the_rest() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        term.snapshots.clear();
        // After the cursor came back, before the mouse did.
        term.signal_after = Some("cursor show");

        let ((), escapes) = signals::capturing_output(|| restore_with(&mut term));

        // The handler found the mouse and the alternate screen still armed.
        let mut want = Vec::new();
        want.extend_from_slice(b"\x1b[?25h");
        want.extend_from_slice(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l");
        want.extend_from_slice(b"\x1b[?2004l");
        want.extend_from_slice(b"\x1b[?1049l");
        assert_eq!(escapes, want, "the signal restored nothing");
        // And the guard finished its own sequence regardless.
        assert_eq!(
            term.calls,
            [
                "cursor show",
                "mouse off",
                "paste off",
                "alt off",
                "raw off",
                "flush"
            ]
        );
        assert_eq!(flags(), (false, false, false, false));
    }

    /// The panic hook and the guard both restore, in whichever order they run.
    #[test]
    fn restoring_twice_is_a_no_op_the_second_time() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);

        restore_with(&mut term);
        assert_eq!(flags(), (false, false, false, false));
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

    /// N13: while `/` is open the status bar *is* the box — it is the only
    /// thing on screen saying the keyboard is armed and the list is filtered
    /// (`filters` keeps an in-flight query out of the chips on purpose). A
    /// tick that fails mid-word must report itself without evicting it.
    #[test]
    fn a_failed_refresh_does_not_evict_the_search_box() {
        let mut app = app();
        app.handle(Input::Char('/'));
        for c in "run".chars() {
            app.handle(Input::Char(c));
        }
        assert!(app.capturing());

        report_refresh(&mut app, Err(anyhow::anyhow!("database is locked")));
        let lines = draw(&mut app, 100, 10);
        let bar = lines.last().unwrap();
        assert!(bar.contains("/run"), "the box was evicted: {bar}");
        assert!(
            bar.contains("refresh failed: database is locked"),
            "the failure went unsaid: {bar}"
        );
        assert!(app.capturing(), "the keyboard was handed back");

        // Once the box is closed the failure has the bar to itself again.
        app.handle(Input::Esc);
        assert!(!app.capturing());
        let lines = draw(&mut app, 100, 10);
        assert!(
            lines.last().unwrap().contains("refresh failed"),
            "{lines:?}"
        );
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

        // A tab switch absolutely needs a redraw. It also asks for a reload
        // here, because the Sessions tab has never loaded and drawing its
        // empty listing first would claim the fleet is idle without looking.
        let tab = Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(apply_event(&mut app, tab), (Action::Refresh, true));
        assert_eq!(app.tab, Tab::Sessions);
        // BackTab walks the other way and still costs a redraw. It asks for no
        // reload, but that says nothing about warmth: the Quests tab is the
        // tick's own tab and `needs_reload` answers `false` for it whatever it
        // has loaded. That a tab which HAS loaded is free of I/O is what
        // `events::tests::the_first_visit_to_a_data_tab_loads_before_it_draws`
        // tests, with a real database behind the two tabs that can be cold.
        let back = Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE));
        assert_eq!(apply_event(&mut app, back), (Action::None, true));
        assert_eq!(app.tab, Tab::Quests);
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

    /// N-1. The demonstrated attack was a *paste*: with bracketed paste off
    /// the terminal hands the pasted bytes straight to the key parser, where
    /// `ESC [ C` is `Input::Right` — the key that walks a guarded action row
    /// off `cancel` — and the `CR` behind it is Enter. Reproduced live against
    /// this branch with `tmux send-keys -l 'c'` followed by
    /// `tmux send-keys -H 1b 5b 43 0d`: the Quest went active -> finished and
    /// its tmux session was killed.
    ///
    /// So the mode is armed on the way in, and every way out disables it —
    /// including the signal path, which writes its own bytes.
    #[test]
    fn the_tui_arms_bracketed_paste_and_every_exit_disables_it() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        {
            let _guard = arm(&mut term, true).expect("arm");
            assert!(
                PASTE_ON.load(Ordering::SeqCst),
                "pasted bytes still arrive as keys"
            );
        }
        assert!(term.calls.contains(&"paste on"), "{:?}", term.calls);
        assert!(term.calls.contains(&"paste off"), "{:?}", term.calls);
        assert!(!PASTE_ON.load(Ordering::SeqCst));

        // And a signal, which never reaches the guard, carries the same undo.
        clear_flags();
        PASTE_ON.store(true, Ordering::SeqCst);
        let ((), escapes) = signals::capturing_output(signals::restore_from_signal);
        assert_eq!(
            escapes, b"\x1b[?25h\x1b[?2004l",
            "a killed TUI leaves the shell wrapping every paste in markers"
        );
    }

    /// The other half of N-1: a paste is one `Event::Paste`, and `apply_event`
    /// treats it as text. It can never become an [`Action`], and with nothing
    /// capturing it is dropped rather than replayed through the key parser.
    #[test]
    fn a_paste_is_text_and_never_a_key() {
        let mut app = app();
        let paste = Event::Paste("\u{1b}[C\rq".to_string());
        assert_eq!(apply_event(&mut app, paste), (Action::None, false));
        assert!(!app.should_quit, "a pasted `q` quit the TUI");
        assert!(app.modal.is_none());
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
                    // A modal is drawn over the whole frame, whatever tab and
                    // whatever size — so it belongs in the sweep, not only in
                    // `form`'s own tests.
                    for modal in [false, true] {
                        for tab in Tab::ALL {
                            let mut app = app();
                            app.tab = tab;
                            app.help = help;
                            if modal {
                                let mut form = crate::tui::form::Form::new("close x?")
                                    .action("close")
                                    .note("kills tmux q-x");
                                form.set_error("a rather long reason it did not work");
                                app.open(app::Prompt::NewQuest, form);
                            }
                            // The sweep itself is the assertion: `draw` panics
                            // on a widget written outside its area, and every
                            // band of the chrome is exercised at every
                            // breakpoint.
                            let lines = draw(&mut app, w, h);
                            // The chrome always wins the last line it was
                            // given: a body that overran would have taken it.
                            if h >= 2 && w >= 40 && !modal {
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

        // Same for the Events tab since bd-8lz.4.6.
        app.handle(Input::Char('4'));
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("no events yet"), "{body}");
        assert!(!body.contains("no open quests"), "{body}");

        // And the Templates tab since bd-8lz.6.2. It has never loaded here —
        // the shell asks for its rows on the way in — so it says so rather
        // than claiming the database has no templates.
        app.handle(Input::Char('3'));
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("loading templates"), "{body}");
        assert!(!body.contains("no events yet"), "{body}");

        app.handle(Input::Char('1'));
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("no open quests"), "{body}");
        assert!(!body.contains("loading templates"), "{body}");
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

    /// N6 (round 1): the only status-bar assertion checked `? help`, which
    /// both branches carry — swapping the two arms would have passed.
    #[test]
    fn the_status_hint_names_the_keys_the_tab_actually_has() {
        // Quests' headline actions are the two that take over the terminal.
        let on = |tab: Tab| {
            let mut a = app();
            a.tab = tab;
            hint(&a)
        };
        let quests = on(Tab::Quests);
        assert!(quests.contains("o attach"), "{quests:?}");
        assert!(quests.contains("b brief"), "{quests:?}");
        // Sessions has its own pair that take over the terminal: `⏎` lands in
        // the agent's window, `p` opens its pane in a pager.
        let fleet = on(Tab::Sessions);
        assert!(fleet.contains("\u{23ce} enter"), "{fleet:?}");
        assert!(fleet.contains("p peek"), "{fleet:?}");
        assert!(!fleet.contains("brief"), "{fleet:?}");
        // Events is read-only: its two keys are what narrows the tail and how
        // to get back onto it.
        let events = on(Tab::Events);
        assert!(events.contains("/ kind"), "{events:?}");
        assert!(events.contains("G tail"), "{events:?}");
        // Templates leads with the key that takes over the terminal — `⏎`
        // runs the routine and lands in its master — and then with the one
        // key on this tab whose case matters: `X` exports, `x` still reloads.
        let routines = on(Tab::Templates);
        assert!(routines.contains("\u{23ce} run"), "{routines:?}");
        assert!(routines.contains("X export"), "{routines:?}");
        // Nothing that takes the terminal is advertised on the read-only tab.
        let events = on(Tab::Events);
        assert!(!events.contains("attach"), "{events:?}");
        assert!(!events.contains("brief"), "{events:?}");
        assert!(!events.contains("peek"), "{events:?}");
        // `?` and `q` are on every tab: one opens the list of everything the
        // hint had no room for, the other is the way out.
        for tab in Tab::ALL {
            let h = on(tab);
            assert!(h.contains("? help"), "{tab:?}: {h:?}");
            assert!(h.contains("q quit"), "{tab:?}: {h:?}");
        }
        // With a form up neither is true — `q` is a letter in a field — so the
        // bar advertises the form's own keys instead.
        let mut with_form = app();
        with_form.handle(Input::Char('n'));
        let h = hint(&with_form);
        assert!(!h.contains("q quit"), "{h:?}");
        assert!(h.contains("Esc cancel"), "{h:?}");
        // And `x` is still reachable on Quests, just from the overlay.
        assert!(
            app::help_rows(Tab::Quests)
                .iter()
                .any(|(k, _)| k.contains('x')),
            "x fell out of the help overlay too"
        );
    }

    /// The segment is capped at two thirds of the row, and `truncate` cuts
    /// from the *right* — so a hint that outgrows its budget loses `q quit`
    /// silently. 70 columns is the narrowest width the render sweep asserts
    /// the chrome at.
    #[test]
    fn the_status_hint_fits_the_segment_it_is_given() {
        let mut a = app();
        for tab in Tab::ALL {
            a.tab = tab;
            let h = hint(&a);
            let want = layout::width(h) as u16;
            assert_eq!(
                layout::right_segment(70, want),
                want,
                "{tab:?}: {h:?} is {want} columns and does not fit at 70"
            );
        }
        a.tab = Tab::Quests;
        a.handle(Input::Char('n'));
        let h = hint(&a);
        let want = layout::width(h) as u16;
        assert_eq!(layout::right_segment(70, want), want, "form hint {h:?}");
    }

    /// Rendered, not just returned: the arms have to reach the actual bar.
    #[test]
    fn the_status_bar_draws_the_active_tabs_hint() {
        let mut app = app();
        let quests = draw(&mut app, 100, 20).last().unwrap().clone();
        assert!(quests.contains("o attach"), "{quests:?}");
        assert!(quests.contains("b brief"), "{quests:?}");

        app.handle(Input::Char('4'));
        let events = draw(&mut app, 100, 20).last().unwrap().clone();
        assert!(events.contains("G tail"), "{events:?}");
        assert!(!events.contains("o attach"), "{events:?}");

        app.handle(Input::Char('3'));
        let templates = draw(&mut app, 100, 20).last().unwrap().clone();
        assert!(templates.contains("X export"), "{templates:?}");
        assert!(!templates.contains("o attach"), "{templates:?}");
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
    // -------------------------------------------------- handing the terminal over
    //
    // The suspend/resume half of the TUI is the one a `TestBackend` cannot
    // reach, and bd-8lz.4.1 shipped two terminal-leak blockers precisely
    // because it went untested. `FakeTerm` records the escape/termios steps,
    // so these assert the real ordered sequence.

    /// A Quest with a live master, its tmux session, and a `Ctx` over both.
    fn quest_ctx(
        slug: &str,
        state: crate::model::QuestState,
        live_master: bool,
        panes: &[(&str, &str)],
    ) -> (Ctx, tempfile::TempDir) {
        use crate::model::{Quest, Session, SessionRole, SessionStatus};
        let db = crate::db::Db::open_in_memory().unwrap();
        let mut quest = Quest::new(slug, "/tmp/work", "laptop");
        quest.goal = Some(format!("the goal of {slug}"));
        quest.state = state;
        let quest = db.insert_quest(&quest).unwrap();
        let tmux_session = format!("q-{slug}");
        let mut master = Session::new(
            &quest.id,
            SessionRole::Master,
            "master",
            &tmux_session,
            "%1",
        );
        if !live_master {
            master.status = SessionStatus::Ended;
        }
        db.insert_session(&master).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux.json");
        let state = crate::tmux::FixtureState {
            next_pane: 9,
            panes: panes
                .iter()
                .map(|(session, id)| crate::tmux::FixturePane {
                    pane_id: (*id).to_string(),
                    pane_pid: 1234,
                    session_name: (*session).to_string(),
                    window_name: "master".to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
        let tmux = Box::new(crate::tmux::FixtureTmux::new(path));
        (Ctx::for_tests(Config::default(), db, tmux), dir)
    }

    /// The `Ctx` owns its `Box<dyn Tmux>` and will not hand it back, so what
    /// an attach did is read off the fixture file itself.
    fn fixture_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("tmux.json")
    }

    fn fixture(dir: &tempfile::TempDir) -> crate::tmux::FixtureState {
        crate::tmux::FixtureTmux::new(fixture_path(dir))
            .load()
            .unwrap()
    }

    fn loaded(ctx: &Ctx) -> App {
        let mut app = App::new(&ctx.config, "laptop");
        app.set_size(120, 30);
        refresh_now(ctx, &mut app);
        app
    }

    fn test_terminal() -> Terminal<TestBackend> {
        Terminal::new(TestBackend::new(120, 30)).unwrap()
    }

    /// The whole point of `handoff`: the body runs on a terminal that is back
    /// to normal, and TUI mode is rebuilt afterwards.
    #[test]
    fn a_handoff_leaves_tui_mode_and_comes_back_into_it() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();

        let mut terminal = test_terminal();
        let seen = handoff(&mut term, &mut terminal, true, || {
            // tmux and the pager both need an ordinary terminal, and this is
            // the only assertion that can prove they get one.
            flags()
        })
        .expect("handoff");
        assert_eq!(
            seen,
            (false, false, false, false),
            "the body ran in TUI mode"
        );
        assert_eq!(
            flags(),
            (true, true, true, true),
            "TUI mode never came back"
        );
        assert_eq!(
            term.calls,
            [
                "cursor show",
                "mouse off",
                "paste off",
                "alt off",
                "raw off",
                "flush",
                "raw on",
                "alt on",
                "paste on",
                "mouse on",
            ]
        );
        restore_with(&mut term);
    }

    /// `[ui] mouse = false`: nothing is armed on the way back that was not
    /// armed on the way in.
    #[test]
    fn a_handoff_does_not_arm_mouse_capture_the_config_turned_off() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, false).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();

        let mut terminal = test_terminal();
        handoff(&mut term, &mut terminal, false, || ()).expect("handoff");
        assert_eq!(flags(), (true, true, false, true));
        assert_eq!(
            term.calls,
            [
                "cursor show",
                "paste off",
                "alt off",
                "raw off",
                "flush",
                "raw on",
                "alt on",
                "paste on"
            ]
        );
        restore_with(&mut term);
    }

    /// bd-8lz.4.1 left this as the one way its `restore_with` early-return
    /// could become reachable: a resume that fails half way has to leave the
    /// guard something to undo, or the cursor stays hidden for good.
    #[test]
    fn a_resume_that_fails_still_leaves_the_guard_something_to_undo() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::failing_at("alt on");
        RAW_ON.store(true, Ordering::SeqCst);
        ALT_ON.store(true, Ordering::SeqCst);

        let mut terminal = test_terminal();
        let err = handoff(&mut term, &mut terminal, false, || ()).unwrap_err();
        assert!(err.to_string().contains("alt on"), "{err}");
        // Half armed — and armed is what makes the undo run at all. Bracketed
        // paste comes after the alternate screen, so `alt on` failing means it
        // was never reached.
        assert_eq!(flags(), (true, true, false, false));
        term.calls.clear();
        restore_with(&mut term);
        assert_eq!(term.calls[0], "cursor show");
        assert_eq!(flags(), (false, false, false, false));
    }

    // ------------------------------------------------------------- attaching

    #[test]
    fn o_hands_the_terminal_to_tmux_and_takes_it_back() {
        let (ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        let mut app = loaded(&ctx);
        assert_eq!(app.handle(Input::Char('o')), Action::Attach);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        let state = fixture(&_dir);
        assert_eq!(
            state.attached,
            Some(("q-needs-me".to_string(), Some("%1".to_string())))
        );
        // Not `exec`: the TUI has to get its process back to redraw.
        assert_eq!(state.attach_mode.as_deref(), Some("child"));
        assert!(app.status.contains("back from needs-me"), "{}", app.status);
        assert!(!app.should_quit);
        assert_eq!(
            flags(),
            (true, true, true, true),
            "the TUI did not come back"
        );
        assert!(term.calls.contains(&"raw off"), "{:?}", term.calls);
        assert!(term.calls.ends_with(&["mouse on"]), "{:?}", term.calls);
        restore_with(&mut term);
    }

    // ------------------------------------------------- attaching over ssh

    /// A `Ctx` with one remote and a scriptable ssh, plus an `App` whose
    /// listing holds exactly one row — a Quest on that remote.
    fn remote_rig(
        state: crate::model::QuestState,
    ) -> (Ctx, std::sync::Arc<crate::remote::stub::StubSsh>, App) {
        let mut config = Config::default();
        config.machine.name = "laptop".to_string();
        config.remotes = vec![crate::config::Remote {
            name: "ws".to_string(),
            ssh: "ws-host".to_string(),
        }];
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = Box::new(crate::tmux::FixtureTmux::new(std::path::PathBuf::from(
            "/nonexistent/tmux.json",
        )));
        let ssh = std::sync::Arc::new(crate::remote::stub::StubSsh::new(&[]));
        let ctx = Ctx::for_tests(config, db, tmux)
            .with_ssh(ssh.clone() as std::sync::Arc<dyn crate::remote::Ssh>);

        let mut quest = crate::model::Quest::new("over-there", "/tmp/work", "ws");
        quest.state = state;
        let view = crate::commands::QuestView::new(quest, &[]);
        let raw = serde_json::to_value(&view).unwrap();
        let row =
            crate::commands::QuestRow::remote(crate::remote::RemoteQuest { view, raw }, false);

        let mut app = App::new(&ctx.config, "laptop");
        app.set_size(120, 30);
        quests::seed(&mut app, vec![row]);
        (ctx, ssh, app)
    }

    /// SPEC §15 from the TUI: `o` on a Quest that runs elsewhere hands the
    /// terminal to `ssh -t <alias> tmux attach`, and `[ui] return_after_detach`
    /// brings it back — the same handoff a local attach uses.
    #[test]
    fn o_on_a_remote_quest_hands_the_terminal_to_ssh() {
        let (ctx, ssh, mut app) = remote_rig(crate::model::QuestState::Active);
        assert_eq!(app.handle(Input::Char('o')), Action::Attach);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        assert_eq!(
            ssh.attaches(),
            [(
                // A child, not an exec: the TUI has to get its process back.
                "child".to_string(),
                "ws-host".to_string(),
                vec![
                    "tmux".to_string(),
                    "attach".to_string(),
                    "-t".to_string(),
                    "=q-over-there".to_string()
                ],
            )]
        );
        assert!(
            app.status.contains("back from ws:q-over-there"),
            "{}",
            app.status
        );
        assert!(!app.should_quit);
        assert_eq!(
            flags(),
            (true, true, true, true),
            "the TUI did not come back"
        );
        restore_with(&mut term);
    }

    /// `[ui] return_after_detach = false`: the terminal is given away for
    /// good and the TUI ends, exactly as it does for a local attach.
    #[test]
    fn a_remote_attach_can_give_the_terminal_away_for_good() {
        let (mut ctx, ssh, mut app) = remote_rig(crate::model::QuestState::Active);
        ctx.config.ui.return_after_detach = false;

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        assert_eq!(ssh.attaches()[0].0, "exec");
        assert!(app.should_quit);
        assert_eq!(
            flags(),
            (false, false, false, false),
            "the terminal was kept"
        );
    }

    /// A finished Quest has no tmux session to attach to, wherever it runs —
    /// the same refusal `enter::resolve` makes locally, made before any ssh.
    #[test]
    fn a_finished_remote_quest_is_not_attachable_either() {
        let (ctx, ssh, mut app) = remote_rig(crate::model::QuestState::Finished);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        assert!(ssh.attaches().is_empty(), "{:?}", ssh.attaches());
        assert!(app.status.contains("finished on ws"), "{}", app.status);
        assert_eq!(
            flags(),
            (true, true, true, true),
            "the screen was given away"
        );
        restore_with(&mut term);
    }

    /// A remote dropped from the config leaves its rows behind until the next
    /// round; entering one has to say so rather than panic or dial nothing.
    #[test]
    fn a_row_whose_remote_is_no_longer_configured_says_so() {
        let (mut ctx, ssh, mut app) = remote_rig(crate::model::QuestState::Active);
        ctx.config.remotes.clear();

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        assert!(ssh.attaches().is_empty());
        assert!(
            app.status.contains("cannot enter over-there"),
            "{}",
            app.status
        );
        restore_with(&mut term);
    }

    /// A round that lands turns into rows and, when a machine did not answer,
    /// into the standing chip that says the listing is partly the cache.
    ///
    /// Through `absorb` itself — it is the whole of the TUI's remote data
    /// path, cache write included, and a test that re-implemented its body
    /// would prove nothing about it.
    #[test]
    fn a_finished_round_becomes_rows_and_a_chip() {
        let (ctx, _ssh, mut app) = remote_rig(crate::model::QuestState::Active);
        app.quests.remote.clear();

        let view = crate::commands::QuestView::new(
            crate::model::Quest::new("over-there", "/tmp/work", "workstation"),
            &[],
        );
        // The envelope a real `q list --json --no-remote` sends, prefix and all.
        let payload = serde_json::json!({
            "quests": [view],
            "machines": [{
                "name": "workstation", "kind": "local", "status": "ok",
                "quests": 1, "tmux_prefix": "work_"
            }],
        })
        .to_string();
        let round = crate::remote::Round::for_tests(vec![
            (
                crate::config::Remote {
                    name: "ws".to_string(),
                    ssh: "ws-host".to_string(),
                },
                crate::remote::SshOutcome::Done {
                    code: Some(0),
                    stdout: payload.clone(),
                    stderr: String::new(),
                },
            ),
            (
                crate::config::Remote {
                    name: "box".to_string(),
                    ssh: "box-host".to_string(),
                },
                crate::remote::SshOutcome::Failed("host is down".to_string()),
            ),
        ]);

        absorb(&ctx, &mut app, round);

        // The rows the round brought back, stamped with the *config* name.
        assert_eq!(app.quests.remote.len(), 1);
        let row = &app.quests.remote[0];
        assert_eq!(row.view.quest.slug, "over-there");
        assert_eq!(row.view.quest.machine, "ws");
        assert!(row.origin.is_remote() && !row.origin.is_stale());

        let note = app.remote_note.clone().expect("a machine is down");
        assert!(note.contains("box \u{26a0} unreachable"), "{note}");
        assert!(note.contains("host is down"), "{note}");
        // A chip, not a message: it leads the bar and outlives a keypress.
        assert!(app.chips().contains("box"), "{}", app.chips());
        app.say("something else");
        assert!(app.chips().contains("box"));

        // The far end's tmux prefix, which is what `o` on that row attaches to.
        assert_eq!(app.remote_tmux.get("ws").map(String::as_str), Some("work_"));

        // And the cache write really happened, on this thread.
        let cached = ctx.db().unwrap().get_remote_cache("ws").unwrap();
        assert_eq!(cached.map(|c| c.payload), Some(payload));

        // A local reload folds the round's rows back into the listing.
        refresh_now(&ctx, &mut app);
        assert!(
            app.quests
                .loaded()
                .iter()
                .any(|r| r.view.quest.slug == "over-there"),
            "the remote row did not survive a reload: {:?}",
            app.refresh_error
        );
    }

    /// Inside tmux there is no process to replace and nothing to wait for:
    /// the client switches and the TUI keeps running in its own pane.
    #[test]
    fn attaching_from_inside_tmux_switches_the_client_instead() {
        let (ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        let tmux = crate::tmux::FixtureTmux::new(fixture_path(&_dir));
        let mut state = tmux.load().unwrap();
        state.in_tmux = Some(true);
        tmux.save(&state).unwrap();

        let mut app = loaded(&ctx);
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        assert_eq!(fixture(&_dir).attach_mode.as_deref(), Some("switch"));
        // N9a: the client moved, this process never left — "back from" would
        // be reporting a round trip that did not happen.
        assert!(
            app.status.contains("switched to needs-me"),
            "{}",
            app.status
        );
        assert!(!app.status.contains("back from"), "{}", app.status);
        assert_eq!(flags(), (true, true, true, true));
        restore_with(&mut term);
    }

    /// `[ui] return_after_detach = false`: the terminal is handed over for
    /// good, so it must be restored *before* the attach and never taken back.
    #[test]
    fn return_after_detach_off_gives_the_terminal_away_and_quits() {
        let (mut ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        ctx.config.ui.return_after_detach = false;
        let mut app = loaded(&ctx);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        assert_eq!(fixture(&_dir).attach_mode.as_deref(), Some("exec"));
        assert!(app.should_quit, "the TUI kept running with no terminal");
        assert_eq!(flags(), (false, false, false, false));
        assert_eq!(
            term.calls,
            [
                "cursor show",
                "mouse off",
                "paste off",
                "alt off",
                "raw off",
                "flush"
            ],
            "the terminal was not handed back before the exec"
        );
    }

    // ------------------------------------------------- templates (SPEC §11)

    /// A `Ctx` with one template stored, and an App sitting on the Templates
    /// tab with it selected. The tmux is a fixture and `bd` refuses every
    /// call, so a run reaches neither.
    fn templates_ctx(cwd: &std::path::Path) -> (Ctx, tempfile::TempDir, App) {
        let db = crate::db::Db::open_in_memory().unwrap();
        let mut template = crate::model::Template::new("weekly-hygiene");
        template.description = Some("refresh the work repo".to_string());
        template.goal = Some("tidy up".to_string());
        template.cwd = Some(cwd.to_string_lossy().to_string());
        db.insert_template(&template).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux.json");
        std::fs::write(&path, "{}").unwrap();
        let ctx = Ctx::for_tests(
            Config::default(),
            db,
            Box::new(crate::tmux::FixtureTmux::new(path)),
        )
        .with_bd(Box::new(crate::beads::stub::NoBd));

        let mut app = App::new(&ctx.config, "laptop");
        app.tab = Tab::Templates;
        app.set_size(120, 30);
        refresh_now(&ctx, &mut app);
        (ctx, dir, app)
    }

    /// SPEC §11's one keypress, end to end through the loop's own machinery:
    /// `⏎` asks for the run, the run makes the Quest, and `land` hands the
    /// terminal to the master it started — leaving TUI mode to do it and
    /// taking it back afterwards.
    #[test]
    fn a_template_runs_on_one_keypress_and_lands_in_the_master_it_makes() {
        let cwd = tempfile::tempdir().unwrap();
        let (ctx, _dir, mut app) = templates_ctx(cwd.path());
        assert_eq!(app.handle(Input::Enter), Action::Run);
        templates::run_now(&ctx, &mut app);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        land(&ctx, &mut term, &mut terminal, &mut app, None).expect("land");

        // A child attach, so the TUI comes back — `[ui] return_after_detach`
        // is on by default.
        assert_eq!(fixture(&_dir).attach_mode.as_deref(), Some("child"));
        assert!(app.status.contains("back from"), "{}", app.status);
        assert!(!app.should_quit, "the TUI quit on a returning attach");
        assert_eq!(
            flags(),
            (true, true, true, true),
            "TUI mode never came back"
        );
        // Landed in exactly once: a second call has nothing to do.
        assert!(app.templates.take_landing().is_none());
        restore_with(&mut term);
    }

    /// `[ui] return_after_detach = false`: the same rule the attach follows —
    /// the terminal is given away before the exec and never taken back.
    #[test]
    fn a_template_run_with_return_after_detach_off_gives_the_terminal_away() {
        let cwd = tempfile::tempdir().unwrap();
        let (mut ctx, _dir, mut app) = templates_ctx(cwd.path());
        ctx.config.ui.return_after_detach = false;
        app.handle(Input::Enter);
        templates::run_now(&ctx, &mut app);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        let mut terminal = test_terminal();
        land(&ctx, &mut term, &mut terminal, &mut app, None).expect("land");

        assert_eq!(fixture(&_dir).attach_mode.as_deref(), Some("exec"));
        assert!(app.should_quit, "the TUI kept running with no terminal");
        assert_eq!(flags(), (false, false, false, false));
        // Nothing will be drawn again, so what the run had to say went to the
        // terminal on the way out — where `q tpl run` flushes the same line.
        assert!(
            term.notes.iter().any(|n| n.contains("no beads epic")),
            "the warning went nowhere: {:?}",
            term.notes
        );
    }

    /// A warning the run raised reaches the user on every shape the landing
    /// takes. `land` overwrites the status line, so the warning rides on the
    /// landing and is said from there rather than left on the `Ctx` for a
    /// redraw that may never come (bd-8lz.6.2).
    #[test]
    fn a_run_says_its_warnings_however_it_lands() {
        let _lock = lifecycle_lock();

        // The bare `⏎`: no form between the keypress and the landing.
        let cwd = tempfile::tempdir().unwrap();
        let (ctx, _dir, mut app) = templates_ctx(cwd.path());
        assert_eq!(app.handle(Input::Enter), Action::Run);
        templates::run_now(&ctx, &mut app);
        assert!(
            ctx.take_warnings().is_empty(),
            "left buffered, where a later unrelated action would wear it"
        );
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        land(&ctx, &mut term, &mut terminal, &mut app, None).expect("land");
        restore_with(&mut term);
        assert!(app.status.contains("back from"), "{}", app.status);
        assert!(app.status.contains("no beads epic"), "{}", app.status);
        // And the loop's reload afterwards has nothing left to add.
        let was = app.status.clone();
        refresh_now(&ctx, &mut app);
        assert_eq!(app.status, was);

        // The argument form: `submit` drains the warning onto the status line
        // and `land` used to overwrite it before a single frame was drawn.
        let cwd = tempfile::tempdir().unwrap();
        let (ctx, _dir, mut app) = templates_ctx(cwd.path());
        let mut with_arg = crate::model::Template::new("pr-review");
        with_arg.goal = Some("review PR {{arg.pr}}".to_string());
        with_arg.cwd = Some(cwd.path().to_string_lossy().to_string());
        ctx.db().unwrap().insert_template(&with_arg).unwrap();
        refresh_now(&ctx, &mut app);
        app.handle(Input::Char('g'));
        assert_eq!(app.handle(Input::Enter), Action::None, "no form went up");
        focus_field(&mut app, "arg pr");
        for c in "4821".chars() {
            app.handle(Input::Char(c));
        }
        arm_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);

        submit(&ctx, &mut app);
        assert!(app.modal.is_none(), "{}", app.status);
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        land(&ctx, &mut term, &mut terminal, &mut app, None).expect("land");
        restore_with(&mut term);
        assert!(app.status.contains("back from"), "{}", app.status);
        assert!(app.status.contains("no beads epic"), "{}", app.status);
    }

    /// The `Action::Submit` wiring itself: the loop runs the prompt and lands
    /// in the master it made, in that order and without a frame in between.
    #[test]
    fn the_loops_dispatcher_runs_a_template_prompt_and_lands_in_it() {
        let cwd = tempfile::tempdir().unwrap();
        let (ctx, _dir, mut app) = templates_ctx(cwd.path());
        let mut with_arg = crate::model::Template::new("pr-review");
        with_arg.goal = Some("review PR {{arg.pr}}".to_string());
        with_arg.cwd = Some(cwd.path().to_string_lossy().to_string());
        ctx.db().unwrap().insert_template(&with_arg).unwrap();
        refresh_now(&ctx, &mut app);
        app.handle(Input::Char('g'));
        app.handle(Input::Enter);
        focus_field(&mut app, "arg pr");
        for c in "4821".chars() {
            app.handle(Input::Char(c));
        }
        arm_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        // Exactly the loop's `Action::Submit` arm.
        submit(&ctx, &mut app);
        land(&ctx, &mut term, &mut terminal, &mut app, None).expect("land");
        restore_with(&mut term);

        let quests = ctx.db().unwrap().list_quests(true).unwrap();
        assert_eq!(quests.len(), 1);
        assert_eq!(quests[0].goal.as_deref(), Some("review PR 4821"));
        assert_eq!(fixture(&_dir).attach_mode.as_deref(), Some("child"));
        assert!(app.status.contains("back from"), "{}", app.status);
        assert!(app.templates.take_landing().is_none(), "landed twice");
    }

    /// Nothing to land in is the ordinary case — every prompt that is not a
    /// template run goes through the same call.
    #[test]
    fn landing_with_nothing_queued_never_touches_the_terminal() {
        let cwd = tempfile::tempdir().unwrap();
        let (ctx, _dir, mut app) = templates_ctx(cwd.path());
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        land(&ctx, &mut term, &mut terminal, &mut app, None).expect("land");
        assert!(term.calls.is_empty(), "{:?}", term.calls);
        assert!(fixture(&_dir).attach_mode.is_none());
    }

    /// `X` pages the definition, the same document `q tpl export` writes, and
    /// through the same handoff the brief uses.
    #[test]
    fn the_export_reaches_the_pager_as_the_toml_q_tpl_export_writes() {
        let cwd = tempfile::tempdir().unwrap();
        let (_ctx, _dir, mut app) = templates_ctx(cwd.path());
        assert_eq!(app.handle(Input::Char('X')), Action::Export);

        let out = _dir.path().join("paged.toml");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        pager::with_pager(Some(&format!("tee {}", out.display())), || {
            export_in_pager(&mut term, &mut terminal, &mut app).expect("export");
        });

        let seen = std::fs::read_to_string(&out).unwrap();
        assert!(seen.contains("[[template]]"), "{seen}");
        assert!(seen.contains("weekly-hygiene"), "{seen}");
        assert!(seen.contains("tidy up"), "{seen}");
        // Run stats are history and never travel (SPEC §11).
        assert!(!seen.contains("run_count"), "{seen}");
        assert_eq!(
            flags(),
            (true, true, true, true),
            "TUI mode never came back"
        );
        restore_with(&mut term);
    }

    /// Every reason not to attach is the same reason `q enter` gives, and none
    /// of them may cost the user their screen.
    #[test]
    fn a_quest_that_cannot_be_entered_says_so_and_keeps_the_screen() {
        for (what, live_master, panes, want) in [
            ("no tmux session", true, &[][..], "no tmux session"),
            (
                "master ended",
                false,
                &[("q-needs-me", "%1")][..],
                "master session of needs-me ended",
            ),
        ] {
            let (ctx, _dir) = quest_ctx(
                "needs-me",
                crate::model::QuestState::Active,
                live_master,
                panes,
            );
            let mut app = loaded(&ctx);
            let _lock = lifecycle_lock();
            let mut term = FakeTerm::default();
            let guard = arm(&mut term, true).expect("arm");
            std::mem::forget(guard);
            term.calls.clear();
            let mut terminal = test_terminal();
            attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

            assert!(app.status.contains(want), "{what}: {}", app.status);
            assert!(fixture(&_dir).attached.is_none(), "{what}: it attached");
            // The screen was never given away, so there is nothing to rebuild.
            assert!(term.calls.is_empty(), "{what}: {:?}", term.calls);
            assert_eq!(flags(), (true, true, true, true), "{what}");
            assert!(!app.should_quit, "{what}");
            restore_with(&mut term);
        }
    }

    /// A finished Quest is `q resume`'s business, from here as from the CLI.
    #[test]
    fn a_finished_quest_is_not_attachable_from_the_tui_either() {
        let (ctx, _dir) = quest_ctx(
            "shipped",
            crate::model::QuestState::Finished,
            true,
            &[("q-shipped", "%1")],
        );
        let mut app = loaded(&ctx);
        // Finished Quests are hidden until `f`, which needs the reload it asks
        // for before there is anything to select.
        assert_eq!(app.handle(Input::Char('f')), Action::Refresh);
        refresh_now(&ctx, &mut app);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");
        assert!(app.status.contains("is finished"), "{}", app.status);
        assert!(fixture(&_dir).attached.is_none());
    }

    // ------------------------------------------ the Sessions tab's two keys

    /// A Quest with a master and one worker, each in its own pane, plus the
    /// `Ctx` over both. The Sessions tab is what these exercise, so both
    /// windows have to exist and be distinguishable.
    fn fleet_ctx() -> (Ctx, tempfile::TempDir) {
        use crate::model::{Quest, Session, SessionRole, SessionStatus};
        let db = crate::db::Db::open_in_memory().unwrap();
        let quest = db
            .insert_quest(&Quest::new("alpha", "/tmp/work", "laptop"))
            .unwrap();
        for (label, role, pane) in [
            ("master", SessionRole::Master, "%1"),
            ("tests", SessionRole::Worker, "%2"),
        ] {
            let mut row = Session::new(&quest.id, role, label, "q-alpha", pane);
            row.status = SessionStatus::Idle;
            db.insert_session(&row).unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux.json");
        let state = crate::tmux::FixtureState {
            next_pane: 9,
            panes: [("master", "%1"), ("tests", "%2")]
                .iter()
                .map(|(window, pane)| crate::tmux::FixturePane {
                    pane_id: (*pane).to_string(),
                    pane_pid: 1234,
                    session_name: "q-alpha".to_string(),
                    window_name: (*window).to_string(),
                    buffer: format!("last line of {window}\n"),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
        (
            Ctx::for_tests(
                Config::default(),
                db,
                Box::new(crate::tmux::FixtureTmux::new(path)),
            ),
            dir,
        )
    }

    /// Puts the Sessions tab up, loaded, with the selection on `label`.
    fn on_session(ctx: &Ctx, label: &str) -> App {
        let mut app = loaded(ctx);
        // Through the shell's own key, so the tab switch goes through
        // `App::select` exactly as it does for a user. The tab has never
        // loaded, so the switch asks for its rows on the way in.
        assert_eq!(app.handle(Input::Char('2')), Action::Refresh);
        assert_eq!(app.tab, Tab::Sessions);
        refresh_now(ctx, &mut app);
        for _ in 0..6 {
            if sessions::selected(&app).map(|s| s.label) == Some(label.to_string()) {
                return app;
            }
            app.handle(Input::Down);
        }
        panic!("no session labelled {label} in the fleet");
    }

    /// SPEC §17: "`⏎` attach na točno taj window" — the worker's pane, not the
    /// Quest's master. This is the half `q enter --session <label>` already
    /// does, reached through the same `enter::resolve`.
    #[test]
    fn enter_on_the_sessions_tab_attaches_to_exactly_that_window() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        assert_eq!(app.handle(Input::Enter), Action::Attach);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");

        let state = fixture(&_dir);
        assert_eq!(
            state.attached,
            Some(("q-alpha".to_string(), Some("%2".to_string()))),
            "it attached to the master instead of the worker"
        );
        assert!(
            app.status.contains("back from alpha/tests"),
            "{}",
            app.status
        );
        restore_with(&mut term);
    }

    /// And the master row still lands in window 0, so the two rows are not the
    /// same attach with a different label on it.
    #[test]
    fn the_master_row_attaches_to_window_zero() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "master");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");
        assert_eq!(
            fixture(&_dir).attached,
            Some(("q-alpha".to_string(), Some("%1".to_string())))
        );
        restore_with(&mut term);
    }

    /// A worker whose window ended says what `q enter --session` says, and
    /// nothing is handed over.
    #[test]
    fn attaching_to_a_session_whose_window_is_gone_is_refused() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        // The worker's pane disappears; the sweep inside `attach` notices.
        let path = fixture_path(&_dir);
        let tmux = crate::tmux::FixtureTmux::new(&path);
        let mut state = tmux.load().unwrap();
        state.panes.retain(|p| p.pane_id != "%2");
        tmux.save(&state).unwrap();

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");
        assert!(
            app.status.contains("cannot enter alpha/tests"),
            "{}",
            app.status
        );
        assert!(fixture(&_dir).attached.is_none());
    }

    /// SPEC §17 `p`: the pane, through the same `handoff` the attach and the
    /// brief use, and the same `peek::capture` `q peek` uses.
    #[test]
    fn p_pages_the_selected_pane_through_the_one_handoff() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        assert_eq!(app.handle(Input::Char('p')), Action::Peek);

        let out = _dir.path().join("paged");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        let mut terminal = test_terminal();
        pager::with_pager(Some(&format!("tee {}", out.display())), || {
            peek_in_pager(&ctx, &mut term, &mut terminal, &mut app).expect("peek");
        });

        let paged = std::fs::read_to_string(&out).unwrap();
        assert!(paged.contains("last line of tests"), "{paged}");
        assert!(!paged.contains("last line of master"), "{paged}");
        assert!(paged.contains("alpha/tests"), "{paged}");
        assert!(paged.contains("%2"), "{paged}");
        // Same one mechanism as the attach and the brief, in the same order.
        assert_eq!(
            term.calls,
            [
                "cursor show",
                "mouse off",
                "paste off",
                "alt off",
                "raw off",
                "flush",
                "raw on",
                "alt on",
                "paste on",
                "mouse on",
            ]
        );
        restore_with(&mut term);
    }

    /// A pane that is gone is a status message, not a blank screen with a
    /// pager sitting on nothing.
    #[test]
    fn peeking_at_a_dead_pane_never_reaches_the_pager() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        let path = fixture_path(&_dir);
        let tmux = crate::tmux::FixtureTmux::new(&path);
        let mut state = tmux.load().unwrap();
        state.panes.retain(|p| p.pane_id != "%2");
        tmux.save(&state).unwrap();

        let out = _dir.path().join("never");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        pager::with_pager(Some(&format!("tee {}", out.display())), || {
            peek_in_pager(&ctx, &mut term, &mut terminal, &mut app).expect("peek");
        });
        assert!(!out.exists(), "the pager ran on a dead pane");
        assert!(
            app.status.contains("cannot peek at alpha/tests"),
            "{}",
            app.status
        );
        assert_eq!(
            term.calls,
            Vec::<&str>::new(),
            "the terminal was handed over"
        );
    }

    /// B1. A row a `q spawn` inserted before its window opened has no pane,
    /// and `capture-pane -t ''` reads whatever pane is current — q's own.
    #[test]
    fn peeking_at_a_row_whose_window_never_opened_never_reaches_the_pager() {
        let (ctx, _dir) = fleet_ctx();
        // The worker loses its pane the way a half-finished spawn leaves it:
        // the row is live, the pane column is empty.
        ctx.db()
            .unwrap()
            .update_session_pane(&session_id(&ctx, "tests"), "")
            .unwrap();
        let mut app = on_session(&ctx, "tests");

        let out = _dir.path().join("never");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        pager::with_pager(Some(&format!("tee {}", out.display())), || {
            peek_in_pager(&ctx, &mut term, &mut terminal, &mut app).expect("peek");
        });
        assert!(!out.exists(), "the pager ran on q's own pane");
        assert!(app.status.contains("has no pane"), "{}", app.status);
        assert_eq!(term.calls, Vec::<&str>::new());
        // Both windows are still there.
        assert_eq!(fixture(&_dir).panes.len(), 2);
    }

    // ------------------------------- the event loop's own prompt dispatcher

    /// N-3. Every earlier test of these three prompts called `sessions::submit`
    /// directly. `tui::submit` is what the event loop actually calls, and if
    /// its match arm sent session prompts to `quests::submit` they would land
    /// on that function's `Prompt::Send(_) | Kill(_) | Reset(_) => Ok(())` arm:
    /// the form would close, the status would say nothing was wrong, and
    /// nothing would happen. These drive the real dispatcher.
    fn focus_field(app: &mut App, label: &str) {
        for _ in 0..24 {
            let at = app
                .modal
                .as_ref()
                .expect("no form is open")
                .form
                .focused()
                .map(crate::tui::form::Field::label);
            if at == Some(label) {
                return;
            }
            app.handle(Input::Tab);
        }
        panic!("no field labelled {label}");
    }

    /// Move the action row off `cancel`, the way the user has to.
    fn arm_action(app: &mut App) {
        focus_field(app, crate::tui::form::ACTION);
        for _ in 0..3 {
            if app.modal.as_ref().unwrap().form.confirmed() {
                return;
            }
            app.handle(Input::Right);
        }
        panic!("the action row never left `{}`", crate::tui::form::CANCEL);
    }

    fn session_id(ctx: &Ctx, label: &str) -> String {
        ctx.db()
            .unwrap()
            .list_live_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.label == label)
            .unwrap_or_else(|| panic!("no session labelled {label}"))
            .id
    }

    #[test]
    fn the_loops_dispatcher_runs_a_kill_prompt_and_not_a_quest_one() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        assert_eq!(app.handle(Input::Char('k')), Action::None);
        arm_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);

        submit(&ctx, &mut app);
        assert!(app.modal.is_none(), "{}", app.status);
        assert!(app.status.contains("killed alpha/tests"), "{}", app.status);
        // The proof the prompt was really run: the window is gone.
        let panes = fixture(&_dir).panes;
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].window_name, "master");
    }

    #[test]
    fn the_loops_dispatcher_runs_a_send_prompt_and_not_a_quest_one() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        assert_eq!(app.handle(Input::Char('t')), Action::None);
        focus_field(&mut app, "text");
        for c in "carry on".chars() {
            app.handle(Input::Char(c));
        }
        arm_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);

        submit(&ctx, &mut app);
        assert!(app.modal.is_none(), "{}", app.status);
        assert!(app.status.contains("sent to alpha/tests"), "{}", app.status);
        let panes = fixture(&_dir).panes;
        let worker = panes.iter().find(|p| p.pane_id == "%2").unwrap();
        assert!(worker.buffer.contains("carry on"), "{:?}", worker.buffer);
        // And only into that pane.
        let master = panes.iter().find(|p| p.pane_id == "%1").unwrap();
        assert!(!master.buffer.contains("carry on"), "{:?}", master.buffer);
    }

    #[test]
    fn the_loops_dispatcher_runs_a_reset_prompt_and_not_a_quest_one() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        assert_eq!(app.handle(Input::Char('Z')), Action::None);
        arm_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);

        submit(&ctx, &mut app);
        assert!(app.modal.is_none(), "{}", app.status);
        assert!(
            app.status.contains("resetting alpha/tests"),
            "{}",
            app.status
        );
        let id = session_id(&ctx, "tests");
        let events = ctx
            .db()
            .unwrap()
            .list_events_by_kinds(&quest_id(&ctx), &["session.reset_scheduled"], 10)
            .unwrap();
        assert_eq!(events.len(), 1, "nothing was scheduled");
        assert_eq!(events[0].session_id.as_deref(), Some(id.as_str()));
    }

    /// A failing session prompt must come back into the form the way a Quest
    /// prompt does, rather than closing as if it had worked.
    #[test]
    fn a_session_prompt_that_fails_keeps_the_form_up_with_the_reason() {
        let (ctx, _dir) = fleet_ctx();
        let mut app = on_session(&ctx, "tests");
        app.handle(Input::Char('k'));
        // The window dies while the box is up.
        let tmux = crate::tmux::FixtureTmux::new(fixture_path(&_dir));
        let mut state = tmux.load().unwrap();
        state.panes.retain(|p| p.pane_id != "%2");
        tmux.save(&state).unwrap();
        arm_action(&mut app);
        app.handle(Input::Enter);

        submit(&ctx, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("the form closed on a failure")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("Esc and try again"), "{error}");
    }

    fn quest_id(ctx: &Ctx) -> String {
        ctx.db().unwrap().list_quests(true).unwrap()[0].id.clone()
    }

    /// N-1. SPEC §17's `⏎` is "attach to exactly that window". `enter::resolve`
    /// answers a narrower question — which *live* session carries this label —
    /// and a label is reused once its worker is gone, so with `a` on, an ended
    /// `alpha/tests` would have attached to its live replacement under a name
    /// identical to the row's.
    #[test]
    fn enter_refuses_a_row_whose_label_now_belongs_to_another_session() {
        use crate::model::{Session, SessionRole, SessionStatus};
        let (ctx, _dir) = fleet_ctx();
        // The worker ends and a replacement takes its label and a new pane.
        let old = session_id(&ctx, "tests");
        ctx.db()
            .unwrap()
            .mark_session_ended(&old, crate::model::now())
            .unwrap();
        let quest = quest_id(&ctx);
        let mut row = Session::new(&quest, SessionRole::Worker, "tests", "q-alpha", "%2");
        row.status = SessionStatus::Idle;
        let fresh = ctx.db().unwrap().insert_session(&row).unwrap();
        assert_ne!(fresh.id, old);

        let mut app = loaded(&ctx);
        // The cold Sessions tab asks for its rows on the way in.
        assert_eq!(app.handle(Input::Char('2')), Action::Refresh);
        // `a` shows the ended rows, and the reload it asks for is what puts
        // them in the listing.
        assert_eq!(app.handle(Input::Char('a')), Action::Refresh);
        refresh_now(&ctx, &mut app);
        for _ in 0..8 {
            if sessions::selected(&app).map(|s| s.session) == Some(old.clone()) {
                break;
            }
            app.handle(Input::Down);
        }
        assert_eq!(sessions::selected(&app).map(|s| s.session), Some(old));

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");
        assert!(
            app.status.contains("not that session any more"),
            "it attached to the replacement: {}",
            app.status
        );
        assert!(fixture(&_dir).attached.is_none());
        assert!(term.calls.is_empty(), "{:?}", term.calls);
    }

    /// With nothing selected there is nothing to attach to, and the loop must
    /// not blank the screen to find that out.
    #[test]
    fn attaching_with_an_empty_listing_does_nothing_at_all() {
        let (ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        let mut app = App::new(&ctx.config, "laptop");
        app.set_size(120, 30);
        assert_eq!(app.handle(Input::Char('o')), Action::None);

        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let mut terminal = test_terminal();
        attach(&ctx, &mut term, &mut terminal, &mut app).expect("attach");
        assert!(term.calls.is_empty(), "{:?}", term.calls);
        assert!(fixture(&_dir).attached.is_none());
    }

    // ---------------------------------------------------------- the `b` pager

    #[test]
    fn b_pages_the_selections_brief_through_the_same_handoff() {
        let (ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        let mut app = loaded(&ctx);
        assert_eq!(app.handle(Input::Char('b')), Action::Brief);

        let out = _dir.path().join("paged.md");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();
        let mut terminal = test_terminal();
        pager::with_pager(Some(&format!("tee {}", out.display())), || {
            brief_in_pager(&ctx, &mut term, &mut terminal, &mut app).expect("brief");
        });

        let paged = std::fs::read_to_string(&out).unwrap();
        assert!(paged.contains("needs-me"), "{paged}");
        assert!(paged.contains("the goal of needs-me"), "{paged}");
        // Same one mechanism as the attach, in the same order.
        assert_eq!(
            term.calls,
            [
                "cursor show",
                "mouse off",
                "paste off",
                "alt off",
                "raw off",
                "flush",
                "raw on",
                "alt on",
                "paste on",
                "mouse on",
            ]
        );
        assert_eq!(flags(), (true, true, true, true));
        restore_with(&mut term);
    }

    /// N-4. The brief is the one library call the TUI used to make with a `bd`
    /// discovered off the process environment: `brief::render`'s default
    /// `bd_list` was `beads::client()`, a client that writes progress notices
    /// to stderr — onto the alternate screen the TUI still owns — and that a
    /// unit test cannot replace, so it shells out to the real `bd`.
    ///
    /// Now the brief goes through `Ctx`'s own client, which is what this
    /// proves: the issue below exists only in the injected stub.
    #[test]
    fn the_brief_reads_beads_through_the_ctxs_own_client() {
        let (ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        let quest = ctx.db().unwrap().list_quests(true).unwrap().remove(0);
        ctx.db()
            .unwrap()
            .update_quest(
                &quest.id,
                &crate::db::quest::QuestPatch {
                    beads_epic: Some(Some("bd-e1".to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        let label = format!("quest:{}", quest.id);
        let mut stub = crate::beads::stub::StubBd::working("bd-e1");
        stub.listing = Some(
            serde_json::json!([
                { "id": "bd-77", "title": "only the stub knows this",
                  "status": "blocked", "labels": [&label] },
            ])
            .to_string(),
        );
        let ctx = ctx.with_bd(Box::new(std::sync::Arc::new(stub)));
        let mut app = loaded(&ctx);

        let out = _dir.path().join("paged.md");
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        pager::with_pager(Some(&format!("tee {}", out.display())), || {
            brief_in_pager(&ctx, &mut term, &mut terminal, &mut app).expect("brief");
        });
        restore_with(&mut term);

        let paged = std::fs::read_to_string(&out).unwrap();
        assert!(
            paged.contains("only the stub knows this"),
            "the brief went to a `bd` the Ctx does not own\n{paged}"
        );
    }

    /// A pager that will not start is a status message; the TUI is still there
    /// underneath it.
    #[test]
    fn a_pager_that_cannot_start_is_reported_and_the_tui_comes_back() {
        let (ctx, _dir) = quest_ctx(
            "needs-me",
            crate::model::QuestState::Active,
            true,
            &[("q-needs-me", "%1")],
        );
        let mut app = loaded(&ctx);
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        let mut terminal = test_terminal();
        pager::with_pager(Some("q-no-such-pager-exists"), || {
            brief_in_pager(&ctx, &mut term, &mut terminal, &mut app).expect("brief");
        });
        assert!(
            app.status.contains("cannot page the brief"),
            "{}",
            app.status
        );
        assert_eq!(flags(), (true, true, true, true));
        restore_with(&mut term);
    }

    // ----------------------------------------------------------- signals (4.7)

    /// bd-8lz.4.7: a signal has to undo exactly what the guard would have, and
    /// the two must not both do it — whichever gets there first wins.
    #[test]
    fn a_signal_restore_and_the_guard_cannot_both_undo_the_terminal() {
        let _lock = lifecycle_lock();
        let mut term = FakeTerm::default();
        let guard = arm(&mut term, true).expect("arm");
        std::mem::forget(guard);
        term.calls.clear();

        // Everything the handler does short of re-raising, which would take
        // the test runner with it — with its output pointed at a pipe, since
        // `libc::write` bypasses libtest's capture and would otherwise put
        // these escapes on the developer's own terminal.
        let ((), escapes) = signals::capturing_output(signals::restore_from_signal);
        assert!(!escapes.is_empty(), "the handler restored nothing");
        assert!(escapes.starts_with(b"\x1b[?25h"), "{escapes:?}");
        assert_eq!(flags(), (false, false, false, false));

        // The guard now has nothing left to do, so a `q` that was killed mid
        // teardown cannot double-write the escapes.
        restore_with(&mut term);
        assert!(term.calls.is_empty(), "{:?}", term.calls);
    }
}
