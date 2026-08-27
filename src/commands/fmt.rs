//! Human formatting shared by the read commands: ages, tables, path and text
//! shortening. Everything here is pure, so it is unit-tested directly.

use serde_json::Value;

use crate::model::now;

/// `45s`, `4m`, `2h`, `3d` — the coarsest unit that is still non-zero.
pub fn age(ts: i64) -> String {
    age_at(now(), ts)
}

pub fn age_at(now: i64, ts: i64) -> String {
    let secs = (now - ts).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Local wall clock, minute precision.
pub fn stamp(ts: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// UTC wall clock, second precision — for logs that are compared across
/// machines.
pub fn stamp_utc(ts: i64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// `$HOME/x` as `~/x`. A path that merely starts with the same characters is
/// left alone.
pub fn tilde(path: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.to_string();
    };
    tilde_at(&home.to_string_lossy(), path)
}

pub fn tilde_at(home: &str, path: &str) -> String {
    if home.is_empty() || home == "/" {
        return path.to_string();
    }
    let home = home.trim_end_matches('/');
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

/// One line, at most `max` characters, ellipsised. Whitespace runs — newlines
/// included — collapse to a single space.
pub fn oneline(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let keep = max.saturating_sub(1);
    flat.chars().take(keep).collect::<String>() + "…"
}

/// How much of a prompt an event payload keeps: enough to recognise it in
/// `q events`, not enough to turn the log into a transcript.
pub const EVENT_PROMPT_CHARS: usize = 200;

/// At most `max` characters, ellipsised, with the ends trimmed. Newlines are
/// kept — unlike `oneline`, this is for stored text, not a table cell.
pub fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

/// `-` for anything absent, so a column never collapses.
pub fn or_dash(value: Option<&str>) -> String {
    match value {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => "-".to_string(),
    }
}

/// The glyph ramp of SPEC §8's context bar, lowest first.
const CTX_RAMP: [char; 4] = ['\u{2581}', '\u{2583}', '\u{2585}', '\u{2587}'];
/// Cells the bar occupies. Four, so each covers a quarter of the window.
pub const CTX_CELLS: usize = 4;

/// SPEC §8's `▁▃▅▇` context bar for a `ctx_pct` reading.
///
/// Each cell owns a 25-point band and is drawn at the height its band is
/// filled to, quantised onto the ramp in four ~6-point steps — so every glyph
/// SPEC §8 names actually appears, and the bar is monotone in the percentage.
/// A reading above 100 (a hook that reported nonsense) is clamped rather than
/// dropped: the number beside it is the honest part.
pub fn ctx_bar(pct: u8) -> String {
    let pct = (pct as usize).min(100);
    let band = 100 / CTX_CELLS;
    (0..CTX_CELLS)
        .map(|i| {
            let local = pct.saturating_sub(i * band).min(band);
            CTX_RAMP[(local * CTX_RAMP.len() / band).min(CTX_RAMP.len() - 1)]
        })
        .collect()
}

/// An event payload as `k=v k=v`; anything that is not an object falls back to
/// its compact JSON. Absent or empty payloads render as `-`.
pub fn payload(payload: Option<&Value>, max: usize) -> String {
    let Some(value) = payload else {
        return "-".to_string();
    };
    let text = match value {
        Value::Null => return "-".to_string(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", scalar(v)))
            .collect::<Vec<_>>()
            .join(" "),
        other => scalar(other),
    };
    if text.is_empty() {
        return "-".to_string();
    }
    oneline(&text, max)
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Left-aligned columns padded to their widest cell; the last column is never
/// padded, so trailing whitespace never reaches the terminal.
pub fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let columns = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(columns) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let mut out = String::new();
    let line = |out: &mut String, cells: &[String]| {
        let mut parts: Vec<String> = Vec::with_capacity(columns);
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == cells.len() {
                parts.push(cell.clone());
            } else {
                let pad = widths[i].saturating_sub(cell.chars().count());
                parts.push(format!("{cell}{}", " ".repeat(pad)));
            }
        }
        out.push_str(parts.join("  ").trim_end());
    };
    line(
        &mut out,
        &header.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
    );
    for row in rows {
        out.push('\n');
        line(&mut out, row);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_picks_the_coarsest_non_zero_unit() {
        let now = 1_000_000;
        assert_eq!(age_at(now, now), "0s");
        assert_eq!(age_at(now, now - 59), "59s");
        assert_eq!(age_at(now, now - 60), "1m");
        assert_eq!(age_at(now, now - 4 * 60 - 30), "4m");
        assert_eq!(age_at(now, now - 3_600), "1h");
        assert_eq!(age_at(now, now - 2 * 3_600), "2h");
        assert_eq!(age_at(now, now - 3 * 86_400), "3d");
        // A clock that went backwards must not print a negative age.
        assert_eq!(age_at(now, now + 500), "0s");
    }

    #[test]
    fn stamp_utc_is_second_precision_utc() {
        assert_eq!(stamp_utc(0), "1970-01-01 00:00:00");
        assert_eq!(stamp_utc(1_700_000_000), "2023-11-14 22:13:20");
    }

    #[test]
    fn tilde_only_replaces_a_whole_home_component() {
        assert_eq!(tilde_at("/Users/x", "/Users/x/Code/q"), "~/Code/q");
        assert_eq!(tilde_at("/Users/x/", "/Users/x"), "~");
        assert_eq!(tilde_at("/Users/x", "/Users/xavier/q"), "/Users/xavier/q");
        assert_eq!(tilde_at("/", "/Users/x"), "/Users/x");
        assert_eq!(tilde_at("", "/Users/x"), "/Users/x");
    }

    #[test]
    fn oneline_flattens_and_ellipsises() {
        assert_eq!(oneline("a\n  b\tc", 40), "a b c");
        assert_eq!(oneline("abcdef", 6), "abcdef");
        assert_eq!(oneline("abcdef", 5), "abcd…");
        assert_eq!(oneline("", 5), "");
    }

    #[test]
    fn truncate_is_char_safe_and_keeps_newlines() {
        assert_eq!(truncate("  abc ", 10), "abc");
        assert_eq!(truncate("čćžšđ", 3), "čć…");
        assert_eq!(truncate("čćžšđ", 5), "čćžšđ");
        assert_eq!(truncate("a\nb", 10), "a\nb");
    }

    #[test]
    fn payload_renders_an_object_as_key_values() {
        // `preserve_order`: keys render in the order the payload was built.
        let value = serde_json::json!({ "from": "a", "to": "b", "n": 2 });
        assert_eq!(payload(Some(&value), 80), "from=a to=b n=2");
        assert_eq!(payload(None, 80), "-");
        assert_eq!(payload(Some(&serde_json::Value::Null), 80), "-");
        assert_eq!(payload(Some(&serde_json::json!("plain")), 80), "plain");
        assert_eq!(payload(Some(&serde_json::json!({})), 80), "-");
        assert_eq!(payload(Some(&serde_json::json!("")), 80), "-");
    }

    #[test]
    fn table_pads_every_column_but_the_last() {
        let rows = vec![
            vec!["a".to_string(), "long value".to_string()],
            vec!["bbb".to_string(), "x".to_string()],
        ];
        assert_eq!(
            table(&["ID", "V"], &rows),
            "ID   V\na    long value\nbbb  x"
        );
    }

    /// SPEC §8's `▁▃▅▇` bar, at the boundaries that decide what it says.
    #[test]
    fn the_context_bar_reads_every_boundary_the_way_spec_8_draws_it() {
        assert_eq!(ctx_bar(0), "▁▁▁▁");
        assert_eq!(ctx_bar(100), "▇▇▇▇");
        // Each cell owns a quarter, and fills before the next one starts.
        assert_eq!(ctx_bar(25), "▇▁▁▁");
        assert_eq!(ctx_bar(50), "▇▇▁▁");
        assert_eq!(ctx_bar(75), "▇▇▇▁");
        // Every glyph SPEC §8 names actually appears.
        assert_eq!(ctx_bar(41), "▇▅▁▁");
        assert_eq!(ctx_bar(7), "▃▁▁▁");
        assert_eq!(ctx_bar(65), "▇▇▅▁");
        assert_eq!(ctx_bar(70), "▇▇▇▁");
        // Four cells, whatever the reading, so the column never shifts.
        for pct in 0..=100u8 {
            assert_eq!(ctx_bar(pct).chars().count(), CTX_CELLS, "{pct}");
        }
        // Monotone: a higher reading never draws a shorter bar.
        let level = |s: &str| -> usize {
            s.chars()
                .map(|c| CTX_RAMP.iter().position(|r| *r == c).unwrap())
                .sum()
        };
        for pct in 1..=100u8 {
            assert!(level(&ctx_bar(pct)) >= level(&ctx_bar(pct - 1)), "{pct}");
        }
        // A hook that reported nonsense is clamped rather than panicking.
        assert_eq!(ctx_bar(255), "▇▇▇▇");
    }
}
