//! `q list` — every Quest with its derived state (SPEC §4, §16), this
//! machine's and every remote's (SPEC §15).
//!
//! ## `--json`
//!
//! ```json
//! {
//!   "quests":   [ … one object per row, in the order the table shows them … ],
//!   "machines": [ … one object per machine this listing covers … ]
//! }
//! ```
//!
//! A **row** is the flattened `QuestView` every other command emits, plus a
//! `source` object saying where it came from:
//!
//! | | |
//! |---|---|
//! | `"source": {"kind": "local"}` | out of this machine's database |
//! | `"source": {"kind": "remote", "stale": false}` | off the wire this round |
//! | `"source": {"kind": "remote", "stale": true}` | the cache, standing in for a machine that did not answer |
//!
//! A remote row is re-emitted **verbatim**, exactly as the far end sent it, so
//! a field a newer `q` over there knows and this one does not is passed
//! through rather than dropped. The two keys this side does overwrite are
//! `machine` — always `remotes[].name`, never what the far end calls itself —
//! and `source`.
//!
//! A **machine** entry says whether that machine answered. It is the only
//! place an unreachable remote with an empty cache appears at all: it has no
//! rows to contribute, and a machine that is down must not read as a machine
//! with no Quests.
//!
//! ```json
//! {"name": "laptop", "kind": "local",  "status": "ok", "quests": 3}
//! {"name": "ws", "kind": "remote", "ssh": "ws", "status": "ok",
//!  "stale": false, "fetched_at": 1756300000, "quests": 2}
//! {"name": "ws", "kind": "remote", "ssh": "ws", "status": "unreachable",
//!  "reason": "no answer within 5s", "stale": true, "fetched_at": 1756299000,
//!  "quests": 2}
//! {"name": "box", "kind": "remote", "ssh": "box", "status": "incompatible",
//!  "reason": "cannot read `q list --json`: …", "stale": false,
//!  "fetched_at": null, "quests": 0}
//! ```
//!
//! `status` is `ok` | `unreachable` | `incompatible`, with `reason` present on
//! the last two. `--machine <name>` narrows both arrays to that machine, and
//! `--no-remote` leaves only the local entry.
//!
//! This envelope is also the wire format between machines: the fan-out asks a
//! remote for `q list --json --no-remote` and reads `quests` out of the answer
//! (a bare array is still accepted, which is what a `q` from before the
//! envelope sends — see [`crate::remote::parse`]).

use crate::Ctx;
use crate::cli::QuestState as StateFilter;
use crate::commands::flush_warnings;
use crate::commands::{QuestRow, fill_progress, fmt, load_quests, merge_remote};
use crate::model::DisplayState;
use crate::output;
use crate::remote::{self, RemoteResult};

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
    if !printing {
        flush_warnings(ctx);
        return Ok(());
    }

    // The remote fan-out (SPEC §15), asked for exactly the listing this one is
    // — `fetch_all` forwards `--all`/`--state`, so remote rows are filtered the
    // same way local ones were, rather than by a second rule here.
    let mut remotes = remote::fetch_all(ctx, all, state);
    // Said before the table, and said whether or not the machine had a cache
    // to fall back on: rows that are missing cannot report themselves.
    remote::warn_unreachable(ctx, &remotes);
    flush_warnings(ctx);

    fill_progress(ctx, &mut rows);
    let machines = machines_json(ctx, &remotes, rows.len());
    merge_remote(&mut rows, &mut remotes);

    let mut payload = serde_json::Map::new();
    payload.insert(
        remote::QUESTS.to_string(),
        serde_json::Value::Array(rows.iter().map(row_json).collect()),
    );
    payload.insert(
        remote::MACHINES.to_string(),
        serde_json::Value::Array(machines),
    );
    let payload = serde_json::Value::Object(payload);
    output::emit(ctx.json, &payload, || human(&rows))?;
    Ok(())
}

/// One row of the `quests` array — see this module's header. A remote's row is
/// the object it arrived in, never a re-serialization of the parsed view.
fn row_json(row: &QuestRow) -> serde_json::Value {
    let mut value = match &row.raw {
        Some(raw) => raw.clone(),
        None => serde_json::to_value(&row.view).unwrap_or(serde_json::Value::Null),
    };
    if let Some(object) = value.as_object_mut() {
        object.insert(
            remote::SOURCE.to_string(),
            serde_json::to_value(&row.origin).unwrap_or(serde_json::Value::Null),
        );
    }
    value
}

