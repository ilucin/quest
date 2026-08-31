//! `q rename` — a new slug, and the tmux session and Claude session names that
//! follow it (SPEC §6, §10).
//!
//! `apply` is the whole rename as a library call, so `q name --apply`
//! (auto-naming) renames exactly the way `q rename` does.

use serde::Serialize;

use crate::Ctx;
use crate::commands::new::validate_slug;
use crate::commands::sweep_quiet;
use crate::db::Db;
use crate::db::quest::QuestPatch;
use crate::error::QError;
use crate::model::{NameSource, Quest};
use crate::naming::{self, Sync};
use crate::output;
use crate::tmux::{WORKER_SEP, live_panes, session_name, sessions_of_quest, worker_session_name};

/// What a rename did, for the payload of whatever asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct Renamed {
    pub quest: Quest,
    pub from: String,
    pub to: String,
    pub tmux_session: String,
    pub changed: bool,
    /// Workers whose tmux session did not follow the rename (SPEC §6 v2): they
    /// stay reachable under the old name, and `q doctor` reports the mismatch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stranded: Vec<Stranded>,
    /// The live Claude sessions told their new name, and those still owed one.
    #[serde(flatten)]
    pub sync: Sync,
}

/// A worker whose tmux rename did not land during a fleet rename (SPEC §6 v2).
/// Its Claude session keeps running under the old tmux name; `q doctor` flags
/// the mismatch and `q close`/`q rm` still tear it down (R7).
#[derive(Debug, Clone, Serialize)]
pub struct Stranded {
    /// The session row that owns this tmux session, when one does; a rowless,
    /// hand-made pane carries `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    pub tmux_session: String,
    pub reason: String,
}

impl Renamed {
    /// The one-line human rendering both `q rename` and `q name --apply` print.
    pub fn describe(&self) -> String {
        if !self.changed {
            return format!("{} is already named {}", self.quest.id, self.to);
        }
        let mut line = format!(
            "renamed {} · {} → {} · tmux {}",
            self.quest.id, self.from, self.to, self.tmux_session
        );
        if !self.sync.pending.is_empty() {
            line.push_str(&format!(
                " · /rename held for {}",
                self.sync.pending.join(", ")
            ));
        }
        if !self.stranded.is_empty() {
            let names: Vec<&str> = self
                .stranded
                .iter()
                .map(|s| s.tmux_session.as_str())
                .collect();
            line.push_str(&format!(
                " · {} worker(s) stranded under the old name (run q doctor): {}",
                names.len(),
                names.join(", ")
            ));
        }
        line
    }
}

pub fn run(ctx: &Ctx, target: &str, slug: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(target)?;
    let out = apply(ctx, &quest, slug, NameSource::Manual, None)?;
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &out, || out.describe())?;
    }
    Ok(())
}

/// Renames `quest` to `slug`: the tmux session, the session rows, the Quest row
/// (with `name_source`, and `name_input_hash` when the caller has one), the
/// `name.changed` event, and a `/rename` to every live Claude session.
///
/// `input_hash` is only written when given, so `q rename` leaves the
/// auto-naming hash alone while `q name --apply` records what it named from.
pub fn apply(
    ctx: &Ctx,
    quest: &Quest,
    slug: &str,
    source: NameSource,
    input_hash: Option<&str>,
) -> anyhow::Result<Renamed> {
    let db = ctx.db()?;
    validate_slug(slug)?;
    let from = quest.slug.clone();
    if from == slug {
        return Ok(Renamed {
            tmux_session: session_name(&ctx.config, slug),
            from,
            to: slug.to_string(),
            quest: quest.clone(),
            changed: false,
            stranded: Vec::new(),
            sync: Sync::default(),
        });
    }
    if let Some(other) = db.get_quest_by_slug(slug)? {
        return Err(QError::Other(format!(
            "slug `{slug}` is already taken by quest {}",
            other.id
        ))
        .into());
    }

    let old_session = session_name(&ctx.config, &from);
    let new_session = session_name(&ctx.config, slug);
    if ctx.tmux().has_session(&new_session)? {
        return Err(QError::Tmux(format!("tmux session `{new_session}` already exists")).into());
    }
    // The main session is renamed first and hard: it is the one a failure must
    // roll back cleanly, before any database write. The workers follow
    // best-effort (SPEC §6 v2 — a fleet of sessions), and a rename that half
    // fails there is a doctor mismatch, not a failed `q rename`.
    let renamed = ctx.tmux().has_session(&old_session)?;
    if renamed {
        ctx.tmux().rename_session(&old_session, &new_session)?;
    }

    let patch = QuestPatch {
        slug: Some(slug.to_string()),
        name_source: Some(source),
        name_input_hash: input_hash.map(|h| Some(h.to_string())),
        ..QuestPatch::default()
    };
    let quest = match db.update_quest(&quest.id, &patch) {
        Ok(quest) => quest,
        Err(e) => {
            if renamed {
                let _ = ctx.tmux().rename_session(&new_session, &old_session);
            }
            return Err(e);
        }
    };
    let stranded = rename_fleet(ctx, db, &quest, &from, slug)?;
    db.append_event(
        &quest.id,
        None,
        "name.changed",
        &serde_json::json!({ "from": from, "to": quest.slug, "source": source }),
    )?;
    // Claude keeps its own session name; it only follows when the pane is idle.
    let sync = naming::sync_claude_names(db, ctx.tmux(), &quest, &from)?;
    // The epic was titled after the old slug (`beads::epic_title`).
    crate::beads::sync_epic_title(ctx, &quest);

    Ok(Renamed {
        quest,
        from,
        to: slug.to_string(),
        tmux_session: new_session,
        changed: true,
        stranded,
        sync,
    })
}

