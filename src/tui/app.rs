//! The TUI state machine (SPEC §17).
//!
//! `App` holds every piece of state the renderer reads, and `App::handle` is
//! the single, pure key → state transition. Nothing in this module touches a
//! terminal, so the whole keymap is unit-testable.
#![allow(dead_code)]

use crate::config::Config;

use super::form::{Form, Outcome};
use super::keys::{Input, MouseInput};
use super::layout::{self, RowMode};
use super::{events, quests, sessions, templates};

/// The four tabs of SPEC §17, in bar order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Quests,
    Sessions,
    Templates,
    Events,
}

impl Tab {
    pub const ALL: [Tab; 4] = [Tab::Quests, Tab::Sessions, Tab::Templates, Tab::Events];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Quests => "Quests",
            Tab::Sessions => "Sessions",
            Tab::Templates => "Templates",
            Tab::Events => "Events",
        }
    }

    pub fn index(self) -> usize {
        Tab::ALL.iter().position(|t| *t == self).unwrap_or(0)
    }

    /// The digit key that jumps straight here.
    pub fn digit(self) -> u32 {
        self.index() as u32 + 1
    }

    pub fn from_digit(d: u32) -> Option<Tab> {
        Tab::ALL.get(d.checked_sub(1)? as usize).copied()
    }

    pub fn next(self) -> Tab {
        Tab::ALL[(self.index() + 1) % Tab::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        Tab::ALL[(self.index() + Tab::ALL.len() - 1) % Tab::ALL.len()]
    }
}

/// What the event loop must do after a transition. Everything the state
/// machine cannot do by itself lives here, so no tab ever reaches for the
/// terminal or a subprocess from inside `handle`.
///
/// The two hand-over variants carry no target: the loop asks the active tab
/// which row it is on, exactly as the renderer does, so a selection can never
/// be recorded here and then go stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// State updated (or nothing happened); just redraw.
    None,
    /// Reload the active tab's data before redrawing.
    Refresh,
    /// Leave TUI mode and attach to the selection's master (SPEC §17 `o`).
    Attach,
    /// Leave TUI mode and page the selection's brief (SPEC §17 `b`).
    Brief,
    /// Run the open modal's [`Prompt`] (SPEC §17 `n` / `r` / `c` / `R`).
    /// Carries nothing: the loop reads the form — and the Quest id the prompt
    /// was opened against — out of `App::modal`.
    Submit,
    Quit,
}

/// The Quest a prompt was opened against, and enough of what the box said
/// about it to notice if it stopped being true.
///
/// The Quest is carried by **id**, not by "whatever is selected when Enter
/// lands": a tick can reload and reorder the listing while a prompt is up, and
/// closing the wrong Quest because the rows moved under a confirmation is the
/// one failure this shape makes impossible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub quest: String,
    /// The slug the box named. A rename from another terminal makes the box a
    /// lie — about which Quest, and about which tmux session dies with it.
    pub slug: String,
    /// The one column nothing can change. An id can be minted twice: `new_id`
    /// is 16 bits and its retry only checks *live* rows, so a `q rm` followed
    /// by a `q new` can hand the id of a deleted Quest to a new one, and a
    /// re-fetch by id alone would find a stranger.
    pub created_at: i64,
    /// Whether the Quest was finished when the box was drawn. The close and
    /// resume prompts say different things — and mean different things — on
    /// either side of this.
    pub finished: bool,
    /// The beads epic the box named. `q set <slug> beads_epic <other>` from
    /// another terminal would otherwise leave the box saying `bd-e1` while the
    /// refetched Quest closes `bd-e2` (N-6).
    pub epic: Option<String>,
}

/// What an open form will do when it is submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Prompt {
    NewQuest,
    Rename(Target),
    Close(Target),
    Resume(Target),
}

impl Prompt {
    /// The Quest the prompt was opened against, if any.
    pub fn quest(&self) -> Option<&str> {
        self.target().map(|t| t.quest.as_str())
    }

