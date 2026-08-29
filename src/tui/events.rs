//! Events tab (SPEC §17): the live tail — every Quest's log on one screen,
//! filtered by Quest and by kind.
//!
//! The feed itself is not computed here — [`crate::commands::events::load`] is
//! the one definition of "the event feed", shared with `q events`, so the tail
//! and the command line can never disagree about which rows exist, which kinds
//! they match, or what the Quest and session they belong to are called. The
//! kind box parses its patterns with [`KindPattern::parse`], the very parser
//! behind `--kind`.
//!
//! Nothing on this tab acts. It reads, and that is all it does: there is no
//! `Action` variant here, no form, and no prompt.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Wrap};

use crate::Ctx;
use crate::commands::events::{self as feed, EventRow};
use crate::commands::fmt;
use crate::db::event::{EventFilter, KindPattern};
use crate::model::Quest;

use super::app::{Action, App};
use super::keys::Input;
use super::layout;

/// The tab's own half of the `?` overlay.
pub const HELP: &[(&str, &str)] = &[
    ("↑ ↓ / k j", "move (leaving the last row pauses the tail)"),
    ("Enter / d", "toggle the whole event, payload and all"),
    ("/", "filter by kind — `note`, `session.*`, several at once"),
    (
        "Esc",
        "close the panel, then the kind filter, then the quest",
    ),
    ("g / G", "oldest / newest (G resumes the tail)"),
];

/// How much of the log the tab holds. Far more than a screenful — scrolling
/// back is the point of a tail — and bounded, because this is re-read on every
/// tick.
const TAIL: usize = 500;
/// Columns the Quest slug may take before it is cut (SPEC §10 allows 40).
const QUEST_COLS: usize = 20;
/// The same for a session label.
const SESSION_COLS: usize = 14;
/// And for a kind, which is `<area>.<what>` by convention.
const KIND_COLS: usize = 22;
/// Payload text kept in a row. The panel has the rest.
const PAYLOAD_COLS: usize = 200;
/// How far a three-line-mode payload is indented under its meta line.
const INDENT: &str = "    ";

/// Per-tab state, owned by `App`.
#[derive(Debug)]
pub struct State {
    /// The tail as last loaded, oldest first.
    rows: Vec<EventRow>,
    /// Index into the *visible* rows.
    selected: usize,
    /// The selected event's id, so a reload that brings new rows in keeps the
    /// selection on the same event rather than on the same line. Events are
    /// append-only and never renumbered, which makes the id a perfect anchor.
    selected_id: Option<i64>,
    /// First row drawn.
    offset: usize,
    /// `e` on the Quests tab (SPEC §17): only this Quest's events. Held by id;
    /// the slug is re-derived on every reload so a rename in another terminal
    /// cannot leave the chip lying.
    quest: Option<String>,
    quest_slug: Option<String>,
    /// `/`: the kind patterns, and whether the box is open.
    query: String,
    filtering: bool,
    /// The kind patterns `rows` were actually fetched under. The `/` box
    /// reaches the SQL, so a query that changes without a reload leaves rows
    /// that are narrower than the filters claim — and, when the dropped box
    /// matched nothing, an EMPTY listing that would otherwise read as "no
    /// events yet" with a full log in the database.
    loaded_query: String,
    /// Whether the tail follows the newest event. True until the selection is
    /// moved off the last row, and true again the moment it comes back — so
    /// `G` is "resume" and there is no separate mode key to get out of step
    /// with where the selection actually is.
    follow: bool,
    /// Whether [`refresh`] has ever run for this tab. Empty `rows` alone
    /// cannot say: "the log is empty" and "nobody has asked yet" look
    /// identical, and only one of them is honest to draw.
    loaded: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            rows: Vec::new(),
            selected: 0,
            selected_id: None,
            offset: 0,
            quest: None,
            quest_slug: None,
            query: String::new(),
            filtering: false,
            loaded_query: String::new(),
            follow: true,
            loaded: false,
        }
    }
}

impl State {
    /// Whether `/` has the keyboard. The shell asks before claiming its own
    /// bare-letter keys, so typing `q` into the box does not quit.
    pub fn capturing(&self) -> bool {
        self.filtering
    }

    /// Whether this tab has ever been loaded. A tab that has not must reload
    /// before it is drawn, whatever its filters say.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// Whether `rows` were fetched under a filter that is no longer in force.
    /// Derived rather than flagged: a remembered bit is one more thing to
    /// forget to clear, and the two strings cannot drift.
    pub fn stale(&self) -> bool {
        self.loaded_query != self.query
    }

    /// Give the keyboard back and drop the half-typed filter. Leaving the tab
    /// is the one way out of the box that is not Esc or Enter, and an armed
    /// capture behind another tab is invisible.
    pub fn cancel_capture(&mut self) {
        if self.filtering {
            self.filtering = false;
            self.query.clear();
            // `selected` indexes the *visible* rows: dropping the filter
            // widens them under it, so the selection has to be re-anchored on
            // its own event.
            self.resync();
        }
    }

    /// The filters currently hiding rows, plus whether the tail is live — the
    /// one mode on this tab with no other trace on screen.
    pub fn filters(&self) -> String {
        let mut on: Vec<String> = Vec::new();
        if let Some(quest) = self.quest.as_deref() {
            on.push(format!(
                "quest {}",
                self.quest_slug.as_deref().unwrap_or(quest)
            ));
        }
        // While the box is open it *is* the indicator, in the status bar, with
        // a cursor and a match count; a second copy would only repeat it.
        if !self.query.is_empty() && !self.filtering {
            on.push(format!("kind {}", self.query));
        }
        on.push(if self.follow { "tailing" } else { "paused" }.to_string());
        on.join(" ")
    }

    /// The `/` box's text as `--kind` patterns. `Err` is the message the CLI
    /// would have printed for the same argument.
    fn patterns(&self) -> Result<Vec<KindPattern>, String> {
        parse_kinds(&self.query)
    }

    /// The rows actually on screen, after the quest and kind filters.
    ///
    /// Both are also pushed into the query [`refresh`] runs, so this is a
    /// re-check rather than the only check — which is what keeps the listing
    /// honest in the tick between a filter key and the reload it asks for.
    fn visible(&self) -> Vec<usize> {
        // Parsed once for the whole listing, not once per row: this runs
        // several times a frame over `TAIL` rows.
        let kinds = self.patterns().ok();
        (0..self.rows.len())
            .filter(|i| self.passes(&self.rows[*i], kinds.as_deref()))
            .collect()
    }

    /// Whether one row survives the filters. `kinds` is the parsed `/` box —
    /// `None` while it is half-typed, which matches nothing rather than
    /// everything: the box says why, and a filter that silently stopped
    /// filtering would be a worse lie than an empty listing.
    fn passes(&self, row: &EventRow, kinds: Option<&[KindPattern]>) -> bool {
        if self
            .quest
            .as_deref()
            .is_some_and(|q| q != row.event.quest_id)
        {
            return false;
        }
        match kinds {
            Some([]) => true,
            Some(kinds) => kinds.iter().any(|k| k.matches(&row.event.kind)),
            None => false,
        }
    }

    fn selected_row(&self) -> Option<&EventRow> {
        let visible = self.visible();
        visible.get(self.selected).map(|i| &self.rows[*i])
    }

    /// Keep the selection on the event it was on — or on the newest one while
    /// the tail is live. Falls back to clamping the index when that event is
    /// gone or filtered away.
    fn resync(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
            self.selected_id = None;
            self.offset = 0;
            return;
        }
        if self.follow {
            // The tail: whatever the newest matching row is now.
            self.selected = visible.len() - 1;
        } else if let Some(at) = self
            .selected_id
            .and_then(|id| visible.iter().position(|i| self.rows[*i].event.id == id))
        {
            self.selected = at;
        } else {
            self.selected = self.selected.min(visible.len() - 1);
        }
        self.selected_id = Some(self.rows[visible[self.selected]].event.id);
        self.offset = self.offset.min(self.selected).min(visible.len() - 1);
    }

    fn move_by(&mut self, delta: isize, viewport: usize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
        self.settle(viewport);
    }

    fn move_to(&mut self, at: usize, viewport: usize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        self.selected = at.min(len - 1);
        self.settle(viewport);
    }

    fn settle(&mut self, viewport: usize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        self.selected = self.selected.min(visible.len() - 1);
        self.selected_id = Some(self.rows[visible[self.selected]].event.id);
        // The tail is live exactly while the newest row is the selected one.
        // An arrow key away from the bottom is the user taking the wheel, and
        // a tick that yanked the selection back would make reading impossible.
        self.follow = self.selected + 1 == visible.len();
        let viewport = viewport.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
        // Both branches above only ever push `offset` FORWARD, so a viewport
        // that GREW since the last frame would leave the body half empty with
        // rows above the fold and nothing able to scroll back to them — and on
        // a tail, where the selection is pinned to the last row, nothing ever
        // does. Pulling back to the last full screen is what heals it. A tail
        // carries no group headers, so `viewport` IS the body's row capacity
        // and the pull-back lands flush — unlike the grouped tabs, where the
        // headers make the same clamp a lower bound.
        self.offset = self.offset.min(visible.len().saturating_sub(viewport));
    }
}

