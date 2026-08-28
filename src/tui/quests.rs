//! Quests tab (SPEC §17): grouped two-line rows, the beads bar, the master's
//! context reading, the detail panel behind `Enter` and the attach behind
//! `o`.
//!
//! The listing itself is not computed here — [`crate::commands::load_quests`]
//! is the one definition of "the Quest listing", shared with `q list`, so the
//! CLI and the TUI can never disagree about what exists or in what order.
//! This module only turns those rows into lines.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph};

use crate::Ctx;
use crate::commands::{
    QuestRow, close, fill_progress, fmt, load_quests, new, proxy, rename, resume, tpl,
};
use crate::error::QError;
use crate::model::{
    DisplayState, Event, Link, NameSource, Quest, QuestState, Session, SessionRole, SessionStatus,
    Template,
};

use super::app::{Action, App, Prompt, Tab, Target};
use super::form::Form;
use super::keys::Input;
use super::layout::{self, RowMode};

/// The tab's own half of the `?` overlay.
pub const HELP: &[(&str, &str)] = &[
    ("o", "enter the master (attach to its tmux session)"),
    ("Enter", "toggle the detail panel"),
    ("s", "this Quest's sessions"),
    ("e", "this Quest's events, in the tail"),
    ("n", "new Quest (a form)"),
    ("r / c / R", "rename · close · resume (each prompts)"),
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
/// Payload text kept in an event line.
const PAYLOAD_COLS: usize = 40;
/// How far the second line is indented under the glyphs.
const INDENT: &str = "    ";

// The form field labels. Constants because the openers write them and
// [`submit`] reads them back; a typo in either would silently mean "blank".
const F_NAME: &str = "name";
const F_GOAL: &str = "goal";
const F_DIR: &str = "dir";
const F_WORKFLOW: &str = "workflow";
const F_MACHINE: &str = "machine";
const F_TEMPLATE: &str = "template";
const F_BEADS: &str = "beads epic";
const F_SLUG: &str = "slug";
const F_CLOSE_EPIC: &str = "close beads epic";
const F_PROMPT: &str = "prompt";
/// The template select's "no template" option. Parenthesised, so no template
/// name can collide with it: a name is a slug (`^[a-z0-9]+(-[a-z0-9]+)*$`).
const NO_TEMPLATE: &str = "(none)";
/// The new-Quest form's "leave it unset" workflow choice — a Quest may have
/// none, and a template's own workflow fills the blank when one is picked.
const NO_WORKFLOW: &str = "(none)";

/// Per-tab state, owned by `App`.
#[derive(Default)]
pub struct State {
    /// The listing as last loaded: already swept, machine-filtered and ranked,
    /// with the last remote round folded in.
    rows: Vec<QuestRow>,
    /// The rows the last remote round brought back (SPEC §15), kept apart from
    /// `rows` because the two arrive on different clocks: `[ui] tick_local`
    /// reloads the database every 2 s, `[ui] tick_remote` asks the machines
    /// every 10 s. A local reload re-merges this snapshot rather than waiting
    /// for ssh, so a machine that is down never slows a tick down.
    pub(super) remote: Vec<QuestRow>,
    /// Links per row, index-aligned with `rows`.
    links: Vec<Vec<Link>>,
    /// The selected Quest's most recent events; only the selection needs them.
    events: Vec<Event>,
    /// Which Quest [`State::events`] was read for. Selection keys move faster
    /// than any reload, so the pairing is carried explicitly rather than
    /// assumed: the panel shows nothing before it shows the wrong Quest's.
    events_for: Option<Anchor>,
    /// Index into the *visible* rows.
    selected: usize,
    /// The selected Quest, so a reload that reorders keeps the selection on the
    /// same Quest rather than on the same line.
    selected_id: Option<Anchor>,
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
    /// Every template, for the new-Quest form's select (SPEC §11, §17).
    /// Loaded by `refresh` like everything else this tab draws.
    templates: Vec<Template>,
    /// Every workflow name, for the new-Quest form's select (SPEC §11).
    ///
    /// A select rather than a text field: the set is small, closed and known,
    /// and `q new --workflow` refuses a name that is not in it — so free text
    /// here could only ever produce a form that fails on submit. The Templates
    /// tab's workflow field stays free text on purpose; see
    /// `crate::tui::templates`.
    workflows: Vec<String>,
}

/// Which Quest the selection (or a loaded set of events) is on.
///
/// Not the id alone: a Quest id is 16 bits and unique only per **machine**, so
/// with a remote machine in the listing two rows can carry the same one. An
/// anchor that was just an id re-attaches to whichever row matches first —
/// always a local one, because the merge appends the remotes — so the
/// highlight jumps off the remote row on the next tick, and every key the
/// `LOCAL_ONLY` guard exists to gate then acts on the wrong Quest.
///
/// `remote` is part of it as well as the machine name, because a local Quest
/// can carry a remote's name in its `machine` column (`q new --machine ws`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Anchor {
    remote: bool,
    machine: String,
    id: String,
}

impl Anchor {
    fn of(row: &QuestRow) -> Anchor {
        Anchor {
            remote: row.origin.is_remote(),
            machine: row.view.quest.machine.clone(),
            id: row.view.quest.id.clone(),
        }
    }

    /// A Quest in this machine's own database — what `n`, `r` and `R` hand
    /// back.
    fn local(quest: &Quest) -> Anchor {
        Anchor {
            remote: false,
            machine: quest.machine.clone(),
            id: quest.id.clone(),
        }
    }
}

impl State {
    /// The workflow names the new-Quest form offers (SPEC §11).
    ///
    /// A directory that cannot be read falls back to the built-in names and
    /// keeps the last good list out of it: a tick must not be able to fail, and
    /// a form that silently offered *nothing* would be worse than one that
    /// offers the five that are compiled in and always exist.
    fn workflows_or_builtins(&self, ctx: &Ctx) -> Vec<String> {
        ctx.workflows().names().unwrap_or_else(|_| {
            crate::workflows::BUILTIN
                .iter()
                .map(|(name, _)| (*name).to_string())
                .collect()
        })
    }

    /// Whether `/` has the keyboard. The shell asks before claiming its own
    /// bare-letter keys, so typing `q` into the box does not quit.
    pub fn capturing(&self) -> bool {
        self.searching
    }

    /// The listing as last loaded, for the shell's own tests.
    #[cfg(test)]
    pub(super) fn loaded(&self) -> &[QuestRow] {
        &self.rows
    }

    /// Give the keyboard back and drop the half-typed query. Leaving the tab
    /// is the one way out of the box that is not Esc or Enter, and an armed
    /// capture behind another tab is invisible: every bare letter would be
    /// swallowed as text on return, with no box on screen to explain it.
    pub fn cancel_capture(&mut self) {
        if self.searching {
            self.searching = false;
            self.query.clear();
            // `selected` indexes the *visible* rows: dropping the query widens
            // them under it, so the selection has to be re-anchored on its own
            // Quest or the next frame settles it onto whichever Quest now
            // happens to sit at that index.
            self.resync();
        }
    }

    /// The filters currently hiding (or revealing) rows, for the chrome line.
    /// A committed `/` or `m` has no other trace on screen, and a transient
    /// status message is not a mode indicator — the next one erases it.
    pub fn filters(&self) -> String {
        let mut on: Vec<String> = Vec::new();
        // While the box is open it *is* the indicator, in the status bar, with
        // a cursor and a match count; a second copy of the query would only
        // repeat it.
        if !self.query.is_empty() && !self.searching {
            on.push(format!("/{}", self.query));
        }
        if let Some(m) = self.machine.as_deref() {
            on.push(format!("m {m}"));
        }
        if self.show_finished {
            on.push("+finished".to_string());
        }
        on.join(" ")
    }

    /// The selected Quest's events — empty unless they were read *for* that
    /// Quest, so a selection the reload has not caught up with shows nothing
    /// rather than the previous Quest's history under the new one's title.
    fn selected_events(&self) -> &[Event] {
        match (self.events_for.as_ref(), self.selected_id.as_ref()) {
            (Some(loaded), Some(selected)) if loaded == selected => &self.events,
            _ => &[],
        }
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
        if let Some(anchor) = self.selected_id.as_ref()
            && let Some(at) = visible
                .iter()
                .position(|i| Anchor::of(&self.rows[*i]) == *anchor)
        {
            self.selected = at;
        } else {
            self.selected = self.selected.min(visible.len() - 1);
        }
        self.selected_id = Some(Anchor::of(&self.rows[visible[self.selected]]));
        self.offset = self.offset.min(self.selected).min(visible.len() - 1);
    }

    /// Put the selection on a specific Quest — after creating or resuming one,
    /// where "the row that is there now" is not what the user means. The next
    /// reload's `resync` finds it by id wherever it has been ranked.
    ///
    /// Deliberately *not* `resync`: a Quest that was just created is not in
    /// `rows` yet, and `resync` answers "that id is not here" by overwriting
    /// `selected_id` with whatever sits at the clamped index — which would
    /// throw the anchor away one line after it was set.
    fn focus_on(&mut self, anchor: Anchor) {
        let visible = self.visible();
        if let Some(at) = visible
            .iter()
            .position(|i| Anchor::of(&self.rows[*i]) == anchor)
        {
            self.selected = at;
        }
        self.selected_id = Some(anchor);
    }

    /// [`State::focus_on`] by id, for tests that have one in hand: the row is
    /// already loaded, so its machine comes from the row itself.
    #[cfg(test)]
    fn focus_on_id(&mut self, id: &str) {
        let anchor = self
            .rows
            .iter()
            .find(|r| r.view.quest.id == id)
            .map(Anchor::of)
            .unwrap_or_else(|| panic!("no row with id `{id}`"));
        self.focus_on(anchor);
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
            .map(|i| Anchor::of(&self.rows[*i]));
        let viewport = viewport.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
        // Both branches above only ever push `offset` FORWARD, so a viewport
        // that GREW since the last frame would leave the body half empty with
        // rows stranded above the fold. Pulling back to the last full screen
        // is what heals it. `viewport` reserves a header line for every group
        // the LISTING has, and the window starting at `offset` may span fewer
        // of them, so the clamp is a LOWER bound on what the body could hold:
        // it never pushes a row off the bottom, and what it can still leave is
        // the headers of the groups the window does not reach.
        self.offset = self.offset.min(visible.len().saturating_sub(viewport));
    }
}

// ------------------------------------------------------------------ loading

/// Reload this tab's data. Called by the event loop on tick and on `x`, never
/// from the state machine, so `App::handle` stays pure.
pub fn refresh(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    // `f` decides whether finished Quests are even fetched; the filter in
    // `visible` then only has the machine and the query left to do.
    let mut rows = load_quests(ctx, app.quests.show_finished)?;
    fill_progress(ctx, &mut rows);
    // The remote rows are the last round's, whatever its age: re-fetching here
    // would put a 5 s deadline on a 2 s tick. `f` is not forwarded either —
    // the round asks for `--all` and `visible` hides what the toggle hides, so
    // a keypress never waits for a machine to answer.
    rows.extend(app.quests.remote.iter().cloned());
    crate::commands::sort_quests(&mut rows);

    let db = ctx.db()?;
    let mut links = Vec::with_capacity(rows.len());
    for row in &rows {
        // Links live in the database of the machine that owns the Quest
        // (SPEC §15: no sync), and ids are not unique across machines — a
        // lookup by a remote id would show some local Quest's links under it.
        links.push(if row.origin.is_remote() {
            Vec::new()
        } else {
            db.list_links_by_quest(&row.view.quest.id)?
        });
    }

    let templates = db.list_templates()?;
    // Never fatal: a workflows directory that cannot be read must not stop the
    // Quests tab from drawing. The built-ins are always there, and `q workflow
    // list` is where the real error is reported.
    let workflows = app.quests.workflows_or_builtins(ctx);

    app.quests.rows = rows;
    app.quests.links = links;
    app.quests.templates = templates;
    app.quests.workflows = workflows;
    app.quests.resync();
    // A reload can reorder the list under the selection, so the viewport is
    // re-settled rather than only clamped: `resync` alone can scroll up but
    // never down, which leaves the highlight below the fold.
    settle_view(app);

    // The events are re-read even for an unchanged selection: this is the
    // reload, and the tail is exactly what has changed since the last one.
    // Cleared as well as unpaired, so an emptied listing (no selection left to
    // read for) drops them rather than keeping the last Quest's.
    app.quests.events.clear();
    app.quests.events_for = None;
    sync(ctx, app)
}

/// Bring the selection's own data in line, without the full reload a tick
/// does. Runs before every redraw and is a no-op unless the selection moved,
/// which is what keeps `j`/`k` off the tmux sweep and off `bd`.
pub fn sync(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    if app.quests.events_for == app.quests.selected_id {
        return Ok(());
    }
    // Only the selection's events are read: the panel shows one Quest's. A
    // remote Quest's are in that machine's database, and its id means nothing
    // in this one, so it is not looked up at all (bd-8lz.5.3 proxies).
    let remote = app
        .quests
        .selected_row()
        .is_some_and(|row| row.origin.is_remote());
    app.quests.events = match app.quests.selected_id.as_ref() {
        Some(anchor) if !remote => ctx.db()?.list_events_by_quest(&anchor.id, EVENTS)?,
        _ => Vec::new(),
    };
    app.quests.events_for = app.quests.selected_id.clone();
    Ok(())
}

/// Put a listing straight into the tab, with no database behind it — for the
/// shell's own tests, which drive `attach` against rows a remote sent.
/// Finished rows are shown, so a test can select one.
#[cfg(test)]
pub(super) fn seed(app: &mut App, rows: Vec<QuestRow>) {
    app.quests.links = vec![Vec::new(); rows.len()];
    app.quests.remote = rows
        .iter()
        .filter(|r| r.origin.is_remote())
        .cloned()
        .collect();
    app.quests.rows = rows;
    app.quests.show_finished = true;
    app.quests.resync();
}

/// Scroll the selection back into view for the current terminal size — after a
/// reload that reordered the rows, and after a resize.
pub fn settle_view(app: &mut App) {
    let page = viewport(app);
    app.quests.settle(page);
}

// ------------------------------------------------------------------- keymap

/// Keys the shell did not claim. Pure: anything needing the terminal leaves
/// through an `Action`, never from in here.
pub fn handle(app: &mut App, input: Input) -> Action {
    if app.quests.searching {
        return search_key(app, input);
    }
    if let Input::Char(c) = input
        && LOCAL_ONLY.contains(&c)
        && refuse_remote(app, c)
    {
        return Action::None;
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
        // SPEC §17 contradicts itself: `Enter` toggles the detail panel two
        // lines above, and `⏎/o` is "enter master (attach)" below. Reading it
        // the other way — both keys attach — would leave the detail panel, an
        // explicitly specified feature, with no binding at all, so this is the
        // only self-consistent split. `o` attaches; `Enter`, the key that gets
        // pressed by accident, only opens the panel rather than taking over
        // the terminal. (Nothing to do with phone keyboards: §17 gives `Ctrl-J`
        // as the Enter alias precisely for clients where Enter never arrives,
        // so either reading is reachable there.)
        Input::Char('o') => attach_selection(app),
        Input::Enter => toggle_detail(app),
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
        Input::Char('e') => events_of_selection(app),
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
        Input::Char('n') => open_new(app),
        Input::Char('r') => open_rename(app),
        Input::Char('c') => open_close(app),
        Input::Char('R') => open_resume(app),
        Input::Char('b') => brief_selection(app),
        Input::Char('l') => show_links(app),
        _ => Action::None,
    }
}

