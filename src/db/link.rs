//! `link` table — external references attached to a Quest (SPEC §12).

use rusqlite::{Row, params};

use super::{Db, db_err, json_col, json_val};
use crate::model::Link;

const COLUMNS: &str = "id, quest_id, session_id, kind, ref, title, meta, enriched_at, created_at";

impl Db {
    /// `UNIQUE(quest_id, kind, ref)` — re-adding the same reference is an error,
    /// not a silent duplicate.
    ///
    /// `link.id` is ignored: the returned row carries the rowid SQLite assigned.
    pub fn insert_link(&self, link: &Link) -> anyhow::Result<Link> {
        let meta = json_val(link.meta.as_ref())?;
        self.conn
            .execute(
                "INSERT INTO link (quest_id, session_id, kind, ref, title, meta, enriched_at, \
                 created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    link.quest_id,
                    link.session_id,
                    link.kind,
                    link.r#ref,
                    link.title,
                    meta,
                    link.enriched_at,
                    link.created_at,
                ],
            )
            .map_err(db_err)?;
        Ok(Link {
            id: self.conn.last_insert_rowid(),
            ..link.clone()
        })
    }

    pub fn list_links_by_quest(&self, quest_id: &str) -> anyhow::Result<Vec<Link>> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {COLUMNS} FROM link WHERE quest_id = ?1 ORDER BY created_at, id"
            ))
            .map_err(db_err)?;
        let rows = stmt.query_map([quest_id], row_to_link).map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }
}

fn row_to_link(row: &Row) -> rusqlite::Result<Link> {
    Ok(Link {
        id: row.get("id")?,
        quest_id: row.get("quest_id")?,
        session_id: row.get("session_id")?,
        kind: row.get("kind")?,
        r#ref: row.get("ref")?,
        title: row.get("title")?,
        meta: json_col(row, "meta")?,
        enriched_at: row.get("enriched_at")?,
        created_at: row.get("created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Quest, now};

    fn link(quest_id: &str, kind: &str, reference: &str) -> Link {
        Link::new(quest_id, kind, reference)
    }

    #[test]
    fn new_stamps_the_creation_time_and_leaves_the_rest_empty() {
        let before = now();
        let l = Link::new("q-0001", "pr", "https://x/1");
        assert_eq!(l.id, 0);
        assert_eq!(l.quest_id, "q-0001");
        assert_eq!(l.kind, "pr");
        assert_eq!(l.r#ref, "https://x/1");
        assert!(l.session_id.is_none() && l.title.is_none() && l.meta.is_none());
        assert!(l.enriched_at.is_none());
        assert!(l.created_at >= before, "{} < {before}", l.created_at);
    }

    #[test]
    fn insert_assigns_the_rowid_and_ignores_the_one_given() {
        let db = Db::open_in_memory().unwrap();
        let q = db
            .insert_quest(&Quest::new("alpha", "/tmp/repo", "laptop"))
            .unwrap();
        let mut l = Link::new(&q.id, "url", "https://x");
        l.id = 4242;
        assert_eq!(db.insert_link(&l).unwrap().id, 1);
    }

    #[test]
    fn insert_then_list_round_trips_including_enrichment() {
        let db = Db::open_in_memory().unwrap();
        let q = db
            .insert_quest(&Quest::new("alpha", "/tmp/repo", "laptop"))
            .unwrap();

        let mut pr = link(&q.id, "pr", "https://github.com/x/y/pull/1");
        pr.title = Some("Fix the backfill".to_string());
        pr.meta = Some(serde_json::json!({ "state": "open", "ci": "passing" }));
        pr.enriched_at = Some(1234);
        let stored = db.insert_link(&pr).unwrap();
        assert!(stored.id > 0);

        db.insert_link(&link(&q.id, "branch", "fix/backfill"))
            .unwrap();
        let listed = db.list_links_by_quest(&q.id).unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0], stored);
        assert_eq!(listed[0].meta.as_ref().unwrap()["ci"], "passing");
        assert_eq!(listed[1].kind, "branch");
        assert!(db.list_links_by_quest("q-nope").unwrap().is_empty());
    }

    #[test]
    fn the_same_reference_cannot_be_added_twice() {
        let db = Db::open_in_memory().unwrap();
        let q = db
            .insert_quest(&Quest::new("alpha", "/tmp/repo", "laptop"))
            .unwrap();
        db.insert_link(&link(&q.id, "url", "https://x")).unwrap();
        let e = db
            .insert_link(&link(&q.id, "url", "https://x"))
            .unwrap_err();
        assert!(e.to_string().contains("UNIQUE"), "{e}");
        // A different kind for the same ref is fine.
        db.insert_link(&link(&q.id, "artifact", "https://x"))
            .unwrap();
    }
}
