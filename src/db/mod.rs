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
        Db::open_with_timeout(path, BUSY_TIMEOUT_MS)
    }

    /// `open` with its own lock budget, for callers that would rather skip
    /// than stall — the statusline runs after every message. Opening a
    /// database, WAL switch and migrations included, waits at most `busy_ms`
    /// per statement.
    pub fn open_with_timeout(path: &Path, busy_ms: u32) -> anyhow::Result<Db> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| QError::Db(format!("cannot create {}: {e}", parent.display())))?;
        }
        let conn = Connection::open(path)
            .map_err(|e| QError::Db(format!("cannot open {}: {e}", path.display())))?;
        Db::prepare(conn, busy_ms)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Db> {
        Db::prepare(
            Connection::open_in_memory().map_err(db_err)?,
            BUSY_TIMEOUT_MS,
        )
    }

    fn prepare(mut conn: Connection, busy_ms: u32) -> anyhow::Result<Db> {
        // Before anything that can contend, so every later statement waits its
        // turn instead of failing.
        conn.pragma_update(None, "busy_timeout", busy_ms)
            .map_err(db_err)?;
        set_wal(&conn, busy_ms)?;
        conn.pragma_update(None, "foreign_keys", true)
            .map_err(db_err)?;
        migrations::migrate(&mut conn)?;
        Ok(Db { conn })
    }

    /// Runs `f` inside one `BEGIN IMMEDIATE` transaction: the write lock is
    /// taken up front (or the busy timeout hits before anything is written),
    /// and every statement in `f` commits or none does.
    pub fn transaction<T>(&self, f: impl FnOnce(&Db) -> anyhow::Result<T>) -> anyhow::Result<T> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(db_err)?;
        let out = f(self)?;
        tx.commit().map_err(db_err)?;
        Ok(out)
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

/// How long any statement waits on a lock held by another `q`, by default.
const BUSY_TIMEOUT_MS: u32 = 5000;

/// Switches the file to WAL. In-memory databases stay on the "memory" journal;
/// that is expected.
///
/// The mode change needs a brief exclusive lock and, unlike ordinary
/// statements, is refused outright rather than routed through the busy handler
/// — so N processes opening the same fresh file have to be retried by hand.
fn set_wal(conn: &Connection, busy_ms: u32) -> anyhow::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(u64::from(busy_ms));
    loop {
        match conn.pragma_update_and_check(None, "journal_mode", "WAL", |_| Ok(())) {
            Ok(()) => return Ok(()),
            Err(e) if is_busy(&e) && std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(e) => return Err(db_err(e)),
        }
    }
}

fn is_busy(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::DatabaseBusy
                || f.code == rusqlite::ErrorCode::DatabaseLocked
    )
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

/// A `Row` does not expose its statement, so the column index rusqlite wants is
/// out of reach; `usize::MAX` is its "unknown column" sentinel, which drops the
/// index from the message and leaves the name below to identify the column.
const UNKNOWN_COLUMN: usize = usize::MAX;

/// Names the offending column in a row-conversion failure.
#[derive(Debug)]
struct ColumnError {
    column: String,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for ColumnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "column `{}`: {}", self.column, self.source)
    }
}

impl std::error::Error for ColumnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

fn column_err(
    name: &str,
    ty: Type,
    e: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        UNKNOWN_COLUMN,
        ty,
        Box::new(ColumnError {
            column: name.to_string(),
            source: e.into(),
        }),
    )
}

/// Reads a TEXT column into one of `model`'s enums.
pub(crate) fn enum_col<T>(row: &Row, name: &str) -> rusqlite::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    let raw: String = row.get(name)?;
    T::from_str(&raw).map_err(|e| column_err(name, Type::Text, e))
}

/// Reads a nullable INTEGER column into a `u8` — the percentage columns.
pub(crate) fn u8_col(row: &Row, name: &str) -> rusqlite::Result<Option<u8>> {
    let raw: Option<i64> = row.get(name)?;
    raw.map(|v| u8::try_from(v).map_err(|e| column_err(name, Type::Integer, e)))
        .transpose()
}

/// Reads a nullable INTEGER column holding a boolean — anything non-zero is
/// true, so a hand-edited `2` still reads as "on".
pub(crate) fn bool_col(row: &Row, name: &str) -> rusqlite::Result<Option<bool>> {
    let raw: Option<i64> = row.get(name)?;
    Ok(raw.map(|v| v != 0))
}

/// Reads a nullable TEXT column holding JSON.
pub(crate) fn json_col<T: DeserializeOwned>(row: &Row, name: &str) -> rusqlite::Result<Option<T>> {
    let raw: Option<String> = row.get(name)?;
    match raw {
        None => Ok(None),
        Some(text) => serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| column_err(name, Type::Text, e)),
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
    fn a_bad_column_value_names_the_column() {
        let db = Db::open_in_memory().unwrap();
        db.insert_quest(&crate::model::Quest::new("alpha", "/tmp", "laptop"))
            .unwrap();

        db.conn
            .execute("UPDATE quest SET state = 'napping'", [])
            .unwrap();
        let e = db.list_quests(true).unwrap_err();
        assert!(e.to_string().contains("column `state`"), "{e}");
        assert!(e.to_string().contains("napping"), "{e}");

        db.conn
            .execute("UPDATE quest SET state = 'active', ctx_reset_pct = 999", [])
            .unwrap();
        let e = db.list_quests(true).unwrap_err();
        assert!(e.to_string().contains("column `ctx_reset_pct`"), "{e}");
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