/// One `/` box's worth of `--kind` arguments: whitespace- or comma-separated,
/// each parsed by [`KindPattern::parse`] exactly as `q events --kind` parses
/// one. An empty box is no filter at all rather than a filter matching
/// nothing.
fn parse_kinds(text: &str) -> Result<Vec<KindPattern>, String> {
    text.split([' ', ',', '\t'])
        .filter(|token| !token.is_empty())
        .map(|token| KindPattern::parse(token).map_err(|e| format!("{e:#}")))
        .collect()
}

/// Event payloads are written by hooks and by agents, so they are
/// user-influenced text on a raw-mode terminal. Control characters are
/// replaced rather than dropped: the row says something was there, and no
/// escape sequence can reach the terminal through a payload, a kind or a
/// label. Ratatui's own renderer drops most zero-width graphemes, but "the
/// renderer happens to swallow it" is not a property to leave a terminal's
/// safety resting on.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '\u{fffd}' } else { c })
        .collect()
}

/// One column's worth of text: control characters neutralised, then cut to
/// `cols` *display* columns.
///
/// The order is the whole point. `sanitize` turns a zero-width control
/// character into a one-column `\u{fffd}`, so measuring first and sanitizing
/// afterwards under-counts and the cell overflows the column it was budgeted.
/// The cut itself is [`layout::truncate`], which cuts between grapheme
/// clusters — `fmt::truncate` counts `char`s and would strand a combining mark
/// on the ellipsis.
fn cell(text: &str, cols: usize) -> String {
    layout::truncate(&sanitize(text), cols)
}

// ------------------------------------------------------------------ loading

/// Reload this tab's data. Called by the event loop on tick and on `x`, never
/// from the state machine, so `App::handle` stays pure.
pub fn refresh(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    // `e` on the Quests tab hands its selection over (SPEC §17). Consumed
    // rather than read, so the hand-off happens once and `Esc` can clear the
    // filter for good afterwards.
    if let Some(quest) = app.focus_quest.take() {
        app.events.quest = Some(quest);
        app.events.quest_slug = None;
        // The anchor belongs to whatever was selected before the filter; the
        // filtered listing is a different set of rows, and a hand-off is a
        // jump to what is happening in that Quest now.
        app.events.selected_id = None;
        app.events.offset = 0;
        app.events.follow = true;
    }

    let db = ctx.db()?;
    let quests: Vec<Quest> = match app.events.quest.as_deref() {
        // By id, and finished or not: the filter outlives the listing it came
        // from, and a Quest that was closed since still has a log worth
        // reading.
        Some(id) => db.get_quest(id)?.into_iter().collect(),
        None => db
            .list_quests(true)?
            .into_iter()
            .filter(|q| ctx.machine_filter().is_none_or(|m| m == q.machine))
            .collect(),
    };
    // Re-derived every reload rather than remembered: a `q rename` in another
    // terminal would otherwise leave the chip naming a slug that is gone.
    app.events.quest_slug = app
        .events
        .quest
        .as_ref()
        .and_then(|_| quests.first().map(|q| q.slug.clone()));

    // The kind filter goes into the query, not only into `visible`: the tail
    // is the last `TAIL` *matching* rows, so filtering to a rare kind finds it
    // instead of finding nothing behind a screenful of notes.
    let filter = EventFilter {
        kinds: app.events.patterns().unwrap_or_default(),
        session_id: None,
    };
    app.events.rows = feed::load(db, &quests, &filter, TAIL)?;
    app.events.loaded_query = app.events.query.clone();
    app.events.loaded = true;
    app.events.resync();
    settle_view(app);
    // The box reports how many rows match, and the reload is what decided
    // that. Safe to write from here: `report_refresh` keeps a failed reload in
    // `refresh_error` and no longer clears `status`.
    if app.events.filtering {
        typing(app);
    }
    Ok(())
}

/// Scroll the selection back into view for the current terminal size.
pub fn settle_view(app: &mut App) {
    let page = viewport(app);
    app.events.settle(page);
}

/// How many rows the body can show. No group headings to reserve for: a tail
/// is chronological, and a heading in the middle of one would be a lie about
/// the order.
fn viewport(app: &App) -> usize {
    let body = app.height.saturating_sub(2) as usize;
    (body / app.row_mode().lines() as usize).max(1)
}

// ------------------------------------------------------------------- keymap

/// Keys the shell did not claim. Pure — and on this tab it could not be
/// anything else: nothing here reaches a terminal, a process or the database.
pub fn handle(app: &mut App, input: Input) -> Action {
    if app.events.filtering {
        return filter_key(app, input);
    }
    let page = viewport(app);
    match input {
        Input::Up | Input::Char('k') => {
            app.events.move_by(-1, page);
            Action::None
        }
        Input::Down | Input::Char('j') => {
            app.events.move_by(1, page);
            Action::None
        }
        Input::PageUp => {
            app.events.move_by(-(page as isize), page);
            Action::None
        }
        Input::PageDown => {
            app.events.move_by(page as isize, page);
            Action::None
        }
        Input::Home | Input::Char('g') => {
            app.events.move_to(0, page);
            Action::None
        }
        Input::End | Input::Char('G') => {
            app.events.move_to(usize::MAX, page);
            Action::None
        }
        // `d` matches the Quests tab; `Enter` stays a toggle here too, since
        // an event row has no master to enter.
        Input::Enter | Input::Char('d') => toggle_detail(app),
        Input::Char('/') => {
            app.events.filtering = true;
            typing(app);
            Action::None
        }
        // Innermost thing first, so one key peels the view back a layer at a
        // time and nothing is dismissed that the user cannot see.
        Input::Esc => {
            if app.detail {
                app.detail = false;
                Action::None
            } else if !app.events.query.is_empty() {
                app.events.query.clear();
                app.events.resync();
                app.say("kind filter cleared");
                // The rows the wider filter admits were never fetched.
                Action::Refresh
            } else if app.events.quest.take().is_some() {
                app.events.quest_slug = None;
                app.events.resync();
                app.say("all quests");
                Action::Refresh
            } else {
                Action::None
            }
        }
        _ => Action::None,
    }
}

/// The `/` box. Only Esc, Enter and editing keys mean anything here; every
/// other character is a kind pattern.
fn filter_key(app: &mut App, input: Input) -> Action {
    match input {
        Input::Esc => {
            app.events.filtering = false;
            app.events.query.clear();
            app.events.resync();
            app.status.clear();
            Action::Refresh
        }
        Input::Enter => {
            app.events.filtering = false;
            let query = app.events.query.clone();
            if query.is_empty() {
                app.status.clear();
            } else {
                match parse_kinds(&query) {
                    Ok(_) => app.say(format!("kind {query} \u{b7} Esc clears")),
                    // Closing the box does not make a bad pattern good — it
                    // still matches nothing, so it still has to say why.
                    Err(why) => app.say(format!("{why} \u{b7} Esc clears")),
                }
            }
            // A paste carries no `Action` by design, so a filter that arrived
            // that way reaches the SQL only when something else asks. This is
            // that something: committing the box is the last moment the query
            // could still be ahead of the rows.
            if app.events.stale() {
                Action::Refresh
            } else {
                Action::None
            }
        }
        Input::Backspace => {
            app.events.query.pop();
            app.events.resync();
            typing(app);
            Action::Refresh
        }
        Input::Char(c) => {
            app.events.query.push(c);
            app.events.resync();
            typing(app);
            Action::Refresh
        }
        _ => Action::None,
    }
}

/// A paste while `/` holds the keyboard: text into the filter, and nothing
/// else. Ignored when the box is not open — a paste is not a way to start one.
pub(super) fn paste(app: &mut App, text: &str) -> bool {
    if !app.events.filtering {
        return false;
    }
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return false;
    }
    app.events.query.push_str(&clean);
    app.events.resync();
    typing(app);
    true
}

/// The box *is* the status bar: there is no room for a second one, and the
/// filtered tail above it is the other half of the feedback.
fn typing(app: &mut App) {
    let query = app.events.query.clone();
    let note = match parse_kinds(&query) {
        Ok(_) => format!("{} matching", app.events.visible().len()),
        Err(why) => why,
    };
    app.say(format!("/{query}\u{2588}  {note}"));
}

fn toggle_detail(app: &mut App) -> Action {
    if app.events.selected_row().is_none() {
        return Action::None;
    }
    app.detail = !app.detail;
    Action::None
}

// -------------------------------------------------------------------- render

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // The size the shell has just published is the one this frame is drawn
    // for, so a resize scrolls the selection back into view before it is
    // missed rather than on the next keypress.
    settle_view(app);
    let app = &*app;
    let (list, panel) =
        layout::panel_split(area, app.detail && app.events.selected_row().is_some());
    if let Some(list) = list {
        render_list(frame, list, app);
    }
    if let Some(panel) = panel {
        render_panel(frame, panel, app);
    }
}

fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.events;
    let visible = state.visible();
    if visible.is_empty() {
        frame.render_widget(Paragraph::new(empty_lines(state)), inset(area));
        return;
    }

    // The Quest is only worth a column when more than one can show up — the
    // same rule the Sessions tab follows.
    let across = state.quest.is_none();
    let cells: Vec<Cells> = visible.iter().map(|i| cells_of(&state.rows[*i])).collect();
    let widths = widths_of(&cells, across);

    let width = area.width as usize;
    let capacity = (area.height as usize).max(1);
    let per_row = app.row_mode().lines() as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (n, _) in visible.iter().enumerate().skip(state.offset) {
        // Saturating, not plain: the exception below draws a whole two-line
        // row into a one-line body, so `lines` can already be longer than the
        // body it is being packed into. `truncate` cuts it back at the end.
        let left = capacity.saturating_sub(lines.len());
        if left == 0 {
            break;
        }
        // A row goes on screen whole or not at all: half of a two-line row
        // under the fold reads as the next event's payload. The exception is a
        // body with no room for even one, where the head of the row is all
        // there is to show and nothing below it can be mistaken for it.
        if left < per_row && !lines.is_empty() {
            break;
        }
        lines.extend(row_lines(
            &cells[n],
            &widths,
            across,
            n == state.selected,
            width,
            per_row,
        ));
    }
    lines.truncate(capacity);
    frame.render_widget(Paragraph::new(lines), area);
}

fn empty_lines(state: &State) -> Vec<Line<'static>> {
    // A committed kind filter reaches the SQL, so "nothing matched" shows up
    // as an EMPTY `rows` rather than as rows the listing filtered away. Asking
    // `rows` alone would then blame the Quest — or the whole fleet — for an
    // emptiness the `/` box caused. The box's own text is the honest witness.
    let why = if !state.rows.is_empty() || !state.query.is_empty() {
        "no events match the filters"
    } else if state.stale() {
        // The box that emptied these rows is gone and what the wider filter
        // admits has not been asked for yet. Blaming the fleet here is the
        // same lie one branch up, one reload later.
        "reloading the tail\u{2026}"
    } else if state.quest.is_none() {
        "no events yet"
    } else if state.quest_slug.is_some() {
        "this quest has no events yet"
    } else {
        // The slug is re-derived every reload, so a Quest filter with no slug
        // behind it is a Quest that was deleted while the filter was up.
        "that quest is gone \u{b7} Esc for all quests"
    };
    vec![
        Line::from(Span::raw(why).bold()),
        Line::from(""),
        Line::from(Span::raw("/ filters by kind \u{b7} Esc clears \u{b7} ? for keys").dim()),
    ]
}

fn inset(area: Rect) -> Rect {
    Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    }
}

/// What a row says: how long ago, which Quest and session, what kind, and as
/// much of the payload as fits. Every cell is sanitized here, once, so the
/// composed row carries no control character into a width calculation or into
/// the terminal.
struct Cells {
    age: String,
    quest: String,
    session: String,
    kind: String,
    payload: String,
}

fn cells_of(row: &EventRow) -> Cells {
    Cells {
        age: fmt::age(row.event.ts),
        quest: cell(&row.quest_slug, QUEST_COLS),
        session: cell(&row.session, SESSION_COLS),
        kind: cell(&row.event.kind, KIND_COLS),
        // `fmt::payload` first, so a row reads the same `k=v k=v` way it reads
        // in `q events`; it caps the text in `char`s, `cell` caps it again in
        // columns.
        payload: cell(
            &fmt::payload(row.event.payload.as_ref(), PAYLOAD_COLS),
            PAYLOAD_COLS,
        ),
    }
}

/// The widest cell in each column, so the columns line up across the tail
/// rather than per screenful. The kind is not among them: it is last on the
/// meta line in two-line mode and alone on its own line in three, so nothing
/// follows it that padding would line up.
fn widths_of(cells: &[Cells], across: bool) -> [usize; 3] {
    let mut w = [0usize; 3];
    for c in cells {
        let each = [
            layout::width(&c.age),
            if across { layout::width(&c.quest) } else { 0 },
            layout::width(&c.session),
        ];
        for (at, value) in each.into_iter().enumerate() {
            w[at] = w[at].max(value);
        }
    }
    w
}

/// One event: when, where and what kind, with the payload on a line of its
/// own underneath. SPEC §17's narrow band gets a third line, spent on the same
/// trade the Quests tab makes — the fixed-width fact (here the kind) moves off
/// the meta line so the variable-width one keeps the full terminal.
///
/// The cursor is NOT drawn the way Quests draws its own: there the reversed
/// style is on the head line alone and the third line is dimmed even on the
/// selected row, so the block is one line tall. Here every line of the row
/// carries it, because an event is two or three lines and has to read as one.
fn row_lines<'a>(
    c: &Cells,
    w: &[usize; 3],
    across: bool,
    selected: bool,
    width: usize,
    per_row: usize,
) -> Vec<Line<'a>> {
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
    };
    let mut head = String::from(if selected { "\u{25b8} " } else { "  " });
    head.push_str(&pad(&c.age, w[0]));
    head.push(' ');
    if across {
        head.push_str(&pad(&c.quest, w[1]));
        head.push(' ');
    }
    head.push_str(&pad(&c.session, w[2]));
    let three = per_row >= 3;
    if !three {
        head.push(' ');
        head.push_str(&c.kind);
    }
    let mut out = vec![
        Line::from(Span::styled(
            layout::truncate(head.trim_end(), width),
            style,
        )),
        Line::from(Span::styled(
            layout::truncate(&format!("{INDENT}{}", c.payload), width),
            style,
        )),
    ];
    if three {
        // Dim only when the row is not the selected one: the cursor's
        // reversed block is what says which event is which, and dimming
        // inside it would break the block up.
        let kind = if selected {
            style
        } else {
            style.add_modifier(Modifier::DIM)
        };
        out.push(Line::from(Span::styled(
            layout::truncate(&format!("{INDENT}{}", c.kind), width),
            kind,
        )));
    }
    out
}

/// Right-pad to `want` display columns. `format!("{:w$}")` counts `char`s, and
/// a slug with a wide glyph in it would then push the columns out of line.
fn pad(s: &str, want: usize) -> String {
    let used = layout::width(s);
    format!("{s}{}", " ".repeat(want.saturating_sub(used)))
}

// --------------------------------------------------------------- detail panel

fn render_panel(frame: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.events.selected_row() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", cell(&row.event.kind, KIND_COLS)))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    // Wrapped rather than clipped: the panel is where a long prompt or a long
    // error is actually read, and a one-line JSON string would otherwise show
    // its first forty columns and an ellipsis.
    frame.render_widget(
        Paragraph::new(panel_lines(row)).wrap(Wrap { trim: false }),
        inner,
    );
}

/// The whole event: when, where, and the payload pretty-printed.
fn panel_lines<'a>(row: &EventRow) -> Vec<Line<'a>> {
    let event = &row.event;
    let mut out = vec![
        Line::from(Span::raw(sanitize(&event.kind)).bold()),
        Line::from(
            Span::raw(format!(
                "{} \u{b7} {} ago",
                fmt::stamp_utc(event.ts),
                fmt::age(event.ts)
            ))
            .dim(),
        ),
        Line::from(
            Span::raw(format!(
                "{} \u{b7} {} \u{b7} #{}",
                sanitize(&row.quest_slug),
                sanitize(&row.session),
                event.id
            ))
            .dim(),
        ),
        Line::from(""),
    ];
    match event.payload.as_ref() {
        None | Some(serde_json::Value::Null) => {
            out.push(Line::from(Span::raw("no payload").dim()));
        }
        Some(value) => {
            let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
            out.extend(
                text.lines()
                    .map(|line| Line::from(Span::raw(sanitize(line)))),
            );
        }
    }
    out
}

