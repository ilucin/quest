//! Workflows (SPEC §11): **markdown files, not database rows.**
//!
//! ```text
//! q workflow list | show | add | edit | rm | set
//! ```
//!
//! A workflow is the prompt that tells a master *how* to orchestrate — the
//! third section of every brief (SPEC §9). Five are built into the binary with
//! `include_str!`; the rest live in `<config dir>/workflows/<name>.md`, next to
//! `config.toml`, so `Q_CONFIG` moves both together and no test can reach the
//! real `~/.config/q`.
//!
//! The rules this module had to pick, and why:
//!
//! * **A user file shadows the built-in of the same name.** That is the whole
//!   editing story: `q workflow edit orchestrator` copies the built-in out to
//!   disk and opens *that*, and `q workflow rm orchestrator` deletes the copy
//!   and reveals the built-in again. A built-in is never lost and never
//!   modified, so there is always a way back.
//! * **A worker gets the `## worker` section, or the whole file.** SPEC §11
//!   gives workers "only their section if `## worker` is defined in the file".
//!   When it is not, the alternative to the whole file is *nothing* — a worker
//!   with no idea how the Quest is being run — so the whole file it is, with
//!   the brief saying out loud that it is reading the master's copy. See
//!   [`worker_section`].
//! * **The name grammar is the slug grammar** ([`validate_name`]): a workflow
//!   name is typed as a flag value, is a file name on disk, and is matched
//!   exactly, and one rule for slugs, labels, template names and workflow names
//!   is one rule to remember.
//! * **Unknown is `not_found`, malformed is `invalid`** — and the `not_found`
//!   names every workflow there is, because the only useful thing to say to
//!   someone who mistyped `orchestartor` is the list. This is the `cwd`
//!   distinction of `q tpl` (`crate::commands::tpl`), one level up.
//! * **A name is checked where it is *set*, not on every write.** `q new
//!   --workflow`, `q spawn --workflow`, `q set <quest> workflow`, `q tpl
//!   add/edit --workflow` all refuse an unknown one; a `q tpl edit
//!   --description` over a template whose workflow file has since been deleted
//!   does not, exactly as it does not re-check that template's `cwd`. A
//!   definition is allowed to travel ahead of the files it names — it is
//!   checked again at `q tpl run`, which goes through `q new`.
//! * **The registry is on the `Ctx`, not discovered.** Like `tmux`, `ssh` and
//!   `bd`: an in-crate test gets a directory that does not exist (built-ins
//!   only) rather than whatever the developer has in their home directory.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::QError;

/// The five workflows SPEC §11 names, embedded in the binary. Sorted by name,
/// which is also the order [`Registry::list`] reports them in.
pub const BUILTIN: [(&str, &str); 5] = [
    (
        "orchestrator",
        include_str!("workflows/builtin/orchestrator.md"),
    ),
    ("research", include_str!("workflows/builtin/research.md")),
    ("review", include_str!("workflows/builtin/review.md")),
    ("routine", include_str!("workflows/builtin/routine.md")),
    ("solo", include_str!("workflows/builtin/solo.md")),
];

/// The heading that carves a worker's half out of a workflow file (SPEC §11).
pub const WORKER_HEADING: &str = "## worker";

/// The extension every user workflow file carries.
const EXT: &str = "md";

/// The "there is no user directory" path: the built-ins and nothing else.
///
/// A sentinel rather than an `Option<PathBuf>` threaded through every lookup,
/// and the same one `crate::Ctx::for_tests` uses — it keeps `Registry` a plain
/// directory in every code path, including the ones a test drives.
pub const NO_DIR: &str = "/nonexistent/q/workflows";

/// Where a workflow came from — the one thing `q workflow list` exists to make
/// unambiguous.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// Embedded in the binary, with no file shadowing it.
    Builtin,
    /// A file with no built-in of that name.
    User,
    /// A file standing in front of a built-in of the same name.
    Shadow,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Builtin => "builtin",
            Source::User => "user",
            Source::Shadow => "user (shadows builtin)",
        }
    }
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One row of `q workflow list`: what it is and where it comes from, without
/// its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub name: String,
    pub source: Source,
    /// The file backing it, for a `user`/`shadow` row; `null` for a built-in.
    pub path: Option<PathBuf>,
    /// First non-blank, non-heading line of the body — enough for a listing.
    pub summary: String,
    pub has_worker_section: bool,
    pub chars: usize,
}

