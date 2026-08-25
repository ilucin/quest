use serde::Serialize;

/// Single print path for every command: JSON when `--json`, otherwise the
/// human rendering (only built when actually needed).
pub fn emit<T, F>(json: bool, value: &T, human: F) -> anyhow::Result<()>
where
    T: Serialize,
    F: FnOnce() -> String,
{
    if json {
        println!("{}", serde_json::to_string(value)?);
    } else {
        println!("{}", human());
    }
    Ok(())
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
