//! `template` table — reusable Quest definitions (SPEC §11).

use rusqlite::{Row, params};

use super::{Db, ID_ATTEMPTS, db_err, is_id_collision, json_col, json_val};
use crate::model::{Template, new_id};

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
}
