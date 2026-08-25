//! `event` table — append-only, the source of truth for "what happened".

use rusqlite::{Row, params};

use super::{Db, db_err, json_col, json_val};
use crate::model::{Event, now};

const COLUMNS: &str = "id, quest_id, session_id, ts, kind, payload";

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
}

fn row_to_event(row: &Row) -> rusqlite::Result<Event> {
    Ok(Event {
        id: row.get("id")?,
        quest_id: row.get("quest_id")?,
        session_id: row.get("session_id")?,
        ts: row.get("ts")?,
        kind: row.get("kind")?,
        payload: json_col(row, "payload")?,
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
}
