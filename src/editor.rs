//! Handing the user a buffer in their editor and reading it back — what
//! `q tpl edit` does with a template's TOML (SPEC §11).
//!
//! Behind the same `$Q_FIXTURE` gate as tmux, ssh and `bd`: **no test may
//! launch an editor**. `cargo test` has no terminal, so a real `vi` would
//! either hang the suite or scribble on the one the developer is sitting in.
//! Under the fixture nothing is spawned at all:
//!
//! | | |
//! |---|---|
//! | `Q_FIXTURE_EDITOR` | a file whose contents are what the "editor" saved |
//! | `Q_FIXTURE_EDITOR_FAIL` | the editor exits non-zero (content unused) |
//! | `Q_FIXTURE_EDITOR_SEEN` | a file the buffer the editor was *handed* is written to |
//!
//! The stub writes `initial` out before it reads the reply so a test can
//! assert on the half of the round trip a user sees: `q tpl edit` has to open
//! on the template's current definition. Without it [`edit`] could be handed
//! `""` and every `tpl edit` test would still pass, while every real user
//! opened an empty buffer and wiped their template on `:wq`.
//!
//! Outside a fixture the program is `$Q_EDITOR`, else `$VISUAL`, else
//! `$EDITOR`, else `vi` — `$Q_EDITOR` first so a scripted caller can override
//! one command's editor without changing the one the user's shell exports.
//! `q config edit` predates this module and still reads `$VISUAL`/`$EDITOR`
//! directly; it edits a path that already exists, which is the part this does
//! not do.

use std::path::{Path, PathBuf};

use crate::error::QError;
use crate::model::new_id;

/// Writes `initial` to a temporary file, opens it, and returns what came back.
/// The file is removed either way; `suffix` only gives the editor a filetype
/// to highlight by.
pub fn edit(initial: &str, suffix: &str) -> anyhow::Result<String> {
    if fixtured() {
        return fixture(initial);
    }
    let path = std::env::temp_dir().join(format!("q-{}{suffix}", new_id("edit")));
    std::fs::write(&path, initial).map_err(|e| QError::Io(format!("{}: {e}", path.display())))?;
    let out = run(&path);
    let _ = std::fs::remove_file(&path);
    out
}

fn run(path: &Path) -> anyhow::Result<String> {
    let editor = program();
    let mut parts = editor.split_whitespace();
    let program = parts
        .next()
        .ok_or_else(|| QError::Other("no editor: $VISUAL/$EDITOR is empty".to_string()))?;
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status()
        .map_err(|e| QError::Other(format!("cannot run editor `{editor}`: {e}")))?;
    if !status.success() {
        return Err(QError::Other(format!("editor `{editor}` exited with {status}")).into());
    }
    std::fs::read_to_string(path).map_err(|e| QError::Io(format!("{}: {e}", path.display())).into())
}

/// `$Q_EDITOR`, `$VISUAL`, `$EDITOR`, `vi` — the first that is set to
/// something.
fn program() -> String {
    ["Q_EDITOR", "VISUAL", "EDITOR"]
        .into_iter()
        .find_map(|var| {
            std::env::var(var)
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string())
}

fn fixtured() -> bool {
    std::env::var_os("Q_FIXTURE").is_some_and(|v| !v.is_empty())
}

fn fixture(initial: &str) -> anyhow::Result<String> {
    if let Some(seen) = std::env::var_os("Q_FIXTURE_EDITOR_SEEN") {
        let seen = PathBuf::from(seen);
        std::fs::write(&seen, initial)
            .map_err(|e| QError::Io(format!("{}: {e}", seen.display())))?;
    }
    if std::env::var_os("Q_FIXTURE_EDITOR_FAIL").is_some() {
        return Err(QError::Other("editor `stub` exited with exit status: 1".to_string()).into());
    }
    let path: PathBuf = std::env::var_os("Q_FIXTURE_EDITOR")
        .map(PathBuf::from)
        .ok_or_else(|| {
            QError::Other(
                "no editor under $Q_FIXTURE; a test that means to edit sets $Q_FIXTURE_EDITOR"
                    .to_string(),
            )
        })?;
    std::fs::read_to_string(&path)
        .map_err(|e| QError::Io(format!("{}: {e}", path.display())).into())
}
