//! Execution states and structured lifecycle events.

use serde::Serialize;

/// The execution state of a model or test.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    #[default]
    Pending,
    Ready,
    Running,
    Passed,
    Failed,
    Skipped,
    Blocked,
    Cancelled,
}

impl ExecutionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            ExecutionStatus::Passed
                | ExecutionStatus::Failed
                | ExecutionStatus::Skipped
                | ExecutionStatus::Blocked
                | ExecutionStatus::Cancelled
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            ExecutionStatus::Pending => "pending",
            ExecutionStatus::Ready => "ready",
            ExecutionStatus::Running => "running",
            ExecutionStatus::Passed => "passed",
            ExecutionStatus::Failed => "failed",
            ExecutionStatus::Skipped => "skipped",
            ExecutionStatus::Blocked => "blocked",
            ExecutionStatus::Cancelled => "cancelled",
        }
    }
}

/// A structured engine event. Human output is derived from the same stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EngineEvent {
    CompileStarted,
    CompileFinished {
        model_count: usize,
        test_count: usize,
    },
    PlanCreated {
        plan_id: String,
        model_count: usize,
        test_count: usize,
    },
    ModelQueued {
        model: String,
    },
    ModelStarted {
        model: String,
    },
    ModelFinished {
        model: String,
        status: ExecutionStatus,
        query_id: Option<String>,
        duration_ms: u64,
    },
    TestStarted {
        test: String,
    },
    TestFinished {
        test: String,
        status: ExecutionStatus,
        row_count: u64,
        query_id: Option<String>,
    },
    RunFinished {
        run_id: String,
        status: ExecutionStatus,
    },
}