// ---------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::{Event, Session, SessionRole};
    use crate::tui::app::Tab;
    use crate::tui::keys::MouseInput;
    use crate::tui::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // ------------------------------------------------------------- fixtures

    /// A `Ctx` over an in-memory database and a fixture tmux the test owns.
    /// Nothing here touches the process environment or the developer's home:
    /// `Ctx::for_tests` bypasses `Q_DB` and `Q_FIXTURE`, and this tab reaches
    /// neither tmux nor the Claude registry nor `bd`.
    struct Rig {
        ctx: Ctx,
        _tmux: tempfile::TempDir,
    }

    impl Rig {
        fn new() -> Rig {
            let tmux = tempfile::tempdir().unwrap();
            std::fs::write(tmux.path().join("tmux.json"), "{}").unwrap();
            Rig {
                ctx: Ctx::for_tests(
                    Config::default(),
                    Db::open_in_memory().unwrap(),
                    Box::new(crate::tmux::FixtureTmux::new(tmux.path().join("tmux.json"))),
                ),
                _tmux: tmux,
            }
        }

        fn db(&self) -> &Db {
            self.ctx.db().unwrap()
        }

        fn quest(&self, slug: &str) -> Quest {
            self.db()
                .insert_quest(&Quest::new(slug, "/tmp/work", "laptop"))
                .unwrap()
        }

        fn session(&self, quest: &Quest, label: &str) -> Session {
            self.db()
                .insert_session(&Session::new(
                    &quest.id,
                    SessionRole::Worker,
                    label,
                    &format!("q-{}", quest.slug),
                    "%1",
                ))
                .unwrap()
        }

        fn event(&self, quest: &Quest, kind: &str) -> Event {
            self.db()
                .append_event(&quest.id, None, kind, &serde_json::Value::Null)
                .unwrap()
        }

        fn event_with(
            &self,
            quest: &Quest,
            session: Option<&Session>,
            kind: &str,
            payload: serde_json::Value,
        ) -> Event {
            self.db()
                .append_event(&quest.id, session.map(|s| s.id.as_str()), kind, &payload)
                .unwrap()
        }

        /// The app the event loop hands this tab, loaded through the SHELL's
        /// dispatcher rather than by calling this module's `refresh` directly:
        /// a routing bug that sent the Events tab somewhere else would
        /// otherwise pass every test in this file.
        fn app(&self) -> App {
            let mut app = App::new(&self.ctx.config, "laptop");
            app.set_size(120, 30);
            // Open by default in production; the band-layout tests want the
            // full width and the panel tests below opt back in.
            app.detail = false;
            app.select(Tab::Events);
            self.reload(&mut app);
            app
        }

        /// One tick, exactly as `refresh_now` runs it.
        fn reload(&self, app: &mut App) {
            crate::tui::refresh(&self.ctx, app).unwrap();
        }

        /// A key through the real dispatcher, honouring the `Action::Refresh`
        /// it returns the way the loop does.
        fn key(&self, app: &mut App, input: Input) -> Action {
            let action = app.handle(input);
            if action == Action::Refresh {
                self.reload(app);
            }
            action
        }

        /// A click on `tab`'s label in the tab bar, through the real
        /// `App::handle_mouse` and honouring its `Action` the way the loop
        /// does. The bar's geometry is whatever the last frame published, so
        /// the caller has to have drawn one.
        fn click_tab(&self, app: &mut App, tab: Tab) -> Action {
            let (_, _, col, _) = crate::tui::app::tab_layout()
                .into_iter()
                .find(|(t, _, _, _)| *t == tab)
                .unwrap();
            let action = app.handle_mouse(MouseInput::Click { col, row: 0 });
            if action == Action::Refresh {
                self.reload(app);
            }
            action
        }
    }

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

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        draw(app, w, h).join("\n")
    }

    /// Every drawn line carrying the cursor's reversed style, as
    /// `(row, text)`. The style is what the block IS — reading the glyphs back
    /// would only find the `\u{25b8}` marker on the head line.
    fn reversed_rows(app: &mut App, w: u16, h: u16) -> Vec<(u16, String)> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .filter(|y| {
                buffer
                    .cell((0, *y))
                    .is_some_and(|c| c.modifier.contains(Modifier::REVERSED))
            })
            .map(|y| {
                let text = (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>();
                (y, text.trim_end().to_string())
            })
            .collect()
    }

    fn kinds(app: &App) -> Vec<String> {
        app.events
            .visible()
            .into_iter()
            .map(|i| app.events.rows[i].event.kind.clone())
            .collect()
    }

    /// The invariant every filter key has to preserve: `selected` indexes the
    /// VISIBLE rows, so it must be in range AND must name the same event
    /// `selected_id` claims. 4.2's blocking B3 was exactly this pair drifting
    /// apart — the cursor on one row, the tab's idea of the selection on
    /// another.
    #[track_caller]
    fn anchored(app: &App) {
        let state = &app.events;
        let visible = state.visible();
        if visible.is_empty() {
            assert_eq!(state.selected, 0, "a selection into an empty listing");
            assert_eq!(state.selected_id, None, "a stale anchor with nothing shown");
            return;
        }
        assert!(
            state.selected < visible.len(),
            "selected {} is past the {} visible rows",
            state.selected,
            visible.len()
        );
        let under_the_cursor = state.rows[visible[state.selected]].event.id;
        assert_eq!(
            state.selected_id,
            Some(under_the_cursor),
            "the anchor names a different event than the cursor is on"
        );
        assert_eq!(
            state.selected_row().map(|r| r.event.id),
            Some(under_the_cursor)
        );
    }

    // -------------------------------------------------------------- loading

    /// The tab is the fleet's tail, and it is the same feed `q events` prints
    /// — `commands::events::load`, not a second query with a second idea of
    /// what an event row is.
    #[test]
    fn the_tail_is_the_shared_feed_across_every_quest() {
        let rig = Rig::new();
        let a = rig.quest("alpha");
        let b = rig.quest("beta");
        rig.event(&a, "quest.created");
        rig.event(&b, "session.start");
        rig.event(&a, "note");

        let app = rig.app();
        assert_eq!(kinds(&app), ["quest.created", "session.start", "note"]);
        let slugs: Vec<&str> = app
            .events
            .rows
            .iter()
            .map(|r| r.quest_slug.as_str())
            .collect();
        assert_eq!(slugs, ["alpha", "beta", "alpha"]);

        // Byte for byte what `load` hands the command line for the same set.
        let want = feed::load(
            rig.db(),
            &rig.db().list_quests(true).unwrap(),
            &EventFilter::default(),
            TAIL,
        )
        .unwrap();
        let ids: Vec<i64> = want.iter().map(|r| r.event.id).collect();
        let got: Vec<i64> = app.events.rows.iter().map(|r| r.event.id).collect();
        assert_eq!(got, ids);
        anchored(&app);
    }

    /// A row names the session an event came from, the way `q events` does.
    #[test]
    fn a_row_names_the_session_and_the_quest() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "tests");
        rig.event_with(
            &quest,
            Some(&session),
            "session.prompt",
            serde_json::json!({ "text": "run the suite" }),
        );
        let mut app = rig.app();
        let body = screen(&mut app, 120, 12);
        assert!(body.contains("alpha"), "{body}");
        assert!(body.contains("tests"), "{body}");
        assert!(body.contains("session.prompt"), "{body}");
        assert!(body.contains("text=run the suite"), "{body}");
    }

    // ------------------------------------------------- filters + re-anchoring

    /// The rail: any mutation of filter state re-anchors. Typing a filter that
    /// still admits the selected event must leave the cursor ON that event,
    /// not on whatever row happens to sit at the same index afterwards.
    #[test]
    fn a_kind_filter_keeps_the_selection_on_the_same_event() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["note", "note", "session.start", "note", "session.stop"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        // Off the tail and onto `session.start`, which is third of five.
        rig.key(&mut app, Input::Char('g'));
        rig.key(&mut app, Input::Char('j'));
        rig.key(&mut app, Input::Char('j'));
        let want = app.events.selected_row().unwrap().event.id;
        assert_eq!(
            app.events.selected_row().unwrap().event.kind,
            "session.start"
        );

        rig.key(&mut app, Input::Char('/'));
        for c in "session.*".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        rig.key(&mut app, Input::Enter);

        assert_eq!(kinds(&app), ["session.start", "session.stop"]);
        assert_eq!(
            app.events.selected_id,
            Some(want),
            "the filter moved the cursor to a different event"
        );
        assert_eq!(app.events.selected, 0);
        anchored(&app);
    }

    /// And when the filter hides the selected event, the cursor lands
    /// somewhere real and says so — never on an index whose row it no longer
    /// names.
    #[test]
    fn a_filter_that_hides_the_selection_re_anchors_onto_a_real_row() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["session.start", "note", "note", "note", "note"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('g'));
        rig.key(&mut app, Input::Char('j')); // a `note`, which the filter drops
        assert_eq!(app.events.selected_row().unwrap().event.kind, "note");

        rig.key(&mut app, Input::Char('/'));
        for c in "session.start".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        rig.key(&mut app, Input::Enter);

        assert_eq!(kinds(&app), ["session.start"]);
        anchored(&app);
        assert_eq!(
            app.events.selected_row().unwrap().event.kind,
            "session.start"
        );
    }

    /// Esc clears the kind filter and widens the listing under the cursor —
    /// the mutation that has to re-anchor on the way back out too.
    #[test]
    fn clearing_the_kind_filter_re_anchors_and_reloads() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["note", "session.start", "note"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('/'));
        for c in "session.*".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        rig.key(&mut app, Input::Enter);
        assert_eq!(kinds(&app), ["session.start"]);
        anchored(&app);

        // Esc while the box is closed clears the committed filter. It has to
        // reload: the rows the wider filter admits were never fetched.
        assert_eq!(app.handle(Input::Esc), Action::Refresh);
        rig.reload(&mut app);
        assert!(app.events.query.is_empty());
        assert_eq!(kinds(&app), ["note", "session.start", "note"]);
        anchored(&app);
    }

    /// Leaving the tab with a half-typed query drops it — and the drop is a
    /// visibility mutation like any other, so it re-anchors. This is the
    /// 4.2 `cancel_capture` bug in its Events-tab shape.
    #[test]
    fn abandoning_a_half_typed_filter_re_anchors() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["note", "session.start", "note", "note"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('/'));
        for c in "session.*".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        assert!(app.events.capturing());
        assert_eq!(app.events.visible().len(), 1);

        // Through `select`. A tab-bar CLICK is the only thing that reaches it
        // with the box open: `handle_global` is gated on `capturing()`, so
        // while the box has the keyboard a digit is text and `Tab` does
        // nothing.
        app.select(Tab::Quests);
        assert!(!app.events.capturing(), "the box outlived the tab");
        assert!(app.events.query.is_empty());
        app.select(Tab::Events);
        // The rail, checked before anything reloads: the cursor and the
        // anchor still name the same event, and it is one that is on screen.
        anchored(&app);

        // The rows the wider filter admits were never fetched -- the committed
        // filter reached the SQL -- so until the reload lands the listing is
        // SHORT AND MISLABELLED, not merely short. That is what `stale()` is
        // for, and why arriving on this tab reloads; here `select` is called
        // directly, below the handler that honours it.
        assert!(app.events.stale());
        assert_eq!(app.events.visible().len(), 1);
        rig.reload(&mut app);
        assert!(!app.events.stale());
        assert_eq!(app.events.visible().len(), 4);
        anchored(&app);
    }

    /// B1: the mouse is the one way out of an open `/` box that is not Esc or
    /// Enter, and the filter it drops had reached the SQL. Coming back to rows
    /// fetched under a filter that no longer exists must not read as "no
    /// events yet" with a full log in the database.
    #[test]
    fn clicking_off_an_open_filter_box_never_leaves_the_tail_lying() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["note", "session.start", "note", "note"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        // The tab bar's geometry is whatever the last frame published.
        draw(&mut app, 120, 30);

        rig.key(&mut app, Input::Char('/'));
        for c in "zzz".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        assert!(
            app.events.rows.is_empty(),
            "the committed filter has to reach the SQL for this to be the bug"
        );

        // Out of the box the only way the keyboard cannot go.
        rig.click_tab(&mut app, Tab::Quests);
        assert_eq!(app.tab, Tab::Quests);
        assert!(!app.events.capturing());
        assert!(app.events.query.is_empty());

        // ...and back. The reload is owed by the tab that is arriving, so it
        // has happened before the first frame is drawn.
        rig.click_tab(&mut app, Tab::Events);
        assert_eq!(app.tab, Tab::Events);
        assert_eq!(kinds(&app), ["note", "session.start", "note", "note"]);
        anchored(&app);

        let screen = screen(&mut app, 120, 30);
        assert!(!screen.contains("no events yet"), "{screen}");
        assert!(screen.contains("session.start"), "{screen}");
    }

    /// N4: the slug is re-derived on every reload, so a Quest filter with
    /// nothing behind it is a Quest that was deleted from another terminal.
    /// Blaming it for having "no events yet" is the B1 lie in its other shape.
    #[test]
    fn a_quest_deleted_under_the_filter_is_not_blamed_for_the_emptiness() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "note");
        let mut app = rig.app();
        app.focus_quest = Some(quest.id.clone());
        rig.reload(&mut app);
        assert_eq!(app.events.quest_slug.as_deref(), Some("alpha"));

        rig.db().delete_quest(&quest.id).unwrap();
        rig.reload(&mut app);
        assert!(app.events.quest.is_some());
        assert_eq!(app.events.quest_slug, None);

        let screen = screen(&mut app, 120, 30);
        assert!(!screen.contains("no events yet"), "{screen}");
        assert!(screen.contains("that quest is gone"), "{screen}");
    }

    /// The quest filter is the other half of SPEC §17's "filter po questu".
    #[test]
    fn the_quest_filter_narrows_the_tail_and_esc_lifts_it() {
        let rig = Rig::new();
        let a = rig.quest("alpha");
        let b = rig.quest("beta");
        rig.event(&a, "a.one");
        rig.event(&b, "b.one");
        rig.event(&a, "a.two");

        let mut app = rig.app();
        app.focus_quest = Some(a.id.clone());
        rig.reload(&mut app);
        assert_eq!(kinds(&app), ["a.one", "a.two"]);
        assert_eq!(app.events.quest_slug.as_deref(), Some("alpha"));
        assert!(app.filters().contains("quest alpha"));
        anchored(&app);

        assert_eq!(app.handle(Input::Esc), Action::Refresh);
        rig.reload(&mut app);
        assert_eq!(kinds(&app), ["a.one", "b.one", "a.two"]);
        assert_eq!(app.events.quest_slug, None);
        anchored(&app);
    }

    /// The chip is re-derived on every reload, so a rename in another terminal
    /// cannot leave it naming a slug that is gone.
    #[test]
    fn the_quest_chip_follows_a_rename() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "note");
        let mut app = rig.app();
        app.focus_quest = Some(quest.id.clone());
        rig.reload(&mut app);
        assert!(app.filters().contains("quest alpha"));

        rig.db()
            .update_quest(
                &quest.id,
                &crate::db::quest::QuestPatch {
                    slug: Some("renamed".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();
        rig.reload(&mut app);
        assert!(app.filters().contains("quest renamed"), "{}", app.filters());
    }

    /// A pattern that cannot be parsed matches nothing rather than everything,
    /// and the box says why — before and after Enter closes it.
    #[test]
    fn a_bad_pattern_matches_nothing_and_says_why() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "note");
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('/'));
        for c in "se*sion".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        assert!(app.status.contains("trailing"), "{}", app.status);
        assert!(app.events.visible().is_empty());
        anchored(&app);

        rig.key(&mut app, Input::Enter);
        assert!(!app.events.capturing());
        assert!(app.status.contains("trailing"), "{}", app.status);

        // And it is recoverable: backspacing the `*` out brings the rows back.
        rig.key(&mut app, Input::Char('/'));
        rig.key(&mut app, Input::Backspace);
        assert!(app.events.visible().is_empty(), "`se*sio` is still bad");
        for _ in 0..6 {
            rig.key(&mut app, Input::Backspace);
        }
        assert_eq!(app.events.query, "");
        assert_eq!(kinds(&app), ["note"]);
        anchored(&app);
    }

    /// N-A: a paste carries no `Action` — that is deliberate — so the key
    /// that COMMITS a pasted filter is the one that has to fetch what it
    /// admits. `Enter` used to close the box over rows fetched under the old
    /// filter, leaving the chip reading `kind rare.kind` over a screen
    /// reading "no events match the filters" with the match in the database.
    #[test]
    fn enter_after_a_paste_fetches_what_the_pasted_filter_admits() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "rare.kind");
        for _ in 0..(TAIL + 20) {
            rig.event(&quest, "note");
        }
        let mut app = rig.app();
        assert!(
            !kinds(&app).contains(&"rare.kind".to_string()),
            "the unfiltered tail should have scrolled it out"
        );

        rig.key(&mut app, Input::Char('/'));
        // The real paste path: `App::paste` is all `apply_event` calls, and it
        // returns a redraw flag, never an `Action` the loop could honour.
        assert!(app.paste("rare.kind"));
        assert_eq!(app.events.query, "rare.kind");

        // The commit. Nothing else stands between the paste and the screen.
        let action = rig.key(&mut app, Input::Enter);
        assert_eq!(
            action,
            Action::Refresh,
            "the committed filter reached the SQL; the rows have to follow"
        );
        assert!(!app.events.capturing());
        assert!(!app.events.stale());

        let body = screen(&mut app, 120, 12);
        assert!(
            !body.contains("no events match the filters"),
            "the screen denies an event the database has:\n{body}"
        );
        assert!(body.contains("rare.kind"), "{body}");
        assert_eq!(kinds(&app), ["rare.kind"]);
        anchored(&app);
    }

    /// The other half of N-A, and the half deliberately left to the tick: a
    /// paste that is never committed. The box is still open, so this is
    /// in-progress editing rather than a verdict, and the next tick fetches
    /// what the pasted filter admits with no key pressed at all.
    #[test]
    fn a_pasted_filter_that_is_never_committed_lands_on_the_next_tick() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "rare.kind");
        for _ in 0..(TAIL + 20) {
            rig.event(&quest, "note");
        }
        let mut app = rig.app();

        rig.key(&mut app, Input::Char('/'));
        assert!(app.paste("rare.kind"));
        assert!(app.events.stale(), "the paste cannot fetch, by design");
        assert!(app.events.capturing(), "the box is still open");

        // No key. Just the tick `refresh_now` runs anyway.
        rig.reload(&mut app);
        assert!(!app.events.stale());
        assert_eq!(kinds(&app), ["rare.kind"]);
        let body = screen(&mut app, 120, 12);
        assert!(body.contains("rare.kind"), "{body}");
        anchored(&app);
    }

    /// The kind filter goes into the SQL, not only into `visible`: a rare kind
    /// behind more than `TAIL` rows still has to be findable.
    #[test]
    fn the_kind_filter_reaches_the_query_not_only_the_listing() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "session.start");
        for _ in 0..(TAIL + 20) {
            rig.event(&quest, "note");
        }
        let mut app = rig.app();
        assert!(
            !kinds(&app).contains(&"session.start".to_string()),
            "the unfiltered tail should have scrolled it out"
        );

        rig.key(&mut app, Input::Char('/'));
        for c in "session.start".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        rig.key(&mut app, Input::Enter);
        assert_eq!(kinds(&app), ["session.start"]);
        anchored(&app);
    }

    // -------------------------------------------------------------- the tail

    /// Live tail: events arriving between ticks pull the selection along, and
    /// moving off the last row stops that until `G` puts it back.
    #[test]
    fn the_tail_follows_until_the_selection_is_moved_and_g_resumes_it() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["one", "two", "three"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        assert!(app.events.follow);
        assert_eq!(app.events.selected_row().unwrap().event.kind, "three");

        // A tick with something new in it.
        rig.event(&quest, "four");
        rig.reload(&mut app);
        assert_eq!(app.events.selected_row().unwrap().event.kind, "four");
        assert!(app.filters().contains("tailing"));
        anchored(&app);

        // Reading back: the tail stops moving under the cursor.
        rig.key(&mut app, Input::Char('k'));
        assert!(!app.events.follow);
        assert!(app.filters().contains("paused"));
        let held = app.events.selected_id;
        rig.event(&quest, "five");
        rig.event(&quest, "six");
        rig.reload(&mut app);
        assert_eq!(app.events.selected_id, held, "the tail yanked the cursor");
        assert_eq!(app.events.selected_row().unwrap().event.kind, "three");
        anchored(&app);

        // `G` is resume, and the next tick keeps following.
        rig.key(&mut app, Input::Char('G'));
        assert!(app.events.follow);
        assert_eq!(app.events.selected_row().unwrap().event.kind, "six");
        rig.event(&quest, "seven");
        rig.reload(&mut app);
        assert_eq!(app.events.selected_row().unwrap().event.kind, "seven");
        anchored(&app);
    }

    /// A paused tail that scrolls back to the bottom by hand is following
    /// again — there is no separate mode key to get out of step with.
    #[test]
    fn stepping_back_onto_the_newest_row_resumes_the_tail() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for kind in ["one", "two"] {
            rig.event(&quest, kind);
        }
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('k'));
        assert!(!app.events.follow);
        rig.key(&mut app, Input::Char('j'));
        assert!(app.events.follow);
        rig.event(&quest, "three");
        rig.reload(&mut app);
        assert_eq!(app.events.selected_row().unwrap().event.kind, "three");
    }

    // ------------------------------------------------------------- empty set

    #[test]
    fn an_empty_listing_says_which_kind_of_empty_it_is() {
        let rig = Rig::new();
        let mut app = rig.app();
        assert!(screen(&mut app, 100, 12).contains("no events yet"));
        anchored(&app);
        // Enter cannot open a panel onto nothing.
        assert_eq!(app.handle(Input::Enter), Action::None);
        assert!(!app.detail);

        let quest = rig.quest("alpha");
        rig.reload(&mut app);
        app.focus_quest = Some(quest.id.clone());
        rig.reload(&mut app);
        assert!(
            screen(&mut app, 100, 12).contains("this quest has no events yet"),
            "{}",
            screen(&mut app, 100, 12)
        );

        rig.event(&quest, "note");
        rig.reload(&mut app);
        rig.key(&mut app, Input::Char('/'));
        for c in "nothing.matches".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        assert!(
            screen(&mut app, 100, 12).contains("no events match the filters"),
            "{}",
            screen(&mut app, 100, 12)
        );
        anchored(&app);

        // And every movement key on an empty listing is a no-op, not a panic.
        for input in [
            Input::Up,
            Input::Down,
            Input::PageUp,
            Input::PageDown,
            Input::Home,
            Input::End,
            Input::Enter,
        ] {
            app.handle(input);
            anchored(&app);
        }
    }

    // ------------------------------------------- payloads: long and hostile

    /// A cell may never be wider than the column it was budgeted, whatever is
    /// in it. Control characters are the trap: `UnicodeWidthStr` costs them
    /// zero, and `sanitize` turns each into a one-column `\u{fffd}`, so a cell
    /// measured before it is sanitized under-counts by its whole length.
    #[test]
    fn cells_never_exceed_their_column_budget() {
        let long = "x".repeat(5_000);
        let control = "\u{0}".repeat(400);
        for text in [
            long.as_str(),
            control.as_str(),
            "\u{1b}[31mred\u{1b}[0m\u{7}",
            "\u{4e2d}\u{6587}".repeat(40).as_str(),
            "e\u{301}".repeat(60).as_str(),
            "\u{2764}\u{fe0f}".repeat(40).as_str(),
            "ok",
            "",
        ] {
            for cols in [1usize, 2, 7, KIND_COLS, QUEST_COLS, PAYLOAD_COLS] {
                let out = cell(text, cols);
                assert!(
                    layout::width(&out) <= cols,
                    "{:?} at {cols} columns became {} wide: {out:?}",
                    text,
                    layout::width(&out)
                );
                assert!(
                    !out.chars().any(char::is_control),
                    "a control character survived into {out:?}"
                );
            }
        }
    }

    /// The whole row, drawn: a payload far longer than the terminal is cut,
    /// and it is cut in the row rather than wrapped onto the next event's line.
    #[test]
    fn a_very_long_payload_is_cut_and_never_wraps() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event_with(
            &quest,
            None,
            "session.prompt",
            serde_json::json!({ "text": "z".repeat(9_000) }),
        );
        rig.event(&quest, "note");
        let mut app = rig.app();

        // Two-line mode at 120, three-line at 60: the payload has its own line
        // in both, and neither pushes the second event off its own row.
        for (w, mode) in [
            (120u16, layout::RowMode::Two),
            (60u16, layout::RowMode::Three),
        ] {
            let lines = draw(&mut app, w, 20);
            assert_eq!(app.row_mode(), mode);
            let zs = lines
                .iter()
                .filter(|l| l.contains("zzzz"))
                .collect::<Vec<_>>();
            assert_eq!(zs.len(), 1, "the payload wrapped at {w}: {lines:?}");
            assert!(zs[0].chars().count() <= w as usize, "{w}: {:?}", zs[0]);
            assert!(
                lines.iter().any(|l| l.contains("note")),
                "the long row swallowed the next one at {w}: {lines:?}"
            );
            // Where the kind sits is the difference between the two bands, and
            // the narrow one only earns its third line by drawing it.
            let own_line = lines
                .iter()
                .any(|l| l.trim_end() == format!("{INDENT}session.prompt"));
            assert_eq!(
                own_line,
                mode == layout::RowMode::Three,
                "the kind is on the wrong line at {w}: {lines:?}"
            );
        }
    }

    /// N1: SPEC §17's narrow band is three lines per event, and `viewport` —
    /// which is what `PageDown` steps by — counts on it. A band that budgets
    /// three and draws two leaves every third body line permanently blank and
    /// puts paging out of step with what is on screen.
    #[test]
    fn the_narrow_band_draws_every_line_it_budgets_for() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "worker");
        for n in 0..6 {
            rig.event_with(
                &quest,
                Some(&session),
                &format!("kind.{n}"),
                serde_json::json!({ "text": format!("payload {n}") }),
            );
        }
        let mut app = rig.app();

        for (w, per_row) in [(120u16, 2usize), (60, 3)] {
            app.set_size(w, 14);
            app.handle(Input::Char('G'));
            assert_eq!(app.row_mode().lines() as usize, per_row);
            let lines = draw(&mut app, w, 14);
            let body: Vec<&String> = lines[1..lines.len() - 1].iter().collect();
            let heads = body
                .iter()
                .filter(|l| !l.is_empty() && !l.starts_with(INDENT))
                .count();
            let drawn = body.iter().filter(|l| !l.is_empty()).count();
            assert_eq!(
                drawn,
                heads * per_row,
                "{w}: {heads} rows drew {drawn} lines: {body:?}"
            );
            // `viewport` is how many rows the body holds, so it is also how
            // many the loop drew.
            assert_eq!(viewport(&app), heads, "{w}: {body:?}");
            // Every line of the selected row is on screen, kind included.
            let row = app.events.selected_row().unwrap();
            assert!(
                body.iter().any(|l| l.contains(&row.event.kind)),
                "{w}: the selected row lost its kind: {body:?}"
            );
        }
    }

    /// N-D: the claim `row_lines` makes about its own cursor. An event is two
    /// or three lines and has to read as ONE, so the reversed block covers
    /// every line of the selected row — unlike the Quests tab, which reverses
    /// its head line alone. Nothing else on screen says which event is which.
    #[test]
    fn the_cursor_block_covers_every_line_of_the_selected_event() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "worker");
        for n in 0..4 {
            rig.event_with(
                &quest,
                Some(&session),
                &format!("kind.{n}"),
                serde_json::json!({ "text": format!("payload {n}") }),
            );
        }
        let mut app = rig.app();

        for (w, per_row) in [(120u16, 2usize), (60, 3)] {
            app.set_size(w, 16);
            app.handle(Input::Char('G'));
            assert_eq!(app.row_mode().lines() as usize, per_row);
            let reversed = reversed_rows(&mut app, w, 16);
            assert_eq!(
                reversed.len(),
                per_row,
                "{w}: the block is {} lines over a {per_row}-line row: {reversed:?}",
                reversed.len()
            );
            // Contiguous, and the row under the cursor is the one it covers.
            let kind = app.events.selected_row().unwrap().event.kind.clone();
            assert!(
                reversed.iter().any(|(_, l)| l.contains(&kind)),
                "{reversed:?}"
            );
            let rows: Vec<u16> = reversed.iter().map(|(y, _)| *y).collect();
            assert!(
                rows.windows(2).all(|p| p[1] == p[0] + 1),
                "the block has a hole in it: {rows:?}"
            );
        }
    }

    /// Payloads are written by hooks and by agents. Nothing in one may reach
    /// the terminal as an escape sequence — in the listing OR in the panel.
    #[test]
    fn control_characters_in_a_payload_render_literally() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "wo\u{1b}[2Jrker");
        rig.event_with(
            &quest,
            Some(&session),
            "note\u{7}",
            serde_json::json!({
                "text": "\u{1b}[31mpwned\u{1b}[0m",
                "osc": "\u{1b}]0;title\u{7}",
                "nul": "a\u{0}b",
            }),
        );
        let mut app = rig.app();

        for (w, h) in [(120u16, 20u16), (60, 20)] {
            let body = screen(&mut app, w, h);
            assert!(
                !body.contains('\u{1b}'),
                "an ESC reached the buffer at {w}x{h}"
            );
            assert!(!body.contains('\u{7}'), "a BEL reached the buffer");
            assert!(!body.contains('\u{0}'), "a NUL reached the buffer");
            assert!(
                body.contains('\u{fffd}'),
                "the control characters vanished instead of being shown: {body}"
            );
        }

        // The panel prints the payload in full, so it is the other surface.
        rig.key(&mut app, Input::Enter);
        assert!(app.detail);
        let panel = screen(&mut app, 120, 20);
        assert!(!panel.contains('\u{1b}'), "an ESC reached the panel");
        assert!(!panel.contains('\u{7}'), "a BEL reached the panel");
        assert!(panel.contains("pwned"), "{panel}");
        assert!(panel.contains('\u{fffd}'), "{panel}");
    }

    // ---------------------------------------------------------- the viewport

    /// A sweep, not a case: at every shape the terminal can take, the selected
    /// row must be ON SCREEN and exactly one row may carry the cursor.
    #[test]
    fn the_selected_row_is_on_screen_at_every_shape() {
        let rig = Rig::new();
        let a = rig.quest("alpha");
        let b = rig.quest("beta-with-a-longer-slug");
        for n in 0..40 {
            let quest = if n % 3 == 0 { &b } else { &a };
            rig.event_with(
                quest,
                None,
                if n % 2 == 0 { "note" } else { "session.stop" },
                serde_json::json!({ "n": n, "text": "t".repeat(n) }),
            );
        }
        let mut app = rig.app();

        for w in [40u16, 60, 70, 99, 100, 140] {
            for h in [3u16, 4, 5, 8, 13, 30] {
                for detail in [false, true] {
                    for up in [0usize, 1, 7, 19, 39] {
                        app.detail = detail;
                        app.set_size(w, h);
                        app.handle(Input::Char('G'));
                        for _ in 0..up {
                            app.handle(Input::Char('k'));
                        }
                        anchored(&app);
                        let want = app.events.selected_row().unwrap().event.id;
                        let lines = draw(&mut app, w, h);
                        // The panel takes the whole body below the split, and
                        // then there is no listing to be on screen in.
                        let listing = layout::panel_split(Rect::new(0, 1, w, h - 2), detail)
                            .0
                            .is_some();
                        let marked = lines.iter().filter(|l| l.contains('\u{25b8}')).count();
                        if listing {
                            assert_eq!(
                                marked, 1,
                                "{w}x{h} detail={detail} up={up}: {marked} cursors: {lines:?}"
                            );
                        }
                        assert_eq!(
                            app.events.selected_row().map(|r| r.event.id),
                            Some(want),
                            "drawing moved the selection at {w}x{h}"
                        );
                    }
                }
            }
        }
    }

    /// Both row modes draw the event, and neither hides half of one under the
    /// fold: a payload line with no meta line above it reads as the previous
    /// event's.
    #[test]
    fn a_row_is_drawn_whole_or_not_at_all() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for n in 0..12 {
            rig.event(&quest, &format!("kind.{n}"));
        }
        let mut app = rig.app();
        for (w, per_row) in [(120u16, 2usize), (60, 3)] {
            for h in 3..14u16 {
                app.set_size(w, h);
                app.handle(Input::Char('G'));
                let lines = draw(&mut app, w, h);
                let body: Vec<&String> = lines[1..lines.len() - 1].iter().collect();
                let heads = body
                    .iter()
                    .filter(|l| !l.is_empty() && !l.starts_with(INDENT))
                    .count();
                let drawn = body.iter().filter(|l| !l.is_empty()).count();
                // Every drawn line belongs to a head or to the payload under
                // one, and a partial row is only ever the LAST thing on screen
                // in a body with no room for a whole one.
                assert!(
                    drawn <= heads * per_row,
                    "{w}x{h}: {drawn} lines under {heads} heads: {body:?}"
                );
                if body.len() >= per_row {
                    assert!(heads >= 1, "{w}x{h}: nothing drawn: {body:?}");
                }
            }
        }
    }

    /// D1. The viewport GROWING between two frames has to pull `offset` back,
    /// or the tail leaves the bottom of the screen blank and never heals: on
    /// this tab the selection sits at the END of the listing, so `offset` is
    /// always about `len - viewport` and nothing — not `G`, not `k`, not a
    /// reload — moves the selection far enough to scroll the view up again.
    ///
    /// A sweep rather than the one repro, and the axis that matters is the
    /// pair of shapes: listing size x shape BEFORE x shape AFTER x where the
    /// cursor sits. A sweep that only ever draws one shape per selection
    /// cannot see this. Both orders are swept, so shrinking is covered too,
    /// and 69 and 70 columns are in the list because that is where three-line
    /// rows become two and the body's row count doubles without the height
    /// changing at all.
    #[test]
    fn a_grown_viewport_refills_the_body() {
        const SHAPES: [(u16, u16); 6] =
            [(100, 40), (100, 10), (69, 20), (70, 20), (120, 4), (60, 30)];
        for len in [7usize, 25, 60] {
            let rig = Rig::new();
            let quest = rig.quest("alpha");
            for n in 0..len {
                rig.event_with(
                    &quest,
                    None,
                    &format!("kind.{n}"),
                    serde_json::json!({ "text": format!("payload {n}") }),
                );
            }
            let mut app = rig.app();
            app.detail = false;
            for (w1, h1) in SHAPES {
                for (w2, h2) in SHAPES {
                    for up in [0usize, 1, 5, 1_000] {
                        app.set_size(w1, h1);
                        app.handle(Input::Char('G'));
                        for _ in 0..up {
                            app.handle(Input::Char('k'));
                        }
                        // The first frame settles the view for the first
                        // shape. The second frame IS the resize.
                        draw(&mut app, w1, h1);
                        let lines = draw(&mut app, w2, h2);

                        let at = format!("{len} events, {w1}x{h1} -> {w2}x{h2}, up={up}");
                        let per_row = app.row_mode().lines() as usize;
                        let rows = viewport(&app);
                        let visible = app.events.visible().len();
                        let body = &lines[1..lines.len() - 1];
                        let heads = body
                            .iter()
                            .filter(|l| !l.is_empty() && !l.starts_with(INDENT))
                            .count();
                        let drawn = body.iter().filter(|l| !l.is_empty()).count();

                        // The body is FULL whenever there are enough events to
                        // fill it: as many rows as fit, or the whole listing.
                        assert_eq!(
                            heads,
                            rows.min(visible),
                            "{at}: {heads} rows in a body that holds {rows}: {body:?}"
                        );
                        // And every drawn row is drawn whole. The exception is
                        // a body with no room for even one, where the head is
                        // all there is and nothing below it can be mistaken
                        // for the next event.
                        assert_eq!(
                            drawn,
                            (heads * per_row).min(body.len()),
                            "{at}: a row is half off the bottom: {body:?}"
                        );
                        // The cursor is on screen, exactly once.
                        let marked = body.iter().filter(|l| l.contains('\u{25b8}')).count();
                        assert_eq!(marked, 1, "{at}: {marked} cursors: {body:?}");
                        anchored(&app);
                    }
                }
            }
        }
    }

    // ------------------------------------------------------- the first visit

    /// N1. A tab that has never loaded has to load BEFORE it is drawn.
    /// `stale()` cannot see this on its own: on a cold tab nothing was ever
    /// fetched, so there is no loaded filter to disagree with the live one and
    /// the switch used to return `Action::None`. The tab then drew its "there
    /// is nothing here" copy — a claim about the database made without having
    /// asked it — for up to a whole tick.
    #[test]
    fn the_first_visit_to_a_data_tab_loads_before_it_draws() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "session.start");

        let mut app = App::new(&rig.ctx.config, "laptop");
        app.set_size(120, 30);
        // The loop's own first refresh, which lands on the Quests tab.
        rig.reload(&mut app);
        assert_eq!(app.tab, Tab::Quests);

        // The Sessions tab says "no live sessions" until it has looked.
        assert_eq!(
            rig.key(&mut app, Input::Char('2')),
            Action::Refresh,
            "a cold Sessions tab drew before it loaded"
        );
        // And the Events tab says "no events yet" — with an event in the log.
        assert_eq!(
            rig.key(&mut app, Input::Char('4')),
            Action::Refresh,
            "a cold Events tab drew before it loaded"
        );
        let text = screen(&mut app, 120, 30);
        assert!(!text.contains("no events yet"), "{text}");
        assert!(text.contains("session.start"), "{text}");

        // Having loaded once, neither tab reloads just for being visited:
        // the tick owns that, and a reload per keystroke would put a
        // synchronous query behind the tab bar.
        assert_eq!(rig.key(&mut app, Input::Char('1')), Action::None);
        assert_eq!(rig.key(&mut app, Input::Char('2')), Action::None);
        assert_eq!(rig.key(&mut app, Input::Char('4')), Action::None);
    }

    // ------------------------------------------------------ the `e` hand-off

    /// SPEC §17's "filter po questu", reached the way a user reaches it:
    /// `e` on the Quests tab, through the REAL key dispatcher, with the loop's
    /// own `Action::Refresh` honoured by the shell's `refresh`.
    #[test]
    fn e_on_the_quests_tab_lands_on_that_quests_tail() {
        let rig = Rig::new();
        let a = rig.quest("alpha");
        let b = rig.quest("beta");
        rig.event(&a, "a.one");
        rig.event(&b, "b.one");
        rig.event(&a, "a.two");
        let of = |picked: &Quest, a: &Quest, _b: &Quest| -> Vec<&'static str> {
            if picked.id == a.id {
                vec!["a.one", "a.two"]
            } else {
                vec!["b.one"]
            }
        };

        let mut app = App::new(&rig.ctx.config, "laptop");
        app.set_size(120, 30);
        // This test is about the quest-filter hand-off, not the panel Esc peels
        // first; close the default-open panel so Esc reaches the filter.
        app.detail = false;
        rig.reload(&mut app);
        assert_eq!(app.tab, Tab::Quests);

        // Whichever Quest the listing happens to put the cursor on, `e` has to
        // hand over THAT one — so the expectation is derived from the
        // selection rather than assumed.
        let first = crate::tui::quests::selected_quest(&app).unwrap();
        assert_eq!(rig.key(&mut app, Input::Char('e')), Action::Refresh);
        assert_eq!(app.tab, Tab::Events);
        assert_eq!(app.focus_quest, None, "the hint was not consumed");
        assert_eq!(app.events.quest.as_deref(), Some(first.id.as_str()));
        assert_eq!(kinds(&app), of(&first, &a, &b));
        assert!(
            app.filters().contains(&format!("quest {}", first.slug)),
            "{}",
            app.filters()
        );
        anchored(&app);

        // And it follows the selection rather than a fixed Quest: the other
        // row hands over the other log.
        app.select(Tab::Quests);
        app.handle(Input::Char('j'));
        let second = crate::tui::quests::selected_quest(&app).unwrap();
        assert_ne!(second.id, first.id, "`j` never left the first row");
        rig.key(&mut app, Input::Char('e'));
        assert_eq!(app.events.quest.as_deref(), Some(second.id.as_str()));
        assert_eq!(kinds(&app), of(&second, &a, &b));
        anchored(&app);

        // And the hand-off is a one-shot: Esc gives every Quest back for good.
        assert_eq!(rig.key(&mut app, Input::Esc), Action::Refresh);
        assert_eq!(kinds(&app), ["a.one", "b.one", "a.two"]);
        rig.reload(&mut app);
        assert_eq!(kinds(&app), ["a.one", "b.one", "a.two"]);
    }

    /// `e` is a letter in the `/` box before it is a hand-off, and the shell
    /// is what decides which — so both halves go through the real dispatcher.
    ///
    /// The keyboard cannot reach the hand-off with the box still armed (every
    /// key is text while it is open), so the tear-down itself is proven where
    /// the sibling `s` proves it: `handing_the_selection_to_events_tears_down_
    /// an_armed_capture` in `quests.rs` calls the hand-off against an armed
    /// box directly, exactly as the N14 test for `s` does.
    #[test]
    fn e_is_a_letter_in_the_box_before_it_is_the_hand_off() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "note");
        let mut app = App::new(&rig.ctx.config, "laptop");
        app.set_size(120, 30);
        rig.reload(&mut app);
        app.handle(Input::Char('/'));
        app.handle(Input::Char('a'));
        assert!(app.capturing());

        // `e` is a letter in the box, not the hand-off, while it is open.
        assert_eq!(app.tab, Tab::Quests);
        app.handle(Input::Char('e'));
        assert!(app.status.starts_with("/ae"), "{}", app.status);
        assert_eq!(app.tab, Tab::Quests, "a key in the box switched tabs");

        // Back to a query that still names the Quest, then committed: Enter
        // hands the keyboard back and keeps the filter.
        app.handle(Input::Backspace);
        app.handle(Input::Enter);
        assert!(!app.capturing());
        assert!(app.filters().contains("/a"), "{}", app.filters());

        // Now the same key is the hand-off.
        assert_eq!(rig.key(&mut app, Input::Char('e')), Action::Refresh);
        assert_eq!(app.tab, Tab::Events);
        assert!(!app.quests.capturing(), "the box outlived the tab");
        assert_eq!(app.events.quest.as_deref(), Some(quest.id.as_str()));
        assert!(app.filters().contains("quest alpha"), "{}", app.filters());
        assert_eq!(kinds(&app), ["note"]);
        anchored(&app);
    }

    /// `e` with nothing selected is a no-op rather than a hand-off of nothing.
    #[test]
    fn e_on_an_empty_quests_listing_does_nothing() {
        let rig = Rig::new();
        let mut app = App::new(&rig.ctx.config, "laptop");
        app.set_size(120, 30);
        rig.reload(&mut app);
        assert_eq!(app.handle(Input::Char('e')), Action::None);
        assert_eq!(app.tab, Tab::Quests);
        assert_eq!(app.focus_quest, None);
    }

    /// The overlay has to name it too — a key in the help and nowhere else is
    /// the shape this bead inherited.
    #[test]
    fn the_help_overlay_lists_the_keys_the_tab_answers_to() {
        let rows = crate::tui::app::help_rows(Tab::Events);
        for key in ["/", "g / G", "Enter / d"] {
            assert!(
                rows.iter().any(|(k, _)| *k == key),
                "{key} is not in {rows:?}"
            );
        }
        // And `e` is advertised on the tab that actually binds it.
        assert!(
            crate::tui::app::help_rows(Tab::Quests)
                .iter()
                .any(|(k, _)| *k == "e"),
        );
    }

    // ----------------------------------------------------- the capture gate

    /// `q` in the box is a kind pattern, not quit. The shell asks the tab
    /// before claiming its bare letters.
    #[test]
    fn typing_into_the_box_does_not_reach_the_shells_own_keys() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "quest.created");
        rig.event(&quest, "quest.v2");
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('/'));
        // `q` is quit and `2` is the Sessions tab, outside the box. The
        // pattern is exact -- `quest.` alone would be an exact pattern too,
        // and would match neither row.
        for c in "quest.v2".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        assert!(!app.should_quit, "`q` quit out of the filter box");
        assert_eq!(app.tab, Tab::Events, "a digit switched tabs");
        assert_eq!(app.events.query, "quest.v2");
        assert_eq!(kinds(&app), ["quest.v2"]);
        // The one key that always gets out.
        app.handle(Input::Ctrl('c'));
        assert!(app.should_quit);
    }

    /// A paste is text into the box and nothing else — never a way to open
    /// one, and never a key sequence.
    #[test]
    fn a_paste_only_ever_lands_in_an_open_box() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "session.start");
        rig.event(&quest, "note");
        let mut app = rig.app();

        assert!(!app.paste("session.*"), "a paste opened the box");
        assert!(app.events.query.is_empty());

        rig.key(&mut app, Input::Char('/'));
        assert!(app.paste("session.\u{1b}[C*\r"));
        assert!(
            !app.events.query.chars().any(char::is_control),
            "{:?}",
            app.events.query
        );
        rig.reload(&mut app);
        anchored(&app);
    }

    // ---------------------------------------------------------- the panel

    #[test]
    fn enter_opens_the_whole_event_and_esc_closes_it_first() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event_with(
            &quest,
            None,
            "session.prompt",
            serde_json::json!({ "text": "the whole prompt", "n": 3 }),
        );
        let mut app = rig.app();
        rig.key(&mut app, Input::Char('/'));
        for c in "session.*".chars() {
            rig.key(&mut app, Input::Char(c));
        }
        rig.key(&mut app, Input::Enter);

        rig.key(&mut app, Input::Enter);
        assert!(app.detail);
        let panel = screen(&mut app, 120, 20);
        assert!(panel.contains("the whole prompt"), "{panel}");
        assert!(panel.contains("alpha"), "{panel}");

        // Esc peels the panel first, then the filter, then nothing.
        assert_eq!(app.handle(Input::Esc), Action::None);
        assert!(!app.detail);
        assert!(!app.events.query.is_empty());
        assert_eq!(app.handle(Input::Esc), Action::Refresh);
        assert!(app.events.query.is_empty());
        rig.reload(&mut app);
        assert_eq!(app.handle(Input::Esc), Action::None);
    }

    #[test]
    fn an_event_with_no_payload_says_so() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.event(&quest, "session.stop");
        let mut app = rig.app();
        rig.key(&mut app, Input::Enter);
        assert!(screen(&mut app, 120, 20).contains("no payload"));
    }

    // ------------------------------------------------------------ the parser

    #[test]
    fn the_box_splits_on_whitespace_and_commas() {
        assert_eq!(parse_kinds("").unwrap(), []);
        assert_eq!(parse_kinds("   ").unwrap(), []);
        assert_eq!(
            parse_kinds("note, session.*  phase").unwrap(),
            [
                KindPattern::Exact("note".to_string()),
                KindPattern::Prefix("session.".to_string()),
                KindPattern::Exact("phase".to_string()),
            ]
        );
        assert!(parse_kinds("se*sion").is_err());
    }
}
