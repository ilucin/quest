//! Sessions tab (SPEC §17): the fleet — every Quest's sessions on one screen,
//! waiting first, with the context bar of SPEC §8 and the four keys that act
//! on a live agent.
//!
//! The listing itself is not computed here — [`crate::commands::sessions::load`]
//! is the one definition of "the session listing", shared with `q sessions`, so
//! the CLI and the TUI can never disagree about what is running. Nothing that
//! acts is implemented here either: `⏎` resolves through [`enter::resolve`],
//! `p` through [`peek::capture`], `t` through [`send::apply`], `k` through
//! [`kill::apply`] and `Z` through [`reset::spawn_detached`] — the same entry
//! points the CLI uses.
//!
//! [`enter::resolve`]: crate::commands::enter::resolve
//! [`peek::capture`]: crate::commands::peek::capture

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::Ctx;
use crate::commands::new::MASTER;
use crate::commands::sessions::SessionView;
use crate::commands::{fmt, kill, reset, send, sessions as listing, target as resolve_target};
use crate::config::Config;
use crate::error::QError;
use crate::model::{SessionRole, SessionStatus};

use super::app::{Action, App, Prompt, SessionTarget};
use super::form::Form;
use super::keys::Input;
use super::layout;

/// The tab's own half of the `?` overlay.
pub const HELP: &[(&str, &str)] = &[
    ("Enter / o", "attach to exactly this window"),
    ("p", "peek at the pane (a pager)"),
    ("t", "send text (a form; idle-gated)"),
    ("k", "kill this worker (prompts) — so j/k do not move here"),
    ("Z", "reset its context window (prompts)"),
    ("a", "show ended sessions"),
    ("Esc", "clear the quest filter"),
    ("g / G", "first / last row"),
];

/// Groups a listing can be split into: waiting, busy, starting, idle, ended.
const GROUPS: usize = 5;
/// Columns a phase line may occupy before it is cut.
const PHASE_COLS: usize = 28;
/// The same for a status cell, which carries `waiting: <what for>`.
const STATUS_COLS: usize = 22;
/// And for the Quest slug, which is only bounded by SPEC §10's 40.
const QUEST_COLS: usize = 20;

// The form field labels. Constants because the openers write them and
// [`submit`] reads them back; a typo in either would silently mean "blank".
const F_TEXT: &str = "text";
const F_FORCE: &str = "force";

/// Per-tab state, owned by `App`.
pub struct State {
    /// The fleet as last loaded: already swept, annotated and ranked.
    rows: Vec<SessionView>,
    /// Index into the *visible* rows.
    selected: usize,
    /// The selected session's id, so a reload that reorders keeps the
    /// selection on the same agent rather than on the same line.
    selected_id: Option<String>,
    /// First row drawn — moved only when the selection would leave the view.
    offset: usize,
    /// `s` on the Quests tab (SPEC §17): show only this Quest's sessions.
    /// Held by id; the slug is re-derived on every reload so a rename in
    /// another terminal cannot leave the chip lying.
    quest: Option<String>,
    quest_slug: Option<String>,
    /// `a`: ended sessions are hidden by default — the fleet view is about
    /// what is running now.
    show_ended: bool,
    /// `[context] worker_warn_pct` (SPEC §8): the reading at which a worker's
    /// context bar starts saying so. Workers only — a master over its own
    /// threshold is auto-reset, and does not need a warning.
    warn_pct: u8,
    /// `[context] reset_strategy`, so the `Z` box can name what it will type.
    strategy: &'static str,
    /// Whether [`refresh`] has ever run for this tab. Empty `rows` alone
    /// cannot say: "the fleet is idle" and "nobody has asked yet" look
    /// identical, and only one of them is honest to draw.
    loaded: bool,
}

impl State {
    pub fn new(config: &Config) -> State {
        State {
            rows: Vec::new(),
            selected: 0,
            selected_id: None,
            offset: 0,
            quest: None,
            quest_slug: None,
            show_ended: false,
            warn_pct: config.context.worker_warn_pct,
            strategy: reset::Strategy::from_config(config).as_str(),
            loaded: false,
        }
    }

    /// Whether this tab has ever been loaded. A tab that has not must reload
    /// before it is drawn: until then its empty listing is a claim about the
    /// fleet made without having looked.
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// The filters currently hiding (or revealing) rows, for the chrome line.
    pub fn filters(&self) -> String {
        let mut on: Vec<String> = Vec::new();
        if let Some(quest) = self.quest.as_deref() {
            on.push(format!(
                "quest {}",
                self.quest_slug.as_deref().unwrap_or(quest)
            ));
        }
        if self.show_ended {
            on.push("+ended".to_string());
        }
        on.join(" ")
    }

    /// The rows actually on screen, after the quest filter and `a`.
    fn visible(&self) -> Vec<usize> {
        (0..self.rows.len())
            .filter(|i| self.passes(&self.rows[*i]))
            .collect()
    }

    fn passes(&self, view: &SessionView) -> bool {
        if self
            .quest
            .as_deref()
            .is_some_and(|q| q != view.session.quest_id)
        {
            return false;
        }
        // Belt and braces: the loader already drops them, but `a` flips this
        // before the reload that acts on it.
        self.show_ended || view.session.status != SessionStatus::Ended
    }

    fn selected_row(&self) -> Option<&SessionView> {
        let visible = self.visible();
        visible.get(self.selected).map(|i| &self.rows[*i])
    }

    /// Keep the selection on the session it was on; fall back to clamping the
    /// index when that session is gone or filtered away.
    fn resync(&mut self) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected = 0;
            self.selected_id = None;
            self.offset = 0;
            return;
        }
        if let Some(id) = self.selected_id.as_deref()
            && let Some(at) = visible.iter().position(|i| self.rows[*i].session.id == id)
        {
            self.selected = at;
        } else {
            self.selected = self.selected.min(visible.len() - 1);
        }
        self.selected_id = Some(self.rows[visible[self.selected]].session.id.clone());
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
        self.selected_id = visible
            .get(self.selected)
            .map(|i| self.rows[*i].session.id.clone());
        let viewport = viewport.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + viewport {
            self.offset = self.selected + 1 - viewport;
        }
        // Both branches above only ever push `offset` FORWARD, so a viewport
        // that GREW since the last frame would leave the body half empty with
        // rows stranded above the fold. Pulling back to the last full screen
        // is what heals it. `visible.len()` counts rows and `viewport` is
        // already net of the group headers, so this errs towards showing MORE
        // than fits — which the renderer then cuts — never towards a gap.
        self.offset = self.offset.min(visible.len().saturating_sub(viewport));
    }
}

impl Default for State {
    fn default() -> State {
        State::new(&Config::default())
    }
}

// ------------------------------------------------------------------ loading

/// Reload this tab's data. Called by the event loop on tick and on `x`, never
/// from the state machine, so `App::handle` stays pure.
pub fn refresh(ctx: &Ctx, app: &mut App) -> anyhow::Result<()> {
    // `s` on the Quests tab hands its selection over (SPEC §17). Consumed
    // rather than read, so the hand-off happens once and `Esc` can clear the
    // filter for good afterwards.
    if let Some(quest) = app.focus_quest.take() {
        app.sessions.quest = Some(quest);
        app.sessions.quest_slug = None;
        // The anchor belongs to whatever was selected before the filter; the
        // filtered listing is a different set of rows.
        app.sessions.selected_id = None;
        app.sessions.offset = 0;
    }

    let mut rows = listing::load(
        ctx,
        &listing::Args {
            quest: None,
            all: app.sessions.show_ended,
        },
    )?;
    sort_fleet(&mut rows);

    // Re-derived every reload rather than remembered: a `q rename` in another
    // terminal would otherwise leave the chip naming a slug that is gone.
    app.sessions.quest_slug = match app.sessions.quest.as_deref() {
        Some(id) => ctx.db()?.get_quest(id)?.map(|q| q.slug),
        None => None,
    };
    app.sessions.rows = rows;
    app.sessions.loaded = true;
    app.sessions.resync();
    settle_view(app);
    Ok(())
}

/// SPEC §17: waiting on top. Then what is running, what is still coming up,
/// what is between turns, and what is over — ties to the most recently touched
/// session, and to the Quest and label after that so the order never depends
/// on the random ids.
pub fn sort_fleet(rows: &mut [SessionView]) {
    rows.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then(b.session.updated_at.cmp(&a.session.updated_at))
            .then(a.quest_slug.cmp(&b.quest_slug))
            .then(a.session.label.cmp(&b.session.label))
    });
}

/// The group a session belongs to, and the order the groups are shown in.
pub fn rank(view: &SessionView) -> u8 {
    match view.session.status {
        SessionStatus::Waiting => 0,
        SessionStatus::Busy => 1,
        SessionStatus::Starting => 2,
        SessionStatus::Idle => 3,
        SessionStatus::Ended => 4,
    }
}

