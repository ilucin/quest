//! The TUI's modal input layer (SPEC §17, §21): a text field, a cycling
//! select, a toggle, validation and an error line — enough for the new-Quest
//! form and for every prompt that follows it.
//!
//! Deliberately generic. `quests` builds the Quest forms out of it and
//! bd-8lz.4.5's send-text prompt is a one-field [`Form`] built the same way,
//! so there is one place where "a box is holding the keyboard" is implemented
//! and one place where it is drawn.
//!
//! Pure, like the rest of the state machine: [`Form::handle`] only edits the
//! form and says what happened, and [`render`] only draws it.
#![allow(dead_code)]

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

use super::keys::Input;
use super::layout;

/// The block the caret is painted as; the terminal's own cursor is hidden
/// while the TUI draws.
const CARET: char = '\u{2588}';
/// Marks the focused row.
const FOCUS: &str = "\u{25b8} ";
/// Two borders plus one column of padding on each side.
const BOX_CHROME: u16 = 4;

/// What a keypress did to a form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Absorbed: the box keeps the keyboard and stays on screen.
    Editing,
    /// Enter — the caller validates and either acts or sets an error.
    Submit,
    /// Esc.
    Cancel,
}

/// One row of a form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field {
    /// Free text with a caret.
    Text {
        label: String,
        value: String,
        /// Byte offset of the caret in `value`, always on a char boundary.
        cursor: usize,
        /// What an empty value means, shown in its place.
        blank: String,
    },
    /// One of a fixed list, cycled with `←`/`→`.
    Select {
        label: String,
        options: Vec<String>,
        at: usize,
    },
    /// Flipped with `←`/`→`/space.
    Toggle { label: String, on: bool },
    /// Something the form has to say. Never focused, so it can never swallow a
    /// keystroke.
    Note(String),
}

impl Field {
    pub fn label(&self) -> &str {
        match self {
            Field::Text { label, .. }
            | Field::Select { label, .. }
            | Field::Toggle { label, .. } => label,
            Field::Note(_) => "",
        }
    }

    fn focusable(&self) -> bool {
        !matches!(self, Field::Note(_))
    }

    /// The value as the box shows it, with the caret when this row has focus.
    fn shown(&self, focused: bool) -> String {
        match self {
            Field::Text {
                value,
                cursor,
                blank,
                ..
            } => {
                if !focused {
                    return if value.is_empty() {
                        blank.clone()
                    } else {
                        value.clone()
                    };
                }
                let (head, tail) = value.split_at(*cursor);
                let mut out = format!("{head}{CARET}{tail}");
                if value.is_empty() {
                    out.push_str(blank);
                }
                out
            }
            Field::Select { options, at, .. } => match options.get(*at) {
                Some(option) => format!("\u{2039} {option} \u{203a}"),
                None => String::new(),
            },
            Field::Toggle { on, .. } => if *on { "[x] yes" } else { "[ ] no" }.to_string(),
            Field::Note(text) => text.clone(),
        }
    }
}

/// A modal form: fields, focus, and whatever the last submission complained
/// about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Form {
    pub title: String,
    /// The one-line "how do I get out of this" that also goes in the status
    /// bar, so a box clipped by a very short terminal is never the only thing
    /// saying the keyboard is captured.
    pub hint: String,
    fields: Vec<Field>,
    /// `usize::MAX` when nothing in the form can take focus.
    focus: usize,
    error: Option<String>,
}

impl Form {
    pub fn new(title: impl Into<String>) -> Form {
        Form {
            title: title.into(),
            hint: "Tab field \u{b7} \u{23ce} ok \u{b7} Esc cancel".to_string(),
            fields: Vec::new(),
            focus: usize::MAX,
            error: None,
        }
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Form {
        self.hint = hint.into();
        self
    }

    /// A text field. `blank` says what leaving it empty will mean.
    pub fn text(self, label: &str, value: &str, blank: &str) -> Form {
        self.push(Field::Text {
            label: label.to_string(),
            value: value.to_string(),
            cursor: value.len(),
            blank: blank.to_string(),
        })
    }

    /// A cycling select. An empty `options` would be a field with no value and
    /// no way to get one, so it degrades to a single placeholder.
    pub fn select(self, label: &str, options: Vec<String>, at: usize) -> Form {
        let options = if options.is_empty() {
            vec!["(none)".to_string()]
        } else {
            options
        };
        let at = at.min(options.len() - 1);
        self.push(Field::Select {
            label: label.to_string(),
            options,
            at,
        })
    }

    pub fn toggle(self, label: &str, on: bool) -> Form {
        self.push(Field::Toggle {
            label: label.to_string(),
            on,
        })
    }

    pub fn note(self, text: impl Into<String>) -> Form {
        self.push(Field::Note(text.into()))
    }

    fn push(mut self, field: Field) -> Form {
        if self.focus == usize::MAX && field.focusable() {
            self.focus = self.fields.len();
        }
        self.fields.push(field);
        self
    }

    // ------------------------------------------------------------- reading

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    fn find(&self, label: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.label() == label)
    }

