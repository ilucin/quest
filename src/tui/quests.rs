//! Quests tab (SPEC §17): grouped two-line rows, the beads bar, the master's
//! context reading, and the detail panel behind `Enter`.
//!
//! The listing itself is not computed here — [`crate::commands::load_quests`]
//! is the one definition of "the Quest listing", shared with `q list`, so the
//! CLI and the TUI can never disagree about what exists or in what order.
//! This module only turns those rows into lines.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::Ctx;
use crate::commands::{QuestRow, fill_progress, fmt, load_quests};
use crate::model::{DisplayState, Event, Link, Session, SessionRole, SessionStatus};

use super::app::{Action, App, Tab};
use super::keys::Input;
use super::layout::{self, RowMode};

/// The tab's own half of the `?` overlay.
pub const HELP: &[(&str, &str)] = &[
    ("Enter / o", "toggle the detail panel"),
    ("s", "this Quest's sessions"),
    ("n", "new Quest"),
    ("r / c / R", "rename · close · resume"),
    ("b", "brief in a pager"),
    ("l", "links"),
    ("f", "show finished Quests"),
    ("m", "cycle the machine filter"),
    ("/", "search (Esc clears, Enter keeps)"),
    ("g / G", "first / last row"),
];

/// Events the detail panel shows (SPEC §17: "zadnjih 10 evenata").
const EVENTS: usize = 10;
/// Cells in the beads mini bar.
const BAR: usize = 7;
/// Groups a listing can be split into: needs-you, active, idle, finished.
const GROUPS: usize = 4;
/// Columns the detail panel wants when it can have them.
const PANEL_COLS: u16 = 44;
/// Below this the panel takes the whole body rather than squeezing the list.
const PANEL_SPLIT_COLS: u16 = 88;
/// Payload text kept in an event line.
const PAYLOAD_COLS: usize = 40;
/// How far the second line is indented under the glyphs.
const INDENT: &str = "    ";

/// Per-tab state, owned by `App`.
#[derive(Default)]
pub struct State {
    /// The listing as last loaded: already swept, machine-filtered and ranked.
    rows: Vec<QuestRow>,
    /// Links per row, index-aligned with `rows`.
    links: Vec<Vec<Link>>,
    /// The selected Quest's most recent events; only the selection needs them.
    events: Vec<Event>,
    /// Index into the *visible* rows.
    selected: usize,
    /// The selected Quest's id, so a reload that reorders keeps the selection
    /// on the same Quest rather than on the same line.
    selected_id: Option<String>,
    /// First row drawn — moved only when the selection would leave the view,
    /// so an arrow key does not scroll a list that did not need to.
    offset: usize,
    /// `f`: finished Quests are hidden by default (SPEC §17).
    show_finished: bool,
    /// `m`: `None` is every machine.
    machine: Option<String>,
    /// `/`: the committed query, and whether the box is open.
    query: String,
    searching: bool,
}

impl State {
    /// Whether `/` has the keyboard. The shell asks before claiming its own
    /// bare-letter keys, so typing `q` into the box does not quit.
    pub fn capturing(&self) -> bool {
        self.searching
    }

    /// The rows actually on screen, after the `f`, `m` and `/` filters.
    fn visible(&self) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|i| self.passes(&self.rows[*i]))
            .collect()
    }

    fn passes(&self, row: &QuestRow) -> bool {
        let quest = &row.view.quest;
        if !self.show_finished && row.view.display_state == DisplayState::Finished {
            return false;
        }
        if self.machine.as_deref().is_some_and(|m| m != quest.machine) {
            return false;
        }
        if self.query.is_empty() {
            return true;
        }
        let needle = self.query.to_lowercase();
        let haystacks = [
            quest.slug.as_str(),
            quest.goal.as_deref().unwrap_or(""),
            quest.id.as_str(),
            quest.machine.as_str(),
        ];
        haystacks.iter().any(|h| h.to_lowercase().contains(&needle))
    }

    fn selected_row(&self) -> Option<&QuestRow> {
        let visible = self.visible();
        visible.get(self.selected).map(|i| &self.rows[*i])
    }

    fn selected_links(&self) -> &[Link] {
        let visible = self.visible();
        visible
            .get(self.selected)
            .and_then(|i| self.links.get(*i))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Keep the selection on the Quest it was on; fall back to clamping the
    /// index when that Quest is gone or filtered away.
    fn resync(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
            self.selected_id = None;
            self.offset = 0;
            return;
        }
        if let Some(id) = self.selected_id.as_deref()
            && let Some(at) = visible
                .iter()
                .position(|i| self.rows[*i].view.quest.id == id)
        {
            self.selected = at;
        } else {
            self.selected = self.selected.min(visible.len() - 1);
        }
        self.selected_id = Some(self.rows[visible[self.selected]].view.quest.id.clone());
        self.offset = self.offset.min(self.selected).min(visible.len() - 1);
    }

    fn move_by(&mut self, delta: isize, viewport: usize) {
        let len = self.visible().len();
        if len == 0 {
            return;
        }
        let last = len as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
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

    /// Remember which Quest is selected, and scroll only as far as it takes to
    /// keep it on screen.
    fn settle(&mut self, viewport: usize) {
        let visible = self.visible();
        self.selected_id = visible
            .get(self.selected)
            .map(|i| self.rows[*i].view.quest.id.clone());
        let viewport = viewport.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
    }
}

// ------------------------------------------------------------------ loading

/// Reload this tab's data. Called by the event loop on tick and on `x`, never
/// from the state machine, so `App::handle` stays pure.
pub fn refresh(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    // `f` decides whether finished Quests are even fetched; the filter in
    // `visible` then only has the machine and the query left to do.
    let mut rows = load_quests(ctx, app.quests.show_finished)?;
    fill_progress(&mut rows);

    let db = ctx.db()?;
    let mut links = Vec::with_capacity(rows.len());
    for row in &rows {
        links.push(db.list_links_by_quest(&row.view.quest.id)?);
    }

    app.quests.rows = rows;
    app.quests.links = links;
    app.quests.resync();

    // Only the selection's events are read: the panel shows one Quest's.
    app.quests.events = match app.quests.selected_id.as_deref() {
        Some(id) => db.list_events_by_quest(id, EVENTS)?,
        None => Vec::new(),
    };
    Ok(())
}

