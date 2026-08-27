//! `q resume` — a finished (or session-less) Quest gets a fresh master
//! (SPEC §5). The old session rows stay behind as history.

use std::io::Write;

use crate::Ctx;
use crate::commands::new::spawn_master;
use crate::commands::{AttachMode, attach_mode, live, sweep_quiet};
use crate::error::QError;
use crate::model::{Quest, QuestState, Session};
use crate::output;
use crate::tmux::session_name;

/// The fresh master a resume brought up.
pub struct Resumed {
    pub quest: Quest,
    pub session: Session,
    pub tmux_session: String,
}

impl Resumed {
    /// The one-line human rendering, shared by `q resume` and the TUI's prompt.
    pub fn describe(&self) -> String {
        format!(
            "resumed {} ({}) · tmux {} · run: q enter {}",
            self.quest.id, self.quest.slug, self.tmux_session, self.quest.slug
        )
    }
}

pub fn run(ctx: &Ctx, target: &str, prompt: Option<&str>, detach: bool) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let quest = ctx.db()?.resolve_quest(target)?;
    let out = apply(ctx, &quest, prompt)?;

    let attach = attach_mode(ctx, !detach);
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "quest": out.quest,
                "session": out.session,
                "tmux_session": out.tmux_session,
                "attach": attach,
            }),
            || out.describe(),
        )?;
    }
    if attach != AttachMode::None {
        // An exec attach replaces this process, so nothing buffered survives it.
        std::io::stdout().flush()?;
        ctx.tmux()
            .attach(&out.tmux_session, Some(&out.session.tmux_pane))?;
    }
    Ok(())
}

/// The resume itself: refuse when the Quest still has something live, then a
/// fresh master and `state = active` (SPEC §5). Shared with the TUI's `R`
/// prompt, which does everything but the attach.
pub fn apply(ctx: &Ctx, quest: &Quest, prompt: Option<&str>) -> anyhow::Result<Resumed> {
    let db = ctx.db()?;
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
    let master = spawn_master(ctx, quest, prompt.map(str::to_string))?;
    let quest = db.update_quest_state(&quest.id, QuestState::Active, None)?;
    db.append_event(
        &quest.id,
        Some(&master.session.id),
        "quest.resumed",
        &serde_json::json!({ "session": master.session.id, "prompt": prompt }),
    )?;

    Ok(Resumed {
        quest,
        session: master.session,
        tmux_session: master.tmux_session,
    })
}