    fn find_mut(&mut self, label: &str) -> Option<&mut Field> {
        self.fields.iter_mut().find(|f| f.label() == label)
    }

    /// A text field's value, trimmed. Missing labels read as empty rather than
    /// panicking: a form is data, and a caller asking for a field it did not
    /// build is asking for "nothing was given".
    pub fn trimmed(&self, label: &str) -> &str {
        match self.find(label) {
            Some(Field::Text { value, .. }) => value.trim(),
            _ => "",
        }
    }

    /// The same, as `None` when it is blank — the shape every `Option<&str>`
    /// argument in `commands` wants.
    pub fn optional(&self, label: &str) -> Option<&str> {
        Some(self.trimmed(label)).filter(|v| !v.is_empty())
    }

    /// A select's current option, or `""`.
    pub fn choice(&self, label: &str) -> &str {
        match self.find(label) {
            Some(Field::Select { options, at, .. }) => {
                options.get(*at).map(String::as_str).unwrap_or("")
            }
            _ => "",
        }
    }

    pub fn is_on(&self, label: &str) -> bool {
        matches!(self.find(label), Some(Field::Toggle { on: true, .. }))
    }

    /// Fill a text field only when it is still empty: a template's defaults
    /// must never overwrite something already typed.
    pub fn fill_blank(&mut self, label: &str, with: &str) {
        if let Some(Field::Text { value, cursor, .. }) = self.find_mut(label)
            && value.trim().is_empty()
        {
            *value = with.to_string();
            *cursor = value.len();
        }
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
    }

    pub fn focused(&self) -> Option<&Field> {
        self.fields.get(self.focus)
    }

    // ------------------------------------------------------------- editing

    /// Esc cancels, Enter submits, Tab and the arrows move; everything else is
    /// the focused field's — which is what keeps `q`, `x` and the digits text
    /// while a box is up.
    pub fn handle(&mut self, input: Input) -> Outcome {
        match input {
            Input::Esc => Outcome::Cancel,
            Input::Enter => Outcome::Submit,
            Input::Tab | Input::Down => {
                self.step_focus(1);
                Outcome::Editing
            }
            Input::BackTab | Input::Up => {
                self.step_focus(-1);
                Outcome::Editing
            }
            _ => {
                if self.edit(input) {
                    // The complaint was about what was in the box a moment ago.
                    self.error = None;
                }
                Outcome::Editing
            }
        }
    }

    /// Wraps, and skips notes. A form of nothing but notes never moves.
    fn step_focus(&mut self, delta: isize) {
        let n = self.fields.len();
        if n == 0 || !self.fields.iter().any(Field::focusable) {
            return;
        }
        let mut at = if self.focus == usize::MAX {
            0
        } else {
            self.focus
        };
        for _ in 0..n {
            at = (at as isize + delta).rem_euclid(n as isize) as usize;
            if self.fields[at].focusable() {
                self.focus = at;
                return;
            }
        }
    }