/// One resolved workflow, body included — what `q workflow show` prints and
/// what the brief renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Workflow {
    pub name: String,
    pub source: Source,
    pub path: Option<PathBuf>,
    pub body: String,
}

impl Workflow {
    /// The text a session of `role` is handed (SPEC §11). A master always gets
    /// the whole file; a worker gets its `## worker` section when the file
    /// defines one.
    pub fn for_role(&self, role: crate::model::SessionRole) -> Part<'_> {
        match role {
            crate::model::SessionRole::Master => Part::Whole(&self.body),
            crate::model::SessionRole::Worker => match worker_section(&self.body) {
                Some(section) => Part::Worker(section),
                None => Part::WholeForWorker(&self.body),
            },
        }
    }
}

/// Which half of a workflow a brief is rendering, so the brief can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part<'a> {
    /// The master's: the whole file.
    Whole(&'a str),
    /// The worker's own `## worker` section.
    Worker(&'a str),
    /// A worker reading a file that defines no `## worker` section, so it gets
    /// the master's copy. Rendered with a note saying exactly that.
    WholeForWorker(&'a str),
}

impl<'a> Part<'a> {
    pub fn text(self) -> &'a str {
        match self {
            Part::Whole(t) | Part::Worker(t) | Part::WholeForWorker(t) => t,
        }
    }
}

/// The `<config dir>/workflows` directory and everything that can be read out
/// of it, with the built-ins behind it.
///
/// Cheap to construct and stateless: nothing is cached, because `q workflow
/// add` in one terminal has to be visible to the `q new` in the next one.
#[derive(Debug, Clone)]
pub struct Registry {
    dir: PathBuf,
}

impl Registry {
    pub fn new(dir: impl Into<PathBuf>) -> Registry {
        Registry { dir: dir.into() }
    }

    /// The built-ins alone — the default a brief renders with, and what a
    /// caller with no config directory to point at gets.
    pub fn builtin_only() -> Registry {
        Registry::new(NO_DIR)
    }

    /// The registry off the process environment, for the callers that have no
    /// `Ctx` to take it from — the hooks. A config path that cannot be
    /// resolved is the built-ins, not a failed `SessionStart`.
    pub fn discover() -> Registry {
        Registry::user_dir()
            .map(Registry::new)
            .unwrap_or_else(|_| Registry::builtin_only())
    }

    /// The user workflow directory that goes with the config file — i.e. with
    /// `$Q_CONFIG` when it is set, so a sandboxed run cannot reach the real one.
    pub fn user_dir() -> anyhow::Result<PathBuf> {
        let config = crate::config::Config::path()?;
        Ok(match config.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.join("workflows"),
            _ => PathBuf::from("workflows"),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where a name's file lives, whether or not it is there.
    pub fn path_of(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.{EXT}"))
    }

    /// Every workflow, built-in and user, by name. A file shadows the built-in
    /// it shares a name with.
    ///
    /// A directory that is not there is not an error — it is the ordinary state
    /// of a machine that has never run `q workflow add`. Anything else about it
    /// (unreadable, not a directory) is reported: silently answering "only the
    /// built-ins" would hide a whole shelf of workflows the user wrote.
    pub fn list(&self) -> anyhow::Result<Vec<Entry>> {
        let mut by_name: BTreeMap<String, Entry> = BTreeMap::new();
        for (name, body) in BUILTIN {
            by_name.insert(name.to_string(), entry(name, Source::Builtin, None, body));
        }
        for (name, path) in self.files()? {
            let body = read(&path)?;
            let source = if is_builtin(&name) {
                Source::Shadow
            } else {
                Source::User
            };
            by_name.insert(name.clone(), entry(&name, source, Some(path), &body));
        }
        Ok(by_name.into_values().collect())
    }

    /// Every workflow name, sorted — what an error message offers and what the
    /// TUI's select lists.
    pub fn names(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.list()?.into_iter().map(|e| e.name).collect())
    }

    /// One workflow, body included. A user file wins over the built-in.
    pub fn get(&self, name: &str) -> anyhow::Result<Workflow> {
        validate_name(name)?;
        let path = self.path_of(name);
        if path.is_file() {
            return Ok(Workflow {
                name: name.to_string(),
                source: if is_builtin(name) {
                    Source::Shadow
                } else {
                    Source::User
                },
                body: read(&path)?,
                path: Some(path),
            });
        }
        if let Some(body) = builtin(name) {
            return Ok(Workflow {
                name: name.to_string(),
                source: Source::Builtin,
                path: None,
                body: body.to_string(),
            });
        }
        Err(self.unknown(name))
    }

    /// `name` names a workflow that exists — the check every command that
    /// *accepts* a workflow name runs (see the module docs).
    ///
    /// A blank name is not a workflow name at all: it is how `q set <quest>
    /// workflow ""` clears the column, so callers strip it before they get
    /// here and it is refused rather than silently accepted.
    pub fn require(&self, name: &str) -> anyhow::Result<()> {
        validate_name(name)?;
        if self.path_of(name).is_file() || is_builtin(name) {
            return Ok(());
        }
        Err(self.unknown(name))
    }

    /// `Some(())` when the name is set and unknown; `Ok(())` for a blank one.
    /// The spelling every caller with an `Option<&str>` flag uses.
    pub fn require_opt(&self, name: Option<&str>) -> anyhow::Result<()> {
        match name.map(str::trim).filter(|n| !n.is_empty()) {
            Some(name) => self.require(name),
            None => Ok(()),
        }
    }

    /// Writes `body` to the user file for `name`, creating the directory.
    /// Returns the path it landed at.
    pub fn write(&self, name: &str, body: &str) -> anyhow::Result<PathBuf> {
        validate_name(name)?;
        let body = normalize(body);
        if body.is_empty() {
            return Err(QError::Invalid(format!(
                "workflow `{name}` would be empty; a workflow is the prompt a master reads"
            ))
            .into());
        }
        let path = self.path_of(name);
        crate::config::write_atomic(&path, &body)?;
        Ok(path)
    }

    /// Deletes the user file for `name`. Only a file: a built-in is embedded in
    /// the binary and cannot be removed, and saying so is more use than a
    /// `not_found` about a workflow that plainly exists.
    pub fn remove(&self, name: &str) -> anyhow::Result<PathBuf> {
        validate_name(name)?;
        let path = self.path_of(name);
        if !path.is_file() {
            if is_builtin(name) {
                return Err(QError::Invalid(format!(
                    "`{name}` is a built-in workflow and is embedded in the binary; \
                     there is nothing to remove (`q workflow edit {name}` copies it here first)"
                ))
                .into());
            }
            return Err(self.unknown(name));
        }
        std::fs::remove_file(&path)
            .map_err(|e| QError::Io(std::io::Error::other(format!("{}: {e}", path.display()))))?;
        Ok(path)
    }

    /// `not found: unknown workflow `x`; known: a, b, c` — the list, because
    /// that is the only useful reply to a typo.
    pub fn unknown(&self, name: &str) -> anyhow::Error {
        let known = self
            .names()
            .map(|names| names.join(", "))
            .unwrap_or_else(|_| BUILTIN.map(|(n, _)| n).join(", "));
        QError::NotFound(format!(
            "unknown workflow `{name}`; known: {known} (add one with `q workflow add {name}`)"
        ))
        .into()
    }

    /// `<name>.md` files in the directory, name first. A file whose stem is not
    /// a legal workflow name is skipped rather than fatal: the directory is the
    /// user's, and an editor's `orchestrator.md~` must not break `q new`.
    fn files(&self) -> anyhow::Result<Vec<(String, PathBuf)>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(QError::Io(std::io::Error::other(format!(
                    "{}: {e}",
                    self.dir.display()
                )))
                .into());
            }
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                QError::Io(std::io::Error::other(format!(
                    "{}: {e}",
                    self.dir.display()
                )))
            })?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(EXT) {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if validate_name(stem).is_err() {
                continue;
            }
            out.push((stem.to_string(), path));
        }
        out.sort();
        Ok(out)
    }
}

