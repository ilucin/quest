use std::io::{ErrorKind, Write};

use serde::Serialize;

/// Single print path for every command: JSON when `--json`, otherwise the
/// human rendering (only built when actually needed). A closed pipe
/// (`q brief | head`) is not an error.
pub fn emit<T, F>(json: bool, value: &T, human: F) -> anyhow::Result<()>
where
    T: Serialize,
    F: FnOnce() -> String,
{
    let text = if json {
        serde_json::to_string(value)?
    } else {
        sanitize(human())
    };
    match writeln!(std::io::stdout().lock(), "{text}") {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
}

/// Neutralise terminal-driving control characters in human output — ESC, BEL,
/// CR and the rest of C0/C1 — so an event payload or goal carrying `\033]0;…\a`
/// cannot retitle or clear the terminal (bd-8lz.8). `\n` and `\t` are kept: the
/// layout needs them. Mirrors the TUI's own sanitize (`tui::events`); `--json`
/// is already safe via serde, so only the human path passes through here.
pub fn sanitize(text: String) -> String {
    let keep = |c: char| c == '\n' || c == '\t';
    if !text.chars().any(|c| c.is_control() && !keep(c)) {
        return text;
    }
    text.chars()
        .map(|c| {
            if c.is_control() && !keep(c) {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect()
}

/// Centralized error rendering, mirroring `emit`'s two modes. `code` is a
/// stable snake_case identifier (see `QError::code`, or "usage"/"other").
pub fn emit_error(json: bool, msg: &str, code: &str) {
    if json {
        let payload = serde_json::json!({ "error": msg, "code": code });
        eprintln!("{payload}");
    } else {
        eprintln!("error: {msg}");
    }
}

/// First line, escapes stripped, at most `max` chars, ellipsised — a
/// statusline is wide and coloured, and one report line has room for neither.
/// Colour has to go before the cut, or the cut can land inside an escape and
/// leave the terminal wearing it.
pub fn first_line(s: &str, max: usize) -> String {
    let line = strip_ansi(s.lines().next().unwrap_or(""));
    let line = line.trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Drops ANSI escape sequences: CSI (`ESC [ … final`), OSC (`ESC ] … BEL`, or
/// `ESC \\`), and any other two-character escape.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('[') => {
                // Parameter and intermediate bytes, then one final byte @..~.
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut esc = false;
                for c in chars.by_ref() {
                    if c == '\u{7}' || (esc && c == '\\') {
                        break;
                    }
                    esc = c == '\u{1b}';
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_is_one_line_and_bounded() {
        assert_eq!(first_line("  a  \nb", 10), "a");
        assert_eq!(first_line("", 10), "");
        assert_eq!(first_line("čćžšđ", 3), "čć…");
        assert_eq!(first_line("čćžšđ", 5), "čćžšđ");
    }

    #[test]
    fn colour_is_stripped_before_the_line_is_cut() {
        // A coloured statusline: the escapes go, the text stays.
        assert_eq!(first_line("\u{1b}[1;32mctx 42%\u{1b}[0m", 60), "ctx 42%");
        // Cutting a coloured line can no longer land inside an escape.
        let cut = first_line("\u{1b}[31mabcdefghij\u{1b}[0m", 5);
        assert_eq!(cut, "abcd…");
        assert!(!cut.contains('\u{1b}'));

        assert_eq!(strip_ansi("plain"), "plain");
        // OSC 8 hyperlink, BEL-terminated, and the ESC \ form.
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{7}link"), "link");
        assert_eq!(strip_ansi("\u{1b}]0;title\u{1b}\\rest"), "rest");
        // A lone escape at the end must not panic or leak.
        assert_eq!(strip_ansi("a\u{1b}"), "a");
        assert_eq!(strip_ansi("a\u{1b}[38;5;196m"), "a");
    }

    #[test]
    fn sanitize_neutralises_escapes_and_keeps_layout() {
        // bd-8lz.8's repro: an OSC retitle and a clear-screen in a payload.
        let out = sanitize("\u{1b}]0;pwned\u{7}\u{1b}[2Jhi".to_string());
        assert!(
            !out.chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t')
        );
        assert!(out.contains("pwned") && out.contains("hi"));
        // Newlines and tabs survive; the rest becomes the replacement char.
        assert_eq!(sanitize("a\nb\tc".to_string()), "a\nb\tc");
        assert_eq!(sanitize("a\rb".to_string()), "a\u{fffd}b");
        // Clean text is returned untouched (and unallocated).
        assert_eq!(sanitize("plain".to_string()), "plain");
    }
}
