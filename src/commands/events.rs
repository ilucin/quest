//! `q events` — a Quest's event log, filtered, optionally tailed (SPEC §16).
//!
//! The loading half is shared with the TUI's Events tab (SPEC §17): [`load`]
//! is the one definition of "the event feed", so the live tail and the command
//! line can never disagree about which rows exist or what they are called.
//! The tab spans every Quest and the command spans one, which is the only
//! difference — and it is a parameter.

use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::time::Duration;

use crate::Ctx;
use crate::brief;
use crate::commands::{fmt, sweep_quiet};
use crate::db::Db;
use crate::db::event::{EventFilter, KindPattern};
use crate::error::QError;
use crate::model::{Event, Quest};
use crate::output;

pub struct Args<'a> {
    pub quest: Option<&'a str>,
    pub kinds: &'a [String],
    pub session: Option<&'a str>,
    pub limit: usize,
    pub follow: bool,
}

const POLL: Duration = Duration::from_millis(500);
/// Rows one poll may return; anything beyond is picked up by the next one.
const POLL_LIMIT: usize = 1_000;
const KIND_WIDTH: usize = 16;
const PAYLOAD_WIDTH: usize = 100;

/// One `--kind` argument each, parsed. Separate from [`EventFilter`] so the
/// TUI's filter box reports a bad pattern the same way the CLI does.
pub fn kinds_of(patterns: &[String]) -> anyhow::Result<Vec<KindPattern>> {
    patterns.iter().map(|k| KindPattern::parse(k)).collect()
}

/// The Quest slug and session label every event row wants to print, looked up
/// once rather than per row.
///
/// Refreshable on purpose: a session that starts after a tail does is
/// otherwise printed by id for the rest of the run.
#[derive(Debug, Default, Clone)]
pub struct Names {
    quests: HashMap<String, String>,
    sessions: HashMap<String, String>,
}

impl Names {
    pub fn of(db: &Db, quests: &[Quest]) -> anyhow::Result<Names> {
        let mut names = Names::default();
        for quest in quests {
            names.quests.insert(quest.id.clone(), quest.slug.clone());
            names.learn_sessions(db, &quest.id)?;
        }
        Ok(names)
    }

    pub fn learn_sessions(&mut self, db: &Db, quest_id: &str) -> anyhow::Result<()> {
        for session in db.list_sessions_by_quest(quest_id)? {
            self.sessions.insert(session.id, session.label);
        }
        Ok(())
    }

    pub fn knows_session(&self, id: &str) -> bool {
        self.sessions.contains_key(id)
    }

    /// The slug, falling back to the id: a row whose Quest is not in the set
    /// still names something that can be looked up.
    fn quest<'a>(&'a self, id: &'a str) -> &'a str {
        self.quests.get(id).map_or(id, String::as_str)
    }

    /// The label, the session id when it is unknown, `-` when the event
    /// belongs to no session.
    fn session<'a>(&'a self, event: &'a Event) -> &'a str {
        match event.session_id.as_deref() {
            Some(id) => self.sessions.get(id).map_or(id, String::as_str),
            None => "-",
        }
    }

    pub fn row(&self, event: Event) -> EventRow {
        EventRow {
            quest_slug: self.quest(&event.quest_id).to_string(),
            session: self.session(&event).to_string(),
            event,
        }
    }

    pub fn rows(&self, events: Vec<Event>) -> Vec<EventRow> {
        events.into_iter().map(|e| self.row(e)).collect()
    }
}

/// An event with the Quest and session it belongs to already named, so a
/// fleet-wide listing is self-describing.
#[derive(Debug, Clone)]
pub struct EventRow {
    pub event: Event,
    pub quest_slug: String,
    pub session: String,
}