fn read(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path)
        .map_err(|e| QError::Io(std::io::Error::other(format!("{}: {e}", path.display()))).into())
}

/// Trailing whitespace off every line and exactly one trailing newline — a
/// workflow is a prompt, and a prompt that differs from another only in
/// trailing blanks is the same prompt.
fn normalize(body: &str) -> String {
    let trimmed: Vec<&str> = body.lines().map(str::trim_end).collect();
    let text = trimmed.join("\n");
    let text = text.trim_matches('\n').to_string();
    if text.is_empty() {
        return text;
    }
    format!("{text}\n")
}

fn entry(name: &str, source: Source, path: Option<PathBuf>, body: &str) -> Entry {
    Entry {
        name: name.to_string(),
        source,
        path,
        summary: summary(body),
        has_worker_section: worker_section(body).is_some(),
        chars: body.chars().count(),
    }
}

/// The first line that is neither blank nor a heading, shortened — a listing
/// has one column for "what is this".
fn summary(body: &str) -> String {
    let line = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");
    crate::commands::fmt::oneline(line, 60)
}

pub fn is_builtin(name: &str) -> bool {
    BUILTIN.iter().any(|(n, _)| *n == name)
}

pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTIN
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, body)| *body)
}

/// A workflow name follows the slug grammar (`crate::commands::new`): it is a
/// file name, a flag value and an exact match, and one grammar for slugs,
/// labels, template names and workflow names is one rule to remember.
pub fn validate_name(name: &str) -> anyhow::Result<()> {
    crate::commands::new::validate_workflow_name(name)
}

