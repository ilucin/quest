//! `q link add|rm`, `q links`, `q artifact add` — references attached to a
//! Quest (SPEC §12). Enrichment (`--refresh`) is a later milestone.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Ctx;
use crate::cli::LinkKind;
use crate::commands::report::{self, Target};
use crate::error::QError;
use crate::model::Link;
use crate::output;

pub struct AddArgs<'a> {
    pub r#ref: &'a str,
    pub kind: Option<LinkKind>,
    pub title: Option<&'a str>,
    pub quest: Option<&'a str>,
}

pub fn add(ctx: &Ctx, args: &AddArgs) -> anyhow::Result<()> {
    let reference = expand_tilde(args.r#ref.trim());
    if reference.is_empty() {
        return Err(QError::Invalid("reference must not be empty".to_string()).into());
    }
    let target = report::resolve(ctx, args.quest)?;
    let cwd = std::env::current_dir()?;
    let db = ctx.db()?;

    // Without --kind the same ref under any kind is the same link: a PR first
    // added as `url` must not become a second row once autodetected.
    if args.kind.is_none()
        && let Some(existing) = db.find_link_by_ref(&target.quest.id, &reference)?
    {
        return report_existing(ctx, existing, args.title, None);
    }

    let kind = match args.kind {
        Some(k) => k,
        None => detect_kind(&reference, &probe(&cwd)).ok_or_else(|| {
            QError::Invalid(format!(
                "cannot tell what `{reference}` is; pass --kind <pr|task|worktree|url|branch|beads|brain|artifact>"
            ))
        })?,
    };
    // Paths are stored absolute so every session and machine reads the same ref.
    let reference = match kind {
        LinkKind::Worktree | LinkKind::Artifact => absolute(&cwd, &reference),
        LinkKind::Pr => normalize_pr_url(&reference),
        _ => reference,
    };
    let mut link = Link::new(&target.quest.id, kind.as_str(), &reference);
    link.title = args.title.map(str::to_string);
    insert(ctx, &target, link, "link.added", None, args.title, None)
}

pub fn add_artifact(
    ctx: &Ctx,
    path: &str,
    note: Option<&str>,
    quest: Option<&str>,
) -> anyhow::Result<()> {
    let path = expand_tilde(path.trim());
    if path.is_empty() {
        return Err(QError::Invalid("path must not be empty".to_string()).into());
    }
    let target = report::resolve(ctx, quest)?;
    let cwd = std::env::current_dir()?;
    let mut link = Link::new(&target.quest.id, "artifact", &artifact_path(&cwd, &path)?);
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    if let Some(n) = note {
        link.meta = Some(serde_json::json!({ "note": n }));
    }
    insert(ctx, &target, link, "artifact.added", note, None, note)
}

/// Idempotent on `UNIQUE(quest_id, kind, ref)`: an existing row is returned as
/// is and no event is written. `title`/`note` fill blanks on the existing row.
#[allow(clippy::too_many_arguments)]
fn insert(
    ctx: &Ctx,
    target: &Target,
    mut link: Link,
    event_kind: &str,
    event_note: Option<&str>,
    title: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let db = ctx.db()?;
    if let Some(existing) = db.find_link(&target.quest.id, &link.kind, &link.r#ref)? {
        return report_existing(ctx, existing, title, note);
    }
    link.session_id = target.session_id().map(str::to_string);
    let stored = store(db, link, event_kind, event_note, false)?;
    emit_link(ctx, &stored, true, "")
}

/// Inserts `link` (its `quest_id`/`session_id` already set) and appends the
/// matching event. `auto` marks hook captures (SPEC §12) in the payload.
/// Callers check `find_link` first; a duplicate here is a real error.
pub(crate) fn store(
    db: &crate::db::Db,
    link: Link,
    event_kind: &str,
    event_note: Option<&str>,
    auto: bool,
) -> anyhow::Result<Link> {
    let stored = db.insert_link(&link)?;
    let mut payload = serde_json::json!({
        "id": stored.id,
        "kind": stored.kind,
        "ref": stored.r#ref,
    });
    if let Some(n) = event_note {
        payload["note"] = serde_json::Value::String(n.to_string());
    }
    if auto {
        payload["auto"] = serde_json::Value::Bool(true);
    }
    db.append_event(
        &stored.quest_id,
        stored.session_id.as_deref(),
        event_kind,
        &payload,
    )?;
    Ok(stored)
}

/// The row already exists: set `title`/`note` where the row has none, keep
/// what is there otherwise, and say which happened. No event either way.
fn report_existing(
    ctx: &Ctx,
    mut link: Link,
    title: Option<&str>,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let mut notes: Vec<&str> = vec!["already linked"];
    let mut changed = false;
    if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
        if link.title.as_deref().is_some_and(|c| !c.is_empty()) {
            notes.push("kept existing title");
        } else {
            link.title = Some(t.to_string());
            notes.push("title set");
            changed = true;
        }
    }
    if let Some(n) = note {
        let has_note = link
            .meta
            .as_ref()
            .and_then(|m| m.get("note"))
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty());
        if has_note {
            notes.push("kept existing note");
        } else {
            let mut meta = link.meta.take().unwrap_or_else(|| serde_json::json!({}));
            meta["note"] = serde_json::Value::String(n.to_string());
            link.meta = Some(meta);
            notes.push("note set");
            changed = true;
        }
    }
    if changed {
        ctx.db()?.update_link_details(&link)?;
    }
    let suffix = format!(" ({})", notes.join("; "));
    emit_link(ctx, &link, false, &suffix)
}

