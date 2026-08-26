//! `q peek <session> [--lines N]` — what a session's pane shows right now
//! (SPEC §6: `tmux capture-pane -p`).

use crate::Ctx;
use crate::commands::{sweep_quiet, target};
use crate::error::QError;
use crate::output;

/// A screenful and a bit: enough to see the last exchange, short enough to
/// read in a terminal.
pub const DEFAULT_LINES: usize = 40;

pub fn run(ctx: &Ctx, session_target: &str, lines: usize) -> anyhow::Result<()> {
    if lines == 0 {
        return Err(QError::Invalid("--lines must be at least 1".to_string()).into());
    }
    sweep_quiet(ctx)?;
    let found = target::resolve(ctx, session_target)?;
    // A pane that is gone has no output to show, and the sweep just proved it.
    found.require_live()?;

    let text = ctx.tmux().capture_pane(&found.session.tmux_pane, lines)?;
    if ctx.json || !ctx.quiet {
        output::emit(
            ctx.json,
            &serde_json::json!({
                "session": found.session.id,
                "quest": found.quest.slug,
                "label": found.session.label,
                "pane": found.session.tmux_pane,
                "lines": lines,
                "text": text,
            }),
            // Raw, so `q peek | grep` behaves like the capture it is.
            || text.clone(),
        )?;
    }
    Ok(())
}