/// The one definition of "the event feed": the last `limit` events of
/// `quests` matching `filter`, oldest first, each named.
///
/// `q events` passes one Quest and the TUI's Events tab passes every Quest it
/// is not filtered to, so the two run the same query with the same filter.
pub fn load(
    db: &Db,
    quests: &[Quest],
    filter: &EventFilter,
    limit: usize,
) -> anyhow::Result<Vec<EventRow>> {
    let names = Names::of(db, quests)?;
    let mut events: Vec<Event> = Vec::new();
    for quest in quests {
        events.extend(db.list_events_latest(&quest.id, filter, limit)?);
    }
    if quests.len() > 1 {
        // One append-only table and one autoincrementing id, so the ids are
        // the order things happened in across Quests as well as within one.
        events.sort_by_key(|e| e.id);
        // Each Quest gave up its own last `limit`, and the union's tail is a
        // superset of the global one: cutting it here is exact, not a guess.
        if events.len() > limit {
            events.drain(..events.len() - limit);
        }
    }
    Ok(names.rows(events))
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    // The sweep only tidies session state; tmux being down must not hide the log.
    let _ = sweep_quiet(ctx);
    let db = ctx.db()?;
    let quest = db.resolve_quest(&brief::default_target(args.quest)?)?;

    let mut filter = EventFilter {
        kinds: kinds_of(args.kinds)?,
        session_id: None,
    };
    if let Some(target) = args.session {
        let sessions = db.list_sessions_by_quest(&quest.id)?;
        let session = brief::resolve_session(&quest, &sessions, target)
            .ok_or_else(|| QError::NotFound(format!("session `{target}` not in {}", quest.slug)))?;
        filter.session_id = Some(session.id.clone());
    }

    let quests = std::slice::from_ref(&quest);
    let mut names = Names::of(db, quests)?;
    let rows = load(db, quests, &filter, args.limit)?;
    // An empty first page (`-n 0`, or a filter nothing matches yet) must not
    // make the tail replay the whole history: start from the newest row.
    let mut last_id = match rows.last() {
        Some(row) => row.event.id,
        None => db.last_event_id(&quest.id)?,
    };

    if !args.follow {
        if ctx.json || !ctx.quiet {
            let events: Vec<&Event> = rows.iter().map(|r| &r.event).collect();
            output::emit(ctx.json, &events, || human_page(&rows))?;
        }
        return Ok(());
    }

    // Follow: one line per event from the start, so a consumer sees a single
    // stream rather than an array followed by objects.
    let mut out = std::io::stdout().lock();
    if !write_lines(&mut out, ctx, &rows)? {
        return Ok(());
    }
    let mut polls_left = fixture_iterations();
    loop {
        if let Some(n) = polls_left.as_mut() {
            if *n == 0 {
                return Ok(());
            }
            *n -= 1;
        }
        std::thread::sleep(POLL);
        let fresh = db.list_events_after(&quest.id, last_id, &filter, POLL_LIMIT)?;
        if fresh.is_empty() {
            continue;
        }
        if fresh.iter().any(|e| {
            e.session_id
                .as_ref()
                .is_some_and(|id| !names.knows_session(id))
        }) {
            // A session that started after we did.
            names.learn_sessions(db, &quest.id)?;
        }
        last_id = fresh.last().map_or(last_id, |e| e.id);
        let fresh = names.rows(fresh);
        if !write_lines(&mut out, ctx, &fresh)? {
            return Ok(());
        }
    }
}