/// Move the rest of the fleet under the new slug (SPEC §6 v2): rename every
/// worker tmux session `q-<from>+<label>` → `q-<slug>+<label>` best-effort, and
/// remap every live session row's `tmux_session`. The main was already renamed
/// by [`apply`]; a pre-v2 worker row still carrying the main name is remapped to
/// the new main. A worker tmux rename that fails leaves a mismatch reported both
/// on the rename itself (as [`Stranded`]) and by `q doctor`, rather than failing
/// the whole rename.
fn rename_fleet(
    ctx: &Ctx,
    db: &Db,
    quest: &Quest,
    from: &str,
    slug: &str,
) -> anyhow::Result<Vec<Stranded>> {
    let old_main = session_name(&ctx.config, from);
    let worker_prefix = format!("{old_main}{WORKER_SEP}");
    // Rename the live worker tmux sessions (rowless ones included). A worker
    // whose target name is already taken, or whose rename errors, is left where
    // it is — and its old name is remembered so the row remap below does **not**
    // follow it. Remapping a row onto a name another session owns would make the
    // next sweep key orphan detection on a `(tmux_session, pane)` pair that no
    // longer exists, ending a live worker (correctness review #1).
    let panes = live_panes(ctx.tmux())?;
    let mut stranded: Vec<(String, &'static str)> = Vec::new();
    for name in sessions_of_quest(&ctx.config, &panes, from) {
        if let Some(label) = name.strip_prefix(&worker_prefix) {
            let target = worker_session_name(&ctx.config, slug, label);
            if ctx.tmux().has_session(&target).unwrap_or(false) {
                stranded.push((name.clone(), "target tmux session already exists"));
            } else if ctx.tmux().rename_session(&name, &target).is_err() {
                stranded.push((name.clone(), "tmux rename failed"));
            }
        }
    }
    let stranded_names: std::collections::HashSet<&str> =
        stranded.iter().map(|(n, _)| n.as_str()).collect();
    // Remap every live row's tmux_session to the new slug, except the workers
    // whose tmux rename did not land — those keep their old name so the live
    // pane stays reachable and the mismatch is reported instead.
    let rows = db.list_sessions_by_quest(&quest.id)?;
    for session in &rows {
        if session.status == crate::model::SessionStatus::Ended {
            continue;
        }
        if stranded_names.contains(session.tmux_session.as_str()) {
            continue;
        }
        if let Some(new_name) = remapped(&ctx.config, &session.tmux_session, from, slug) {
            db.update_session_tmux_session(&session.id, &new_name)?;
        }
    }
    Ok(stranded
        .into_iter()
        .map(|(name, reason)| Stranded {
            session: rows
                .iter()
                .find(|s| s.tmux_session == name)
                .map(|s| s.id.clone()),
            tmux_session: name,
            reason: reason.to_string(),
        })
        .collect())
}

/// A live row's tmux session name under the new slug, or `None` when it does not
/// belong to `from`. `q-<from>` → `q-<slug>`; `q-<from>+<label>` →
/// `q-<slug>+<label>`; the `+`-split keeps a slug that itself contains `-` whole.
fn remapped(
    config: &crate::config::Config,
    tmux_session: &str,
    from: &str,
    slug: &str,
) -> Option<String> {
    let rest = tmux_session.strip_prefix(config.tmux.session_prefix.as_str())?;
    let (quest_part, suffix) = match rest.split_once(WORKER_SEP) {
        Some((q, s)) => (q, Some(s)),
        None => (rest, None),
    };
    if quest_part != from {
        return None;
    }
    Some(match suffix {
        Some(label) => worker_session_name(config, slug, label),
        None => session_name(config, slug),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn remapped_moves_main_and_workers_but_not_a_sibling() {
        let config = Config::default();
        assert_eq!(
            remapped(&config, "q-foo", "foo", "bar").as_deref(),
            Some("q-bar")
        );
        assert_eq!(
            remapped(&config, "q-foo+review", "foo", "bar").as_deref(),
            Some("q-bar+review")
        );
        // A sibling Quest's session is not ours.
        assert_eq!(remapped(&config, "q-foo-2", "foo", "bar"), None);
        assert_eq!(remapped(&config, "irssi", "foo", "bar"), None);
    }
}