/// The `machines` array: this machine when the listing covers it, then every
/// remote it asked, in config order.
///
/// Counts come from before the merge, so `quests` is per machine rather than a
/// share of one ranked list.
fn machines_json(ctx: &Ctx, remotes: &[RemoteResult], local_rows: usize) -> Vec<serde_json::Value> {
    let mut out = Vec::with_capacity(remotes.len() + 1);
    let local = ctx.config.machine.name.as_str();
    if ctx.machine_filter().is_none_or(|m| m == local) {
        out.push(serde_json::json!({
            "name": local,
            "kind": "local",
            "status": "ok",
            "quests": local_rows,
        }));
    }
    for result in remotes {
        let mut entry = serde_json::json!({
            "name": result.name,
            "kind": "remote",
            "ssh": result.ssh,
            "stale": result.stale,
            "fetched_at": result.fetched_at,
            "quests": result.quests.len(),
        });
        // Flattened in: `{"status": "unreachable", "reason": …}` rather than
        // bd-8lz.5.1's nested `{"status": {"status": …}}`.
        if let (Some(entry), Some(status)) = (
            entry.as_object_mut(),
            serde_json::to_value(&result.status)
                .ok()
                .and_then(|s| s.as_object().cloned()),
        ) {
            entry.extend(status);
        }
        out.push(entry);
    }
    out
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
                r.machine_cell(),
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
    use crate::commands::{Origin, QuestView, sort_quests};
    use crate::model::{Quest, QuestState, Session, SessionRole, SessionStatus};
    use crate::remote::{RemoteQuest, RemoteStatus};

    /// One row as a remote sent it: the parsed view, and a verbatim object
    /// carrying a field this `q` has never heard of.
    fn remote_quest(machine: &str, slug: &str, state: QuestState, updated_at: i64) -> RemoteQuest {
        let mut quest = Quest::new(slug, "/tmp", machine);
        quest.state = state;
        quest.updated_at = updated_at;
        let view = QuestView::new(quest, &[]);
        let mut raw = serde_json::to_value(&view).unwrap();
        raw["something_from_the_future"] = serde_json::json!("hello");
        RemoteQuest { view, raw }
    }

    fn result(
        name: &str,
        status: RemoteStatus,
        stale: bool,
        quests: Vec<RemoteQuest>,
    ) -> RemoteResult {
        RemoteResult {
            name: name.to_string(),
            ssh: format!("{name}-host"),
            status,
            quests,
            stale,
            fetched_at: Some(1000),
        }
    }

    fn slugs(rows: &[QuestRow]) -> Vec<&str> {
        rows.iter().map(|r| r.view.quest.slug.as_str()).collect()
    }

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
        QuestRow::local(QuestView::new(quest, &sessions), sessions)
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

    /// SPEC §17's grouping is the listing's, not one section per machine: a
    /// Quest that needs you needs you wherever it runs.
    #[test]
    fn every_machine_lands_in_one_ranking() {
        use QuestState as Q;
        use SessionStatus as S;
        let mut rows = vec![
            row("local-idle", Q::Active, 50, &[S::Idle]),
            row("local-waiting", Q::Active, 10, &[S::Waiting]),
        ];
        let mut remotes = vec![
            result(
                "ws",
                RemoteStatus::Ok,
                false,
                vec![
                    remote_quest("ws", "ws-active", Q::Active, 90),
                    remote_quest("ws", "ws-idle", Q::Active, 20),
                ],
            ),
            result(
                "box",
                RemoteStatus::unreachable("down"),
                true,
                vec![remote_quest("box", "box-cached", Q::Active, 40)],
            ),
        ];
        // The remote rows carry no sessions, so they rank as idle; the local
        // waiting one leads whatever its age.
        merge_remote(&mut rows, &mut remotes);
        assert_eq!(
            slugs(&rows),
            [
                "local-waiting",
                "ws-active",
                "local-idle",
                "box-cached",
                "ws-idle"
            ]
        );
        // Drained, not copied: the rows are in the listing now.
        assert!(remotes.iter().all(|r| r.quests.is_empty()));
    }

    /// Ties keep the order the rows arrived in — local, then the remotes in
    /// config order — because the sort is stable.
    #[test]
    fn rows_that_tie_keep_local_first_then_config_order() {
        let mut rows = vec![row("local", QuestState::Active, 7, &[])];
        let mut remotes = vec![
            result(
                "ws",
                RemoteStatus::Ok,
                false,
                vec![remote_quest("ws", "from-ws", QuestState::Active, 7)],
            ),
            result(
                "box",
                RemoteStatus::Ok,
                false,
                vec![remote_quest("box", "from-box", QuestState::Active, 7)],
            ),
        ];
        merge_remote(&mut rows, &mut remotes);
        assert_eq!(slugs(&rows), ["local", "from-ws", "from-box"]);
    }

    #[test]
    fn the_machine_column_marks_a_row_that_came_out_of_the_cache() {
        let fresh = QuestRow::remote(remote_quest("ws", "a", QuestState::Active, 1), false);
        let stale = QuestRow::remote(remote_quest("ws", "b", QuestState::Active, 1), true);
        let local = row("c", QuestState::Active, 1, &[]);
        assert_eq!(fresh.machine_cell(), "ws");
        assert_eq!(stale.machine_cell(), "ws \u{26a0} stale");
        assert_eq!(local.machine_cell(), "laptop");

        let table = human(&[local, fresh, stale]);
        assert!(table.contains("MACHINE"), "{table}");
        assert!(table.contains("ws \u{26a0} stale"), "{table}");
    }

    /// The three row classes of the `--json` contract, and the promise that a
    /// remote row is re-emitted rather than re-serialized.
    #[test]
    fn a_json_row_says_where_it_came_from() {
        let local = row("c", QuestState::Active, 1, &[]);
        assert_eq!(
            row_json(&local)[crate::remote::SOURCE],
            serde_json::json!({ "kind": "local" })
        );
        assert_eq!(row_json(&local)["slug"], "c");

        let fresh = QuestRow::remote(remote_quest("ws", "a", QuestState::Active, 1), false);
        let value = row_json(&fresh);
        assert_eq!(
            value[crate::remote::SOURCE],
            serde_json::json!({ "kind": "remote", "stale": false })
        );
        assert_eq!(value["machine"], "ws");
        // Verbatim: a field a newer `q` at the far end sent is still here.
        assert_eq!(value["something_from_the_future"], "hello");

        let stale = QuestRow::remote(remote_quest("ws", "b", QuestState::Active, 1), true);
        assert_eq!(
            row_json(&stale)[crate::remote::SOURCE],
            serde_json::json!({ "kind": "remote", "stale": true })
        );
    }

    fn ctx(machine: Option<&str>) -> Ctx {
        let mut config = crate::config::Config::default();
        config.machine.name = "laptop".to_string();
        let db = crate::db::Db::open_in_memory().unwrap();
        let tmux = Box::new(crate::tmux::FixtureTmux::new(std::path::PathBuf::from(
            "/nonexistent/tmux.json",
        )));
        Ctx::for_tests(config, db, tmux).with_machine(machine)
    }

    /// The `machines` array is the only place a machine that is down but has
    /// nothing cached appears at all — it has no rows to contribute, and
    /// "no answer" must not read as "no Quests".
    #[test]
    fn the_machines_array_reports_every_machine_the_listing_covers() {
        let remotes = vec![
            result(
                "ws",
                RemoteStatus::Ok,
                false,
                vec![remote_quest("ws", "a", QuestState::Active, 1)],
            ),
            result(
                "box",
                RemoteStatus::unreachable("host is down"),
                false,
                vec![],
            ),
            result(
                "old",
                RemoteStatus::incompatible("cannot read"),
                false,
                vec![],
            ),
        ];
        let entries = machines_json(&ctx(None), &remotes, 2);
        assert_eq!(
            entries[0],
            serde_json::json!({
                "name": "laptop", "kind": "local", "status": "ok", "quests": 2
            })
        );
        assert_eq!(
            entries[1],
            serde_json::json!({
                "name": "ws", "kind": "remote", "ssh": "ws-host", "status": "ok",
                "stale": false, "fetched_at": 1000, "quests": 1
            })
        );
        // Flattened, not nested: `{"status": …, "reason": …}`.
        assert_eq!(entries[2]["status"], "unreachable");
        assert_eq!(entries[2]["reason"], "host is down");
        assert_eq!(entries[2]["quests"], 0);
        assert_eq!(entries[3]["status"], "incompatible");
    }

    /// `--machine` narrows the roster as well as the rows: a listing pinned to
    /// one machine must not report on the others.
    #[test]
    fn a_machine_filter_narrows_the_machines_array_too() {
        let remotes = vec![result("ws", RemoteStatus::Ok, false, vec![])];
        let entries = machines_json(&ctx(Some("ws")), &remotes, 0);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "ws");

        // Pinned to this machine, the fan-out found nothing to ask.
        let entries = machines_json(&ctx(Some("laptop")), &[], 3);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["kind"], "local");
    }

    #[test]
    fn origin_answers_the_two_questions_a_row_cannot() {
        assert!(!Origin::Local.is_remote());
        assert!(!Origin::Local.is_stale());
        assert!(Origin::Remote { stale: false }.is_remote());
        assert!(!Origin::Remote { stale: false }.is_stale());
        assert!(Origin::Remote { stale: true }.is_stale());
    }
}