/// The `## worker` section of a workflow file: everything after the heading, up
/// to the next heading at the same level or above.
///
/// The heading is matched case-insensitively on a line of its own, so
/// `## Worker` counts and a `## worker notes` heading — a different section —
/// does not. `None` means the file defines no worker half; see the module docs
/// for what a worker gets then.
pub fn worker_section(body: &str) -> Option<&str> {
    // Byte offsets rather than collected lines, so the section borrows `body`.
    let mut offset = 0usize;
    let mut start: Option<usize> = None;
    for line in body.split_inclusive('\n') {
        let line_start = offset;
        offset += line.len();
        let text = line.trim_end_matches('\n');
        match start {
            None if is_worker_heading(text) => start = Some(offset),
            None => {}
            Some(start) if is_section_break(text) => {
                return Some(body[start..line_start].trim_matches('\n'));
            }
            Some(_) => {}
        }
    }
    start.map(|start| body[start..].trim_matches('\n'))
}

fn is_worker_heading(line: &str) -> bool {
    line.trim().eq_ignore_ascii_case(WORKER_HEADING)
}

/// A `#` or `##` heading — what ends the worker section. A deeper heading
/// (`###`) is part of it.
fn is_section_break(line: &str) -> bool {
    let line = line.trim_start();
    (line.starts_with("## ") && !line.starts_with("### "))
        || (line.starts_with("# ") && !line.starts_with("## "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SessionRole;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn every_builtin_is_embedded_named_and_substantial() {
        let names: Vec<&str> = BUILTIN.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            ["orchestrator", "research", "review", "routine", "solo"],
            "SPEC §11 names exactly these five, and `list` reports them sorted"
        );
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "BUILTIN must stay sorted by name");
        for (name, body) in BUILTIN {
            assert!(validate_name(name).is_ok(), "{name}");
            assert!(body.len() > 500, "{name} is a stub ({} bytes)", body.len());
            assert!(
                body.starts_with(&format!("# {name}\n")),
                "{name} must open with its own H1"
            );
            // A workflow is a prompt about running a Quest; it has to name the
            // commands the agent reading it is expected to call.
            assert!(body.contains("q note"), "{name} never mentions `q note`");
            assert!(body.contains("q phase"), "{name} never mentions `q phase`");
        }
        // The orchestrator's specified addition (SPEC §11).
        let orchestrator = builtin("orchestrator").unwrap();
        assert!(orchestrator.contains("q spawn"), "no worker spawning");
        assert!(orchestrator.contains("plan-review") && orchestrator.contains("code-review"));
    }

    #[test]
    fn the_worker_section_is_carved_out_at_the_next_heading() {
        let body = "# w\n\nmaster stuff\n\n## worker\n\nworker stuff\n\n### detail\n\nmore\n\n## after\n\nnot yours\n";
        let section = worker_section(body).unwrap();
        assert!(section.starts_with("worker stuff"), "{section:?}");
        assert!(section.contains("### detail"), "a deeper heading is inside");
        assert!(section.contains("more"));
        assert!(!section.contains("not yours"), "{section:?}");
        assert!(!section.contains("master stuff"));

        // Last section in the file: runs to the end.
        let tail = "# w\n\nmaster\n\n## worker\nonly this\n";
        assert_eq!(worker_section(tail).unwrap(), "only this");

        // Case and surrounding blanks do not matter; a longer heading is a
        // different section.
        assert_eq!(worker_section("## Worker\nx\n").unwrap(), "x");
        assert_eq!(worker_section("  ## worker  \nx\n").unwrap(), "x");
        assert!(worker_section("## worker notes\nx\n").is_none());
        assert!(worker_section("## workers\nx\n").is_none());
        assert!(worker_section("# w\n\nno section here\n").is_none());
        assert!(worker_section("").is_none());
        // An empty section is a section — the file said workers get nothing
        // extra, which is not the same as saying nothing.
        assert_eq!(worker_section("## worker\n\n## next\n").unwrap(), "");
    }

    #[test]
    fn four_builtins_define_a_worker_section_and_solo_deliberately_does_not() {
        for name in ["orchestrator", "review", "research", "routine"] {
            let section = worker_section(builtin(name).unwrap())
                .unwrap_or_else(|| panic!("{name} has no `## worker` section"));
            assert!(section.len() > 200, "{name}'s worker section is a stub");
            assert!(
                !section.contains("q spawn"),
                "{name}'s worker must not be told to spawn workers of its own"
            );
        }
        assert!(
            worker_section(builtin("solo").unwrap()).is_none(),
            "solo is one master and no workers; it is the fallback case on purpose"
        );
    }

    #[test]
    fn a_master_gets_the_whole_file_and_a_worker_gets_its_section() {
        let w = Workflow {
            name: "x".to_string(),
            source: Source::Builtin,
            path: None,
            body: "# x\n\nmaster half\n\n## worker\n\nworker half\n".to_string(),
        };
        assert_eq!(w.for_role(SessionRole::Master), Part::Whole(&w.body));
        assert_eq!(w.for_role(SessionRole::Worker), Part::Worker("worker half"));

        // With no section, the worker gets the master's copy — flagged, so the
        // brief can say why.
        let bare = Workflow {
            body: "# x\n\nall of it\n".to_string(),
            ..w.clone()
        };
        assert_eq!(
            bare.for_role(SessionRole::Worker),
            Part::WholeForWorker(&bare.body)
        );
        assert_eq!(bare.for_role(SessionRole::Worker).text(), &bare.body);
    }

    #[test]
    fn an_absent_directory_is_just_the_builtins() {
        let registry = Registry::new("/nonexistent/q/workflows");
        let names = registry.names().unwrap();
        assert_eq!(
            names,
            ["orchestrator", "research", "review", "routine", "solo"]
        );
        assert_eq!(registry.get("solo").unwrap().source, Source::Builtin);
        assert!(registry.list().unwrap().iter().all(|e| e.path.is_none()));
    }

    #[test]
    fn a_user_file_shadows_the_builtin_of_the_same_name() {
        let dir = dir();
        let registry = Registry::new(dir.path());
        let before = registry.get("solo").unwrap();
        assert_eq!(before.source, Source::Builtin);

        registry.write("solo", "# solo\n\nmine\n").unwrap();
        let after = registry.get("solo").unwrap();
        assert_eq!(after.source, Source::Shadow);
        assert_eq!(after.body, "# solo\n\nmine\n");
        assert_eq!(after.path, Some(registry.path_of("solo")));
        // The built-in is untouched and comes back when the file goes.
        assert_eq!(builtin("solo").unwrap(), before.body);
        registry.remove("solo").unwrap();
        assert_eq!(registry.get("solo").unwrap().source, Source::Builtin);
    }

    #[test]
    fn listing_marks_builtin_user_and_shadow() {
        let dir = dir();
        let registry = Registry::new(dir.path());
        registry.write("solo", "# solo\n\nmine\n").unwrap();
        registry.write("triage", "# triage\n\nours\n").unwrap();
        let rows = registry.list().unwrap();
        let by: BTreeMap<&str, Source> = rows.iter().map(|e| (e.name.as_str(), e.source)).collect();
        assert_eq!(by["solo"], Source::Shadow);
        assert_eq!(by["triage"], Source::User);
        assert_eq!(by["orchestrator"], Source::Builtin);
        // Six: the five built-ins, one of them shadowed, plus the new one.
        assert_eq!(rows.len(), 6, "{rows:#?}");
        let names: Vec<&str> = rows.iter().map(|e| e.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        let triage = rows.iter().find(|e| e.name == "triage").unwrap();
        assert_eq!(triage.summary, "ours");
        assert!(!triage.has_worker_section);
    }

    #[test]
    fn an_unknown_name_names_every_known_one() {
        let dir = dir();
        let registry = Registry::new(dir.path());
        registry.write("triage", "# triage\n\nx\n").unwrap();
        let e = registry.get("orchestartor").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found"),
            "{e}"
        );
        let msg = e.to_string();
        assert!(msg.contains("orchestartor"), "{msg}");
        assert!(
            msg.contains("orchestrator, research, review, routine, solo, triage"),
            "{msg}"
        );
        assert!(registry.require("triage").is_ok());
        assert!(registry.require("nope").is_err());
    }

    #[test]
    fn a_malformed_name_is_invalid_not_missing() {
        let dir = dir();
        let registry = Registry::new(dir.path());
        for bad in [
            "",
            "Upper",
            "with space",
            "sla/sh",
            "double--dash",
            "../escape",
        ] {
            let e = registry.get(bad).unwrap_err();
            assert_eq!(
                e.downcast_ref::<QError>().map(QError::code),
                Some("invalid"),
                "accepted `{bad}`: {e}"
            );
            assert!(registry.require(bad).is_err(), "`{bad}`");
            assert!(registry.write(bad, "x").is_err(), "`{bad}`");
            assert!(registry.remove(bad).is_err(), "`{bad}`");
        }
        // A traversal cannot reach outside the directory even as a file name.
        assert!(!registry.path_of("solo").to_string_lossy().contains(".."));
    }

    #[test]
    fn a_blank_optional_name_is_no_workflow_at_all() {
        let registry = Registry::new("/nonexistent/q/workflows");
        assert!(registry.require_opt(None).is_ok());
        assert!(registry.require_opt(Some("")).is_ok());
        assert!(registry.require_opt(Some("   ")).is_ok());
        assert!(registry.require_opt(Some(" solo ")).is_ok());
        assert!(registry.require_opt(Some("nope")).is_err());
    }

    #[test]
    fn writing_normalizes_and_refuses_an_empty_body() {
        let dir = dir();
        let registry = Registry::new(dir.path());
        let path = registry
            .write("triage", "\n\n# triage   \n\nbody   \n\n\n")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# triage\n\nbody\n"
        );
        for empty in ["", "   ", "\n\n\n"] {
            let e = registry.write("triage", empty).unwrap_err();
            assert!(e.to_string().contains("would be empty"), "{e}");
        }
        // The refused write did not clobber what was there.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# triage\n\nbody\n"
        );
    }

    #[test]
    fn removing_distinguishes_a_builtin_from_a_name_that_is_nothing() {
        let dir = dir();
        let registry = Registry::new(dir.path());
        let e = registry.remove("solo").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("invalid"),
            "{e}"
        );
        assert!(e.to_string().contains("built-in"), "{e}");
        let e = registry.remove("triage").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found"),
            "{e}"
        );
    }

    #[test]
    fn a_file_that_is_not_a_workflow_is_skipped_rather_than_fatal() {
        let dir = dir();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        std::fs::write(dir.path().join("Bad Name.md"), "x").unwrap();
        std::fs::write(dir.path().join("solo.md~"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub.md")).unwrap();
        let registry = Registry::new(dir.path());
        // The directory entry `sub.md` has the extension but is not a file;
        // reading it has to fail loudly rather than be reported as a workflow.
        assert!(registry.list().is_err());
        std::fs::remove_dir(dir.path().join("sub.md")).unwrap();
        assert_eq!(registry.names().unwrap().len(), BUILTIN.len());
    }

    #[test]
    fn the_user_directory_follows_the_config_file() {
        // Not `Registry::user_dir()` — that reads the process environment, and
        // a unit test must not depend on it. This is the rule it applies.
        let config = Path::new("/tmp/sandbox/config.toml");
        assert_eq!(
            config.parent().unwrap().join("workflows"),
            Path::new("/tmp/sandbox/workflows")
        );
    }
}
