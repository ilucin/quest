//! `event` table — append-only, the source of truth for "what happened".

use rusqlite::{Row, ToSql, params};

use super::{Db, db_err, json_col, json_val};
use crate::error::QError;
use crate::model::{Event, now};

const COLUMNS: &str = "id, quest_id, session_id, ts, kind, payload";

/// One `--kind` argument: an exact kind (`note`, `session.stop`) or a prefix
/// glob with a single trailing `*` (`session.*`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KindPattern {
    Exact(String),
    Prefix(String),
}

impl KindPattern {
    pub fn parse(text: &str) -> anyhow::Result<KindPattern> {
        let text = text.trim();
        if text.is_empty() {
            return Err(QError::Invalid("empty --kind".to_string()).into());
        }
        match text.strip_suffix('*') {
            Some(prefix) if prefix.contains('*') => Err(QError::Invalid(format!(
                "--kind `{text}`: only a trailing `*` is supported"
            ))
            .into()),
            Some(prefix) => Ok(KindPattern::Prefix(prefix.to_string())),
            None if text.contains('*') => Err(QError::Invalid(format!(
                "--kind `{text}`: only a trailing `*` is supported"
            ))
            .into()),
            None => Ok(KindPattern::Exact(text.to_string())),
        }
    }

    pub fn matches(&self, kind: &str) -> bool {
        match self {
            KindPattern::Exact(k) => kind == k,
            KindPattern::Prefix(p) => kind.starts_with(p),
        }
    }

    /// `kind = ?` or `kind LIKE ? ESCAPE '\'`, plus the value to bind.
    fn sql(&self, index: usize) -> (String, String) {
        match self {
            KindPattern::Exact(k) => (format!("kind = ?{index}"), k.clone()),
            KindPattern::Prefix(p) => (
                format!("kind LIKE ?{index} ESCAPE '\\'"),
                format!("{}%", like_escape(p)),
            ),
        }
    }
}

fn like_escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// What `q events` narrows a Quest's log by. Empty `kinds` matches every kind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventFilter {
    pub kinds: Vec<KindPattern>,
    pub session_id: Option<String>,
}

impl EventFilter {
    /// The `WHERE` tail after `quest_id = ?1`; parameters start at `?3`
    /// (`?2` is the limit or the cursor, see the callers).
    fn where_sql(&self, first_param: usize, params: &mut Vec<Box<dyn ToSql>>) -> String {
        let mut sql = String::new();
        let mut next = first_param;
        if let Some(session_id) = &self.session_id {
            sql.push_str(&format!(" AND session_id = ?{next}"));
            params.push(Box::new(session_id.clone()));
            next += 1;
        }
        if !self.kinds.is_empty() {
            let mut clauses = Vec::with_capacity(self.kinds.len());
            for kind in &self.kinds {
                let (clause, value) = kind.sql(next);
                clauses.push(clause);
                params.push(Box::new(value));
                next += 1;
            }
            sql.push_str(&format!(" AND ({})", clauses.join(" OR ")));
        }
        sql
    }
}

impl Db {
    /// Appends one event and returns the stored row. A `null` payload is
    /// stored as SQL NULL.
    pub fn append_event(
        &self,
        quest_id: &str,
        session_id: Option<&str>,
        kind: &str,
        payload: &serde_json::Value,
    ) -> anyhow::Result<Event> {
        let encoded = json_val(Some(payload))?;
        let ts = now();
        self.conn
            .execute(
                "INSERT INTO event (quest_id, session_id, ts, kind, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![quest_id, session_id, ts, kind, encoded],
            )
            .map_err(db_err)?;
        Ok(Event {
            id: self.conn.last_insert_rowid(),
            quest_id: quest_id.to_string(),
            session_id: session_id.map(str::to_string),
            ts,
            kind: kind.to_string(),
            payload: encoded.map(|_| payload.clone()),
        })
    }

