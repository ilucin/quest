//! The TUI's modal input layer (SPEC §17, §21): a text field, a cycling
//! select, a toggle, validation and an error line — enough for the new-Quest
//! form and for every prompt that follows it.
//!
//! Deliberately generic. `quests` builds the Quest forms out of it and
//! bd-8lz.4.5's send-text prompt is a one-field [`Form`] built the same way,
//! so there is one place where "a box is holding the keyboard" is implemented
//! and one place where it is drawn.
//!
//! Every form has to say what Enter means: [`Form::action`] for anything that
//! destroys or spawns, [`Form::harmless`] for anything that does not. There is
//! no third option — a form that says neither refuses to submit at all, which
//! is what keeps the guard from being something each new prompt has to
//! remember (see [`Commit`]).
//!
//! Pure, like the rest of the state machine: [`Form::handle`] only edits the
//! form and says what happened, and [`render`] only draws it.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use unicode_segmentation::UnicodeSegmentation;

use super::keys::Input;
use super::layout;

/// The block the caret is painted as; the terminal's own cursor is hidden
/// while the TUI draws.
const CARET: char = '\u{2588}';
/// Marks the focused row.
const FOCUS: &str = "\u{25b8} ";
/// Two borders plus one column of padding on each side.
const BOX_CHROME: u16 = 4;
/// The label of the row that decides whether a submit means anything.
pub const ACTION: &str = "action";
/// Its non-committal option, and the one it starts on.
pub const CANCEL: &str = "cancel";
/// Marks a cut in a value too wide for the box.
const CUT: char = '\u{2026}';

