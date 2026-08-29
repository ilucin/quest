//! `q set <quest> <key> <value>` — the Quest properties the CLI can change
//! (SPEC §16).

use crate::Ctx;
use crate::beads;
use crate::cli::SetKey;
use crate::commands::new::resolve_dir;
use crate::commands::sweep_quiet;
use crate::db::quest::QuestPatch;
use crate::error::QError;
use crate::model::Quest;
use crate::output;

/// Spellings that clear `ctx_reset_pct` back to the configured default.
const CLEAR: [&str; 3] = ["default", "none", ""];

/// `q set <quest> beads_epic new` — mint an epic rather than link one.
pub const NEW_EPIC: &str = "new";

/// What one `q set` did, for the payload of whatever asked for it.
pub struct Applied {
    pub quest: Quest,
    pub key: &'static str,
    /// What actually landed in the column.
    pub value: String,
    pub epic_relabelled: bool,
}

impl Applied {
    pub fn describe(&self) -> String {
        let moved = if self.epic_relabelled {
            " · epic relabelled"
        } else {
            ""
        };
        format!(
            "{} ({}) · {} = {}{moved}",
            self.quest.id, self.quest.slug, self.key, self.value
        )
    }
}

pub fn run(ctx: &Ctx, target: &str, key: SetKey, value: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(target)?;
    let out = apply(ctx, &quest, key, value)?;
    crate::commands::flush_warnings(ctx);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": out.quest,
                "key": out.key,
                "value": out.value,
                "epic_relabelled": out.epic_relabelled,
            }),
            || out.describe(),
        )?;
    }
    Ok(())
}

/// The whole of `q set` as a library call, so the TUI's edit form writes
/// exactly what the CLI writes. Warnings stay buffered on the `Ctx`.
pub fn apply(ctx: &Ctx, quest: &Quest, key: SetKey, value: &str) -> anyhow::Result<Applied> {
    let db = ctx.db()?;
    // The label the epic carries right now, before the column is overwritten.
    let had_repo = quest.beads_repo.clone();

    // A Quest made with the TUI's bare `n` has no epic; this is how it gets
    // one afterwards, titled from the slug and goal it has by now.
    if key == SetKey::BeadsEpic && value.trim() == NEW_EPIC {
        return new_epic(ctx, quest);
    }

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
        // SPEC §11: a workflow is a file, so this is where the name is
        // checked — a blank one clears the column and is not a name at all.
        SetKey::Workflow => {
            // `check_opt` is the one door `q new --workflow` and `q spawn
            // --workflow` also go through: trimmed, checked, and what comes
            // back is what is stored.
            let checked = ctx.workflows().check_opt(Some(value))?;
            stored = checked.clone().unwrap_or_default();
            patch.workflow = Some(checked);
        }
        SetKey::BeadsEpic => {
            stored = beads::validate_epic_id(value)?;
            patch.beads_epic = Some(blank_to_null(&stored));
        }
        SetKey::BeadsRepo => {
            stored = beads::validate_repo_label(value)?;
            patch.beads_repo = Some(blank_to_null(&stored));
        }
        SetKey::AutoReset => {
            let on = parse_toggle(value)?;
            stored = match on {
                Some(true) => "on".to_string(),
                Some(false) => "off".to_string(),
                None => "default".to_string(),
            };
            patch.auto_reset = Some(on);
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
        // The epic's title carries the goal (`beads::epic_title`).
        SetKey::Goal => {
            beads::sync_epic_title(ctx, &quest);
            false
        }
        _ => false,
    };
    let key = key_name(key);
    db.append_event(
        &quest.id,
        None,
        "quest.updated",
        &serde_json::json!({ "key": key, "value": stored }),
    )?;
    Ok(Applied {
        quest,
        key,
        value: stored,
        epic_relabelled: relabelled,
    })
}