    /// Most recent first, capped at `limit`.
    pub fn list_events_by_quest(&self, quest_id: &str, limit: usize) -> anyhow::Result<Vec<Event>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM event WHERE quest_id = ?1 ORDER BY ts DESC, id DESC LIMIT ?2"
            ))
            .map_err(db_err)?;
        let rows = stmt
            .query_map(
                params![quest_id, i64::try_from(limit).unwrap_or(i64::MAX)],
                row_to_event,
            )
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    /// Only events whose kind is in `kinds`, most recent first, capped at
    /// `limit`. An empty `kinds` matches nothing.
    pub fn list_events_by_kinds(
        &self,
        quest_id: &str,
        kinds: &[&str],
        limit: usize,
    ) -> anyhow::Result<Vec<Event>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let filter = EventFilter {
            kinds: kinds
                .iter()
                .map(|k| KindPattern::Exact(k.to_string()))
                .collect(),
            session_id: None,
        };
        let mut events = self.list_events_latest(quest_id, &filter, limit)?;
        events.reverse();
        Ok(events)
    }

    /// Highest event id for a Quest, `0` when it has none. The `--follow`
    /// cursor starts here when the first page is empty.
    pub fn last_event_id(&self, quest_id: &str) -> anyhow::Result<i64> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(id), 0) FROM event WHERE quest_id = ?1",
                params![quest_id],
                |r| r.get(0),
            )
            .map_err(db_err)
    }

    /// The last `limit` events matching `filter`, oldest first — the page
    /// `q events` opens with.
    pub fn list_events_latest(
        &self,
        quest_id: &str,
        filter: &EventFilter,
        limit: usize,
    ) -> anyhow::Result<Vec<Event>> {
        let mut params: Vec<Box<dyn ToSql>> = vec![
            Box::new(quest_id.to_string()),
            Box::new(i64::try_from(limit).unwrap_or(i64::MAX)),
        ];
        let tail = filter.where_sql(3, &mut params);
        let sql = format!(
            "SELECT {COLUMNS} FROM event WHERE quest_id = ?1{tail} ORDER BY id DESC LIMIT ?2"
        );
        let mut events = self.query_events(&sql, &params)?;
        events.reverse();
        Ok(events)
    }

    /// Events with `id > after_id` matching `filter`, oldest first, capped at
    /// `limit` — one poll of `q events --follow`.
    pub fn list_events_after(
        &self,
        quest_id: &str,
        after_id: i64,
        filter: &EventFilter,
        limit: usize,
    ) -> anyhow::Result<Vec<Event>> {
        let mut params: Vec<Box<dyn ToSql>> = vec![
            Box::new(quest_id.to_string()),
            Box::new(after_id),
            Box::new(i64::try_from(limit).unwrap_or(i64::MAX)),
        ];
        let tail = filter.where_sql(4, &mut params);
        let sql = format!(
            "SELECT {COLUMNS} FROM event WHERE quest_id = ?1 AND id > ?2{tail} \
             ORDER BY id ASC LIMIT ?3"
        );
        self.query_events(&sql, &params)
    }

    fn query_events(&self, sql: &str, params: &[Box<dyn ToSql>]) -> anyhow::Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(sql).map_err(db_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), row_to_event)
            .map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }
}

