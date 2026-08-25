//! `q rename` — a new slug, and the tmux session that follows it (SPEC §6).

use crate::Ctx;
use crate::commands::new::validate_slug;
use crate::commands::sweep_quiet;
use crate::db::quest::QuestPatch;
use crate::error::QError;
use crate::model::NameSource;
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, slug: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    validate_slug(slug)?;
    let from = quest.slug.clone();
    if from == slug {
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({ "quest": quest, "from": from, "to": slug }),
                || format!("{} is already named {slug}", quest.id),
            )?;
        }
        return Ok(());
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

    // TODO(M2): tell the running Claude sessions via `/rename`.
    let patch = QuestPatch {
        slug: Some(slug.to_string()),
        name_source: Some(NameSource::Manual),
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
        &serde_json::json!({ "from": from, "to": quest.slug }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "from": from,
                "to": quest.slug,
                "tmux_session": new_session,
            }),
            || {
                format!(
                    "renamed {} · {from} → {} · tmux {new_session}",
                    quest.id, quest.slug
                )
            },
        )?;
    }
    Ok(())
}
