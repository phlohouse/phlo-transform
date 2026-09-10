//! Phlo Transform engine.
//!
//! Phase 1 turns the compiler into an MVP build engine: a compact adapter
//! boundary, planning, dependency-aware bounded-concurrency execution,
//! operational state and versioned artifacts. The engine depends on the
//! compiler (`phlo-transform-core`) but the compiler does not depend on the
//! engine.

pub mod adapter;
pub mod artifacts;
pub mod cancel;
pub mod error;
pub mod events;
pub mod plan;
pub mod run;
pub mod state;
pub mod util;

pub use adapter::{Adapter, ColumnInfo, QueryResult};
pub use artifacts::{
    ArtifactWriter, ColumnLineageArtifact, GraphArtifactFile, LineageArtifact, ManifestArtifact,
    ModelLineageArtifact, PlanArtifact, RunArtifact, SCHEMA_VERSION,
};
pub use cancel::CancelHandle;
pub use error::{AdapterError, EngineError};
pub use events::{EngineEvent, ExecutionStatus};
pub use plan::{dependency_closure, Plan, PlanAction, PlannedModel, PlannedTest, Planner};
pub use run::{ModelResult, RunOptions, RunResult, Runner, TestResult};
pub use state::{
    MaterializedRecord, ModelRunRecord, RunRecord, RunSummary, SqliteStateStore, StateStore,
    TestRunRecord,
};
