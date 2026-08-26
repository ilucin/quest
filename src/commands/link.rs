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
    let reference = args.r#ref.trim();
    if reference.is_empty() {
        return Err(QError::Invalid("reference must not be empty".to_string()).into());
    }
    let target = report::resolve(ctx, args.quest)?;
    let cwd = std::env::current_dir()?;
    let kind = match args.kind {
        Some(k) => k,
        None => detect_kind(reference, &probe(&cwd)).ok_or_else(|| {
            QError::Invalid(format!(
                "cannot tell what `{reference}` is; pass --kind <pr|task|worktree|url|branch|beads|brain|artifact>"
            ))
        })?,
    };
    // Paths are stored absolute so every session and machine reads the same ref.
    let reference = match kind {
        LinkKind::Worktree | LinkKind::Artifact => absolute(&cwd, reference),
        _ => reference.to_string(),
    };
    let mut link = Link::new(&target.quest.id, kind.as_str(), &reference);
    link.title = args.title.map(str::to_string);
    insert(ctx, &target, link, "link.added", None)
}

pub fn add_artifact(
    ctx: &Ctx,
    path: &str,
    note: Option<&str>,
    quest: Option<&str>,
) -> anyhow::Result<()> {
    let path = path.trim();
    if path.is_empty() {
        return Err(QError::Invalid("path must not be empty".to_string()).into());
    }
    let target = report::resolve(ctx, quest)?;
    let cwd = std::env::current_dir()?;
    let mut link = Link::new(&target.quest.id, "artifact", &absolute(&cwd, path));
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    if let Some(n) = note {
        link.meta = Some(serde_json::json!({ "note": n }));
    }
    insert(ctx, &target, link, "artifact.added", note)
}

/// Idempotent on `UNIQUE(quest_id, kind, ref)`: an existing row is returned as
/// is and no event is written.
fn insert(
    ctx: &Ctx,
    target: &Target,
    mut link: Link,
    event_kind: &str,
    note: Option<&str>,
) -> anyhow::Result<()> {
    let db = ctx.db()?;
    let existing = db.find_link(&target.quest.id, &link.kind, &link.r#ref)?;
    let (link, created) = match existing {
        Some(l) => (l, false),
        None => {
            link.session_id = target.session_id().map(str::to_string);
            let stored = db.insert_link(&link)?;
            let mut payload = serde_json::json!({
                "id": stored.id,
                "kind": stored.kind,
                "ref": stored.r#ref,
            });
            if let Some(n) = note {
                payload["note"] = serde_json::Value::String(n.to_string());
            }
            db.append_event(&target.quest.id, target.session_id(), event_kind, &payload)?;
            (stored, true)
        }
    };

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "link": link, "created": created }),
            || {
                let suffix = if created { "" } else { " (already linked)" };
                format!("{}{suffix}", line(&link))
            },
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
    if refresh && !ctx.quiet {
        // TODO(M2): enrichment; the flag is accepted so scripts can start using it.
        eprintln!("note: --refresh is not implemented yet; showing stored links");
    }
    let target = report::resolve(ctx, quest)?;
    let links = ctx.db()?.list_links_by_quest(&target.quest.id)?;

    output::emit(ctx.json, &links, || {
        if links.is_empty() {
            return format!("no links on {} ({})", target.quest.id, target.quest.slug);
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
                out.push('\n');
            }
        }
        out.trim_end().to_string()
    })
}

fn line(link: &Link) -> String {
    match link.title.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("link #{} {} {} — {t}", link.id, link.kind, link.r#ref),
        None => format!("link #{} {} {}", link.id, link.kind, link.r#ref),
    }
}

fn absolute(cwd: &Path, path: &str) -> String {
    let p = Path::new(path);
    let joined: PathBuf = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    joined
        .canonicalize()
        .unwrap_or(joined)
        .to_string_lossy()
        .into_owned()
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

fn strip_scheme(r: &str) -> Option<&str> {
    r.strip_prefix("https://")
        .or_else(|| r.strip_prefix("http://"))
}

/// `github.com/<org>/<repo>/pull/<n>`
fn is_github_pr(rest: &str) -> bool {
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
fn is_productive_task(rest: &str) -> bool {
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
        let cwd = Path::new("/tmp/work");
        assert_eq!(absolute(cwd, "/nope/hosts"), "/nope/hosts");
        assert_eq!(absolute(cwd, "nope/report.md"), "/tmp/work/nope/report.md");
    }
}
