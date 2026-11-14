//! Runs, their event log and each agent's message history.
use crate::{Db, StoreError};
use patina_core::events::{Event, now_ms};
use patina_core::provider::Message;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RunRow {
    pub id: String,
    pub project_id: String,
    pub team_id: String,
    pub prompt: String,
    pub status: String,
    pub usage: Value,
    pub created_at: i64,
    pub updated_at: i64,
}

fn run_from_row(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    let usage: String = r.get(5)?;
    Ok(RunRow {
        id: r.get(0)?,
        project_id: r.get(1)?,
        team_id: r.get(2)?,
        prompt: r.get(3)?,
        status: r.get(4)?,
        usage: serde_json::from_str(&usage).unwrap_or(Value::Null),
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

const RUN_COLS: &str =
    "id, project_id, team_id, prompt, status, usage_json, created_at, updated_at";

pub async fn create_run(
    db: &Db,
    id: &str,
    project_id: &str,
    team_id: &str,
    prompt: &str,
    budgets: &Value,
) -> Result<(), StoreError> {
    let (id, project_id, team_id, prompt, budgets) = (
        id.to_string(),
        project_id.to_string(),
        team_id.to_string(),
        prompt.to_string(),
        budgets.to_string(),
    );
    db.call(move |c| {
        let now = now_ms();
        c.execute(
            "INSERT INTO runs(id, project_id, team_id, prompt, status, budgets_json, usage_json, created_at, updated_at)
             VALUES(?1, ?2, ?3, ?4, 'queued', ?5, '{}', ?6, ?6)",
            params![id, project_id, team_id, prompt, budgets, now],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_status(db: &Db, id: &str, status: &str) -> Result<(), StoreError> {
    let (id, status) = (id.to_string(), status.to_string());
    db.call(move |c| {
        c.execute(
            "UPDATE runs SET status = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, status, now_ms()],
        )?;
        Ok(())
    })
    .await
}

pub async fn set_usage(db: &Db, id: &str, usage: &Value) -> Result<(), StoreError> {
    let (id, usage) = (id.to_string(), usage.to_string());
    db.call(move |c| {
        c.execute(
            "UPDATE runs SET usage_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, usage, now_ms()],
        )?;
        Ok(())
    })
    .await
}

pub async fn get_run(db: &Db, id: &str) -> Result<Option<RunRow>, StoreError> {
    let id = id.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            &format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"),
            [id],
            run_from_row,
        )
        .optional()?)
    })
    .await
}

pub async fn list_runs(db: &Db, limit: i64) -> Result<Vec<RunRow>, StoreError> {
    db.call(move |c| {
        let mut s = c.prepare(&format!(
            "SELECT {RUN_COLS} FROM runs ORDER BY id DESC LIMIT ?1"
        ))?;
        let rows = s
            .query_map([limit], run_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Append one event, assigning the next sequence number. Safe because one thread owns the connection.
pub fn append_event_tx(
    c: &Connection,
    run_id: &str,
    agent_id: Option<&str>,
    kind: &str,
    payload: &Value,
) -> Result<Event, StoreError> {
    let seq: i64 = c.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE run_id = ?1",
        [run_id],
        |r| r.get(0),
    )?;
    let ts = now_ms();
    c.execute(
        "INSERT INTO events(run_id, seq, ts, agent_id, type, payload_json) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![run_id, seq, ts, agent_id, kind, payload.to_string()],
    )?;
    c.execute(
        "UPDATE runs SET updated_at = ?2 WHERE id = ?1",
        params![run_id, ts],
    )?;
    Ok(Event {
        run_id: run_id.into(),
        seq,
        ts,
        agent_id: agent_id.map(str::to_string),
        kind: kind.into(),
        payload: payload.clone(),
    })
}

pub async fn append_event(
    db: &Db,
    run_id: &str,
    agent_id: Option<&str>,
    kind: &str,
    payload: Value,
) -> Result<Event, StoreError> {
    let (run_id, agent_id, kind) = (
        run_id.to_string(),
        agent_id.map(str::to_string),
        kind.to_string(),
    );
    db.call(move |c| append_event_tx(c, &run_id, agent_id.as_deref(), &kind, &payload))
        .await
}

/// Events with `seq > after`, oldest first.
pub async fn events_after(
    db: &Db,
    run_id: &str,
    after: i64,
    limit: i64,
) -> Result<Vec<Event>, StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        let mut s = c.prepare("SELECT seq, ts, agent_id, type, payload_json FROM events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3")?;
        let rows = s
            .query_map(params![run_id, after, limit], |r| {
                let p: String = r.get(4)?;
                Ok(Event {
                    run_id: run_id.clone(),
                    seq: r.get(0)?,
                    ts: r.get(1)?,
                    agent_id: r.get(2)?,
                    kind: r.get(3)?,
                    payload: serde_json::from_str(&p).unwrap_or(Value::Null),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

pub async fn save_message(
    db: &Db,
    run_id: &str,
    agent_id: &str,
    idx: i64,
    m: &Message,
) -> Result<(), StoreError> {
    let (run_id, agent_id) = (run_id.to_string(), agent_id.to_string());
    let role = serde_json::to_value(m.role)
        .map_err(|e| StoreError::Other(e.to_string()))?
        .as_str()
        .unwrap_or("user")
        .to_string();
    let parts = serde_json::to_string(&m.parts).map_err(|e| StoreError::Other(e.to_string()))?;
    db.call(move |c| {
        c.execute(
            "INSERT INTO messages(run_id, agent_id, idx, role, parts_json) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id, agent_id, idx) DO UPDATE SET role = excluded.role, parts_json = excluded.parts_json",
            params![run_id, agent_id, idx, role, parts],
        )?;
        Ok(())
    })
    .await
}

pub async fn load_messages(
    db: &Db,
    run_id: &str,
    agent_id: &str,
) -> Result<Vec<Message>, StoreError> {
    let (run_id, agent_id) = (run_id.to_string(), agent_id.to_string());
    db.call(move |c| {
        let mut s = c.prepare("SELECT role, parts_json FROM messages WHERE run_id = ?1 AND agent_id = ?2 ORDER BY idx")?;
        let rows = s
            .query_map(params![run_id, agent_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(role, parts)| {
                let role = serde_json::from_value(Value::String(role)).map_err(|e| StoreError::Other(e.to_string()))?;
                let parts = serde_json::from_str(&parts).map_err(|e| StoreError::Other(e.to_string()))?;
                Ok(Message { role, parts })
            })
            .collect()
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    async fn db() -> (tempfile::TempDir, Db) {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open(&t.path().join("p.db")).unwrap();
        (t, db)
    }

    #[tokio::test]
    async fn events_get_gapless_sequence_and_replay_after() {
        let (_t, db) = db().await;
        create_run(&db, "r1", "", "agent:a", "hi", &json!({}))
            .await
            .unwrap();
        for i in 0..5 {
            let e = append_event(&db, "r1", Some("a"), "text", json!({"i": i}))
                .await
                .unwrap();
            assert_eq!(e.seq, i + 1);
        }
        let after2 = events_after(&db, "r1", 2, 100).await.unwrap();
        assert_eq!(
            after2.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );
        assert_eq!(after2[0].payload["i"], 2);
    }

    #[tokio::test]
    async fn sequences_are_per_run_and_concurrent_appends_do_not_collide() {
        let (_t, db) = db().await;
        for r in ["r1", "r2"] {
            create_run(&db, r, "", "t", "p", &json!({})).await.unwrap();
        }
        let mut tasks = vec![];
        for i in 0..20 {
            let db = db.clone();
            tasks.push(tokio::spawn(async move {
                append_event(
                    &db,
                    if i % 2 == 0 { "r1" } else { "r2" },
                    None,
                    "text",
                    json!({}),
                )
                .await
                .unwrap()
                .seq
            }));
        }
        let mut seqs = vec![];
        for t in tasks {
            seqs.push(t.await.unwrap());
        }
        seqs.sort();
        assert_eq!(
            seqs,
            vec![1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10]
        );
    }

    #[tokio::test]
    async fn status_messages_and_listing() {
        let (_t, db) = db().await;
        create_run(&db, "r1", "", "t", "first", &json!({}))
            .await
            .unwrap();
        create_run(&db, "r2", "", "t", "second", &json!({}))
            .await
            .unwrap();
        set_status(&db, "r1", "running").await.unwrap();
        assert_eq!(get_run(&db, "r1").await.unwrap().unwrap().status, "running");
        assert!(get_run(&db, "nope").await.unwrap().is_none());
        assert_eq!(
            list_runs(&db, 10)
                .await
                .unwrap()
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["r2", "r1"]
        );
        save_message(&db, "r1", "a", 0, &Message::user("hello"))
            .await
            .unwrap();
        save_message(&db, "r1", "a", 1, &Message::assistant("hi"))
            .await
            .unwrap();
        let m = load_messages(&db, "r1", "a").await.unwrap();
        assert_eq!(m.len(), 2);
        assert!(matches!(&m[1].parts[0], patina_core::provider::Part::Text(t) if t == "hi"));
    }
}