    pub fn target(&self) -> Option<&Target> {
        match self {
            Prompt::NewQuest => None,
            Prompt::Rename(t) | Prompt::Close(t) | Prompt::Resume(t) => Some(t),
        }
    }
}

/// A form on screen, holding the keyboard, together with what submitting it
/// means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modal {
    pub prompt: Prompt,
    pub form: Form,
}

/// One row of the `?` overlay: the keys every tab answers to.
pub const HELP: &[(&str, &str)] = &[
    ("Tab / S-Tab", "next / previous tab"),
    (
        "1 2 3 4",
        "jump to a tab (phone keyboards: no Enter needed)",
    ),
    ("↑ ↓ / k j", "move the selection"),
    (
        "Enter / Ctrl-J",
        "the active tab's Enter (Ctrl-J is its alias)",
    ),
    ("x", "refresh now"),
    ("?", "toggle this help"),
    ("q / Esc", "close the help, or quit"),
    ("Ctrl-C", "quit"),
];

/// The overlay for one tab: the shell's keys, then that tab's own. A tab with
/// no keys of its own shows only the first half.
pub fn help_rows(tab: Tab) -> Vec<(&'static str, &'static str)> {
    let mut rows: Vec<(&str, &str)> = HELP.to_vec();
    let own: &[(&str, &str)] = match tab {
        Tab::Quests => quests::HELP,
        _ => &[],
    };
    if !own.is_empty() {
        rows.push(("", ""));
        rows.extend_from_slice(own);
    }
    rows
}

/// How long a status message holds the bar before the chrome line returns.
/// In seconds rather than ticks so a 10 s remote tick does not leave it up for
/// most of a minute.
const STATUS_SECS: u64 = 8;

/// One space before the first tab label, so the bar is not flush to the edge.
const TAB_LEAD: u16 = 1;
/// Rendered between tab labels.
pub const TAB_SEP: &str = " │ ";

/// Each tab's label and the columns it occupies in the header bar. Shared by
/// the renderer and by mouse hit-testing, so a click always lands where the
/// label is drawn.
pub fn tab_layout() -> Vec<(Tab, String, u16, u16)> {
    let sep = layout::width(TAB_SEP) as u16;
    let mut out = Vec::with_capacity(Tab::ALL.len());
    let mut x = TAB_LEAD;
    for tab in Tab::ALL {
        let label = format!("{} {}", tab.digit(), tab.title());
        let w = layout::width(&label) as u16;
        out.push((tab, label, x, w));
        x += w + sep;
    }
    out
}

/// The tab whose label covers column `col` of a tab bar `bar_width` columns
/// wide. Columns past the bar belong to the machine label, not to the tab
/// whose label the renderer clipped there.
pub fn tab_at_column(col: u16, bar_width: u16) -> Option<Tab> {
    if col >= bar_width {
        return None;
    }
    tab_layout()
        .into_iter()
        .find(|(_, _, x, w)| col >= *x && col < x + w)
        .map(|(tab, _, _, _)| tab)
}

