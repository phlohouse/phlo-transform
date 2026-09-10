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
pub mod diagnostics;
pub mod discovery;
pub mod graph;
pub mod identity;
pub mod model;
pub mod report;
pub mod resolve;
pub mod rewrite;
pub mod schema;
pub mod select;
pub mod semantic;
pub mod version;

use std::path::Path;

pub use compile::{compile, compile_with_provider};
pub use compiled::{Compilation, CompiledModel, CompiledTest};
pub use diagnostics::{codes, Diagnostic, Severity};
pub use discovery::load_project;
pub use graph::{Dependency, EdgeKind, GraphNode, TransformGraph};
pub use identity::{IdentityError, ModelId, Namespace, SourceId};
pub use model::{
    FrontendKind, ModelConfig, ModelOrigin, Relation, RootKind, RootNamespaceStrategy, RootRef,
    SemanticModel, SemanticProject, SemanticTest, TestId, TransformRoot, TransformRootId,
    WorkspaceDefaults,
};
pub use phlo_transform_sql::Materialization;
pub use report::{
    CheckReport, ColumnLineageReport, ColumnReport, GraphArtifact, GraphEdgeArtifact,
    GraphNodeArtifact, ImpactReport, InspectReport, ListReport, ModelDetail, ModelLineageReport,
    ModelSummary, RootReport, SourceSummary, TestSummary,
};
pub use schema::{
    EmptySchemaProvider, RelationSchema, SchemaColumn, SchemaProvider, StaticSchemaProvider,
};
pub use select::{select_models, SelectionOptions};
pub use semantic::{
    Assertion, ColumnContract, ColumnRef, DataType, ModelContract, ModelSchema, Nullability,
    OutputColumn, RelationRef,
};
pub use version::{
    model_version, EmptySourceStateProvider, ModelVersion, SourceStateProvider,
    StaticSourceStateProvider, VersionInputs, COMPILER_SEMANTICS_VERSION,
};

/// Load a native workspace and compile it in one step.
pub fn load_and_compile(workspace_root: &Path) -> Result<Compilation, Vec<Diagnostic>> {
    let project = load_project(workspace_root)?;
    Ok(compile(&project))
}
