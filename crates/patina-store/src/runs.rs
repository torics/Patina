//! Runs, their event log and each agent's message history.
use crate::{Db, StoreError};
use patina_core::events::{Event, now_ms};
use patina_core::provider::Message;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub project_id: String,
    pub team_id: String,
    pub prompt: String,
    pub status: String,
    pub budgets: Value,
    pub usage: Value,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    pub disposition: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The process that is driving the run, as `patina-tools` identifies processes. `None` when nobody is.
    pub owner: Option<String>,
    /// When retention removed the run's log and context.
    pub pruned_at: Option<i64>,
}

fn run_from_row(r: &rusqlite::Row) -> rusqlite::Result<RunRow> {
    let json = |i: usize| -> rusqlite::Result<Value> { Ok(serde_json::from_str(&r.get::<_, String>(i)?).unwrap_or(Value::Null)) };
    Ok(RunRow {
        id: r.get(0)?,
        project_id: r.get(1)?,
        team_id: r.get(2)?,
        prompt: r.get(3)?,
        status: r.get(4)?,
        budgets: json(5)?,
        usage: json(6)?,
        worktree_path: r.get(7)?,
        branch: r.get(8)?,
        base_commit: r.get(9)?,
        disposition: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
        owner: r.get(13)?,
        pruned_at: r.get(14)?,
    })
}

const RUN_COLS: &str = "id, project_id, team_id, prompt, status, budgets_json, usage_json, worktree_path, branch, base_commit, disposition, created_at, updated_at, owner, pruned_at";

pub async fn create_run(db: &Db, id: &str, project_id: &str, team_id: &str, prompt: &str, budgets: &Value) -> Result<(), StoreError> {
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

/// Record where the run works: its worktree, branch and the commit it started from.
pub async fn set_worktree(db: &Db, id: &str, worktree: Option<&str>, branch: Option<&str>, base: Option<&str>) -> Result<(), StoreError> {
    let (id, w, b, c0) = (
        id.to_string(),
        worktree.map(str::to_string),
        branch.map(str::to_string),
        base.map(str::to_string),
    );
    db.call(move |c| {
        c.execute(
            "UPDATE runs SET worktree_path = ?2, branch = ?3, base_commit = ?4, updated_at = ?5 WHERE id = ?1",
            params![id, w, b, c0, now_ms()],
        )?;
        Ok(())
    })
    .await
}

/// The worktree is gone; remember what became of the changes.
pub async fn set_disposition(db: &Db, id: &str, disposition: &str, keep_branch: bool) -> Result<(), StoreError> {
    let (id, d) = (id.to_string(), disposition.to_string());
    db.call(move |c| {
        c.execute(
            "UPDATE runs SET disposition = ?2, worktree_path = NULL, updated_at = ?3 WHERE id = ?1",
            params![id, d, now_ms()],
        )?;
        if !keep_branch {
            c.execute("UPDATE runs SET branch = NULL WHERE id = ?1", params![id])?;
        }
        Ok(())
    })
    .await
}

pub async fn set_budgets(db: &Db, id: &str, budgets: &Value) -> Result<(), StoreError> {
    let (id, b) = (id.to_string(), budgets.to_string());
    db.call(move |c| {
        c.execute(
            "UPDATE runs SET budgets_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, b, now_ms()],
        )?;
        Ok(())
    })
    .await
}

#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointRow {
    pub step: i64,
    pub commit_sha: String,
    pub summary: String,
    pub created_at: i64,
}

pub async fn add_checkpoint(db: &Db, run_id: &str, step: i64, commit: &str, summary: &str) -> Result<(), StoreError> {
    let (run_id, commit, summary) = (run_id.to_string(), commit.to_string(), summary.to_string());
    db.call(move |c| {
        c.execute(
            "INSERT INTO checkpoints(run_id, step, commit_sha, summary, created_at) VALUES(?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id, step) DO UPDATE SET commit_sha = excluded.commit_sha, summary = excluded.summary",
            params![run_id, step, commit, summary, now_ms()],
        )?;
        Ok(())
    })
    .await
}

