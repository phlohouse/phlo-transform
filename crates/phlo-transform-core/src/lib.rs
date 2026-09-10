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

use std::path::Path;

pub use compile::compile;
pub use compiled::{Compilation, CompiledModel};
pub use diagnostics::{codes, Diagnostic, Severity};
pub use discovery::load_project;
pub use graph::{Dependency, EdgeKind, GraphNode, TransformGraph};
pub use identity::{IdentityError, ModelId, Namespace, SourceId};
pub use model::{
    FrontendKind, ModelOrigin, RootKind, RootNamespaceStrategy, RootRef, SemanticModel,
    SemanticProject, TransformRoot, TransformRootId,
};
pub use report::{
    CheckReport, GraphArtifact, GraphEdgeArtifact, GraphNodeArtifact, InspectReport, ListReport,
    ModelDetail, ModelSummary, RootReport, SourceSummary,
};

/// Load a native workspace and compile it in one step.
pub fn load_and_compile(workspace_root: &Path) -> Result<Compilation, Vec<Diagnostic>> {
    let project = load_project(workspace_root)?;
    Ok(compile(&project))
}