pub struct App {
    /// The machine this `q` speaks for (`--machine` or `[machine] name`).
    pub machine: String,
    pub tab: Tab,
    pub help: bool,
    pub should_quit: bool,
    /// `[ui] rows` — the default row height in the responsive middle band.
    pub rows: u8,
    /// `[ui] mouse`.
    pub mouse: bool,
    /// `[ui] tick_local`, in seconds.
    pub tick_secs: u64,
    /// Last known terminal size; the renderer keeps it current.
    pub width: u16,
    pub height: u16,
    /// Columns the tab bar was last drawn into. The renderer owns it, and
    /// mouse hit-testing reads it so a click can only select a visible tab.
    pub tab_bar_width: u16,
    /// Ticks elapsed, and refreshes asked for — the tabs hang their reload
    /// bookkeeping off these.
    pub ticks: u64,
    pub refreshes: u64,
    /// Transient one-line feedback in the status bar, set by a key handler,
    /// with the tick it was set on: it is *transient*, so the chrome line
    /// comes back rather than a stale "search cleared" owning the bar for the
    /// rest of the session.
    pub status: String,
    status_at: u64,
    /// The last reload that failed, kept apart from `status` so a tick's
    /// success cannot wipe a message a keypress just wrote (and so a failure
    /// survives until the next reload rather than until the next keypress).
    pub refresh_error: Option<String>,
    /// The Quest a tab handed to another tab — `s` on the Quests tab means
    /// "the Sessions tab, filtered to this one" (SPEC §17).
    pub focus_quest: Option<String>,
    /// Whether the right-hand detail panel is up. Shell-level rather than
    /// per-tab: it is one panel, and `Enter` means the same thing everywhere.
    pub detail: bool,
    /// The open form, if any. Shell-level like `help`: it is drawn over the
    /// whole frame whatever tab is behind it, which is what makes
    /// "`capturing()` implies something on screen" true by construction.
    pub modal: Option<Modal>,
    /// Every machine this `q` knows of — the local one first, then the
    /// configured remotes. The new-Quest form's machine field cycles it.
    pub machines: Vec<String>,
    /// `[tmux] session_prefix`, so a prompt can name the tmux session it is
    /// about to kill exactly as `q close` does.
    pub tmux_prefix: String,
    pub quests: quests::State,
    pub sessions: sessions::State,
    pub templates: templates::State,
    pub events: events::State,
}

impl App {
    pub fn new(config: &Config, machine: &str) -> App {
        App {
            machine: machine.to_string(),
            tab: Tab::Quests,
            help: false,
            should_quit: false,
            rows: config.ui.rows,
            mouse: config.ui.mouse,
            tick_secs: config.ui.tick_local.max(1),
            width: 0,
            height: 0,
            tab_bar_width: 0,
            ticks: 0,
            refreshes: 0,
            status: String::new(),
            status_at: 0,
            refresh_error: None,
            focus_quest: None,
            detail: false,
            modal: None,
            machines: machines(config, machine),
            tmux_prefix: config.tmux.session_prefix.clone(),
            quests: quests::State::default(),
            sessions: sessions::State::default(),
            templates: templates::State::default(),
            events: events::State::default(),
        }
    }

    /// Row height for the current width, per SPEC §17's breakpoints.
    pub fn row_mode(&self) -> RowMode {
        layout::row_mode(self.width, self.rows)
    }

    pub fn set_size(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
    }

    pub fn tick(&mut self) {
        self.ticks += 1;
    }

    /// Global keys first, then the active tab's own. Pure: the only way it
    /// reaches the outside world is the `Action` it returns.
    pub fn handle(&mut self, input: Input) -> Action {
        if self.help {
            return self.handle_help(input);
        }
        // A form owns every key but the unconditional escape hatch: `q`, `x`
        // and the digits are text in a field, not quit/refresh/switch-tab.
        if self.modal.is_some() {
            if input == Input::Ctrl('c') {
                return self.quit();
            }
            return self.handle_modal(input);
        }
        // Same rule for a tab capturing text (the `/` search box): the shell's
        // bare-letter keys would eat the typing.
        if self.capturing() {
            if input == Input::Ctrl('c') {
                return self.quit();
            }
        } else if let Some(action) = self.handle_global(input) {
            return action;
        }
        match self.tab {
            Tab::Quests => quests::handle(self, input),
            Tab::Sessions => sessions::handle(self, input),
            Tab::Templates => templates::handle(self, input),
            Tab::Events => events::handle(self, input),
        }
    }

