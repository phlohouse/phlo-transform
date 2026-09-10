//! Engine errors.

use thiserror::Error;

/// A structured adapter failure.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("{code}: {message}")]
pub struct AdapterError {
    pub code: String,
    pub message: String,
    /// Whether a retry could plausibly succeed. Retries are not yet automated.
    pub retryable: bool,
}

impl AdapterError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }
}

/// An engine-level failure.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error("workspace did not compile; fix the reported diagnostics first")]
    Compilation,
    #[error("state store error: {0}")]
    State(String),
    #[error("artifact error: {0}")]
    Artifact(String),
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
    #[error("plan is stale: {0}; run `phlo-transform plan` again")]
    StalePlan(String),
}
