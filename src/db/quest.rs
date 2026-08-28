//! `quest` table, plus the small `name_cache` that keys auto-naming.

use rusqlite::{Row, ToSql, params};

use super::{Db, ID_ATTEMPTS, bool_col, db_err, enum_col, is_id_collision, u8_col};
use crate::error::QError;
use crate::model::{NameOrigin, NameSource, Quest, QuestState, new_id, now};

const COLUMNS: &str = "id, slug, name_source, name_input_hash, goal, cwd, machine, state, \
     workflow, template_id, beads_epic, beads_repo, brain_session, ctx_reset_pct, \
     auto_reset, created_at, updated_at, finished_at";

/// Fields `q set` / `q rename` can change. `None` leaves a column alone; the
/// nullable columns nest, so `Some(None)` clears them.
#[derive(Debug, Default, Clone)]
pub struct QuestPatch {
    pub slug: Option<String>,
    pub name_source: Option<NameSource>,
    pub name_input_hash: Option<Option<String>>,
    pub goal: Option<Option<String>>,
    pub cwd: Option<String>,
    pub workflow: Option<Option<String>>,
    pub ctx_reset_pct: Option<Option<u8>>,
    pub beads_epic: Option<Option<String>>,
    pub beads_repo: Option<Option<String>>,
    pub brain_session: Option<Option<String>>,

    pub auto_reset: Option<Option<bool>>,
}

/// One `name_cache` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedName {
    pub slug: String,
    pub source: NameOrigin,
}

impl QuestPatch {
    fn is_empty(&self) -> bool {
        self.slug.is_none()
            && self.name_source.is_none()
            && self.name_input_hash.is_none()
            && self.goal.is_none()
            && self.cwd.is_none()
            && self.workflow.is_none()
            && self.ctx_reset_pct.is_none()
            && self.beads_epic.is_none()
            && self.beads_repo.is_none()
            && self.brain_session.is_none()
            && self.auto_reset.is_none()
    }
}

impl Db {
    /// Inserts `quest`, regenerating its id on collision. The stored row is
    /// returned, so callers see the id that actually landed.
    pub fn insert_quest(&self, quest: &Quest) -> anyhow::Result<Quest> {
        let mut row = quest.clone();
        for attempt in 0..ID_ATTEMPTS {
            match self.try_insert_quest(&row) {
                Ok(()) => return Ok(row),
                Err(e) if is_id_collision(&e, "quest") && attempt + 1 < ID_ATTEMPTS => {
                    row.id = new_id("q");
                }
                Err(e) => return Err(db_err(e)),
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    fn try_insert_quest(&self, q: &Quest) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO quest ({COLUMNS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
                  ?18)"
            ),
            params![
                q.id,
                q.slug,
                q.name_source,
                q.name_input_hash,
                q.goal,
                q.cwd,
                q.machine,
                q.state,
                q.workflow,
                q.template_id,
                q.beads_epic,
                q.beads_repo,
                q.brain_session,
                q.ctx_reset_pct,
                q.auto_reset,
                q.created_at,
                q.updated_at,
                q.finished_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_quest(&self, id: &str) -> anyhow::Result<Option<Quest>> {
        self.one_quest("id", id)
    }

    pub fn get_quest_by_slug(&self, slug: &str) -> anyhow::Result<Option<Quest>> {
        self.one_quest("slug", slug)
    }

    fn one_quest(&self, column: &str, value: &str) -> anyhow::Result<Option<Quest>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM quest WHERE {column} = ?1"),
                [value],
                row_to_quest,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })
    }

    /// Newest first. Finished Quests are hidden unless asked for.
    ///
    /// `created_at` is second-precision and ids are random, so the tie-break is
    /// `rowid`: SQLite hands it out monotonically, which makes "newest first"
    /// mean insertion order whenever two Quests land in the same second.
    pub fn list_quests(&self, include_finished: bool) -> anyhow::Result<Vec<Quest>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM quest {} ORDER BY created_at DESC, rowid DESC",
            if include_finished {
                ""
            } else {
                "WHERE state != 'finished'"
            }
        );
        let mut stmt = self.conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt.query_map([], row_to_quest).map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    pub fn update_quest_state(
        &self,
        id: &str,
        state: QuestState,
        finished_at: Option<i64>,
    ) -> anyhow::Result<Quest> {
        self.conn
            .execute(
                "UPDATE quest SET state = ?1, finished_at = ?2, updated_at = ?3 WHERE id = ?4",
                params![state, finished_at, now(), id],
            )
            .map_err(db_err)?;
        self.require_quest(id)
    }