    /// Whether the keystroke changed anything.
    fn edit(&mut self, input: Input) -> bool {
        let Some(field) = self.fields.get_mut(self.focus) else {
            return false;
        };
        match field {
            Field::Text { value, cursor, .. } => match input {
                Input::Char(c) => {
                    value.insert(*cursor, c);
                    *cursor += c.len_utf8();
                    true
                }
                Input::Backspace => {
                    if *cursor == 0 {
                        return false;
                    }
                    let at = prev_boundary(value, *cursor);
                    value.replace_range(at..*cursor, "");
                    *cursor = at;
                    true
                }
                Input::Delete => {
                    if *cursor >= value.len() {
                        return false;
                    }
                    let to = next_boundary(value, *cursor);
                    value.replace_range(*cursor..to, "");
                    true
                }
                Input::Left => {
                    let at = prev_boundary(value, *cursor);
                    let moved = at != *cursor;
                    *cursor = at;
                    moved
                }
                Input::Right => {
                    let at = next_boundary(value, *cursor);
                    let moved = at != *cursor;
                    *cursor = at;
                    moved
                }
                Input::Home => {
                    let moved = *cursor != 0;
                    *cursor = 0;
                    moved
                }
                Input::End => {
                    let moved = *cursor != value.len();
                    *cursor = value.len();
                    moved
                }
                // Readline's "kill the line", the one gesture a field this
                // small needs beyond the caret.
                Input::Ctrl('u') => {
                    let had = !value.is_empty();
                    value.clear();
                    *cursor = 0;
                    had
                }
                _ => false,
            },
            Field::Select { options, at, .. } => {
                let n = options.len();
                match input {
                    Input::Left => {
                        *at = (*at + n - 1) % n;
                        true
                    }
                    Input::Right | Input::Char(' ') => {
                        *at = (*at + 1) % n;
                        true
                    }
                    _ => false,
                }
            }
            Field::Toggle { on, .. } => match input {
                Input::Left | Input::Right | Input::Char(' ') => {
                    *on = !*on;
                    true
                }
                _ => false,
            },
            Field::Note(_) => false,
        }
    }
}

fn prev_boundary(s: &str, at: usize) -> usize {
    s[..at]
        .chars()
        .next_back()
        .map(|c| at - c.len_utf8())
        .unwrap_or(0)
}

fn next_boundary(s: &str, at: usize) -> usize {
    s[at..]
        .chars()
        .next()
        .map(|c| at + c.len_utf8())
        .unwrap_or(at)
}

// -------------------------------------------------------------------- render

/// The lines the box holds, flat, so the width can be measured before the
/// widget is built and every line clipped to the same budget.
fn body(form: &Form) -> Vec<(String, Style)> {
    let width = form
        .fields
        .iter()
        .filter(|f| f.focusable())
        .map(|f| layout::width(f.label()))
        .max()
        .unwrap_or(0);
    let mut out: Vec<(String, Style)> = Vec::new();
    for (i, field) in form.fields.iter().enumerate() {
        let focused = i == form.focus;
        let style = if focused {
            Style::default().add_modifier(Modifier::BOLD)
        } else if matches!(field, Field::Note(_)) {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        let marker = if focused { FOCUS } else { "  " };
        let text = match field {
            Field::Note(note) => format!("{marker}{note}"),
            _ => format!(
                "{marker}{:>width$}  {}",
                field.label(),
                field.shown(focused)
            ),
        };
        out.push((text, style));
    }
    if let Some(error) = form.error() {
        out.push((String::new(), Style::default()));
        out.push((
            format!("  {error}"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    out.push((String::new(), Style::default()));
    out.push((
        format!("  {}", form.hint),
        Style::default().add_modifier(Modifier::DIM),
    ));
    out
}

/// Draw `form` centred in `area`.
///
/// A terminal too short for a bordered box gets the focused row on one line
/// rather than nothing: a captured keyboard with no box on screen is the one
/// state this layer must never reach.
pub fn render(frame: &mut Frame, area: Rect, form: &Form) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = body(form);
    let inner = lines
        .iter()
        .map(|(text, _)| layout::width(text))
        .chain(std::iter::once(layout::width(&form.title) + 2))
        .max()
        .unwrap_or(0);
    let want_w = (inner as u16).saturating_add(BOX_CHROME);
    let want_h = (lines.len() as u16).saturating_add(2);

    if area.height < 3 || want_w <= BOX_CHROME {
        render_cramped(frame, area, form);
        return;
    }
    let box_area = layout::centered(area, want_w, want_h);
    let budget = box_area.width.saturating_sub(BOX_CHROME) as usize;
    let shown: Vec<Line> = lines
        .into_iter()
        .take(box_area.height.saturating_sub(2) as usize)
        .map(|(text, style)| Line::from(Span::styled(layout::truncate(&text, budget), style)))
        .collect();

    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Paragraph::new(shown).block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" {} ", form.title))
                .padding(Padding::horizontal(1)),
        ),
        box_area,
    );
}