/// The keys that read or write *this* machine's database: sessions, events,
/// brief, links, rename, close, resume. SPEC §15 keeps every machine's
/// registry, hooks and database to itself, so on a remote row they would all
/// run against the wrong one — and not harmlessly: a Quest id is 16 bits and
/// unique only per machine, so `c` on a remote row could close a local Quest
/// that happens to share it.
///
/// bd-8lz.5.3 taught the **CLI** to proxy every one of these over ssh; the TUI
/// is not wired to that path yet, because each of these keys opens a form, a
/// pager or a confirm that reads and writes local rows, and routing them
/// through the proxy is a screen's worth of work per key rather than a shared
/// gate. So they stay refused here — and the refusal names the command that
/// does work, instead of claiming the operation needs that machine when a
/// shell on this one can now do it.
///
/// `o` is deliberately not in here: entering a remote Quest is SPEC §15's one
/// remote action, and it goes over ssh rather than through the database.
const LOCAL_ONLY: [char; 7] = ['s', 'e', 'r', 'c', 'R', 'b', 'l'];

/// The `q` command each refused key stands for, so the message hands over
/// something to run rather than a dead end.
fn cli_equivalent(key: char) -> &'static str {
    match key {
        's' => "q sessions",
        'e' => "q events",
        'r' => "q rename",
        'c' => "q close",
        'R' => "q resume",
        'b' => "q brief",
        'l' => "q links",
        _ => "q show",
    }
}

/// Whether `key` was refused because the selection lives on another machine.
fn refuse_remote(app: &mut App, key: char) -> bool {
    let Some(row) = app.quests.selected_row() else {
        return false;
    };
    if !row.origin.is_remote() {
        return false;
    }
    let (slug, machine) = (row.view.quest.slug.clone(), row.view.quest.machine.clone());
    app.say(format!(
        "{slug} runs on {machine}; the TUI does not proxy `{key}` yet \u{b7} \
         run `{} {slug}` in a shell, or o to enter",
        cli_equivalent(key)
    ));
    true
}

/// The machine a remote selection lives on, or `None` when the selection is
/// this machine's (or there is none).
pub fn selected_remote(app: &App) -> Option<String> {
    let row = app.quests.selected_row()?;
    row.origin
        .is_remote()
        .then(|| row.view.quest.machine.clone())
}

