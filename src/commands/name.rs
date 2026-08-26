//! `q name <quest> [--auto] [--apply] [--refresh] [--detach]` (SPEC §10, §16).
//!
//! Without `--auto` this only reports how the Quest got its current name.
//! `--auto` proposes one — from the cache, from `claude -p`, or from the
//! heuristic — and prints it; `--apply` also renames. `--detach` re-runs the
//! same command in the background and returns at once, which is how the
//! master's `Stop` hook keeps naming off its critical path.

use crate::Ctx;
use crate::commands::{rename, sweep_quiet};
use crate::model::NameSource;
use crate::naming::{self, Input};
use crate::output;

pub struct Args<'a> {
    pub quest: &'a str,
    pub auto: bool,
    pub apply: bool,
    pub refresh: bool,
    pub detach: bool,
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
        return output::emit(
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
        );
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

    // The proposal may collide with another Quest; an auto name steps aside
    // rather than failing, exactly as `q new` does.
    let slug = naming::free_slug(db, &quest, &proposal.slug)?.unwrap_or_else(|| quest.slug.clone());
    let renamed = rename::apply(
        ctx,
        &quest,
        &slug,
        NameSource::Auto,
        Some(&proposal.input_hash),
    )?;
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
    renamed.quest = ctx.db()?.update_quest(
        &renamed.quest.id,
        &crate::db::quest::QuestPatch {
            name_input_hash: Some(Some(input_hash.to_string())),
            ..Default::default()
        },
    )?;
    Ok(renamed)
}

/// Re-runs this command detached and returns immediately.
fn detach(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let mut argv = vec!["name".to_string(), args.quest.to_string()];
    for (on, flag) in [
        (args.auto, "--auto"),
        (args.apply, "--apply"),
        (args.refresh, "--refresh"),
    ] {
        if on {
            argv.push(flag.to_string());
        }
    }
    naming::spawn_detached(&argv)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({ "quest": args.quest, "detached": true, "args": argv }),
            || format!("naming {} in the background", args.quest),
        )?;
    }
    Ok(())
}
