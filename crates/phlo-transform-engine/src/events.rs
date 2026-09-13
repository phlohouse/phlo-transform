//! Execution states and structured lifecycle events.

use serde::Serialize;

/// The execution state of a model, seed or test.
///
/// Lifecycle: `pending` → `ready` → `running` → a terminal state. `skipped`
/// and `cached` mean the planner's decision satisfied the node without
/// executing; `blocked` means a required upstream failed so the node never
/// ran; `cancelled` covers both `--fail-fast` shutdown and external
/// cancellation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    #[default]
    Pending,
    Ready,
    Running,
    Passed,
    Failed,
    Skipped,
    /// Reused from a prior successful execution or a compatible
    /// materialisation — no work was done this run.
    Cached,
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
                | ExecutionStatus::Cached
                | ExecutionStatus::Blocked
                | ExecutionStatus::Cancelled
        )
    }

    /// Whether the node's dataset is available to dependents — execution
    /// succeeded or a valid earlier materialisation was reused.
    pub fn is_satisfied(self) -> bool {
        matches!(
            self,
            ExecutionStatus::Passed | ExecutionStatus::Skipped | ExecutionStatus::Cached
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
            ExecutionStatus::Cached => "cached",
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
    /// The run started (or continued, when `continued_from` names the run
    /// being resumed or retried).
    RunStarted {
        run_id: String,
        plan_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        continued_from: Option<String>,
    },
    ModelQueued {
        model: String,
    },
    SeedStarted {
        seed: String,
    },
    /// A seed load attempt failed and will be retried.
    SeedRetrying {
        seed: String,
        attempt: u32,
        delay_ms: u64,
        reason: String,
    },
    SeedFinished {
        seed: String,
        status: ExecutionStatus,
    },
    ModelStarted {
        model: String,
    },
    /// An attempt failed and the model will be retried after `delay_ms`.
    ModelRetrying {
        model: String,
        attempt: u32,
        delay_ms: u64,
        reason: String,
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