/// `beads_epic new`: refused when the Quest already has an epic — linking is
/// `beads_epic <id>`, and two epics for one Quest is never what was meant. A
/// `bd` that will not create one is an error here, unlike at `q new`: the epic
/// is the whole of what was asked for.
fn new_epic(ctx: &Ctx, quest: &Quest) -> anyhow::Result<Applied> {
    if let Some(epic) = beads::epic_of(quest) {
        return Err(QError::Conflict(format!(
            "{} already has epic {epic}; link another with `beads_epic <id>`",
            quest.slug
        ))
        .into());
    }
    let quest = crate::commands::new::attach_epic(ctx, quest.clone(), quest.beads_repo.as_deref());
    let Some(epic) = beads::epic_of(&quest) else {
        let why = ctx
            .take_warnings()
            .into_iter()
            .next()
            .unwrap_or_else(|| "`bd create` failed".to_string());
        return Err(QError::Other(why).into());
    };
    let value = epic.to_string();
    ctx.db()?.append_event(
        &quest.id,
        None,
        "quest.updated",
        &serde_json::json!({ "key": "beads_epic", "value": value }),
    )?;
    Ok(Applied {
        quest,
        key: "beads_epic",
        value,
        epic_relabelled: false,
    })
}

/// `beads_repo` is not q's property: the label lives on the epic, and agents
/// are told to reuse it. Changing only q's copy would leave the two disagreeing
/// with nothing to say so, so the epic is relabelled in the same breath —
/// `bd update --remove-label … --add-label …`, one write. A `bd` that will not
/// cooperate is a warning with the command to run by hand, never a failed
/// `q set`: the column is already stored by then.
fn relabel_epic(ctx: &Ctx, quest: &Quest, old: Option<&str>, new: &str) -> bool {
    let Some(epic) = beads::epic_of(quest) else {
        return false;
    };
    let old = old.map(str::trim).filter(|o| !o.is_empty());
    if new.is_empty() {
        if let Some(old) = old {
            ctx.warn(format!(
                "note: quest {} no longer records a repo label, but epic {epic} still \
                 carries repo:{old}; remove it with `bd label remove {epic} repo:{old}`",
                quest.slug
            ));
        }
        return false;
    }
    // Re-setting the same value is how a label that drifted gets repaired, so
    // there is nothing to remove in that case — only the label to (re)add.
    let remove = old.filter(|o| *o != new);
    match ctx.bd().relabel_repo(epic, remove, new) {
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
            ctx.warn(format!(
                "warning: epic {epic} could not be relabelled ({e}); it still carries \
                 repo:{}; fix it with `bd update {epic}{undo} --add-label repo:{new}`",
                old.unwrap_or("-")
            ));
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

/// `on`/`off` (and the usual synonyms), or one of `CLEAR` to fall back to
/// `[context] auto_reset`.
fn parse_toggle(value: &str) -> anyhow::Result<Option<bool>> {
    let trimmed = value.trim().to_ascii_lowercase();
    if CLEAR.contains(&trimmed.as_str()) {
        return Ok(None);
    }
    match trimmed.as_str() {
        "on" | "true" | "yes" | "1" => Ok(Some(true)),
        "off" | "false" | "no" | "0" => Ok(Some(false)),
        _ => Err(QError::Other(format!(
            "invalid auto_reset `{value}`: expected on, off, or `default` to clear it"
        ))
        .into()),
    }
}

fn key_name(key: SetKey) -> &'static str {
    match key {
        SetKey::Goal => "goal",
        SetKey::Cwd => "cwd",
        SetKey::Workflow => "workflow",
        SetKey::AutoReset => "auto_reset",
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

    #[test]
    fn auto_reset_takes_a_toggle_or_clears() {
        for on in ["on", "ON", " true ", "yes", "1"] {
            assert_eq!(parse_toggle(on).unwrap(), Some(true), "`{on}`");
        }
        for off in ["off", "false", "no", "0"] {
            assert_eq!(parse_toggle(off).unwrap(), Some(false), "`{off}`");
        }
        for clear in ["default", "NONE", ""] {
            assert_eq!(parse_toggle(clear).unwrap(), None, "`{clear}`");
        }
        for bad in ["maybe", "2", "-1"] {
            assert!(parse_toggle(bad).is_err(), "accepted `{bad}`");
        }
    }
}
