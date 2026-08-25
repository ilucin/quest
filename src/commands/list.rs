//! `q list` — every Quest with its derived state (SPEC §4, §16).

use crate::Ctx;
use crate::cli::QuestState as StateFilter;
use crate::commands::{QuestView, fmt, sweep_quiet};
use crate::model::DisplayState;
use crate::output;

pub fn run(ctx: &Ctx, all: bool, state: Option<StateFilter>) -> anyhow::Result<()> {
    sweep_quiet(ctx)?;
    let db = ctx.db()?;
    let wanted = state.map(display_state_of);
    let include_finished = all || wanted == Some(DisplayState::Finished);

    let mut views: Vec<QuestView> = Vec::new();
    for quest in db.list_quests(include_finished)? {
        // TODO(M4): a remote machine's Quests come over ssh, not out of this db.
        if ctx.machine_filter().is_some_and(|m| m != quest.machine) {
            continue;
        }
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        let view = QuestView::new(quest, &sessions);
        if wanted.is_some_and(|w| w != view.display_state) {
            continue;
        }
        views.push(view);
    }
    sort(&mut views);

    if ctx.json || !ctx.quiet {
        output::emit(ctx.json, &views, || human(&views))?;
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

/// What needs the human first, then what is running, then the rest; ties go to
/// the most recently touched Quest.
fn sort(views: &mut [QuestView]) {
    views.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then(b.quest.updated_at.cmp(&a.quest.updated_at))
    });
}

fn rank(view: &QuestView) -> u8 {
    if view.needs_you {
        return 0;
    }
    match view.display_state {
        DisplayState::Active => 1,
        DisplayState::Idle => 2,
        DisplayState::Finished => 3,
    }
}

fn human(views: &[QuestView]) -> String {
    if views.is_empty() {
        return "no quests".to_string();
    }
    let rows: Vec<Vec<String>> = views
        .iter()
        .map(|v| {
            vec![
                v.quest.id.clone(),
                v.quest.slug.clone(),
                v.state_cell(),
                v.quest.machine.clone(),
                v.live_sessions.to_string(),
                fmt::tilde(&v.quest.cwd),
                fmt::age(v.quest.updated_at),
            ]
        })
        .collect();
    fmt::table(
        &["ID", "SLUG", "STATE", "MACHINE", "SESS", "CWD", "AGE"],
        &rows,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Quest, QuestState, Session, SessionRole, SessionStatus};

    fn view(
        slug: &str,
        state: QuestState,
        updated_at: i64,
        statuses: &[SessionStatus],
    ) -> QuestView {
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
        QuestView::new(quest, &sessions)
    }

    #[test]
    fn sorting_puts_needs_you_first_then_state_then_recency() {
        use QuestState as Q;
        use SessionStatus as S;
        let mut views = vec![
            view("finished", Q::Finished, 90, &[]),
            view("idle-old", Q::Active, 10, &[S::Idle]),
            view("active", Q::Active, 20, &[S::Busy]),
            view("idle-new", Q::Active, 30, &[S::Idle]),
            view("waiting", Q::Active, 1, &[S::Waiting]),
        ];
        sort(&mut views);
        let order: Vec<&str> = views.iter().map(|v| v.quest.slug.as_str()).collect();
        assert_eq!(
            order,
            ["waiting", "active", "idle-new", "idle-old", "finished"]
        );
    }

    #[test]
    fn a_waiting_session_is_marked_in_the_state_cell() {
        let v = view("x", QuestState::Active, 0, &[SessionStatus::Waiting]);
        assert_eq!(v.state_cell(), "active · needs you");
        assert_eq!(v.live_sessions, 1);
        let v = view("x", QuestState::Active, 0, &[SessionStatus::Ended]);
        assert_eq!(v.state_cell(), "idle");
        assert_eq!(v.live_sessions, 0);
    }

    #[test]
    fn an_empty_listing_says_so() {
        assert_eq!(human(&[]), "no quests");
    }
}