/// One row, no border: the title and whatever has focus.
fn render_cramped(frame: &mut Frame, area: Rect, form: &Form) {
    let focused = form.focused();
    let tail = match (form.error(), focused) {
        (Some(error), _) => error.to_string(),
        (None, Some(field)) => format!("{} {}", field.label(), field.shown(true)),
        (None, None) => form.hint.clone(),
    };
    let row = Rect { height: 1, ..area };
    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            layout::truncate(&format!("{}: {tail}", form.title), area.width as usize),
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        row,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn form() -> Form {
        Form::new("new quest")
            .text("name", "", "(auto)")
            .text("goal", "ship it", "(none)")
            .select("machine", vec!["laptop".into(), "ws".into()], 0)
            .toggle("beads epic", true)
            .note("a note")
    }

    fn type_in(f: &mut Form, text: &str) {
        for c in text.chars() {
            assert_eq!(f.handle(Input::Char(c)), Outcome::Editing);
        }
    }

    fn draw(form: &Form, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), form))
            .unwrap();
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

    #[test]
    fn focus_starts_on_the_first_focusable_field_and_wraps_past_notes() {
        let mut f = form();
        assert_eq!(f.focused().map(Field::label), Some("name"));
        for want in ["goal", "machine", "beads epic", "name"] {
            f.handle(Input::Tab);
            assert_eq!(f.focused().map(Field::label), Some(want));
        }
        // Backwards skips the note just the same.
        f.handle(Input::BackTab);
        assert_eq!(f.focused().map(Field::label), Some("beads epic"));
    }

    #[test]
    fn a_form_of_nothing_but_notes_has_no_focus_and_moves_nowhere() {
        let mut f = Form::new("close").note("kills tmux q-x");
        assert!(f.focused().is_none());
        f.handle(Input::Tab);
        assert!(f.focused().is_none());
        assert_eq!(f.handle(Input::Char('q')), Outcome::Editing);
        assert_eq!(f.handle(Input::Enter), Outcome::Submit);
    }

    #[test]
    fn typing_edits_the_focused_text_field_only() {
        let mut f = form();
        type_in(&mut f, "cdc-backfill");
        assert_eq!(f.trimmed("name"), "cdc-backfill");
        assert_eq!(f.trimmed("goal"), "ship it");
    }

    #[test]
    fn the_caret_moves_and_edits_mid_string() {
        let mut f = Form::new("t").text("f", "abcd", "");
        f.handle(Input::Left);
        f.handle(Input::Left);
        type_in(&mut f, "X");
        assert_eq!(f.trimmed("f"), "abXcd");
        f.handle(Input::Backspace);
        assert_eq!(f.trimmed("f"), "abcd");
        f.handle(Input::Delete);
        assert_eq!(f.trimmed("f"), "abd");
        f.handle(Input::Home);
        f.handle(Input::Delete);
        assert_eq!(f.trimmed("f"), "bd");
        f.handle(Input::End);
        type_in(&mut f, "!");
        assert_eq!(f.trimmed("f"), "bd!");
        f.handle(Input::Ctrl('u'));
        assert_eq!(f.trimmed("f"), "");
        // Nothing to delete at either edge.
        f.handle(Input::Backspace);
        f.handle(Input::Delete);
        assert_eq!(f.trimmed("f"), "");
    }

    /// A multi-byte character must not be cut in half by a caret move.
    #[test]
    fn the_caret_walks_whole_characters() {
        let mut f = Form::new("t").text("f", "héllo", "");
        f.handle(Input::Home);
        for _ in 0..2 {
            f.handle(Input::Right);
        }
        type_in(&mut f, "X");
        assert_eq!(f.trimmed("f"), "héXllo");
        f.handle(Input::Left);
        f.handle(Input::Backspace);
        assert_eq!(f.trimmed("f"), "hXllo");
    }

    #[test]
    fn a_select_cycles_both_ways_and_wraps() {
        let mut f = Form::new("t").select("m", vec!["a".into(), "b".into(), "c".into()], 0);
        assert_eq!(f.choice("m"), "a");
        f.handle(Input::Right);
        assert_eq!(f.choice("m"), "b");
        f.handle(Input::Char(' '));
        assert_eq!(f.choice("m"), "c");
        f.handle(Input::Right);
        assert_eq!(f.choice("m"), "a");
        f.handle(Input::Left);
        assert_eq!(f.choice("m"), "c");
        // Typing is not text here, and does not move the selection either.
        f.handle(Input::Char('q'));
        assert_eq!(f.choice("m"), "c");
    }

    #[test]
    fn an_empty_select_still_has_a_value() {
        let f = Form::new("t").select("tpl", Vec::new(), 4);
        assert_eq!(f.choice("tpl"), "(none)");
    }

    #[test]
    fn a_toggle_flips() {
        let mut f = Form::new("t").toggle("epic", false);
        assert!(!f.is_on("epic"));
        f.handle(Input::Char(' '));
        assert!(f.is_on("epic"));
        f.handle(Input::Left);
        assert!(!f.is_on("epic"));
        f.handle(Input::Right);
        assert!(f.is_on("epic"));
    }

    #[test]
    fn enter_submits_and_esc_cancels() {
        let mut f = form();
        assert_eq!(f.handle(Input::Enter), Outcome::Submit);
        assert_eq!(f.handle(Input::Esc), Outcome::Cancel);
    }

    /// The shell's bare-letter keys are text in here — the whole point of the
    /// mode gate in `App::handle`.
    #[test]
    fn the_shells_own_keys_are_text_in_a_field() {
        let mut f = Form::new("t").text("f", "", "");
        for c in "qx1234?nrcR".chars() {
            assert_eq!(f.handle(Input::Char(c)), Outcome::Editing);
        }
        assert_eq!(f.trimmed("f"), "qx1234?nrcR");
    }

    #[test]
    fn an_error_survives_navigation_but_not_an_edit() {
        let mut f = form();
        f.set_error("slug `x` is already taken");
        assert_eq!(f.error(), Some("slug `x` is already taken"));
        f.handle(Input::Tab);
        assert!(f.error().is_some(), "moving does not fix the input");
        f.handle(Input::Char('!'));
        assert_eq!(f.error(), None);
    }

    #[test]
    fn fill_blank_never_overwrites() {
        let mut f = form();
        f.fill_blank("goal", "from the template");
        assert_eq!(f.trimmed("goal"), "ship it");
        f.fill_blank("name", "from the template");
        assert_eq!(f.trimmed("name"), "from the template");
        // An unknown label is a no-op, not a panic.
        f.fill_blank("nope", "x");
        assert_eq!(f.trimmed("nope"), "");
        assert_eq!(f.optional("nope"), None);
    }

    #[test]
    fn optional_reads_blank_as_none() {
        let mut f = Form::new("t").text("f", "   ", "");
        assert_eq!(f.optional("f"), None);
        type_in(&mut f, "x");
        assert_eq!(f.optional("f"), Some("x"));
    }

    #[test]
    fn the_box_shows_every_field_the_caret_and_the_hint() {
        let f = form();
        let lines = draw(&f, 60, 20).join("\n");
        for want in [
            "new quest",
            "name",
            "goal",
            "ship it",
            "laptop",
            "[x] yes",
            "a note",
            "Esc cancel",
        ] {
            assert!(lines.contains(want), "missing {want} in\n{lines}");
        }
        assert!(lines.contains(CARET), "no caret in\n{lines}");
    }

    #[test]
    fn the_box_shows_the_error() {
        let mut f = form();
        f.set_error("no such directory: /nope");
        assert!(
            draw(&f, 60, 20)
                .join("\n")
                .contains("no such directory: /nope")
        );
    }

    /// The invariant the mode gate rests on: however small the terminal, the
    /// box says something. A captured keyboard with a blank screen is the one
    /// outcome this layer may not produce.
    #[test]
    fn a_tiny_terminal_still_shows_the_form() {
        let f = form();
        for (w, h) in [(80, 24), (40, 10), (20, 4), (30, 2), (30, 1), (8, 1)] {
            let drawn = draw(&f, w, h).join("");
            assert!(!drawn.trim().is_empty(), "nothing drawn at {w}x{h}");
        }
    }

    #[test]
    fn the_cramped_line_prefers_the_error_over_the_field() {
        let mut f = form();
        assert!(draw(&f, 40, 1).join("").contains("name"));
        f.set_error("boom");
        assert!(draw(&f, 40, 1).join("").contains("boom"));
    }
}
