use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolExecutionStatus {
    Pending,
    Completed,
    Error,
    TimedOut,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstantContextTiming {
    pub duration_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstantContextToolUpdate {
    pub step_id: String,
    pub tool_call_id: String,
    pub tool_name: String,
    pub command: Value,
    pub status: ToolExecutionStatus,
    pub output: String,
    pub timing: InstantContextTiming,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstantContextToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}
