//! `template` table — reusable Quest definitions (SPEC §11).

use rusqlite::{Row, params};

use super::{Db, ID_ATTEMPTS, db_err, is_id_collision, json_col, json_val};
use crate::error::QError;
use crate::model::{Template, new_id, now};

const COLUMNS: &str = "id, name, description, cwd, workflow, goal, master_prompt, beads_repo, \
     create_brain, tags, run_count, last_run_at, created_at, updated_at";

impl Db {
    /// Inserts `template`, regenerating its id on collision. A duplicate `name`
    /// is the caller's error and is not retried.
    pub fn insert_template(&self, template: &Template) -> anyhow::Result<Template> {
        let tags = json_val(template.tags.as_ref())?;
        let mut row = template.clone();
        for attempt in 0..ID_ATTEMPTS {
            match self.try_insert_template(&row, tags.as_deref()) {
                Ok(()) => return Ok(row),
                Err(e) if is_id_collision(&e, "template") && attempt + 1 < ID_ATTEMPTS => {
                    row.id = new_id("t");
                }
                Err(e) => return Err(db_err(e)),
            }
        }
        unreachable!("the loop returns on the last attempt")
    }

    fn try_insert_template(&self, t: &Template, tags: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            &format!(
                "INSERT INTO template ({COLUMNS}) VALUES \
                 (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                t.id,
                t.name,
                t.description,
                t.cwd,
                t.workflow,
                t.goal,
                t.master_prompt,
                t.beads_repo,
                t.create_brain,
                tags,
                t.run_count,
                t.last_run_at,
                t.created_at,
                t.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_templates(&self) -> anyhow::Result<Vec<Template>> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT {COLUMNS} FROM template ORDER BY name"))
            .map_err(db_err)?;
        let rows = stmt.query_map([], row_to_template).map_err(db_err)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)
    }

    pub fn get_template(&self, id: &str) -> anyhow::Result<Option<Template>> {
        self.one_template("id", id)
    }

    pub fn get_template_by_name(&self, name: &str) -> anyhow::Result<Option<Template>> {
        self.one_template("name", name)
    }

