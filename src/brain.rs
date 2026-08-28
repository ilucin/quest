//! Brain integration (SPEC §14): the session note `q new --brain` writes under
//! the brain root, the links `q link add` syncs into it, and the knowledge
//! summary `q close --summarize` asks `claude -p` to write.
//!
//! **Brain root.** `$Q_BRAIN_ROOT` overrides everything — the test and manual
//! seam, so a run points at a temp dir and never at the real personal brain.
//! Otherwise the `brain_root:` line of `~/.brainrc` is parsed. `None` means the
//! brain is unavailable and every caller degrades to a no-op. Under `$Q_FIXTURE`
//! (every test) a missing `$Q_BRAIN_ROOT` resolves to `None` rather than reading
//! `~/.brainrc`, so no test can ever touch the real brain.
//!
//! **`claude -p`** is behind the [`Summarizer`] trait, stubbed under `$Q_FIXTURE`
//! the same way `tmux`, `bd` and `notify` are, so no test spawns a real agent:
//!
//! | var | stands for |
//! |---|---|
//! | `Q_FIXTURE_CLAUDE` | stdout of the stub `claude -p` — the summary path it "wrote" (absent = claude unavailable) |
//! | `Q_FIXTURE_CLAUDE_LOG` | appended the brief handed to `claude`, so a test can assert the invocation |
//!
//! **Idempotence.** Writing the note never clobbers an existing one (the body is
//! the human's), and a link is appended only if that exact `kind: ref` line is
//! not already in the YAML block.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::proc;

/// `claude -p` gets this long before it is killed — a summary is a real agent
/// turn, so the budget is generous; a hang past it is a graceful skip.
const SUMMARIZE_TIMEOUT: Duration = Duration::from_secs(180);

/// What the summarizer tells `claude` to do. The brief follows it; `claude`
/// writes the note and prints only its path, which `q` records as
/// `summarized_to:`.
const SUMMARIZE_PROMPT: &str = "You are closing a Quest. Below is its brief. Write a concise, \
curated knowledge summary of what was learned into a new note under `knowledge/` in this brain \
(create the file yourself). Then print ONLY the path of the note you wrote, nothing else.\n\n";

/// The brain root for this run, or `None` when the brain is unavailable.
///
/// `$Q_BRAIN_ROOT` wins outright. Otherwise `~/.brainrc` is parsed — except
/// under `$Q_FIXTURE`, where a missing override is `None` so no test reads the
/// real brain.
pub fn root() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("Q_BRAIN_ROOT").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    if std::env::var_os("Q_FIXTURE").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    brainrc_root(&dirs::home_dir()?.join(".brainrc"))
}

