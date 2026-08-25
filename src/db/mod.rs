//! SQLite storage: one file per machine, one repo module per table (SPEC §4).
//!
//! Most of this surface is used by the commands that land in later milestones;
//! the M0 binary only opens the database.
#![allow(dead_code)]

pub mod event;
pub mod link;
pub mod migrations;
pub mod quest;
pub mod session;
pub mod template;

use std::path::{Path, PathBuf};
use std::str::FromStr;

use rusqlite::types::Type;
use rusqlite::{Connection, Row};
use serde::de::DeserializeOwned;

use crate::error::QError;

/// Owns the connection. Hook writes from N sessions land here concurrently, so
/// the file is opened in WAL mode with a busy timeout.
#[derive(Debug)]
pub struct Db {
    conn: Connection,
}

impl Db {
    /// `$Q_DB`, else `~/.local/share/q/q.db`.
    pub fn path() -> anyhow::Result<PathBuf> {
        path_from(std::env::var_os("Q_DB"))
    }

    pub fn open_default() -> anyhow::Result<Db> {
        Db::open(&Db::path()?)
    }

    pub fn open(path: &Path) -> anyhow::Result<Db> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| QError::Db(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| QError::Db(format!("cannot open {}: {e}", path.display())))?;
        Db::prepare(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Db> {
        Db::prepare(Connection::open_in_memory().map_err(db_err)?)
    }

    fn prepare(mut conn: Connection) -> anyhow::Result<Db> {
        // In-memory databases stay on the "memory" journal; that is expected.
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(()))
            .map_err(db_err)?;
        conn.pragma_update(None, "busy_timeout", 5000)
            .map_err(db_err)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(db_err)?;
        migrations::migrate(&mut conn)?;
        Ok(Db { conn })
    }

    pub fn schema_version(&self) -> anyhow::Result<u32> {
        migrations::user_version(&self.conn)
    }

    pub fn journal_mode(&self) -> anyhow::Result<String> {
        self.conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(db_err)
    }
}

fn path_from(env: Option<std::ffi::OsString>) -> anyhow::Result<PathBuf> {
    if let Some(raw) = env
        && !raw.is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| QError::Db("cannot determine the home directory".to_string()))?;
    Ok(home.join(".local").join("share").join("q").join("q.db"))
}

pub(crate) fn db_err(e: rusqlite::Error) -> anyhow::Error {
    QError::Db(e.to_string()).into()
}

/// True when a UNIQUE/PK violation is about `<table>.id`, i.e. a generated id
/// collided and the insert is worth retrying. Any other constraint (a duplicate
/// slug, say) is the caller's problem.
pub(crate) fn is_id_collision(e: &rusqlite::Error, table: &str) -> bool {
    match e {
        rusqlite::Error::SqliteFailure(f, Some(msg)) => {
            f.code == rusqlite::ErrorCode::ConstraintViolation
                && msg.contains(&format!("{table}.id"))
        }
        _ => false,
    }
}

/// Number of fresh ids tried before an insert gives up.
pub(crate) const ID_ATTEMPTS: usize = 5;

/// Reads a TEXT column into one of `model`'s enums.
pub(crate) fn enum_col<T>(row: &Row, name: &str) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let raw: String = row.get(name)?;
    T::from_str(&raw)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e)))
}

/// Reads a nullable TEXT column holding JSON.
pub(crate) fn json_col<T: DeserializeOwned>(row: &Row, name: &str) -> rusqlite::Result<Option<T>> {
    let raw: Option<String> = row.get(name)?;
    match raw {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(e))),
    }
}

/// Serializes a value for a JSON TEXT column; `null` is stored as SQL NULL.
pub(crate) fn json_val<T: serde::Serialize>(value: Option<&T>) -> anyhow::Result<Option<String>> {
    match value {
        None => Ok(None),
        Some(v) => {
            let text = serde_json::to_string(v)?;
            Ok(if text == "null" { None } else { Some(text) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use migrations::SCHEMA_VERSION;

    #[test]
    fn a_fresh_database_is_at_the_current_schema_version() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn every_table_exists() {
        let db = Db::open_in_memory().unwrap();
        for table in [
            "quest",
            "session",
            "event",
            "link",
            "template",
            "name_cache",
        ] {
            let count: i64 = db
                .conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "missing table {table}");
        }
        let index: i64 = db
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'event_quest_ts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index, 1, "missing index event_quest_ts");
    }

    #[test]
    fn opening_a_file_twice_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("q.db");
        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert_eq!(db.journal_mode().unwrap().to_lowercase(), "wal");
        drop(db);

        let again = Db::open(&path).unwrap();
        assert_eq!(again.schema_version().unwrap(), SCHEMA_VERSION);
        // A second open must not re-run migrations over existing tables.
        let tables: i64 = again
            .conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'quest'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn open_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("q.db");
        Db::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn a_newer_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.db");
        Db::open(&path).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 7)
                .unwrap();
        }
        let e = Db::open(&path).unwrap_err();
        assert_eq!(
            e.downcast_ref::<QError>().map(QError::code),
            Some("db"),
            "{e}"
        );
        assert!(e.to_string().contains("upgrade q"), "{e}");
    }

    #[test]
    fn path_prefers_the_env_override() {
        use std::ffi::OsString;
        let p = path_from(Some(OsString::from("/tmp/q-test/q.db"))).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/q-test/q.db"));
        for empty in [Some(OsString::new()), None] {
            let p = path_from(empty).unwrap();
            assert!(p.ends_with(".local/share/q/q.db"), "{}", p.display());
        }
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Db::open_in_memory().unwrap();
        let e = db.conn.execute(
            "INSERT INTO session (id, quest_id, role, label, tmux_session, tmux_pane, status, started_at, updated_at)
             VALUES ('s-0001', 'q-nope', 'worker', 'w1', 'q-x', '%1', 'idle', 1, 1)",
            [],
        );
        assert!(e.is_err(), "orphan session was accepted");
    }
}