pub async fn list_checkpoints(db: &Db, run_id: &str) -> Result<Vec<CheckpointRow>, StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        let mut s = c.prepare("SELECT step, commit_sha, summary, created_at FROM checkpoints WHERE run_id = ?1 ORDER BY step")?;
        let rows = s
            .query_map([run_id], |r| {
                Ok(CheckpointRow {
                    step: r.get(0)?,
                    commit_sha: r.get(1)?,
                    summary: r.get(2)?,
                    created_at: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Checkpoints after `step` no longer describe the worktree once it is rolled back.
pub async fn drop_checkpoints_after(db: &Db, run_id: &str, step: i64) -> Result<(), StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        c.execute("DELETE FROM checkpoints WHERE run_id = ?1 AND step > ?2", params![run_id, step])?;
        Ok(())
    })
    .await
}

/// Number of messages stored for an agent, which is the index of its next one.
pub async fn message_count(db: &Db, run_id: &str, agent_id: &str) -> Result<i64, StoreError> {
    let (run_id, agent_id) = (run_id.to_string(), agent_id.to_string());
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM messages WHERE run_id = ?1 AND agent_id = ?2",
            params![run_id, agent_id],
            |r| r.get(0),
        )?)
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
        Ok(
            c.query_row(&format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"), [id], run_from_row)
                .optional()?,
        )
    })
    .await
}