// ------------------------------------------------------------------- keymap

/// Keys the shell did not claim. Pure: anything needing the terminal leaves
/// through an `Action`, never from in here.
pub fn handle(app: &mut App, input: Input) -> Action {
    if app.quests.searching {
        return search_key(app, input);
    }
    let page = viewport(app);
    match input {
        Input::Up | Input::Char('k') => {
            app.quests.move_by(-1, page);
            Action::None
        }
        Input::Down | Input::Char('j') => {
            app.quests.move_by(1, page);
            Action::None
        }
        Input::PageUp => {
            app.quests.move_by(-(page as isize), page);
            Action::None
        }
        Input::PageDown => {
            app.quests.move_by(page as isize, page);
            Action::None
        }
        Input::Home | Input::Char('g') => {
            app.quests.move_to(0, page);
            Action::None
        }
        Input::End | Input::Char('G') => {
            app.quests.move_to(usize::MAX, page);
            Action::None
        }
        // SPEC §17 binds Enter to attaching to the master. Attaching suspends
        // the TUI and execs tmux, which is bd-8lz.4.3; until then Enter is the
        // detail panel, which is what `o` will keep meaning afterwards.
        Input::Enter | Input::Char('o') => toggle_detail(app),
        Input::Esc => {
            if app.detail {
                app.detail = false;
            } else if !app.quests.query.is_empty() {
                app.quests.query.clear();
                app.quests.resync();
                app.say("search cleared");
            }
            Action::None
        }
        Input::Char('s') => sessions_of_selection(app),
        Input::Char('f') => {
            app.quests.show_finished = !app.quests.show_finished;
            app.quests.resync();
            app.say(if app.quests.show_finished {
                "showing finished quests"
            } else {
                "finished quests hidden"
            });
            // The finished rows were never fetched, so this needs a reload.
            Action::Refresh
        }
        Input::Char('m') => {
            cycle_machine(app);
            Action::None
        }
        Input::Char('/') => {
            app.quests.searching = true;
            typing(app);
            Action::None
        }
        Input::Char('n') => todo_key(app, "new quest", "bd-8lz.4.4"),
        Input::Char('r') => selection_todo(app, "rename", "bd-8lz.4.4"),
        Input::Char('c') => selection_todo(app, "close", "bd-8lz.4.4"),
        Input::Char('R') => selection_todo(app, "resume", "bd-8lz.4.4"),
        Input::Char('b') => selection_todo(app, "brief in a pager", "bd-8lz.4.3"),
        Input::Char('l') => show_links(app),
        _ => Action::None,
    }
}

/// How many rows the body can show. There are at most four groups, so
/// reserving four lines for their headers is a bound rather than a guess, and
/// the selection is on screen however the rows happen to be grouped.
fn viewport(app: &App) -> usize {
    let body = app.height.saturating_sub(2) as usize;
    (body.saturating_sub(GROUPS) / app.row_mode().lines() as usize).max(1)
}

/// The `/` box. Only Esc, Enter and editing keys mean anything here; every
/// other character is text.
fn search_key(app: &mut App, input: Input) -> Action {
    match input {
        Input::Esc => {
            app.quests.searching = false;
            app.quests.query.clear();
            app.quests.resync();
            app.status.clear();
            Action::None
        }
        Input::Enter => {
            app.quests.searching = false;
            let query = app.quests.query.clone();
            if query.is_empty() {
                app.status.clear();
            } else {
                app.say(format!("filter /{query} · Esc clears"));
            }
            Action::None
        }
        Input::Backspace => {
            app.quests.query.pop();
            app.quests.resync();
            typing(app);
            Action::None
        }
        Input::Char(c) => {
            app.quests.query.push(c);
            app.quests.resync();
            typing(app);
            Action::None
        }
        _ => Action::None,
    }
}

/// The box *is* the status bar: there is no room for a second one, and the
/// filtered list above it is the other half of the feedback.
fn typing(app: &mut App) {
    let query = app.quests.query.clone();
    let matched = app.quests.visible().len();
    app.say(format!("/{query}\u{2588}  {matched} matching"));
}

fn toggle_detail(app: &mut App) -> Action {
    if app.quests.selected_row().is_none() {
        return Action::None;
    }
    app.detail = !app.detail;
    Action::None
}

/// `s` — hand the selection to the Sessions tab (bd-8lz.4.5 reads
/// `App::focus_quest`; until then the tab is still its placeholder).
fn sessions_of_selection(app: &mut App) -> Action {
    let Some(row) = app.quests.selected_row() else {
        return Action::None;
    };
    let (id, slug) = (row.view.quest.id.clone(), row.view.quest.slug.clone());
    app.focus_quest = Some(id);
    app.tab = Tab::Sessions;
    app.say(format!("sessions of {slug}"));
    Action::Refresh
}

/// `m` — no prompt: the machines are whatever the listing holds, cycled
/// through in order and back to "all".
fn cycle_machine(app: &mut App) {
    let mut machines: Vec<String> = app
        .quests
        .rows
        .iter()
        .map(|r| r.view.quest.machine.clone())
        .collect();
    machines.sort();
    machines.dedup();
    let next = match app.quests.machine.as_deref() {
        None => machines.first().cloned(),
        Some(current) => machines
            .iter()
            .position(|m| m == current)
            .and_then(|at| machines.get(at + 1))
            .cloned(),
    };
    app.quests.machine = next;
    app.quests.resync();
    match app.quests.machine.as_deref() {
        Some(m) => app.say(format!("machine {m}")),
        None => app.say("all machines"),
    }
}

