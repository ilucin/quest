//! `q prompt <session>` — print a session's stored first prompt (SPEC §6).
//!
//! Used two ways: by a human, to see what a session was (or will be) launched
//! with; and by `q start`'s injected command, which embeds a large or
//! multi-line prompt as `"$(q prompt <id>)"` so a stray newline never submits
//! it early. So this stays read-only and does **not** sweep: it runs inside a
//! pane that is mid-launch, where a sweep could demote a sibling to `off`.

use crate::Ctx;
use crate::commands::target;
use crate::output;

pub fn run(ctx: &Ctx, session_target: &str) -> anyhow::Result<()> {
    let found = target::resolve(ctx, session_target)?;
    let prompt = found.session.first_prompt.clone().unwrap_or_default();
    // The human path prints the text verbatim (bar control-char sanitising), so
    // the `$(q prompt …)` substitution hands Claude exactly what was stored.
    output::emit(
        ctx.json,
        &serde_json::json!({
            "session": found.session.id,
            "quest": found.quest.slug,
            "label": found.session.label,
            "prompt": prompt,
        }),
        || prompt.clone(),
    )
}
