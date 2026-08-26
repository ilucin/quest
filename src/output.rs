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
        human()
    };
    match writeln!(std::io::stdout().lock(), "{text}") {
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => Ok(other?),
    }
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