    /// A bracketed paste. Text, and only ever text: it goes into the focused
    /// text field of whatever is capturing, and nowhere else.
    ///
    /// Nothing here can return an [`Action`]. That is the point — a paste
    /// carrying `ESC [ C` and a `CR` used to arrive as an arrow key and an
    /// Enter, which walked a guarded action row off `cancel` and submitted it
    /// (N-1). With no capture on screen a paste is dropped outright rather
    /// than replayed as keys.
    ///
    /// Returns whether anything on screen changed.
    pub fn paste(&mut self, text: &str) -> bool {
        if self.help {
            return false;
        }
        if let Some(modal) = self.modal.as_mut() {
            return modal.form.paste(text);
        }
        match self.tab {
            Tab::Quests => quests::paste(self, text),
            _ => false,
        }
    }

    /// While the overlay is up it swallows everything: only closing it and a
    /// hard quit get through, so `?` can never leave the user stuck.
    fn handle_help(&mut self, input: Input) -> Action {
        match input {
            Input::Ctrl('c') => self.quit(),
            Input::Char('?' | 'q') | Input::Esc | Input::Enter => {
                self.help = false;
                Action::None
            }
            _ => Action::None,
        }
    }

    /// The open form's key. Everything it does is state; submitting leaves
    /// through an `Action` so the process spawning stays in the event loop.
    fn handle_modal(&mut self, input: Input) -> Action {
        let Some(modal) = self.modal.as_mut() else {
            return Action::None;
        };
        match modal.form.handle(input) {
            Outcome::Editing => Action::None,
            Outcome::Cancel => {
                self.dismiss();
                Action::None
            }
            Outcome::Submit => Action::Submit,
        }
    }

    /// Put a form up. The hint goes in the status bar as well as in the box:
    /// on a terminal too short for the box the bar is what says the keyboard
    /// is captured, and while a form is up `current_status` does not expire.
    pub(super) fn open(&mut self, prompt: Prompt, form: Form) {
        self.say(format!("{} · {}", form.title, form.hint));
        self.modal = Some(Modal { prompt, form });
    }

    /// Take the form down and give the bar back.
    pub(super) fn dismiss(&mut self) {
        if self.modal.take().is_some() {
            self.status.clear();
        }
    }

    /// `None` when the key is the active tab's business rather than the shell's.
    fn handle_global(&mut self, input: Input) -> Option<Action> {
        match input {
            Input::Ctrl('c') | Input::Char('q') => Some(self.quit()),
            Input::Char('?') => {
                self.help = true;
                Some(Action::None)
            }
            Input::Tab => {
                self.select(self.tab.next());
                Some(Action::None)
            }
            Input::BackTab => {
                self.select(self.tab.prev());
                Some(Action::None)
            }
            Input::Char('x') => {
                self.refreshes += 1;
                Some(Action::Refresh)
            }
            // Digits jump between tabs — the SPEC §17 affordance for phone ssh
            // clients, where the modified keys are the ones that never arrive.
            _ => {
                let digit = input.digit()?;
                // An out-of-range digit is still the shell's to swallow, not
                // the tab's — otherwise `5` would mean something on one tab
                // only.
                if let Some(tab) = Tab::from_digit(digit) {
                    self.select(tab);
                }
                Some(Action::None)
            }
        }
    }

    /// Whether something is reading raw text rather than commands. True only
    /// while that something is on screen: a form is drawn over every tab, and
    /// the `/` box *is* the status bar.
    pub(super) fn capturing(&self) -> bool {
        if self.modal.is_some() {
            return true;
        }
        match self.tab {
            Tab::Quests => self.quests.capturing(),
            _ => false,
        }
    }

    /// One-line feedback for the next redraw.
    pub fn say(&mut self, message: impl Into<String>) {
        self.status = message.into();
        self.status_at = self.ticks;
    }

    /// The message to show, or `None` once it has had its time. Text being
    /// typed never expires: while a tab is capturing, the status bar *is* the
    /// input box.
    pub fn current_status(&self) -> Option<&str> {
        if self.status.is_empty() {
            return None;
        }
        if self.capturing() {
            return Some(&self.status);
        }
        let ttl = (STATUS_SECS / self.tick_secs.max(1)).max(1);
        (self.ticks.saturating_sub(self.status_at) < ttl).then_some(self.status.as_str())
    }

