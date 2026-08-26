//! Pure layout arithmetic for the TUI (SPEC §17). No terminal, no state —
//! everything here is a function of a width or a `Rect`, so the breakpoints
//! can be unit-tested at their boundaries.
#![allow(dead_code)]

use ratatui::layout::Rect;
use unicode_width::UnicodeWidthChar;

/// At or above this width a Quest row fits on two lines (SPEC §17).
pub const WIDE_COLS: u16 = 100;
/// Below this width two lines no longer fit and rows go to three.
pub const NARROW_COLS: u16 = 70;

/// How many lines one list row occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowMode {
    Two,
    Three,
}

impl RowMode {
    pub fn lines(self) -> u16 {
        match self {
            RowMode::Two => 2,
            RowMode::Three => 3,
        }
    }

    /// `[ui] rows` is a `u8`; anything outside 2..=3 (which config validation
    /// already rejects) falls back to two.
    pub fn from_config(rows: u8) -> RowMode {
        if rows >= 3 {
            RowMode::Three
        } else {
            RowMode::Two
        }
    }
}

/// SPEC §17: ≥100 columns is always two-line, <70 is always three-line. In
/// between, `[ui] rows` decides — that is what the config knob is for.
pub fn row_mode(width: u16, configured: u8) -> RowMode {
    if width >= WIDE_COLS {
        RowMode::Two
    } else if width < NARROW_COLS {
        RowMode::Three
    } else {
        RowMode::from_config(configured)
    }
}

/// The three fixed bands every tab renders into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chrome {
    /// Tab bar plus machine name.
    pub header: Rect,
    /// The active tab's own area.
    pub body: Rect,
    /// One-line hint bar.
    pub status: Rect,
}

/// Deliberately arithmetic rather than a `Layout`: on a terminal too short
/// for all three bands the constraint solver picks a winner of its own, and
/// the tab bar is the one band that must survive.
pub fn chrome(area: Rect) -> Chrome {
    let header_h = area.height.min(1);
    let status_h = area.height.saturating_sub(header_h).min(1);
    let body_h = area.height.saturating_sub(header_h + status_h);
    let row = |y: u16, h: u16| Rect {
        x: area.x,
        y: area.y + y,
        width: area.width,
        height: h,
    };
    Chrome {
        header: row(0, header_h),
        body: row(header_h, body_h),
        status: row(header_h + body_h, status_h),
    }
}

/// How wide a right-hand chrome segment (the machine label, the key hint) may
/// be on a row `total` columns wide. A fixed `Constraint::Length` starves the
/// left band on a narrow terminal — at 60 columns a 302-column machine name
/// leaves the tab bar zero columns — so the left band always keeps a third.
pub fn right_segment(total: u16, want: u16) -> u16 {
    want.min(total.saturating_sub(total.div_ceil(3)))
}

/// A `w`×`h` box centred in `area`, clamped to it — the help overlay.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Display width of a string in terminal cells.
pub fn width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Truncate to `max` display columns, ellipsising when anything was cut.
/// Column-aware rather than char-aware, so CJK and emoji do not overflow.
pub fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if used + cw > max.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_mode_at_the_breakpoints() {
        // Wide is two-line whatever the config says.
        assert_eq!(row_mode(WIDE_COLS, 3), RowMode::Two);
        assert_eq!(row_mode(200, 3), RowMode::Two);
        // Narrow is three-line whatever the config says.
        assert_eq!(row_mode(NARROW_COLS - 1, 2), RowMode::Three);
        assert_eq!(row_mode(0, 2), RowMode::Three);
        // The band in between is the config's to decide.
        assert_eq!(row_mode(NARROW_COLS, 2), RowMode::Two);
        assert_eq!(row_mode(NARROW_COLS, 3), RowMode::Three);
        assert_eq!(row_mode(WIDE_COLS - 1, 2), RowMode::Two);
        assert_eq!(row_mode(WIDE_COLS - 1, 3), RowMode::Three);
    }

    #[test]
    fn row_mode_lines_match_the_variant() {
        assert_eq!(RowMode::Two.lines(), 2);
        assert_eq!(RowMode::Three.lines(), 3);
        assert_eq!(RowMode::from_config(2), RowMode::Two);
        assert_eq!(RowMode::from_config(3), RowMode::Three);
        // Out-of-range values never panic; config validation rejects them anyway.
        assert_eq!(RowMode::from_config(0), RowMode::Two);
        assert_eq!(RowMode::from_config(9), RowMode::Three);
    }

    #[test]
    fn chrome_reserves_one_line_top_and_bottom() {
        let c = chrome(Rect::new(0, 0, 80, 24));
        assert_eq!(c.header, Rect::new(0, 0, 80, 1));
        assert_eq!(c.body, Rect::new(0, 1, 80, 22));
        assert_eq!(c.status, Rect::new(0, 23, 80, 1));
    }

    #[test]
    fn chrome_survives_a_terminal_too_short_for_it() {
        let c = chrome(Rect::new(0, 0, 20, 1));
        assert_eq!(c.header.height, 1);
        assert_eq!(c.body.height, 0);
        assert_eq!(c.status.height, 0);
    }

    #[test]
    fn right_segment_never_starves_the_left_band() {
        // Room for everything: the segment gets exactly what it asked for.
        assert_eq!(right_segment(100, 8), 8);
        assert_eq!(right_segment(80, 29), 29);
        // Cramped: the left band keeps at least a third of the row.
        assert_eq!(right_segment(30, 29), 20);
        assert_eq!(right_segment(20, 29), 13);
        // A machine name longer than the terminal cannot wipe the tab bar.
        assert_eq!(right_segment(60, 302), 40);
        for total in 0..=200u16 {
            let left = total - right_segment(total, u16::MAX);
            assert!(left >= total / 3, "total {total} left {left}");
        }
    }

    #[test]
    fn centered_is_clamped_to_its_parent() {
        assert_eq!(
            centered(Rect::new(0, 0, 80, 24), 40, 10),
            Rect::new(20, 7, 40, 10)
        );
        assert_eq!(
            centered(Rect::new(0, 0, 10, 4), 40, 10),
            Rect::new(0, 0, 10, 4)
        );
    }

    #[test]
    fn truncate_counts_display_columns() {
        assert_eq!(truncate("abcdef", 10), "abcdef");
        assert_eq!(truncate("abcdef", 6), "abcdef");
        assert_eq!(truncate("abcdef", 5), "abcd…");
        assert_eq!(truncate("abcdef", 0), "");
        // Wide characters take two columns each.
        assert_eq!(width("日本語"), 6);
        assert_eq!(truncate("日本語", 6), "日本語");
        assert_eq!(truncate("日本語", 5), "日本…");
        assert!(width(&truncate("日本語", 5)) <= 5);
    }
}