pub async fn list_runs(db: &Db, limit: i64) -> Result<Vec<RunRow>, StoreError> {
    db.call(move |c| {
        let mut s = c.prepare(&format!("SELECT {RUN_COLS} FROM runs ORDER BY id DESC LIMIT ?1"))?;
        let rows = s.query_map([limit], run_from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Append one event, assigning the next sequence number. Safe because one thread owns the connection.
pub fn append_event_tx(c: &Connection, run_id: &str, agent_id: Option<&str>, kind: &str, payload: &Value) -> Result<Event, StoreError> {
    let seq: i64 = c.query_row("SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE run_id = ?1", [run_id], |r| {
        r.get(0)
    })?;
    let ts = now_ms();
    c.execute(
        "INSERT INTO events(run_id, seq, ts, agent_id, type, payload_json) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        params![run_id, seq, ts, agent_id, kind, payload.to_string()],
    )?;
    c.execute("UPDATE runs SET updated_at = ?2 WHERE id = ?1", params![run_id, ts])?;
    Ok(Event {
        run_id: run_id.into(),
        seq,
        ts,
        agent_id: agent_id.map(str::to_string),
        kind: kind.into(),
        payload: payload.clone(),
    })
}

pub async fn append_event(db: &Db, run_id: &str, agent_id: Option<&str>, kind: &str, payload: Value) -> Result<Event, StoreError> {
    let (run_id, agent_id, kind) = (run_id.to_string(), agent_id.map(str::to_string), kind.to_string());
    db.call(move |c| append_event_tx(c, &run_id, agent_id.as_deref(), &kind, &payload))
        .await
}

/// Events with `seq > after`, oldest first.
pub async fn events_after(db: &Db, run_id: &str, after: i64, limit: i64) -> Result<Vec<Event>, StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        let mut s =
            c.prepare("SELECT seq, ts, agent_id, type, payload_json FROM events WHERE run_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3")?;
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

pub async fn save_message(db: &Db, run_id: &str, agent_id: &str, idx: i64, m: &Message) -> Result<(), StoreError> {
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

pub async fn load_messages(db: &Db, run_id: &str, agent_id: &str) -> Result<Vec<Message>, StoreError> {
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

/// Record which process drives the run, or that none does.
pub async fn set_owner(db: &Db, id: &str, owner: Option<&str>) -> Result<(), StoreError> {
    let (id, owner) = (id.to_string(), owner.map(str::to_string));
    db.call(move |c| {
        c.execute("UPDATE runs SET owner = ?2 WHERE id = ?1", params![id, owner])?;
        Ok(())
    })
    .await
}

/// Runs that some process was working on, or that wait for an approval: what a restart has to look at.
pub async fn unfinished_runs(db: &Db) -> Result<Vec<RunRow>, StoreError> {
    db.call(move |c| {
        let mut s = c.prepare(&format!(
            "SELECT {RUN_COLS} FROM runs WHERE status IN ('queued', 'running', 'waiting_approval') ORDER BY id"
        ))?;
        let rows = s.query_map([], run_from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Sequence number of the agent's latest event of one type.
pub async fn last_seq_of(db: &Db, run_id: &str, agent_id: &str, kind: &str) -> Result<Option<i64>, StoreError> {
    let (run_id, agent_id, kind) = (run_id.to_string(), agent_id.to_string(), kind.to_string());
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT MAX(seq) FROM events WHERE run_id = ?1 AND agent_id = ?2 AND type = ?3",
            params![run_id, agent_id, kind],
            |r| r.get(0),
        )?)
    })
    .await
}

/// Replace an agent's stored context, after a compaction. One transaction: a crash leaves the old or the new.
pub async fn replace_messages(db: &Db, run_id: &str, agent_id: &str, messages: &[Message]) -> Result<(), StoreError> {
    let (run_id, agent_id) = (run_id.to_string(), agent_id.to_string());
    let mut rows = Vec::with_capacity(messages.len());
    for m in messages {
        let role = serde_json::to_value(m.role)
            .map_err(|e| StoreError::Other(e.to_string()))?
            .as_str()
            .unwrap_or("user")
            .to_string();
        rows.push((role, serde_json::to_string(&m.parts).map_err(|e| StoreError::Other(e.to_string()))?));
    }
    db.call(move |c| {
        let tx = c.transaction()?;
        tx.execute(
            "DELETE FROM messages WHERE run_id = ?1 AND agent_id = ?2",
            params![run_id, agent_id],
        )?;
        for (idx, (role, parts)) in rows.iter().enumerate() {
            tx.execute(
                "INSERT INTO messages(run_id, agent_id, idx, role, parts_json) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![run_id, agent_id, idx as i64, role, parts],
            )?;
        }
        tx.commit()?;
        Ok(())
    })
    .await
}

/// The run's latest sequence number, 0 for an empty log.
pub async fn last_seq(db: &Db, run_id: &str) -> Result<i64, StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| Ok(c.query_row("SELECT COALESCE(MAX(seq), 0) FROM events WHERE run_id = ?1", [run_id], |r| r.get(0))?))
        .await
}

/// Take back a message that could not be delivered.
pub async fn delete_steering(db: &Db, id: &str) -> Result<(), StoreError> {
    let id = id.to_string();
    db.call(move |c| {
        c.execute("DELETE FROM steering WHERE id = ?1 AND delivered_at IS NULL", [id])?;
        Ok(())
    })
    .await
}

/// Queue a message from the owner for the run's agent.
pub async fn add_steering(db: &Db, id: &str, run_id: &str, text: &str) -> Result<(), StoreError> {
    let (id, run_id, text) = (id.to_string(), run_id.to_string(), text.to_string());
    db.call(move |c| {
        c.execute(
            "INSERT INTO steering(id, run_id, text, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![id, run_id, text, now_ms()],
        )?;
        Ok(())
    })
    .await
}

/// Hand out the owner's waiting messages, oldest first. Each is handed out once.
pub async fn take_steering(db: &Db, run_id: &str) -> Result<Vec<String>, StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        let tx = c.transaction()?;
        let texts = {
            let mut s = tx.prepare("SELECT text FROM steering WHERE run_id = ?1 AND delivered_at IS NULL ORDER BY id")?;
            s.query_map([&run_id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?
        };
        tx.execute(
            "UPDATE steering SET delivered_at = ?2 WHERE run_id = ?1 AND delivered_at IS NULL",
            params![run_id, now_ms()],
        )?;
        tx.commit()?;
        Ok(texts)
    })
    .await
}

/// How many of the owner's messages still wait for the agent.
pub async fn waiting_steering(db: &Db, run_id: &str) -> Result<i64, StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM steering WHERE run_id = ?1 AND delivered_at IS NULL",
            [run_id],
            |r| r.get(0),
        )?)
    })
    .await
}

