//! The pager the TUI hands text to (SPEC §17: `b` — "brief u pageru").
//!
//! Only *what* to run and *how* lives here; leaving and re-entering TUI mode
//! around it is [`super::handoff`]'s job, the same one the tmux attach uses.

use std::io::Write;
use std::process::{Command, Stdio};

use crate::error::QError;

/// `-R` keeps the colour escapes a brief may carry. Deliberately *not* `-F`:
/// the TUI's alternate screen comes back the moment the pager exits, so a
/// pager that quits on its own for a short brief would flash and vanish.
const DEFAULT: &[&str] = &["less", "-R"];

/// The pager to run: `$PAGER` when it is set to something, else [`DEFAULT`].
///
/// Split on whitespace rather than run through a shell: `$PAGER` is a command
/// with flags (`less -R`), not a script, and a TUI must not hand a user's
/// environment to `sh -c`.
pub fn command() -> Vec<String> {
    let configured = std::env::var("PAGER").unwrap_or_default();
    let parts: Vec<String> = configured.split_whitespace().map(str::to_string).collect();
    if parts.is_empty() {
        return DEFAULT.iter().map(|s| (*s).to_string()).collect();
    }
    parts
}

/// Page `text`, returning when the pager exits.
///
/// The terminal is already out of TUI mode by the time this runs, so the child
/// inherits a normal one and can do whatever it likes with it.
pub fn show(text: &str) -> anyhow::Result<()> {
    let argv = command();
    let (program, args) = argv.split_first().expect("command is never empty");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| QError::Other(format!("cannot run pager `{program}`: {e}")))?;
    // Dropped before the wait: a pager reads until EOF, and a `stdin` still
    // held open here would hang the TUI for good.
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| QError::Other("pager has no stdin".to_string()))?;
        // A pager the user quits early closes the pipe under us; that is how
        // `q` finds out, not a failure to report.
        let _ = stdin.write_all(text.as_bytes());
        let _ = stdin.flush();
    }
    child
        .wait()
        .map_err(|e| QError::Other(format!("pager `{program}` failed: {e}")))?;
    Ok(())
}

/// Run `f` with `$PAGER` set to `value`, then put the environment back.
/// The variable is process-global, so every test that sets it — here and in
/// the TUI's own — has to share this one lock.
#[cfg(test)]
pub(super) fn with_pager<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    static PAGER_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _lock = PAGER_ENV.lock().unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("PAGER");
    // Safety: single-threaded within the lock, and put back below.
    unsafe {
        match value {
            Some(v) => std::env::set_var("PAGER", v),
            None => std::env::remove_var("PAGER"),
        }
    }
    let out = f();
    unsafe {
        match previous {
            Some(v) => std::env::set_var("PAGER", v),
            None => std::env::remove_var("PAGER"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pager_comes_from_the_environment_with_a_working_default() {
        with_pager(None, || assert_eq!(command(), DEFAULT));
        // Set but empty, and set to nothing but spaces: both mean "unset".
        with_pager(Some(""), || assert_eq!(command(), DEFAULT));
        with_pager(Some("   "), || assert_eq!(command(), DEFAULT));
        // Flags come along; the program is never handed to a shell.
        with_pager(Some("less -R"), || assert_eq!(command(), ["less", "-R"]));
        with_pager(Some("bat  --paging always"), || {
            assert_eq!(command(), ["bat", "--paging", "always"]);
        });
    }

    /// The brief has to reach the pager's stdin, and the call has to come back
    /// — a pager left holding the pipe would wedge the TUI with no way out.
    #[test]
    fn the_text_reaches_the_pager_and_the_call_returns() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("paged");
        with_pager(Some(&format!("tee {}", out.display())), || {
            show("# brief\n\nsecond line\n").unwrap();
        });
        let seen = std::fs::read_to_string(&out).unwrap();
        assert!(seen.contains("second line"), "{seen}");
    }

    /// A pager that is not installed is a status message, not the end of the
    /// session.
    #[test]
    fn a_missing_pager_is_reported_rather_than_swallowed() {
        with_pager(Some("q-no-such-pager-exists"), || {
            let e = show("text").unwrap_err();
            assert!(format!("{e:#}").contains("cannot run pager"), "{e:#}");
        });
    }
}
