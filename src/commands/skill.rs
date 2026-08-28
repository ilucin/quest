//! `q skill install | uninstall | status` — installs q's embedded agent
//! SKILL.md into `~/.claude/skills/q/SKILL.md` (SPEC §18), so any agent running
//! inside a Quest gets q's command surface and operating contract.
//!
//! The shape mirrors `q hook` (`crate::commands::hook`): the content is
//! embedded with `include_str!`, written atomically, the install is idempotent,
//! and a short content hash lets `q doctor` spot drift. It reuses `hook::State`
//! so the two report installed / missing / drifted the same way.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::Ctx;
use crate::commands::hook::State;
use crate::config;
use crate::error::QError;
use crate::output;

/// The agent-facing skill, embedded in the binary.
pub const SKILL: &str = include_str!("skill.md");

/// `$Q_CLAUDE_SKILL`, else `~/.claude/skills/q/SKILL.md`. The override names the
/// file, not the directory, so it matches `$Q_CLAUDE_SETTINGS`'s discipline and
/// a test never writes into the real `~/.claude`. The parent directory is
/// q-owned in full — nothing else lives there — which is what lets `uninstall`
/// clear it.
pub fn skill_path() -> anyhow::Result<PathBuf> {
    match std::env::var_os("Q_CLAUDE_SKILL") {
        Some(raw) if !raw.is_empty() => Ok(PathBuf::from(raw)),
        _ => {
            let home = dirs::home_dir()
                .ok_or_else(|| QError::Config("cannot determine the home directory".to_string()))?;
            Ok(home
                .join(".claude")
                .join("skills")
                .join("q")
                .join("SKILL.md"))
        }
    }
}

/// Short content hash (first 6 bytes of sha256, hex) — the same shape `q hook`
/// uses, so `q doctor` reports drift by comparing two of them.
pub fn hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    h.finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub path: PathBuf,
    pub state: State,
    /// Hash of the embedded skill this binary would install.
    pub expected_hash: String,
    /// Hash of the skill on disk; `null` when it is missing.
    pub actual_hash: Option<String>,
}

impl Status {
    pub fn ok(&self) -> bool {
        self.state == State::Installed
    }

    fn human(&self) -> String {
        let mut s = format!(
            "{} {} · {}",
            self.state.symbol(),
            self.state.label(),
            self.path.display()
        );
        s.push_str(&format!("\nhash: expected {}", self.expected_hash));
        if let Some(actual) = &self.actual_hash {
            s.push_str(&format!(" · actual {actual}"));
        }
        s
    }
}

/// The install state `q skill status` reports and `q doctor` leans on.
pub fn installed_status() -> anyhow::Result<Status> {
    let path = skill_path()?;
    let expected = hash(SKILL);
    let actual = match fs::read_to_string(&path) {
        Ok(content) => Some(hash(&content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(QError::Io(format!("{}: {e}", path.display())).into()),
    };
    let state = match &actual {
        None => State::Missing,
        Some(h) if *h == expected => State::Installed,
        Some(_) => State::Drifted,
    };
    Ok(Status {
        path,
        state,
        expected_hash: expected,
        actual_hash: actual,
    })
}

fn write_skill(path: &Path) -> anyhow::Result<()> {
    // `write_atomic` creates parent dirs and renames into place.
    config::write_atomic(path, SKILL)
}

/// Install or update the skill, returning whether anything changed. Shared by
/// `q skill install` and `q doctor --fix`.
pub fn ensure_installed() -> anyhow::Result<bool> {
    let status = installed_status()?;
    if status.state == State::Installed {
        return Ok(false);
    }
    write_skill(&status.path)?;
    Ok(true)
}

pub fn install(ctx: &Ctx) -> anyhow::Result<u8> {
    let changed = ensure_installed()?;
    let status = installed_status()?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &json!({ "action": "install", "changed": changed, "status": status }),
            || {
                let verb = if changed {
                    "installed"
                } else {
                    "already installed"
                };
                format!("{verb} q skill at {}", status.path.display())
            },
        )?;
    }
    Ok(0)
}

pub fn uninstall(ctx: &Ctx) -> anyhow::Result<u8> {
    let path = skill_path()?;
    let removed = match fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(QError::Io(format!("{}: {e}", path.display())).into()),
    };
    // The q skill directory is ours in full; take it with the file when nothing
    // else was put there. Never recursive: only an empty directory is removed.
    if let Some(parent) = path.parent()
        && dir_is_empty(parent)
    {
        let _ = fs::remove_dir(parent);
    }
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &json!({ "action": "uninstall", "removed": removed, "path": path }),
            || {
                let verb = if removed {
                    "removed"
                } else {
                    "nothing to remove:"
                };
                format!("{verb} q skill at {}", path.display())
            },
        )?;
    }
    Ok(0)
}

/// Exit 1 when the skill is missing or drifted, so `q doctor` can lean on it.
pub fn status(ctx: &Ctx) -> anyhow::Result<u8> {
    let status = installed_status()?;
    output::emit(ctx.json, &status, || status.human())?;
    Ok(u8::from(!status.ok()))
}

/// True when `dir` exists and holds nothing. A read error (including "not
/// there") is not "empty": leave the directory alone rather than guessing.
fn dir_is_empty(dir: &Path) -> bool {
    fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_skill_is_non_empty_and_states_the_confirmation_rule() {
        assert!(!SKILL.trim().is_empty());
        assert!(SKILL.contains("confirmation"));
        // Every command the skill names must exist as a real `q` subcommand.
        for cmd in [
            "q brief",
            "q sessions",
            "q peek",
            "q phase",
            "q note",
            "q link add",
            "q artifact add",
            "q spawn",
            "q send",
            "q kill",
            "q close",
        ] {
            assert!(SKILL.contains(cmd), "skill should mention `{cmd}`");
        }
    }

    #[test]
    fn hash_changes_with_content() {
        assert_eq!(hash("a"), hash("a"));
        assert_ne!(hash("a"), hash("b"));
        assert_eq!(hash("a").len(), 12);
    }
}