    fn one_template(&self, column: &str, value: &str) -> anyhow::Result<Option<Template>> {
        self.conn
            .query_row(
                &format!("SELECT {COLUMNS} FROM template WHERE {column} = ?1"),
                [value],
                row_to_template,
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })
    }

    /// The template `target` names, by the rule SPEC §16 gives Quest targets:
    /// exact id, exact name, then unique prefix, then unique substring. Two
    /// matches at the same stage are ambiguous and are listed rather than
    /// guessed at.
    pub fn resolve_template(&self, target: &str) -> anyhow::Result<Template> {
        let target = target.trim();
        if target.is_empty() {
            return Err(QError::NotFound("template ``".to_string()).into());
        }
        let all = self.list_templates()?;
        if let Some(hit) = all.iter().find(|t| t.id == target).cloned() {
            return Ok(hit);
        }
        if let Some(hit) = all.iter().find(|t| t.name == target).cloned() {
            return Ok(hit);
        }
        for matches in [
            narrow(&all, |t| t.name.starts_with(target)),
            narrow(&all, |t| t.name.contains(target)),
        ] {
            match matches.len() {
                0 => continue,
                1 => return Ok(matches.into_iter().next().expect("length checked")),
                _ => {
                    return Err(QError::Ambiguous {
                        target: target.to_string(),
                        candidates: matches.into_iter().map(|t| t.name).collect(),
                    }
                    .into());
                }
            }
        }
        Err(QError::NotFound(format!("template `{target}` (known: {})", names(&all))).into())
    }

    /// Writes every definition column of `row` back, by id. `run_count` and
    /// `last_run_at` are not among them: they are history, and only
    /// [`Db::bump_template_run`] moves them.
    pub fn update_template(&self, id: &str, row: &Template) -> anyhow::Result<Template> {
        let tags = json_val(row.tags.as_ref())?;
        self.conn
            .execute(
                "UPDATE template SET name = ?1, description = ?2, cwd = ?3, workflow = ?4, \
                 goal = ?5, master_prompt = ?6, beads_repo = ?7, create_brain = ?8, tags = ?9, \
                 updated_at = ?10 WHERE id = ?11",
                params![
                    row.name,
                    row.description,
                    row.cwd,
                    row.workflow,
                    row.goal,
                    row.master_prompt,
                    row.beads_repo,
                    row.create_brain,
                    tags,
                    now(),
                    id,
                ],
            )
            .map_err(db_err)?;
        self.require_template(id)
    }

    /// One more run of this template, as of `at` (SPEC §11's `run_count` /
    /// `last_run_at`). `updated_at` is left alone: a run is not an edit, and
    /// the Templates tab sorts edits and runs by different columns.
    pub fn bump_template_run(&self, id: &str, at: i64) -> anyhow::Result<Template> {
        self.conn
            .execute(
                "UPDATE template SET run_count = run_count + 1, last_run_at = ?1 WHERE id = ?2",
                params![at, id],
            )
            .map_err(db_err)?;
        self.require_template(id)
    }

    /// Deletes the template and unlinks the Quests that point at it, returning
    /// how many were unlinked.
    ///
    /// `quest.template_id` is a real foreign key, so the row cannot simply go.
    /// Clearing it is the right answer rather than refusing the delete: a
    /// template is a *definition*, and a Quest that was made from one keeps the
    /// only record that matters — the `template_id` in its `quest.created`
    /// event — whatever happens to the definition afterwards.
    pub fn delete_template(&self, id: &str) -> anyhow::Result<usize> {
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        let unlinked = tx
            .execute(
                "UPDATE quest SET template_id = NULL WHERE template_id = ?1",
                [id],
            )
            .map_err(db_err)?;
        tx.execute("DELETE FROM template WHERE id = ?1", [id])
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(unlinked)
    }

    fn require_template(&self, id: &str) -> anyhow::Result<Template> {
        self.get_template(id)?
            .ok_or_else(|| QError::NotFound(format!("template `{id}`")).into())
    }
}

fn narrow(all: &[Template], keep: impl Fn(&Template) -> bool) -> Vec<Template> {
    all.iter().filter(|t| keep(t)).cloned().collect()
}

