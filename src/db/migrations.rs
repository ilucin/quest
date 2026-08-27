//! Versioned schema migrations, tracked in `PRAGMA user_version`.
//!
//! Append a `(version, sql)` pair — never edit a shipped one — and bump
//! `SCHEMA_VERSION`. `q doctor` reports the version the database is on.

use rusqlite::{Connection, TransactionBehavior};

use crate::error::QError;

/// The version this binary expects. Must equal the last entry in `MIGRATIONS`.
pub const SCHEMA_VERSION: u32 = 4;

const MIGRATIONS: &[(u32, &str)] = &[(1, V1), (2, V2), (3, V3), (4, V4)];

const V1: &str = r#"
CREATE TABLE quest (
  id            TEXT PRIMARY KEY,          -- 'q-7f3a' (4 hex, collisions are retried)
  slug          TEXT NOT NULL UNIQUE,      -- 'cdc-backfill-retry' (kebab, <=40)
  name_source   TEXT NOT NULL,             -- 'manual' | 'auto' | 'template'
  name_input_hash TEXT,                    -- hash of the auto-naming input (cache invalidation)
  goal          TEXT,                      -- free text, 1-3 sentences
  cwd           TEXT NOT NULL,
  machine       TEXT NOT NULL,             -- from config [machine].name
  state         TEXT NOT NULL,             -- 'active' | 'finished'  (idle is derived)
  workflow      TEXT,                      -- workflow name
  template_id   TEXT REFERENCES template(id),
  beads_epic    TEXT,                      -- 'bd-123'
  beads_repo    TEXT,                      -- value of the repo:<name> label
  brain_session TEXT,                      -- brain session slug
  ctx_reset_pct INTEGER,                   -- master threshold override (NULL = config)
  created_at    INTEGER NOT NULL, updated_at INTEGER NOT NULL,
  finished_at   INTEGER
);

CREATE TABLE session (
  id            TEXT PRIMARY KEY,          -- 's-3b9c'
  quest_id      TEXT NOT NULL REFERENCES quest(id),
  role          TEXT NOT NULL,             -- 'master' | 'worker'
  label         TEXT NOT NULL,             -- 'master' | 'w1-tests' (= tmux window name)
  tmux_session  TEXT NOT NULL,             -- 'q-<slug>'
  tmux_pane     TEXT NOT NULL,             -- '%42' — the identity
  claude_pid    INTEGER,                   -- last known
  claude_session_id TEXT,                  -- last known (changes across /clear)
  claude_name   TEXT,
  workflow      TEXT,                      -- when it differs from the quest's
  phase         TEXT,                      -- self-reported: 'planning', 'implementing', …
  status        TEXT NOT NULL,             -- 'starting'|'busy'|'idle'|'waiting'|'ended'
  waiting_for   TEXT,                      -- 'permission' | 'input' | …
  ctx_pct       INTEGER,                   -- last known context window %
  ctx_updated_at INTEGER,
  first_prompt  TEXT,
  last_prompt   TEXT,
  started_at    INTEGER NOT NULL, ended_at INTEGER, updated_at INTEGER NOT NULL
);

CREATE TABLE event (
  id        INTEGER PRIMARY KEY,
  quest_id  TEXT NOT NULL,
  session_id TEXT,
  ts        INTEGER NOT NULL,
  kind      TEXT NOT NULL,   -- 'quest.created','session.start','session.stop','session.waiting',
                             -- 'session.prompt','session.compact','session.reset','session.end',
                             -- 'phase','link.added','artifact.added','note','name.changed',…
  payload   TEXT             -- JSON
);
CREATE INDEX event_quest_ts ON event(quest_id, ts);

CREATE TABLE link (
  id        INTEGER PRIMARY KEY,
  quest_id  TEXT NOT NULL,
  session_id TEXT,                 -- who added it (NULL = manual/CLI)
  kind      TEXT NOT NULL,         -- 'pr'|'task'|'worktree'|'artifact'|'url'|'brain'|'beads'|'branch'
  ref       TEXT NOT NULL,         -- URL or path
  title     TEXT,                  -- enrichment cache
  meta      TEXT,                  -- JSON enrichment (pr state, ci, task status…)
  enriched_at INTEGER,
  created_at INTEGER NOT NULL,
  UNIQUE(quest_id, kind, ref)
);

