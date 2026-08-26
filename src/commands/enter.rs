//! `q enter` — attach to a Quest's tmux session, master window first (SPEC §6).

use std::io::Write;

use crate::Ctx;
use crate::commands::new::MASTER;
use crate::commands::{attach_mode, live, sweep_quiet};
use crate::error::QError;
use crate::model::QuestState;
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, label: Option<&str>) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    if quest.state == QuestState::Finished {
        return Err(QError::Other(format!(
            "quest {} is finished; run `q resume {}`",
            quest.slug, quest.slug
        ))
        .into());
    }
    let tmux_session = session_name(&ctx.config, &quest.slug);
    if !ctx.tmux().has_session(&tmux_session)? {
        return Err(QError::Tmux(format!(
            "no tmux session `{tmux_session}`; run `q resume {}`",
            quest.slug
        ))
        .into());
    }

    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let window = match label {
        // The tmux session can outlive its master window; attaching would then
        // land on whatever window is left instead of the Quest's master.
        None => {
            if !live(&sessions).any(|s| s.label == MASTER) {
                return Err(QError::Other(format!(
                    "master session of {} ended; run `q resume {}`",
                    quest.slug, quest.slug
                ))
                .into());
            }
            MASTER.to_string()
        }
        Some(label) => {
            let known: Vec<&str> = live(&sessions).map(|s| s.label.as_str()).collect();
            if !known.contains(&label) {
                let live_labels = if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                };
                return Err(QError::NotFound(format!(
                    "session `{label}` in quest {} (live: {live_labels})",
                    quest.slug
                ))
                .into());
            }
            label.to_string()
        }
    };

    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "tmux_session": tmux_session,
                "window": window,
                "attach": attach_mode(ctx, true),
            }),
            || format!("attaching to {tmux_session}:{window}"),
        )?;
    }
    // A real attach replaces this process, so nothing buffered survives it.
    std::io::stdout().flush()?;
    ctx.tmux().attach(&tmux_session, Some(&window))
}
