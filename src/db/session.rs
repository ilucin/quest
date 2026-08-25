//! `session` table. A session's identity is its tmux pane (SPEC §6); the
//! database is machine-bound, so the pane alone is unique here.

use rusqlite::{Row, params};

use super::{Db, ID_ATTEMPTS, db_err, enum_col, is_id_collision};
use crate::error::QError;
use crate::model::{Session, SessionRole, SessionStatus, new_id, now};

const COLUMNS: &str = "id, quest_id, role, label, tmux_session, tmux_pane, claude_pid, \
     claude_session_id, claude_name, workflow, phase, status, waiting_for, ctx_pct, \
     ctx_updated_at, first_prompt, last_prompt, started_at, ended_at, updated_at";

impl Db {
    /// Inserts `session`, regenerating its id on collision.
    pub fn insert_session(&self, session: &Session) -> anyhow::Result<Session> {
        let mut row = session.clone();
        for attempt in 0..ID_ATTEMPTS {
            match self.try_insert_session(&row) {
                Ok(()) => return Ok(row),
                Err(e) if is_id_collision(&e, "session") && attempt + 1 < ID_ATTEMPTS => {
                    row.id = new_id("s");
                }
                Err(e) => return Err(db_err(e)),
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    fn try_insert_session(&self, s: &Session) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO session ({COLUMNS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                  ?18, ?19, ?20)"
            ),
            params![
                s.id,
                s.quest_id,
                s.role,
                s.label,
                s.tmux_session,
                s.tmux_pane,
                s.claude_pid,
                s.claude_session_id,
                s.claude_name,
                s.workflow,
                s.phase,
                s.status,
                s.waiting_for,
                s.ctx_pct,
                s.ctx_updated_at,
                s.first_prompt,
                s.last_prompt,
                s.started_at,
                s.ended_at,
                s.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, id: &str) -> anyhow::Result<Option<Session>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM session WHERE id = ?1"),
                [id],
                row_to_session,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })
    }

    /// Oldest first, so the master (window 0) leads the list.
    pub fn list_sessions_by_quest(&self, quest_id: &str) -> anyhow::Result<Vec<Session>> {
        self.query_sessions(
            &format!("SELECT {COLUMNS} FROM session WHERE quest_id = ?1 ORDER BY started_at, id"),
            [quest_id],
        )
    }

    /// Every session across every Quest that has not ended — the TUI's fleet view.
    pub fn list_live_sessions(&self) -> anyhow::Result<Vec<Session>> {
        self.query_sessions(
            &format!(
                "SELECT {COLUMNS} FROM session WHERE status != 'ended' ORDER BY started_at, id"
            ),
            [],
        )
    }

    fn query_sessions<P: rusqlite::Params>(
        &self,
        sql: &str,
        params: P,
    ) -> anyhow::Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(sql).map_err(db_err)?;
        let rows = stmt.query_map(params, row_to_session).map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    pub fn update_session_status(
        &self,
        id: &str,
        status: SessionStatus,
        waiting_for: Option<&str>,
    ) -> anyhow::Result<Session> {
        self.conn
            .execute(
                "UPDATE session SET status = ?1, waiting_for = ?2, updated_at = ?3 WHERE id = ?4",
                params![status, waiting_for, now(), id],
            )
            .map_err(db_err)?;
        self.require_session(id)
    }

    pub fn mark_session_ended(&self, id: &str, ended_at: i64) -> anyhow::Result<Session> {
        self.conn
            .execute(
                "UPDATE session SET status = 'ended', waiting_for = NULL, ended_at = ?1, \
                 updated_at = ?2 WHERE id = ?3",
                params![ended_at, now(), id],
            )
            .map_err(db_err)?;
        self.require_session(id)
    }

    /// The live session in that pane. Ended rows stay behind as history, and a
    /// pane can be reused, so they are skipped.
    pub fn find_session_by_pane(&self, tmux_pane: &str) -> anyhow::Result<Option<Session>> {
        self.conn
            .query_row(
                &format!(
                    "SELECT {COLUMNS} FROM session WHERE tmux_pane = ?1 AND status != 'ended' \
                     ORDER BY started_at DESC, id DESC LIMIT 1"
                ),
                [tmux_pane],
                row_to_session,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })
    }

    fn require_session(&self, id: &str) -> anyhow::Result<Session> {
        self.get_session(id)?
            .ok_or_else(|| QError::NotFound(format!("session `{id}`")).into())
    }
}

