//! `q resume` — a finished (or session-less) Quest gets a fresh master
//! (SPEC §5). The old session rows stay behind as history.

use std::io::Write;

use crate::Ctx;
use crate::commands::new::spawn_master;
use crate::commands::{NONE, attach_mode, live, sweep_quiet};
use crate::error::QError;
use crate::model::QuestState;
use crate::output;
use crate::tmux::session_name;

pub fn run(ctx: &Ctx, target: &str, prompt: Option<&str>, detach: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);

    let finished = quest.state == QuestState::Finished;
    if ctx.tmux().has_session(&tmux_session)? {
        // A finished Quest has no sessions left to enter, so pointing at
        // `q enter` would only bounce the caller back here.
        return Err(if finished {
            QError::Tmux(format!(
                "tmux session `{tmux_session}` still exists; kill it first \
                 (tmux kill-session -t ={tmux_session}) or `q rm -f`"
            ))
        } else {
            QError::Tmux(format!(
                "tmux session `{tmux_session}` is still running; run `q enter {}`",
                quest.slug
            ))
        }
        .into());
    }
    // An active Quest whose master is gone is as resumable as a finished one.
    if !finished {
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        if live(&sessions).next().is_some() {
            return Err(QError::Other(format!(
                "quest {} is still active; run `q enter {}`",
                quest.slug, quest.slug
            ))
            .into());
        }
    }

    // TODO(M2): without `--prompt` the master should come up on its brief.
    let master = spawn_master(ctx, &quest, prompt.map(str::to_string))?;
    let quest = db.update_quest_state(&quest.id, QuestState::Active, None)?;
    db.append_event(
        &quest.id,
        Some(&master.session.id),
        "quest.resumed",
        &serde_json::json!({ "session": master.session.id, "prompt": prompt }),
    )?;

    let attach = attach_mode(ctx, !detach);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": quest,
                "session": master.session,
                "tmux_session": master.tmux_session,
                "attach": attach,
            }),
            || {
                format!(
                    "resumed {} ({}) · tmux {} · run: q enter {}",
                    quest.id, quest.slug, master.tmux_session, quest.slug
                )
            },
        )?;
    }
    if attach != NONE {
        // An exec attach replaces this process, so nothing buffered survives it.
        std::io::stdout().flush()?;
        ctx.tmux()
            .attach(&master.tmux_session, Some(&master.session.tmux_pane))?;
    }
    Ok(())
}
