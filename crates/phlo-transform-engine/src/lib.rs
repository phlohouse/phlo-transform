//! Phlo Transform engine.
//!
//! Phase 1 turns the compiler into an MVP build engine: a compact adapter
//! boundary, planning, dependency-aware bounded-concurrency execution,
//! operational state and versioned artifacts. The engine depends on the
//! compiler (`phlo-transform-core`) but the compiler does not depend on the
//! engine.

pub mod adapter;
pub mod artifacts;
pub mod audit;
pub mod branch_diff;
pub mod cancel;
pub mod changed;
pub mod contracts;
pub mod diff;
pub mod environment;
pub mod error;
pub mod events;
pub mod failure;
pub mod gates;
pub mod plan;
pub mod promotion;
pub mod run;
pub mod source_state;
pub mod state;
pub mod state_postgres;
pub mod util;

pub use adapter::{Adapter, CatalogRequest, ColumnInfo, QueryResult};
pub use artifacts::{
    ArtifactWriter, CandidateProvenance, DiffArtifact, EnvironmentArtifact, GraphArtifactFile,
    LineageArtifact, LineageDiffArtifact, LineageEnvironment, ManifestArtifact,
    OpenLineageArtifact, PlanArtifact, PromotionArtifact, RunArtifact, SCHEMA_VERSION,
};
pub use audit::{
    audited_diff, audited_lineage, compiled_catalog, contract_breaking_changes,
    environment_artifact_name, read_branch_diff, read_diff, read_environment, read_environment_for,
    remove_environment_artifacts, write_environment_artifacts, AuditEvidence, LineageEvidence,
};
pub use branch_diff::{
    branch_diff, materialized_for_environment, model_keys, retarget, seeds_for_environment,
    BranchDiffReport, BranchDiffRequest, DatasetDiff, DatasetKind, DatasetStatus,
    ModelContractDiff, ModelRowDiff, ModelSchemaDiff,
};
pub use cancel::CancelHandle;
pub use changed::changed_models;
pub use contracts::{
    breaking_contract_changes, contract_diff, effective_key, key_change, ContractChange,
    ContractSafety, RecordedKey,
};
pub use diff::{
    diff, DiffPolicy, DiffReport, DiffRequest, DiffStrategy, PolicyResult, RowSummary, SchemaChange,
};
pub use environment::{catalog_name, ensure_environment, EnvironmentSetup, EnvironmentSpec};
pub use error::{AdapterError, EngineError};
pub use events::{EngineEvent, ExecutionStatus};
pub use failure::{Attempt, Failure, FailureCategory, RetryPolicy};
pub use gates::{evaluate_gates, GateInput, GateReport, GateResult};
pub use plan::{
    dependency_closure, diff_reasons, Membership, Plan, PlanAction, PlanOptions, PlanReason,
    PlanSelection, PlannedModel, PlannedSeed, PlannedTest, Planner, ReasonKind,
};
pub use promotion::{promote, PromotionRecord, PromotionRequest};
pub use run::{ModelResult, RunCounts, RunOptions, RunResult, Runner, SeedResult, TestResult};
pub use source_state::{
    adapter_default_schema, collect_source_states, relation_for_source, seed_for_relation,
    seed_relation,
};
pub use state::{
    MaterializedRecord, ModelRunRecord, RunRecord, RunSummary, SeedRecord, SeedRunRecord,
    SqliteStateStore, StateStore, StoredPlan, StoredRun, TestRunRecord,
};
pub use state_postgres::PostgresStateStore;
