use serde::Serialize;

/// Single print path for every command: JSON when `--json`, otherwise the
/// human rendering (only built when actually needed).
pub fn emit<T, F>(json: bool, value: &T, human: F)
where
    T: Serialize,
    F: FnOnce() -> String,
{
    if json {
        match serde_json::to_string(value) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("{{\"error\": \"serialize: {e}\"}}"),
        }
    } else {
        println!("{}", human());
    }
}

/// Centralized error rendering, mirroring `emit`'s two modes.
pub fn emit_error(json: bool, msg: &str) {
    if json {
        let payload = serde_json::json!({ "error": msg });
        eprintln!("{payload}");
    } else {
        eprintln!("error: {msg}");
    }
}