/// `l` — the panel already lists them; the status line is the one-glance form.
fn show_links(app: &mut App) -> Action {
    if app.quests.selected_row().is_none() {
        return Action::None;
    }
    let links = app.quests.selected_links();
    let message = if links.is_empty() {
        "no links".to_string()
    } else {
        links.iter().map(link_cell).collect::<Vec<_>>().join(" · ")
    };
    app.detail = true;
    app.say(message);
    Action::None
}

fn todo_key(app: &mut App, what: &str, bead: &str) -> Action {
    app.say(format!("{what}: lands in {bead}"));
    Action::None
}

/// The selection plumbing is live even where the prompt is not: the message
/// names the Quest the action would have run against.
fn selection_todo(app: &mut App, what: &str, bead: &str) -> Action {
    let Some(row) = app.quests.selected_row() else {
        return Action::None;
    };
    let slug = row.view.quest.slug.clone();
    app.say(format!("{what} {slug}: lands in {bead}"));
    Action::None
}

// -------------------------------------------------------------------- render

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let (list, panel) = split(area, app.detail && app.quests.selected_row().is_some());
    if let Some(list) = list {
        render_list(frame, list, app);
    }
    if let Some(panel) = panel {
        render_panel(frame, panel, app);
    }
}

/// With the panel up, a wide body is split and a narrow one is handed over
/// whole — a list squeezed into thirty columns shows nothing worth reading.
fn split(area: Rect, panel: bool) -> (Option<Rect>, Option<Rect>) {
    if !panel {
        return (Some(area), None);
    }
    if area.width < PANEL_SPLIT_COLS {
        return (None, Some(area));
    }
    let want = PANEL_COLS.min(area.width / 2);
    let [list, panel] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(want)]).areas(area);
    (Some(list), Some(panel))
}

fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    let state = &app.quests;
    let visible = state.visible();
    if visible.is_empty() {
        frame.render_widget(Paragraph::new(empty_lines(state)), inset(area));
        return;
    }

    let mode = app.row_mode();
    let width = area.width as usize;
    let capacity = (area.height as usize).max(1);

    // Group headers cost a line each, so the window is computed over rendered
    // lines rather than over rows.
    let mut lines: Vec<Line> = Vec::new();
    let mut group: Option<u8> = None;
    for (n, i) in visible.iter().enumerate().skip(state.offset) {
        if lines.len() >= capacity {
            break;
        }
        let row = &state.rows[*i];
        let rank = crate::commands::rank(&row.view);
        if group != Some(rank) {
            group = Some(rank);
            lines.push(Line::from(
                Span::raw(layout::truncate(group_title(rank), width)).dim(),
            ));
        }
        lines.extend(row_lines(
            row,
            state.links.get(*i).map(Vec::as_slice).unwrap_or(&[]),
            n == state.selected,
            mode,
            width,
        ));
    }
    lines.truncate(capacity);
    frame.render_widget(Paragraph::new(lines), area);
}

fn empty_lines(state: &State) -> Vec<Line<'static>> {
    let why = if !state.rows.is_empty() {
        "no quests match the filters"
    } else if state.show_finished {
        "no quests yet"
    } else {
        "no open quests"
    };
    vec![
        Line::from(Span::raw(why).bold()),
        Line::from(""),
        Line::from(Span::raw("n starts one · f shows finished · ? for keys").dim()),
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

fn group_title(rank: u8) -> &'static str {
    match rank {
        0 => "needs you",
        1 => "active",
        2 => "idle",
        _ => "finished",
    }
}

/// SPEC §17's row:
/// ```text
/// ▸ ⏸ cdc-backfill-retry     ws  3/7 ▓▓▓░░░░  master ctx 41%  2 sess  needs you  4m
///      make the CDC backfill idempotent · PR #4821 (CI ✓) · task Backfill CDC
/// ```
/// Three-line mode moves the right-hand facts onto a line of their own.
fn row_lines<'a>(
    row: &QuestRow,
    links: &[Link],
    selected: bool,
    mode: RowMode,
    width: usize,
) -> Vec<Line<'a>> {
    let head = format!(
        "{} {} {}",
        if selected { "▸" } else { " " },
        state_glyph(row),
        row.view.quest.slug
    );
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
    };

    let (facts, tail) = match mode {
        RowMode::Two => (String::new(), right_facts(row)),
        RowMode::Three => (right_facts(row), urgency(row)),
    };

    let mut out = vec![Line::from(Span::styled(pack(&head, &tail, width), style))];
    out.push(Line::from(Span::raw(layout::truncate(
        &format!("{INDENT}{}", goal_and_links(row, links)),
        width,
    ))));
    if !facts.is_empty() {
        out.push(Line::from(
            Span::raw(layout::truncate(&format!("{INDENT}{facts}"), width)).dim(),
        ));
    }
    out
}

/// `left` flush left and `right` flush right on one `width`-column line; the
/// left half gives way first, because the facts on the right are fixed-size
/// and a slug is not.
fn pack(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let rw = layout::width(right);
    if right.is_empty() || rw + 2 >= width {
        return layout::truncate(left, width);
    }
    let room = width - rw - 1;
    let left = layout::truncate(left, room);
    let pad = room - layout::width(&left) + 1;
    format!("{left}{}{right}", " ".repeat(pad))
}

/// The facts SPEC §17 puts at the right end of the first line.
fn right_facts(row: &QuestRow) -> String {
    let mut parts = vec![row.view.quest.machine.clone()];
    if let Some(p) = row.view.progress {
        parts.push(format!("{} {}", p.cell(), p.bar(BAR)));
    }
    if let Some(ctx) = row.view.master_ctx_pct {
        parts.push(format!("master ctx {ctx}%"));
    }
    let live = row.view.live_sessions;
    if live > 0 {
        parts.push(format!("{live} sess"));
    }
    let tail = urgency(row);
    if !tail.is_empty() {
        parts.push(tail);
    }
    parts.join("  ")
}

