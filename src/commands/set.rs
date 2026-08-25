//! `q set <quest> <key> <value>` — the Quest properties the CLI can change
//! (SPEC §16).

use crate::Ctx;
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

    let mut patch = QuestPatch::default();
    let mut stored = value.to_string();
    match key {
        SetKey::Goal => patch.goal = Some(value.to_string()),
        SetKey::Cwd => {
            let dir = resolve_dir(Some(value))?;
            stored = dir.to_string_lossy().into_owned();
            patch.cwd = Some(stored.clone());
        }
        // TODO(M5): validate against the workflow registry.
        SetKey::Workflow => patch.workflow = Some(value.to_string()),
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
            &serde_json::json!({ "quest": quest, "key": key, "value": stored }),
            || format!("{} ({}) · {key} = {stored}", quest.id, quest.slug),
        )?;
    }
    Ok(())
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
