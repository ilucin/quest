//! `q brief` — the Quest brief as markdown or `--json` (SPEC §9).

use crate::Ctx;
use crate::brief::{self, Opts};
use crate::commands::sweep_quiet;
use crate::model::SessionRole;
use crate::output;

pub struct Args<'a> {
    pub quest: Option<&'a str>,
    pub role: Option<SessionRole>,
    pub session: Option<&'a str>,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(&brief::default_target(args.quest)?)?;
    let opts = Opts {
        role: brief::default_role(args.role),
        session: brief::default_session(args.session),
        ..Opts::default()
    };
    let markdown = brief::render(db, &quest, &opts)?;
    let payload = serde_json::json!({
        "quest_id": quest.id,
        "for": opts.role,
        "markdown": markdown,
    });
    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &payload, || markdown.clone())?;
    }
    Ok(())
}