fn row_to_event(row: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get("id")?,
        quest_id: row.get("quest_id")?,
        session_id: row.get("session_id")?,
        ts: row.get("ts")?,
        kind: row.get("kind")?,
        // A hand-edited or foreign payload that is not JSON still reads.
        payload: match json_col(row, "payload") {
            Ok(payload) => payload,
            Err(_) => row
                .get::<_, Option<String>>("payload")?
                .map(serde_json::Value::String),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Quest, Session, SessionRole};

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn quest(db: &Db, slug: &str) -> Quest {
        db.insert_quest(&Quest::new(slug, "/tmp/repo", "laptop"))
            .unwrap()
    }

    #[test]
    fn append_stores_the_payload_as_json() {
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

        let payload = serde_json::json!({ "phase": "planning", "n": 3 });
        let event = db
            .append_event(&q.id, Some(&s.id), "phase", &payload)
            .unwrap();
        assert!(event.id > 0);
        assert_eq!(event.kind, "phase");
        assert_eq!(event.session_id.as_deref(), Some(s.id.as_str()));
        assert_eq!(event.payload.as_ref(), Some(&payload));

        let stored = db.list_events_by_quest(&q.id, 10).unwrap();
        assert_eq!(stored, vec![event]);
    }

    #[test]
    fn a_null_payload_stays_null() {
        let db = db();
        let q = quest(&db, "alpha");
        let event = db
            .append_event(&q.id, None, "quest.created", &serde_json::Value::Null)
            .unwrap();
        assert_eq!(event.payload, None);
        assert_eq!(db.list_events_by_quest(&q.id, 10).unwrap()[0].payload, None);
    }

    #[test]
    fn listing_is_newest_first_scoped_and_capped() {
        let db = db();
        let a = quest(&db, "alpha");
        let b = quest(&db, "beta");
        for kind in ["one", "two", "three"] {
            db.append_event(&a.id, None, kind, &serde_json::Value::Null)
                .unwrap();
        }
        db.append_event(&b.id, None, "other", &serde_json::Value::Null)
            .unwrap();

        let kinds: Vec<String> = db
            .list_events_by_quest(&a.id, 10)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["three", "two", "one"]);
        assert_eq!(db.list_events_by_quest(&a.id, 2).unwrap().len(), 2);
        assert_eq!(db.list_events_by_quest(&b.id, 10).unwrap().len(), 1);
        assert!(db.list_events_by_quest("q-nope", 10).unwrap().is_empty());
    }

    #[test]
    fn non_json_payload_reads_as_a_raw_string() {
        let db = db();
        let q = quest(&db, "alpha");
        db.conn
            .execute(
                "INSERT INTO event (quest_id, ts, kind, payload) VALUES (?1, 1, 'note', 'not json')",
                [&q.id],
            )
            .unwrap();
        let events = db.list_events_by_quest(&q.id, 10).unwrap();
        assert_eq!(events[0].payload, Some(serde_json::json!("not json")));
    }

    #[test]
    fn listing_by_kinds_filters_and_caps() {
        let db = db();
        let q = quest(&db, "alpha");
        for kind in ["note", "session.prompt", "phase", "note", "session.stop"] {
            db.append_event(&q.id, None, kind, &serde_json::Value::Null)
                .unwrap();
        }
        let kinds: Vec<String> = db
            .list_events_by_kinds(&q.id, &["note", "phase"], 10)
            .unwrap()
            .into_iter()
            .map(|e| e.kind)
            .collect();
        assert_eq!(kinds, ["note", "phase", "note"]);
        assert_eq!(
            db.list_events_by_kinds(&q.id, &["note", "phase"], 2)
                .unwrap()
                .len(),
            2
        );
        assert!(db.list_events_by_kinds(&q.id, &[], 10).unwrap().is_empty());
    }

    #[test]
    fn kind_patterns_parse_exact_and_trailing_glob() {
        assert_eq!(
            KindPattern::parse("note").unwrap(),
            KindPattern::Exact("note".to_string())
        );
        assert_eq!(
            KindPattern::parse("session.*").unwrap(),
            KindPattern::Prefix("session.".to_string())
        );
        assert_eq!(
            KindPattern::parse("*").unwrap(),
            KindPattern::Prefix(String::new())
        );
        assert!(KindPattern::parse("").is_err());
        assert!(KindPattern::parse("se*sion").is_err());
        assert!(KindPattern::parse("*.stop").is_err());

        let glob = KindPattern::parse("session.*").unwrap();
        assert!(glob.matches("session.stop"));
        assert!(glob.matches("session."));
        assert!(!glob.matches("sessionx"));
        assert!(!glob.matches("note"));
        let exact = KindPattern::parse("session.stop").unwrap();
        assert!(exact.matches("session.stop"));
        assert!(!exact.matches("session.stopped"));
    }

    #[test]
    fn like_escaping_neutralises_wildcards() {
        assert_eq!(like_escape("a_b%c\\d"), "a\\_b\\%c\\\\d");
        let (clause, value) = KindPattern::Prefix("s_".to_string()).sql(3);
        assert_eq!(clause, "kind LIKE ?3 ESCAPE '\\'");
        assert_eq!(value, "s\\_%");
    }

    fn filter(kinds: &[&str], session_id: Option<&str>) -> EventFilter {
        EventFilter {
            kinds: kinds
                .iter()
                .map(|k| KindPattern::parse(k).unwrap())
                .collect(),
            session_id: session_id.map(str::to_string),
        }
    }

    fn kinds_of(events: &[Event]) -> Vec<&str> {
        events.iter().map(|e| e.kind.as_str()).collect()
    }

    #[test]
    fn latest_returns_the_last_n_oldest_first_and_filters() {
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
        let null = serde_json::Value::Null;
        db.append_event(&q.id, None, "quest.created", &null)
            .unwrap();
        db.append_event(&q.id, Some(&s.id), "session.start", &null)
            .unwrap();
        db.append_event(&q.id, Some(&s.id), "note", &null).unwrap();
        db.append_event(&q.id, None, "note", &null).unwrap();
        db.append_event(&q.id, Some(&s.id), "session.stop", &null)
            .unwrap();
        // `_` must not act as a LIKE wildcard.
        db.append_event(&q.id, None, "session_x", &null).unwrap();

        let all = db
            .list_events_latest(&q.id, &filter(&[], None), 50)
            .unwrap();
        assert_eq!(
            kinds_of(&all),
            [
                "quest.created",
                "session.start",
                "note",
                "note",
                "session.stop",
                "session_x"
            ]
        );
        assert!(all.windows(2).all(|w| w[0].id < w[1].id));

        let last2 = db.list_events_latest(&q.id, &filter(&[], None), 2).unwrap();
        assert_eq!(kinds_of(&last2), ["session.stop", "session_x"]);

        let glob = db
            .list_events_latest(&q.id, &filter(&["session.*"], None), 50)
            .unwrap();
        assert_eq!(kinds_of(&glob), ["session.start", "session.stop"]);

        let multi = db
            .list_events_latest(&q.id, &filter(&["note", "session.stop"], None), 50)
            .unwrap();
        assert_eq!(kinds_of(&multi), ["note", "note", "session.stop"]);

        let by_session = db
            .list_events_latest(&q.id, &filter(&["note"], Some(&s.id)), 50)
            .unwrap();
        assert_eq!(by_session.len(), 1);
        assert_eq!(by_session[0].session_id.as_deref(), Some(s.id.as_str()));

        assert!(
            db.list_events_latest(&q.id, &filter(&[], Some("s-ghost")), 50)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn after_returns_only_newer_rows_in_id_order() {
        let db = db();
        let q = quest(&db, "alpha");
        let null = serde_json::Value::Null;
        let first = db.append_event(&q.id, None, "note", &null).unwrap();
        db.append_event(&q.id, None, "phase", &null).unwrap();
        db.append_event(&q.id, None, "note", &null).unwrap();

        let after = db
            .list_events_after(&q.id, first.id, &filter(&[], None), 50)
            .unwrap();
        assert_eq!(kinds_of(&after), ["phase", "note"]);
        assert!(after.iter().all(|e| e.id > first.id));

        let notes = db
            .list_events_after(&q.id, first.id, &filter(&["note"], None), 50)
            .unwrap();
        assert_eq!(kinds_of(&notes), ["note"]);

        let last = after.last().unwrap().id;
        assert!(
            db.list_events_after(&q.id, last, &filter(&[], None), 50)
                .unwrap()
                .is_empty()
        );
    }
}