/// Retention: remove the event log, the agents' contexts, answered approvals and delivered messages of runs
/// that ended before `cutoff` (Unix milliseconds). The run rows and their checkpoints stay. Runs that are
/// active, paused or waiting are never touched. Returns the ids of the runs that were pruned.
pub async fn prune_before(db: &Db, cutoff: i64) -> Result<Vec<String>, StoreError> {
    db.call(move |c| {
        let tx = c.transaction()?;
        let ids = {
            let mut s = tx.prepare(
                "SELECT id FROM runs WHERE pruned_at IS NULL AND updated_at < ?1
                 AND status IN ('completed', 'failed', 'cancelled', 'interrupted', 'budget_exceeded') ORDER BY id",
            )?;
            s.query_map([cutoff], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?
        };
        for id in &ids {
            for table in ["events", "messages", "approvals", "steering"] {
                tx.execute(&format!("DELETE FROM {table} WHERE run_id = ?1"), [id])?;
            }
            // `updated_at` stays: it says when the run ended.
            tx.execute("UPDATE runs SET pruned_at = ?2 WHERE id = ?1", params![id, now_ms()])?;
        }
        tx.commit()?;
        Ok(ids)
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
        create_run(&db, "r1", "", "agent:a", "hi", &json!({})).await.unwrap();
        for i in 0..5 {
            let e = append_event(&db, "r1", Some("a"), "text", json!({"i": i})).await.unwrap();
            assert_eq!(e.seq, i + 1);
        }
        let after2 = events_after(&db, "r1", 2, 100).await.unwrap();
        assert_eq!(after2.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![3, 4, 5]);
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
                append_event(&db, if i % 2 == 0 { "r1" } else { "r2" }, None, "text", json!({}))
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
        assert_eq!(seqs, vec![1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10]);
    }

    #[tokio::test]
    async fn worktree_checkpoints_and_disposition() {
        let (_t, db) = db().await;
        create_run(&db, "r1", "site", "agent:a", "p", &json!({"max_steps": 5}))
            .await
            .unwrap();
        set_worktree(&db, "r1", Some("/w/r1"), Some("patina/run-r1"), Some("abc"))
            .await
            .unwrap();
        add_checkpoint(&db, "r1", 1, "c1", "write_file").await.unwrap();
        add_checkpoint(&db, "r1", 3, "c3", "edit_file").await.unwrap();
        assert_eq!(
            list_checkpoints(&db, "r1")
                .await
                .unwrap()
                .iter()
                .map(|c| c.step)
                .collect::<Vec<_>>(),
            [1, 3]
        );
        drop_checkpoints_after(&db, "r1", 1).await.unwrap();
        assert_eq!(list_checkpoints(&db, "r1").await.unwrap().len(), 1);
        let r = get_run(&db, "r1").await.unwrap().unwrap();
        assert_eq!(
            (r.budgets["max_steps"].clone(), r.branch.as_deref(), r.disposition.clone()),
            (json!(5), Some("patina/run-r1"), None)
        );
        set_disposition(&db, "r1", "merged", false).await.unwrap();
        let r = get_run(&db, "r1").await.unwrap().unwrap();
        assert_eq!((r.worktree_path, r.branch, r.disposition.as_deref()), (None, None, Some("merged")));
    }

    #[tokio::test]
    async fn status_messages_and_listing() {
        let (_t, db) = db().await;
        create_run(&db, "r1", "", "t", "first", &json!({})).await.unwrap();
        create_run(&db, "r2", "", "t", "second", &json!({})).await.unwrap();
        set_status(&db, "r1", "running").await.unwrap();
        assert_eq!(get_run(&db, "r1").await.unwrap().unwrap().status, "running");
        assert!(get_run(&db, "nope").await.unwrap().is_none());
        assert_eq!(
            list_runs(&db, 10).await.unwrap().iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["r2", "r1"]
        );
        save_message(&db, "r1", "a", 0, &Message::user("hello")).await.unwrap();
        save_message(&db, "r1", "a", 1, &Message::assistant("hi")).await.unwrap();
        let m = load_messages(&db, "r1", "a").await.unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(message_count(&db, "r1", "a").await.unwrap(), 2);
        assert!(matches!(&m[1].parts[0], patina_core::provider::Part::Text(t) if t == "hi"));
    }

    #[tokio::test]
    async fn a_compacted_context_replaces_the_stored_one() {
        let (_t, db) = db().await;
        create_run(&db, "r1", "", "agent:a", "hi", &json!({})).await.unwrap();
        for (i, text) in ["one", "two", "three"].iter().enumerate() {
            save_message(&db, "r1", "a", i as i64, &Message::user(*text)).await.unwrap();
        }
        save_message(&db, "r1", "b", 0, &Message::user("other agent")).await.unwrap();
        replace_messages(&db, "r1", "a", &[Message::user("one"), Message::assistant("summary")])
            .await
            .unwrap();
        let now = load_messages(&db, "r1", "a").await.unwrap();
        assert_eq!(
            serde_json::to_value(&now).unwrap(),
            json!([{"role": "user", "parts": [{"text": "one"}]}, {"role": "assistant", "parts": [{"text": "summary"}]}])
        );
        assert_eq!(message_count(&db, "r1", "a").await.unwrap(), 2);
        assert_eq!(load_messages(&db, "r1", "b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_owners_messages_are_handed_out_once_in_order() {
        let (_t, db) = db().await;
        create_run(&db, "r1", "", "agent:a", "hi", &json!({})).await.unwrap();
        create_run(&db, "r2", "", "agent:a", "hi", &json!({})).await.unwrap();
        add_steering(&db, "m1", "r1", "first").await.unwrap();
        add_steering(&db, "m2", "r1", "second").await.unwrap();
        add_steering(&db, "m3", "r2", "elsewhere").await.unwrap();
        assert_eq!(waiting_steering(&db, "r1").await.unwrap(), 2);
        assert_eq!(take_steering(&db, "r1").await.unwrap(), ["first", "second"]);
        assert!(take_steering(&db, "r1").await.unwrap().is_empty());
        assert_eq!(waiting_steering(&db, "r1").await.unwrap(), 0);
        assert_eq!(take_steering(&db, "r2").await.unwrap(), ["elsewhere"]);
    }

    #[tokio::test]
    async fn retention_prunes_only_runs_that_ended_long_ago() {
        let (_t, db) = db().await;
        for (id, status) in [
            ("old", "completed"),
            ("paused", "paused"),
            ("waiting", "waiting_approval"),
            ("new", "failed"),
        ] {
            create_run(&db, id, "", "agent:a", "hi", &json!({})).await.unwrap();
            append_event(&db, id, Some("a"), "text", json!({"text": "x"})).await.unwrap();
            save_message(&db, id, "a", 0, &Message::user("hi")).await.unwrap();
            add_checkpoint(&db, id, 1, "abc", "created a").await.unwrap();
            set_status(&db, id, status).await.unwrap();
        }
        let long_ago = now_ms() - 40 * 86_400_000;
        db.call(move |c| {
            c.execute("UPDATE runs SET updated_at = ?1 WHERE id != 'new'", [long_ago])?;
            Ok(())
        })
        .await
        .unwrap();
        let cutoff = now_ms() - 30 * 86_400_000;
        assert_eq!(prune_before(&db, cutoff).await.unwrap(), ["old"]);
        assert!(events_after(&db, "old", 0, 10).await.unwrap().is_empty());
        assert!(load_messages(&db, "old", "a").await.unwrap().is_empty());
        let old = get_run(&db, "old").await.unwrap().unwrap();
        assert!(old.pruned_at.is_some() && old.updated_at == long_ago && old.prompt == "hi");
        assert_eq!(list_checkpoints(&db, "old").await.unwrap().len(), 1);
        // Paused and waiting runs keep everything however old they are; so does a run that ended recently.
        for id in ["paused", "waiting", "new"] {
            assert_eq!(events_after(&db, id, 0, 10).await.unwrap().len(), 1, "{id}");
            assert!(get_run(&db, id).await.unwrap().unwrap().pruned_at.is_none());
        }
        // A second pass finds nothing new.
        assert!(prune_before(&db, cutoff).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_restart_finds_the_runs_that_were_in_progress() {
        let (_t, db) = db().await;
        for (id, status) in [
            ("a", "running"),
            ("b", "completed"),
            ("c", "waiting_approval"),
            ("d", "queued"),
            ("e", "paused"),
        ] {
            create_run(&db, id, "", "agent:a", "hi", &json!({})).await.unwrap();
            set_status(&db, id, status).await.unwrap();
        }
        set_owner(&db, "a", Some("123:456")).await.unwrap();
        let found = unfinished_runs(&db).await.unwrap();
        assert_eq!(found.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["a", "c", "d"]);
        assert_eq!(found[0].owner.as_deref(), Some("123:456"));
        set_owner(&db, "a", None).await.unwrap();
        assert_eq!(get_run(&db, "a").await.unwrap().unwrap().owner, None);
        append_event(&db, "a", Some("x"), "usage", json!({})).await.unwrap();
        append_event(&db, "a", Some("x"), "text", json!({})).await.unwrap();
        append_event(&db, "a", Some("y"), "usage", json!({})).await.unwrap();
        assert_eq!(last_seq_of(&db, "a", "x", "usage").await.unwrap(), Some(1));
        assert_eq!(last_seq_of(&db, "a", "x", "checkpoint").await.unwrap(), None);
    }
}