/// Split out because three-line mode keeps it on the first line while the
/// rest moves down: "needs you" and the age are why a row is looked at.
fn urgency(row: &QuestRow) -> String {
    let age = fmt::age(row.view.quest.updated_at);
    if row.view.needs_you {
        format!("needs you  {age}")
    } else {
        age
    }
}

/// SPEC §17's `⏸` for idle, with a filled dot for running and a tick for
/// done. `▸` is spent on the selection marker, so it is not reused here.
fn state_glyph(row: &QuestRow) -> &'static str {
    match row.view.display_state {
        DisplayState::Active => "●",
        DisplayState::Idle => "⏸",
        DisplayState::Finished => "✓",
    }
}

fn goal_and_links(row: &QuestRow, links: &[Link]) -> String {
    let mut parts: Vec<String> = Vec::new();
    match row.view.quest.goal.as_deref().map(str::trim) {
        Some(g) if !g.is_empty() => parts.push(fmt::oneline(g, 200)),
        _ => parts.push("no goal".to_string()),
    }
    parts.extend(links.iter().filter(|l| headline(l)).map(link_cell));
    parts.join(" · ")
}

/// The link kinds that earn a place on the row; the rest live in the panel.
fn headline(link: &Link) -> bool {
    matches!(link.kind.as_str(), "pr" | "task")
}

