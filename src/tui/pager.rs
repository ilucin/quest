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
pub fn command() -> Vec<String> {
    #[cfg(test)]
    if let Some(argv) = test_override() {
        return argv;
    }
    command_from(std::env::var("PAGER").ok().as_deref())
}

/// The whole decision, as a pure function of what `$PAGER` says — so the
/// parsing is tested without a process environment anywhere near it.
///
/// Split on whitespace rather than run through a shell: `$PAGER` is a command
/// with flags (`less -R`), not a script, and a TUI must not hand a user's
/// environment to `sh -c`.
fn command_from(configured: Option<&str>) -> Vec<String> {
    let parts: Vec<String> = configured
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
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
    show_with(&command(), text)
}

fn show_with(argv: &[String], text: &str) -> anyhow::Result<()> {
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

/// What [`command`] returns while a test is steering it.
#[cfg(test)]
static OVERRIDE: std::sync::Mutex<Option<Vec<String>>> = std::sync::Mutex::new(None);

#[cfg(test)]
fn test_override() -> Option<Vec<String>> {
    OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Run `f` as if `$PAGER` were `value`.
///
/// Deliberately *not* `set_var`/`remove_var`: `setenv(3)` is unsafe against
/// concurrent *readers*, not just writers, so no lock held by the setters can
/// make it sound — and this binary's other tests read the environment
/// (`Q_DB`, `Q_CONFIG`, `Q_FIXTURE`, `TMUX`) on other threads throughout.
/// An override [`command`] consults instead is the same steering with none of
/// the process-global blast radius; the serial lock only keeps two tests from
/// steering it at once.
#[cfg(test)]
pub(super) fn with_pager<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            *OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // Declared after the lock, so it clears the override before releasing it —
    // and clears it even if `f` unwinds.
    let _reset = Reset;
    *OVERRIDE.lock().unwrap_or_else(|e| e.into_inner()) = Some(command_from(value));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pager_comes_from_the_environment_with_a_working_default() {
        assert_eq!(command_from(None), DEFAULT);
        // Set but empty, and set to nothing but spaces: both mean "unset".
        assert_eq!(command_from(Some("")), DEFAULT);
        assert_eq!(command_from(Some("   ")), DEFAULT);
        // Flags come along; the program is never handed to a shell.
        assert_eq!(command_from(Some("less -R")), ["less", "-R"]);
        assert_eq!(
            command_from(Some("bat  --paging always")),
            ["bat", "--paging", "always"]
        );
    }

    /// B1 (round 1): `with_pager` used to `setenv`, which is UB against every
    /// other thread of a parallel test binary that reads the environment.
    /// Steering the pager must leave `environ` exactly as it found it.
    #[test]
    fn steering_the_pager_never_touches_the_process_environment() {
        let before = std::env::var_os("PAGER");
        with_pager(Some("tee /dev/null"), || {
            assert_eq!(command(), ["tee", "/dev/null"]);
            assert_eq!(std::env::var_os("PAGER"), before, "$PAGER was written");
        });
        assert_eq!(std::env::var_os("PAGER"), before, "$PAGER was left behind");
        // And the override is gone, so an unsteered call reads the real thing.
        assert_eq!(
            command(),
            command_from(std::env::var("PAGER").ok().as_deref())
        );
    }

    /// The brief has to reach the pager's stdin, and the call has to come back
    /// — a pager left holding the pipe would wedge the TUI with no way out.
    #[test]
    fn the_text_reaches_the_pager_and_the_call_returns() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("paged");
        let argv = command_from(Some(&format!("tee {}", out.display())));
        show_with(&argv, "# brief\n\nsecond line\n").unwrap();
        let seen = std::fs::read_to_string(&out).unwrap();
        assert!(seen.contains("second line"), "{seen}");
    }

    /// A pager that is not installed is a status message, not the end of the
    /// session.
    #[test]
    fn a_missing_pager_is_reported_rather_than_swallowed() {
        let argv = command_from(Some("q-no-such-pager-exists"));
        let e = show_with(&argv, "text").unwrap_err();
        assert!(format!("{e:#}").contains("cannot run pager"), "{e:#}");
    }
}