fn row_to_session(row: &Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get("id")?,
        quest_id: row.get("quest_id")?,
        role: enum_col::<SessionRole>(row, "role")?,
        label: row.get("label")?,
        tmux_session: row.get("tmux_session")?,
        tmux_pane: row.get("tmux_pane")?,
        claude_pid: row.get("claude_pid")?,
        claude_session_id: row.get("claude_session_id")?,
        claude_name: row.get("claude_name")?,
        workflow: row.get("workflow")?,
        phase: row.get("phase")?,
        status: enum_col::<SessionStatus>(row, "status")?,
        waiting_for: row.get("waiting_for")?,
        ctx_pct: row.get("ctx_pct")?,
        ctx_updated_at: row.get("ctx_updated_at")?,
        first_prompt: row.get("first_prompt")?,
        last_prompt: row.get("last_prompt")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Quest;

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn quest(db: &Db, slug: &str) -> Quest {
        db.insert_quest(&Quest::new(slug, "/tmp/repo", "laptop"))
            .unwrap()
    }

    fn session(db: &Db, quest_id: &str, label: &str, pane: &str) -> Session {
        db.insert_session(&Session::new(
            quest_id,
            SessionRole::Worker,
            label,
            "q-alpha",
            pane,
        ))
        .unwrap()
    }

    #[test]
    fn insert_then_get_round_trips_every_column() {
        let db = db();
        let q = quest(&db, "alpha");
        let mut s = Session::new(&q.id, SessionRole::Master, "master", "q-alpha", "%42");
        s.claude_pid = Some(1234);
        s.claude_session_id = Some("abc".to_string());
        s.claude_name = Some("alpha/master".to_string());
        s.workflow = Some("orchestrator".to_string());
        s.phase = Some("planning".to_string());
        s.status = SessionStatus::Waiting;
        s.waiting_for = Some("permission".to_string());
        s.ctx_pct = Some(41);
        s.ctx_updated_at = Some(99);
        s.first_prompt = Some("go".to_string());
        s.last_prompt = Some("continue".to_string());

        let stored = db.insert_session(&s).unwrap();
        assert_eq!(stored, s);
        assert_eq!(db.get_session(&s.id).unwrap().unwrap(), s);
        assert!(db.get_session("s-nope").unwrap().is_none());
    }

    #[test]
    fn a_colliding_id_is_retried() {
        let db = db();
        let q = quest(&db, "alpha");
        let first = session(&db, &q.id, "w1", "%1");
        let mut clash = Session::new(&q.id, SessionRole::Worker, "w2", "q-alpha", "%2");
        clash.id = first.id.clone();
        let stored = db.insert_session(&clash).unwrap();
        assert_ne!(stored.id, first.id);
        assert_eq!(db.get_session(&stored.id).unwrap().unwrap().label, "w2");
    }

    #[test]
    fn list_by_quest_is_scoped_and_ordered() {
        let db = db();
        let a = quest(&db, "alpha");
        let b = quest(&db, "beta");
        let mut first = Session::new(&a.id, SessionRole::Master, "master", "q-alpha", "%1");
        first.started_at = 100;
        let first = db.insert_session(&first).unwrap();
        let mut second = Session::new(&a.id, SessionRole::Worker, "w1", "q-alpha", "%2");
        second.started_at = 200;
        db.insert_session(&second).unwrap();
        session(&db, &b.id, "master", "%3");

        let labels: Vec<String> = db
            .list_sessions_by_quest(&a.id)
            .unwrap()
            .into_iter()
            .map(|s| s.label)
            .collect();
        assert_eq!(labels, ["master", "w1"]);
        assert_eq!(db.list_sessions_by_quest(&b.id).unwrap().len(), 1);
        assert_eq!(first.role, SessionRole::Master);
    }

    #[test]
    fn live_sessions_span_quests_and_skip_ended() {
        let db = db();
        let a = quest(&db, "alpha");
        let b = quest(&db, "beta");
        let s1 = session(&db, &a.id, "w1", "%1");
        session(&db, &b.id, "w1", "%2");
        assert_eq!(db.list_live_sessions().unwrap().len(), 2);

        db.mark_session_ended(&s1.id, 500).unwrap();
        let live: Vec<String> = db
            .list_live_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.quest_id)
            .collect();
        assert_eq!(live, [b.id]);
    }

    #[test]
    fn status_updates_carry_the_reason() {
        let db = db();
        let q = quest(&db, "alpha");
        let s = session(&db, &q.id, "w1", "%1");
        assert_eq!(s.status, SessionStatus::Starting);

        let waiting = db
            .update_session_status(&s.id, SessionStatus::Waiting, Some("permission"))
            .unwrap();
        assert_eq!(waiting.status, SessionStatus::Waiting);
        assert_eq!(waiting.waiting_for.as_deref(), Some("permission"));

        let busy = db
            .update_session_status(&s.id, SessionStatus::Busy, None)
            .unwrap();
        assert_eq!(busy.status, SessionStatus::Busy);
        assert_eq!(busy.waiting_for, None);
    }

    #[test]
    fn ending_a_session_clears_what_it_was_waiting_for() {
        let db = db();
        let q = quest(&db, "alpha");
        let s = session(&db, &q.id, "w1", "%1");
        db.update_session_status(&s.id, SessionStatus::Waiting, Some("input"))
            .unwrap();

        let ended = db.mark_session_ended(&s.id, 4242).unwrap();
        assert_eq!(ended.status, SessionStatus::Ended);
        assert_eq!(ended.ended_at, Some(4242));
        assert_eq!(ended.waiting_for, None);
    }

    #[test]
    fn updating_an_unknown_session_is_not_found() {
        let db = db();
        let e = db
            .update_session_status("s-nope", SessionStatus::Busy, None)
            .unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("not_found"),
            "{e}"
        );
    }

    #[test]
    fn find_by_pane_returns_the_live_occupant() {
        let db = db();
        let q = quest(&db, "alpha");
        let old = session(&db, &q.id, "w1", "%7");
        assert_eq!(db.find_session_by_pane("%7").unwrap().unwrap().id, old.id);
        assert!(db.find_session_by_pane("%99").unwrap().is_none());

        db.mark_session_ended(&old.id, 1).unwrap();
        assert!(db.find_session_by_pane("%7").unwrap().is_none());

        let new = session(&db, &q.id, "w2", "%7");
        assert_eq!(db.find_session_by_pane("%7").unwrap().unwrap().id, new.id);
    }
}