/// `PR #4821 (CI ✓)` / `task Backfill CDC`. Enrichment (`meta.ci`,
/// `meta.state`) is not written yet — when it is, this picks it up.
fn link_cell(link: &Link) -> String {
    let meta = |key: &str| {
        link.meta
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let title = link.title.as_deref().filter(|t| !t.is_empty());
    match link.kind.as_str() {
        "pr" => {
            let mut out = match pr_number(&link.r#ref) {
                Some(n) => format!("PR #{n}"),
                None => "PR".to_string(),
            };
            if let Some(state) = meta("state") {
                out.push_str(&format!(" {state}"));
            }
            if let Some(ci) = meta("ci") {
                out.push_str(&format!(" (CI {ci})"));
            }
            out
        }
        "task" => match title {
            Some(t) => format!("task {}", fmt::oneline(t, 40)),
            None => "task".to_string(),
        },
        kind => match title {
            Some(t) => format!("{kind} {}", fmt::oneline(t, 40)),
            None => format!("{kind} {}", fmt::oneline(&link.r#ref, 40)),
        },
    }
}

/// The number out of `…/pull/<n>`; `None` for anything else stored as a PR.
fn pr_number(reference: &str) -> Option<&str> {
    let (_, tail) = reference.rsplit_once("/pull/")?;
    let n = tail.split(['/', '?', '#']).next()?;
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then_some(n)
}

// --------------------------------------------------------------- detail panel

fn render_panel(frame: &mut Frame, area: Rect, app: &App) {
    let Some(row) = app.quests.selected_row() else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", row.view.quest.slug))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let lines = panel_lines(row, app.quests.selected_links(), &app.quests.events);
    let width = inner.width as usize;
    let shown: Vec<Line> = lines
        .into_iter()
        .take(inner.height as usize)
        .map(|l| clip(l, width))
        .collect();
    frame.render_widget(Paragraph::new(shown), inner);
}

fn clip(line: Line<'_>, width: usize) -> Line<'static> {
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let style = line.spans.first().map(|s| s.style).unwrap_or_default();
    Line::from(Span::styled(layout::truncate(&text, width), style))
}

/// SPEC §17: goal, sessions with status/phase/ctx, links, the last 10 events,
/// the beads breakdown.
fn panel_lines<'a>(row: &QuestRow, links: &[Link], events: &[Event]) -> Vec<Line<'a>> {
    let view = &row.view;
    let mut out: Vec<Line> = Vec::new();
    let head = |out: &mut Vec<Line>, title: &str| {
        out.push(Line::from(""));
        out.push(Line::from(Span::raw(title.to_string()).bold()));
    };

    out.push(Line::from(Span::raw(format!(
        "{}  {}",
        view.quest.id,
        view.state_cell()
    ))));
    out.push(Line::from(Span::raw(
        view.quest
            .goal
            .as_deref()
            .filter(|g| !g.trim().is_empty())
            .map(|g| fmt::oneline(g, 200))
            .unwrap_or_else(|| "no goal".to_string()),
    )));
    out.push(Line::from(
        Span::raw(format!(
            "{} · {} · {} ago",
            view.quest.machine,
            fmt::tilde(&view.quest.cwd),
            fmt::age(view.quest.updated_at)
        ))
        .dim(),
    ));

    head(&mut out, "beads");
    out.push(Line::from(Span::raw(beads_line(row))));

    head(&mut out, "sessions");
    let mut sessions: Vec<&Session> = row.sessions.iter().collect();
    sessions.sort_by_key(|s| {
        (
            s.status == SessionStatus::Ended,
            s.role != SessionRole::Master,
            s.started_at,
        )
    });
    if sessions.is_empty() {
        out.push(Line::from(Span::raw("none").dim()));
    } else {
        for s in sessions {
            out.push(Line::from(Span::raw(session_line(s))));
        }
    }

    head(&mut out, "links");
    if links.is_empty() {
        out.push(Line::from(Span::raw("none").dim()));
    } else {
        for l in links {
            out.push(Line::from(Span::raw(format!("{} {}", l.kind, l.r#ref))));
        }
    }

    head(&mut out, "events");
    if events.is_empty() {
        out.push(Line::from(Span::raw("none").dim()));
    } else {
        for e in events {
            out.push(Line::from(Span::raw(format!(
                "{:>4} {} {}",
                fmt::age(e.ts),
                e.kind,
                fmt::payload(e.payload.as_ref(), PAYLOAD_COLS)
            ))));
        }
    }
    out
}

/// `bd-42 · 3/7 closed · 2 open`, or why there is nothing to say.
fn beads_line(row: &QuestRow) -> String {
    let Some(epic) = row.view.quest.beads_epic.as_deref() else {
        return "no epic".to_string();
    };
    match row.view.progress {
        Some(p) => format!("{epic} · {}", p.summary()),
        None => format!("{epic} · progress unavailable"),
    }
}

fn session_line(s: &Session) -> String {
    let mut out = format!("{} {} {}", s.label, s.role, s.status);
    if let Some(waiting) = s.waiting_for.as_deref().filter(|w| !w.is_empty()) {
        out.push_str(&format!(" ({waiting})"));
    }
    if let Some(phase) = s.phase.as_deref().filter(|p| !p.is_empty()) {
        out.push_str(&format!(" · {phase}"));
    }
    if let Some(ctx) = s.ctx_pct {
        out.push_str(&format!(" · ctx {ctx}%"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::QuestView;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::{Quest, QuestState};
    use crate::tui::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // ------------------------------------------------------------- fixtures

    fn quest(slug: &str, state: QuestState, updated_at: i64) -> Quest {
        let mut q = Quest::new(slug, "/tmp/work", "laptop");
        q.state = state;
        q.updated_at = updated_at;
        q.goal = Some(format!("goal of {slug}"));
        q
    }

    fn session(quest_id: &str, role: SessionRole, status: SessionStatus) -> Session {
        let mut s = Session::new(quest_id, role, "master", "q-x", "%1");
        s.status = status;
        s
    }

    fn row(quest: Quest, sessions: Vec<Session>) -> QuestRow {
        QuestRow {
            view: QuestView::new(quest, &sessions),
            sessions,
        }
    }

    fn app_with(rows: Vec<QuestRow>) -> App {
        let mut app = App::new(&Config::default(), "laptop");
        app.set_size(120, 30);
        let links = vec![Vec::new(); rows.len()];
        app.quests.rows = rows;
        app.quests.links = links;
        crate::commands::sort_quests(&mut app.quests.rows);
        app.quests.resync();
        app
    }

    /// The three groups, one Quest each, deliberately out of order.
    fn grouped() -> App {
        let waiting = quest("needs-me", QuestState::Active, 10);
        let busy = quest("running", QuestState::Active, 20);
        let sleepy = quest("resting", QuestState::Active, 30);
        let done = quest("shipped", QuestState::Finished, 40);
        let w = vec![session(
            &waiting.id,
            SessionRole::Master,
            SessionStatus::Waiting,
        )];
        let b = vec![session(&busy.id, SessionRole::Master, SessionStatus::Busy)];
        let s = vec![session(
            &sleepy.id,
            SessionRole::Master,
            SessionStatus::Idle,
        )];
        app_with(vec![
            row(sleepy, s),
            row(done, Vec::new()),
            row(busy, b),
            row(waiting, w),
        ])
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

    fn line_of(lines: &[String], needle: &str) -> Option<usize> {
        lines.iter().position(|l| l.contains(needle))
    }

    // --------------------------------------------------------------- render

    #[test]
    fn rows_are_grouped_needs_you_then_active_then_idle() {
        let mut app = grouped();
        let lines = draw(&mut app, 120, 30);
        let text = lines.join("\n");
        for group in ["needs you", "active", "idle"] {
            assert!(text.contains(group), "missing group {group}\n{text}");
        }
        let (needs, active, idle) = (
            line_of(&lines, "needs-me").unwrap(),
            line_of(&lines, "running").unwrap(),
            line_of(&lines, "resting").unwrap(),
        );
        assert!(needs < active && active < idle, "{text}");
        // Their headers sit directly above them.
        assert!(lines[needs - 1].contains("needs you"), "{text}");
        assert!(lines[active - 1].trim() == "active", "{text}");
        assert!(lines[idle - 1].trim() == "idle", "{text}");
    }

    #[test]
    fn finished_quests_are_hidden_until_f() {
        let mut app = grouped();
        assert!(!screen(&mut app, 120, 30).contains("shipped"));
        app.quests.show_finished = true;
        app.quests.resync();
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("shipped"), "{text}");
        assert!(text.contains("finished"), "{text}");
    }

    #[test]
    fn f_toggles_the_flag_and_asks_for_a_reload() {
        let mut app = grouped();
        assert_eq!(handle(&mut app, Input::Char('f')), Action::Refresh);
        assert!(app.quests.show_finished);
        assert!(app.status.contains("showing finished"), "{}", app.status);
        assert_eq!(handle(&mut app, Input::Char('f')), Action::Refresh);
        assert!(!app.quests.show_finished);
    }

    #[test]
    fn an_empty_listing_says_so_rather_than_drawing_nothing() {
        let mut app = app_with(Vec::new());
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("no open quests"), "{text}");
        assert!(text.contains("n starts one"), "{text}");
        // Narrow, with the panel asked for: still no panic, still a message.
        app.detail = true;
        let text = screen(&mut app, 40, 12);
        assert!(text.contains("no open"), "{text}");
    }

    #[test]
    fn a_filtered_out_listing_says_the_filters_did_it() {
        let mut app = grouped();
        app.quests.query = "nothing-matches-this".to_string();
        app.quests.resync();
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("no quests match the filters"), "{text}");
    }

    #[test]
    fn a_quest_with_no_sessions_and_no_epic_still_renders() {
        let mut q = quest("bare", QuestState::Active, 5);
        q.goal = None;
        q.beads_epic = None;
        let mut app = app_with(vec![row(q, Vec::new())]);
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("bare"), "{text}");
        assert!(text.contains("no goal"), "{text}");
        // No sessions and no epic: neither fact is invented.
        assert!(!text.contains("sess"), "{text}");
        assert!(!text.contains("master ctx"), "{text}");
        app.detail = true;
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("no epic"), "{text}");
        assert!(text.contains("none"), "{text}");
    }

    #[test]
    fn both_row_modes_draw_every_line_of_the_selected_row() {
        let mut app = grouped();
        // Wide: two lines, the facts on the first.
        assert_eq!(app.row_mode(), RowMode::Two);
        let lines = draw(&mut app, 120, 30);
        let at = line_of(&lines, "needs-me").unwrap();
        assert!(lines[at].contains("laptop"), "{:?}", lines[at]);
        assert!(lines[at].contains("needs you"), "{:?}", lines[at]);
        assert!(lines[at + 1].contains("goal of needs-me"), "{:?}", lines);

        // Narrow: three lines, the facts on the third.
        let lines = draw(&mut app, 60, 30);
        assert_eq!(app.row_mode(), RowMode::Three);
        let at = line_of(&lines, "needs-me").unwrap();
        assert!(lines[at].contains("needs you"), "{:?}", lines[at]);
        assert!(lines[at + 1].contains("goal of"), "{:?}", lines[at + 1]);
        assert!(lines[at + 2].contains("laptop"), "{:?}", lines[at + 2]);
    }

    #[test]
    fn a_wide_or_combining_name_never_overflows_the_row() {
        // Asserted on the lines themselves rather than on the rendered buffer:
        // a `TestBackend` cell holds one column, so reading a wide glyph back
        // out of it and re-measuring counts its padding twice.
        let names = [
            "日本語のクエスト-とても長い名前です-ほんとうに",
            "cafe\u{301}-nai\u{308}ve-e\u{301}e\u{301}e\u{301}",
            &"x".repeat(200),
            "🚀-emoji-quest-🎯",
        ];
        for name in names {
            let mut q = quest(name, QuestState::Active, 9);
            q.goal = Some("a goal long enough to need cutting on every width".repeat(3));
            let mut r = row(q, Vec::new());
            r.view.progress = Some(crate::beads::Progress {
                closed: 3,
                total: 7,
                ..Default::default()
            });
            for width in [200usize, 120, 100, 80, 70, 60, 40, 20, 10, 3, 1, 0] {
                for mode in [RowMode::Two, RowMode::Three] {
                    for selected in [true, false] {
                        for line in row_lines(&r, &[], selected, mode, width) {
                            let text: String =
                                line.spans.iter().map(|s| s.content.as_ref()).collect();
                            assert!(
                                layout::width(&text) <= width,
                                "{name:?} at {width} ({mode:?}): {text:?} is {} columns",
                                layout::width(&text)
                            );
                        }
                    }
                }
            }
        }

        // And the renderer itself never panics on any of them, at any size.
        let rows: Vec<QuestRow> = names
            .iter()
            .map(|n| row(quest(n, QuestState::Active, 9), Vec::new()))
            .collect();
        let mut app = app_with(rows);
        for (w, h) in [
            (200, 40),
            (120, 30),
            (100, 20),
            (80, 20),
            (60, 20),
            (40, 12),
            (20, 8),
            (4, 3),
        ] {
            draw(&mut app, w, h);
            app.detail = !app.detail;
            draw(&mut app, w, h);
        }
    }

    #[test]
    fn the_beads_bar_and_ctx_reading_reach_the_row() {
        let mut q = quest("shipping", QuestState::Active, 5);
        q.beads_epic = Some("bd-42".to_string());
        let mut master = session(&q.id, SessionRole::Master, SessionStatus::Busy);
        master.ctx_pct = Some(41);
        master.ctx_updated_at = Some(100);
        let mut r = row(q, vec![master]);
        r.view.progress = Some(crate::beads::Progress {
            closed: 3,
            total: 7,
            open: 4,
            ..Default::default()
        });
        let mut app = app_with(vec![r]);
        let lines = draw(&mut app, 120, 30);
        let at = line_of(&lines, "shipping").unwrap();
        assert!(lines[at].contains("3/7 ▓▓▓░░░░"), "{:?}", lines[at]);
        assert!(lines[at].contains("master ctx 41%"), "{:?}", lines[at]);
        assert!(lines[at].contains("1 sess"), "{:?}", lines[at]);
    }

    #[test]
    fn the_detail_panel_shows_the_selected_quest() {
        let mut app = grouped();
        assert!(!app.detail);
        assert_eq!(handle(&mut app, Input::Enter), Action::None);
        assert!(app.detail);
        let text = screen(&mut app, 120, 30);
        for want in [
            "needs-me",
            "goal of needs-me",
            "sessions",
            "links",
            "events",
            "beads",
        ] {
            assert!(text.contains(want), "missing {want}\n{text}");
        }
        // Still shows the list beside it at this width.
        assert!(text.contains("running"), "{text}");
        // `o` is the same toggle.
        handle(&mut app, Input::Char('o'));
        assert!(!app.detail);
    }

    #[test]
    fn a_narrow_terminal_gives_the_panel_the_whole_body() {
        let mut app = grouped();
        app.detail = true;
        let text = screen(&mut app, 60, 24);
        assert!(text.contains("needs-me"), "{text}");
        assert!(
            !text.contains("running"),
            "the list should be hidden\n{text}"
        );
    }

    // --------------------------------------------------------------- keymap

    #[test]
    fn movement_keeps_the_selection_on_its_quest_across_a_reorder() {
        let mut app = grouped();
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "needs-me"
        );
        handle(&mut app, Input::Char('j'));
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "running"
        );
        // A reload that reorders must not move the selection to another Quest:
        // `running` goes from the middle to the end, the index does not.
        app.quests.rows.rotate_left(1);
        app.quests.resync();
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "running"
        );
        assert_ne!(app.quests.selected, 1);
        crate::commands::sort_quests(&mut app.quests.rows);
        app.quests.resync();
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "running"
        );
        // The ends clamp rather than wrap.
        handle(&mut app, Input::Char('G'));
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "resting"
        );
        handle(&mut app, Input::Down);
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "resting"
        );
        handle(&mut app, Input::Char('g'));
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "needs-me"
        );
        handle(&mut app, Input::Up);
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "needs-me"
        );
    }

    #[test]
    fn a_long_list_scrolls_only_as_far_as_the_selection_needs() {
        let rows: Vec<QuestRow> = (0..40)
            .map(|n| {
                row(
                    quest(&format!("quest-{n:02}"), QuestState::Active, 100 - n),
                    Vec::new(),
                )
            })
            .collect();
        let mut app = app_with(rows);
        app.set_size(120, 26);
        // The top of the list does not move while the selection is on screen.
        for _ in 0..3 {
            handle(&mut app, Input::Down);
        }
        assert_eq!(app.quests.offset, 0);
        let lines = draw(&mut app, 120, 26);
        assert!(line_of(&lines, "quest-00").is_some(), "{lines:?}");

        // Past the fold it scrolls, and the selection is always drawn.
        for n in 0..30 {
            handle(&mut app, Input::Down);
            let selected = app.quests.selected_row().unwrap().view.quest.slug.clone();
            let lines = draw(&mut app, 120, 26);
            assert!(
                line_of(&lines, &selected).is_some(),
                "step {n}: {selected} off screen\n{lines:#?}"
            );
        }
        // And back up again.
        for n in 0..33 {
            handle(&mut app, Input::Up);
            let selected = app.quests.selected_row().unwrap().view.quest.slug.clone();
            let lines = draw(&mut app, 120, 26);
            assert!(
                line_of(&lines, &selected).is_some(),
                "back {n}: {selected} off screen\n{lines:#?}"
            );
        }
        assert_eq!(app.quests.offset, 0);

        // A page jump and the ends behave the same way.
        for key in [
            Input::PageDown,
            Input::Char('G'),
            Input::PageUp,
            Input::Char('g'),
        ] {
            handle(&mut app, key);
            let selected = app.quests.selected_row().unwrap().view.quest.slug.clone();
            let lines = draw(&mut app, 120, 26);
            assert!(line_of(&lines, &selected).is_some(), "{key:?}\n{lines:#?}");
        }
    }

    #[test]
    fn search_captures_the_keyboard_so_typing_does_not_quit() {
        let mut app = grouped();
        assert_eq!(app.handle(Input::Char('/')), Action::None);
        assert!(app.quests.capturing());
        // `q` and `x` are the shell's keys — while typing they are text.
        for c in "run".chars() {
            app.handle(Input::Char(c));
        }
        app.handle(Input::Char('q'));
        assert!(!app.should_quit);
        assert_eq!(app.quests.query, "runq");
        app.handle(Input::Backspace);
        assert_eq!(app.quests.query, "run");
        assert_eq!(app.quests.visible().len(), 1);
        // The status bar is the box: the query, and how much it matched.
        assert!(app.status.starts_with("/run"), "{}", app.status);
        assert!(app.status.contains("1 matching"), "{}", app.status);
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("running"), "{text}");
        assert!(!text.contains("resting"), "{text}");
        // Enter keeps the query and hands the keyboard back.
        app.handle(Input::Enter);
        assert!(!app.quests.capturing());
        assert_eq!(app.quests.query, "run");
        // Esc from the list clears it.
        app.handle(Input::Esc);
        assert!(app.quests.query.is_empty());
        assert_eq!(app.quests.visible().len(), 3);
    }

    #[test]
    fn ctrl_c_still_quits_out_of_the_search_box() {
        let mut app = grouped();
        app.handle(Input::Char('/'));
        assert_eq!(app.handle(Input::Ctrl('c')), Action::Quit);
        assert!(app.should_quit);
    }

    #[test]
    fn esc_in_the_box_cancels_the_search_entirely() {
        let mut app = grouped();
        app.handle(Input::Char('/'));
        for c in "run".chars() {
            app.handle(Input::Char(c));
        }
        app.handle(Input::Esc);
        assert!(!app.quests.capturing());
        assert!(app.quests.query.is_empty());
    }

    #[test]
    fn m_cycles_the_machines_present_and_back_to_all() {
        let ws = {
            let mut q = quest("remote", QuestState::Active, 4);
            q.machine = "ws".to_string();
            q
        };
        let mut app = app_with(vec![
            row(quest("local", QuestState::Active, 5), Vec::new()),
            row(ws, Vec::new()),
        ]);
        handle(&mut app, Input::Char('m'));
        assert_eq!(app.quests.machine.as_deref(), Some("laptop"));
        assert_eq!(app.quests.visible().len(), 1);
        handle(&mut app, Input::Char('m'));
        assert_eq!(app.quests.machine.as_deref(), Some("ws"));
        assert!(screen(&mut app, 120, 20).contains("remote"));
        handle(&mut app, Input::Char('m'));
        assert_eq!(app.quests.machine, None);
        assert_eq!(app.quests.visible().len(), 2);
    }

    #[test]
    fn s_hands_the_selection_to_the_sessions_tab() {
        let mut app = grouped();
        let id = app.quests.selected_row().unwrap().view.quest.id.clone();
        assert_eq!(handle(&mut app, Input::Char('s')), Action::Refresh);
        assert_eq!(app.tab, Tab::Sessions);
        assert_eq!(app.focus_quest.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn the_unbuilt_keys_name_their_bead_and_their_selection() {
        let mut app = grouped();
        for (key, want) in [
            ('n', "bd-8lz.4.4"),
            ('r', "bd-8lz.4.4"),
            ('c', "bd-8lz.4.4"),
            ('R', "bd-8lz.4.4"),
            ('b', "bd-8lz.4.3"),
        ] {
            assert_eq!(handle(&mut app, Input::Char(key)), Action::None);
            assert!(app.status.contains(want), "{key}: {}", app.status);
            assert!(!app.should_quit);
        }
        // The ones that act on a Quest say which.
        handle(&mut app, Input::Char('r'));
        assert!(app.status.contains("needs-me"), "{}", app.status);
    }

    #[test]
    fn l_opens_the_panel_and_summarises_the_links() {
        let mut app = grouped();
        app.quests.links[0] = vec![
            {
                let mut l = Link::new("q", "pr", "https://github.com/acme/api/pull/4821");
                l.meta = Some(serde_json::json!({ "ci": "✓" }));
                l
            },
            {
                let mut l = Link::new("q", "task", "https://app.productive.io/1/task/9");
                l.title = Some("Backfill CDC".to_string());
                l
            },
        ];
        app.quests.resync();
        assert_eq!(handle(&mut app, Input::Char('l')), Action::None);
        assert!(app.detail);
        assert!(app.status.contains("PR #4821 (CI ✓)"), "{}", app.status);
        assert!(app.status.contains("task Backfill CDC"), "{}", app.status);
        // And the row's second line carries them too. The panel is closed
        // first: its border title is the slug, and would be found instead.
        app.detail = false;
        let lines = draw(&mut app, 140, 30);
        let at = line_of(&lines, "needs-me").unwrap();
        assert!(lines[at + 1].contains("PR #4821"), "{:?}", lines[at + 1]);
        assert!(
            lines[at + 1].contains("task Backfill CDC"),
            "{:?}",
            lines[at + 1]
        );
    }

    #[test]
    fn a_status_message_survives_a_successful_tick() {
        let mut app = grouped();
        handle(&mut app, Input::Char('n'));
        assert!(!app.status.is_empty());
        crate::tui::report_refresh(&mut app, Ok(()));
        assert!(
            app.status.contains("bd-8lz.4.4"),
            "a tick wiped the message: {:?}",
            app.status
        );
        assert!(app.refresh_error.is_none());
        // A failed reload shows instead, and outranks the message.
        crate::tui::report_refresh(&mut app, Err(anyhow::anyhow!("database is locked")));
        let text = screen(&mut app, 120, 30);
        assert!(
            text.contains("refresh failed: database is locked"),
            "{text}"
        );
        crate::tui::report_refresh(&mut app, Ok(()));
        let text = screen(&mut app, 120, 30);
        assert!(!text.contains("refresh failed"), "{text}");
        assert!(text.contains("bd-8lz.4.4"), "{text}");
    }

    #[test]
    fn the_help_overlay_lists_the_tabs_own_keys() {
        let mut app = grouped();
        app.help = true;
        let text = screen(&mut app, 120, 40);
        assert!(text.contains("toggle the detail panel"), "{text}");
        assert!(text.contains("cycle the machine filter"), "{text}");
        // The shell's keys are still there.
        assert!(text.contains("next / previous tab"), "{text}");
    }

    // ------------------------------------------------------ loading, end to end

    /// A tmux that reports exactly the panes a test seeded, so `load_quests`'
    /// liveness sweep does not end every session for want of a real server.
    fn tmux_with(panes: &[(&str, &str)]) -> (tempfile::TempDir, Box<dyn crate::tmux::Tmux>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tmux.json");
        let state = crate::tmux::FixtureState {
            next_pane: panes.len() as u32 + 1,
            panes: panes
                .iter()
                .enumerate()
                .map(|(i, (session, id))| crate::tmux::FixturePane {
                    pane_id: (*id).to_string(),
                    pane_pid: 1000 + i as i32,
                    session_name: (*session).to_string(),
                    window_name: "master".to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        std::fs::write(&path, serde_json::to_string(&state).unwrap()).unwrap();
        (dir, Box::new(crate::tmux::FixtureTmux::new(path)))
    }

    #[test]
    fn refresh_loads_the_listing_out_of_the_database() {
        let db = Db::open_in_memory().unwrap();
        let mut waiting = Quest::new("needs-me", "/tmp/work", "laptop");
        waiting.goal = Some("unblock me".to_string());
        let waiting = db.insert_quest(&waiting).unwrap();
        let mut s = Session::new(&waiting.id, SessionRole::Master, "master", "q-x", "%1");
        s.status = SessionStatus::Waiting;
        s.ctx_pct = Some(41);
        db.insert_session(&s).unwrap();
        db.insert_link(&Link::new(
            &waiting.id,
            "pr",
            "https://github.com/acme/api/pull/4821",
        ))
        .unwrap();
        db.append_event(&waiting.id, None, "quest.created", &serde_json::json!({}))
            .unwrap();

        let mut idle = Quest::new("resting", "/tmp/work", "laptop");
        idle.goal = Some("later".to_string());
        db.insert_quest(&idle).unwrap();

        let (_tmux_dir, tmux) = tmux_with(&[("q-x", "%1")]);
        let ctx = Ctx::for_tests(Config::default(), db, tmux);
        let mut app = App::new(&ctx.config, "laptop");
        app.set_size(120, 30);
        refresh(&ctx, &mut app).unwrap();

        assert_eq!(app.quests.rows.len(), 2);
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "needs-me"
        );
        assert_eq!(app.quests.events.len(), 1);
        let lines = draw(&mut app, 120, 30);
        let text = lines.join("\n");
        assert!(text.contains("needs you"), "{text}");
        assert!(text.contains("master ctx 41%"), "{text}");
        assert!(text.contains("PR #4821"), "{text}");
        assert!(
            line_of(&lines, "needs-me") < line_of(&lines, "resting"),
            "{text}"
        );
    }

    #[test]
    fn a_link_cell_reads_the_reference_and_any_enrichment() {
        let pr = |r: &str| Link::new("q", "pr", r);
        assert_eq!(link_cell(&pr("https://github.com/a/b/pull/7")), "PR #7");
        assert_eq!(link_cell(&pr("git@x/no-number")), "PR");
        let mut enriched = pr("https://github.com/a/b/pull/7");
        enriched.meta = Some(serde_json::json!({ "state": "open", "ci": "✓" }));
        assert_eq!(link_cell(&enriched), "PR #7 open (CI ✓)");
        assert_eq!(pr_number("https://github.com/a/b/pull/7?x=1"), Some("7"));
        assert_eq!(pr_number("https://github.com/a/b/pull/"), None);
    }
}
