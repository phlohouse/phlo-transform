//! Engine errors.

use phlo_transform_core::Diagnostic;
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
    /// A workspace or materialised Git tree failed to load or compile —
    /// carries the diagnostics so callers can report them in their own
    /// format (CLI diagnostic rendering, API error payloads).
    #[error("`{label}` {problem}: {}", render_diagnostics(.diagnostics))]
    FailedDiagnostics {
        label: String,
        problem: &'static str,
        diagnostics: Vec<Diagnostic>,
    },
    /// A Git operation failed — ref resolution, merge-base, tree checkout.
    #[error("git: {0}")]
    Git(String),
    /// A required capability was not configured — callers map this to
    /// their "not configured" error surface (CLI message, API007).
    #[error("not configured: {0}")]
    NotConfigured(String),
    #[error("state store error: {0}")]
    State(String),
    #[error("artifact error: {0}")]
    Artifact(String),
    #[error("invalid plan: {0}")]
    InvalidPlan(String),
    #[error("plan is stale: {0}; run `phlo-transform plan` again")]
    StalePlan(String),
    #[error("promotion failed: {0}")]
    Promotion(String),
    #[error("environment error: {0}")]
    Environment(String),
}

fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.render_human())
        .collect::<Vec<_>>()
        .join("; ")
}
