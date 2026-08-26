//! `q events` — a Quest's event log, filtered, optionally tailed (SPEC §16).

use std::collections::HashMap;
use std::io::{ErrorKind, Write};
use std::time::Duration;

use crate::Ctx;
use crate::brief;
use crate::commands::{fmt, sweep_quiet};
use crate::db::event::{EventFilter, KindPattern};
use crate::error::QError;
use crate::model::Event;
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

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    // The sweep only tidies session state; tmux being down must not hide the log.
    let _ = sweep_quiet(ctx);
    let db = ctx.db()?;
    let quest = db.resolve_quest(&brief::default_target(args.quest)?)?;
    let sessions = db.list_sessions_by_quest(&quest.id)?;

    let mut filter = EventFilter::default();
    for kind in args.kinds {
        filter.kinds.push(KindPattern::parse(kind)?);
    }
    if let Some(target) = args.session {
        let session = brief::resolve_session(&quest, &sessions, target)
            .ok_or_else(|| QError::NotFound(format!("session `{target}` not in {}", quest.slug)))?;
        filter.session_id = Some(session.id.clone());
    }
    let mut labels: HashMap<String, String> =
        sessions.into_iter().map(|s| (s.id, s.label)).collect();

    let events = db.list_events_latest(&quest.id, &filter, args.limit)?;
    // An empty first page (`-n 0`, or a filter nothing matches yet) must not
    // make the tail replay the whole history: start from the newest row.
    let mut last_id = match events.last() {
        Some(e) => e.id,
        None => db.last_event_id(&quest.id)?,
    };

    if !args.follow {
        if ctx.json || !ctx.quiet {
            output::emit(ctx.json, &events, || human_page(&events, &labels))?;
        }
        return Ok(());
    }

    // Follow: one line per event from the start, so a consumer sees a single
    // stream rather than an array followed by objects.
    let mut out = std::io::stdout().lock();
    if !write_lines(&mut out, ctx, &events, &labels)? {
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
                .is_some_and(|id| !labels.contains_key(id))
        }) {
            // A session that started after we did.
            for s in db.list_sessions_by_quest(&quest.id)? {
                labels.insert(s.id, s.label);
            }
        }
        last_id = fresh.last().map_or(last_id, |e| e.id);
        if !write_lines(&mut out, ctx, &fresh, &labels)? {
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

/// Writes `events` one per line; `Ok(false)` once the reader has gone away
/// (`q events --follow | head`), which ends the tail without an error.
fn write_lines(
    out: &mut impl Write,
    ctx: &Ctx,
    events: &[Event],
    labels: &HashMap<String, String>,
) -> anyhow::Result<bool> {
    for event in events {
        let line = if ctx.json {
            serde_json::to_string(event)?
        } else if ctx.quiet {
            continue;
        } else {
            human_line(event, labels)
        };
        match writeln!(out, "{line}").and_then(|()| out.flush()) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::BrokenPipe => return Ok(false),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(true)
}

fn human_page(events: &[Event], labels: &HashMap<String, String>) -> String {
    if events.is_empty() {
        return "no events".to_string();
    }
    events
        .iter()
        .map(|e| human_line(e, labels))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `<UTC stamp>  <kind>  [<session label or id>]  <payload>`.
fn human_line(event: &Event, labels: &HashMap<String, String>) -> String {
    let session = event
        .session_id
        .as_ref()
        .map(|id| labels.get(id).unwrap_or(id).as_str())
        .unwrap_or("-");
    format!(
        "{}  {:<KIND_WIDTH$}  [{}]  {}",
        fmt::stamp_utc(event.ts),
        event.kind,
        session,
        fmt::payload(event.payload.as_ref(), PAYLOAD_WIDTH)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_uses_the_label_when_known_and_the_id_otherwise() {
        let mut labels = HashMap::new();
        labels.insert("s-1".to_string(), "master".to_string());
        let event = Event {
            id: 1,
            quest_id: "q-1".to_string(),
            session_id: Some("s-1".to_string()),
            ts: 0,
            kind: "note".to_string(),
            payload: Some(serde_json::json!({ "text": "hi" })),
        };
        assert_eq!(
            human_line(&event, &labels),
            "1970-01-01 00:00:00  note              [master]  text=hi"
        );
        let unknown = Event {
            session_id: Some("s-9".to_string()),
            payload: None,
            ..event.clone()
        };
        assert_eq!(
            human_line(&unknown, &labels),
            "1970-01-01 00:00:00  note              [s-9]  -"
        );
        let none = Event {
            session_id: None,
            ..event
        };
        assert!(human_line(&none, &labels).contains("  [-]  "));
    }
}