    /// The active tab's filter indicator, empty when nothing is filtered.
    pub fn filters(&self) -> String {
        match self.tab {
            Tab::Quests => self.quests.filters(),
            _ => String::new(),
        }
    }

    /// The one way to change tabs: every tab switch has to go through the
    /// capture teardown, not only the ones the tab bar drives.
    pub(super) fn select(&mut self, tab: Tab) {
        if self.tab != tab {
            // Whatever was being typed is abandoned with the tab: a capture
            // left armed behind an inactive tab is invisible, and the mouse
            // can switch tabs from inside the box (the keyboard cannot).
            self.cancel_capture();
            self.tab = tab;
            self.status.clear();
        }
    }

    /// Give the keyboard back, whatever was holding it. A form is a Quests-tab
    /// affordance drawn over the whole frame, so leaving the tab abandons it
    /// exactly as it abandons a half-typed `/` query.
    fn cancel_capture(&mut self) {
        self.modal = None;
        if self.tab == Tab::Quests {
            self.quests.cancel_capture();
        }
    }

    fn quit(&mut self) -> Action {
        self.should_quit = true;
        Action::Quit
    }

    /// Mouse, gated on `[ui] mouse` by the event loop that captures it.
    /// A click on the tab bar selects a tab; wheel scroll is the active tab's
    /// vertical movement.
    pub fn handle_mouse(&mut self, ev: MouseInput) -> Action {
        if self.help {
            // A click dismisses the overlay; a wheel scroll over it must not,
            // or a nudge of the mouse loses the help the user just opened.
            if matches!(ev, MouseInput::Click { .. }) {
                self.help = false;
            }
            return Action::None;
        }
        match ev {
            MouseInput::Click { col, row: 0 } => {
                if let Some(tab) = tab_at_column(col, self.tab_bar_width) {
                    self.select(tab);
                }
                Action::None
            }
            MouseInput::Click { .. } => Action::None,
            // A wheel nudge over an open box would step its focus — onto the
            // `close beads epic` toggle, or off the action row — with nothing
            // about the gesture saying a field changed. The list underneath is
            // not scrollable while a box owns the keyboard anyway.
            MouseInput::ScrollUp if self.modal.is_some() => Action::None,
            MouseInput::ScrollDown if self.modal.is_some() => Action::None,
            MouseInput::ScrollUp => self.handle(Input::Up),
            MouseInput::ScrollDown => self.handle(Input::Down),
        }
    }
}