fn group_title(rank: u8) -> &'static str {
    match rank {
        0 => "waiting",
        1 => "busy",
        2 => "starting",
        3 => "idle",
        _ => "ended",
    }
}

/// Scroll the selection back into view for the current terminal size.
pub fn settle_view(app: &mut App) {
    let page = viewport(app);
    app.sessions.settle(page);
}

/// How many rows the body can show. The group headers cost a line each, so the
/// listing's *own* headers are reserved rather than all [`GROUPS`] of them: a
/// fleet that is entirely idle costs one header, not five, and on a short
/// terminal the difference is most of the screen.
fn viewport(app: &App) -> usize {
    let body = app.height.saturating_sub(2) as usize;
    let state = &app.sessions;
    let mut seen = [false; GROUPS];
    for i in state.visible() {
        seen[rank(&state.rows[i]) as usize] = true;
    }
    body.saturating_sub(seen.iter().filter(|s| **s).count())
        .max(1)
}

// ------------------------------------------------------------------- keymap

/// Keys the shell did not claim. Pure: anything needing the terminal or a
/// subprocess leaves through an `Action`, never from in here.
///
/// `j`/`k` are deliberately unbound: SPEC §17 gives `k` to *kill* on this tab,
/// and half a vim keymap — `j` moving while `k` ends an agent — is worse than
/// none.
pub fn handle(app: &mut App, input: Input) -> Action {
    let page = viewport(app);
    match input {
        Input::Up => {
            app.sessions.move_by(-1, page);
            Action::None
        }
        Input::Down => {
            app.sessions.move_by(1, page);
            Action::None
        }
        Input::PageUp => {
            app.sessions.move_by(-(page as isize), page);
            Action::None
        }
        Input::PageDown => {
            app.sessions.move_by(page as isize, page);
            Action::None
        }
        Input::Home | Input::Char('g') => {
            app.sessions.move_to(0, page);
            Action::None
        }
        Input::End | Input::Char('G') => {
            app.sessions.move_to(usize::MAX, page);
            Action::None
        }
        // SPEC §17: "`⏎` attach na točno taj window". `o` is the same key it
        // is on the Quests tab, so a hand that learned one keeps it.
        Input::Enter | Input::Char('o') => act_on_selection(app, Action::Attach),
        Input::Char('p') => act_on_selection(app, Action::Peek),
        Input::Char('t') => open_send(app),
        Input::Char('k') => open_kill(app),
        Input::Char('Z') => open_reset(app),
        Input::Char('a') => {
            app.sessions.show_ended = !app.sessions.show_ended;
            app.sessions.resync();
            app.say(if app.sessions.show_ended {
                "showing ended sessions"
            } else {
                "ended sessions hidden"
            });
            // The ended rows were not fetched, so this needs a reload.
            Action::Refresh
        }
        Input::Esc => {
            if app.sessions.quest.take().is_some() {
                app.sessions.quest_slug = None;
                // `selected` indexes the *visible* rows: dropping the filter
                // widens them under it, so the selection has to be re-anchored
                // on its own session.
                app.sessions.resync();
                app.say("all quests");
            }
            Action::None
        }
        _ => Action::None,
    }
}

/// The two keys the loop performs: nothing happens with an empty listing, and
/// the loop re-asks for the selection rather than being handed it.
fn act_on_selection(app: &mut App, action: Action) -> Action {
    match app.sessions.selected_row() {
        Some(_) => action,
        None => Action::None,
    }
}

/// What the Sessions tab's `⏎` and `p` act on: enough to re-resolve the row
/// against the database, and what to call it in a message.
pub struct Selection {
    pub session: String,
    pub quest: String,
    pub label: String,
    pub name: String,
}

/// The selected session, cloned so the borrow of `app` ends with the lookup.
pub fn selected(app: &App) -> Option<Selection> {
    let view = app.sessions.selected_row()?;
    Some(Selection {
        session: view.session.id.clone(),
        quest: view.session.quest_id.clone(),
        label: view.session.label.clone(),
        name: name_of(view),
    })
}

fn name_of(view: &SessionView) -> String {
    format!("{}/{}", view.quest_slug, view.session.label)
}

// --------------------------------------------------------------- the forms

/// What a prompt records about the session it was opened against; see
/// [`SessionTarget`] for why each field is in there.
fn target_of(view: &SessionView) -> SessionTarget {
    SessionTarget {
        session: view.session.id.clone(),
        quest: view.session.quest_id.clone(),
        pane: view.session.tmux_pane.clone(),
        started_at: view.session.started_at,
        name: name_of(view),
        ended: view.session.status == SessionStatus::Ended,
    }
}

/// Why `q send` / `q reset` would refuse this row *as the listing has it* —
/// the same shape the real gate takes, without the registry read that gate
/// does (`handle` is pure). The real gate runs again at submit.
fn not_idle(view: &SessionView) -> Option<String> {
    match view.session.status {
        SessionStatus::Idle => view
            .registry
            .as_deref()
            .map(|said| format!("claude's own registry says {said}")),
        _ => Some(format!(
            "q has it as {}",
            listing::status_cell(&view.session)
        )),
    }
}

/// A row a `q spawn` inserted before its window opened carries no pane, and
/// tmux reads an empty `-t` target as "whatever is current" — q's own window,
/// when q runs inside tmux. The sweep ends such a row a few seconds in; until
/// then none of the three boxes may open on it, because each of them would say
/// "pane " with nothing after it and then act on the wrong window.
fn no_pane(view: &SessionView) -> bool {
    view.session.tmux_pane.is_empty()
}

