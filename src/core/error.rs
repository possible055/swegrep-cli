use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Error)]
#[error("{message}")]
pub struct FastContextError {
    pub message: String,
    pub code: String,
    pub details: Value,
}

impl FastContextError {
    pub fn new(message: impl Into<String>, code: impl Into<String>, details: Value) -> Self {
        Self {
            message: message.into(),
            code: code.into(),
            details,
        }
    }
}