CREATE TABLE template (
  id          TEXT PRIMARY KEY,    -- 't-1a2b'
  name        TEXT NOT NULL UNIQUE,
  description TEXT,
  cwd         TEXT,                -- NULL = the cwd at run time
  workflow    TEXT,
  goal        TEXT,                -- supports {{date}}, {{arg}} placeholders
  master_prompt TEXT,              -- the master's first prompt
  beads_repo  TEXT,
  create_brain INTEGER NOT NULL DEFAULT 0,
  tags        TEXT,                -- JSON array
  run_count   INTEGER NOT NULL DEFAULT 0,
  last_run_at INTEGER,
  created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL
);

CREATE TABLE name_cache (          -- auto-naming: input_hash -> slug
  input_hash TEXT PRIMARY KEY, slug TEXT NOT NULL, created_at INTEGER NOT NULL
);
"#;

/// SPEC §8 wants a per-Quest `auto_reset` override, which SPEC §4 never gave a
/// column. Nullable like `ctx_reset_pct`: NULL follows `[context] auto_reset`.
const V2: &str = r#"
ALTER TABLE quest ADD COLUMN auto_reset INTEGER;   -- 1 | 0 | NULL = config
"#;

/// Auto-naming (SPEC §10): where a cached proposal came from, and the
/// `/rename` a live Claude session still owes because it was busy when its
/// Quest was renamed.
const V3: &str = r#"
ALTER TABLE name_cache ADD COLUMN source TEXT NOT NULL DEFAULT 'claude';
ALTER TABLE session ADD COLUMN pending_rename TEXT;
"#;

/// Multi-machine (SPEC §15): the last answer each remote gave to
/// `q list --json --no-remote`, so an unreachable machine still shows its
/// last-known Quests instead of vanishing from the listing.
const V4: &str = r#"
CREATE TABLE remote_cache (
  name       TEXT PRIMARY KEY,   -- remotes[].name from the config
  payload    TEXT NOT NULL,      -- JSON array of quest views, as the remote sent it
  fetched_at INTEGER NOT NULL
);
"#;

pub fn user_version(conn: &Connection) -> anyhow::Result<u32> {
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(super::db_err)?;
    u32::try_from(version)
        .map_err(|_| QError::Db(format!("nonsensical schema version {version}")).into())
}

/// Applies every migration newer than the database's `user_version`, in one
/// transaction. A database from a newer binary is left untouched.
///
/// Several `q` processes may open the same file at once, so the transaction
/// takes its write lock up front (`IMMEDIATE`) and the version is re-read
/// inside it: whatever a concurrent process already applied is skipped.
pub fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    if let Some(done) = settled(user_version(conn)?)? {
        return Ok(done);
    }

    let tx = conn
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(super::db_err)?;
    let current = user_version(&tx)?;
    if let Some(done) = settled(current)? {
        return Ok(done);
    }

    for (version, sql) in MIGRATIONS {
        if *version > current {
            tx.execute_batch(sql)
                .map_err(|e| QError::Db(format!("migration to version {version} failed: {e}")))?;
        }
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)
        .map_err(super::db_err)?;
    tx.commit().map_err(super::db_err)?;
    Ok(())
}

/// `Some(())` when `current` needs no migration, `None` when it does. A
/// database from a newer binary is an error rather than something to downgrade.
fn settled(current: u32) -> anyhow::Result<Option<()>> {
    if current > SCHEMA_VERSION {
        return Err(QError::Db(format!(
            "database schema is version {current}, but this q understands {SCHEMA_VERSION}; upgrade q"
        ))
        .into());
    }
    Ok((current == SCHEMA_VERSION).then_some(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_version_matches_the_last_migration() {
        let last = MIGRATIONS.last().expect("at least one migration").0;
        assert_eq!(last, SCHEMA_VERSION);
    }

    #[test]
    fn migration_versions_are_contiguous_from_one() {
        for (i, (version, _)) in MIGRATIONS.iter().enumerate() {
            assert_eq!(*version, i as u32 + 1, "gap at index {i}");
        }
    }

    #[test]
    fn concurrent_opens_all_succeed_on_a_fresh_database() {
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.db");
        let gate = Arc::new(Barrier::new(6));

        let racers: Vec<_> = (0..6)
            .map(|_| {
                let path = path.clone();
                let gate = Arc::clone(&gate);
                std::thread::spawn(move || {
                    gate.wait();
                    let db = crate::db::Db::open(&path)?;
                    Ok::<_, anyhow::Error>((db.schema_version()?, db.journal_mode()?))
                })
            })
            .collect();

        for racer in racers {
            let (version, journal) = racer.join().expect("thread panicked").expect("open failed");
            assert_eq!(version, SCHEMA_VERSION);
            assert_eq!(journal.to_lowercase(), "wal");
        }
    }
}