/// `t` — SPEC §17. Typing into a live Claude session is destructive when
/// mistimed (SPEC §23 #5), so this is an `action` prompt and the idle gate is
/// on by default: `force` is a separate, explicit switch, exactly as
/// `q send --force` is.
fn open_send(app: &mut App) -> Action {
    let Some(view) = app.sessions.selected_row() else {
        return Action::None;
    };
    if view.session.status == SessionStatus::Ended {
        let name = name_of(view);
        app.say(format!("{name} has ended; there is no pane to type into"));
        return Action::None;
    }
    if no_pane(view) {
        let name = name_of(view);
        app.say(format!(
            "{name} has no pane yet; it never finished starting"
        ));
        return Action::None;
    }
    let gate = not_idle(view);
    let target = target_of(view);
    let mut form = Form::new(format!("send to {}", target.name))
        .hint("Tab field \u{b7} \u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .text(F_TEXT, "", "");
    form = match &gate {
        Some(why) => form.note(format!("{why} \u{b7} sending anyway needs force")),
        None => form.note("between turns \u{b7} the text will land as a prompt"),
    };
    app.open(
        Prompt::Send(target),
        form.toggle(F_FORCE, false)
            .note(
                "unforced, a send is refused unless the session is idle: text typed \
                 mid-turn is swallowed, and text typed at a permission prompt answers it",
            )
            .action("send"),
    );
    Action::None
}

/// `k` — SPEC §17. The master is refused up front rather than at submit: the
/// box would have nothing to offer but a failure.
fn open_kill(app: &mut App) -> Action {
    let Some(view) = app.sessions.selected_row() else {
        return Action::None;
    };
    if view.session.role == SessionRole::Master || view.session.label == MASTER {
        let (name, slug) = (name_of(view), view.quest_slug.clone());
        app.say(format!(
            "{name} is the master of {slug}; close the whole Quest from the Quests tab"
        ));
        return Action::None;
    }
    if view.session.status == SessionStatus::Ended {
        let name = name_of(view);
        app.say(format!("{name} has already ended"));
        return Action::None;
    }
    if no_pane(view) {
        let name = name_of(view);
        app.say(format!(
            "{name} has no window yet; it never finished starting"
        ));
        return Action::None;
    }
    let pane = view.session.tmux_pane.clone();
    let target = target_of(view);
    let form = Form::new(format!("kill {}?", target.name))
        .hint("\u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .action("kill")
        .note(format!("kills the tmux window of pane {pane}"))
        .note("whatever it was doing is lost; the row stays as history");
    app.open(Prompt::Kill(target), form);
    Action::None
}

/// `Z` — SPEC §17, the manual half of SPEC §8's context reset.
fn open_reset(app: &mut App) -> Action {
    let Some(view) = app.sessions.selected_row() else {
        return Action::None;
    };
    if view.session.status == SessionStatus::Ended {
        let name = name_of(view);
        app.say(format!("{name} has ended; there is no context to reset"));
        return Action::None;
    }
    if no_pane(view) {
        let name = name_of(view);
        app.say(format!(
            "{name} has no pane yet; it never finished starting"
        ));
        return Action::None;
    }
    let gate = not_idle(view);
    let strategy = app.sessions.strategy;
    let pane = view.session.tmux_pane.clone();
    let ctx_pct = view
        .session
        .ctx_pct
        .map(|p| format!("{p}%"))
        .unwrap_or_else(|| "unknown".to_string());
    let target = target_of(view);
    let mut form = Form::new(format!("reset {}?", target.name))
        .hint("\u{2190}\u{2192} chooses \u{b7} \u{23ce} runs the action \u{b7} Esc cancels")
        .action("reset")
        .note(format!(
            "types /{strategy} into pane {pane}, then the follow-up prompt (ctx {ctx_pct})"
        ))
        .note("the session loses its context window; the fresh one comes up on the brief");
    if let Some(why) = &gate {
        form = form.note(format!("{why} \u{b7} a reset of a busy session is refused"));
    }
    app.open(Prompt::Reset(target), form);
    Action::None
}

// ------------------------------------------------------------- running them

/// Run the open form. Called by the event loop, never from `handle`: every one
/// of these types into a live agent or kills a tmux window.
pub fn submit(ctx: &Ctx, app: &mut App, prompt: &Prompt, form: &Form) -> anyhow::Result<()> {
    match prompt {
        Prompt::Send(target) => send_text(ctx, app, target, form),
        Prompt::Kill(target) => kill_session(ctx, app, target),
        Prompt::Reset(target) => reset_session(ctx, app, target),
        // A Quest prompt never reaches here; `tui::submit` dispatches on the
        // variant and this arm exists only so the match is total.
        _ => Ok(()),
    }
}

/// The session the box was opened against, re-read now — and every promise the
/// box made about it re-checked.
///
/// SPEC §6 makes the tmux pane the session's identity, and the pane is what
/// `kill`, `send` and `reset` all act on, so the pane is the check that
/// matters. It is not enough on its own: `%N` restarts from `%0` with the tmux
/// server, so the row is checked too — by id *and* `started_at`, because
/// `new_id` is 16 bits and its retry only looks at live rows. The Quest, the
/// name the box printed, and whether the session had ended are checked because
/// the box said all three out loud.
fn session_for(ctx: &Ctx, target: &SessionTarget) -> anyhow::Result<resolve_target::Target> {
    // The sweep first: a pane that vanished while the box was up has to show
    // up here as `ended`, not as a live row we then type into.
    let _ = crate::commands::sweep_quiet(ctx);
    let db = ctx.db()?;
    let session = db
        .get_session(&target.session)?
        .filter(|s| s.started_at == target.started_at && s.quest_id == target.quest)
        .ok_or_else(|| {
            QError::NotFound(format!(
                "session {} ({}) is gone",
                target.name, target.session
            ))
        })?;
    let quest = db.get_quest(&target.quest)?.ok_or_else(|| {
        QError::NotFound(format!(
            "quest of {} ({}) is gone",
            target.name, target.quest
        ))
    })?;
    // Before the comparison, not after: an empty pane equals an empty pane, so
    // a row that never opened a window passes the pane check against a target
    // captured from itself. tmux reads an empty `-t` as "whatever is current",
    // and inside tmux that is q's own window — `k` would kill it, `p` would
    // page it, `t --force` would type into it. The commands refuse this too;
    // saying it here keeps the box honest about which row it is.
    if session.tmux_pane.is_empty() || target.pane.is_empty() {
        return Err(QError::Invalid(format!(
            "{} has no pane; it never finished starting",
            target.name
        ))
        .into());
    }
    if session.tmux_pane != target.pane {
        return Err(QError::Invalid(format!(
            "{} is pane {} now, not {} as this box says; Esc and try again",
            target.name, session.tmux_pane, target.pane
        ))
        .into());
    }
    let now = format!("{}/{}", quest.slug, session.label);
    if now != target.name {
        return Err(QError::Invalid(format!(
            "{} is called {now} now; Esc and try again",
            target.name
        ))
        .into());
    }
    let ended = session.status == SessionStatus::Ended;
    if ended != target.ended {
        let state = if ended { "has ended" } else { "is live again" };
        return Err(QError::Invalid(format!(
            "{now} {state} now, which is not what this box says; Esc and try again"
        ))
        .into());
    }
    Ok(resolve_target::Target { quest, session })
}

fn send_text(ctx: &Ctx, app: &mut App, target: &SessionTarget, form: &Form) -> anyhow::Result<()> {
    let found = session_for(ctx, target)?;
    let sent = send::apply(ctx, &found, form.trimmed(F_TEXT), form.is_on(F_FORCE))?;
    app.say(sent.describe());
    Ok(())
}

fn kill_session(ctx: &Ctx, app: &mut App, target: &SessionTarget) -> anyhow::Result<()> {
    let found = session_for(ctx, target)?;
    let killed = kill::apply(ctx, &found)?;
    // The selection is deliberately not moved: with `a` off the row drops out
    // of the listing and `resync` clamps onto whatever took its place.
    app.say(killed.describe());
    Ok(())
}

/// `Z` hands the work to a detached `q reset` — the mechanism SPEC §8 already
/// specifies for the `Stop` hook — because the synchronous path waits for the
/// fresh brief (up to three minutes on `/compact`) and the event loop cannot
/// block for that. The idle gate is taken here, in front of the user, so a
/// refusal is visible rather than buried in a process nobody is watching.
fn reset_session(ctx: &Ctx, app: &mut App, target: &SessionTarget) -> anyhow::Result<()> {
    let found = session_for(ctx, target)?;
    let strategy = reset::Strategy::from_config(&ctx.config);
    reset::spawn_detached(ctx, &found, strategy)?;
    app.say(format!(
        "resetting {} via /{} \u{b7} watch the events tab",
        found.name(),
        strategy.as_str()
    ));
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
    let state = &app.sessions;
    let visible = state.visible();
    if visible.is_empty() {
        frame.render_widget(Paragraph::new(empty_lines(state)), inset(area));
        return;
    }

    // The Quest is only worth a column when more than one can show up — the
    // same rule `q sessions` follows.
    let across = state.quest.is_none();
    let cells: Vec<Cells> = visible
        .iter()
        .map(|i| cells_of(&state.rows[*i], state.warn_pct))
        .collect();
    let widths = widths_of(&cells, across);

    let width = area.width as usize;
    let capacity = (area.height as usize).max(1);
    let mut lines: Vec<Line> = Vec::new();
    let mut group: Option<u8> = None;
    for (n, i) in visible.iter().enumerate().skip(state.offset) {
        let left = capacity - lines.len();
        if left == 0 {
            break;
        }
        let view = &state.rows[*i];
        let at = rank(view);
        // A heading and its first row go on screen together or not at all:
        // pushing the pair and truncating afterwards left a header with
        // nothing beneath it, and dropping only the header put the row under
        // the PREVIOUS group's heading. With one line left the whole group is
        // held over — `settle` has already fitted the selected row above here.
        // The exception is a one-line body, where the row is all there is room
        // for and no heading is above it to mislabel it.
        if group != Some(at) {
            if left < 2 {
                if !lines.is_empty() {
                    break;
                }
            } else {
                lines.push(Line::from(
                    Span::raw(layout::truncate(group_title(at), width)).dim(),
                ));
            }
        }
        group = Some(at);
        lines.push(row_line(
            &cells[n],
            &widths,
            across,
            n == state.selected,
            width,
        ));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn empty_lines(state: &State) -> Vec<Line<'static>> {
    let why = if !state.rows.is_empty() {
        "no sessions match the filters"
    } else if state.show_ended {
        "no sessions yet"
    } else {
        "no live sessions"
    };
    vec![
        Line::from(Span::raw(why).bold()),
        Line::from(""),
        Line::from(
            Span::raw("a shows ended \u{b7} Esc clears the quest filter \u{b7} ? for keys").dim(),
        ),
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

/// SPEC §17's columns: quest / label / role / status / phase / ctx bar / age.
struct Cells {
    quest: String,
    label: String,
    role: String,
    status: String,
    phase: String,
    ctx: String,
    age: String,
}

fn cells_of(view: &SessionView, warn_pct: u8) -> Cells {
    let s = &view.session;
    Cells {
        quest: fmt::truncate(&view.quest_slug, QUEST_COLS),
        label: s.label.clone(),
        role: s.role.to_string(),
        status: fmt::truncate(&listing::status_cell(s), STATUS_COLS),
        phase: fmt::truncate(
            &fmt::or_dash(s.phase.as_deref().filter(|p| !p.trim().is_empty())),
            PHASE_COLS,
        ),
        ctx: ctx_cell(view, warn_pct),
        age: fmt::age(s.updated_at),
    }
}

/// SPEC §8's `▁▃▅▇` bar and the reading beside it, with the `!` a worker over
/// `[context] worker_warn_pct` earns. A session the statusline hook has never
/// reported for has no bar at all — an empty one would read as 0%.
fn ctx_cell(view: &SessionView, warn_pct: u8) -> String {
    let Some(pct) = view.session.ctx_pct else {
        return "-".to_string();
    };
    let mut out = format!("{} {pct}%", fmt::ctx_bar(pct));
    if view.session.role == SessionRole::Worker && pct >= warn_pct {
        out.push_str(" !");
    }
    out
}

/// The widest cell in each column, so the columns line up across the listing
/// rather than per screenful.
fn widths_of(cells: &[Cells], across: bool) -> [usize; 6] {
    let mut w = [0usize; 6];
    for c in cells {
        let each = [
            if across { layout::width(&c.quest) } else { 0 },
            layout::width(&c.label),
            layout::width(&c.role),
            layout::width(&c.status),
            layout::width(&c.phase),
            layout::width(&c.ctx),
        ];
        for (at, value) in each.into_iter().enumerate() {
            w[at] = w[at].max(value);
        }
    }
    w
}

fn row_line<'a>(c: &Cells, w: &[usize; 6], across: bool, selected: bool, width: usize) -> Line<'a> {
    let mut out = String::from(if selected { "\u{25b8} " } else { "  " });
    if across {
        out.push_str(&pad(&c.quest, w[0]));
        out.push(' ');
    }
    for (text, want) in [
        (&c.label, w[1]),
        (&c.role, w[2]),
        (&c.status, w[3]),
        (&c.phase, w[4]),
        (&c.ctx, w[5]),
    ] {
        out.push_str(&pad(text, want));
        out.push(' ');
    }
    out.push_str(&c.age);
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default()
    };
    Line::from(Span::styled(layout::truncate(&out, width), style))
}

/// Right-pad to `want` display columns. `format!("{:w$}")` counts `char`s, and
/// a slug with a wide glyph in it would then push the columns out of line.
fn pad(s: &str, want: usize) -> String {
    let used = layout::width(s);
    format!("{s}{}", " ".repeat(want.saturating_sub(used)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use crate::model::{Quest, Session};
    use crate::tui::app::Tab;
    use crate::tui::form::{self, Field};
    use crate::tui::render;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    // ------------------------------------------------------------- fixtures

    /// A `Ctx` over an in-memory database, a fixture tmux and a registry
    /// directory the test owns. Nothing here touches the process environment
    /// or anything under the developer's home: `Ctx::for_tests` bypasses
    /// `Q_DB`/`Q_FIXTURE`, and `registry::dir_override` bypasses
    /// `~/.claude/sessions`.
    struct Rig {
        ctx: Ctx,
        tmux: tempfile::TempDir,
        registry: tempfile::TempDir,
        _guard: crate::registry::dir_override::Guard,
    }

    /// How a session is described to [`Rig::session`].
    struct Spec {
        label: String,
        role: SessionRole,
        status: SessionStatus,
        phase: Option<&'static str>,
        ctx_pct: Option<u8>,
        waiting_for: Option<&'static str>,
        updated_at: i64,
    }

    fn spec(label: &str, status: SessionStatus) -> Spec {
        Spec {
            label: label.to_string(),
            role: SessionRole::Worker,
            status,
            phase: None,
            ctx_pct: None,
            waiting_for: None,
            updated_at: 1_000,
        }
    }

    impl Rig {
        fn new() -> Rig {
            let tmux = tempfile::tempdir().unwrap();
            std::fs::write(tmux.path().join("tmux.json"), "{}").unwrap();
            let registry = tempfile::tempdir().unwrap();
            let guard = crate::registry::dir_override::at(registry.path().to_path_buf());
            Rig {
                ctx: Ctx::for_tests(
                    Config::default(),
                    Db::open_in_memory().unwrap(),
                    Box::new(crate::tmux::FixtureTmux::new(tmux.path().join("tmux.json"))),
                ),
                tmux,
                registry,
                _guard: guard,
            }
        }

        fn db(&self) -> &Db {
            self.ctx.db().unwrap()
        }

        fn fixture(&self) -> crate::tmux::FixtureTmux {
            crate::tmux::FixtureTmux::new(self.tmux.path().join("tmux.json"))
        }

        fn quest(&self, slug: &str) -> Quest {
            let mut quest = Quest::new(slug, "/tmp/work", "laptop");
            quest.goal = Some(format!("the goal of {slug}"));
            self.db().insert_quest(&quest).unwrap()
        }

        /// A session row plus the tmux pane that keeps the sweep from ending
        /// it. `pane` is explicit so a test can leave a row with no pane.
        fn session(&self, quest: &Quest, pane: &str, spec: Spec) -> Session {
            let tmux_session = format!("q-{}", quest.slug);
            let mut row = Session::new(
                quest.id.as_str(),
                spec.role,
                &spec.label,
                &tmux_session,
                pane,
            );
            row.status = spec.status;
            row.phase = spec.phase.map(str::to_string);
            row.ctx_pct = spec.ctx_pct;
            row.waiting_for = spec.waiting_for.map(str::to_string);
            row.updated_at = spec.updated_at;
            row.started_at = spec.updated_at;
            // A pid no process can hold, so the registry lookup is a miss in
            // the rig's own directory rather than a `ps` for the pane's tree.
            row.claude_pid =
                Some(900_000_000 + (pane.trim_start_matches('%').parse::<i64>().unwrap_or(0)));
            if spec.status == SessionStatus::Ended {
                row.ended_at = Some(spec.updated_at + 1);
            }
            let row = self.db().insert_session(&row).unwrap();
            if !pane.is_empty() && spec.status != SessionStatus::Ended {
                self.add_pane(&tmux_session, &spec.label, pane);
            }
            row
        }

        fn add_pane(&self, tmux_session: &str, window: &str, pane: &str) {
            let fixture = self.fixture();
            let mut state = fixture.load().unwrap();
            state.panes.push(crate::tmux::FixturePane {
                pane_id: pane.to_string(),
                pane_pid: 90_000,
                session_name: tmux_session.to_string(),
                window_name: window.to_string(),
                ..Default::default()
            });
            fixture.save(&state).unwrap();
        }

        /// What Claude's own registry says about `session`, seeded as the file
        /// `q` would find by pid.
        fn say_registry(&self, session: &Session, quest: &Quest, status: &str) {
            let pid = session.claude_pid.unwrap();
            std::fs::write(
                self.registry.path().join(format!("{pid}.json")),
                serde_json::json!({
                    "pid": pid,
                    "name": format!("{}/{}", quest.slug, session.label),
                    "status": status,
                })
                .to_string(),
            )
            .unwrap();
        }

        fn app(&self) -> App {
            let mut app = App::new(&self.ctx.config, "laptop");
            app.set_size(120, 30);
            app.tab = Tab::Sessions;
            refresh(&self.ctx, &mut app).unwrap();
            app
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

    fn line_of(lines: &[String], needle: &str) -> Option<usize> {
        lines.iter().position(|l| l.contains(needle))
    }

    /// Exactly what the event loop does with `Action::Submit`.
    fn run_submit(rig: &Rig, app: &mut App) {
        let Some(modal) = app.modal.take() else {
            panic!("no form is open");
        };
        let outcome = submit(&rig.ctx, app, &modal.prompt, &modal.form);
        let mut modal = modal;
        if let Err(e) = outcome {
            modal.form.set_error(format!("{e:#}"));
            app.modal = Some(modal);
        }
    }

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

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle(Input::Char(c));
        }
    }

    /// Move the action row off `cancel`, the way the user has to.
    fn choose_action(app: &mut App) {
        focus(app, form::ACTION);
        for _ in 0..3 {
            if app.modal.as_ref().unwrap().form.confirmed() {
                return;
            }
            app.handle(Input::Right);
        }
        panic!("the action row never left `{}`", form::CANCEL);
    }

    /// A fleet of three Quests: one waiting worker, one busy master, one idle
    /// worker, deliberately out of rank order.
    fn fleet() -> (Rig, App) {
        let rig = Rig::new();
        let alpha = rig.quest("alpha");
        let beta = rig.quest("beta");

        let mut master = spec("master", SessionStatus::Busy);
        master.role = SessionRole::Master;
        master.phase = Some("wiring the loader");
        master.ctx_pct = Some(41);
        master.updated_at = 3_000;
        rig.session(&alpha, "%1", master);

        let mut waiting = spec("tests", SessionStatus::Waiting);
        waiting.waiting_for = Some("permission");
        waiting.ctx_pct = Some(72);
        waiting.updated_at = 1_000;
        rig.session(&alpha, "%2", waiting);

        let mut idle = spec("docs", SessionStatus::Idle);
        idle.updated_at = 2_000;
        rig.session(&beta, "%3", idle);

        let app = rig.app();
        (rig, app)
    }

    // --------------------------------------------------------------- render

    /// SPEC §17: "waiting na vrhu".
    #[test]
    fn waiting_sessions_lead_the_fleet_whatever_quest_they_are_in() {
        let (_rig, mut app) = fleet();
        let lines = draw(&mut app, 120, 30);
        let text = lines.join("\n");
        for group in ["waiting", "busy", "idle"] {
            assert!(text.contains(group), "missing group {group}\n{text}");
        }
        let (waiting, busy, idle) = (
            line_of(&lines, "tests").unwrap(),
            line_of(&lines, "master").unwrap(),
            line_of(&lines, "docs").unwrap(),
        );
        assert!(waiting < busy && busy < idle, "{text}");
        // Their headers sit directly above them.
        assert!(lines[waiting - 1].trim() == "waiting", "{text}");
        assert!(lines[busy - 1].trim() == "busy", "{text}");
        assert!(lines[idle - 1].trim() == "idle", "{text}");
    }

    /// SPEC §17's columns, and the reason a session is waiting alongside them.
    #[test]
    fn a_row_carries_quest_label_role_status_phase_ctx_and_age() {
        let (_rig, mut app) = fleet();
        let lines = draw(&mut app, 120, 30);
        let master = &lines[line_of(&lines, "master").unwrap()];
        for cell in ["alpha", "master", "wiring the loader", "41%"] {
            assert!(master.contains(cell), "{cell} missing from {master:?}");
        }
        assert!(master.contains(&fmt::ctx_bar(41)), "{master:?}");
        let waiting = &lines[line_of(&lines, "tests").unwrap()];
        assert!(waiting.contains("waiting: permission"), "{waiting:?}");
        assert!(waiting.contains("worker"), "{waiting:?}");
    }

    /// SPEC §8's `[context] worker_warn_pct`, which is only about workers: the
    /// master's own threshold is what the auto-reset acts on.
    #[test]
    fn a_worker_over_the_warn_threshold_is_marked_and_a_master_is_not() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let mut master = spec("master", SessionStatus::Idle);
        master.role = SessionRole::Master;
        master.ctx_pct = Some(99);
        rig.session(&quest, "%1", master);
        let mut hot = spec("hot", SessionStatus::Idle);
        hot.ctx_pct = Some(70);
        rig.session(&quest, "%2", hot);
        let mut cool = spec("cool", SessionStatus::Idle);
        cool.ctx_pct = Some(69);
        rig.session(&quest, "%3", cool);

        let mut app = rig.app();
        assert_eq!(app.sessions.warn_pct, 70);
        let lines = draw(&mut app, 120, 30);
        assert!(lines[line_of(&lines, "hot").unwrap()].contains("70% !"));
        assert!(!lines[line_of(&lines, "cool").unwrap()].contains('!'));
        // 99% and no warning: a master over its threshold is auto-reset.
        assert!(!lines[line_of(&lines, "master").unwrap()].contains('!'));
    }

    /// The two facts a fresh session has none of. An empty bar would read as
    /// 0%, and an empty phase column would collapse the ones beside it.
    #[test]
    fn a_session_with_no_ctx_reading_and_no_phase_still_renders() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.session(&quest, "%1", spec("bare", SessionStatus::Starting));
        let mut app = rig.app();
        let lines = draw(&mut app, 120, 30);
        let row = &lines[line_of(&lines, "bare").unwrap()];
        assert!(row.contains('-'), "{row:?}");
        assert!(!row.contains('%'), "an invented reading: {row:?}");
        for glyph in ['\u{2581}', '\u{2583}', '\u{2585}', '\u{2587}'] {
            assert!(!row.contains(glyph), "an invented bar: {row:?}");
        }
        assert!(lines.join("\n").contains("starting"), "{lines:?}");
    }

    /// SPEC §17's fleet view is about what is running; `a` is how the history
    /// comes back.
    #[test]
    fn ended_sessions_are_hidden_until_a() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.session(&quest, "%1", spec("live", SessionStatus::Idle));
        rig.session(&quest, "%2", spec("gone", SessionStatus::Ended));

        let mut app = rig.app();
        assert!(!screen(&mut app, 120, 30).contains("gone"));
        assert_eq!(handle(&mut app, Input::Char('a')), Action::Refresh);
        assert!(app.sessions.show_ended);
        refresh(&rig.ctx, &mut app).unwrap();
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("gone"), "{text}");
        assert!(text.contains("ended"), "{text}");
        assert!(app.filters().contains("+ended"), "{}", app.filters());
    }

    #[test]
    fn an_empty_fleet_says_so_rather_than_drawing_nothing() {
        let rig = Rig::new();
        let mut app = rig.app();
        let text = screen(&mut app, 120, 30);
        assert!(text.contains("no live sessions"), "{text}");
        assert!(text.contains("a shows ended"), "{text}");
        // And a listing every filter hides says which of the two it was.
        let quest = rig.quest("alpha");
        rig.session(&quest, "%1", spec("live", SessionStatus::Idle));
        let mut app = rig.app();
        app.sessions.quest = Some("q-nope".to_string());
        app.sessions.resync();
        assert!(
            screen(&mut app, 120, 30).contains("no sessions match the filters"),
            "{}",
            screen(&mut app, 120, 30)
        );
    }

    /// Nothing selected, so none of the five keys may do anything at all.
    #[test]
    fn every_key_is_a_no_op_on_an_empty_fleet() {
        let rig = Rig::new();
        let mut app = rig.app();
        for key in ['o', 'p', 't', 'k', 'Z'] {
            assert_eq!(handle(&mut app, Input::Char(key)), Action::None, "{key}");
            assert!(app.modal.is_none(), "{key} opened a box on nothing");
        }
        assert_eq!(handle(&mut app, Input::Enter), Action::None);
    }

    /// The same defect the Events tab was carrying (bd-8lz.4.6 D1), latent
    /// here only because the selection usually sits near the top: `settle`
    /// pushed `offset` forward and never back, so a viewport that GREW between
    /// two frames left the bottom of the listing blank with rows stranded
    /// above the fold.
    #[test]
    fn a_grown_viewport_refills_the_listing() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        for n in 0..20 {
            rig.session(
                &quest,
                &format!("%{n}"),
                spec(&format!("worker-{n:02}"), SessionStatus::Idle),
            );
        }
        let mut app = rig.app();
        // A short terminal with the cursor at the end pushes `offset` as far
        // forward as it goes.
        app.set_size(120, 12);
        handle(&mut app, Input::End);
        draw(&mut app, 120, 12);
        assert!(app.sessions.offset > 0, "the short frame never scrolled");

        // Now the terminal grows past the whole listing. Every session fits,
        // so every session has to be on screen.
        let lines = draw(&mut app, 120, 60);
        for n in 0..20 {
            let label = format!("worker-{n:02}");
            assert!(
                line_of(&lines, &label).is_some(),
                "{label} stranded: {lines:#?}"
            );
        }
    }

    /// N-7. `viewport` reserved all five group headers unconditionally, so at
    /// a body of 8 rows the effective page was 3 and PageDown moved 3 with
    /// five usable lines empty. It reserves the listing's own headers instead.
    #[test]
    fn the_viewport_only_pays_for_the_groups_the_listing_actually_has() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        // More rows than either page, so the clamp cannot stand in for the
        // step and the PageDown assertion tells the two page sizes apart.
        for n in 0..12u8 {
            let mut row = spec(&format!("w{n}"), SessionStatus::Idle);
            row.updated_at = 1_000 + n as i64;
            rig.session(&quest, &format!("%{}", n + 1), row);
        }
        let mut app = rig.app();
        // One group ("idle"), so a body of 10 shows 9 rows, not 5.
        app.set_size(120, 12);
        assert_eq!(viewport(&app), 9);

        // PageDown moves by that page rather than by a constant.
        assert_eq!(app.sessions.selected, 0);
        handle(&mut app, Input::PageDown);
        assert_eq!(app.sessions.selected, 9, "PageDown paged by the wrong step");
        handle(&mut app, Input::PageUp);
        assert_eq!(app.sessions.selected, 0, "PageUp paged by the wrong step");
    }

    /// The same off-by-one at the bottom of the loop: the header and its row
    /// were pushed as a pair and the excess truncated afterwards, so a header
    /// could render with nothing under it — and at a one-line body it ate the
    /// selected row entirely.
    #[test]
    fn a_group_header_never_renders_without_its_row_under_it() {
        let (_rig, mut app) = fleet();
        for h in 1..=12u16 {
            app.set_size(120, h + 2);
            let lines = draw(&mut app, 120, h + 2);
            // The body is everything but the tab bar and the status line.
            let body: Vec<&String> = lines[1..lines.len().saturating_sub(1)].iter().collect();
            let headers = ["waiting", "busy", "starting", "idle", "ended"];
            for (n, line) in body.iter().enumerate() {
                if !headers.contains(&line.trim()) {
                    continue;
                }
                let under = body.get(n + 1).map(|l| l.trim()).unwrap_or("");
                assert!(
                    !under.is_empty() && !headers.contains(&under),
                    "h={h}: `{}` has nothing under it\n{}",
                    line.trim(),
                    lines.join("\n")
                );
            }
            // And whatever the height, the selected row is on screen.
            assert!(
                body.iter().any(|l| l.starts_with('\u{25b8}')),
                "h={h}: the selection is invisible\n{}",
                lines.join("\n")
            );
        }
    }

    /// R2-1. The round-2 review's exhaustive probe, kept as a test: every fleet
    /// shape x every body height x every selection, asserting all three
    /// viewport properties at once. Suppressing the header on the last line
    /// without also dropping its row traded orphan headers for rows drawn
    /// under the PREVIOUS group's heading.
    #[test]
    fn no_row_ever_renders_under_another_group_s_header() {
        const SHAPES: [[usize; GROUPS]; 9] = [
            [1, 1, 1, 9, 0],
            [1, 1, 1, 1, 1],
            [3, 0, 0, 3, 0],
            [0, 1, 0, 1, 0],
            [2, 2, 2, 2, 2],
            [1, 0, 1, 0, 1],
            [0, 0, 0, 5, 0],
            [4, 1, 0, 0, 1],
            [1, 2, 3, 4, 1],
        ];
        const STATUSES: [SessionStatus; GROUPS] = [
            SessionStatus::Waiting,
            SessionStatus::Busy,
            SessionStatus::Starting,
            SessionStatus::Idle,
            SessionStatus::Ended,
        ];
        const HEADERS: [&str; GROUPS] = ["waiting", "busy", "starting", "idle", "ended"];

        // The label carries its own group, so a row can be checked against the
        // heading it actually ended up under.
        fn group_of(line: &str) -> Option<usize> {
            line.split_whitespace().find_map(|token| {
                HEADERS.iter().position(|h| {
                    token.strip_prefix(h).is_some_and(|rest| {
                        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
                    })
                })
            })
        }

        let mut orphans = 0;
        let mut mislabelled = 0;
        let mut invisible = 0;
        let mut first = String::new();
        for shape in SHAPES {
            let rig = Rig::new();
            let quest = rig.quest("alpha");
            let mut pane = 0i64;
            for (at, count) in shape.iter().enumerate() {
                for n in 0..*count {
                    pane += 1;
                    let mut row = spec(&format!("{}{n}", HEADERS[at]), STATUSES[at]);
                    row.updated_at = 9_000 - pane;
                    rig.session(&quest, &format!("%{pane}"), row);
                }
            }
            let mut app = rig.app();
            app.sessions.show_ended = true;
            refresh(&rig.ctx, &mut app).unwrap();
            let len = app.sessions.visible().len();
            assert_eq!(len, shape.iter().sum::<usize>(), "{shape:?}");

            for body_h in 1..=20u16 {
                for sel in 0..len {
                    app.sessions.selected = sel;
                    app.sessions.offset = 0;
                    let lines = draw(&mut app, 120, body_h + 2);
                    let body: Vec<&str> =
                        lines[1..lines.len() - 1].iter().map(|l| l.trim()).collect();
                    let mut blame = |what: &str| {
                        if first.is_empty() {
                            first = format!(
                                "{what} \u{b7} {shape:?} h={body_h} sel={sel}\n{}",
                                lines.join("\n")
                            );
                        }
                    };
                    let mut under: Option<usize> = None;
                    for (n, line) in body.iter().enumerate() {
                        if let Some(at) = HEADERS.iter().position(|h| h == line) {
                            let next = body.get(n + 1).copied().unwrap_or("");
                            if next.is_empty() || HEADERS.contains(&next) {
                                orphans += 1;
                                blame(&format!("orphan header `{line}`"));
                            }
                            under = Some(at);
                            continue;
                        }
                        let Some(at) = group_of(line) else { continue };
                        if let Some(seen) = under.filter(|seen| *seen != at) {
                            mislabelled += 1;
                            blame(&format!("`{line}` under `{}`", HEADERS[seen]));
                        }
                        under = Some(at);
                    }
                    if !body.iter().any(|l| l.starts_with('\u{25b8}')) {
                        invisible += 1;
                        blame("the selection is invisible");
                    }
                }
            }
        }
        assert_eq!(
            (orphans, mislabelled, invisible),
            (0, 0, 0),
            "orphan headers / mislabelled rows / invisible selections\n{first}"
        );
    }

    // ------------------------------------------------- the Quests hand-off

    /// SPEC §17: `s` on the Quests tab means "the Sessions tab, filtered to
    /// this one". The hint is consumed once, so `Esc` can clear it for good.
    #[test]
    fn s_on_the_quests_tab_hands_this_tab_its_quest() {
        let rig = Rig::new();
        let alpha = rig.quest("alpha");
        let beta = rig.quest("beta");
        rig.session(&alpha, "%1", spec("mine", SessionStatus::Idle));
        rig.session(&beta, "%2", spec("theirs", SessionStatus::Idle));

        let mut app = App::new(&rig.ctx.config, "laptop");
        app.set_size(120, 30);
        crate::tui::quests::refresh(&rig.ctx, &mut app).unwrap();
        // Land the Quests-tab selection on beta, then hand it over.
        for _ in 0..4 {
            if crate::tui::quests::selected_quest(&app).map(|q| q.id) == Some(beta.id.clone()) {
                break;
            }
            app.handle(Input::Down);
        }
        assert_eq!(
            crate::tui::quests::selected_quest(&app).map(|q| q.id),
            Some(beta.id.clone())
        );
        assert_eq!(app.handle(Input::Char('s')), Action::Refresh);
        assert_eq!(app.tab, Tab::Sessions);
        assert_eq!(app.focus_quest.as_deref(), Some(beta.id.as_str()));

        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(app.focus_quest, None, "the hint was not consumed");
        assert_eq!(app.sessions.quest.as_deref(), Some(beta.id.as_str()));
        assert!(app.filters().contains("quest beta"), "{}", app.filters());

        let lines = draw(&mut app, 120, 30);
        let text = lines.join("\n");
        assert!(text.contains("theirs"), "{text}");
        assert!(!text.contains("mine"), "{text}");
        // The Quest column goes away with the filter — one Quest, one answer —
        // and the chip in the status bar is what says which Quest it is.
        let row = &lines[line_of(&lines, "theirs").unwrap()];
        assert!(!row.contains("beta"), "{row:?}");
        assert!(
            lines.last().unwrap().contains("[quest beta]"),
            "{:?}",
            lines.last()
        );

        // Esc widens it again, and the selection is re-anchored rather than
        // left indexing rows that moved under it.
        assert_eq!(handle(&mut app, Input::Esc), Action::None);
        assert_eq!(app.sessions.quest, None);
        let after = screen(&mut app, 120, 30);
        assert!(
            after.contains("mine") && after.contains("theirs"),
            "{after}"
        );
        assert_eq!(
            app.sessions.selected_row().map(|v| v.session.label.clone()),
            Some("theirs".to_string()),
            "the selection jumped when the filter went away"
        );
    }

    // --------------------------------------------------- the guarded prompts

    /// B2's rule, on all three: a bare Enter does nothing, and the box stays
    /// up saying what is missing. `q send` types into a live Claude and
    /// `q reset` sends it `/clear`; neither is `harmless()`.
    #[test]
    fn every_destructive_prompt_refuses_a_bare_enter() {
        let (rig, mut app) = fleet();
        for (key, title) in [('t', "send to"), ('k', "kill"), ('Z', "reset")] {
            // Off the master, which `k` refuses outright.
            app.sessions.selected_id = None;
            app.sessions.quest = None;
            let at = app
                .sessions
                .visible()
                .iter()
                .position(|i| app.sessions.rows[*i].session.label == "tests")
                .unwrap();
            app.sessions.move_to(at, 20);

            assert_eq!(handle(&mut app, Input::Char(key)), Action::None);
            let modal = app.modal.as_ref().expect("no box for {key}");
            assert!(modal.form.title.contains(title), "{}", modal.form.title);
            assert!(
                !modal.form.confirmed(),
                "{key} starts confirmed — a stray Enter would run it"
            );
            assert_eq!(app.handle(Input::Enter), Action::None, "{key} submitted");
            assert!(app.modal.is_some(), "{key} took the box down");
            let error = app.modal.as_ref().unwrap().form.error().unwrap_or_default();
            assert!(error.contains("nothing done"), "{key}: {error:?}");
            app.handle(Input::Esc);
            assert!(app.modal.is_none());
            let _ = &rig;
        }
    }

    /// Every one of them declares an action row rather than `harmless()` —
    /// the claim that makes a bare Enter mean something.
    #[test]
    fn no_session_prompt_is_declared_harmless() {
        let (_rig, mut app) = fleet();
        let at = app
            .sessions
            .visible()
            .iter()
            .position(|i| app.sessions.rows[*i].session.label == "tests")
            .unwrap();
        app.sessions.move_to(at, 20);
        for (key, verb) in [('t', "send"), ('k', "kill"), ('Z', "reset")] {
            handle(&mut app, Input::Char(key));
            let form = &app.modal.as_ref().unwrap().form;
            assert!(
                form.fields().iter().any(|f| f.label() == form::ACTION),
                "{key} has no action row"
            );
            let options: Vec<String> = form
                .fields()
                .iter()
                .filter_map(|f| match f {
                    Field::Select { label, options, .. } if label == form::ACTION => {
                        Some(options.clone())
                    }
                    _ => None,
                })
                .next()
                .unwrap();
            assert_eq!(options, [form::CANCEL.to_string(), verb.to_string()]);
            app.handle(Input::Esc);
        }
    }

    /// The master is refused up front rather than at submit: SPEC §6 makes
    /// window 0 the Quest itself.
    #[test]
    fn k_refuses_the_master_without_opening_a_box() {
        let (_rig, mut app) = fleet();
        let at = app
            .sessions
            .visible()
            .iter()
            .position(|i| app.sessions.rows[*i].session.label == "master")
            .unwrap();
        app.sessions.move_to(at, 20);
        assert_eq!(handle(&mut app, Input::Char('k')), Action::None);
        assert!(app.modal.is_none(), "a box that could only fail");
        assert!(app.status.contains("is the master"), "{}", app.status);
    }

    // ----------------------------------------------------------- submitting

    #[test]
    fn t_sends_the_text_into_the_pane_and_logs_it() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();

        handle(&mut app, Input::Char('t'));
        focus(&mut app, F_TEXT);
        type_text(&mut app, "run the suite");
        choose_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        run_submit(&rig, &mut app);

        assert!(app.modal.is_none(), "the form stayed up: {}", app.status);
        assert!(app.status.contains("sent to alpha/tests"), "{}", app.status);
        let pane = rig.fixture().load().unwrap().panes[0].buffer.clone();
        assert_eq!(pane, "run the suite\n");
        let events = rig.db().list_events_by_quest(&quest.id, 10).unwrap();
        assert!(
            events.iter().any(|e| e.kind == "session.send"),
            "{events:?}"
        );
        let _ = session;
    }

    /// SPEC §23 #5: send-keys into a live TUI is fragile, so `q send` refuses
    /// unless the session is idle and `--force` is the explicit way past. The
    /// TUI honours the same gate — the toggle is off by default, and the
    /// refusal comes back into the box rather than being bypassed.
    #[test]
    fn t_honours_the_idle_gate_and_force_is_the_only_way_past_it() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.session(&quest, "%1", spec("tests", SessionStatus::Busy));
        let mut app = rig.app();

        handle(&mut app, Input::Char('t'));
        // The box says so before anything is typed.
        let notes = form_notes(&app);
        assert!(
            notes.iter().any(|n| n.contains("q has it as busy")),
            "{notes:?}"
        );
        assert!(
            !app.modal.as_ref().unwrap().form.is_on(F_FORCE),
            "force is on by default"
        );

        focus(&mut app, F_TEXT);
        type_text(&mut app, "stop that");
        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("the gate let it through")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("is not idle"), "{error}");
        assert!(error.contains("--force"), "{error}");
        assert_eq!(rig.fixture().load().unwrap().panes[0].buffer, "");

        // Force, and the same submission goes through — and says it forced.
        focus(&mut app, F_FORCE);
        app.handle(Input::Char(' '));
        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        assert!(app.modal.is_none(), "still refused: {}", app.status);
        assert!(app.status.contains("forced past"), "{}", app.status);
        assert_eq!(rig.fixture().load().unwrap().panes[0].buffer, "stop that\n");
    }

    /// The gate takes two opinions (SPEC §23 #5). A row `q` has as idle is
    /// still refused when Claude's own registry says otherwise.
    #[test]
    fn the_registry_can_refuse_a_send_the_database_would_have_allowed() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        rig.say_registry(&session, &quest, "busy");

        let mut app = rig.app();
        handle(&mut app, Input::Char('t'));
        focus(&mut app, F_TEXT);
        type_text(&mut app, "hello");
        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("let through")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("registry"), "{error}");
        assert_eq!(rig.fixture().load().unwrap().panes[0].buffer, "");
    }

    #[test]
    fn k_kills_the_window_and_ends_the_row() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();

        handle(&mut app, Input::Char('k'));
        choose_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        run_submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", app.status);
        assert!(app.status.contains("killed alpha/tests"), "{}", app.status);
        assert!(rig.fixture().load().unwrap().panes.is_empty());
        let row = rig.db().get_session(&session.id).unwrap().unwrap();
        assert_eq!(row.status, SessionStatus::Ended);
    }

    /// `Z` hands the work to a detached `q reset` (SPEC §8's own mechanism),
    /// so the event loop never waits on the brief. Under a fixture tmux the
    /// launch is suppressed, and the scheduled command line is what proves
    /// what would have run.
    #[test]
    fn z_schedules_the_reset_rather_than_blocking_the_loop() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();

        handle(&mut app, Input::Char('Z'));
        let notes = form_notes(&app);
        assert!(notes.iter().any(|n| n.contains("/clear")), "{notes:?}");
        choose_action(&mut app);
        assert_eq!(app.handle(Input::Enter), Action::Submit);
        run_submit(&rig, &mut app);

        assert!(app.modal.is_none(), "{}", app.status);
        assert!(
            app.status.contains("resetting alpha/tests"),
            "{}",
            app.status
        );
        let events = rig.db().list_events_by_quest(&quest.id, 10).unwrap();
        let scheduled = events
            .iter()
            .find(|e| e.kind == "session.reset_scheduled")
            .expect("nothing was scheduled");
        let payload = scheduled.payload.as_ref().unwrap();
        assert_eq!(payload["manual"], serde_json::json!(true));
        let argv = payload["argv"].as_array().unwrap();
        let argv: Vec<&str> = argv.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(
            &argv[1..],
            [
                "reset",
                session.id.as_str(),
                "--delay",
                "0",
                "--strategy",
                "clear",
                "--quiet"
            ]
        );
        // Nothing was typed into the pane from here: that is the child's job.
        assert_eq!(rig.fixture().load().unwrap().panes[0].buffer, "");
    }

    /// A busy session is refused in front of the user rather than in a
    /// detached process nobody is watching.
    #[test]
    fn z_refuses_a_session_that_is_not_idle() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.session(&quest, "%1", spec("tests", SessionStatus::Busy));
        let mut app = rig.app();

        handle(&mut app, Input::Char('Z'));
        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("let through")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("is not idle"), "{error}");
        let events = rig.db().list_events_by_quest(&quest.id, 10).unwrap();
        assert!(
            !events.iter().any(|e| e.kind == "session.reset_scheduled"),
            "{events:?}"
        );
    }

    // ------------------------------------------------- the identity re-check

    /// SPEC §6 makes the pane the session's identity, and the pane is what
    /// every one of these acts on. A row that moved to another pane while the
    /// box was up is a different agent.
    #[test]
    fn a_session_that_moved_pane_under_an_open_box_is_refused() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();

        handle(&mut app, Input::Char('t'));
        focus(&mut app, F_TEXT);
        type_text(&mut app, "hello");
        // Another terminal, while the box is up.
        rig.add_pane("q-alpha", "tests", "%9");
        rig.db().update_session_pane(&session.id, "%9").unwrap();

        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("acted on a stale pane")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("pane %9"), "{error}");
        assert!(error.contains("Esc and try again"), "{error}");
        let panes = rig.fixture().load().unwrap().panes;
        assert!(panes.iter().all(|p| p.buffer.is_empty()), "{panes:?}");
    }

    /// The other half: a session that ended between the box opening and the
    /// submit must not be killed, and must not have text typed into a pane
    /// that by now belongs to something else.
    #[test]
    fn a_session_that_ended_under_an_open_box_is_refused() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();

        handle(&mut app, Input::Char('k'));
        // The pane goes away — the sweep inside `session_for` is what notices.
        rig.fixture()
            .save(&crate::tmux::FixtureState::default())
            .unwrap();

        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("killed an ended session")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("has ended"), "{error}");
        assert!(error.contains("Esc and try again"), "{error}");
        let row = rig.db().get_session(&session.id).unwrap().unwrap();
        assert!(row.ended_at.is_some());
    }

    /// A 16-bit id can be minted twice, so the id alone is not identity;
    /// `started_at` is the column nothing can change.
    #[test]
    fn a_recycled_session_id_is_not_the_session_the_box_named() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        let session = rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();
        handle(&mut app, Input::Char('t'));
        focus(&mut app, F_TEXT);
        type_text(&mut app, "hello");

        // `q rm` and a `q spawn` in another terminal, handing the same id to a
        // different session.
        rig.db().delete_session(&session.id).unwrap();
        let mut reborn = Session::new(&quest.id, SessionRole::Worker, "tests", "q-alpha", "%1");
        reborn.id = session.id.clone();
        reborn.status = SessionStatus::Idle;
        reborn.started_at = session.started_at + 99;
        rig.db().insert_session(&reborn).unwrap();

        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("acted on a stranger")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("is gone"), "{error}");
        assert_eq!(rig.fixture().load().unwrap().panes[0].buffer, "");
    }

    /// The box printed `<slug>/<label>`; a rename underneath makes it a lie
    /// about which agent this is.
    #[test]
    fn a_quest_renamed_under_an_open_box_is_refused() {
        let rig = Rig::new();
        let quest = rig.quest("alpha");
        rig.session(&quest, "%1", spec("tests", SessionStatus::Idle));
        let mut app = rig.app();
        handle(&mut app, Input::Char('k'));

        rig.db()
            .update_quest(
                &quest.id,
                &crate::db::quest::QuestPatch {
                    slug: Some("omega".to_string()),
                    ..Default::default()
                },
            )
            .unwrap();

        choose_action(&mut app);
        app.handle(Input::Enter);
        run_submit(&rig, &mut app);
        let error = app
            .modal
            .as_ref()
            .expect("killed a renamed quest's worker")
            .form
            .error()
            .unwrap()
            .to_string();
        assert!(error.contains("omega/tests"), "{error}");
        assert!(
            !rig.fixture().load().unwrap().panes.is_empty(),
            "it killed it anyway"
        );
    }

    // ------------------------------------------- rows that never got a pane

    /// B1. `q spawn` inserts the worker row *before* it opens the window, and
    /// the sweep leaves a pane-less row alone for `START_GRACE_SECS`. In that
    /// window the row is `starting`, live, not master — so every one of the
    /// three boxes used to open on it, and every five-part identity check used
    /// to pass, because the target's pane was captured from the same row:
    /// `"" != ""` is false. tmux reads an empty `-t` as "whatever is current",
    /// which inside tmux is the window `q` itself is running in.
    #[test]
    fn the_three_destructive_keys_refuse_a_row_whose_window_never_opened() {
        for (key, want) in [
            ('k', "has no window yet"),
            ('t', "has no pane yet"),
            ('Z', "has no pane yet"),
        ] {
            let rig = Rig::new();
            let quest = rig.quest("alpha");
            // Live, idle, a worker, and young enough that the sweep leaves the
            // missing pane alone — nothing else refuses it.
            rig.session(&quest, "", pending("tests"));
            // A second Quest with a real window, so there is something for an
            // empty target to have hit.
            let beta = rig.quest("beta");
            rig.session(&beta, "%1", spec("docs", SessionStatus::Idle));
            let mut app = rig.app();
            for _ in 0..4 {
                if sessions_label(&app) == "tests" {
                    break;
                }
                handle(&mut app, Input::Down);
            }
            assert_eq!(sessions_label(&app), "tests");

            assert_eq!(handle(&mut app, Input::Char(key)), Action::None);
            assert!(app.modal.is_none(), "`{key}` opened a box: {}", app.status);
            assert!(
                app.status.contains("alpha/tests") && app.status.contains(want),
                "`{key}` said {:?}",
                app.status
            );
            // Nothing was killed, and nothing was typed anywhere.
            let panes = rig.fixture().load().unwrap().panes;
            assert_eq!(panes.len(), 1, "`{key}` killed a window");
            assert!(panes[0].buffer.is_empty(), "`{key}`: {:?}", panes[0].buffer);
            let events = rig.db().list_events_by_quest(&quest.id, 10).unwrap();
            assert!(events.is_empty(), "`{key}`: {events:?}");
        }
    }

    /// And the layer under the openers: a prompt carrying a pane-less target
    /// is refused at submit too, so a future opener that forgets the check
    /// cannot get past `session_for`.
    #[test]
    fn submitting_a_prompt_with_no_pane_is_refused_by_the_identity_check() {
        for open in [
            Prompt::Kill as fn(SessionTarget) -> Prompt,
            Prompt::Send,
            Prompt::Reset,
        ] {
            let rig = Rig::new();
            let quest = rig.quest("alpha");
            let session = rig.session(&quest, "", pending("tests"));
            let beta = rig.quest("beta");
            rig.session(&beta, "%1", spec("docs", SessionStatus::Idle));
            let mut app = rig.app();

            let target = SessionTarget {
                session: session.id.clone(),
                quest: quest.id.clone(),
                pane: String::new(),
                started_at: session.started_at,
                name: "alpha/tests".to_string(),
                ended: false,
            };
            let form = Form::new("box").text(F_TEXT, "hello", "").action("go");
            let error = submit(&rig.ctx, &mut app, &open(target), &form)
                .expect_err("a pane-less target was acted on");
            assert!(format!("{error:#}").contains("has no pane"), "{error:#}");

            let panes = rig.fixture().load().unwrap().panes;
            assert_eq!(panes.len(), 1);
            assert!(panes[0].buffer.is_empty(), "{:?}", panes[0].buffer);
            assert!(
                rig.db()
                    .list_events_by_quest(&quest.id, 10)
                    .unwrap()
                    .is_empty()
            );
        }
    }

    // ---------------------------------------------------------------- misc

    /// `k` is kill here, so half a vim keymap would be a trap.
    #[test]
    fn j_and_k_do_not_move_the_selection_on_this_tab() {
        let (_rig, mut app) = fleet();
        let first = app.sessions.selected_id.clone();
        assert_eq!(handle(&mut app, Input::Char('j')), Action::None);
        assert_eq!(app.sessions.selected_id, first);
        assert_eq!(handle(&mut app, Input::Down), Action::None);
        assert_ne!(app.sessions.selected_id, first, "the arrows still move");
    }

    #[test]
    fn the_selection_survives_a_reload_that_reorders_the_fleet() {
        let (rig, mut app) = fleet();
        handle(&mut app, Input::Down);
        let on = app.sessions.selected_id.clone().unwrap();
        // The waiting worker settles, which moves everything.
        let waiting: Vec<Session> = rig
            .db()
            .list_live_sessions()
            .unwrap()
            .into_iter()
            .filter(|s| s.label == "tests")
            .collect();
        rig.db()
            .update_session_status(&waiting[0].id, SessionStatus::Idle, None)
            .unwrap();
        refresh(&rig.ctx, &mut app).unwrap();
        assert_eq!(app.sessions.selected_id.as_deref(), Some(on.as_str()));
    }

    /// The row is a packed table, so a narrow terminal is where it would
    /// overflow. `draw` panics on a widget written outside its area, and the
    /// chrome keeping its own line is what proves the body did not take it.
    #[test]
    fn the_fleet_renders_at_every_terminal_size() {
        let (_rig, mut app) = fleet();
        for w in [1u16, 2, 20, 40, 70, 100, 200] {
            for h in [1u16, 2, 3, 8, 24] {
                let lines = draw(&mut app, w, h);
                assert_eq!(lines.len() as u16, h, "{w}x{h}");
                for line in &lines {
                    assert!(layout::width(line) <= w as usize, "{w}x{h}: {line:?}");
                }
                // 70 columns is the narrowest width the hint is *asserted*
                // to fit at (`the_status_hint_fits_the_segment_it_is_given`);
                // below that `right_segment` truncates it, which is accepted.
                if h >= 2 && w >= 70 {
                    assert!(
                        lines.last().unwrap().contains("q quit"),
                        "{w}x{h}: {:?}",
                        lines.last()
                    );
                }
            }
        }
        // And with a box up, which is drawn over the whole frame.
        handle(&mut app, Input::Char('t'));
        for w in [1u16, 12, 40, 120] {
            for h in [1u16, 4, 24] {
                draw(&mut app, w, h);
            }
        }
    }

    /// The `?` overlay grows the tab's own half (SPEC §17's keys), so a user
    /// who cannot remember `Z` has somewhere to look.
    #[test]
    fn the_help_overlay_lists_this_tabs_keys() {
        let (_rig, mut app) = fleet();
        app.handle(Input::Char('?'));
        let text = screen(&mut app, 120, 40);
        for key in ["peek", "send text", "kill this worker", "reset its context"] {
            assert!(text.contains(key), "{key} missing\n{text}");
        }
    }

    /// A row a `q spawn` inserted but never gave a pane, young enough that the
    /// sweep's start grace has not run out.
    fn pending(label: &'static str) -> Spec {
        let mut spec = spec(label, SessionStatus::Idle);
        spec.updated_at = crate::model::now();
        spec
    }

    fn sessions_label(app: &App) -> String {
        selected(app).expect("nothing selected").label
    }

    fn form_notes(app: &App) -> Vec<String> {
        app.modal
            .as_ref()
            .unwrap()
            .form
            .fields()
            .iter()
            .filter_map(|f| match f {
                Field::Note(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }
}