/// How many rows the body can show. The group headers cost a line each, so
/// the listing's *own* headers are reserved rather than all [`GROUPS`] of
/// them: a listing with nothing finished pays three headers, not four, and
/// reserving the fourth costs a whole row of a two-line listing.
fn viewport(app: &App) -> usize {
    let body = app.height.saturating_sub(2) as usize;
    let state = &app.quests;
    let mut seen = [false; GROUPS];
    for i in state.visible() {
        seen[crate::commands::rank(&state.rows[i].view) as usize] = true;
    }
    let headers = seen.iter().filter(|s| **s).count();
    (body.saturating_sub(headers) / app.row_mode().lines() as usize).max(1)
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

/// A paste while `/` holds the keyboard: text into the query, and nothing
/// else. Ignored when the box is not open — a paste is not a way to start one.
pub(super) fn paste(app: &mut App, text: &str) -> bool {
    if !app.quests.searching {
        return false;
    }
    let clean: String = text.chars().filter(|c| !c.is_control()).collect();
    if clean.is_empty() {
        return false;
    }
    app.quests.query.push_str(&clean);
    app.quests.resync();
    typing(app);
    true
}

/// The box *is* the status bar: there is no room for a second one, and the
/// filtered list above it is the other half of the feedback.
fn typing(app: &mut App) {
    let query = app.quests.query.clone();
    let matched = app.quests.visible().len();
    app.say(format!("/{query}\u{2588}  {matched} matching"));
}

/// The Quest an attach or a brief would run against, cloned so the borrow of
/// `app` ends with the lookup.
pub fn selected_quest(app: &App) -> Option<Quest> {
    app.quests.selected_row().map(|r| r.view.quest.clone())
}

/// `o` — the loop does the attaching; `handle` only says which key was hit.
fn attach_selection(app: &mut App) -> Action {
    match app.quests.selected_row() {
        Some(_) => Action::Attach,
        None => Action::None,
    }
}

/// `b` — same shape: the pager is the loop's business (SPEC §17).
fn brief_selection(app: &mut App) -> Action {
    match app.quests.selected_row() {
        Some(_) => Action::Brief,
        None => Action::None,
    }
}

fn toggle_detail(app: &mut App) -> Action {
    if app.quests.selected_row().is_none() {
        return Action::None;
    }
    app.detail = !app.detail;
    Action::None
}

/// `s` — hand the selection to the Sessions tab, which reads it out of
/// `App::focus_quest` on its next reload.
fn sessions_of_selection(app: &mut App) -> Action {
    hand_over(app, Tab::Sessions, "sessions")
}

/// `e` — the same hand-off to the Events tab (SPEC §17: "filter po questu").
fn events_of_selection(app: &mut App) -> Action {
    hand_over(app, Tab::Events, "events")
}

/// Both hand-offs, which differ only in where they land: the selected Quest's
/// id goes into `App::focus_quest` and the target tab consumes it in `refresh`.
///
/// The `Action::Refresh` is what makes it a hand-off rather than a tab switch:
/// the target tab has to reload before it can honour the filter, and until it
/// does it is still showing the previous listing.
fn hand_over(app: &mut App, tab: Tab, what: &str) -> Action {
    let Some(row) = app.quests.selected_row() else {
        return Action::None;
    };
    let (id, slug) = (row.view.quest.id.clone(), row.view.quest.slug.clone());
    app.focus_quest = Some(id);
    // Through `select`, not by assignment: it is what tears a capture down, and
    // an armed capture behind an inactive tab is invisible.
    app.select(tab);
    app.say(format!("{what} of {slug}"));
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

// --------------------------------------------------------------- the forms
//
// Building a form is pure — the data it offers (`machines`, `templates`) was
// loaded by `refresh`. Running one is `submit`, below, which the event loop
// calls: `q new` starts tmux and Claude, and that is loop work.

/// `n` — the new-Quest form of SPEC §17: name / goal / dir / workflow /
/// machine / template, plus the beads epic §5 step 2 creates by default.
fn open_new(app: &mut App) -> Action {
    let machines = app.machines.clone();
    let workflows: Vec<String> = std::iter::once(NO_WORKFLOW.to_string())
        .chain(app.quests.workflows.iter().cloned())
        .collect();
    let templates: Vec<String> = std::iter::once(NO_TEMPLATE.to_string())
        .chain(app.quests.templates.iter().map(|t| t.name.clone()))
        .collect();
    let has_templates = templates.len() > 1;
    let mut form = Form::new("new quest")
        .hint("Tab field \u{b7} \u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .text(F_NAME, "", "(auto)")
        .text(F_GOAL, "", "(none)")
        .text(F_DIR, "", "(where q was started)")
        .select(F_WORKFLOW, workflows, 0)
        .select(F_MACHINE, machines, 0)
        .select(F_TEMPLATE, templates, 0)
        .toggle(F_BEADS, true);
    if has_templates {
        // Every field a template supplies, including the two that are not
        // cosmetic: the label its epic carries, and the master's first prompt.
        form = form.note(
            "a template fills goal · dir · workflow left blank, and brings its \
             beads repo and the master's first prompt",
        );
    }
    // Last, so typing still lands in the first field. `n` starts a tmux
    // session and a Claude inside it; that is not something a stray Enter — or
    // a newline in a pasted goal — gets to do.
    app.open(Prompt::NewQuest, form.action("create"));
    Action::None
}

/// What a prompt records about the Quest it was opened against (see
/// [`Target`]): its identity, and what the box was about to tell the user.
fn target_of(row: &QuestRow) -> Target {
    Target {
        quest: row.view.quest.id.clone(),
        slug: row.view.quest.slug.clone(),
        created_at: row.view.quest.created_at,
        finished: row.view.display_state == DisplayState::Finished,
        epic: row.view.quest.beads_epic.clone(),
    }
}

/// `r` — SPEC §10: a manual slug, which also renames the tmux session and
/// tells every idle Claude session its new name.
fn open_rename(app: &mut App) -> Action {
    let Some(row) = app.quests.selected_row() else {
        return Action::None;
    };
    let target = target_of(row);
    // Declared harmless rather than merely left without an action row: a
    // rename destroys nothing and starts nothing, and a bare Enter re-submits
    // the slug the Quest already has, which is a no-op. Without the claim the
    // form would refuse to submit at all (N-2).
    let form = Form::new(format!("rename {}", target.slug))
        .harmless()
        .hint("\u{23ce} renames \u{b7} Esc cancels")
        .text(F_SLUG, &target.slug, "")
        .note("lowercase kebab-case, at most 40 characters");
    app.open(Prompt::Rename(target), form);
    Action::None
}

/// `c` — SPEC §5's confirmation, with `--close-epic` as its one live option.
///
/// `--summarize` (the brain summary §5 also offers) is not offered: there is
/// no implementation of it anywhere — `close.rs` still carries the `TODO(M2)`
/// — and a toggle that quietly does nothing is worse than a note saying so.
fn open_close(app: &mut App) -> Action {
    let Some(row) = app.quests.selected_row() else {
        return Action::None;
    };
    let target = target_of(row);
    let epic = row.view.quest.beads_epic.clone();
    let live = row.view.live_sessions;
    let tmux = format!("{}{}", app.tmux_prefix, target.slug);

    // The action row is first, and starts on `cancel`. This box stands in for
    // `q close`'s `[y/N]`, which reads a bare Enter as *abort*; there is
    // nothing to type here, so the affirmative is one arrow key away and a
    // keystroke that merely arrived is never it.
    let mut form = Form::new(format!("close {}?", target.slug))
        .hint("\u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .action("close");
    form = if target.finished {
        form.note("already finished — only the epic is left to do".to_string())
    } else {
        form.note(format!("kills tmux {tmux} · ends {live} live session(s)"))
    };
    form = match epic {
        Some(epic) => form
            .toggle(F_CLOSE_EPIC, false)
            .note(format!("epic {epic}")),
        None => form.note("no beads epic"),
    };
    app.open(
        Prompt::Close(target),
        form.note("brain summary (--summarize) lands with brain integration"),
    );
    Action::None
}

/// `R` — SPEC §5: a fresh master from the brief, or from a prompt given here.
fn open_resume(app: &mut App) -> Action {
    let Some(row) = app.quests.selected_row() else {
        return Action::None;
    };
    let target = target_of(row);
    let form = Form::new(format!("resume {}", target.slug))
        .hint("Tab field \u{b7} \u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .text(F_PROMPT, "", "(none — the master comes up on its brief)")
        .note("spawns a new master; the old session rows stay as history")
        // After the field, so the prompt can still be typed straight away.
        .action("resume");
    app.open(Prompt::Resume(target), form);
    Action::None
}

// ------------------------------------------------------------- running them

/// Run the open form. Called by the event loop, never from `handle`: every one
/// of these spawns tmux or writes to the database.
///
/// Each arm goes through the same `commands::` entry point the CLI uses, so
/// `n` and `q new`, `r` and `q rename`, `c` and `q close`, `R` and `q resume`
/// cannot drift apart. An `Err` leaves the form up with the message in it —
/// the input is usually the thing to fix.
pub fn submit(ctx: &Ctx, app: &mut App, prompt: &Prompt, form: &Form) -> anyhow::Result<()> {
    match prompt {
        Prompt::NewQuest => create(ctx, app, form),
        Prompt::Rename(target) => rename_quest(ctx, app, target, form),
        Prompt::Close(target) => close_quest(ctx, app, target, form),
        Prompt::Resume(target) => resume_quest(ctx, app, target, form),
        // A Sessions or Templates prompt never reaches here; `tui::submit`
        // dispatches on the variant first. Listed rather than `_` so a new
        // Quests prompt added without wiring fails to compile here instead of
        // silently doing nothing.
        Prompt::Send(_)
        | Prompt::Kill(_)
        | Prompt::Reset(_)
        | Prompt::AddTemplate
        | Prompt::EditTemplate(_)
        | Prompt::DeleteTemplate(_)
        | Prompt::RunTemplate(_) => Ok(()),
    }
}

/// The Quest the box was opened against, re-read now — by id, not by the
/// selection: a tick can have reordered the listing while the prompt was up,
/// and `q rm` can have removed the Quest outright.
///
/// The id alone is not identity. `new_id` is 16 bits and its retry loop only
/// checks live rows, so a `q rm` and a `q new` in another terminal can hand a
/// deleted Quest's id to a new one; `created_at` is the column nothing can
/// change. The slug is checked too, because the box put it in its title and a
/// rename underneath would make it name a Quest the user did not pick.
fn quest_for(ctx: &Ctx, target: &Target) -> anyhow::Result<Quest> {
    let quest = ctx
        .db()?
        .get_quest(&target.quest)?
        .filter(|q| q.created_at == target.created_at)
        .ok_or_else(|| {
            QError::NotFound(format!("quest {} ({}) is gone", target.slug, target.quest))
        })?;
    if quest.slug != target.slug {
        return Err(QError::Invalid(format!(
            "{} was renamed to {} while this box was up; Esc and try again",
            target.slug, quest.slug
        ))
        .into());
    }
    Ok(quest)
}

/// The same, for the two prompts whose text depends on whether the Quest is
/// finished. The box promised to kill a tmux session and end N sessions, or
/// promised that only the epic was left; acting on the other branch would do
/// something the user was never shown.
///
/// The epic is checked here for the same reason: the close box names it, and
/// `close --close-epic` closes whatever the *refetched* Quest carries. A
/// `q set <slug> beads_epic <other>` underneath an open box would otherwise
/// close an epic the box never mentioned (N-6).
fn quest_for_state(ctx: &Ctx, target: &Target) -> anyhow::Result<Quest> {
    let quest = quest_for(ctx, target)?;
    let finished = quest.state == QuestState::Finished;
    if finished != target.finished {
        let now = if finished { "finished" } else { "running" };
        return Err(QError::Invalid(format!(
            "{} is {now} now, which is not what this box says; Esc and try again",
            quest.slug
        ))
        .into());
    }
    if quest.beads_epic != target.epic {
        let now = quest.beads_epic.as_deref().unwrap_or("none");
        let was = target.epic.as_deref().unwrap_or("none");
        return Err(QError::Invalid(format!(
            "{}'s beads epic is {now} now, not {was} as this box says; Esc and try again",
            quest.slug
        ))
        .into());
    }
    Ok(quest)
}

/// The new-Quest form's workflow, or `None` for the sentinel that means "left
/// blank" — which is what lets a chosen template's own workflow fill it in
/// (`tpl::Merge`), exactly as an empty text field used to.
fn chosen_workflow(form: &Form) -> Option<&str> {
    Some(form.choice(F_WORKFLOW)).filter(|w| !w.is_empty() && *w != NO_WORKFLOW)
}

fn create(ctx: &Ctx, app: &mut App, form: &Form) -> anyhow::Result<()> {
    // The chosen template supplies whatever the form was left blank for; it
    // never overrides something typed — so only the template's own text is
    // placeholder-expanded, and a goal typed into the form is taken literally.
    // `tpl::Merge` is the same merge `q new --template` runs (SPEC §16).
    let chosen: Option<Template> = app
        .quests
        .templates
        .iter()
        .find(|t| t.name == form.choice(F_TEMPLATE))
        .cloned();
    let template = chosen.map(expand).transpose()?;
    let machine = form.choice(F_MACHINE).to_string();
    let no_beads = !form.is_on(F_BEADS);
    let merged = tpl::Merge::new(
        template.as_ref(),
        &tpl::Given {
            goal: form.optional(F_GOAL),
            dir: form.optional(F_DIR),
            workflow: chosen_workflow(form),
            no_beads,
            ..tpl::Given::default()
        },
    );

    let args = new::Args {
        // SPEC §11: the Quest records which template made it, and its run is
        // counted inside `new::create` — so a Quest that is never created
        // never counts, whatever the caller forgets.
        template: template.as_ref(),
        name: form.optional(F_NAME),
        goal: merged.goal.as_deref(),
        dir: merged.dir.as_deref(),
        workflow: merged.workflow.as_deref(),
        repo: merged.repo.as_deref(),
        no_beads,
        prompt: merged.prompt.as_deref(),
        prompt_file: None,
        no_auto_reset: false,
        // A template's `create_brain` maps onto this in a later milestone
        // (7.9); the TUI form has no brain toggle of its own yet.
        brain: false,
        // The TUI never attaches on its own: `q new` ends at a tmux pane,
        // but the TUI is the fleet view and blanking it the instant a
        // Quest exists is not what `n` asks for. `o` is one key away.
        detach: true,
        machine: Some(&machine),
    };

    // SPEC §15: the machine select is not a label. A remote here means the
    // Quest is created *on that machine*, over ssh — the same builder `q new
    // --machine` uses. Before bd-8lz.5.3 this field only stamped a local row
    // with a remote's name, which made it indistinguishable from a real one.
    if let Some(remote) = ctx.config.remotes.iter().find(|r| r.name == machine) {
        let created = proxy::create_remote(ctx, remote, &args)?;
        // The template's *text* travelled as plain `q new` flags, but the link
        // did not: `template_id` names a row in this machine's database, and
        // `proxy::create_remote` has nowhere to put it. So the run is not
        // counted either — `run_count` records Quests this definition made,
        // and the Quest over there is not one of them.
        let note = match template.as_ref() {
            Some(t) => format!(
                " · not linked to template {} (it is this machine's)",
                t.name
            ),
            None => String::new(),
        };
        // No anchor: the row lives in that machine's database and arrives with
        // the next remote tick (SPEC §17's `[ui] tick_remote`).
        app.say(format!(
            "created {} on {} · it appears at the next remote tick{note}",
            created.slug, created.machine
        ));
        return Ok(());
    }

    let created = new::create(ctx, &args)?;
    app.quests.focus_on(Anchor::local(&created.quest));
    app.say(format!("created {} · o enters it", created.quest.slug));
    Ok(())
}

/// The form's half of `q tpl run`'s expansion: `{{date}}` is filled in, and a
/// template that wants a `{{arg.k}}` is refused with the command that can give
/// it one.
fn expand(template: Template) -> anyhow::Result<Template> {
    tpl::expanded_without_args(&template).map_err(|e| {
        // The command first: the form's error line is one row wide and
        // ellipsises, and the actionable half is the one that has to survive.
        QError::Invalid(format!(
            "run it from the CLI: q tpl run {} --arg k=v — {e:#}",
            template.name
        ))
        .into()
    })
}

fn rename_quest(ctx: &Ctx, app: &mut App, target: &Target, form: &Form) -> anyhow::Result<()> {
    let quest = quest_for(ctx, target)?;
    let renamed = rename::apply(ctx, &quest, form.trimmed(F_SLUG), NameSource::Manual, None)?;
    // Same Quest, so `resync` would keep it anyway; said out loud because the
    // slug it is keyed on is not the one the selection was made under.
    app.quests.focus_on(Anchor::local(&renamed.quest));
    app.say(renamed.describe());
    Ok(())
}

fn close_quest(ctx: &Ctx, app: &mut App, target: &Target, form: &Form) -> anyhow::Result<()> {
    let quest = quest_for_state(ctx, target)?;
    let closed = close::apply(ctx, &quest, form.is_on(F_CLOSE_EPIC))?;
    // The selection is deliberately *not* moved: with `f` off the Quest drops
    // out of the listing and `resync` clamps the index, which lands on the row
    // that took its place — the next Quest down. With `f` on it stays put, on
    // the Quest that was just closed.
    app.say(closed.describe());
    Ok(())
}

fn resume_quest(ctx: &Ctx, app: &mut App, target: &Target, form: &Form) -> anyhow::Result<()> {
    let quest = quest_for_state(ctx, target)?;
    let resumed = resume::apply(ctx, &quest, form.optional(F_PROMPT))?;
    app.quests.focus_on(Anchor::local(&resumed.quest));
    app.say(format!("{} · o enters it", resumed.describe()));
    Ok(())
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
        layout::panel_split(area, app.detail && app.quests.selected_row().is_some());
    if let Some(list) = list {
        render_list(frame, list, app);
    }
    if let Some(panel) = panel {
        render_panel(frame, panel, app);
    }
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
    let mut parts = vec![machine_cell(row)];
    // An epic with no issues yet has nothing to report: `0/0 ░░░░░░░` is a
    // row's worth of noise saying only that the Quest is new.
    if let Some(p) = row.view.progress.filter(|p| p.total > 0) {
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

/// SPEC §17's machine column, marked when the row is the cache standing in for
/// a machine that did not answer this round (SPEC §15). The marker is on the
/// row rather than only in the chip because a listing mixes machines: without
/// it a stale row and a live one are indistinguishable side by side.
fn machine_cell(row: &QuestRow) -> String {
    if row.origin.is_stale() {
        format!("{} \u{26a0}", row.view.quest.machine)
    } else {
        row.view.quest.machine.clone()
    }
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
    let lines = panel_lines(
        row,
        app.quests.selected_links(),
        app.quests.selected_events(),
    );
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
            machine_cell(row),
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
    let remote = row.origin.is_remote();
    if remote {
        // Not "none": the sessions are real, they are just in that machine's
        // database (SPEC §15). Saying "none" here would be a claim about a
        // Quest this `q` has never asked about.
        out.push(
            Line::from(Span::raw(format!(
                "{} live · {}",
                view.live_sessions,
                not_fetched(&view.quest.machine)
            )))
            .dim(),
        );
    } else if sessions.is_empty() {
        out.push(Line::from(Span::raw("none").dim()));
    } else {
        for s in sessions {
            out.push(Line::from(Span::raw(session_line(s))));
        }
    }

    // Links and events are read out of *this* machine's database, and a remote
    // Quest is not in it — `reload` and `sync` skip a remote row rather than
    // look its id up locally. So the only honest thing the panel can say here
    // is what the sessions line already says.
    head(&mut out, "links");
    if remote {
        out.push(Line::from(
            Span::raw(not_fetched(&view.quest.machine)).dim(),
        ));
    } else if links.is_empty() {
        out.push(Line::from(Span::raw("none").dim()));
    } else {
        for l in links {
            out.push(Line::from(Span::raw(format!("{} {}", l.kind, l.r#ref))));
        }
    }

    head(&mut out, "events");
    if remote {
        out.push(Line::from(
            Span::raw(not_fetched(&view.quest.machine)).dim(),
        ));
    } else if events.is_empty() {
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

/// What every panel section says about a remote Quest: the rows exist, on that
/// machine, and this `q` has not asked for them (SPEC §15). One wording so the
/// sections cannot contradict each other.
fn not_fetched(machine: &str) -> String {
    format!("on {machine}, not fetched")
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
    use crate::tui::app::tab_layout;
    use crate::tui::keys::MouseInput;
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

    /// A local and a remote row sharing an id — a Quest id is 16 bits, so with
    /// thirty Quests a side the collision is about a 1.4 % event.
    fn colliding_rows() -> Vec<QuestRow> {
        let mut local = quest("local-one", QuestState::Active, 2);
        let mut far = quest("remote-one", QuestState::Active, 1);
        far.machine = "ws".to_string();
        local.id.clone_from(&far.id);
        let view = QuestView::new(far, &[]);
        let raw = serde_json::to_value(&view).unwrap();
        vec![
            QuestRow::local(QuestView::new(local, &[]), Vec::new()),
            QuestRow::remote(crate::remote::RemoteQuest { view, raw }, false),
        ]
    }

    /// The selection is anchored on the machine as well as the id. Anchored on
    /// the id alone, the next 2 s tick re-attaches it to whichever row matches
    /// first — always the local one, because the merge appends the remotes —
    /// and every key `LOCAL_ONLY` guards then acts on the wrong Quest.
    #[test]
    fn the_selection_does_not_jump_to_a_local_row_with_the_same_id() {
        let mut state = State {
            rows: colliding_rows(),
            show_finished: true,
            ..State::default()
        };
        state.links = vec![Vec::new(); state.rows.len()];
        state.selected = 1;
        state.settle(10);
        assert_eq!(state.selected_row().unwrap().view.quest.slug, "remote-one");

        // What every tick does.
        state.resync();
        let row = state.selected_row().unwrap();
        assert_eq!(row.view.quest.slug, "remote-one", "the selection jumped");
        assert!(row.origin.is_remote());

        // …and `focus_on` latches onto the right one too.
        state.selected = 0;
        state.settle(10);
        assert_eq!(state.selected_row().unwrap().view.quest.slug, "local-one");
        let want = Anchor::of(&state.rows[1]);
        state.focus_on(want);
        assert_eq!(state.selected_row().unwrap().view.quest.slug, "remote-one");
    }

    fn session(quest_id: &str, role: SessionRole, status: SessionStatus) -> Session {
        let mut s = Session::new(quest_id, role, "master", "q-x", "%1");
        s.status = status;
        s
    }

    fn row(quest: Quest, sessions: Vec<Session>) -> QuestRow {
        QuestRow::local(QuestView::new(quest, &sessions), sessions)
    }

    /// A row as a remote's `q list --json` produced it — no sessions, and
    /// marked stale when it came out of the cache.
    fn remote_row(machine: &str, slug: &str, state: QuestState, stale: bool) -> QuestRow {
        let mut q = quest(slug, state, 4);
        q.machine = machine.to_string();
        let view = QuestView::new(q, &[]);
        let raw = serde_json::to_value(&view).unwrap();
        QuestRow::remote(crate::remote::RemoteQuest { view, raw }, stale)
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
        // Enter closes it again; `o` is the attach and leaves it alone.
        assert_eq!(handle(&mut app, Input::Char('o')), Action::Attach);
        assert!(app.detail);
        handle(&mut app, Input::Enter);
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

    /// Leaving the tab drops the half-typed query, which widens `visible()`
    /// under a `selected` index that was aimed at the narrowed list. Unless the
    /// selection is re-anchored, the next frame settles the disagreement in
    /// favour of the index and quietly moves the selection to a different
    /// Quest — the one `Enter` attaches to from 4.3 on.
    #[test]
    fn cancelling_a_capture_on_a_tab_switch_keeps_the_same_quest_selected() {
        let rows: Vec<QuestRow> = (0..10)
            .map(|n| {
                row(
                    quest(&format!("quest-{n:02}"), QuestState::Active, 100 - n),
                    Vec::new(),
                )
            })
            .collect();
        let mut app = app_with(rows);
        // Hit-testing reads the width the header published; the click is only
        // meaningful after a frame has been drawn at this size.
        draw(&mut app, 120, 30);

        app.handle(Input::Char('/'));
        for c in "quest-07".chars() {
            app.handle(Input::Char(c));
        }
        assert_eq!(app.quests.visible().len(), 1);
        let want = app.quests.selected_row().unwrap().view.quest.clone();
        assert_eq!(want.slug, "quest-07");

        // Out to Sessions and back, by mouse — the one way out of the box that
        // is neither Esc nor Enter.
        let (_, _, sessions, _) = tab_layout()[1];
        app.handle_mouse(MouseInput::Click {
            col: sessions,
            row: 0,
        });
        let (_, _, quests, _) = tab_layout()[0];
        app.handle_mouse(MouseInput::Click {
            col: quests,
            row: 0,
        });
        assert_eq!(app.tab, Tab::Quests);
        assert!(!app.quests.capturing());
        assert_eq!(app.quests.visible().len(), 10, "the query outlived the tab");

        let text = screen(&mut app, 120, 30);
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "quest-07",
            "the tab switch relocated the selection\n{text}"
        );
        assert_eq!(
            app.quests.selected_id.as_ref().map(|a| a.id.as_str()),
            Some(want.id.as_str())
        );
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

    /// N14: `s` is unreachable while the box is open today, but the handoff
    /// must still go through `select` — the invariant is "a capture is only
    /// ever armed on the active tab", and 4.4/4.5 give other tabs captures.
    #[test]
    fn handing_the_selection_to_sessions_tears_down_an_armed_capture() {
        let mut app = grouped();
        app.quests.searching = true;
        app.quests.query = "run".to_string();
        app.quests.resync();
        let want = app.quests.selected_row().unwrap().view.quest.id.clone();

        assert_eq!(sessions_of_selection(&mut app), Action::Refresh);
        assert_eq!(app.tab, Tab::Sessions);
        assert_eq!(app.focus_quest.as_deref(), Some(want.as_str()));
        assert!(
            !app.quests.capturing(),
            "the box is still holding the keyboard behind an inactive tab"
        );
        assert!(
            app.quests.query.is_empty(),
            "an uncommitted query outlived the tab: {:?}",
            app.quests.query
        );
        assert!(app.status.contains("sessions of running"), "{}", app.status);
    }

    /// The same for `e` (bd-8lz.4.6). `e` is genuinely unreachable while the
    /// box is open -- every key is text in it -- so this is the only place the
    /// tear-down can be shown, and the invariant it protects is the same one:
    /// a capture is only ever armed on the ACTIVE tab.
    #[test]
    fn handing_the_selection_to_events_tears_down_an_armed_capture() {
        let mut app = grouped();
        app.quests.searching = true;
        app.quests.query = "run".to_string();
        app.quests.resync();
        let want = app.quests.selected_row().unwrap().view.quest.id.clone();

        assert_eq!(events_of_selection(&mut app), Action::Refresh);
        assert_eq!(app.tab, Tab::Events);
        assert_eq!(app.focus_quest.as_deref(), Some(want.as_str()));
        assert!(
            !app.quests.capturing(),
            "the box is still holding the keyboard behind an inactive tab"
        );
        assert!(
            app.quests.query.is_empty(),
            "an uncommitted query outlived the tab: {:?}",
            app.quests.query
        );
        assert!(app.status.contains("events of running"), "{}", app.status);
    }

    #[test]
    fn the_prompt_keys_open_their_forms_against_the_selection() {
        for (key, title) in [
            ('n', "new quest"),
            ('r', "rename needs-me"),
            ('c', "close needs-me?"),
            ('R', "resume needs-me"),
        ] {
            let mut app = grouped();
            assert_eq!(handle(&mut app, Input::Char(key)), Action::None);
            let modal = app
                .modal
                .as_ref()
                .unwrap_or_else(|| panic!("{key}: no form"));
            assert_eq!(modal.form.title, title, "{key}");
            assert!(!app.should_quit);
            // The target is carried by id, not by "whatever is selected then".
            let want = (key != 'n').then(|| app.quests.rows[0].view.quest.id.clone());
            assert_eq!(modal.prompt.quest().map(str::to_string), want, "{key}");
        }
    }

    /// With nothing selected there is nothing to rename, close or resume — and
    /// arming a capture over an empty listing would leave a box asking about a
    /// Quest that does not exist.
    #[test]
    fn the_selection_prompts_do_nothing_on_an_empty_listing() {
        for key in ['r', 'c', 'R'] {
            let mut app = app_with(Vec::new());
            assert_eq!(handle(&mut app, Input::Char(key)), Action::None);
            assert!(app.modal.is_none(), "{key} armed a form with no Quest");
            assert!(!app.capturing());
        }
        // `n` needs no selection: it is how the first Quest gets made.
        let mut app = app_with(Vec::new());
        handle(&mut app, Input::Char('n'));
        assert!(app.modal.is_some());
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
        handle(&mut app, Input::Char('m'));
        assert!(!app.status.is_empty());
        crate::tui::report_refresh(&mut app, Ok(()));
        assert!(
            app.status.contains("machine laptop"),
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
        assert!(text.contains("machine laptop"), "{text}");
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

    /// The panel's events are the *selection's* — not the ones the last reload
    /// happened to read. Selection keys move between reloads, and a tick is
    /// 2 s locally and 10 s remote: a whole conversation's worth of a Quest's
    /// history shown under another Quest's name.
    #[test]
    fn the_detail_panel_never_shows_another_quests_events() {
        let db = Db::open_in_memory().unwrap();
        let mut first = Quest::new("aaa-quest", "/tmp/work", "laptop");
        first.goal = Some("the first".to_string());
        let first = db.insert_quest(&first).unwrap();
        let mut master = Session::new(&first.id, SessionRole::Master, "master", "q-x", "%1");
        master.status = SessionStatus::Waiting;
        db.insert_session(&master).unwrap();
        db.append_event(&first.id, None, "event.for.aaa", &serde_json::json!({}))
            .unwrap();

        let mut second = Quest::new("bbb-quest", "/tmp/work", "laptop");
        second.goal = Some("the second".to_string());
        let second = db.insert_quest(&second).unwrap();
        db.append_event(&second.id, None, "event.for.bbb", &serde_json::json!({}))
            .unwrap();

        let (_tmux_dir, tmux) = tmux_with(&[("q-x", "%1")]);
        let ctx = Ctx::for_tests(Config::default(), db, tmux);
        let mut app = App::new(&ctx.config, "laptop");
        app.set_size(120, 30);
        refresh(&ctx, &mut app).unwrap();
        app.detail = true;

        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "aaa-quest"
        );
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("event.for.aaa"), "{text}");

        // Move down. Before anything reloads, the panel may show nothing —
        // but it may never show the previous Quest's events under this title.
        handle(&mut app, Input::Char('j'));
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "bbb-quest"
        );
        let text = screen(&mut app, 120, 30);
        assert!(
            !text.contains("event.for.aaa"),
            "the previous selection's events are still on screen\n{text}"
        );

        // And the loop's redraw preamble fills in the right ones without
        // paying for a whole reload.
        sync(&ctx, &mut app).unwrap();
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("event.for.bbb"), "{text}");
        assert!(!text.contains("event.for.aaa"), "{text}");

        // Back up again: the same guarantee in the other direction.
        handle(&mut app, Input::Char('k'));
        sync(&ctx, &mut app).unwrap();
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("event.for.aaa"), "{text}");
        assert!(!text.contains("event.for.bbb"), "{text}");
    }

    /// A reload can move the selected Quest below the fold — `resync` alone
    /// only ever scrolls up, so the highlight would vanish until a keypress.
    #[test]
    fn a_reload_that_reorders_scrolls_the_selection_back_into_view() {
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
        handle(&mut app, Input::Char('g'));
        let selected = app.quests.selected_row().unwrap().view.quest.slug.clone();
        assert_eq!(selected, "quest-00");

        // The Quest the user is looking at drops to the bottom of the listing
        // (its group changed, or every other Quest was touched since).
        app.quests.rows.rotate_left(1);
        app.quests.resync();
        settle_view(&mut app);
        assert!(
            app.quests.selected >= app.quests.offset
                && app.quests.selected < app.quests.offset + viewport(&app),
            "selected {} outside [{}, {}) after the reorder",
            app.quests.selected,
            app.quests.offset,
            app.quests.offset + viewport(&app)
        );
        let lines = draw(&mut app, 120, 26);
        assert!(line_of(&lines, &selected).is_some(), "{lines:#?}");

        // A resize is the same problem: the viewport shrinks under a selection
        // that was comfortably on screen.
        let lines = draw(&mut app, 120, 10);
        assert!(line_of(&lines, &selected).is_some(), "{lines:#?}");
        let lines = draw(&mut app, 120, 8);
        assert!(line_of(&lines, &selected).is_some(), "{lines:#?}");
    }

    /// The same defect the Events tab was carrying (bd-8lz.4.6 D1), latent
    /// here only because the selection usually sits near the top: `settle`
    /// pushed `offset` forward and never back, so a viewport that GREW between
    /// two frames left the bottom of the listing blank with rows stranded
    /// above the fold.
    #[test]
    fn a_grown_viewport_refills_the_listing() {
        let rows: Vec<QuestRow> = (0..20)
            .map(|n| {
                row(
                    quest(&format!("quest-{n:02}"), QuestState::Active, n as i64),
                    Vec::new(),
                )
            })
            .collect();
        let mut app = app_with(rows);
        // A short terminal with the cursor at the end pushes `offset` as far
        // forward as it goes.
        app.set_size(120, 12);
        handle(&mut app, Input::End);
        draw(&mut app, 120, 12);
        assert!(app.quests.offset > 0, "the short frame never scrolled");

        // Now the terminal grows past the whole listing. Every Quest fits, so
        // every Quest has to be on screen.
        let lines = draw(&mut app, 120, 60);
        for n in 0..20 {
            let slug = format!("quest-{n:02}");
            assert!(
                line_of(&lines, &slug).is_some(),
                "{slug} stranded: {lines:#?}"
            );
        }
        // And the pull-back went the whole way. Without this a clamp that only
        // came half the distance — `len - viewport / 2`, say — would still show
        // every row on a body this tall and pass the loop above.
        assert_eq!(app.quests.offset, 0, "the pull-back stopped short");
    }

    /// The pull-back is only as good as the `viewport` it clamps against, and
    /// `viewport` used to reserve all four group headers whether the listing
    /// had them or not. So it UNDER-counted the body's real capacity, `len -
    /// viewport` came out too large, and the grow healed the listing only
    /// partway: 60 Quests at 120x12 with the cursor at the end, grown to
    /// 120x30, left three body lines blank with `quest-12` still above the
    /// fold. `viewport` reserves the headers the listing actually has.
    #[test]
    fn a_grown_viewport_leaves_no_blank_line_a_row_could_have_filled() {
        let rows: Vec<QuestRow> = (0..60)
            .map(|n| {
                row(
                    quest(&format!("quest-{n:02}"), QuestState::Active, n as i64),
                    Vec::new(),
                )
            })
            .collect();
        let mut app = app_with(rows);
        app.set_size(120, 12);
        handle(&mut app, Input::End);
        draw(&mut app, 120, 12);
        assert!(app.quests.offset > 0, "the short frame never scrolled");

        // The grown terminal still cannot hold all 60, so rows stay above the
        // fold — and every body line the renderer left blank is a line one of
        // them could have used.
        let lines = draw(&mut app, 120, 30);
        assert!(app.quests.offset > 0, "the whole listing fit after all");
        let body = &lines[1..lines.len() - 1];
        let blank = body.iter().rev().take_while(|l| l.is_empty()).count();
        assert!(
            blank < RowMode::Two.lines() as usize,
            "{blank} blank body lines with {} rows above the fold:\n{}",
            app.quests.offset,
            lines.join("\n")
        );
        // The row that was stranded, named.
        assert!(
            line_of(&lines, "quest-12").is_some(),
            "quest-12 stranded:\n{}",
            lines.join("\n")
        );
    }

    /// A committed filter hides rows for as long as it is on; a one-shot
    /// status message is gone by the next keypress.
    #[test]
    fn an_active_filter_stays_on_screen_after_the_message_that_set_it() {
        let mut app = grouped();
        app.handle(Input::Char('/'));
        for c in "run".chars() {
            app.handle(Input::Char(c));
        }
        app.handle(Input::Enter);
        // Another key speaks, and the search feedback is gone.
        handle(&mut app, Input::Char('l'));
        assert!(!app.status.contains("/run"), "{}", app.status);
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("[/run]"), "the filter is invisible\n{text}");
        assert!(!text.contains("resting"), "{text}");

        // A refresh failure outranks the message and still does not hide it.
        crate::tui::report_refresh(&mut app, Err(anyhow::anyhow!("database is locked")));
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("[/run]"), "{text}");
        crate::tui::report_refresh(&mut app, Ok(()));

        // The machine filter is announced the same way. Two Escs: `l` opened
        // the detail panel, and Esc closes that before it clears the search.
        app.handle(Input::Esc);
        app.handle(Input::Esc);
        handle(&mut app, Input::Char('m'));
        handle(&mut app, Input::Char('l'));
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("[m laptop]"), "{text}");
    }

    /// An epic nobody has filed an issue against yet has nothing to say.
    #[test]
    fn an_empty_epic_draws_no_bar_at_all() {
        let mut q = quest("fresh", QuestState::Active, 5);
        q.beads_epic = Some("bd-42".to_string());
        let mut r = row(q, Vec::new());
        r.view.progress = Some(crate::beads::Progress::default());
        let mut app = app_with(vec![r]);
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("fresh"), "{text}");
        assert!(!text.contains("0/0"), "{text}");
        assert!(!text.contains('░'), "{text}");
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

    // ------------------------------------------------------- remote rows

    /// SPEC §15's machine column, and the mark that says a row is the cache
    /// standing in for a machine that did not answer.
    #[test]
    fn a_row_from_a_machine_that_is_down_is_drawn_as_stale() {
        let mut app = app_with(vec![
            row(quest("here", QuestState::Active, 9), Vec::new()),
            remote_row("ws", "fresh", QuestState::Active, false),
            remote_row("box", "cached", QuestState::Active, true),
        ]);
        let text = screen(&mut app, 120, 20);
        assert!(text.contains("laptop"), "{text}");
        assert!(text.contains("ws"), "{text}");
        assert!(text.contains("box \u{26a0}"), "{text}");
    }

    /// `m` (SPEC §17) cycles whatever machines the merged listing holds, this
    /// one included — the remote rows are in `rows` like any other.
    #[test]
    fn m_narrows_to_a_remote_machine_and_back() {
        let mut app = app_with(vec![
            row(quest("here", QuestState::Active, 9), Vec::new()),
            remote_row("ws", "over-there", QuestState::Active, false),
        ]);
        assert_eq!(app.quests.visible().len(), 2);

        handle(&mut app, Input::Char('m'));
        assert_eq!(app.quests.machine.as_deref(), Some("laptop"));
        assert_eq!(app.quests.visible().len(), 1);
        assert_eq!(app.quests.selected_row().unwrap().view.quest.slug, "here");

        handle(&mut app, Input::Char('m'));
        assert_eq!(app.quests.machine.as_deref(), Some("ws"));
        assert_eq!(app.quests.visible().len(), 1);
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "over-there"
        );
        assert!(app.filters().contains("m ws"));

        handle(&mut app, Input::Char('m'));
        assert_eq!(app.quests.machine, None);
        assert_eq!(app.quests.visible().len(), 2);
    }

    /// A machine name is searchable, so `/ws` narrows the merged listing the
    /// same way `m` does.
    #[test]
    fn a_machine_name_is_part_of_the_search_haystack() {
        let mut app = app_with(vec![
            row(quest("here", QuestState::Active, 9), Vec::new()),
            remote_row("ws", "over-there", QuestState::Active, false),
        ]);
        app.quests.query = "ws".to_string();
        app.quests.resync();
        assert_eq!(app.quests.visible().len(), 1);
    }

    /// Every key but `o` runs against *this* machine's database, and a Quest
    /// id is unique only per machine — so `c` on a remote row could otherwise
    /// close a local Quest that happens to share the id.
    #[test]
    fn the_local_only_keys_are_refused_on_a_remote_row() {
        let mut app = app_with(vec![remote_row(
            "ws",
            "over-there",
            QuestState::Active,
            false,
        )]);
        for key in ['s', 'e', 'r', 'c', 'R', 'b', 'l'] {
            app.status.clear();
            let action = handle(&mut app, Input::Char(key));
            assert_eq!(action, Action::None, "`{key}` acted on a remote row");
            assert!(app.modal.is_none(), "`{key}` opened a form");
            assert_eq!(app.tab, Tab::Quests, "`{key}` switched tabs");
            assert!(
                app.status.contains("runs on ws"),
                "`{key}` said nothing: {}",
                app.status
            );
            // D2: the CLI proxies every one of these now, so the refusal must
            // not claim the operation needs that machine — it points at the
            // command that does work from here.
            assert!(
                app.status.contains(cli_equivalent(key)) && app.status.contains("in a shell"),
                "`{key}` did not name what does work: {}",
                app.status
            );
            assert!(
                !app.status.contains("needs that machine"),
                "`{key}` still claims the CLI cannot do this: {}",
                app.status
            );
        }
        // `o` is the one remote action SPEC §15 defines.
        assert_eq!(handle(&mut app, Input::Char('o')), Action::Attach);
        // And a local row still answers all of them.
        let mut app = app_with(vec![row(quest("here", QuestState::Active, 9), Vec::new())]);
        assert_eq!(handle(&mut app, Input::Char('r')), Action::None);
        assert!(app.modal.is_some(), "the rename form did not open");
    }

    #[test]
    fn selected_remote_names_the_machine_only_for_a_remote_row() {
        let mut app = app_with(vec![
            row(quest("here", QuestState::Active, 9), Vec::new()),
            remote_row("ws", "over-there", QuestState::Active, false),
        ]);
        assert_eq!(selected_remote(&app), None);
        handle(&mut app, Input::Down);
        assert_eq!(selected_remote(&app).as_deref(), Some("ws"));
    }

    fn panel_text(row: &QuestRow, links: &[Link], events: &[Event]) -> Vec<String> {
        panel_lines(row, links, events)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    /// The panel says where a remote Quest's sessions, links and events are
    /// rather than claiming there are none: this `q` has never asked that
    /// machine, and `none` reads as a fact about the Quest.
    #[test]
    fn the_panel_does_not_claim_a_remote_quest_is_empty() {
        let mut far = remote_row("ws", "over-there", QuestState::Active, false);
        far.view.live_sessions = 2;
        let lines = panel_text(&far, &[], &[]);

        assert!(
            lines.contains(&"2 live · on ws, not fetched".to_string()),
            "{lines:#?}"
        );
        // Sessions, links and events — all three, in one wording.
        assert_eq!(
            lines
                .iter()
                .filter(|l| l.contains("on ws, not fetched"))
                .count(),
            3,
            "{lines:#?}"
        );
        assert!(!lines.iter().any(|l| l == "none"), "{lines:#?}");

        let mut app = app_with(vec![far]);
        app.detail = true;
        let text = screen(&mut app, 140, 30);
        assert!(text.contains("on ws, not fetched"), "{text}");
    }

    /// …while a local Quest that really has nothing still says so.
    #[test]
    fn the_panel_still_says_none_for_an_empty_local_quest() {
        let here = row(quest("here", QuestState::Active, 9), Vec::new());
        let lines = panel_text(&here, &[], &[]);
        assert_eq!(
            lines.iter().filter(|l| *l == "none").count(),
            3,
            "{lines:#?}"
        );
        assert!(
            !lines.iter().any(|l| l.contains("not fetched")),
            "{lines:#?}"
        );
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::{Quest, QuestState, SessionRole, SessionStatus};
    use crate::tui::form::Field;
    use crate::tui::render;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // ------------------------------------------------------------- fixtures

    /// A real `Ctx` over an in-memory database and a fixture tmux, plus the
    /// directory a Quest can be created in. Nothing here touches the process
    /// environment: `Q_DB`, `Q_CONFIG` and `Q_FIXTURE` are all bypassed by
    /// `Ctx::for_tests`, which is the only way to stay safe under parallel
    /// test threads.
    struct Rig {
        ctx: Ctx,
        tmux: tempfile::TempDir,
        cwd: tempfile::TempDir,
    }

    impl Rig {
        /// Beads refuses every call, which is what a test that is not about
        /// beads wants: reaching `bd` by accident is then a failure, not a
        /// subprocess.
        fn new() -> Rig {
            Rig::with_bd(Box::new(crate::beads::stub::NoBd))
        }

        /// A rig whose `bd` is the caller's, so the epic paths SPEC §5 step 2
        /// and §13 describe run in-crate — the paths that used to be
        /// unreachable here, and where B1 lived.
        fn with_bd(bd: Box<dyn crate::beads::Bd>) -> Rig {
            let tmux = tempfile::tempdir().unwrap();
            let path = tmux.path().join("tmux.json");
            std::fs::write(&path, "{}").unwrap();
            Rig {
                ctx: Ctx::for_tests(
                    Config::default(),
                    Db::open_in_memory().unwrap(),
                    Box::new(crate::tmux::FixtureTmux::new(path)),
                )
                .with_bd(bd),
                tmux,
                cwd: tempfile::tempdir().unwrap(),
            }
        }

        fn dir(&self) -> String {
            self.cwd.path().to_string_lossy().to_string()
        }

        fn fixture(&self) -> crate::tmux::FixtureTmux {
            crate::tmux::FixtureTmux::new(self.tmux.path().join("tmux.json"))
        }

        /// Make the next `new-session` fail, so the rollback `q new` does when
        /// the master will not start is reachable.
        fn break_new_session(&self, why: &str) {
            let fixture = self.fixture();
            let mut state = fixture.load().unwrap();
            state.fail_new_session = Some(why.to_string());
            fixture.save(&state).unwrap();
        }

        fn app(&self) -> App {
            let mut app = App::new(&self.ctx.config, "laptop");
            app.set_size(120, 40);
            refresh(&self.ctx, &mut app).unwrap();
            app
        }

        fn quests(&self) -> Vec<Quest> {
            self.ctx.db().unwrap().list_quests(true).unwrap()
        }

        fn slugs(&self) -> Vec<String> {
            self.quests().into_iter().map(|q| q.slug).collect()
        }
    }

    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
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
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle(Input::Char(c));
        }
    }

    /// Tab to a field by label; panics rather than typing into the wrong one.
    fn focus(app: &mut App, label: &str) {
        for _ in 0..24 {
            let at = app
                .modal
                .as_ref()
                .expect("no form is open")
                .form
                .focused()
                .map(Field::label);
            if at == Some(label) {
                return;
            }
            app.handle(Input::Tab);
        }
        panic!("no field labelled {label}");
    }

    fn set(app: &mut App, label: &str, value: &str) {
        focus(app, label);
        app.handle(Input::Ctrl('u'));
        type_text(app, value);
    }

    /// Move a select onto a named option, wherever it sits in the list — the
    /// select equivalent of [`set`]. The options wrap, so one direction reaches
    /// every one of them.
    fn choose(app: &mut App, label: &str, value: &str) {
        focus(app, label);
        for _ in 0..32 {
            if app
                .modal
                .as_ref()
                .expect("no form is open")
                .form
                .choice(label)
                == value
            {
                return;
            }
            app.handle(Input::Right);
        }
        panic!("no option `{value}` on the {label} row");
    }

    /// Beads is on by default (SPEC §5 step 2). The rig's `bd` is
    /// `stub::NoBd` unless a test asked for another one (`Ctx::for_tests`), so
    /// leaving it on would only earn a refusal and a warning — every
    /// submission that is not about beads turns it off first.
    fn no_beads(app: &mut App) {
        focus(app, F_BEADS);
        assert!(app.modal.as_ref().unwrap().form.is_on(F_BEADS));
        app.handle(Input::Char(' '));
        assert!(!app.modal.as_ref().unwrap().form.is_on(F_BEADS));
    }

    /// Whether the open form has an action row (B2). `rename` has none.
    fn has_action(app: &App) -> bool {
        app.modal.as_ref().is_some_and(|m| {
            m.form
                .fields()
                .iter()
                .any(|f| f.label() == crate::tui::form::ACTION)
        })
    }

    /// Move the action row off `cancel`, the way the user has to before any
    /// of these prompts does anything.
    fn choose_action(app: &mut App) {
        if !has_action(app) {
            return;
        }
        focus(app, crate::tui::form::ACTION);
        // Idempotent: a form left up by a failed submit keeps the choice
        // already made, so this must land on the verb, not cycle past it.
        for _ in 0..3 {
            if app.modal.as_ref().unwrap().form.confirmed() {
                return;
            }
            app.handle(Input::Right);
        }
        panic!("the action row never left `{}`", crate::tui::form::CANCEL);
    }

    /// One key, treated exactly as the event loop treats it: the work happens
    /// only if the state machine asked for it.
    fn press(rig: &Rig, app: &mut App, input: Input) -> Action {
        let action = app.handle(input);
        if action == Action::Submit {
            crate::tui::submit(&rig.ctx, app);
        }
        action
    }

    /// Exactly what the event loop does with `Action::Submit`.
    fn submit(rig: &Rig, app: &mut App) {
        choose_action(app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        crate::tui::submit(&rig.ctx, app);
    }

    /// Drive the whole new-Quest form for a named Quest in the rig's dir.
    fn make(rig: &Rig, app: &mut App, name: &str) {
        app.handle(Input::Char('n'));
        set(app, F_NAME, name);
        set(app, F_DIR, &rig.dir());
        no_beads(app);
        submit(rig, app);
        assert!(app.modal.is_none(), "form still up: {}", screen(app));
        refresh(&rig.ctx, app).unwrap();
    }

    // ------------------------------------------------------------- new quest

    #[test]
    fn the_new_quest_form_creates_exactly_one_quest_and_selects_it() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "cdc-backfill");
        set(&mut app, F_GOAL, "make the backfill idempotent");
        set(&mut app, F_DIR, &rig.dir());
        // A select, not free text: `q new --workflow` refuses a name the
        // registry does not have, so the form offers the names it does.
        choose(&mut app, F_WORKFLOW, "orchestrator");
        no_beads(&mut app);
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "the form is still up: {}", app.status);
        assert!(!app.capturing());
        let quests = rig.quests();
        assert_eq!(quests.len(), 1, "{:?}", rig.slugs());
        assert_eq!(quests[0].slug, "cdc-backfill");
        assert_eq!(
            quests[0].goal.as_deref(),
            Some("make the backfill idempotent")
        );
        assert_eq!(quests[0].workflow.as_deref(), Some("orchestrator"));
        assert_eq!(quests[0].machine, "laptop");
        assert_eq!(quests[0].beads_epic, None);
        // The master is up in its own tmux session, the same as `q new`.
        let sessions = rig
            .ctx
            .db()
            .unwrap()
            .list_sessions_by_quest(&quests[0].id)
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].role, SessionRole::Master);
        assert!(
            rig.fixture()
                .load()
                .unwrap()
                .panes
                .iter()
                .any(|p| p.session_name == "q-cdc-backfill")
        );

        // The selection lands on what was just made, not on wherever the row
        // that used to be there went.
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "cdc-backfill"
        );
        assert!(
            app.status.contains("created cdc-backfill"),
            "{}",
            app.status
        );
    }

    /// A blank name is the auto slug of SPEC §10, not a validation error.
    #[test]
    fn a_blank_name_is_named_from_the_directory() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_DIR, &rig.dir());
        no_beads(&mut app);
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quests = rig.quests();
        assert_eq!(quests.len(), 1);
        assert!(!quests[0].slug.is_empty());
        assert_eq!(quests[0].name_source, crate::model::NameSource::Auto);
    }

    #[test]
    fn a_directory_that_does_not_exist_keeps_the_form_up_and_creates_nothing() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "nowhere");
        set(&mut app, F_DIR, "/no/such/place/at/all");
        no_beads(&mut app);
        submit(&rig, &mut app);

        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(
            form.error().unwrap().contains("no such directory"),
            "{:?}",
            form.error()
        );
        // Everything typed is still there to fix.
        assert_eq!(form.trimmed(F_NAME), "nowhere");
        assert!(rig.quests().is_empty());
        assert!(!app.should_quit);
        assert!(screen(&mut app).contains("no such directory"));

        // Fixing the field clears the complaint, and the second try works.
        set(&mut app, F_DIR, &rig.dir());
        assert!(app.modal.as_ref().unwrap().form.error().is_none());
        submit(&rig, &mut app);
        assert!(app.modal.is_none());
        assert_eq!(rig.slugs(), ["nowhere"]);
    }

    /// SPEC §5: when the master will not start, the Quest row goes with it —
    /// a row pointing at a tmux session that was never created is worse than
    /// no row at all.
    #[test]
    fn a_creation_that_fails_partway_rolls_the_quest_back() {
        let rig = Rig::new();
        let mut app = rig.app();
        rig.break_new_session("no server running on /tmp/tmux-501/default");

        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "half-made");
        set(&mut app, F_DIR, &rig.dir());
        no_beads(&mut app);
        submit(&rig, &mut app);

        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(
            form.error().unwrap().contains("no server running"),
            "{form:?}"
        );
        assert!(
            rig.quests().is_empty(),
            "the half-made quest survived: {:?}",
            rig.slugs()
        );
        assert!(!app.should_quit, "a failed create must not end the TUI");
        // And the listing the TUI is drawing agrees.
        refresh(&rig.ctx, &mut app).unwrap();
        assert!(app.quests.rows.is_empty());
    }

    /// SPEC §15/§17: the form's machine select is not a label. Choosing a
    /// remote creates the Quest **on that machine**, over ssh, with the same
    /// `q new … -d --json --no-remote` the CLI sends — and writes nothing here.
    ///
    /// The bug this pins: before bd-8lz.5.3 this field only stamped a local row
    /// with a remote's name, and the listing then showed a Quest on `ws` that
    /// `ws` had never heard of.
    #[test]
    fn choosing_a_remote_in_the_new_form_creates_the_quest_on_that_machine() {
        let mut config = Config::default();
        config.machine.name = "laptop".to_string();
        config.remotes = vec![crate::config::Remote {
            name: "ws".to_string(),
            ssh: "ws-host".to_string(),
        }];
        // No tmux fixture file: a Quest created over there must never reach
        // this machine's tmux.
        let ctx = Ctx::for_tests(
            config,
            Db::open_in_memory().unwrap(),
            Box::new(crate::tmux::FixtureTmux::new(std::path::PathBuf::from(
                "/nonexistent/tmux.json",
            ))),
        );
        let ssh = std::sync::Arc::new(crate::remote::stub::StubSsh::new(&[(
            "ws-host",
            crate::remote::SshOutcome::Done {
                code: Some(0),
                stdout: serde_json::json!({
                    "quest": { "id": "q-1234", "slug": "over-there", "machine": "ws" },
                    "tmux_session": "q-over-there",
                })
                .to_string(),
                stderr: String::new(),
            },
        )]));
        let ctx = ctx.with_ssh(ssh.clone());

        let mut app = App::new(&ctx.config, "laptop");
        app.set_size(120, 40);
        refresh(&ctx, &mut app).unwrap();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "over-there");
        no_beads(&mut app);
        focus(&mut app, F_MACHINE);
        app.handle(Input::Right);
        assert_eq!(app.modal.as_ref().unwrap().form.choice(F_MACHINE), "ws");
        choose_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        crate::tui::submit(&ctx, &mut app);

        assert!(app.modal.is_none(), "the form is still up: {}", app.status);
        assert!(app.status.contains("over-there"), "{}", app.status);
        assert!(app.status.contains("on ws"), "{}", app.status);
        // Nothing here: the row lives in that machine's database.
        assert!(ctx.db().unwrap().list_quests(true).unwrap().is_empty());

        let calls = ssh.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].0, "ws-host");
        assert_eq!(
            calls[0].1,
            [
                "q",
                "new",
                "--name",
                "over-there",
                "--no-beads",
                "-d",
                "--json",
                "--no-remote"
            ]
        );
    }

    #[test]
    fn cancelling_the_form_restores_nothing() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "never-made");
        set(&mut app, F_GOAL, "nor this");
        app.handle(Input::Esc);

        assert!(app.modal.is_none());
        assert!(!app.capturing());
        assert!(app.status.is_empty(), "{:?}", app.status);
        assert!(rig.quests().is_empty());
        // And the keyboard is the shell's again.
        assert_eq!(app.handle(Input::Char('q')), Action::Quit);
    }

    #[test]
    fn the_machine_field_offers_the_local_machine_and_every_remote() {
        let mut config = Config::default();
        config.remotes.push(crate::config::Remote {
            name: "ws".to_string(),
            ssh: "ws.local".to_string(),
        });
        let mut app = App::new(&config, "laptop");
        app.set_size(120, 40);
        app.handle(Input::Char('n'));
        focus(&mut app, F_MACHINE);
        let form = &app.modal.as_ref().unwrap().form;
        assert_eq!(form.choice(F_MACHINE), "laptop");
        app.handle(Input::Right);
        assert_eq!(app.modal.as_ref().unwrap().form.choice(F_MACHINE), "ws");
        app.handle(Input::Right);
        assert_eq!(app.modal.as_ref().unwrap().form.choice(F_MACHINE), "laptop");
    }

    /// SPEC §11's workflow names, as a select rather than free text: `q new
    /// --workflow` refuses a name the registry does not have, so a text field
    /// here could only ever build a form that fails on submit.
    #[test]
    fn the_workflow_field_is_a_select_over_the_registry() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        let form = &app.modal.as_ref().unwrap().form;
        // The rig's registry has no user directory, so this is the built-ins.
        assert_eq!(form.choice(F_WORKFLOW), NO_WORKFLOW, "unset by default");
        let offered: Vec<String> = (0..6)
            .map(|_| {
                let at = app
                    .modal
                    .as_ref()
                    .unwrap()
                    .form
                    .choice(F_WORKFLOW)
                    .to_string();
                focus(&mut app, F_WORKFLOW);
                app.handle(Input::Right);
                at
            })
            .collect();
        assert_eq!(
            offered,
            [
                NO_WORKFLOW,
                "orchestrator",
                "research",
                "review",
                "routine",
                "solo",
            ]
        );

        // `(none)` means no workflow, not a Quest whose workflow is `(none)`.
        set(&mut app, F_NAME, "unset");
        set(&mut app, F_DIR, &rig.dir());
        choose(&mut app, F_WORKFLOW, NO_WORKFLOW);
        no_beads(&mut app);
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert_eq!(rig.quests()[0].workflow, None);
    }

    /// A user file joins the select, and the whole set comes off the `Ctx`'s
    /// registry rather than the developer's own config directory.
    #[test]
    fn a_user_workflow_file_joins_the_new_quest_form() {
        let dir = tempfile::tempdir().unwrap();
        crate::workflows::Registry::new(dir.path())
            .write("triage", "# triage\n\nmine.\n")
            .unwrap();
        let mut rig = Rig::new();
        rig.ctx = rig.ctx.with_workflows(dir.path());
        let mut app = rig.app();

        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "picked");
        set(&mut app, F_DIR, &rig.dir());
        choose(&mut app, F_WORKFLOW, "triage");
        no_beads(&mut app);
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert_eq!(rig.quests()[0].workflow.as_deref(), Some("triage"));
    }

    /// The template select is what SPEC §17 asks for; with an empty table it
    /// still has to be a legal field with a legal value.
    #[test]
    fn the_template_field_lists_the_templates_and_fills_blanks() {
        let rig = Rig::new();
        let mut template = crate::model::Template::new("weekly-hygiene");
        template.goal = Some("tidy up".to_string());
        template.cwd = Some(rig.dir());
        template.workflow = Some("routine".to_string());
        rig.ctx.db().unwrap().insert_template(&template).unwrap();

        let mut app = rig.app();
        app.handle(Input::Char('n'));
        assert_eq!(
            app.modal.as_ref().unwrap().form.choice(F_TEMPLATE),
            NO_TEMPLATE
        );
        set(&mut app, F_NAME, "from-template");
        // Chosen values win; the blanks come from the template.
        choose(&mut app, F_WORKFLOW, "solo");
        focus(&mut app, F_TEMPLATE);
        app.handle(Input::Right);
        assert_eq!(
            app.modal.as_ref().unwrap().form.choice(F_TEMPLATE),
            "weekly-hygiene"
        );
        no_beads(&mut app);
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quest = &rig.quests()[0];
        assert_eq!(quest.goal.as_deref(), Some("tidy up"));
        assert_eq!(quest.workflow.as_deref(), Some("solo"));
        assert!(
            quest
                .cwd
                .ends_with(rig.cwd.path().file_name().unwrap().to_str().unwrap())
        );
    }

    // --------------------------------------------------------------- rename

    #[test]
    fn rename_moves_the_slug_and_keeps_the_selection_on_the_same_quest() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "old-name");
        let id = rig.quests()[0].id.clone();

        app.handle(Input::Char('r'));
        assert_eq!(app.modal.as_ref().unwrap().form.trimmed(F_SLUG), "old-name");
        set(&mut app, F_SLUG, "new-name");
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert_eq!(rig.slugs(), ["new-name"]);
        assert_eq!(rig.quests()[0].id, id, "a rename must not make a new Quest");
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(app.quests.selected_row().unwrap().view.quest.id, id);
        assert!(app.status.contains("old-name → new-name"), "{}", app.status);
    }

    #[test]
    fn renaming_onto_a_slug_another_quest_holds_keeps_the_form_up() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "first-one");
        make(&rig, &mut app, "second-one");
        // The selection is on the one that was just made.
        assert_eq!(
            app.quests.selected_row().unwrap().view.quest.slug,
            "second-one"
        );

        app.handle(Input::Char('r'));
        set(&mut app, F_SLUG, "first-one");
        submit(&rig, &mut app);

        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(
            form.error().unwrap().contains("already taken"),
            "{:?}",
            form.error()
        );
        let mut slugs = rig.slugs();
        slugs.sort();
        assert_eq!(slugs, ["first-one", "second-one"]);
    }

    #[test]
    fn an_illegal_slug_is_refused_by_the_same_rule_the_cli_uses() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "legal-name");
        app.handle(Input::Char('r'));
        set(&mut app, F_SLUG, "Not A Slug");
        submit(&rig, &mut app);
        let form = &app.modal.as_ref().unwrap().form;
        assert!(form.error().unwrap().contains("invalid slug"), "{form:?}");
        assert_eq!(rig.slugs(), ["legal-name"]);
    }

    // ---------------------------------------------------------------- close

    #[test]
    fn the_close_prompt_shows_what_it_will_do_and_both_of_its_options() {
        // A Quest with an epic reaches `progress_all_with`, which shares the
        // process-wide failure window.
        let _guard = crate::beads::backoff::acquire();
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "with-epic");
        let db = rig.ctx.db().unwrap();
        let id = rig.quests()[0].id.clone();
        db.update_quest(
            &id,
            &crate::db::quest::QuestPatch {
                beads_epic: Some(Some("bd-99".to_string())),
                ..Default::default()
            },
        )
        .unwrap();
        refresh(&rig.ctx, &mut app).unwrap();

        app.handle(Input::Char('c'));
        let text = screen(&mut app);
        assert!(text.contains("close with-epic?"), "{text}");
        assert!(text.contains("kills tmux q-with-epic"), "{text}");
        assert!(text.contains("ends 1 live session(s)"), "{text}");
        // Option one: the beads epic (SPEC §5, §13).
        assert!(text.contains(F_CLOSE_EPIC), "{text}");
        assert!(text.contains("epic bd-99"), "{text}");
        assert!(!app.modal.as_ref().unwrap().form.is_on(F_CLOSE_EPIC));
        focus(&mut app, F_CLOSE_EPIC);
        app.handle(Input::Char(' '));
        assert!(app.modal.as_ref().unwrap().form.is_on(F_CLOSE_EPIC));
        // Option two is named but not offered: nothing in the tree implements
        // it yet, and a toggle that silently does nothing is worse.
        assert!(text.contains("brain summary (--summarize)"), "{text}");

        // A Quest with no epic says so rather than offering a dead toggle.
        app.handle(Input::Esc);
        db.insert_quest(&Quest::new("no-epic", "/tmp/work", "laptop"))
            .unwrap();
        refresh(&rig.ctx, &mut app).unwrap();
        app.quests.focus_on_id(
            &rig.quests()
                .iter()
                .find(|q| q.slug == "no-epic")
                .unwrap()
                .id,
        );
        app.handle(Input::Char('c'));
        let text = screen(&mut app);
        assert!(text.contains("no beads epic"), "{text}");
        assert!(!text.contains(F_CLOSE_EPIC), "{text}");
    }

    #[test]
    fn closing_ends_the_sessions_and_moves_the_selection_to_the_next_quest() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "aaa-first");
        make(&rig, &mut app, "bbb-second");
        // Both idle-ish and equally ranked, so the order is by `updated_at`:
        // the newest first.
        let order: Vec<String> = app
            .quests
            .rows
            .iter()
            .map(|r| r.view.quest.slug.clone())
            .collect();
        assert_eq!(order.len(), 2);

        let doomed = app.quests.selected_row().unwrap().view.quest.slug.clone();
        let survivor = order.iter().find(|s| **s != doomed).unwrap().clone();
        app.handle(Input::Char('c'));
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        refresh(&rig.ctx, &mut app).unwrap();

        let closed = rig.quests().into_iter().find(|q| q.slug == doomed).unwrap();
        assert_eq!(closed.state, QuestState::Finished);
        assert!(closed.finished_at.is_some());
        let sessions = rig
            .ctx
            .db()
            .unwrap()
            .list_sessions_by_quest(&closed.id)
            .unwrap();
        assert!(sessions.iter().all(|s| s.status == SessionStatus::Ended));
        assert!(
            !rig.fixture()
                .load()
                .unwrap()
                .panes
                .iter()
                .any(|p| p.session_name == format!("q-{doomed}")),
            "the tmux session outlived the close"
        );

        // `f` is off, so the closed Quest left the listing and the selection
        // fell onto the row that took its place.
        assert_eq!(app.quests.rows.len(), 1);
        assert_eq!(app.quests.selected_row().unwrap().view.quest.slug, survivor);
        assert!(app.status.contains("closed"), "{}", app.status);
    }

    #[test]
    fn closing_a_quest_that_is_already_finished_says_so_rather_than_failing() {
        let rig = Rig::new();
        let db = rig.ctx.db().unwrap();
        let mut done = Quest::new("shipped", "/tmp/work", "laptop");
        done.state = QuestState::Finished;
        db.insert_quest(&done).unwrap();
        let mut app = rig.app();
        app.quests.show_finished = true;
        refresh(&rig.ctx, &mut app).unwrap();

        app.handle(Input::Char('c'));
        assert!(screen(&mut app).contains("already finished"));
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert!(app.status.contains("already finished"), "{}", app.status);
    }

    // --------------------------------------------------------------- resume

    #[test]
    fn resuming_a_finished_quest_brings_up_a_new_master_and_selects_it() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "come-back");
        let id = rig.quests()[0].id.clone();
        app.handle(Input::Char('c'));
        submit(&rig, &mut app);
        refresh(&rig.ctx, &mut app).unwrap();
        assert!(
            app.quests.rows.is_empty(),
            "the closed Quest is still listed"
        );

        // Reachable through `f`, which is the only way to select a finished
        // Quest (SPEC §17).
        handle(&mut app, Input::Char('f'));
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(app.quests.selected_row().unwrap().view.quest.id, id);

        app.handle(Input::Char('R'));
        set(&mut app, F_PROMPT, "pick up where you left off");
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));

        let quest = rig.quests().into_iter().find(|q| q.id == id).unwrap();
        assert_eq!(quest.state, QuestState::Active);
        let sessions = rig.ctx.db().unwrap().list_sessions_by_quest(&id).unwrap();
        assert_eq!(
            sessions.len(),
            2,
            "the old session row is history, not gone"
        );
        assert_eq!(
            sessions
                .iter()
                .filter(|s| s.status != SessionStatus::Ended)
                .count(),
            1
        );
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(app.quests.selected_row().unwrap().view.quest.id, id);
        assert!(app.status.contains("resumed"), "{}", app.status);
    }

    #[test]
    fn resuming_a_quest_that_is_still_running_keeps_the_form_up() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "still-going");
        app.handle(Input::Char('R'));
        submit(&rig, &mut app);

        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        let error = form.error().unwrap();
        assert!(error.contains("q enter still-going"), "{error}");
        assert!(!app.should_quit);
        // Nothing was spawned: still one live master.
        let quest = &rig.quests()[0];
        assert_eq!(
            rig.ctx
                .db()
                .unwrap()
                .list_sessions_by_quest(&quest.id)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(quest.state, QuestState::Active);
    }

    /// The prompt was opened against a Quest, not against a row number: a tick
    /// that reorders the listing while the box is up must not move the target.
    #[test]
    fn a_prompt_acts_on_the_quest_it_was_opened_against() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "aaa-target");
        make(&rig, &mut app, "zzz-other");
        let target = app.quests.selected_row().unwrap().view.quest.id.clone();

        app.handle(Input::Char('r'));
        // The list reloads and the selection is dragged elsewhere while the
        // box is up — exactly what a 2 s tick can do.
        refresh(&rig.ctx, &mut app).unwrap();
        app.quests
            .focus_on_id(&rig.quests().iter().find(|q| q.id != target).unwrap().id);
        assert_ne!(app.quests.selected_row().unwrap().view.quest.id, target);

        set(&mut app, F_SLUG, "renamed-target");
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let renamed = rig.quests().into_iter().find(|q| q.id == target).unwrap();
        assert_eq!(renamed.slug, "renamed-target");
    }

    /// A Quest that vanished while the prompt was up is an error, not a panic
    /// and not somebody else's Quest being closed.
    #[test]
    fn a_prompt_whose_quest_is_gone_reports_it() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "doomed-one");
        let id = rig.quests()[0].id.clone();
        app.handle(Input::Char('c'));
        rig.ctx.db().unwrap().delete_quest(&id).unwrap();
        submit(&rig, &mut app);
        let form = &app.modal.as_ref().unwrap().form;
        assert!(form.error().unwrap().contains("is gone"), "{form:?}");
    }

    // ------------------------------------------------------------ mode gate

    /// The rule the whole modal layer rests on: while a box is up the shell's
    /// bare-letter keys are text, not commands.
    #[test]
    fn the_shells_keys_are_text_while_a_form_is_up() {
        let rig = Rig::new();
        let mut app = rig.app();
        for (key, label) in [('n', F_GOAL), ('n', F_NAME)] {
            app.modal = None;
            app.handle(Input::Char(key));
            focus(&mut app, label);
            type_text(&mut app, "q x 1 2 3 4");
            assert_eq!(
                app.modal.as_ref().unwrap().form.trimmed(label),
                "q x 1 2 3 4"
            );
            assert!(!app.should_quit, "`q` quit from inside a field");
            assert_eq!(app.tab, Tab::Quests, "a digit switched tabs from a field");
            assert_eq!(app.refreshes, 0, "`x` refreshed from inside a field");
            app.handle(Input::Esc);
        }
        // `?` too: the help overlay would swallow the form underneath it.
        app.handle(Input::Char('n'));
        type_text(&mut app, "?");
        assert!(!app.help);
        assert_eq!(app.modal.as_ref().unwrap().form.trimmed(F_NAME), "?");
        // Ctrl-C is the one key that still gets through.
        assert_eq!(app.handle(Input::Ctrl('c')), Action::Quit);
    }

    /// A capture must never be armed with nothing on screen: the mouse can
    /// leave a form the keyboard cannot.
    #[test]
    fn a_click_on_another_tab_cannot_strand_a_form() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "half-typed");
        assert!(app.capturing());

        let (_, _, x, _) = crate::tui::app::tab_layout()[1];
        app.tab_bar_width = 120 - 8;
        app.handle_mouse(crate::tui::keys::MouseInput::Click { col: x, row: 0 });
        assert_eq!(app.tab, Tab::Sessions);
        assert!(
            app.modal.is_none(),
            "the form is holding an invisible keyboard"
        );
        assert!(!app.capturing());
        assert!(app.status.is_empty(), "{:?}", app.status);
        assert!(rig.quests().is_empty());

        // Back on Quests the shell's keys are the shell's again.
        app.handle_mouse(crate::tui::keys::MouseInput::Click {
            col: crate::tui::app::tab_layout()[0].2,
            row: 0,
        });
        assert_eq!(app.handle(Input::Char('q')), Action::Quit);
    }

    /// `capturing()` is what turns the shell's keys off, so it may only be
    /// true while there is a box on screen saying so.
    #[test]
    fn every_open_form_is_drawn_and_named_in_the_status_bar() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "some-quest");
        for (key, title) in [
            ('n', "new quest"),
            ('r', "rename some-quest"),
            ('c', "close some-quest?"),
            ('R', "resume some-quest"),
        ] {
            app.modal = None;
            app.handle(Input::Char(key));
            assert!(app.capturing(), "{key}");
            let text = screen(&mut app);
            assert!(text.contains(title), "{key}: {title} not drawn\n{text}");
            // And the bar says it too, for a terminal too short for the box.
            assert!(app.status.contains(title), "{key}: {:?}", app.status);
            assert!(text.contains("Esc cancel"), "{key}: no way out\n{text}");
        }
    }

    // ------------------------------------------------- beads, and B1's hole
    //
    // Everything below reaches `bd`. Before the client was injectable these
    // paths were unreachable in-crate — `beads::client()` read its fixture off
    // the process environment, which no in-crate test may touch — so every
    // submission here turned beads off, and the stderr writes on the other
    // side of them went unnoticed until a real terminal tore up.

    /// The Quest's epic (SPEC §5 step 2) is created through the same client
    /// the CLI uses, and stored on the row.
    #[test]
    fn the_new_quest_form_creates_the_beads_epic_by_default() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd::working("bd-e1"));
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();

        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "with-beads");
        set(&mut app, F_GOAL, "ship the thing");
        set(&mut app, F_DIR, &rig.dir());
        assert!(
            app.modal.as_ref().unwrap().form.is_on(F_BEADS),
            "the epic is on by default"
        );
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quest = &rig.quests()[0];
        assert_eq!(quest.beads_epic.as_deref(), Some("bd-e1"));
        let created = bd.created.lock().unwrap().clone();
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].0, "with-beads: ship the thing");
        assert!(created[0].1.contains(&format!("quest:{}", quest.id)));
        // Nothing to report, so nothing is appended to the bar.
        assert!(
            app.status.starts_with("created with-beads"),
            "{}",
            app.status
        );
    }

    /// B1. `n` with the epic on and a `bd` that will not answer. `q new` writes
    /// that warning to stderr; here the same call is running in raw mode on the
    /// alternate screen, where a write scrolls the pane and leaves ratatui
    /// painting over garbage it never repaints. So it comes back as data.
    #[test]
    fn a_bd_that_fails_a_create_reports_through_the_status_bar_not_the_screen() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd::failing("connection refused"));
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();

        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "no-tracker");
        set(&mut app, F_DIR, &rig.dir());
        submit(&rig, &mut app);

        // The Quest is made either way: a missing epic is a warning, not a
        // failed `q new` (SPEC §13).
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quest = &rig.quests()[0];
        assert_eq!(quest.slug, "no-tracker");
        assert_eq!(quest.beads_epic, None);

        // The warning reached the bar, and the frame.
        assert!(
            app.status.contains("warning: no beads epic"),
            "{}",
            app.status
        );
        assert!(app.status.contains("connection refused"), "{}", app.status);
        assert!(
            screen(&mut app).contains("warning: no beads epic"),
            "{}",
            screen(&mut app)
        );
        // And nothing was left buffered to surface against a later action.
        assert!(rig.ctx.take_warnings().is_empty());
    }

    /// The same, on the path where the create itself fails: the rollback closes
    /// the epic it had already minted, and a `bd` that refuses even that is the
    /// one record of an epic that outlived its Quest. It goes in the form, next
    /// to the error that kept the box up.
    #[test]
    fn a_rollback_that_cannot_close_its_epic_says_so_in_the_form() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd {
            create: Ok("bd-e9".to_string()),
            close: Err("bd is wedged".to_string()),
            listing: None,
            created: std::sync::Mutex::new(Vec::new()),
            closed: std::sync::Mutex::new(Vec::new()),
        });
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();
        rig.break_new_session("no server running on /tmp/tmux-501/default");

        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "half-made");
        set(&mut app, F_DIR, &rig.dir());
        submit(&rig, &mut app);

        let error = app
            .modal
            .as_ref()
            .expect("the form was thrown away")
            .form
            .error()
            .unwrap()
            .to_string();
        // The error leads, the warning follows it — on one line, in the box.
        assert!(error.contains("no server running"), "{error}");
        assert!(error.contains("bd close bd-e9"), "{error}");
        assert_eq!(bd.closed_ids(), ["bd-e9"]);
        assert!(rig.quests().is_empty());
        // N-5. The box is not a way around the 120-column cut: `form::render`
        // truncates every line to its own budget just as the status bar does,
        // so the actionable tail of this warning does NOT survive. The epic id
        // does, because it appears early — which is the only part that has to.
        let drawn = screen(&mut app);
        assert!(drawn.contains("no server running"), "{drawn}");
        assert!(drawn.contains("bd-e9"), "{drawn}");
        assert!(
            !drawn.contains("bd close bd-e9"),
            "the box no longer truncates; the claim it does can be dropped\n{drawn}"
        );
        assert!(rig.ctx.take_warnings().is_empty());
    }

    /// SPEC §13: `c` with `close beads epic` on closes the epic through the
    /// same `bd close` the CLI runs.
    #[test]
    fn closing_with_the_epic_toggle_on_closes_the_epic() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd::working("bd-e1"));
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();
        make(&rig, &mut app, "done-with-it");
        let id = rig.quests()[0].id.clone();
        rig.ctx
            .db()
            .unwrap()
            .update_quest(
                &id,
                &crate::db::quest::QuestPatch {
                    beads_epic: Some(Some("bd-e1".to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        refresh(&rig.ctx, &mut app).unwrap();

        app.handle(Input::Char('c'));
        focus(&mut app, F_CLOSE_EPIC);
        app.handle(Input::Char(' '));
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert_eq!(bd.closed_ids(), ["bd-e1"]);
        assert!(app.status.contains("epic bd-e1 closed"), "{}", app.status);
        assert!(rig.ctx.take_warnings().is_empty());
    }

    /// B1's second reproduction, the one where **nothing failed**: closing an
    /// already-closed epic a second time. `q close` prints a note; the TUI must
    /// not, and the note must still reach the user.
    #[test]
    fn a_second_close_of_the_same_epic_notes_it_in_the_bar_not_over_the_frame() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd::working("bd-e1"));
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();
        make(&rig, &mut app, "twice-closed");
        let id = rig.quests()[0].id.clone();
        let db = rig.ctx.db().unwrap();
        db.update_quest(
            &id,
            &crate::db::quest::QuestPatch {
                beads_epic: Some(Some("bd-e1".to_string())),
                ..Default::default()
            },
        )
        .unwrap();
        refresh(&rig.ctx, &mut app).unwrap();

        // First close, epic and all.
        app.handle(Input::Char('c'));
        focus(&mut app, F_CLOSE_EPIC);
        app.handle(Input::Char(' '));
        submit(&rig, &mut app);
        assert_eq!(bd.closed_ids(), ["bd-e1"]);

        // Second, on the finished Quest, asking for the epic again.
        app.quests.show_finished = true;
        refresh(&rig.ctx, &mut app).unwrap();
        app.quests.focus_on_id(&id);
        app.handle(Input::Char('c'));
        assert!(screen(&mut app).contains("already finished"));
        focus(&mut app, F_CLOSE_EPIC);
        app.handle(Input::Char(' '));
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert_eq!(
            bd.closed_ids(),
            ["bd-e1"],
            "the epic must not be closed a second time"
        );
        assert!(
            app.status.contains("already closed by an earlier"),
            "{}",
            app.status
        );
        // The bar is 120 columns wide, so only the head of the note fits.
        assert!(
            screen(&mut app).contains("note: beads epic"),
            "{}",
            screen(&mut app)
        );
        assert!(rig.ctx.take_warnings().is_empty());
    }

    /// A `bd close` that fails leaves the Quest closed and says what did not
    /// happen — as data, again.
    #[test]
    fn a_bd_that_refuses_to_close_the_epic_still_closes_the_quest() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd::failing("bd is wedged"));
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();
        make(&rig, &mut app, "epic-stuck");
        let id = rig.quests()[0].id.clone();
        rig.ctx
            .db()
            .unwrap()
            .update_quest(
                &id,
                &crate::db::quest::QuestPatch {
                    beads_epic: Some(Some("bd-e1".to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        refresh(&rig.ctx, &mut app).unwrap();

        app.handle(Input::Char('c'));
        focus(&mut app, F_CLOSE_EPIC);
        app.handle(Input::Char(' '));
        submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        assert_eq!(
            rig.quests()[0].state,
            QuestState::Finished,
            "the epic is not the Quest"
        );
        assert!(
            app.status.contains("`bd close bd-e1` failed"),
            "{}",
            app.status
        );
        assert!(rig.ctx.take_warnings().is_empty());
    }

    // -------------------------------------------- the affirmative is a choice

    /// B2. `q close` asks `[y/N]` and reads a bare Enter as *abort*. So does
    /// this box. A `c` and an Enter that arrived together — a burst buffered
    /// during a stall, a newline in a paste — must not end a Quest.
    #[test]
    fn a_bare_enter_on_the_close_prompt_closes_nothing() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "still-mine");
        let id = rig.quests()[0].id.clone();

        // Exactly the burst, through the loop's own dispatch: `c`, then
        // Enter, with no key in between.
        press(&rig, &mut app, Input::Char('c'));
        for _ in 0..3 {
            assert_eq!(
                press(&rig, &mut app, Input::Enter),
                Action::None,
                "Enter alone must not submit"
            );
        }
        assert!(app.modal.is_some(), "the box was taken down");

        let quest = rig.quests().into_iter().find(|q| q.id == id).unwrap();
        assert_eq!(quest.state, QuestState::Active);
        assert!(quest.finished_at.is_none());
        assert!(
            rig.fixture()
                .load()
                .unwrap()
                .panes
                .iter()
                .any(|p| p.session_name == "q-still-mine"),
            "the tmux session was killed by a keystroke nobody aimed"
        );
        // And the box says what is missing.
        let text = screen(&mut app);
        assert!(text.contains("nothing done"), "{text}");
        assert!(text.contains("close"), "{text}");
    }

    /// Chosen, it still closes — the box is a guard, not a wall.
    #[test]
    fn choosing_the_action_closes_the_quest() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "for-real");
        let id = rig.quests()[0].id.clone();

        app.handle(Input::Char('c'));
        assert_eq!(
            app.modal
                .as_ref()
                .unwrap()
                .form
                .choice(crate::tui::form::ACTION),
            crate::tui::form::CANCEL,
            "the action row starts on cancel"
        );
        // The focus starts on it, so it is one arrow key and an Enter away.
        assert_eq!(
            app.modal.as_ref().unwrap().form.focused().map(Field::label),
            Some(crate::tui::form::ACTION)
        );
        app.handle(Input::Right);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        crate::tui::submit(&rig.ctx, &mut app);

        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quest = rig.quests().into_iter().find(|q| q.id == id).unwrap();
        assert_eq!(quest.state, QuestState::Finished);
    }

    /// The paste vector. Bracketed paste is off, so pasted text arrives as
    /// ordinary keys: a space would cycle an ordinary select, and the newline
    /// after it would submit. The action row does not answer to Space.
    #[test]
    fn pasted_text_cannot_arm_a_destructive_prompt() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "paste-proof");

        app.handle(Input::Char('c'));
        for c in "fix the thing".chars() {
            app.handle(Input::Char(c));
        }
        assert!(
            !app.modal.as_ref().unwrap().form.confirmed(),
            "a space armed the close"
        );
        assert_eq!(app.handle(Input::Enter), Action::None);
        assert_eq!(rig.quests()[0].state, QuestState::Active);
    }

    /// The keys a terminal *without* bracketed paste hands over for these
    /// bytes. Only the two sequences the live demonstration used are modelled,
    /// because they are the whole attack: `ESC [ C` is CSI-C, which crossterm
    /// parses as `KeyCode::Right`, and `CR` is `KeyCode::Enter`.
    fn as_keys(bytes: &str) -> Vec<Event> {
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        let mut out = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            if let Some(tail) = rest.strip_prefix('\u{1b}') {
                match tail.strip_prefix("[C") {
                    Some(tail) => {
                        out.push(key(KeyCode::Right));
                        rest = tail;
                        continue;
                    }
                    None => {
                        out.push(key(KeyCode::Esc));
                        rest = tail;
                        continue;
                    }
                }
            }
            if let Some(tail) = rest.strip_prefix('\r') {
                out.push(key(KeyCode::Enter));
                rest = tail;
                continue;
            }
            let c = rest.chars().next().unwrap();
            out.push(key(KeyCode::Char(c)));
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// What the app actually receives when `bytes` are pasted into the
    /// terminal the TUI arms. The MODE decides, so this is not an assumption:
    /// with bracketed paste on the terminal wraps the paste and crossterm
    /// hands over one `Event::Paste`; with it off the same bytes go straight
    /// to the key parser.
    fn pasted(bytes: &str) -> Vec<Event> {
        if crate::tui::arms_bracketed_paste() {
            vec![Event::Paste(bytes.to_string())]
        } else {
            as_keys(bytes)
        }
    }

    /// Everything the event loop does with one crossterm event.
    fn deliver(rig: &Rig, app: &mut App, ev: Event) -> Action {
        let (action, _) = crate::tui::apply_event(app, ev);
        if action == Action::Submit {
            crate::tui::submit(&rig.ctx, app);
        }
        action
    }

    /// N-1, the demonstrated attack. `ESC [ C` parses as `Input::Right`, which
    /// walks the close box's guarded action row off `cancel`, and the `CR`
    /// behind it submits. Reproduced live against this branch with
    /// `tmux send-keys -l 'c'` then `tmux send-keys -H 1b 5b 43 0d`: the Quest
    /// went active -> finished and its tmux session was killed.
    ///
    /// Bracketed paste is what makes those bytes arrive as one `Event::Paste`
    /// instead — text, and the close prompt has no text field to put it in.
    #[test]
    fn a_pasted_csi_arrow_and_a_cr_cannot_close_a_quest() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "not-yours");
        let id = rig.quests()[0].id.clone();

        app.handle(Input::Char('c'));
        for ev in pasted("\u{1b}[C\r") {
            assert_eq!(deliver(&rig, &mut app, ev), Action::None, "a paste acted");
        }
        assert!(app.modal.is_some(), "a paste took the box down");
        assert_eq!(
            app.modal
                .as_ref()
                .unwrap()
                .form
                .choice(crate::tui::form::ACTION),
            crate::tui::form::CANCEL,
            "a pasted arrow armed the close"
        );
        // Nothing ran, and nothing is armed to run on the next Enter either.
        assert_eq!(press(&rig, &mut app, Input::Enter), Action::None);
        assert!(app.modal.is_some());

        let quest = rig.quests().into_iter().find(|q| q.id == id).unwrap();
        assert_eq!(quest.state, QuestState::Active);
        assert!(quest.finished_at.is_none());
        assert!(
            rig.fixture()
                .load()
                .unwrap()
                .panes
                .iter()
                .any(|p| p.session_name == "q-not-yours"),
            "the tmux session was killed by a paste"
        );
    }

    /// The same paste on the one Quest prompt that has a text field: the text
    /// lands in the field, and the escape and the `CR` inside it do not become
    /// keys.
    #[test]
    fn a_paste_into_a_prompt_is_literal_text_with_no_keys_in_it() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "resume-me");

        app.handle(Input::Char('R'));
        for ev in pasted("\u{1b}[Ckeep\rgoing") {
            assert_eq!(deliver(&rig, &mut app, ev), Action::None);
        }
        let form = &app.modal.as_ref().expect("the paste submitted it").form;
        assert_eq!(form.trimmed(F_PROMPT), "[Ckeepgoing");
        assert!(!form.confirmed(), "a pasted arrow armed the resume");
    }

    /// And the `/` box, the tab's own text field. Pasting with nothing
    /// capturing goes nowhere at all — a paste is not a way to open a box.
    #[test]
    fn a_paste_reaches_the_search_box_and_nothing_else() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "cdc-backfill");

        app.handle(Input::Char('/'));
        for ev in pasted("\u{1b}[Ccdc\r") {
            assert_eq!(deliver(&rig, &mut app, ev), Action::None);
        }
        assert_eq!(app.quests.query, "[Ccdc");

        app.handle(Input::Esc);
        assert!(!app.quests.capturing());
        let paste = Event::Paste("qqq".to_string());
        assert_eq!(
            crate::tui::apply_event(&mut app, paste),
            (Action::None, false)
        );
        assert!(!app.should_quit, "a pasted `q` quit the TUI");
        assert_eq!(app.quests.query, "");
    }

    /// `n` and `R` start processes, so they are guarded the same way — but
    /// their fields are still typed into from the first keystroke, which is
    /// why their action row is last and not first.
    #[test]
    fn the_prompts_that_start_a_process_are_guarded_too() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "one-quest");

        for key in ['n', 'R'] {
            app.modal = None;
            app.handle(Input::Char(key));
            assert!(
                !app.modal.as_ref().unwrap().form.confirmed(),
                "{key} is armed on arrival"
            );
            // Typing goes into the field, not into the action row.
            assert!(
                matches!(
                    app.modal.as_ref().unwrap().form.focused(),
                    Some(Field::Text { .. })
                ),
                "{key} does not start on a text field"
            );
            assert_eq!(app.handle(Input::Enter), Action::None, "{key}");
            assert!(app.modal.is_some(), "{key}");
            app.handle(Input::Esc);
        }
        // Nothing was created, nothing was resumed.
        assert_eq!(rig.slugs(), ["one-quest"]);
    }

    /// `r` has no action row: it destroys nothing, starts nothing, and a bare
    /// Enter re-submits the slug the Quest already has.
    #[test]
    fn rename_is_not_guarded_and_a_bare_enter_is_a_no_op() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "same-name");
        app.handle(Input::Char('r'));
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        crate::tui::submit(&rig.ctx, &mut app);
        assert_eq!(rig.slugs(), ["same-name"]);
    }

    // ------------------------------------------- the Quest under the prompt

    /// N1. An id can be minted twice. The prompt carries `created_at`, the one
    /// column nothing can change, so a Quest that was deleted and whose id was
    /// handed to a new one is "gone" rather than "closed".
    #[test]
    fn a_prompt_will_not_act_on_a_quest_that_reused_its_id() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "the-original");
        let id = rig.quests()[0].id.clone();
        let db = rig.ctx.db().unwrap();

        app.handle(Input::Char('c'));
        // `q rm` in another terminal, then a `q new` that draws the same id —
        // and, to leave `created_at` as the only difference, the same slug.
        db.delete_quest(&id).unwrap();
        let mut impostor = Quest::new("the-original", "/tmp/work", "laptop");
        impostor.id = id.clone();
        impostor.created_at += 60;
        db.insert_quest(&impostor).unwrap();

        submit(&rig, &mut app);
        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(form.error().unwrap().contains("is gone"), "{form:?}");
        assert_eq!(
            db.get_quest(&id).unwrap().unwrap().state,
            QuestState::Active,
            "the impostor was closed"
        );
    }

    /// N-6, N2's residual. The close box names the epic it is about to close,
    /// and `close --close-epic` closes whatever the *refetched* Quest carries.
    /// A `q set <slug> beads_epic <other>` underneath the open box would
    /// otherwise close an epic the box never mentioned.
    #[test]
    fn a_prompt_refuses_a_quest_whose_epic_changed_while_the_box_was_up() {
        let _guard = crate::beads::backoff::acquire();
        let bd = std::sync::Arc::new(crate::beads::stub::StubBd::working("bd-e1"));
        let rig = Rig::with_bd(Box::new(bd.clone()));
        let mut app = rig.app();
        make(&rig, &mut app, "swapped-epic");
        let id = rig.quests()[0].id.clone();
        let db = rig.ctx.db().unwrap();
        let patch = |epic: &str| crate::db::quest::QuestPatch {
            beads_epic: Some(Some(epic.to_string())),
            ..Default::default()
        };
        db.update_quest(&id, &patch("bd-e1")).unwrap();
        refresh(&rig.ctx, &mut app).unwrap();

        app.handle(Input::Char('c'));
        assert!(screen(&mut app).contains("epic bd-e1"));
        // Another terminal repoints the Quest at a different epic.
        db.update_quest(&id, &patch("bd-e2")).unwrap();
        focus(&mut app, F_CLOSE_EPIC);
        app.handle(Input::Char(' '));
        submit(&rig, &mut app);

        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(
            form.error().unwrap().contains("beads epic is bd-e2 now"),
            "{form:?}"
        );
        assert!(bd.closed_ids().is_empty(), "an unnamed epic was closed");
        assert_eq!(rig.quests()[0].state, QuestState::Active);
    }

    /// N2. The box named a slug and a tmux session. A rename underneath makes
    /// both a lie, so the submit refuses rather than acting on the new one.
    #[test]
    fn a_prompt_refuses_a_quest_renamed_while_the_box_was_up() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "old-name");
        let quest = rig.quests()[0].clone();

        app.handle(Input::Char('c'));
        assert!(screen(&mut app).contains("kills tmux q-old-name"));
        crate::commands::rename::apply(
            &rig.ctx,
            &quest,
            "new-name",
            crate::model::NameSource::Manual,
            None,
        )
        .unwrap();

        submit(&rig, &mut app);
        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(
            form.error().unwrap().contains("renamed to new-name"),
            "{form:?}"
        );
        assert_eq!(rig.quests()[0].state, QuestState::Active);
    }

    /// N2, the dangerous half: the box said "already finished — only the epic
    /// is left", somebody resumed the Quest from another terminal, and the
    /// full-close branch would kill the master they just started.
    #[test]
    fn a_close_refuses_a_quest_that_was_resumed_while_the_box_was_up() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "back-again");
        let id = rig.quests()[0].id.clone();
        // Close it, then open the prompt against the finished Quest.
        app.handle(Input::Char('c'));
        submit(&rig, &mut app);
        app.quests.show_finished = true;
        refresh(&rig.ctx, &mut app).unwrap();
        app.quests.focus_on_id(&id);
        app.handle(Input::Char('c'));
        assert!(screen(&mut app).contains("already finished"));

        // Another terminal brings it back.
        let quest = rig.quests().into_iter().find(|q| q.id == id).unwrap();
        crate::commands::resume::apply(&rig.ctx, &quest, None).unwrap();
        let live = rig
            .ctx
            .db()
            .unwrap()
            .list_sessions_by_quest(&id)
            .unwrap()
            .iter()
            .filter(|s| s.status != SessionStatus::Ended)
            .count();
        assert_eq!(live, 1);

        submit(&rig, &mut app);
        let form = &app.modal.as_ref().expect("the form was thrown away").form;
        assert!(form.error().unwrap().contains("is running now"), "{form:?}");
        assert_eq!(
            rig.quests().into_iter().find(|q| q.id == id).unwrap().state,
            QuestState::Active,
            "the freshly resumed master was killed"
        );
    }

    /// N9. The id-carrying claim, for `close` — the prompt where acting on the
    /// wrong Quest is not recoverable. (`rename` is covered above.)
    #[test]
    fn a_close_acts_on_the_quest_it_was_opened_against() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "aaa-doomed");
        make(&rig, &mut app, "zzz-bystander");
        let target = app.quests.selected_row().unwrap().view.quest.id.clone();

        app.handle(Input::Char('c'));
        // A tick reloads and the selection is dragged elsewhere under the box.
        refresh(&rig.ctx, &mut app).unwrap();
        let other = rig
            .quests()
            .iter()
            .find(|q| q.id != target)
            .unwrap()
            .id
            .clone();
        app.quests.focus_on_id(&other);
        assert_ne!(app.quests.selected_row().unwrap().view.quest.id, target);

        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));
        let quests = rig.quests();
        let closed = quests.iter().find(|q| q.id == target).unwrap();
        let bystander = quests.iter().find(|q| q.id == other).unwrap();
        assert_eq!(closed.state, QuestState::Finished);
        assert_eq!(bystander.state, QuestState::Active);
    }

    // --------------------------------------------------------- the template

    /// N7. SPEC §11: the Quest records the template it came from, and the note
    /// says what a template actually brings — including the master's first
    /// prompt, which is not a cosmetic default.
    #[test]
    fn a_templated_quest_records_its_template_and_takes_its_prompt() {
        let rig = Rig::new();
        let mut template = crate::model::Template::new("weekly-hygiene");
        template.goal = Some("tidy up".to_string());
        template.cwd = Some(rig.dir());
        template.master_prompt = Some("start with the backlog".to_string());
        rig.ctx.db().unwrap().insert_template(&template).unwrap();

        let mut app = rig.app();
        app.handle(Input::Char('n'));
        let text = screen(&mut app);
        assert!(text.contains("master's first prompt"), "{text}");
        assert!(text.contains("beads repo"), "{text}");

        set(&mut app, F_NAME, "from-template");
        focus(&mut app, F_TEMPLATE);
        app.handle(Input::Right);
        no_beads(&mut app);
        submit(&rig, &mut app);
        assert!(app.modal.is_none(), "{}", screen(&mut app));

        let quest = &rig.quests()[0];
        assert_eq!(quest.template_id.as_deref(), Some(template.id.as_str()));
        assert_eq!(quest.goal.as_deref(), Some("tidy up"));
        // The prompt went to the master, which is what the note now promises.
        let pane = rig
            .fixture()
            .load()
            .unwrap()
            .panes
            .into_iter()
            .find(|p| p.session_name == "q-from-template")
            .unwrap();
        assert!(
            pane.command
                .as_deref()
                .is_some_and(|c| c.contains("start with the backlog")),
            "{:?}",
            pane.command
        );
    }

    /// bd-8lz.6.1: the form does the run bookkeeping `q tpl run` does, and
    /// expands the one placeholder it can — `{{date}}`.
    #[test]
    fn a_templated_quest_expands_the_date_and_counts_the_run() {
        let rig = Rig::new();
        let mut template = crate::model::Template::new("weekly-hygiene");
        template.goal = Some("tidy up on {{date}}".to_string());
        template.cwd = Some(rig.dir());
        rig.ctx.db().unwrap().insert_template(&template).unwrap();

        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "from-template");
        focus(&mut app, F_TEMPLATE);
        app.handle(Input::Right);
        no_beads(&mut app);
        // Read on both sides of the submit: computing "today" only afterwards
        // is a once-a-year midnight flake.
        let before = crate::templates::today();
        submit(&rig, &mut app);
        let after = crate::templates::today();
        assert!(app.modal.is_none(), "{}", screen(&mut app));

        let goal = rig.quests()[0].goal.clone().unwrap_or_default();
        assert!(
            goal == format!("tidy up on {before}") || goal == format!("tidy up on {after}"),
            "{goal}"
        );
        let stored = rig
            .ctx
            .db()
            .unwrap()
            .get_template(&template.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.run_count, 1);
        assert!(stored.last_run_at.is_some());
    }

    /// A form has nowhere to type `--arg`, so a template that wants one is
    /// refused rather than instantiated with the braces still in it.
    #[test]
    fn a_template_that_needs_an_argument_is_refused_by_the_form() {
        let rig = Rig::new();
        let mut template = crate::model::Template::new("weekly-hygiene");
        template.goal = Some("tidy {{arg.repo}}".to_string());
        template.cwd = Some(rig.dir());
        rig.ctx.db().unwrap().insert_template(&template).unwrap();

        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "from-template");
        focus(&mut app, F_TEMPLATE);
        app.handle(Input::Right);
        no_beads(&mut app);
        submit(&rig, &mut app);

        let text = screen(&mut app);
        assert!(app.modal.is_some(), "the form closed: {text}");
        assert!(text.contains("q tpl run"), "{text}");
        assert!(rig.quests().is_empty(), "a quest was created: {text}");
        let stored = rig
            .ctx
            .db()
            .unwrap()
            .get_template(&template.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.run_count, 0);
    }

    // ----------------------------------------------------------- the mouse

    /// N5. A wheel nudge over an open box would step its focus with nothing
    /// about the gesture saying a field had changed — onto `close beads epic`,
    /// or off the action row.
    #[test]
    fn the_wheel_does_not_move_focus_inside_a_form() {
        let rig = Rig::new();
        let mut app = rig.app();
        make(&rig, &mut app, "wheel-proof");
        // Both shapes: the many-field form, and a close prompt whose epic
        // toggle a nudge could land on without the user noticing.
        rig.ctx
            .db()
            .unwrap()
            .update_quest(
                &rig.quests()[0].id.clone(),
                &crate::db::quest::QuestPatch {
                    beads_epic: Some(Some("bd-e1".to_string())),
                    ..Default::default()
                },
            )
            .unwrap();
        let _guard = crate::beads::backoff::acquire();
        refresh(&rig.ctx, &mut app).unwrap();

        for key in ['n', 'c'] {
            app.modal = None;
            app.handle(Input::Char(key));
            let before = app.modal.as_ref().unwrap().form.clone();
            for wheel in [
                crate::tui::keys::MouseInput::ScrollDown,
                crate::tui::keys::MouseInput::ScrollUp,
            ] {
                for _ in 0..3 {
                    assert_eq!(app.handle_mouse(wheel), Action::None, "{key}");
                    assert_eq!(&before, &app.modal.as_ref().unwrap().form, "{key}");
                }
            }
            assert!(!app.modal.as_ref().unwrap().form.confirmed(), "{key}");
        }
    }

    /// A tick does not expire the box's own line, and does not take the box
    /// down either.
    #[test]
    fn a_tick_leaves_an_open_form_alone() {
        let rig = Rig::new();
        let mut app = rig.app();
        app.handle(Input::Char('n'));
        set(&mut app, F_NAME, "surviving");
        for _ in 0..50 {
            app.tick();
        }
        refresh(&rig.ctx, &mut app).unwrap();
        crate::tui::report_refresh(&mut app, Ok(()));
        assert!(app.capturing());
        assert_eq!(
            app.modal.as_ref().unwrap().form.trimmed(F_NAME),
            "surviving"
        );
        assert!(screen(&mut app).contains("surviving"));
    }
}
