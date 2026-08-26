//! The TUI state machine (SPEC §17).
//!
//! `App` holds every piece of state the renderer reads, and `App::handle` is
//! the single, pure key → state transition. Nothing in this module touches a
//! terminal, so the whole keymap is unit-testable.
#![allow(dead_code)]

use crate::config::Config;

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
/// machine cannot do by itself lives here; later beads add variants (attaching
/// to a pane, opening a pager) rather than reaching for the terminal from
/// inside `handle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// State updated (or nothing happened); just redraw.
    None,
    /// Reload the active tab's data before redrawing.
    Refresh,
    Quit,
}

/// One row of the `?` overlay.
pub const HELP: &[(&str, &str)] = &[
    ("Tab / S-Tab", "next / previous tab"),
    (
        "1 2 3 4",
        "jump to a tab (phone keyboards: no Enter needed)",
    ),
    ("↑ ↓ / k j", "move the selection"),
    (
        "Enter / Ctrl-J",
        "open the selection (Ctrl-J is the Enter alias)",
    ),
    ("x", "refresh now"),
    ("?", "toggle this help"),
    ("q / Esc", "close the help, or quit"),
    ("Ctrl-C", "quit"),
];

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
    /// Transient one-line feedback in the status bar.
    pub status: String,
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
        if let Some(action) = self.handle_global(input) {
            return action;
        }
        match self.tab {
            Tab::Quests => quests::handle(self, input),
            Tab::Sessions => sessions::handle(self, input),
            Tab::Templates => templates::handle(self, input),
            Tab::Events => events::handle(self, input),
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

    fn select(&mut self, tab: Tab) {
        if self.tab != tab {
            self.tab = tab;
            self.status.clear();
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
            MouseInput::ScrollUp => self.handle(Input::Up),
            MouseInput::ScrollDown => self.handle(Input::Down),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // "refreshing…" message could never be seen — and used to stick.
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
