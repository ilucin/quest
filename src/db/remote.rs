//! `remote_cache` table — the last good answer from each remote (SPEC §15).
//!
//! One row per configured remote, overwritten on every successful fan-out. The
//! payload is stored verbatim as JSON so a listing can be rendered from it
//! without the remote being up — verbatim rather than re-serialized from the
//! parsed views, so that a field a newer `q` at the far end sent and this one
//! does not know survives the round trip.

use rusqlite::params;

use super::{Db, db_err};

/// A remote's last successful `q list --json --no-remote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCache {
    pub payload: String,
    pub fetched_at: i64,
}

impl Db {
    /// Replaces the cached response for `name`.
    pub fn put_remote_cache(
        &self,
        name: &str,
        payload: &str,
        fetched_at: i64,
    ) -> anyhow::Result<()> {
        self.conn
            .execute(
                "INSERT INTO remote_cache (name, payload, fetched_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET payload = excluded.payload,
                                                fetched_at = excluded.fetched_at",
                params![name, payload, fetched_at],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn get_remote_cache(&self, name: &str) -> anyhow::Result<Option<RemoteCache>> {
        self.conn
            .query_row(
                "SELECT payload, fetched_at FROM remote_cache WHERE name = ?1",
                [name],
                |row| {
                    Ok(RemoteCache {
                        payload: row.get("payload")?,
                        fetched_at: row.get("fetched_at")?,
                    })
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(db_err(other)),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_is_stored_and_read_back() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.get_remote_cache("ws").unwrap(), None);

        db.put_remote_cache("ws", "[]", 100).unwrap();
        assert_eq!(
            db.get_remote_cache("ws").unwrap(),
            Some(RemoteCache {
                payload: "[]".to_string(),
                fetched_at: 100
            })
        );
    }

    #[test]
    fn a_second_response_replaces_the_first() {
        let db = Db::open_in_memory().unwrap();
        db.put_remote_cache("ws", "[]", 100).unwrap();
        db.put_remote_cache("ws", "[{}]", 200).unwrap();
        db.put_remote_cache("other", "[]", 150).unwrap();

        let cached = db.get_remote_cache("ws").unwrap().unwrap();
        assert_eq!((cached.payload.as_str(), cached.fetched_at), ("[{}]", 200));
        assert_eq!(
            db.get_remote_cache("other").unwrap().unwrap().fetched_at,
            150
        );
    }
}
