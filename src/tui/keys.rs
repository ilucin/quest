//! crossterm key events → `Input`, the alphabet the app state machine speaks.
//!
//! This is the only place in the TUI that knows about crossterm's key model.
//! Everything downstream (`app.rs`, the tab modules) matches on `Input`, so a
//! key-driven state transition can be tested without a terminal.
#![allow(dead_code)]

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};

/// One normalized keypress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Enter,
    Esc,
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Backspace,
    Delete,
    /// A plain character, with or without Shift.
    Char(char),
    /// Ctrl + a lowercase character.
    Ctrl(char),
}

impl Input {
    /// The digit this key stands for, if any — the mobile-ssh affordance from
    /// SPEC §17, where digits stand in for keys a phone keyboard cannot send.
    pub fn digit(self) -> Option<u32> {
        match self {
            Input::Char(c) => c.to_digit(10),
            _ => None,
        }
    }
}

/// `None` for key events the app has no alphabet for (modifier presses,
/// key releases, F-keys).
///
/// Two aliases matter for phone ssh clients, where Enter never arrives as
/// Enter (SPEC §17): **Ctrl-J** and Ctrl-M both mean Enter.
pub fn normalize(ev: KeyEvent) -> Option<Input> {
    // Terminals that report release/repeat (kitty protocol) would otherwise
    // fire every binding twice.
    if ev.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    match ev.code {
        // Ctrl-J is LF and Ctrl-M is CR; both are Enter on a terminal that
        // will not send one.
        KeyCode::Char('j' | 'J' | 'm' | 'M') if ctrl => Some(Input::Enter),
        KeyCode::Enter => Some(Input::Enter),
        KeyCode::Esc => Some(Input::Esc),
        KeyCode::Tab => Some(Input::Tab),
        KeyCode::BackTab => Some(Input::BackTab),
        KeyCode::Up => Some(Input::Up),
        KeyCode::Down => Some(Input::Down),
        KeyCode::Left => Some(Input::Left),
        KeyCode::Right => Some(Input::Right),
        KeyCode::PageUp => Some(Input::PageUp),
        KeyCode::PageDown => Some(Input::PageDown),
        KeyCode::Home => Some(Input::Home),
        KeyCode::End => Some(Input::End),
        KeyCode::Backspace => Some(Input::Backspace),
        KeyCode::Delete => Some(Input::Delete),
        KeyCode::Char(c) if ctrl => Some(Input::Ctrl(c.to_ascii_lowercase())),
        KeyCode::Char(c) => Some(Input::Char(c)),
        _ => None,
    }
}

/// The only mouse gestures the TUI reacts to. Enabled only when `[ui] mouse`
/// is on — with capture off, the terminal's own selection keeps working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseInput {
    /// Left button down, in frame coordinates.
    Click {
        col: u16,
        row: u16,
    },
    ScrollUp,
    ScrollDown,
}

pub fn normalize_mouse(ev: MouseEvent) -> Option<MouseInput> {
    match ev.kind {
        MouseEventKind::Down(crossterm::event::MouseButton::Left) => Some(MouseInput::Click {
            col: ev.column,
            row: ev.row,
        }),
        MouseEventKind::ScrollUp => Some(MouseInput::ScrollUp),
        MouseEventKind::ScrollDown => Some(MouseInput::ScrollDown),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn ctrl_j_and_ctrl_m_are_enter() {
        assert_eq!(normalize(ctrl('j')), Some(Input::Enter));
        assert_eq!(normalize(ctrl('J')), Some(Input::Enter));
        assert_eq!(normalize(ctrl('m')), Some(Input::Enter));
        assert_eq!(normalize(key(KeyCode::Enter)), Some(Input::Enter));
        // A bare `j` stays a movement key.
        assert_eq!(normalize(key(KeyCode::Char('j'))), Some(Input::Char('j')));
    }

    #[test]
    fn other_ctrl_keys_keep_their_identity_lowercased() {
        assert_eq!(normalize(ctrl('c')), Some(Input::Ctrl('c')));
        assert_eq!(normalize(ctrl('C')), Some(Input::Ctrl('c')));
    }

    #[test]
    fn shifted_characters_arrive_as_characters() {
        let shifted = KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT);
        assert_eq!(normalize(shifted), Some(Input::Char('R')));
    }

    #[test]
    fn navigation_keys_map_across() {
        for (code, want) in [
            (KeyCode::Up, Input::Up),
            (KeyCode::Down, Input::Down),
            (KeyCode::Left, Input::Left),
            (KeyCode::Right, Input::Right),
            (KeyCode::Tab, Input::Tab),
            (KeyCode::BackTab, Input::BackTab),
            (KeyCode::Esc, Input::Esc),
            (KeyCode::PageUp, Input::PageUp),
            (KeyCode::PageDown, Input::PageDown),
            (KeyCode::Home, Input::Home),
            (KeyCode::End, Input::End),
            (KeyCode::Backspace, Input::Backspace),
            (KeyCode::Delete, Input::Delete),
        ] {
            assert_eq!(normalize(key(code)), Some(want), "{code:?}");
        }
    }

    #[test]
    fn releases_and_unknown_keys_are_dropped() {
        let mut release = key(KeyCode::Char('q'));
        release.kind = KeyEventKind::Release;
        assert_eq!(normalize(release), None);
        assert_eq!(normalize(key(KeyCode::F(1))), None);
        assert_eq!(normalize(key(KeyCode::Null)), None);
    }

    #[test]
    fn mouse_events_reduce_to_click_and_scroll() {
        let at = |kind| MouseEvent {
            kind,
            column: 7,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            normalize_mouse(at(MouseEventKind::Down(
                crossterm::event::MouseButton::Left
            ))),
            Some(MouseInput::Click { col: 7, row: 3 })
        );
        assert_eq!(
            normalize_mouse(at(MouseEventKind::ScrollUp)),
            Some(MouseInput::ScrollUp)
        );
        assert_eq!(
            normalize_mouse(at(MouseEventKind::ScrollDown)),
            Some(MouseInput::ScrollDown)
        );
        assert_eq!(normalize_mouse(at(MouseEventKind::Moved)), None);
        assert_eq!(
            normalize_mouse(at(MouseEventKind::Down(
                crossterm::event::MouseButton::Right
            ))),
            None
        );
    }

    #[test]
    fn digits_are_readable_off_an_input() {
        assert_eq!(Input::Char('3').digit(), Some(3));
        assert_eq!(Input::Char('x').digit(), None);
        assert_eq!(Input::Enter.digit(), None);
    }
}