/// What Enter means on a form with no [`action`](Form::action) row.
///
/// Default-deny, and deliberately not a `bool` with a `false` default that
/// reads as "not yet decided": a prompt that destroys something or starts a
/// process and forgets its action row used to be *submittable by a bare
/// Enter*, so the guard was opt-in and every future prompt had to remember it
/// (N-2). Now forgetting means the form refuses to submit at all — loud, and
/// visible the first time it is opened — and the only way past it is
/// [`Form::harmless`], which is a claim the author has to make out loud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Commit {
    /// Nothing may run until an action row says so.
    Guarded,
    /// Declared harmless: submitting destroys nothing and starts nothing, so
    /// Enter is just Enter. `r` (rename) is the one prompt like this.
    Harmless,
}

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
        /// Whether Space is refused as a way to cycle it. Belt and braces on a
        /// destructive prompt: bracketed paste already makes pasted bytes
        /// text rather than keys, and this keeps a space that reaches the
        /// alphabet some other way — a terminal without the mode, a key
        /// buffered during a stall — from arming what the prompt guards.
        guarded: bool,
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
    ///
    /// `budget` is the columns the value may occupy, when the caller knows it.
    /// A focused text field longer than that is *scrolled* rather than cut at
    /// the end: cutting keeps the start and loses the caret, which is exactly
    /// the state where the user is typing and nothing on screen moves.
    fn shown(&self, focused: bool, budget: Option<usize>) -> String {
        match self {
            Field::Text {
                value,
                cursor,
                blank,
                ..
            } => {
                if !focused {
                    let text = if value.is_empty() { blank } else { value };
                    return match budget {
                        Some(budget) => layout::truncate(text, budget),
                        None => text.clone(),
                    };
                }
                let (head, tail) = value.split_at(*cursor);
                let mut out = format!("{head}{CARET}{tail}");
                if value.is_empty() {
                    out.push_str(blank);
                }
                match budget {
                    Some(budget) => window(&out, layout::width(head), budget),
                    None => out,
                }
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
    /// What Enter means without an action row. See [`Commit`].
    commit: Commit,
}

impl Form {
    pub fn new(title: impl Into<String>) -> Form {
        Form {
            title: title.into(),
            hint: "Tab field \u{b7} \u{23ce} ok \u{b7} Esc cancel".to_string(),
            fields: Vec::new(),
            focus: usize::MAX,
            error: None,
            commit: Commit::Guarded,
        }
    }

    /// Declare that submitting this form destroys nothing and starts nothing,
    /// so it needs no [`action`](Form::action) row. The *only* way to make a
    /// bare Enter mean something without one — see [`Commit`].
    pub fn harmless(mut self) -> Form {
        self.commit = Commit::Harmless;
        self
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
            guarded: false,
        })
    }

    /// The affirmative of a prompt that destroys something or starts a
    /// process, as a row that begins on `cancel`.
    ///
    /// `q close` asks `[y/N]` and reads a bare Enter as *abort*; a box whose
    /// Enter is unconditionally affirmative is a safety regression against
    /// that, and one the user cannot see coming — the keys buffered during a
    /// stall (a hanging `bd`, write-lock contention) arrive as if they had
    /// been typed at the box. So the affirmative is never the default, and
    /// never reachable by a key a paste can carry.
    pub fn action(self, verb: &str) -> Form {
        self.push(Field::Select {
            label: ACTION.to_string(),
            options: vec![CANCEL.to_string(), verb.to_string()],
            at: 0,
            guarded: true,
        })
    }

    /// The verb an [`action`](Form::action) row offers.
    fn verb(&self) -> Option<&str> {
        match self.find(ACTION) {
            Some(Field::Select { options, .. }) => options.last().map(String::as_str),
            _ => None,
        }
    }

    /// Whether a submit means anything: for a form with an action row, that
    /// the row has been moved off `cancel`; without one, that the form was
    /// declared [`harmless`](Form::harmless).
    ///
    /// Fails closed. A prompt that is neither is a programming error, and one
    /// that runs a bare Enter is the hazard this whole row exists to close.
    pub fn confirmed(&self) -> bool {
        match self.find(ACTION) {
            Some(_) => self.choice(ACTION) != CANCEL,
            None => self.commit == Commit::Harmless,
        }
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
            // Not a submit while the action row still reads `cancel`, and not
            // a dismissal either: the box stays up, saying what is missing.
            // Dismissing would throw away everything typed and would look, to
            // someone whose Enter arrived from a buffer, exactly like the
            // action having run.
            Input::Enter => {
                if self.confirmed() {
                    Outcome::Submit
                } else {
                    self.set_error(match self.verb() {
                        Some(verb) => format!(
                            "nothing done \u{b7} choose \u{2039} {verb} \u{203a} on the {ACTION} row (\u{2190}\u{2192}), or Esc"
                        ),
                        // Neither an action row nor `harmless()`: a prompt
                        // built wrong. Refused rather than run, and said so
                        // the first time anybody opens it.
                        None => format!(
                            "nothing done \u{b7} this prompt has no {ACTION} row and is not marked harmless \u{b7} Esc"
                        ),
                    });
                    Outcome::Editing
                }
            }
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

    /// Pasted text, into the focused text field and nowhere else.
    ///
    /// Control characters are dropped rather than inserted: a paste is text,
    /// and the whole point of `Event::Paste` is that the `ESC`, `CR` and `LF`
    /// inside it are *not* keys. Returns whether anything changed.
    pub fn paste(&mut self, text: &str) -> bool {
        let Some(Field::Text { value, cursor, .. }) = self.fields.get_mut(self.focus) else {
            return false;
        };
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return false;
        }
        value.insert_str(*cursor, &clean);
        *cursor += clean.len();
        self.error = None;
        true
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
            Field::Select {
                options,
                at,
                guarded,
                ..
            } => {
                let n = options.len();
                match input {
                    Input::Left => {
                        *at = (*at + n - 1) % n;
                        true
                    }
                    Input::Right => {
                        *at = (*at + 1) % n;
                        true
                    }
                    // Space is a character a paste can carry; a guarded row
                    // moves only on the arrows.
                    Input::Char(' ') if !*guarded => {
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

// The caret walks whole grapheme clusters, the same unit `layout::truncate`
// cuts on and the same unit the terminal paints as one glyph. Stepping by
// `char` instead parks the caret between a base letter and its combining mark,
// where the block glyph wears the accent and Backspace eats only the mark.

fn prev_boundary(s: &str, at: usize) -> usize {
    s[..at]
        .graphemes(true)
        .next_back()
        .map(|g| at - g.len())
        .unwrap_or(0)
}

fn next_boundary(s: &str, at: usize) -> usize {
    s[at..]
        .graphemes(true)
        .next()
        .map(|g| at + g.len())
        .unwrap_or(at)
}

/// At most `budget` columns of `s`, always including the column the caret sits
/// in. Anything cut is marked with `\u{2026}`.
fn window(s: &str, caret: usize, budget: usize) -> String {
    if layout::width(s) <= budget {
        return s.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    // The caret is still within the first screenful: an ordinary end-cut.
    if caret < budget {
        return layout::truncate(s, budget);
    }
    // Scrolled. The caret rides the right edge, one column goes to the marker.
    let keep = budget - 1;
    let from = (caret + 1).saturating_sub(keep);
    let mut out = String::new();
    let mut col = 0;
    let mut used = 0;
    for cluster in s.graphemes(true) {
        let w = layout::width(cluster);
        if col + w <= from {
            col += w;
            continue;
        }
        if used + w > keep {
            break;
        }
        out.push_str(cluster);
        used += w;
        col += w;
    }
    format!("{CUT}{out}")
}

// -------------------------------------------------------------------- render

/// The field rows, and the footer that must survive whatever the box cannot
/// fit. `budget`, when known, is the columns one whole row may occupy.
///
/// Split in two because they do not compete on equal terms: a box shorter than
/// its content used to drop from the bottom, which threw away the error line
/// and the hint — the reason a submit failed and the only way out — and kept
/// the fields, which are already on screen.
fn rows(form: &Form, budget: Option<usize>) -> Vec<(String, Style)> {
    let label_w = form
        .fields()
        .iter()
        .filter(|f| f.focusable())
        .map(|f| layout::width(f.label()))
        .max()
        .unwrap_or(0);
    // What is left of a row once the focus marker, the label and the gap
    // between them have taken their columns.
    let value_budget = budget.map(|b| b.saturating_sub(label_w + 4));
    form.fields()
        .iter()
        .enumerate()
        .map(|(i, field)| {
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
                    "{marker}{:>label_w$}  {}",
                    field.label(),
                    field.shown(focused, value_budget)
                ),
            };
            (text, style)
        })
        .collect()
}

/// The error, then the hint — in that order, and that is also the order they
/// survive in: with room for one line the reason a submit failed beats the way
/// out, because the box covers the status bar and the hint is repeated there.
fn footer(form: &Form) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    if let Some(error) = form.error() {
        out.push((
            format!("  {error}"),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
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
    let (rows, footer) = (rows(form, None), footer(form));
    let inner = rows
        .iter()
        .chain(footer.iter())
        .map(|(text, _)| layout::width(text))
        .chain(std::iter::once(layout::width(&form.title) + 2))
        .max()
        .unwrap_or(0);
    let want_w = (inner as u16).saturating_add(BOX_CHROME);
    // One blank line between the fields and the footer.
    let want_h = (rows.len() + footer.len() + 1) as u16 + 2;

    if area.height < 3 || want_w <= BOX_CHROME {
        render_cramped(frame, area, form);
        return;
    }
    let box_area = layout::centered(area, want_w, want_h);
    let budget = box_area.width.saturating_sub(BOX_CHROME) as usize;
    let height = box_area.height.saturating_sub(2) as usize;
    // Re-laid out against the width actually granted, so a focused field wider
    // than the box scrolls with its caret rather than being cut at the start.
    let rows = self::rows(form, Some(budget));

    // The footer takes its lines first, whatever they cost the fields: the
    // fields are already on screen, the error and the hint are not.
    let kept = footer.len().min(height);
    let room = height - kept;
    let mut shown: Vec<(String, Style)> = Vec::with_capacity(height);
    shown.extend(rows.into_iter().take(room));
    if shown.len() < room {
        shown.push((String::new(), Style::default()));
    }
    shown.extend(footer.into_iter().take(kept));

    let lines: Vec<Line> = shown
        .into_iter()
        .map(|(text, style)| Line::from(Span::styled(layout::truncate(&text, budget), style)))
        .collect();

    frame.render_widget(Clear, box_area);
    frame.render_widget(
        Paragraph::new(lines).block(
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
    let head = format!("{}: ", form.title);
    // What the value may have once the title and the label are paid for.
    let budget = (area.width as usize).saturating_sub(layout::width(&head));
    let tail = match (form.error(), form.focused()) {
        (Some(error), _) => error.to_string(),
        (None, Some(field)) => {
            let label = field.label();
            let value = field.shown(true, Some(budget.saturating_sub(label.len() + 1)));
            format!("{label} {value}")
        }
        (None, None) => form.hint.clone(),
    };
    let row = Rect { height: 1, ..area };
    frame.render_widget(Clear, row);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            layout::truncate(&format!("{head}{tail}"), area.width as usize),
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

    /// A plain data form: no action row, so it has to say it is harmless
    /// before Enter means anything (N-2).
    fn form() -> Form {
        Form::new("new quest")
            .harmless()
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
        let mut f = Form::new("close").harmless().note("kills tmux q-x");
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

    /// The caret steps by grapheme cluster — the unit `layout::truncate` cuts
    /// on and the unit the terminal paints. By `char` it parks between a base
    /// letter and its combining mark: the block glyph wears the accent, and
    /// Backspace eats only the mark.
    #[test]
    fn the_caret_walks_whole_grapheme_clusters() {
        // `e` + U+0301, painted as one `é`.
        let mut f = Form::new("t").text("f", "cafe\u{301}", "");
        f.handle(Input::Left);
        assert_eq!(f.trimmed("f"), "cafe\u{301}");
        type_in(&mut f, "X");
        assert_eq!(
            f.trimmed("f"),
            "cafXe\u{301}",
            "the caret split the cluster"
        );
        f.handle(Input::End);
        f.handle(Input::Backspace);
        assert_eq!(
            f.trimmed("f"),
            "cafX",
            "Backspace left a naked combining mark"
        );
        // Delete, from the other side.
        let mut f = Form::new("t").text("f", "e\u{301}x", "");
        f.handle(Input::Home);
        f.handle(Input::Delete);
        assert_eq!(f.trimmed("f"), "x");
        // And a flag, which is a pair of regional indicators.
        let mut f = Form::new("t").text("f", "\u{1f1ed}\u{1f1f7}!", "");
        f.handle(Input::Home);
        f.handle(Input::Right);
        type_in(&mut f, "Z");
        assert_eq!(f.trimmed("f"), "\u{1f1ed}\u{1f1f7}Z!");
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
        // `form()` is `harmless()`; an undeclared one would refuse.
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

    /// A label the form does not have reads as "nothing was given" rather
    /// than panicking — a form is data, and its readers are shared.
    #[test]
    fn an_unknown_label_reads_as_blank() {
        let f = form();
        assert_eq!(f.trimmed("nope"), "");
        assert_eq!(f.optional("nope"), None);
        assert_eq!(f.choice("nope"), "");
        assert!(!f.is_on("nope"));
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
        let mut f = form();
        f.set_error("no such directory: /nope");
        // Narrow-and-tall as well as short-and-wide: the box is on the render
        // path at every size a terminal can be.
        for (w, h) in [
            (80, 24),
            (40, 10),
            (20, 4),
            (30, 2),
            (30, 1),
            (8, 1),
            (8, 40),
            (3, 30),
            (1, 40),
            (2, 24),
            (200, 3),
            (1, 1),
        ] {
            let drawn = draw(&f, w, h);
            for line in &drawn {
                assert!(layout::width(line) <= w as usize, "{w}x{h}: {line:?}");
            }
            assert!(
                !drawn.join("").trim().is_empty(),
                "nothing drawn at {w}x{h}"
            );
        }
    }

    // ------------------------------------------- the affirmative is a choice

    #[test]
    fn a_form_with_an_action_row_does_not_submit_on_a_bare_enter() {
        let mut f = Form::new("close x?").action("close").note("kills tmux q-x");
        assert!(!f.confirmed());
        for _ in 0..3 {
            assert_eq!(f.handle(Input::Enter), Outcome::Editing);
        }
        // And it says what is missing, rather than looking broken.
        let error = f.error().unwrap();
        assert!(error.contains("nothing done"), "{error}");
        assert!(error.contains("close"), "{error}");

        f.handle(Input::Right);
        assert!(f.confirmed());
        assert_eq!(f.handle(Input::Enter), Outcome::Submit);
        // Esc is still the way out at any point.
        assert_eq!(f.handle(Input::Esc), Outcome::Cancel);
    }

    /// Bracketed paste is off, so pasted text arrives as ordinary keys. Space
    /// cycles an ordinary select; on the action row it must not, or a space in
    /// a paste arms the very thing the row guards and the newline after it
    /// fires.
    #[test]
    fn space_cycles_an_ordinary_select_but_never_the_action_row() {
        let mut f = Form::new("t")
            .select("m", vec!["a".into(), "b".into()], 0)
            .action("close");
        f.handle(Input::Char(' '));
        assert_eq!(f.choice("m"), "b");

        f.handle(Input::Tab);
        assert_eq!(f.focused().map(Field::label), Some(ACTION));
        for c in "fix the thing".chars() {
            f.handle(Input::Char(c));
        }
        assert!(!f.confirmed(), "a pasted space armed the action");
        assert_eq!(f.handle(Input::Enter), Outcome::Editing);
        // The arrows, which no paste carries as text, still work both ways.
        f.handle(Input::Right);
        assert!(f.confirmed());
        f.handle(Input::Left);
        assert!(!f.confirmed());
    }

    /// N-2. A form with no action row submits on Enter **only** once it has
    /// declared itself harmless. Before this, no action row meant `confirmed()
    /// == true`, so the guard was opt-in and a prompt that destroys something
    /// and forgets `.action(...)` silently regained the bare-Enter hazard —
    /// which is exactly the prompt bd-8lz.4.5 is about to add.
    #[test]
    fn a_form_that_declares_itself_harmless_submits_on_enter() {
        let mut f = Form::new("rename x").harmless().text("slug", "x", "");
        assert!(f.confirmed());
        assert_eq!(f.handle(Input::Enter), Outcome::Submit);
    }

    /// The other half, and the one that matters: forgetting BOTH fails
    /// closed. A prompt built this way can never run, and says so.
    #[test]
    fn a_form_with_neither_an_action_row_nor_a_harmless_claim_refuses_to_submit() {
        let mut f = Form::new("send text").text("text", "rm -rf /", "");
        assert!(!f.confirmed(), "a bare Enter would have run this");
        assert_eq!(f.handle(Input::Enter), Outcome::Editing);
        assert_eq!(f.handle(Input::Enter), Outcome::Editing);
        let said = f.error().unwrap();
        assert!(said.contains("nothing done"), "{said}");
        assert!(said.contains(ACTION), "{said}");
    }

    /// And every prompt the TUI actually opens is on one side of that line or
    /// the other — the `quests` module's own tests pin which.
    #[test]
    fn declaring_a_form_harmless_is_the_only_way_past_a_missing_action_row() {
        // An action row is enough on its own, and still starts on `cancel`.
        let guarded = Form::new("close x").action("close");
        assert!(!guarded.confirmed());
        // `harmless()` does not override an action row that is still on
        // `cancel`: the row wins, so a stray claim cannot re-arm a guard.
        let both = Form::new("close x").harmless().action("close");
        assert!(!both.confirmed());
    }

    // -------------------------------------------------------- what is drawn

    /// The error is the reason a submit failed and the hint is the way out. On
    /// a box too short for its content they used to be the first two lines
    /// dropped, and the box covers the status bar, so the reason appeared
    /// nowhere at all.
    #[test]
    fn the_error_and_the_hint_survive_a_box_too_short_for_them() {
        let mut f = form();
        f.set_error("no such directory: /nope");
        for h in [20u16, 9, 7, 5, 4, 3] {
            let drawn = draw(&f, 60, h).join("\n");
            assert!(
                drawn.contains("no such directory"),
                "the reason vanished at 60x{h}:\n{drawn}"
            );
            // With room for only one of the two the error wins; the hint is
            // also in the status bar, and the reason is not.
            if h > 3 {
                assert!(
                    drawn.contains("Esc cancel"),
                    "the way out vanished at 60x{h}:\n{drawn}"
                );
            }
        }
    }

    /// With no error to show, the hint alone still gets the last line.
    #[test]
    fn the_hint_survives_on_its_own() {
        let f = form();
        for h in [20u16, 6, 4, 3] {
            let drawn = draw(&f, 60, h).join("\n");
            assert!(drawn.contains("Esc cancel"), "at 60x{h}:\n{drawn}");
        }
    }

    /// Typing past the right edge of the box used to give no feedback at all:
    /// the value was built whole and then cut from the right, taking the caret
    /// with it. The field scrolls instead.
    #[test]
    fn a_text_field_scrolls_to_keep_its_caret_in_view() {
        let mut f = Form::new("t").text("goal", "", "");
        type_in(&mut f, "make the CDC backfill idempotent across retries");
        let drawn = draw(&f, 30, 8).join("\n");
        assert!(drawn.contains(CARET), "the caret left the box:\n{drawn}");
        assert!(drawn.contains("retries"), "the tail is not shown:\n{drawn}");
        assert!(drawn.contains(CUT), "no cut marker:\n{drawn}");

        // Home scrolls back, and the head is shown from the start.
        f.handle(Input::Home);
        let drawn = draw(&f, 30, 8).join("\n");
        assert!(drawn.contains("make the"), "{drawn}");
        assert!(drawn.contains(CARET), "{drawn}");
        assert!(!drawn.contains("retries"), "{drawn}");
    }

    #[test]
    fn the_window_keeps_the_caret_and_marks_what_it_cut() {
        // Fits: untouched.
        assert_eq!(window("abc", 1, 8), "abc");
        // Caret still near the head: an ordinary end-cut.
        assert_eq!(window("abcdefgh", 0, 4), "abc\u{2026}");
        // Scrolled: the caret rides the right edge, the head is marked.
        assert_eq!(window("abcdefgh", 7, 4), "\u{2026}fgh");
        assert_eq!(window("abcdefgh", 8, 4), "\u{2026}gh");
        // Degenerate budgets never panic.
        assert_eq!(window("abcdefgh", 4, 0), "");
        assert_eq!(window("abcdefgh", 7, 1), "\u{2026}");
    }

    /// Wide and combining characters are cut between clusters, never through
    /// one — a cut through a wide char paints half a glyph, and a cut through
    /// a flag leaves a stray letter.
    #[test]
    fn a_scrolled_field_never_splits_a_glyph() {
        // A two-column cluster that does not fit is left out whole.
        assert_eq!(window("日本語", 0, 4), "日\u{2026}");
        assert_eq!(window("日本語", 6, 4), "\u{2026}語");
        // A wider budget shows more context, still whole clusters.
        assert_eq!(window("日本語", 6, 5), "\u{2026}本語");
        // `e` + combining acute is one cluster, so it moves as one.
        assert_eq!(window("abcde\u{301}", 5, 3), "\u{2026}e\u{301}");
        // A flag is a pair of regional indicators; half of one is a letter.
        assert_eq!(
            window("xxxx\u{1f1ed}\u{1f1f7}", 6, 4),
            "\u{2026}\u{1f1ed}\u{1f1f7}"
        );
        assert_eq!(
            window("\u{1f1ed}\u{1f1f7}xxxx", 0, 3),
            "\u{1f1ed}\u{1f1f7}\u{2026}"
        );

        // And the whole box survives every width, at every caret position.
        let mut f = Form::new("t").text("f", "", "");
        type_in(&mut f, "日本語のテキストはとても長い");
        for w in 1..40u16 {
            for _ in 0..14 {
                assert!(!draw(&f, w, 8).join("").is_empty(), "nothing drawn at {w}");
                f.handle(Input::Left);
            }
            f.handle(Input::End);
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
