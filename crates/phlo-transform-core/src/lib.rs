//! Phlo Transform compiler core.
//!
//! Phase 0 provides workspace discovery, SQL parsing, deterministic relation
//! resolution and dependency-graph construction. It contains no execution,
//! state, inventory or adapter concerns.
//!
//! The compiler consumes a frontend-agnostic [`SemanticProject`]. The native
//! filesystem frontend is [`discovery`]; a future dbt importer would lower
//! into the same representation.

// `Diagnostic` is an intentionally rich, value-typed domain error. Returning
// it directly keeps call sites readable; boxing every fallible helper would
// add noise for little benefit.
#![allow(clippy::result_large_err)]

pub mod analyze;
pub mod compile;
pub mod compiled;
pub mod config;
pub mod consumers;
pub mod diagnostics;
pub mod discovery;
pub mod git;
pub mod graph;
pub mod identity;
pub mod lineage;
pub mod model;
pub mod report;
pub mod resolve;
pub mod rewrite;
pub mod schema;
pub mod select;
pub mod semantic;
pub mod version;

use std::path::Path;

pub use compile::{compile, compile_with_options, compile_with_provider};
pub use compiled::{Compilation, CompiledModel, CompiledSeed, CompiledTest};
pub use config::CrossWorkflowPolicy;
pub use consumers::{ConsumerRegistry, EmptyConsumerRegistry, StaticConsumerRegistry};
pub use diagnostics::{codes, Diagnostic, Severity};
pub use discovery::{load_project, model_id_for_path};
pub use git::{
    changes as git_changes, ChangedModel, ChangedPath, ChangedSeed, ChangedTest, DeletedModel,
    GitChanges, GitError, PathStatus,
};
pub use graph::{Dependency, EdgeKind, GraphNode, TransformGraph};
pub use identity::{IdentityError, ModelId, Namespace, SourceId};
pub use lineage::{
    DatasetColumn, DatasetId, DatasetKind, LineageDocument, LineageEdge, LineageEdgeKind,
    LineageGraph, LineageNode, NodeMeta,
};
pub use model::{
    FrontendKind, ModelConfig, ModelOrigin, Relation, RootKind, RootNamespaceStrategy, RootRef,
    SemanticModel, SemanticProject, SemanticSeed, SemanticTest, TestId, TransformRoot,
    TransformRootId, WorkspaceDefaults,
};
pub use phlo_transform_sql::{IncrementalStrategy, Materialization};
pub use report::{
    CheckReport, ColumnLineageReport, ColumnReport, GraphArtifact, GraphEdgeArtifact,
    GraphNodeArtifact, ImpactReport, InspectReport, ListReport, ModelDetail, ModelLineageReport,
    ModelSummary, RootReport, SeedSummary, SourceSummary, TestSummary,
};
pub use schema::{
    classify_schema_change, EmptySchemaProvider, RelationSchema, SchemaChangeSafety, SchemaColumn,
    SchemaProvider, StaticSchemaProvider,
};
pub use select::{
    parse_selector, resolve_selection, SelectedModel, Selection, SelectorError, SelectorKind,
    SelectorSet, SelectorTerm,
};
pub use semantic::{
    Assertion, ColumnContract, ColumnInput, ColumnRef, ColumnTolerance, DataType, DiffPolicySpec,
    Directness, LineageConfidence, ModelContract, ModelSchema, Nullability, OutputColumn,
    RelationRef, Transformation,
};
pub use version::{
    model_version, EmptySourceStateProvider, ModelVersion, SourceStateProvider,
    StaticSourceStateProvider, VersionDetail, VersionInputs, COMPILER_SEMANTICS_VERSION,
};

/// Load a native workspace and compile it in one step.
pub fn load_and_compile(workspace_root: &Path) -> Result<Compilation, Vec<Diagnostic>> {
    let project = load_project(workspace_root)?;
    Ok(compile(&project))
}
