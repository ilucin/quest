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

    /// Drops cache rows a live round can no longer refresh (bd-8lz.5.5): any
    /// remote no longer in `keep` (dropped from the config), and any row older
    /// than `min_fetched_at` (past the age cap). `keep` is the *whole* configured
    /// roster, never a `--machine`-narrowed subset, so a scoped round cannot
    /// evict the others' rows. Returns how many rows were deleted.
    pub fn prune_remote_cache(&self, keep: &[&str], min_fetched_at: i64) -> anyhow::Result<usize> {
        let aged = self
            .conn
            .execute(
                "DELETE FROM remote_cache WHERE fetched_at < ?1",
                params![min_fetched_at],
            )
            .map_err(db_err)?;
        let names: Vec<String> = self
            .conn
            .prepare("SELECT name FROM remote_cache")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .map_err(db_err)?;
        let mut dropped = 0;
        for name in names.iter().filter(|n| !keep.contains(&n.as_str())) {
            dropped += self
                .conn
                .execute("DELETE FROM remote_cache WHERE name = ?1", params![name])
                .map_err(db_err)?;
        }
        Ok(aged + dropped)
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

    #[test]
    fn prune_drops_unconfigured_remotes_and_keeps_the_rest() {
        let db = Db::open_in_memory().unwrap();
        db.put_remote_cache("ws", "[]", 1000).unwrap();
        db.put_remote_cache("old-box", "[]", 1000).unwrap();

        // `old-box` is no longer in the config; a scoped round that names only
        // `ws` must still not evict rows it did not ask about, so `keep` is the
        // whole roster and both survive.
        assert_eq!(db.prune_remote_cache(&["ws", "old-box"], 0).unwrap(), 0);
        // Dropped from the config: gone; `ws` stays.
        assert_eq!(db.prune_remote_cache(&["ws"], 0).unwrap(), 1);
        assert!(db.get_remote_cache("old-box").unwrap().is_none());
        assert!(db.get_remote_cache("ws").unwrap().is_some());
    }

    #[test]
    fn prune_drops_rows_past_the_age_cap() {
        let db = Db::open_in_memory().unwrap();
        db.put_remote_cache("ws", "[]", 100).unwrap();
        db.put_remote_cache("fresh", "[]", 5000).unwrap();

        // Everything fetched before 1000 goes, whatever its name.
        assert_eq!(db.prune_remote_cache(&["ws", "fresh"], 1000).unwrap(), 1);
        assert!(db.get_remote_cache("ws").unwrap().is_none());
        assert!(db.get_remote_cache("fresh").unwrap().is_some());
    }
}