    pub fn update_quest(&self, id: &str, patch: &QuestPatch) -> anyhow::Result<Quest> {
        if patch.is_empty() {
            return self.require_quest(id);
        }
        let ts = now();
        let mut sets: Vec<&str> = Vec::new();
        let mut binds: Vec<(&str, &dyn ToSql)> = Vec::new();
        if let Some(v) = &patch.slug {
            sets.push("slug = :slug");
            binds.push((":slug", v));
        }
        if let Some(v) = &patch.name_source {
            sets.push("name_source = :name_source");
            binds.push((":name_source", v));
        }
        if let Some(v) = &patch.name_input_hash {
            sets.push("name_input_hash = :name_input_hash");
            binds.push((":name_input_hash", v));
        }
        if let Some(v) = &patch.goal {
            sets.push("goal = :goal");
            binds.push((":goal", v));
        }
        if let Some(v) = &patch.cwd {
            sets.push("cwd = :cwd");
            binds.push((":cwd", v));
        }
        if let Some(v) = &patch.workflow {
            sets.push("workflow = :workflow");
            binds.push((":workflow", v));
        }
        if let Some(v) = &patch.ctx_reset_pct {
            sets.push("ctx_reset_pct = :pct");
            binds.push((":pct", v));
        }
        if let Some(v) = &patch.beads_epic {
            sets.push("beads_epic = :beads_epic");
            binds.push((":beads_epic", v));
        }
        if let Some(v) = &patch.beads_repo {
            sets.push("beads_repo = :beads_repo");
            binds.push((":beads_repo", v));
        }
        if let Some(v) = &patch.brain_session {
            sets.push("brain_session = :brain_session");
            binds.push((":brain_session", v));
        }
        if let Some(v) = &patch.auto_reset {
            sets.push("auto_reset = :auto_reset");
            binds.push((":auto_reset", v));
        }
        sets.push("updated_at = :ts");
        binds.push((":ts", &ts));
        binds.push((":id", &id));

        let sql = format!("UPDATE quest SET {} WHERE id = :id", sets.join(", "));
        self.conn.execute(&sql, &binds[..]).map_err(db_err)?;
        self.require_quest(id)
    }

