//! `q show` — one Quest with its sessions and recent events (SPEC §16).

use serde::Serialize;

use crate::Ctx;
use crate::commands::{QuestView, fmt, sweep_quiet};
use crate::model::{Event, Session};
use crate::output;
use crate::tmux::session_name;

/// `q list`'s object plus what only `q show` reads, so both renderings agree
/// on the shape of a Quest (SPEC §16).
#[derive(Debug, Serialize)]
struct ShowView {
    #[serde(flatten)]
    view: QuestView,
    sessions: Vec<Session>,
    events: Vec<Event>,
}

const EVENTS: usize = 10;
const PROMPT_WIDTH: usize = 40;
const PAYLOAD_WIDTH: usize = 60;

pub fn run(ctx: &Ctx, target: &str) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let quest = db.resolve_quest(target)?;
    let sessions = db.list_sessions_by_quest(&quest.id)?;
    let events = db.list_events_by_quest(&quest.id, EVENTS)?;
    let tmux_session = session_name(&ctx.config, &quest.slug);
    let payload = ShowView {
        view: QuestView::new(quest, &sessions),
        sessions,
        events,
    };

    if ctx.json || !ctx.quiet {
        // TODO(M1/M2): links and beads progress belong in both renderings.
        output::emit(ctx.json, &payload, || {
            human(
                &payload.view,
                &tmux_session,
                &payload.sessions,
                &payload.events,
            )
        })?;
    }
    Ok(())
}

fn human(view: &QuestView, tmux_session: &str, sessions: &[Session], events: &[Event]) -> String {
    let quest = &view.quest;
    let mut out = format!("{} {}  {}", quest.id, quest.slug, view.state_cell());
    for (label, value) in [
        ("goal", fmt::or_dash(quest.goal.as_deref())),
        ("cwd", fmt::tilde(&quest.cwd)),
        ("machine", quest.machine.clone()),
        ("workflow", fmt::or_dash(quest.workflow.as_deref())),
        ("tmux", tmux_session.to_string()),
        (
            "created",
            format!(
                "{} ({} ago)",
                fmt::stamp(quest.created_at),
                fmt::age(quest.created_at)
            ),
        ),
        (
            "updated",
            format!(
                "{} ({} ago)",
                fmt::stamp(quest.updated_at),
                fmt::age(quest.updated_at)
            ),
        ),
    ] {
        out.push_str(&format!("\n  {label:<9}{value}"));
    }
    if let Some(finished_at) = quest.finished_at {
        out.push_str(&format!("\n  {:<9}{}", "finished", fmt::stamp(finished_at)));
    }

    out.push_str("\n\nsessions:\n");
    if sessions.is_empty() {
        out.push_str("  none");
    } else {
        let rows: Vec<Vec<String>> = sessions
            .iter()
            .map(|s| {
                vec![
                    s.label.clone(),
                    s.role.to_string(),
                    s.status.to_string(),
                    fmt::or_dash(s.phase.as_deref()),
                    s.ctx_pct
                        .map(|p| format!("{p}%"))
                        .unwrap_or("-".to_string()),
                    fmt::oneline(s.last_prompt.as_deref().unwrap_or("-"), PROMPT_WIDTH),
                ]
            })
            .collect();
        out.push_str(&indent(&fmt::table(
            &["LABEL", "ROLE", "STATUS", "PHASE", "CTX", "LAST PROMPT"],
            &rows,
        )));
    }

    out.push_str("\n\nevents:\n");
    if events.is_empty() {
        out.push_str("  none");
    } else {
        let rows: Vec<Vec<String>> = events
            .iter()
            .map(|e| {
                vec![
                    fmt::stamp_utc(e.ts),
                    e.kind.clone(),
                    fmt::payload(e.payload.as_ref(), PAYLOAD_WIDTH),
                ]
            })
            .collect();
        out.push_str(&indent(&fmt::table(&["WHEN", "KIND", "PAYLOAD"], &rows)));
    }
    out
}

fn indent(block: &str) -> String {
    block
        .lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}
