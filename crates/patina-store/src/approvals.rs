//! Approval requests. They are stored so they survive a restart and can be answered from any device.
use crate::{Db, StoreError};
use patina_core::events::now_ms;
use rusqlite::{OptionalExtension, params};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRow {
    pub id: String,
    pub run_id: String,
    /// Sequence number of the `approval_requested` event.
    pub seq: i64,
    /// `{agent_id, id, name, args, diff}`.
    pub tool_call: Value,
    pub status: String,
    pub decision: Option<Value>,
    pub created_at: i64,
    pub decided_at: Option<i64>,
}

fn from_row(r: &rusqlite::Row) -> rusqlite::Result<ApprovalRow> {
    let tool_call: String = r.get(3)?;
    let decision: Option<String> = r.get(5)?;
    Ok(ApprovalRow {
        id: r.get(0)?,
        run_id: r.get(1)?,
        seq: r.get(2)?,
        tool_call: serde_json::from_str(&tool_call).unwrap_or(Value::Null),
        status: r.get(4)?,
        decision: decision.and_then(|d| serde_json::from_str(&d).ok()),
        created_at: r.get(6)?,
        decided_at: r.get(7)?,
    })
}

const COLS: &str = "id, run_id, seq, tool_call_json, status, decision_json, created_at, decided_at";

pub async fn create(db: &Db, id: &str, run_id: &str, seq: i64, tool_call: &Value) -> Result<(), StoreError> {
    let (id, run_id, tc) = (id.to_string(), run_id.to_string(), tool_call.to_string());
    db.call(move |c| {
        c.execute(
            "INSERT INTO approvals(id, run_id, seq, tool_call_json, status, created_at) VALUES(?1, ?2, ?3, ?4, 'pending', ?5)",
            params![id, run_id, seq, tc, now_ms()],
        )?;
        Ok(())
    })
    .await
}

pub async fn get(db: &Db, id: &str) -> Result<Option<ApprovalRow>, StoreError> {
    let id = id.to_string();
    db.call(move |c| {
        Ok(c.query_row(&format!("SELECT {COLS} FROM approvals WHERE id = ?1"), [id], from_row)
            .optional()?)
    })
    .await
}

/// Approvals with the given status (or all), oldest first.
pub async fn list(db: &Db, status: Option<&str>) -> Result<Vec<ApprovalRow>, StoreError> {
    let status = status.map(str::to_string);
    db.call(move |c| {
        let mut s = c.prepare(&format!(
            "SELECT {COLS} FROM approvals WHERE ?1 IS NULL OR status = ?1 ORDER BY created_at, id"
        ))?;
        let rows = s.query_map([status], from_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    })
    .await
}

/// Record the owner's answer. Returns false if the approval was not pending, so an answer counts once.
pub async fn resolve(db: &Db, id: &str, status: &str, decision: &Value) -> Result<bool, StoreError> {
    let (id, status, decision) = (id.to_string(), status.to_string(), decision.to_string());
    db.call(move |c| {
        let n = c.execute(
            "UPDATE approvals SET status = ?2, decision_json = ?3, decided_at = ?4 WHERE id = ?1 AND status = 'pending'",
            params![id, status, decision, now_ms()],
        )?;
        Ok(n == 1)
    })
    .await
}

/// Mark every pending approval of a run as no longer answerable (the run ended or was cancelled).
pub async fn expire_for_run(db: &Db, run_id: &str) -> Result<(), StoreError> {
    let run_id = run_id.to_string();
    db.call(move |c| {
        c.execute(
            "UPDATE approvals SET status = 'expired', decided_at = ?2 WHERE run_id = ?1 AND status = 'pending'",
            params![run_id, now_ms()],
        )?;
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn approvals_are_stored_listed_and_answered_once() {
        let t = tempfile::tempdir().unwrap();
        let db = Db::open(&t.path().join("p.db")).unwrap();
        crate::runs::create_run(&db, "r1", "site", "agent:a", "p", &json!({}))
            .await
            .unwrap();
        create(&db, "ap1", "r1", 4, &json!({"name": "write_file", "args": {"path": "a"}}))
            .await
            .unwrap();
        create(&db, "ap2", "r1", 9, &json!({"name": "edit_file"})).await.unwrap();
        assert_eq!(list(&db, Some("pending")).await.unwrap().len(), 2);
        assert!(resolve(&db, "ap1", "approved", &json!({"action": "approve"})).await.unwrap());
        assert!(
            !resolve(&db, "ap1", "denied", &json!({})).await.unwrap(),
            "a second answer is ignored"
        );
        let a = get(&db, "ap1").await.unwrap().unwrap();
        assert_eq!(
            (a.status.as_str(), a.tool_call["name"].as_str(), a.decided_at.is_some()),
            ("approved", Some("write_file"), true)
        );
        expire_for_run(&db, "r1").await.unwrap();
        assert_eq!(get(&db, "ap2").await.unwrap().unwrap().status, "expired");
        assert!(list(&db, Some("pending")).await.unwrap().is_empty());
        assert_eq!(list(&db, None).await.unwrap().len(), 2);

        // Reopening the database keeps them: approvals survive a restart.
        drop(db);
        let db = Db::open(&t.path().join("p.db")).unwrap();
        assert_eq!(get(&db, "ap1").await.unwrap().unwrap().status, "approved");
    }
}
