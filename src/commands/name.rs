//! `q name <quest> [--auto] [--apply] [--refresh] [--detach]` (SPEC §10, §16).
//!
//! Without `--auto` this only reports how the Quest got its current name.
//! `--auto` proposes one — from the cache, from `claude -p`, or from the
//! heuristic — and prints it; `--apply` also renames. `--detach` re-runs the
//! same command in the background and returns at once, which is how the
//! master's `Stop` hook keeps naming off its critical path.

use crate::Ctx;
use crate::commands::new::Claim;
use crate::commands::{new, rename, sweep_quiet};
use crate::error::QError;
use crate::model::NameSource;
use crate::naming::{self, Input};
use crate::output;

pub struct Args<'a> {
    pub quest: &'a str,
    pub auto: bool,
    pub apply: bool,
    pub refresh: bool,
    pub detach: bool,
    pub force: bool,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    if args.detach {
        return detach(ctx, args);
    }
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(args.quest)?;

    let input = Input::collect(db, &quest);
    if !args.auto {
        let hash = input.hash();
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({
                    "quest": quest,
                    "slug": quest.slug,
                    "name_source": quest.name_source,
                    "input_hash": hash,
                    "stored_input_hash": quest.name_input_hash,
                    "stale": quest.name_input_hash.as_deref() != Some(hash.as_str()),
                }),
                || {
                    format!(
                        "{} · {} ({}) · propose a new one with: q name {} --auto",
                        quest.id, quest.slug, quest.name_source, quest.slug
                    )
                },
            )?;
        }
        return Ok(());
    }

    // A name somebody chose is not auto-naming's to take over: `--apply` would
    // set `name_source = auto`, and every later `Stop` hook would then keep
    // renaming the Quest behind the user's back.
    if args.apply && quest.name_source != NameSource::Auto && !args.force {
        return Err(QError::Conflict(format!(
            "{} is named `{}` ({}); pass --force to hand it to auto-naming",
            quest.id, quest.slug, quest.name_source
        ))
        .into());
    }

    let proposal = naming::propose(
        db,
        &quest,
        &input,
        &ctx.config.naming.model,
        naming::namer().as_ref(),
        args.refresh,
    )?;

    if !args.apply {
        if ctx.json || !ctx.quiet {
            output::emit(
                ctx.json,
                &serde_json::json!({
                    "quest": quest,
                    "current": quest.slug,
                    "proposal": proposal,
                    "applied": false,
                }),
                || {
                    format!(
                        "{} · {} → {} · apply it with: q name {} --auto --apply",
                        quest.id,
                        quest.slug,
                        proposal.describe(),
                        quest.slug
                    )
                },
            )?;
        }
        return Ok(());
    }

    // The proposal may collide with another Quest or a stray tmux session; an
    // auto name steps aside rather than failing, exactly as `q new` does. With
    // every variant taken it keeps the name it has.
    let slug = match new::claim(ctx, &proposal.slug, &quest.slug)? {
        Claim::Free(slug) => slug,
        Claim::Own | Claim::Exhausted => quest.slug.clone(),
    };
    let renamed = match rename::apply(
        ctx,
        &quest,
        &slug,
        NameSource::Auto,
        Some(&proposal.input_hash),
    ) {
        Ok(renamed) => renamed,
        // Terminal: the input has been paid for and would hash the same next
        // time, so stamping it is what stops every following `Stop` hook from
        // buying the identical model answer again. The failure is logged
        // because the caller is usually a detached child nobody watches.
        Err(e) => {
            let _ = stamp(ctx, &quest.id, &proposal.input_hash);
            let _ = db.append_event(
                &quest.id,
                None,
                "name.failed",
                &serde_json::json!({
                    "slug": slug,
                    "proposal": proposal,
                    "error": e.to_string(),
                }),
            );
            return Err(e);
        }
    };
    // A no-op rename still has to record what it named from, or every `Stop`
    // hook would schedule the same regeneration again.
    let renamed = match renamed.changed {
        true => renamed,
        false => stamp_hash(ctx, renamed, &proposal.input_hash)?,
    };

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": renamed.quest,
                "current": renamed.to,
                "proposal": proposal,
                "applied": true,
                "renamed": renamed,
            }),
            || match renamed.changed {
                true => renamed.describe(),
                false => format!(
                    "{} keeps {} ({})",
                    renamed.quest.id,
                    renamed.to,
                    proposal.describe()
                ),
            },
        )?;
    }
    Ok(())
}

/// Writes `name_input_hash` on a rename that changed nothing else.
fn stamp_hash(
    ctx: &Ctx,
    mut renamed: rename::Renamed,
    input_hash: &str,
) -> anyhow::Result<rename::Renamed> {
    renamed.quest = stamp(ctx, &renamed.quest.id, input_hash)?;
    Ok(renamed)
}

fn stamp(ctx: &Ctx, quest: &str, input_hash: &str) -> anyhow::Result<crate::model::Quest> {
    ctx.db()?.update_quest(
        quest,
        &crate::db::quest::QuestPatch {
            name_input_hash: Some(Some(input_hash.to_string())),
            ..Default::default()
        },
    )
}

/// Re-runs this command detached and returns immediately.
///
/// The Quest is resolved first: the child's stdio goes to `/dev/null`, so a
/// typo'd id would otherwise fail where nobody can see it.
fn detach(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(args.quest)?;
    let mut argv = vec!["name".to_string(), quest.id.clone()];
    for (on, flag) in [
        (args.auto, "--auto"),
        (args.apply, "--apply"),
        (args.refresh, "--refresh"),
        (args.force, "--force"),
    ] {
        if on {
            argv.push(flag.to_string());
        }
    }
    naming::spawn_detached(&argv)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "quest": quest.id, "detached": true, "args": argv }),
            || format!("naming {} in the background", quest.slug),
        )?;
    }
    Ok(())
}