/// The template names, for a "not found" that says what there is instead.
fn names(all: &[Template]) -> String {
    if all.is_empty() {
        return "none".to_string();
    }
    all.iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn row_to_template(row: &Row) -> rusqlite::Result<Template> {
    Ok(Template {
        id: row.get("id")?,
        name: row.get("name")?,
        description: row.get("description")?,
        cwd: row.get("cwd")?,
        workflow: row.get("workflow")?,
        goal: row.get("goal")?,
        master_prompt: row.get("master_prompt")?,
        beads_repo: row.get("beads_repo")?,
        create_brain: row.get("create_brain")?,
        tags: json_col(row, "tags")?,
        run_count: row.get("run_count")?,
        last_run_at: row.get("last_run_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_list_round_trips_every_column() {
        let db = Db::open_in_memory().unwrap();
        let mut t = Template::new("weekly-hygiene");
        t.description = Some("routine".to_string());
        t.cwd = Some("/tmp/repo".to_string());
        t.workflow = Some("solo".to_string());
        t.goal = Some("tidy up {{date}}".to_string());
        t.master_prompt = Some("start with the lint report".to_string());
        t.beads_repo = Some("work".to_string());
        t.create_brain = true;
        t.tags = Some(vec!["routine".to_string(), "weekly".to_string()]);
        t.run_count = 7;
        t.last_run_at = Some(1234);

        let stored = db.insert_template(&t).unwrap();
        assert_eq!(stored, t);

        db.insert_template(&Template::new("audit-deps")).unwrap();
        let listed = db.list_templates().unwrap();
        let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["audit-deps", "weekly-hygiene"]);
        assert_eq!(listed[1], t);
    }

    #[test]
    fn a_colliding_id_is_retried_but_a_duplicate_name_is_not() {
        let db = Db::open_in_memory().unwrap();
        let first = db.insert_template(&Template::new("one")).unwrap();

        let mut clash = Template::new("two");
        clash.id = first.id.clone();
        let stored = db.insert_template(&clash).unwrap();
        assert_ne!(stored.id, first.id);

        let e = db.insert_template(&Template::new("one")).unwrap_err();
        assert!(e.to_string().contains("name"), "{e}");
    }

    #[test]
    fn an_empty_table_lists_nothing() {
        let db = Db::open_in_memory().unwrap();
        assert!(db.list_templates().unwrap().is_empty());
    }

    #[test]
    fn a_template_resolves_by_id_name_prefix_and_substring() {
        let db = Db::open_in_memory().unwrap();
        let hygiene = db
            .insert_template(&Template::new("weekly-hygiene"))
            .unwrap();
        db.insert_template(&Template::new("weekly-report")).unwrap();
        db.insert_template(&Template::new("deps-audit")).unwrap();

        assert_eq!(db.resolve_template(&hygiene.id).unwrap().id, hygiene.id);
        assert_eq!(
            db.resolve_template("weekly-hygiene").unwrap().id,
            hygiene.id
        );
        assert_eq!(db.resolve_template("weekly-h").unwrap().id, hygiene.id);
        assert_eq!(db.resolve_template("audit").unwrap().name, "deps-audit");

        let e = db.resolve_template("weekly").unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("ambiguous"),
            "{e}"
        );
        assert!(e.to_string().contains("weekly-report"), "{e}");

        let e = db.resolve_template("nope").unwrap_err();
        assert!(e.to_string().contains("deps-audit"), "{e}");
        assert!(db.resolve_template("  ").is_err());
    }

    #[test]
    fn updating_writes_the_definition_and_leaves_the_run_stats_alone() {
        let db = Db::open_in_memory().unwrap();
        let mut stored = db.insert_template(&Template::new("routine")).unwrap();
        let bumped = db.bump_template_run(&stored.id, 4242).unwrap();
        assert_eq!((bumped.run_count, bumped.last_run_at), (1, Some(4242)));

        stored.name = "renamed".to_string();
        stored.goal = Some("do the thing".to_string());
        stored.tags = Some(vec!["weekly".to_string()]);
        stored.create_brain = true;
        // Run stats on the value passed in are ignored, not written.
        stored.run_count = 999;
        stored.last_run_at = Some(1);
        let after = db.update_template(&bumped.id, &stored).unwrap();

        assert_eq!(after.name, "renamed");
        assert_eq!(after.goal.as_deref(), Some("do the thing"));
        assert_eq!(after.tags, Some(vec!["weekly".to_string()]));
        assert!(after.create_brain);
        assert_eq!((after.run_count, after.last_run_at), (1, Some(4242)));
    }

    #[test]
    fn every_run_counts() {
        let db = Db::open_in_memory().unwrap();
        let t = db.insert_template(&Template::new("routine")).unwrap();
        for (n, at) in [(1, 10), (2, 20), (3, 30)] {
            let after = db.bump_template_run(&t.id, at).unwrap();
            assert_eq!((after.run_count, after.last_run_at), (n, Some(at)));
        }
    }

    #[test]
    fn deleting_a_template_unlinks_the_quests_that_came_from_it() {
        let db = Db::open_in_memory().unwrap();
        let t = db.insert_template(&Template::new("routine")).unwrap();
        let mut quest = crate::model::Quest::new("alpha", "/tmp", "laptop");
        quest.template_id = Some(t.id.clone());
        let quest = db.insert_quest(&quest).unwrap();
        db.insert_quest(&crate::model::Quest::new("beta", "/tmp", "laptop"))
            .unwrap();

        assert_eq!(db.delete_template(&t.id).unwrap(), 1);
        assert!(db.get_template(&t.id).unwrap().is_none());
        // The Quest survives the definition it was made from.
        assert_eq!(db.get_quest(&quest.id).unwrap().unwrap().template_id, None);
        assert_eq!(db.list_quests(true).unwrap().len(), 2);
    }
}
