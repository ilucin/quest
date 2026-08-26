//! `q set <quest> <key> <value>` — the Quest properties the CLI can change
//! (SPEC §16).

use crate::Ctx;
use crate::beads;
use crate::cli::SetKey;
use crate::commands::new::resolve_dir;
use crate::commands::sweep_quiet;
use crate::db::quest::QuestPatch;
use crate::error::QError;
use crate::output;

/// Spellings that clear `ctx_reset_pct` back to the configured default.
const CLEAR: [&str; 3] = ["default", "none", ""];

pub fn run(ctx: &Ctx, target: &str, key: SetKey, value: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    // The label the epic carries right now, before the column is overwritten.
    let had_repo = quest.beads_repo.clone();

    let mut patch = QuestPatch::default();
    // What actually landed in the column, for the event and the payload.
    let stored;
    match key {
        SetKey::Goal => {
            stored = value.trim().to_string();
            patch.goal = Some(blank_to_null(&stored));
        }
        SetKey::Cwd => {
            let dir = resolve_dir(Some(value))?;
            stored = dir.to_string_lossy().into_owned();
            patch.cwd = Some(stored.clone());
        }
        // TODO(M5): validate against the workflow registry.
        SetKey::Workflow => {
            stored = value.trim().to_string();
            patch.workflow = Some(blank_to_null(&stored));
        }
        SetKey::BeadsEpic => {
            stored = beads::validate_epic_id(value)?;
            patch.beads_epic = Some(blank_to_null(&stored));
        }
        SetKey::BeadsRepo => {
            stored = beads::validate_repo_label(value)?;
            patch.beads_repo = Some(blank_to_null(&stored));
        }
        SetKey::CtxResetPct => {
            let pct = parse_pct(value)?;
            stored = match pct {
                Some(p) => p.to_string(),
                None => "default".to_string(),
            };
            patch.ctx_reset_pct = Some(pct);
        }
    }
    let quest = db.update_quest(&quest.id, &patch)?;
    let relabelled = match key {
        SetKey::BeadsRepo => relabel_epic(ctx, &quest, had_repo.as_deref(), &stored),
        _ => false,
    };
    let key = key_name(key);
    db.append_event(
        &quest.id,
        None,
        "quest.updated",
        &serde_json::json!({ "key": key, "value": stored }),
    )?;

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "key": key,
                "value": stored,
                "epic_relabelled": relabelled,
            }),
            || {
                let moved = if relabelled {
                    " · epic relabelled"
                } else {
                    ""
                };
                format!("{} ({}) · {key} = {stored}{moved}", quest.id, quest.slug)
            },
        )?;
    }
    Ok(())
}

/// `beads_repo` is not q's property: the label lives on the epic, and agents
/// are told to reuse it. Changing only q's copy would leave the two disagreeing
/// with nothing to say so, so the epic is relabelled in the same breath —
/// `bd update --remove-label … --add-label …`, one write. A `bd` that will not
/// cooperate is a warning with the command to run by hand, never a failed
/// `q set`: the column is already stored by then.
fn relabel_epic(ctx: &Ctx, quest: &crate::model::Quest, old: Option<&str>, new: &str) -> bool {
    let Some(epic) = beads::epic_of(quest) else {
        return false;
    };
    let old = old.map(str::trim).filter(|o| !o.is_empty());
    if new.is_empty() {
        if let Some(old) = old {
            eprintln!(
                "note: quest {} no longer records a repo label, but epic {epic} still \
                 carries repo:{old}; remove it with `bd label remove {epic} repo:{old}`",
                quest.slug
            );
        }
        return false;
    }
    // Re-setting the same value is how a label that drifted gets repaired, so
    // there is nothing to remove in that case — only the label to (re)add.
    let remove = old.filter(|o| *o != new);
    match beads::client().relabel_repo(epic, remove, new) {
        Ok(()) => {
            let _ = ctx.db().and_then(|db| {
                db.append_event(
                    &quest.id,
                    None,
                    "beads.epic_relabelled",
                    &serde_json::json!({ "epic": epic, "from": old, "to": new }),
                )
            });
            true
        }
        Err(e) => {
            let undo = match remove {
                Some(old) => format!(" --remove-label repo:{old}"),
                None => String::new(),
            };
            eprintln!(
                "warning: epic {epic} could not be relabelled ({e}); it still carries \
                 repo:{}; fix it with `bd update {epic}{undo} --add-label repo:{new}`",
                old.unwrap_or("-")
            );
            false
        }
    }
}

/// An empty value clears the column rather than storing `""`.
fn blank_to_null(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// `1`..`100`, or one of `CLEAR` to fall back to `[context] master_reset_pct`.
fn parse_pct(value: &str) -> anyhow::Result<Option<u8>> {
    let trimmed = value.trim();
    if CLEAR.contains(&trimmed.to_ascii_lowercase().as_str()) {
        return Ok(None);
    }
    let pct: u8 = trimmed
        .parse()
        .ok()
        .filter(|p| (1..=100).contains(p))
        .ok_or_else(|| {
            QError::Other(format!(
                "invalid ctx_reset_pct `{value}`: expected 1-100, or `default` to clear it"
            ))
        })?;
    Ok(Some(pct))
}

fn key_name(key: SetKey) -> &'static str {
    match key {
        SetKey::Goal => "goal",
        SetKey::Cwd => "cwd",
        SetKey::Workflow => "workflow",
        SetKey::CtxResetPct => "ctx_reset_pct",
        SetKey::BeadsEpic => "beads_epic",
        SetKey::BeadsRepo => "beads_repo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_reset_pct_takes_a_percentage_or_clears() {
        assert_eq!(parse_pct("35").unwrap(), Some(35));
        assert_eq!(parse_pct(" 100 ").unwrap(), Some(100));
        assert_eq!(parse_pct("default").unwrap(), None);
        assert_eq!(parse_pct("NONE").unwrap(), None);
        assert_eq!(parse_pct("").unwrap(), None);
        for bad in ["0", "101", "-1", "half"] {
            assert!(parse_pct(bad).is_err(), "accepted `{bad}`");
        }
    }
}
