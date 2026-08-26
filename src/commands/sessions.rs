//! `q sessions [<quest>]` — the fleet of agents, or one Quest's (SPEC §6, §16).

use serde::Serialize;

use crate::Ctx;
use crate::commands::{fmt, sweep_quiet};
use crate::model::{Quest, Session, SessionRole, SessionStatus};
use crate::output;

pub struct Args<'a> {
    pub quest: Option<&'a str>,
    pub all: bool,
}

/// Ended sessions a Quest listing keeps, newest first — enough to see what the
/// master just wound down without burying what is running.
const RECENT_ENDED: usize = 5;
const PROMPT_WIDTH: usize = 40;

/// A session with the Quest it belongs to, so a fleet-wide listing is
/// self-describing and `--json` needs no second lookup.
#[derive(Debug, Serialize)]
pub struct SessionView {
    #[serde(flatten)]
    pub session: Session,
    pub quest_slug: String,
    pub machine: String,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;

    let views = match args.quest {
        Some(target) => {
            let quest = db.resolve_quest(target)?;
            let sessions = db.list_sessions_by_quest(&quest.id)?;
            of_quest(&quest, sessions, args.all)
        }
        None => {
            let mut out = Vec::new();
            for quest in db.list_quests(args.all)? {
                if ctx.machine_filter().is_some_and(|m| m != quest.machine) {
                    continue;
                }
                let sessions = db.list_sessions_by_quest(&quest.id)?;
                let mut rows = of_quest(&quest, sessions, args.all);
                // Without `--all` the fleet view is about what is running now.
                if !args.all {
                    rows.retain(|v| v.session.status != SessionStatus::Ended);
                }
                out.extend(rows);
            }
            out
        }
    };

    if ctx.json || !ctx.quiet {
        let across_quests = args.quest.is_none();
        output::emit(ctx.json, &views, || human(&views, across_quests))?;
    }
    Ok(())
}

/// One Quest's rows: every live session, then the most recent ended ones
/// (all of them with `--all`).
fn of_quest(quest: &Quest, sessions: Vec<Session>, all: bool) -> Vec<SessionView> {
    let (mut live, mut ended): (Vec<Session>, Vec<Session>) = sessions
        .into_iter()
        .partition(|s| s.status != SessionStatus::Ended);
    // Window order: the master is window 0, workers follow in spawn order.
    // `started_at` is second-precision, so the id breaks a same-second tie.
    live.sort_by_key(|s| (s.role != SessionRole::Master, s.started_at, s.id.clone()));
    ended.sort_by(|a, b| b.ended_at.cmp(&a.ended_at).then(b.id.cmp(&a.id)));
    if !all {
        ended.truncate(RECENT_ENDED);
    }
    live.into_iter()
        .chain(ended)
        .map(|session| SessionView {
            session,
            quest_slug: quest.slug.clone(),
            machine: quest.machine.clone(),
        })
        .collect()
}

fn human(views: &[SessionView], across_quests: bool) -> String {
    if views.is_empty() {
        return "no sessions".to_string();
    }
    let mut header = vec!["LABEL", "ROLE", "STATUS", "PHASE", "CTX", "PANE", "AGE"];
    let mut rows: Vec<Vec<String>> = views
        .iter()
        .map(|v| {
            let s = &v.session;
            vec![
                s.label.clone(),
                s.role.to_string(),
                status_cell(s),
                fmt::or_dash(s.phase.as_deref()),
                s.ctx_pct
                    .map(|p| format!("{p}%"))
                    .unwrap_or("-".to_string()),
                s.tmux_pane.clone(),
                fmt::age(s.updated_at),
            ]
        })
        .collect();
    // The Quest is only worth a column when more than one can show up.
    if across_quests {
        header.insert(0, "QUEST");
        for (row, view) in rows.iter_mut().zip(views) {
            row.insert(0, view.quest_slug.clone());
        }
    }
    header.push("LAST PROMPT");
    for (row, view) in rows.iter_mut().zip(views) {
        row.push(fmt::oneline(
            view.session.last_prompt.as_deref().unwrap_or("-"),
            PROMPT_WIDTH,
        ));
    }
    fmt::table(&header, &rows)
}

/// `waiting` alone says nothing about what for; the hook records it, so show it.
fn status_cell(session: &Session) -> String {
    match (&session.status, session.waiting_for.as_deref()) {
        (SessionStatus::Waiting, Some(what)) => format!("waiting: {what}"),
        (status, _) => status.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quest() -> Quest {
        Quest::new("alpha", "/tmp", "laptop")
    }

    fn session(quest: &Quest, label: &str, status: SessionStatus, started_at: i64) -> Session {
        let mut s = Session::new(&quest.id, SessionRole::Worker, label, "q-alpha", "%1");
        s.status = status;
        s.started_at = started_at;
        if status == SessionStatus::Ended {
            s.ended_at = Some(started_at + 1);
        }
        s
    }

    fn labels(views: &[SessionView]) -> Vec<&str> {
        views.iter().map(|v| v.session.label.as_str()).collect()
    }

    #[test]
    fn the_master_leads_the_live_rows_then_workers_in_start_order() {
        let q = quest();
        let mut master = session(&q, "master", SessionStatus::Idle, 900);
        master.role = SessionRole::Master;
        let rows = vec![
            session(&q, "gone-old", SessionStatus::Ended, 10),
            session(&q, "w2", SessionStatus::Idle, 300),
            master,
            session(&q, "gone-new", SessionStatus::Ended, 50),
            session(&q, "w1", SessionStatus::Busy, 200),
        ];
        assert_eq!(
            labels(&of_quest(&q, rows, false)),
            ["master", "w1", "w2", "gone-new", "gone-old"]
        );
    }

    #[test]
    fn a_quest_listing_caps_the_ended_tail_unless_all_is_asked_for() {
        let q = quest();
        let rows: Vec<Session> = (0..RECENT_ENDED + 3)
            .map(|i| session(&q, &format!("w{i}"), SessionStatus::Ended, i as i64))
            .collect();
        assert_eq!(of_quest(&q, rows.clone(), false).len(), RECENT_ENDED);
        assert_eq!(of_quest(&q, rows, true).len(), RECENT_ENDED + 3);
    }

    #[test]
    fn the_quest_column_appears_only_in_a_fleet_listing() {
        let q = quest();
        let views = of_quest(&q, vec![session(&q, "w1", SessionStatus::Idle, 1)], false);
        assert!(human(&views, false).starts_with("LABEL"));
        let fleet = human(&views, true);
        assert!(fleet.starts_with("QUEST"), "{fleet}");
        assert!(fleet.contains("alpha"), "{fleet}");
        assert!(fleet.lines().next().unwrap().ends_with("LAST PROMPT"));
    }

    #[test]
    fn a_waiting_session_shows_what_it_waits_for() {
        let q = quest();
        let mut s = session(&q, "w1", SessionStatus::Waiting, 1);
        s.waiting_for = Some("permission_prompt".to_string());
        assert_eq!(status_cell(&s), "waiting: permission_prompt");
        s.waiting_for = None;
        assert_eq!(status_cell(&s), "waiting");
        s.status = SessionStatus::Busy;
        s.waiting_for = Some("ignored".to_string());
        assert_eq!(status_cell(&s), "busy");
    }

    #[test]
    fn an_empty_listing_says_so() {
        assert_eq!(human(&[], true), "no sessions");
    }
}