    /// Removes the Quest and everything hanging off it. `event` and `link` have
    /// no FK to `quest` in the schema, so the cascade is explicit and wrapped in
    /// one transaction.
    pub fn delete_quest(&self, id: &str) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        for sql in [
            "DELETE FROM event WHERE quest_id = ?1",
            "DELETE FROM link WHERE quest_id = ?1",
            "DELETE FROM session WHERE quest_id = ?1",
            "DELETE FROM quest WHERE id = ?1",
        ] {
            tx.execute(sql, [id]).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    /// SPEC §16 target resolution: exact id, exact slug, then unique prefix,
    /// then unique substring. An id is unambiguous by construction, so it wins
    /// over a slug that happens to spell the same thing.
    pub fn resolve_quest(&self, target: &str) -> anyhow::Result<Quest> {
        if target.is_empty() {
            return Err(QError::NotFound("quest ``".to_string()).into());
        }
        let all = self.list_quests(true)?;
        if let Some(hit) = all.iter().find(|q| q.id == target).cloned() {
            return Ok(hit);
        }
        if let Some(hit) = all.iter().find(|q| q.slug == target).cloned() {
            return Ok(hit);
        }
        for matches in [
            narrow(&all, |q| {
                q.id.starts_with(target) || q.slug.starts_with(target)
            }),
            narrow(&all, |q| q.id.contains(target) || q.slug.contains(target)),
        ] {
            match matches.len() {
                0 => continue,
                1 => return Ok(matches.into_iter().next().expect("length checked")),
                _ => {
                    return Err(QError::Ambiguous {
                        target: target.to_string(),
                        candidates: matches
                            .into_iter()
                            .map(|q| format!("{} ({})", q.id, q.slug))
                            .collect(),
                    }
                    .into());
                }
            }
        }
        Err(QError::NotFound(format!("quest `{target}`")).into())
    }

    fn require_quest(&self, id: &str) -> anyhow::Result<Quest> {
        self.get_quest(id)?
            .ok_or_else(|| QError::NotFound(format!("quest `{id}`")).into())
    }

    /// A cached proposal for this input, if one was ever accepted. Only
    /// validated model answers land here — a heuristic fallback is never
    /// cached (SPEC §10), so `source` is in practice always `claude`; it is
    /// stored anyway so the provenance survives a future second namer.
    pub fn name_cache_get(&self, input_hash: &str) -> anyhow::Result<Option<CachedName>> {
        self.conn
            .query_row(
                "SELECT slug, source FROM name_cache WHERE input_hash = ?1",
                [input_hash],
                |row| {
                    Ok(CachedName {
                        slug: row.get(0)?,
                        source: enum_col::<NameOrigin>(row, "source")?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })
    }

    pub fn name_cache_put(
        &self,
        input_hash: &str,
        slug: &str,
        source: NameOrigin,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO name_cache (input_hash, slug, source, created_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(input_hash) DO UPDATE SET slug = excluded.slug, \
                 source = excluded.source",
                params![input_hash, slug, source, now()],
            )
            .map_err(db_err)?;
        Ok(())
    }
}

fn narrow(all: &[Quest], pred: impl Fn(&Quest) -> bool) -> Vec<Quest> {
    all.iter().filter(|q| pred(q)).cloned().collect()
}

fn row_to_quest(row: &Row) -> rusqlite::Result<Quest> {
    Ok(Quest {
        id: row.get("id")?,
        slug: row.get("slug")?,
        name_source: enum_col::<NameSource>(row, "name_source")?,
        name_input_hash: row.get("name_input_hash")?,
        goal: row.get("goal")?,
        cwd: row.get("cwd")?,
        machine: row.get("machine")?,
        state: enum_col::<QuestState>(row, "state")?,
        workflow: row.get("workflow")?,
        template_id: row.get("template_id")?,
        beads_epic: row.get("beads_epic")?,
        beads_repo: row.get("beads_repo")?,
        brain_session: row.get("brain_session")?,
        ctx_reset_pct: u8_col(row, "ctx_reset_pct")?,
        auto_reset: bool_col(row, "auto_reset")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        finished_at: row.get("finished_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Session, SessionRole};

    fn db() -> Db {
        Db::open_in_memory().unwrap()
    }

    fn insert(db: &Db, slug: &str) -> Quest {
        db.insert_quest(&Quest::new(slug, "/tmp/repo", "laptop"))
            .unwrap()
    }

    fn code_of(e: &anyhow::Error) -> &'static str {
        e.downcast_ref::<QError>()
            .map(QError::code)
            .unwrap_or("other")
    }

    #[test]
    fn insert_then_get_round_trips_every_column() {
        let db = db();
        let mut quest = Quest::new("cdc-backfill-retry", "/tmp/repo", "laptop");
        quest.name_source = NameSource::Auto;
        quest.name_input_hash = Some("deadbeef".to_string());
        quest.goal = Some("make the backfill idempotent".to_string());
        quest.workflow = Some("orchestrator".to_string());
        quest.beads_epic = Some("bd-123".to_string());
        quest.beads_repo = Some("api".to_string());
        quest.brain_session = Some("cdc".to_string());
        quest.ctx_reset_pct = Some(40);

        let stored = db.insert_quest(&quest).unwrap();
        assert_eq!(stored, quest);
        assert_eq!(db.get_quest(&quest.id).unwrap().unwrap(), quest);
        assert_eq!(db.get_quest_by_slug(&quest.slug).unwrap().unwrap(), quest);
    }

    #[test]
    fn get_returns_none_for_an_unknown_id() {
        let db = db();
        assert!(db.get_quest("q-nope").unwrap().is_none());
        assert!(db.get_quest_by_slug("nope").unwrap().is_none());
    }

    #[test]
    fn a_duplicate_slug_is_not_retried_away() {
        let db = db();
        insert(&db, "same");
        let e = db
            .insert_quest(&Quest::new("same", "/tmp", "laptop"))
            .unwrap_err();
        assert_eq!(code_of(&e), "db");
        assert!(e.to_string().contains("slug"), "{e}");
    }

    #[test]
    fn a_colliding_id_is_retried() {
        let db = db();
        let first = insert(&db, "one");
        let mut clash = Quest::new("two", "/tmp", "laptop");
        clash.id = first.id.clone();
        let stored = db.insert_quest(&clash).unwrap();
        assert_ne!(stored.id, first.id);
        assert_eq!(db.get_quest(&stored.id).unwrap().unwrap().slug, "two");
    }

    #[test]
    fn list_hides_finished_unless_asked() {
        let db = db();
        let a = insert(&db, "alpha");
        let b = insert(&db, "beta");
        db.update_quest_state(&b.id, QuestState::Finished, Some(99))
            .unwrap();

        let open: Vec<String> = db
            .list_quests(false)
            .unwrap()
            .into_iter()
            .map(|q| q.slug)
            .collect();
        assert_eq!(open, ["alpha"]);
        assert_eq!(db.list_quests(true).unwrap().len(), 2);
        assert_eq!(db.get_quest(&a.id).unwrap().unwrap().slug, "alpha");
    }

    #[test]
    fn update_state_records_the_finish_time() {
        let db = db();
        let q = insert(&db, "alpha");
        let done = db
            .update_quest_state(&q.id, QuestState::Finished, Some(1234))
            .unwrap();
        assert_eq!(done.state, QuestState::Finished);
        assert_eq!(done.finished_at, Some(1234));

        let back = db
            .update_quest_state(&q.id, QuestState::Active, None)
            .unwrap();
        assert_eq!(back.state, QuestState::Active);
        assert_eq!(back.finished_at, None);
    }

    #[test]
    fn update_fields_touches_only_what_is_set() {
        let db = db();
        let q = insert(&db, "alpha");
        let patched = db
            .update_quest(
                &q.id,
                &QuestPatch {
                    goal: Some(Some("ship it".to_string())),
                    ctx_reset_pct: Some(Some(25)),
                    ..QuestPatch::default()
                },
            )
            .unwrap();
        assert_eq!(patched.goal.as_deref(), Some("ship it"));
        assert_eq!(patched.ctx_reset_pct, Some(25));
        assert_eq!(patched.slug, "alpha");
        assert_eq!(patched.cwd, q.cwd);

        let cleared = db
            .update_quest(
                &q.id,
                &QuestPatch {
                    ctx_reset_pct: Some(None),
                    ..QuestPatch::default()
                },
            )
            .unwrap();
        assert_eq!(cleared.ctx_reset_pct, None);
        assert_eq!(cleared.goal.as_deref(), Some("ship it"));

        let renamed = db
            .update_quest(
                &q.id,
                &QuestPatch {
                    slug: Some("omega".to_string()),
                    cwd: Some("/tmp/other".to_string()),
                    workflow: Some(Some("solo".to_string())),
                    ..QuestPatch::default()
                },
            )
            .unwrap();
        assert_eq!(renamed.slug, "omega");
        assert_eq!(renamed.cwd, "/tmp/other");
        assert_eq!(renamed.workflow.as_deref(), Some("solo"));

        let blanked = db
            .update_quest(
                &q.id,
                &QuestPatch {
                    goal: Some(None),
                    workflow: Some(None),
                    ..QuestPatch::default()
                },
            )
            .unwrap();
        assert_eq!(blanked.goal, None);
        assert_eq!(blanked.workflow, None);
    }

    #[test]
    fn an_empty_patch_is_a_no_op() {
        let db = db();
        let q = insert(&db, "alpha");
        assert_eq!(db.update_quest(&q.id, &QuestPatch::default()).unwrap(), q);
    }

    #[test]
    fn delete_takes_sessions_events_and_links_with_it() {
        let db = db();
        let q = insert(&db, "alpha");
        let other = insert(&db, "beta");
        let s = db
            .insert_session(&Session::new(
                &q.id,
                SessionRole::Master,
                "master",
                "q-alpha",
                "%1",
            ))
            .unwrap();
        db.append_event(&q.id, Some(&s.id), "note", &serde_json::json!({"t": 1}))
            .unwrap();
        db.append_event(&other.id, None, "note", &serde_json::Value::Null)
            .unwrap();

        db.delete_quest(&q.id).unwrap();
        assert!(db.get_quest(&q.id).unwrap().is_none());
        assert!(db.list_sessions_by_quest(&q.id).unwrap().is_empty());
        assert!(db.list_events_by_quest(&q.id, 10).unwrap().is_empty());
        // Untouched neighbours.
        assert!(db.get_quest(&other.id).unwrap().is_some());
        assert_eq!(db.list_events_by_quest(&other.id, 10).unwrap().len(), 1);
    }

    #[test]
    fn resolve_prefers_an_exact_match_over_a_prefix() {
        let db = db();
        let exact = insert(&db, "cdc");
        insert(&db, "cdc-backfill");
        assert_eq!(db.resolve_quest("cdc").unwrap().id, exact.id);
        assert_eq!(db.resolve_quest(&exact.id).unwrap().id, exact.id);
    }

    #[test]
    fn resolve_prefers_an_exact_id_over_an_exact_slug() {
        let db = db();
        let by_id = insert(&db, "alpha");
        // A slug may legally spell another Quest's id; the id still wins.
        let by_slug = db
            .insert_quest(&Quest::new(&by_id.id, "/tmp/repo", "laptop"))
            .unwrap();
        assert_ne!(by_id.id, by_slug.id);
        assert_eq!(db.resolve_quest(&by_id.id).unwrap().id, by_id.id);
        assert_eq!(db.resolve_quest("alpha").unwrap().id, by_id.id);
    }

    #[test]
    fn resolve_falls_back_to_a_unique_prefix_then_a_substring() {
        let db = db();
        let q = insert(&db, "cdc-backfill-retry");
        assert_eq!(db.resolve_quest("cdc-back").unwrap().id, q.id);
        assert_eq!(db.resolve_quest("backfill").unwrap().id, q.id);
        // An id prefix resolves too.
        assert_eq!(db.resolve_quest(&q.id[..4]).unwrap().id, q.id);
    }

    #[test]
    fn resolve_reports_candidates_when_ambiguous() {
        let db = db();
        let backfill = insert(&db, "cdc-backfill");
        insert(&db, "cdc-restore");
        let e = db.resolve_quest("cdc-").unwrap_err();
        assert_eq!(code_of(&e), "ambiguous");
        match e.downcast_ref::<QError>() {
            Some(QError::Ambiguous { target, candidates }) => {
                assert_eq!(target, "cdc-");
                assert_eq!(candidates.len(), 2);
                // Both halves: the id is what the user retypes, the slug is
                // what they recognise.
                assert!(
                    candidates.contains(&format!("{} (cdc-backfill)", backfill.id)),
                    "{candidates:?}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn resolve_reports_candidates_when_only_the_substring_stage_matches() {
        let db = db();
        insert(&db, "fix-cdc");
        insert(&db, "run-cdc");
        // Neither slug *starts* with the target, so only the substring stage hits.
        let e = db.resolve_quest("cdc").unwrap_err();
        assert_eq!(code_of(&e), "ambiguous");
    }

    #[test]
    fn resolve_reports_not_found() {
        let db = db();
        insert(&db, "alpha");
        for target in ["", "zzz"] {
            let e = db.resolve_quest(target).unwrap_err();
            assert_eq!(code_of(&e), "not_found", "target `{target}`");
        }
    }

    #[test]
    fn resolve_sees_finished_quests() {
        let db = db();
        let q = insert(&db, "alpha");
        db.update_quest_state(&q.id, QuestState::Finished, Some(1))
            .unwrap();
        assert_eq!(db.resolve_quest("alpha").unwrap().id, q.id);
    }

    #[test]
    fn name_cache_reads_back_and_overwrites() {
        let db = db();
        assert!(db.name_cache_get("hash").unwrap().is_none());
        db.name_cache_put("hash", "cdc-backfill", NameOrigin::Claude)
            .unwrap();
        assert_eq!(
            db.name_cache_get("hash").unwrap(),
            Some(CachedName {
                slug: "cdc-backfill".to_string(),
                source: NameOrigin::Claude,
            })
        );
        db.name_cache_put("hash", "cdc-restore", NameOrigin::Heuristic)
            .unwrap();
        assert_eq!(
            db.name_cache_get("hash").unwrap(),
            Some(CachedName {
                slug: "cdc-restore".to_string(),
                source: NameOrigin::Heuristic,
            })
        );
    }

    #[test]
    fn the_name_input_hash_is_patchable_and_clearable() {
        let db = db();
        let q = insert(&db, "alpha");
        assert_eq!(q.name_input_hash, None);
        let q = db
            .update_quest(
                &q.id,
                &QuestPatch {
                    name_input_hash: Some(Some("abc".to_string())),
                    ..QuestPatch::default()
                },
            )
            .unwrap();
        assert_eq!(q.name_input_hash.as_deref(), Some("abc"));
        let q = db
            .update_quest(
                &q.id,
                &QuestPatch {
                    name_input_hash: Some(None),
                    ..QuestPatch::default()
                },
            )
            .unwrap();
        assert_eq!(q.name_input_hash, None);
    }
}
