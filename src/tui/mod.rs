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

    let mut terminal = enter(mouse)?;
    // Restores on the way out whichever way the loop ends — `?`, break, unwind.
    let _guard = Guard;
    let result = event_loop(ctx, &mut terminal, &mut app, tick);
    let _ = terminal.show_cursor();
    result
}

// ------------------------------------------------------------ terminal state

/// Whether mouse capture is currently on, so the panic hook and the guard
/// undo exactly what `enter` did.
static MOUSE_ON: AtomicBool = AtomicBool::new(false);
static HOOK: Once = Once::new();

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

fn enter(mouse: bool) -> anyhow::Result<Terminal<CrosstermBackend<Stdout>>> {
    // A terminal left in raw mode is the worst way for this to fail, so the
    // hook goes in before raw mode does.
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
    });

    enable_raw_mode()?;
    let mut out = io::stdout();
    if let Err(e) = execute!(out, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(e.into());
    }
    if mouse {
        MOUSE_ON.store(true, Ordering::SeqCst);
        execute!(out, EnableMouseCapture)?;
    }
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

/// Idempotent: safe from the panic hook and the guard, in either order.
fn restore() {
    let mut out = io::stdout();
    if MOUSE_ON.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, DisableMouseCapture);
    }
    let _ = execute!(out, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = out.flush();
}

// ------------------------------------------------------------------ the loop

fn event_loop(
    ctx: &Ctx,
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    tick: Duration,
) -> anyhow::Result<()> {
    refresh(ctx, app)?;
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|frame| render(frame, app))?;

        // Block until the next tick is due rather than spinning on a zero
        // timeout; a TUI that burns a core while idle is a bug.
        let timeout = tick.saturating_sub(last_tick.elapsed());
        if poll(timeout)? {
            let action = match read_event()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => keys::normalize(key)
                    .map(|input| app.handle(input))
                    .unwrap_or(Action::None),
                Event::Mouse(m) if app.mouse => keys::normalize_mouse(m)
                    .map(|input| app.handle_mouse(input))
                    .unwrap_or(Action::None),
                Event::Resize(w, h) => {
                    app.set_size(w, h);
                    Action::None
                }
                _ => Action::None,
            };
            match action {
                Action::Quit => break,
                Action::Refresh => refresh(ctx, app)?,
                Action::None => {}
            }
        }

        if last_tick.elapsed() >= tick {
            app.tick();
            refresh(ctx, app)?;
            last_tick = Instant::now();
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
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
        render_help(frame, area);
    }
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let machine = format!(" {} ", app.machine);
    let [bar, right] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(layout::width(&machine) as u16),
    ])
    .areas(area);

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
    frame.render_widget(Paragraph::new(Line::from(Span::raw(machine).bold())), right);
}

fn render_status(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    let hint = " ? help · x refresh · q quit ";
    let left = if app.status.is_empty() {
        format!(
            " {} · rows {} · tick {}s · mouse {}",
            app.machine,
            app.row_mode().lines(),
            app.tick_secs,
            if app.mouse { "on" } else { "off" },
        )
    } else {
        format!(" {}", app.status)
    };

    let [l, r] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(layout::width(hint) as u16),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::raw(layout::truncate(
            &left,
            l.width as usize,
        )))),
        l,
    );
    frame.render_widget(Paragraph::new(Line::from(Span::raw(hint).dim())), r);
}

fn render_help(frame: &mut Frame, area: Rect) {
    let key_width = app::HELP
        .iter()
        .map(|(k, _)| layout::width(k))
        .max()
        .unwrap_or(0);
    let inner_width = app::HELP
        .iter()
        .map(|(_, d)| key_width + 2 + layout::width(d))
        .max()
        .unwrap_or(0);
    let box_area = layout::centered(area, (inner_width + 4) as u16, (app::HELP.len() + 2) as u16);

    let lines: Vec<Line> = app::HELP
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
    use crate::tui::keys::Input;
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
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("Quests tab"), "{body}");
        assert!(body.contains("bd-8lz.4.2"), "{body}");

        app.handle(Input::Char('4'));
        let body = draw(&mut app, 100, 20).join("\n");
        assert!(body.contains("Events tab"), "{body}");
        assert!(!body.contains("Quests tab"), "{body}");
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
        // Sized so the centred overlay covers the body text underneath it.
        let rendered = draw(&mut app, 70, 12).join("\n");
        assert!(rendered.contains("Keys"), "{rendered}");
        for (key, desc) in app::HELP {
            assert!(rendered.contains(key), "missing key {key}: {rendered}");
            assert!(rendered.contains(desc), "missing desc {desc}: {rendered}");
        }
        // `Clear` really wiped what was underneath.
        assert!(!rendered.contains("Quests tab"), "{rendered}");

        // Dismissing it brings the body back.
        app.handle(Input::Esc);
        let rendered = draw(&mut app, 70, 12).join("\n");
        assert!(rendered.contains("Quests tab"), "{rendered}");
        assert!(!rendered.contains("Keys"), "{rendered}");
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
