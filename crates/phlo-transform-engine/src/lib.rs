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
pub mod diff;
pub mod environment;
pub mod error;
pub mod events;
pub mod plan;
pub mod promotion;
pub mod run;
pub mod source_state;
pub mod state;
pub mod util;

pub use adapter::{Adapter, CatalogRequest, ColumnInfo, QueryResult};
pub use artifacts::{
    ArtifactWriter, ColumnLineageArtifact, DiffArtifact, EnvironmentArtifact, GraphArtifactFile,
    LineageArtifact, ManifestArtifact, ModelLineageArtifact, PlanArtifact, PromotionArtifact,
    RunArtifact, SCHEMA_VERSION,
};
pub use cancel::CancelHandle;
pub use diff::{
    diff, DiffPolicy, DiffReport, DiffRequest, DiffStrategy, PolicyResult, RowSummary, SchemaChange,
};
pub use environment::{ensure_environment, EnvironmentSetup, EnvironmentSpec};
pub use error::{AdapterError, EngineError};
pub use events::{EngineEvent, ExecutionStatus};
pub use plan::{
    dependency_closure, ChangeReason, Plan, PlanAction, PlannedModel, PlannedSeed, PlannedTest,
    Planner,
};
pub use promotion::{promote, PromotionRecord, PromotionRequest};
pub use run::{ModelResult, RunOptions, RunResult, Runner, SeedResult, TestResult};
pub use source_state::{
    adapter_default_schema, collect_source_states, relation_for_source, seed_for_relation,
    seed_relation,
};
pub use state::{
    MaterializedRecord, ModelRunRecord, RunRecord, RunSummary, SeedRecord, SqliteStateStore,
    StateStore, TestRunRecord,
};
