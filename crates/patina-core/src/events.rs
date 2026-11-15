use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Event types of section 6. Stored as the snake_case string.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    RunStarted,
    AgentStarted,
    Text,
    Reasoning,
    ToolCall,
    ApprovalRequested,
    ApprovalResolved,
    ToolResult,
    /// A message the owner sent to a run after it started.
    UserMessage,
    Usage,
    Checkpoint,
    Compaction,
    Rollback,
    /// The owner saved a file in the run's worktree from the editor.
    OwnerEdit,
    Error,
    AgentFinished,
    RunFinished,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventType::RunStarted => "run_started",
            EventType::AgentStarted => "agent_started",
            EventType::Text => "text",
            EventType::Reasoning => "reasoning",
            EventType::ToolCall => "tool_call",
            EventType::ApprovalRequested => "approval_requested",
            EventType::ApprovalResolved => "approval_resolved",
            EventType::ToolResult => "tool_result",
            EventType::UserMessage => "user_message",
            EventType::Usage => "usage",
            EventType::Checkpoint => "checkpoint",
            EventType::Compaction => "compaction",
            EventType::Rollback => "rollback",
            EventType::OwnerEdit => "owner_edit",
            EventType::Error => "error",
            EventType::AgentFinished => "agent_finished",
            EventType::RunFinished => "run_finished",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingApproval,
    Paused,
    Completed,
    Failed,
    Cancelled,
    Interrupted,
    BudgetExceeded,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::WaitingApproval => "waiting_approval",
            RunStatus::Paused => "paused",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Interrupted => "interrupted",
            RunStatus::BudgetExceeded => "budget_exceeded",
        }
    }
}

/// One persisted event. `seq` starts at 1 within a run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TS)]
#[ts(export)]
pub struct Event {
    pub run_id: String,
    pub seq: i64,
    /// Unix milliseconds.
    pub ts: i64,
    pub agent_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[ts(type = "Record<string, any>")]
    pub payload: serde_json::Value,
}

/// UUIDv7: sorts by creation time.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_sort_by_time() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert!(a < new_id());
    }

    #[test]
    fn event_type_strings_match_serde() {
        let s = serde_json::to_string(&EventType::ApprovalRequested).unwrap();
        assert_eq!(s, format!("\"{}\"", EventType::ApprovalRequested.as_str()));
        let s = serde_json::to_string(&RunStatus::BudgetExceeded).unwrap();
        assert_eq!(s, format!("\"{}\"", RunStatus::BudgetExceeded.as_str()));
    }
}
