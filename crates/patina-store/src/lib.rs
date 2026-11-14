//! SQLite storage: schema, migrations and the connection setup.
use rusqlite::Connection;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("database schema version {0} is newer than this build supports ({1})")]
    TooNew(i64, i64),
}

/// Each entry is one migration; `PRAGMA user_version` records how many have run.
const MIGRATIONS: &[&str] = &[
    // 1: initial schema (section 6 of the brief)
    "CREATE TABLE runs(
        id TEXT PRIMARY KEY, project_id TEXT NOT NULL, team_id TEXT NOT NULL,
        prompt TEXT NOT NULL, status TEXT NOT NULL, budgets_json TEXT NOT NULL,
        usage_json TEXT NOT NULL, worktree_path TEXT, branch TEXT, base_commit TEXT,
        created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
     CREATE TABLE events(
        run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE, seq INTEGER NOT NULL,
        ts INTEGER NOT NULL, agent_id TEXT, type TEXT NOT NULL, payload_json TEXT NOT NULL,
        PRIMARY KEY (run_id, seq));
     CREATE TABLE messages(
        run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE, agent_id TEXT NOT NULL,
        idx INTEGER NOT NULL, role TEXT NOT NULL, parts_json TEXT NOT NULL,
        PRIMARY KEY (run_id, agent_id, idx));
     CREATE TABLE approvals(
        id TEXT PRIMARY KEY, run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
        seq INTEGER NOT NULL, tool_call_json TEXT NOT NULL, status TEXT NOT NULL,
        decision_json TEXT, decided_at INTEGER);
     CREATE TABLE checkpoints(
        run_id TEXT NOT NULL REFERENCES runs(id) ON DELETE CASCADE, step INTEGER NOT NULL,
        commit_sha TEXT NOT NULL, summary TEXT NOT NULL, created_at INTEGER NOT NULL,
        PRIMARY KEY (run_id, step));
     CREATE TABLE secrets(name TEXT PRIMARY KEY, nonce BLOB NOT NULL, ciphertext BLOB NOT NULL);
     CREATE TABLE sessions(
        token_hash TEXT PRIMARY KEY, csrf_token TEXT NOT NULL,
        created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);",
];

pub fn schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// Open (or create) the database in WAL mode with foreign keys on, and migrate it.
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<(), StoreError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let target = schema_version();
    if current > target {
        return Err(StoreError::TooNew(current, target));
    }
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tables(c: &Connection) -> Vec<String> {
        let mut s = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        s.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn creates_schema_in_wal_mode() {
        let t = tempfile::tempdir().unwrap();
        let c = open(&t.path().join("p.db")).unwrap();
        for name in [
            "approvals",
            "checkpoints",
            "events",
            "messages",
            "runs",
            "secrets",
            "sessions",
        ] {
            assert!(tables(&c).contains(&name.to_string()), "missing {name}");
        }
        let mode: String = c
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = c
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn reopening_is_idempotent() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("p.db");
        drop(open(&p).unwrap());
        let c = open(&p).unwrap();
        let v: i64 = c
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, schema_version());
    }

    #[test]
    fn newer_database_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("p.db");
        let c = Connection::open(&p).unwrap();
        c.pragma_update(None, "user_version", 99).unwrap();
        drop(c);
        assert!(matches!(open(&p), Err(StoreError::TooNew(99, _))));
    }
}