/// `$Q_FOLLOW_ITERATIONS` bounds the poll loop, but only under `Q_FIXTURE`
/// (tests); a real `--follow` runs until the process is stopped.
fn fixture_iterations() -> Option<u32> {
    std::env::var_os("Q_FIXTURE").filter(|v| !v.is_empty())?;
    std::env::var("Q_FOLLOW_ITERATIONS")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Writes `rows` one per line; `Ok(false)` once the reader has gone away
/// (`q events --follow | head`), which ends the tail without an error.
fn write_lines(out: &mut impl Write, ctx: &Ctx, rows: &[EventRow]) -> anyhow::Result<bool> {
    for row in rows {
        let line = if ctx.json {
            serde_json::to_string(&row.event)?
        } else if ctx.quiet {
            continue;
        } else {
            human_line(row)
        };
        match writeln!(out, "{line}").and_then(|()| out.flush()) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

fn human_page(rows: &[EventRow]) -> String {
    if rows.is_empty() {
        return "no events".to_string();
    }
    rows.iter().map(human_line).collect::<Vec<_>>().join("\n")
}

/// `<UTC stamp>  <kind>  [<session label or id>]  <payload>`.
fn human_line(row: &EventRow) -> String {
    format!(
        "{}  {:<KIND_WIDTH$}  [{}]  {}",
        fmt::stamp_utc(row.event.ts),
        row.event.kind,
        row.session,
        fmt::payload(row.event.payload.as_ref(), PAYLOAD_WIDTH)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::model::{Quest, Session, SessionRole};

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn quest(db: &Db, slug: &str) -> Quest {
        db.insert_quest(&Quest::new(slug, "/tmp/repo", "laptop"))
            .unwrap()
    }

    fn names_of(db: &Db, quests: &[Quest]) -> Names {
        Names::of(db, quests).unwrap()
    }

    #[test]
    fn a_line_uses_the_label_when_known_and_the_id_otherwise() {
        let db = db();
        let q = quest(&db, "alpha");
        let s = db
            .insert_session(&Session::new(
                &q.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                "%1",
            ))
            .unwrap();
        let names = names_of(&db, std::slice::from_ref(&q));

        let event = Event {
            id: 1,
            quest_id: q.id.clone(),
            session_id: Some(s.id.clone()),
            ts: 0,
            kind: "note".to_string(),
            payload: Some(serde_json::json!({ "text": "hi" })),
        };
        assert_eq!(
            human_line(&names.row(event.clone())),
            "1970-01-01 00:00:00  note              [master]  text=hi"
        );
        let unknown = Event {
            session_id: Some("s-9".to_string()),
            payload: None,
            ..event.clone()
        };
        assert_eq!(
            human_line(&names.row(unknown)),
            "1970-01-01 00:00:00  note              [s-9]  -"
        );
        let none = Event {
            session_id: None,
            ..event
        };
        assert!(human_line(&names.row(none)).contains("  [-]  "));
    }

    #[test]
    fn a_row_names_the_quest_it_belongs_to() {
        let db = db();
        let a = quest(&db, "alpha");
        let names = names_of(&db, std::slice::from_ref(&a));
        let event = db
            .append_event(&a.id, None, "note", &serde_json::json!({}))
            .unwrap();
        assert_eq!(names.row(event).quest_slug, "alpha");
        // A Quest outside the set is named by id rather than by nothing.
        let stray = Event {
            id: 9,
            quest_id: "q-stray".to_string(),
            session_id: None,
            ts: 0,
            kind: "note".to_string(),
            payload: None,
        };
        assert_eq!(names.row(stray).quest_slug, "q-stray");
    }

    #[test]
    fn learning_sessions_picks_up_one_that_started_mid_tail() {
        let db = db();
        let q = quest(&db, "alpha");
        let mut names = names_of(&db, std::slice::from_ref(&q));
        let late = db
            .insert_session(&Session::new(
                &q.id,
                SessionRole::Worker,
                "tests",
                "q-alpha",
                "%2",
            ))
            .unwrap();
        assert!(!names.knows_session(&late.id));
        names.learn_sessions(&db, &q.id).unwrap();
        assert!(names.knows_session(&late.id));
    }

    /// The fleet feed is the global tail, not the concatenation of per-Quest
    /// ones: the union each Quest hands over is cut down by id.
    #[test]
    fn load_merges_quests_by_id_and_keeps_the_global_tail() {
        let db = db();
        let a = quest(&db, "alpha");
        let b = quest(&db, "beta");
        let null = serde_json::Value::Null;
        for kind in ["a1", "a2", "a3"] {
            db.append_event(&a.id, None, kind, &null).unwrap();
        }
        for kind in ["b1", "b2"] {
            db.append_event(&b.id, None, kind, &null).unwrap();
        }
        db.append_event(&a.id, None, "a4", &null).unwrap();

        let quests = vec![a.clone(), b.clone()];
        let filter = EventFilter::default();
        let all = load(&db, &quests, &filter, 50).unwrap();
        let kinds: Vec<&str> = all.iter().map(|r| r.event.kind.as_str()).collect();
        assert_eq!(kinds, ["a1", "a2", "a3", "b1", "b2", "a4"]);
        assert!(all.windows(2).all(|w| w[0].event.id < w[1].event.id));

        let tail = load(&db, &quests, &filter, 3).unwrap();
        let kinds: Vec<&str> = tail.iter().map(|r| r.event.kind.as_str()).collect();
        assert_eq!(kinds, ["b1", "b2", "a4"]);

        // One Quest is the CLI's shape and skips the merge entirely.
        let one = load(&db, std::slice::from_ref(&a), &filter, 2).unwrap();
        let kinds: Vec<&str> = one.iter().map(|r| r.event.kind.as_str()).collect();
        assert_eq!(kinds, ["a3", "a4"]);
        assert!(one.iter().all(|r| r.quest_slug == "alpha"));

        assert!(load(&db, &quests, &filter, 0).unwrap().is_empty());
        assert!(load(&db, &[], &filter, 10).unwrap().is_empty());
    }

    #[test]
    fn load_applies_the_kind_filter_before_the_tail_is_cut() {
        let db = db();
        let a = quest(&db, "alpha");
        let b = quest(&db, "beta");
        let null = serde_json::Value::Null;
        db.append_event(&a.id, None, "session.start", &null)
            .unwrap();
        for _ in 0..5 {
            db.append_event(&b.id, None, "note", &null).unwrap();
        }
        db.append_event(&b.id, None, "session.stop", &null).unwrap();

        let filter = EventFilter {
            kinds: kinds_of(&["session.*".to_string()]).unwrap(),
            session_id: None,
        };
        let rows = load(&db, &[a, b], &filter, 2).unwrap();
        let kinds: Vec<&str> = rows.iter().map(|r| r.event.kind.as_str()).collect();
        // The five notes in between would have swallowed the tail had the
        // filter run after it.
        assert_eq!(kinds, ["session.start", "session.stop"]);
    }

    #[test]
    fn kinds_of_reports_the_first_bad_pattern() {
        assert_eq!(kinds_of(&[]).unwrap(), []);
        assert_eq!(
            kinds_of(&["note".to_string(), "session.*".to_string()]).unwrap(),
            [
                KindPattern::Exact("note".to_string()),
                KindPattern::Prefix("session.".to_string())
            ]
        );
        let err = kinds_of(&["se*sion".to_string()]).unwrap_err();
        assert!(format!("{err:#}").contains("trailing"), "{err:#}");
    }
}