/// `brain_root:` out of a `.brainrc`, trimmed of surrounding quotes. `None`
/// when the file is missing or names no root.
fn brainrc_root(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("brain_root:") {
            let value = rest.trim().trim_matches(['"', '\'']).trim();
            if !value.is_empty() {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

/// `<root>/sessions/<slug>/<slug>.md` — the session note path convention.
pub fn note_path(root: &Path, slug: &str) -> PathBuf {
    root.join("sessions").join(slug).join(format!("{slug}.md"))
}

/// The metadata a fresh session note carries in its YAML block.
pub struct SessionNote<'a> {
    pub quest_id: &'a str,
    pub machine: &'a str,
    pub cwd: &'a str,
    pub beads_epic: Option<&'a str>,
    /// ISO-8601 UTC, second precision.
    pub created: &'a str,
}

/// What [`write_session_note`] did — an existing note is never clobbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Created,
    Existed,
}

/// Writes `sessions/<slug>/<slug>.md` under `root` with `tags: [session]` and
/// the Quest's YAML block. Idempotent: an existing note is left untouched (its
/// body is the human's) and reported as [`Written::Existed`].
pub fn write_session_note(root: &Path, slug: &str, note: &SessionNote) -> std::io::Result<Written> {
    let path = note_path(root, slug);
    if path.exists() {
        return Ok(Written::Existed);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, render_note(slug, note))?;
    Ok(Written::Created)
}

/// The initial note body: a YAML frontmatter block, then a `# <slug>` heading
/// for the human's own scratch. Field order is fixed —
/// `tags, quest, machine, cwd, beads_epic, created` — with `beads_epic` omitted
/// when the Quest has no epic.
fn render_note(slug: &str, note: &SessionNote) -> String {
    let mut out = String::from("---\ntags: [session]\n");
    out.push_str(&format!("quest: {}\n", note.quest_id));
    out.push_str(&format!("machine: {}\n", note.machine));
    out.push_str(&format!("cwd: {}\n", note.cwd));
    if let Some(epic) = note.beads_epic {
        out.push_str(&format!("beads_epic: {epic}\n"));
    }
    out.push_str(&format!("created: {}\n", note.created));
    out.push_str("---\n\n");
    out.push_str(&format!("# {slug}\n"));
    out
}

/// Appends `kind: ref` into the note's YAML block on `q link add`
/// (SPEC §14, `[brain] sync_links`). No-op — `Ok(false)` — when the note is
/// missing, has no frontmatter, or already carries that exact line.
pub fn append_link(root: &Path, slug: &str, kind: &str, reference: &str) -> std::io::Result<bool> {
    edit_frontmatter(root, slug, |lines| {
        let entry = format!("{kind}: {reference}");
        if lines.iter().any(|l| l.trim() == entry) {
            return false;
        }
        lines.push(entry);
        true
    })
}

/// Sets `summarized_to: <path>` in the note's YAML block, replacing any
/// existing value. No-op — `Ok(false)` — when the note is missing or has no
/// frontmatter.
pub fn set_summarized_to(root: &Path, slug: &str, target: &str) -> std::io::Result<bool> {
    edit_frontmatter(root, slug, |lines| {
        let entry = format!("summarized_to: {target}");
        if let Some(existing) = lines
            .iter_mut()
            .find(|l| l.trim_start().starts_with("summarized_to:"))
        {
            if *existing == entry {
                return false;
            }
            *existing = entry;
        } else {
            lines.push(entry);
        }
        true
    })
}

/// Reads the note, hands `edit` the YAML block's lines (between the opening and
/// closing `---`) to mutate, and rewrites the file only when `edit` returns
/// `true`. `Ok(false)` when the note is missing or has no frontmatter block.
fn edit_frontmatter(
    root: &Path,
    slug: &str,
    edit: impl FnOnce(&mut Vec<String>) -> bool,
) -> std::io::Result<bool> {
    let path = note_path(root, slug);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let lines: Vec<&str> = text.lines().collect();
    // The block is the first `---` line and the next `---` after it.
    if lines.first().map(|l| l.trim()) != Some("---") {
        return Ok(false);
    }
    let Some(close) = lines.iter().skip(1).position(|l| l.trim() == "---") else {
        return Ok(false);
    };
    let close = close + 1;
    let mut block: Vec<String> = lines[1..close].iter().map(|s| s.to_string()).collect();
    if !edit(&mut block) {
        return Ok(false);
    }
    let mut out = String::from("---\n");
    for line in &block {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("---\n");
    for line in &lines[close + 1..] {
        out.push_str(line);
        out.push('\n');
    }
    // Preserve a trailing newline exactly when the original had none of its own
    // beyond the split, so a re-read is stable.
    std::fs::write(&path, out)?;
    Ok(true)
}

/// `claude -p` for `q close --summarize`. Behind a trait so a test drives it
/// without an agent — the same shape `tmux`, `bd` and `notify` are stubbed with.
pub trait Summarizer {
    /// Runs `claude -p` over the Quest `brief`; returns the path of the summary
    /// note it wrote (its stdout), or `None` when `claude` is unavailable or
    /// failed — a graceful skip, never an error.
    fn summarize(&self, brief: &str) -> Option<String>;
}

/// The real summarizer under normal runs, the fixture under `$Q_FIXTURE`.
pub fn summarizer() -> Box<dyn Summarizer> {
    if std::env::var_os("Q_FIXTURE").is_some_and(|v| !v.is_empty()) {
        return Box::new(FixtureSummarizer);
    }
    Box::new(RealSummarizer)
}

struct RealSummarizer;

impl Summarizer for RealSummarizer {
    fn summarize(&self, brief: &str) -> Option<String> {
        let mut cmd = Command::new("claude");
        cmd.arg("-p").arg(format!("{SUMMARIZE_PROMPT}{brief}"));
        let out = proc::run(&mut cmd, b"", SUMMARIZE_TIMEOUT).ok()?;
        if !out.success() {
            return None;
        }
        // The last non-empty line is the path; a chatty agent may precede it.
        out.text()
            .lines()
            .rev()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(str::to_string)
    }
}

/// Records the brief to `$Q_FIXTURE_CLAUDE_LOG` (so a test asserts the
/// invocation) and returns the canned path from `$Q_FIXTURE_CLAUDE`, or `None`
/// when that file is absent — the "claude is unavailable" case.
struct FixtureSummarizer;

impl Summarizer for FixtureSummarizer {
    fn summarize(&self, brief: &str) -> Option<String> {
        if let Some(log) = std::env::var_os("Q_FIXTURE_CLAUDE_LOG")
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log)
        {
            let _ = file.write_all(brief.as_bytes());
        }
        let path = std::env::var_os("Q_FIXTURE_CLAUDE").filter(|p| !p.is_empty())?;
        let text = std::fs::read_to_string(path).ok()?;
        let trimmed = text.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn note<'a>(epic: Option<&'a str>) -> SessionNote<'a> {
        SessionNote {
            quest_id: "q-abc",
            machine: "laptop",
            cwd: "/tmp/work",
            beads_epic: epic,
            created: "2026-08-28 10:00:00",
        }
    }

    #[test]
    fn brainrc_root_reads_the_brain_root_line() {
        let dir = temp();
        let rc = dir.path().join(".brainrc");
        std::fs::write(&rc, "editor: code\nbrain_root: /Users/x/Code/brain\n").unwrap();
        assert_eq!(
            brainrc_root(&rc),
            Some(PathBuf::from("/Users/x/Code/brain"))
        );
        std::fs::write(&rc, "brain_root: \"/quoted/brain\"\n").unwrap();
        assert_eq!(brainrc_root(&rc), Some(PathBuf::from("/quoted/brain")));
        std::fs::write(&rc, "editor: code\n").unwrap();
        assert_eq!(brainrc_root(&rc), None);
        assert_eq!(brainrc_root(&dir.path().join("nope")), None);
    }

    #[test]
    fn note_path_follows_the_sessions_slug_slug_convention() {
        let root = Path::new("/brain");
        assert_eq!(
            note_path(root, "my-quest"),
            Path::new("/brain/sessions/my-quest/my-quest.md")
        );
    }

    #[test]
    fn writes_the_yaml_block_with_tags_and_fields() {
        let dir = temp();
        let w = write_session_note(dir.path(), "alpha", &note(Some("bd-7fx"))).unwrap();
        assert_eq!(w, Written::Created);
        let body = std::fs::read_to_string(note_path(dir.path(), "alpha")).unwrap();
        assert_eq!(
            body,
            "---\ntags: [session]\nquest: q-abc\nmachine: laptop\ncwd: /tmp/work\n\
             beads_epic: bd-7fx\ncreated: 2026-08-28 10:00:00\n---\n\n# alpha\n"
        );
    }

    #[test]
    fn omits_beads_epic_when_absent() {
        let dir = temp();
        write_session_note(dir.path(), "alpha", &note(None)).unwrap();
        let body = std::fs::read_to_string(note_path(dir.path(), "alpha")).unwrap();
        assert!(!body.contains("beads_epic"), "{body}");
        assert!(body.contains("cwd: /tmp/work\ncreated:"), "{body}");
    }

    #[test]
    fn an_existing_note_is_never_clobbered() {
        let dir = temp();
        let path = note_path(dir.path(), "alpha");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "the human's own body\n").unwrap();
        let w = write_session_note(dir.path(), "alpha", &note(Some("bd-1"))).unwrap();
        assert_eq!(w, Written::Existed);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "the human's own body\n"
        );
    }

    #[test]
    fn append_link_adds_into_the_yaml_block_and_dedupes() {
        let dir = temp();
        write_session_note(dir.path(), "alpha", &note(None)).unwrap();
        assert!(append_link(dir.path(), "alpha", "pr", "https://x/pull/1").unwrap());
        let body = std::fs::read_to_string(note_path(dir.path(), "alpha")).unwrap();
        // Inside the frontmatter, before the closing fence and the heading.
        let fm_end = body.find("\n---\n\n").unwrap();
        assert!(body[..fm_end].contains("pr: https://x/pull/1"), "{body}");
        assert!(body.ends_with("# alpha\n"), "{body}");
        // A second identical add is a no-op.
        assert!(!append_link(dir.path(), "alpha", "pr", "https://x/pull/1").unwrap());
        let again = std::fs::read_to_string(note_path(dir.path(), "alpha")).unwrap();
        assert_eq!(body, again);
    }

    #[test]
    fn append_link_on_a_missing_note_is_a_noop() {
        let dir = temp();
        assert!(!append_link(dir.path(), "ghost", "pr", "x").unwrap());
    }

    #[test]
    fn set_summarized_to_inserts_then_replaces() {
        let dir = temp();
        write_session_note(dir.path(), "alpha", &note(None)).unwrap();
        assert!(set_summarized_to(dir.path(), "alpha", "knowledge/a.md").unwrap());
        let body = std::fs::read_to_string(note_path(dir.path(), "alpha")).unwrap();
        assert!(body.contains("summarized_to: knowledge/a.md"), "{body}");
        // Replacing with a new value rewrites the one line, not a second.
        assert!(set_summarized_to(dir.path(), "alpha", "knowledge/b.md").unwrap());
        let body = std::fs::read_to_string(note_path(dir.path(), "alpha")).unwrap();
        assert_eq!(body.matches("summarized_to:").count(), 1, "{body}");
        assert!(body.contains("knowledge/b.md"), "{body}");
        // Same value again is a no-op.
        assert!(!set_summarized_to(dir.path(), "alpha", "knowledge/b.md").unwrap());
    }

    #[test]
    fn edit_frontmatter_refuses_a_note_without_a_block() {
        let dir = temp();
        let path = note_path(dir.path(), "alpha");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "no frontmatter here\n").unwrap();
        assert!(!append_link(dir.path(), "alpha", "pr", "x").unwrap());
    }
}
