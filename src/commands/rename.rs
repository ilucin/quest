//! `q rename` — a new slug, and the tmux session and Claude session names that
//! follow it (SPEC §6, §10).
//!
//! `apply` is the whole rename as a library call, so `q name --apply`
//! (auto-naming) renames exactly the way `q rename` does.

use serde::Serialize;

use crate::Ctx;
use crate::commands::new::validate_slug;
use crate::commands::sweep_quiet;
use crate::db::quest::QuestPatch;
use crate::error::QError;
use crate::model::{NameSource, Quest};
use crate::naming::{self, Sync};
use crate::output;
use crate::tmux::session_name;

/// What a rename did, for the payload of whatever asked for it.
#[derive(Debug, Clone, Serialize)]
pub struct Renamed {
    pub quest: Quest,
    pub from: String,
    pub to: String,
    pub tmux_session: String,
    pub changed: bool,
    /// The live Claude sessions told their new name, and those still owed one.
    #[serde(flatten)]
    pub sync: Sync,
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
    db.update_sessions_tmux_session(&quest.id, &new_session)?;
    db.append_event(
        &quest.id,
        None,
        "name.changed",
        &serde_json::json!({ "from": from, "to": quest.slug, "source": source }),
    )?;
    // Claude keeps its own session name; it only follows when the pane is idle.
    let sync = naming::sync_claude_names(db, ctx.tmux(), &quest)?;

    Ok(Renamed {
        quest,
        from,
        to: slug.to_string(),
        tmux_session: new_session,
        changed: true,
        sync,
    })
}