/// The local machine, then every configured remote, de-duplicated and in a
/// stable order.
fn machines(config: &Config, machine: &str) -> Vec<String> {
    let mut out = vec![machine.to_string()];
    for remote in &config.remotes {
        if !out.contains(&remote.name) {
            out.push(remote.name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::form::{self, Field};
    use crate::tui::keys::{self, Input};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn app() -> App {
        let mut app = App::new(&Config::default(), "laptop");
        app.set_size(120, 40);
        // What `render_header` would have published at that width; hit-testing
        // reads it, and these tests never render.
        app.tab_bar_width = 120 - 8;
        app
    }

    /// Drives the app the way the event loop does: a real crossterm key event
    /// through `normalize`, then `handle`.
    fn press(app: &mut App, ev: KeyEvent) -> Action {
        let input = keys::normalize(ev).expect("key has no Input");
        app.handle(input)
    }

    fn ch(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn tab_cycles_forwards_and_backwards() {
        let mut a = app();
        assert_eq!(a.tab, Tab::Quests);
        for want in [Tab::Sessions, Tab::Templates, Tab::Events, Tab::Quests] {
            assert_eq!(a.handle(Input::Tab), Action::None);
            assert_eq!(a.tab, want);
        }
        for want in [Tab::Events, Tab::Templates, Tab::Sessions, Tab::Quests] {
            a.handle(Input::BackTab);
            assert_eq!(a.tab, want);
        }
    }

    #[test]
    fn digit_keys_jump_straight_to_a_tab() {
        let mut a = app();
        for (digit, want) in [
            ('4', Tab::Events),
            ('1', Tab::Quests),
            ('3', Tab::Templates),
            ('2', Tab::Sessions),
        ] {
            press(&mut a, ch(digit));
            assert_eq!(a.tab, want, "digit {digit}");
        }
        // Out of range: swallowed, nothing moves.
        press(&mut a, ch('5'));
        assert_eq!(a.tab, Tab::Sessions);
        press(&mut a, ch('0'));
        assert_eq!(a.tab, Tab::Sessions);
        assert!(!a.should_quit);
    }

    #[test]
    fn ctrl_j_reaches_the_app_as_enter() {
        let mut a = app();
        a.help = true;
        let ctrl_j = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL);
        press(&mut a, ctrl_j);
        assert!(!a.help, "Ctrl-J must act as Enter and dismiss the overlay");
    }

    #[test]
    fn help_toggles_and_swallows_navigation() {
        let mut a = app();
        press(&mut a, ch('?'));
        assert!(a.help);

        // Nothing behind the overlay moves.
        a.handle(Input::Tab);
        press(&mut a, ch('2'));
        assert_eq!(a.tab, Tab::Quests);
        assert!(a.help);

        // `q` closes the overlay instead of quitting.
        press(&mut a, ch('q'));
        assert!(!a.help);
        assert!(!a.should_quit);

        // Esc closes it too.
        press(&mut a, ch('?'));
        a.handle(Input::Esc);
        assert!(!a.help);
    }

    #[test]
    fn ctrl_c_quits_even_from_the_help_overlay() {
        let mut a = app();
        press(&mut a, ch('?'));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(press(&mut a, ctrl_c), Action::Quit);
        assert!(a.should_quit);
    }

    #[test]
    fn q_quits() {
        let mut a = app();
        assert_eq!(press(&mut a, ch('q')), Action::Quit);
        assert!(a.should_quit);
    }

    #[test]
    fn x_asks_the_loop_for_a_refresh() {
        let mut a = app();
        assert_eq!(press(&mut a, ch('x')), Action::Refresh);
        assert_eq!(a.refreshes, 1);
        assert!(!a.should_quit);
        // The refresh is synchronous and the redraw happens after it, so a
        // "refreshing…" message would be painted only once the refresh it
        // announced was already over — and then stick until the next keypress.
        assert!(a.status.is_empty(), "{:?}", a.status);
    }

    #[test]
    fn row_mode_follows_the_width_and_the_config() {
        let mut a = app();
        a.set_size(120, 40);
        assert_eq!(a.row_mode(), RowMode::Two);
        a.set_size(60, 40);
        assert_eq!(a.row_mode(), RowMode::Three);
        a.set_size(80, 40);
        assert_eq!(a.row_mode(), RowMode::Two);
        a.rows = 3;
        assert_eq!(a.row_mode(), RowMode::Three);
    }

    #[test]
    fn tab_hit_testing_matches_the_rendered_labels() {
        let slots = tab_layout();
        assert_eq!(slots.len(), 4);
        let wide = 200;
        for (tab, label, x, w) in &slots {
            assert!(label.starts_with(char::from_digit(tab.digit(), 10).unwrap()));
            assert_eq!(tab_at_column(*x, wide), Some(*tab));
            assert_eq!(tab_at_column(x + w - 1, wide), Some(*tab));
        }
        // The lead space and the separators belong to no tab.
        assert_eq!(tab_at_column(0, wide), None);
        let (_, _, x, w) = slots[0];
        assert_eq!(tab_at_column(x + w, wide), None);
    }

    #[test]
    fn hit_testing_stops_at_the_edge_of_the_drawn_bar() {
        let (last, _, x, w) = tab_layout().into_iter().last().unwrap();
        // Whole label visible: the last column of it still hits.
        assert_eq!(tab_at_column(x + w - 1, x + w), Some(last));
        // Bar clipped mid-label: the columns past the clip belong to whatever
        // the header drew there (the machine name), not to the clipped tab.
        assert_eq!(tab_at_column(x, x + 1), Some(last));
        assert_eq!(tab_at_column(x + 1, x + 1), None);
        // A bar with no room at all can never select anything.
        for col in 0..x + w + 4 {
            assert_eq!(tab_at_column(col, 0), None, "col {col}");
        }
    }

    #[test]
    fn a_click_on_the_tab_bar_selects_that_tab() {
        let mut a = app();
        let (_, _, x, _) = tab_layout()[2];
        a.handle_mouse(MouseInput::Click { col: x, row: 0 });
        assert_eq!(a.tab, Tab::Templates);
        // Clicks below the bar do not change tabs.
        a.handle_mouse(MouseInput::Click { col: x, row: 5 });
        assert_eq!(a.tab, Tab::Templates);
        // A click dismisses the overlay.
        a.help = true;
        a.handle_mouse(MouseInput::Click { col: 0, row: 9 });
        assert!(!a.help);
    }

    /// The keyboard cannot leave the `/` box except through Esc or Enter, but
    /// the mouse can — and a capture left armed behind another tab is
    /// invisible: no box on screen, and every bare letter swallowed as text
    /// the moment the tab comes back.
    #[test]
    fn a_click_on_another_tab_cannot_leave_the_search_box_armed() {
        let mut a = app();
        a.handle(Input::Char('/'));
        a.handle(Input::Char('r'));
        assert!(a.capturing());
        // The box is the status bar while it is open.
        assert!(a.status.contains("/r"), "{}", a.status);

        let (_, _, x, _) = tab_layout()[1];
        a.handle_mouse(MouseInput::Click { col: x, row: 0 });
        assert_eq!(a.tab, Tab::Sessions);
        assert!(!a.capturing(), "the box is still holding the keyboard");
        // A query left behind would filter the list on return with nothing on
        // screen to say so; `filters` reports it once the box is closed.
        a.tab = Tab::Quests;
        assert!(
            a.filters().is_empty(),
            "an uncommitted query is still filtering: {:?}",
            a.filters()
        );
        a.tab = Tab::Sessions;

        // Back on Quests, the shell's keys are the shell's again.
        let (_, _, x, _) = tab_layout()[0];
        a.handle_mouse(MouseInput::Click { col: x, row: 0 });
        assert_eq!(a.tab, Tab::Quests);
        assert!(!a.capturing());
        assert_eq!(a.handle(Input::Char('q')), Action::Quit);
        assert!(a.should_quit);
    }

    /// A message is transient by contract: `status` is the only thing between
    /// the user and the chrome line, so it has to give the bar back.
    #[test]
    fn a_status_message_gives_the_bar_back_after_a_few_ticks() {
        let mut a = app();
        a.say("search cleared");
        assert_eq!(a.current_status(), Some("search cleared"));
        for _ in 0..(STATUS_SECS / a.tick_secs) {
            a.tick();
        }
        assert_eq!(a.current_status(), None, "the message never expires");

        // A newer message starts its own clock.
        a.say("filter /run");
        assert_eq!(a.current_status(), Some("filter /run"));

        // Text being typed is not a message: the box would vanish mid-word.
        a.handle(Input::Char('/'));
        a.handle(Input::Char('r'));
        for _ in 0..100 {
            a.tick();
        }
        assert!(a.capturing());
        assert_eq!(
            a.current_status().map(str::to_string),
            Some(a.status.clone())
        );
    }

    #[test]
    fn scrolling_over_the_help_overlay_leaves_it_open() {
        let mut a = app();
        a.help = true;
        assert_eq!(a.handle_mouse(MouseInput::ScrollDown), Action::None);
        assert!(a.help, "a wheel scroll must not dismiss the overlay");
        assert_eq!(a.handle_mouse(MouseInput::ScrollUp), Action::None);
        assert!(a.help);
        a.handle_mouse(MouseInput::Click { col: 0, row: 9 });
        assert!(!a.help);
    }

    #[test]
    fn tab_digits_and_order_are_stable() {
        assert_eq!(Tab::from_digit(1), Some(Tab::Quests));
        assert_eq!(Tab::from_digit(4), Some(Tab::Events));
        assert_eq!(Tab::from_digit(5), None);
        assert_eq!(Tab::from_digit(0), None);
        for tab in Tab::ALL {
            assert_eq!(Tab::from_digit(tab.digit()), Some(tab));
            assert_eq!(tab.next().prev(), tab);
        }
    }

    /// The mode gate at its purest: with a form up, `handle_global` never
    /// runs. Every key it claims — `q`, `x`, `?`, Tab and the digits — is the
    /// form's, and only Ctrl-C still gets through.
    #[test]
    fn a_form_takes_every_key_the_shell_would_have_claimed() {
        let mut a = app();
        a.handle(Input::Char('n'));
        assert!(a.modal.is_some());
        assert!(a.capturing());

        for c in ['q', 'x', '?', '1', '4', '0'] {
            assert_eq!(a.handle(Input::Char(c)), Action::None, "{c}");
        }
        assert!(!a.should_quit);
        assert!(!a.help);
        assert_eq!(a.tab, Tab::Quests);
        assert_eq!(a.refreshes, 0);
        // Tab moves between fields, not between tabs.
        a.handle(Input::Tab);
        a.handle(Input::BackTab);
        assert_eq!(a.tab, Tab::Quests);
        assert!(a.modal.is_some());

        // Enter alone does nothing: `n` starts a process, so its action row
        // begins on `cancel` (B2).
        assert_eq!(a.handle(Input::Enter), Action::None);
        assert!(a.modal.is_some(), "Enter must not take the form down");
        assert_eq!(a.tab, Tab::Quests);
        // Chosen, it is the submission; the loop does the work.
        for _ in 0..24 {
            if a.modal.as_ref().unwrap().form.focused().map(Field::label) == Some(form::ACTION) {
                break;
            }
            a.handle(Input::Tab);
        }
        a.handle(Input::Right);
        assert_eq!(a.handle(Input::Enter), Action::Submit);
        assert!(a.modal.is_some(), "Enter must not take the form down");
        // Esc does.
        assert_eq!(a.handle(Input::Esc), Action::None);
        assert!(a.modal.is_none());
        assert!(a.status.is_empty());
        assert!(!a.capturing());
        assert_eq!(a.handle(Input::Char('q')), Action::Quit);
    }

    #[test]
    fn ctrl_c_quits_from_inside_a_form() {
        let mut a = app();
        a.handle(Input::Char('n'));
        assert_eq!(a.handle(Input::Ctrl('c')), Action::Quit);
        assert!(a.should_quit);
    }

    #[test]
    fn the_machine_list_is_the_local_machine_then_the_remotes() {
        let mut config = Config::default();
        config.remotes.push(crate::config::Remote {
            name: "ws".to_string(),
            ssh: "ws.local".to_string(),
        });
        // A remote that repeats the local name is not offered twice.
        config.remotes.push(crate::config::Remote {
            name: "laptop".to_string(),
            ssh: "self".to_string(),
        });
        let a = App::new(&config, "laptop");
        assert_eq!(a.machines, ["laptop", "ws"]);
    }

    #[test]
    fn new_reads_the_ui_config() {
        let mut config = Config::default();
        config.ui.rows = 3;
        config.ui.mouse = false;
        config.ui.tick_local = 7;
        let a = App::new(&config, "ws");
        assert_eq!(a.machine, "ws");
        assert_eq!(a.rows, 3);
        assert!(!a.mouse);
        assert_eq!(a.tick_secs, 7);
        assert_eq!(a.tab, Tab::Quests);
    }
}
