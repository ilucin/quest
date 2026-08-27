//! `q list` — every Quest with its derived state (SPEC §4, §16).

use crate::Ctx;
use crate::cli::QuestState as StateFilter;
use crate::commands::flush_warnings;
use crate::commands::{QuestRow, fill_progress, fmt, load_quests};
use crate::model::DisplayState;
use crate::output;
use crate::remote;

pub fn run(ctx: &Ctx, all: bool, state: Option<StateFilter>) -> anyhow::Result<()> {
    let wanted = state.map(display_state_of);
    let include_finished = all || wanted == Some(DisplayState::Finished);

    let mut rows = load_quests(ctx, include_finished)?;
    if let Some(want) = wanted {
        rows.retain(|r| r.view.display_state == want);
    }

    // Nothing below this line runs when nothing is going to be printed: not the
    // one `bd` call, and not a fan-out that can cost the full remote deadline.
    let printing = ctx.json || !ctx.quiet;
    if printing {
        // The remote fan-out (SPEC §15), asked for the same listing this one
        // is. bd-8lz.5.2 merges these rows into the listing and the TUI; until
        // then the round still runs, so a machine that is down is reported
        // rather than silently missing.
        remote::warn_unreachable(ctx, &remote::fetch_all(ctx, all, state));
    }
    flush_warnings(ctx);

    if printing {
        fill_progress(ctx, &mut rows);
        let views: Vec<&crate::commands::QuestView> = rows.iter().map(|r| &r.view).collect();
        output::emit(ctx.json, &views, || human(&rows))?;
    }
    Ok(())
}

fn display_state_of(state: StateFilter) -> DisplayState {
    match state {
        StateFilter::Active => DisplayState::Active,
        StateFilter::Idle => DisplayState::Idle,
        StateFilter::Finished => DisplayState::Finished,
    }
}

fn human(rows: &[QuestRow]) -> String {
    if rows.is_empty() {
        return "no quests".to_string();
    }
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|r| {
            let v = &r.view;
            vec![
                v.quest.id.clone(),
                v.quest.slug.clone(),
                v.state_cell(),
                v.quest.machine.clone(),
                v.live_sessions.to_string(),
                v.progress_cell(),
                fmt::tilde(&v.quest.cwd),
                fmt::age(v.quest.updated_at),
            ]
        })
        .collect();
    fmt::table(
        &[
            "ID", "SLUG", "STATE", "MACHINE", "SESS", "BEADS", "CWD", "AGE",
        ],
        &cells,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{QuestView, sort_quests};
    use crate::model::{Quest, QuestState, Session, SessionRole, SessionStatus};

    fn row(slug: &str, state: QuestState, updated_at: i64, statuses: &[SessionStatus]) -> QuestRow {
        let mut quest = Quest::new(slug, "/tmp", "laptop");
        quest.state = state;
        quest.updated_at = updated_at;
        let sessions: Vec<Session> = statuses
            .iter()
            .map(|status| {
                let mut s = Session::new(&quest.id, SessionRole::Worker, "w1", "q-x", "%1");
                s.status = *status;
                s
            })
            .collect();
        QuestRow {
            view: QuestView::new(quest, &sessions),
            sessions,
        }
    }

    #[test]
    fn sorting_puts_needs_you_first_then_state_then_recency() {
        use QuestState as Q;
        use SessionStatus as S;
        let mut rows = vec![
            row("finished", Q::Finished, 90, &[]),
            row("idle-old", Q::Active, 10, &[S::Idle]),
            row("active", Q::Active, 20, &[S::Busy]),
            row("idle-new", Q::Active, 30, &[S::Idle]),
            row("waiting", Q::Active, 1, &[S::Waiting]),
        ];
        sort_quests(&mut rows);
        let order: Vec<&str> = rows.iter().map(|r| r.view.quest.slug.as_str()).collect();
        assert_eq!(
            order,
            ["waiting", "active", "idle-new", "idle-old", "finished"]
        );
    }

    #[test]
    fn a_waiting_session_is_marked_in_the_state_cell() {
        let r = row("x", QuestState::Active, 0, &[SessionStatus::Waiting]);
        assert_eq!(r.view.state_cell(), "active · needs you");
        assert_eq!(r.view.live_sessions, 1);
        let r = row("x", QuestState::Active, 0, &[SessionStatus::Ended]);
        assert_eq!(r.view.state_cell(), "idle");
        assert_eq!(r.view.live_sessions, 0);
    }

    #[test]
    fn an_empty_listing_says_so() {
        assert_eq!(human(&[]), "no quests");
    }
}