fn emit_link(ctx: &Ctx, link: &Link, created: bool, suffix: &str) -> anyhow::Result<()> {
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "link": link, "created": created }),
            || format!("{}{suffix}", line(link)),
        )?;
    }
    Ok(())
}

pub fn rm(ctx: &Ctx, id: i64, quest: Option<&str>) -> anyhow::Result<()> {
    let target = report::resolve(ctx, quest)?;
    let db = ctx.db()?;
    let link = db
        .get_link(id)?
        .filter(|l| l.quest_id == target.quest.id)
        .ok_or_else(|| QError::NotFound(format!("link #{id} on quest {}", target.quest.id)))?;
    db.delete_link(id)?;
    db.append_event(
        &target.quest.id,
        target.session_id(),
        "link.removed",
        &serde_json::json!({ "id": link.id, "kind": link.kind, "ref": link.r#ref }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "link": link, "removed": true }),
            || format!("link #{id} removed"),
        )?;
    }
    Ok(())
}

pub fn list(ctx: &Ctx, quest: Option<&str>, refresh: bool) -> anyhow::Result<()> {
    // Read-only: any Quest may be listed from any pane.
    let quest = report::resolve_quest(ctx, quest)?;
    let db = ctx.db()?;
    let mut links = db.list_links_by_quest(&quest.id)?;
    // Lazy enrichment (SPEC §12): best effort, capped, never fails the listing.
    crate::enrich::enrich(db, &mut links, refresh);

    output::emit(ctx.json, &links, || {
        if links.is_empty() {
            return format!("no links on {} ({})", quest.id, quest.slug);
        }
        let mut groups: BTreeMap<&str, Vec<&Link>> = BTreeMap::new();
        for l in &links {
            groups.entry(l.kind.as_str()).or_default().push(l);
        }
        let mut out = String::new();
        for (kind, items) in groups {
            out.push_str(kind);
            out.push('\n');
            for l in items {
                out.push_str(&format!("  #{} {}", l.id, l.r#ref));
                if let Some(t) = l.title.as_deref().filter(|t| !t.is_empty()) {
                    out.push_str(&format!(" — {t}"));
                }
                let extras = meta_extras(l);
                if !extras.is_empty() {
                    out.push_str(&format!(" ({})", extras.join(", ")));
                }
                out.push('\n');
            }
        }
        out.trim_end().to_string()
    })
}

/// The enrichment meta the listing shows, in the brief's order: `state`,
/// `status`, `ci` (SPEC §12). Best effort — any absent key is skipped.
fn meta_extras(link: &Link) -> Vec<String> {
    let Some(meta) = link.meta.as_ref().and_then(serde_json::Value::as_object) else {
        return Vec::new();
    };
    ["state", "status", "ci"]
        .iter()
        .filter_map(|key| {
            meta.get(*key)
                .and_then(serde_json::Value::as_str)
                .map(|v| format!("{key}: {v}"))
        })
        .collect()
}

fn line(link: &Link) -> String {
    match link.title.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("link #{} {} {} — {t}", link.id, link.kind, link.r#ref),
        None => format!("link #{} {} {}", link.id, link.kind, link.r#ref),
    }
}

/// `~` and `~/x` become the home directory; anything else is returned as is.
fn expand_tilde(reference: &str) -> String {
    let Some(rest) = reference.strip_prefix('~') else {
        return reference.to_string();
    };
    if !(rest.is_empty() || rest.starts_with('/')) {
        return reference.to_string();
    }
    let Some(home) = dirs::home_dir() else {
        return reference.to_string();
    };
    match rest.trim_start_matches('/') {
        "" => home.to_string_lossy().into_owned(),
        tail => home.join(tail).to_string_lossy().into_owned(),
    }
}

/// Absolute path with the deepest existing ancestor canonicalised (so `/tmp/x`
/// and `/private/tmp/x` are one ref on macOS) and the rest joined verbatim.
pub(crate) fn absolute(cwd: &Path, path: &str) -> String {
    let p = Path::new(path);
    let joined: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    if let Ok(c) = joined.canonicalize() {
        return c.to_string_lossy().into_owned();
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut head = joined.as_path();
    while let Some(parent) = head.parent() {
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            if let Some(name) = head.file_name() {
                out.push(name);
            }
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out.to_string_lossy().into_owned();
        }
        if let Some(name) = head.file_name() {
            tail.push(name.to_os_string());
        }
        head = parent;
    }
    joined.to_string_lossy().into_owned()
}

/// An artifact is a file path: URLs are refused and the parent directory must
/// exist (the file itself may still be on its way).
fn artifact_path(cwd: &Path, path: &str) -> anyhow::Result<String> {
    if path.contains("://") {
        return Err(QError::Invalid(format!(
            "`{path}` looks like a URL; artifacts are files — use `q link add` for URLs"
        ))
        .into());
    }
    let p = Path::new(path);
    let joined: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    let parent = joined.parent().filter(|d| !d.as_os_str().is_empty());
    match parent {
        Some(d) if d.is_dir() => {}
        _ => {
            return Err(
                QError::Invalid(format!("parent directory of `{path}` does not exist")).into(),
            );
        }
    }
    Ok(absolute(cwd, path))
}

/// `https://github.com/<org>/<repo>/pull/<n>` — everything after the number
/// (`/files`, `?diff=`, `#issuecomment`) is dropped, `http`/`www.` unified.
pub(crate) fn normalize_pr_url(reference: &str) -> String {
    let Some(rest) = strip_scheme(reference) else {
        return reference.to_string();
    };
    if !is_github_pr(rest) {
        return reference.to_string();
    }
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.trim_start_matches("github.com/");
    let path = path.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').collect();
    format!(
        "https://github.com/{}/{}/pull/{}",
        parts[0], parts[1], parts[3]
    )
}

// ------------------------------------------------------------ kind detection

/// What a reference looks like on disk, as seen by `detect_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Missing,
    /// A directory containing `.git` (a repo or a worktree).
    GitDir,
    Dir,
    File,
}

fn probe(cwd: &Path) -> impl Fn(&str) -> PathKind + '_ {
    move |reference: &str| {
        let p = Path::new(reference);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        if p.is_dir() {
            if p.join(".git").exists() {
                PathKind::GitDir
            } else {
                PathKind::Dir
            }
        } else if p.is_file() {
            PathKind::File
        } else {
            PathKind::Missing
        }
    }
}

/// SPEC §12 autodetection. Pure apart from the injected filesystem probe;
/// `None` means the caller must pass `--kind`.
pub fn detect_kind(reference: &str, probe: &dyn Fn(&str) -> PathKind) -> Option<LinkKind> {
    let r = reference.trim();
    if r.is_empty() {
        return None;
    }
    if let Some(rest) = strip_scheme(r) {
        if is_github_pr(rest) {
            return Some(LinkKind::Pr);
        }
        if is_productive_task(rest) {
            return Some(LinkKind::Task);
        }
        return Some(LinkKind::Url);
    }
    if is_beads_id(r) {
        return Some(LinkKind::Beads);
    }
    match probe(r) {
        PathKind::GitDir => Some(LinkKind::Worktree),
        // A produced file or an output directory; a plain checkout without
        // `.git` is not a worktree we could enrich.
        PathKind::File | PathKind::Dir => Some(LinkKind::Artifact),
        PathKind::Missing => None,
    }
}

pub(crate) fn strip_scheme(r: &str) -> Option<&str> {
    r.strip_prefix("https://")
        .or_else(|| r.strip_prefix("http://"))
}

/// `github.com/<org>/<repo>/pull/<n>`
pub(crate) fn is_github_pr(rest: &str) -> bool {
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let Some(path) = rest.strip_prefix("github.com/") else {
        return false;
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').collect();
    parts.len() >= 4
        && !parts[0].is_empty()
        && !parts[1].is_empty()
        && parts[2] == "pull"
        && !parts[3].is_empty()
        && parts[3].chars().all(|c| c.is_ascii_digit())
}

/// `app.productive.io/<org>/.../task/<id>` or `.../tasks/<id>` (also
/// `?...task/<id>` deep links, SPEC §12).
pub(crate) fn is_productive_task(rest: &str) -> bool {
    let Some(path) = rest.strip_prefix("app.productive.io/") else {
        return false;
    };
    let mut segments = path.split(['/', '?', '&', '=']).peekable();
    while let Some(seg) = segments.next() {
        if (seg == "task" || seg == "tasks")
            && segments
                .peek()
                .is_some_and(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
        {
            return true;
        }
    }
    false
}

/// `bd-<id>` where the id is lowercase alphanumerics and dots (`bd-8lz.2.5`).
fn is_beads_id(r: &str) -> bool {
    r.strip_prefix("bd-").is_some_and(|id| {
        !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.')
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs(kind: PathKind) -> impl Fn(&str) -> PathKind {
        move |_| kind
    }

    #[test]
    fn detects_kinds_from_reference_patterns() {
        use LinkKind as K;
        let cases: &[(&str, PathKind, Option<K>)] = &[
            (
                "https://github.com/acme/api/pull/42",
                PathKind::Missing,
                Some(K::Pr),
            ),
            (
                "http://www.github.com/acme/api/pull/42?x=1",
                PathKind::Missing,
                Some(K::Pr),
            ),
            (
                "https://github.com/acme/api/pull/",
                PathKind::Missing,
                Some(K::Url),
            ),
            (
                "https://github.com/acme/api/pull/abc",
                PathKind::Missing,
                Some(K::Url),
            ),
            (
                "https://github.com/acme/api/issues/42",
                PathKind::Missing,
                Some(K::Url),
            ),
            (
                "https://app.productive.io/1-acme/tasks/123",
                PathKind::Missing,
                Some(K::Task),
            ),
            (
                "https://app.productive.io/1-acme/tasks?filter=1&task/123",
                PathKind::Missing,
                Some(K::Task),
            ),
            (
                "https://app.productive.io/1-acme/task/9",
                PathKind::Missing,
                Some(K::Task),
            ),
            (
                "https://app.productive.io/1-acme/tasks",
                PathKind::Missing,
                Some(K::Url),
            ),
            (
                "https://app.productive.io/1-acme/tasks/abc",
                PathKind::Missing,
                Some(K::Url),
            ),
            ("https://example.com/x", PathKind::Missing, Some(K::Url)),
            ("http://example.com", PathKind::Missing, Some(K::Url)),
            ("bd-8lz.2.5", PathKind::Missing, Some(K::Beads)),
            ("bd-123", PathKind::Missing, Some(K::Beads)),
            ("bd-", PathKind::Missing, None),
            ("bd-ABC", PathKind::Missing, None),
            ("/repo/.worktrees/x", PathKind::GitDir, Some(K::Worktree)),
            (".worktrees/x", PathKind::GitDir, Some(K::Worktree)),
            ("output/report.html", PathKind::File, Some(K::Artifact)),
            ("output", PathKind::Dir, Some(K::Artifact)),
            ("feat/some-branch", PathKind::Missing, None),
            ("", PathKind::Missing, None),
            ("   ", PathKind::Missing, None),
            // A URL wins over whatever happens to be on disk.
            ("https://example.com", PathKind::GitDir, Some(K::Url)),
        ];
        for (reference, on_disk, want) in cases {
            let got = detect_kind(reference, &fs(*on_disk));
            assert_eq!(got, *want, "`{reference}` with {on_disk:?}");
        }
    }

    #[test]
    fn absolute_paths_stay_and_relative_ones_join_cwd() {
        let cwd = Path::new("/nope/work");
        assert_eq!(absolute(cwd, "/nope/hosts"), "/nope/hosts");
        assert_eq!(absolute(cwd, "nope/report.md"), "/nope/work/nope/report.md");
    }

    #[test]
    fn absolute_canonicalises_the_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap();
        let got = absolute(dir.path(), "later/report.md");
        assert_eq!(got, real.join("later/report.md").to_string_lossy());
        let got = absolute(Path::new("/"), &dir.path().join("x.md").to_string_lossy());
        assert_eq!(got, real.join("x.md").to_string_lossy());
    }

    #[test]
    fn artifact_path_rejects_urls_and_missing_parents() {
        let dir = tempfile::tempdir().unwrap();
        let e = artifact_path(dir.path(), "https://example.com/x").unwrap_err();
        assert!(e.to_string().contains("URL"), "{e}");
        let e = artifact_path(dir.path(), "nope/deeper/x.md").unwrap_err();
        assert!(e.to_string().contains("parent directory"), "{e}");
        let ok = artifact_path(dir.path(), "x.md").unwrap();
        assert!(ok.ends_with("/x.md"), "{ok}");
    }

    #[test]
    fn pr_urls_are_normalised_others_untouched() {
        for (input, want) in [
            (
                "http://www.github.com/acme/api/pull/42/files?diff=split#r1",
                "https://github.com/acme/api/pull/42",
            ),
            (
                "https://github.com/acme/api/pull/42",
                "https://github.com/acme/api/pull/42",
            ),
            (
                "https://github.com/acme/api/issues/42",
                "https://github.com/acme/api/issues/42",
            ),
            ("bd-1", "bd-1"),
        ] {
            assert_eq!(normalize_pr_url(input), want, "{input}");
        }
    }

    #[test]
    fn tilde_expands_only_as_a_home_prefix() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home.to_string_lossy());
        assert_eq!(
            expand_tilde("~/out/a.md"),
            home.join("out/a.md").to_string_lossy()
        );
        assert_eq!(expand_tilde("~user/x"), "~user/x");
        assert_eq!(expand_tilde("a/~/b"), "a/~/b");
    }
}
