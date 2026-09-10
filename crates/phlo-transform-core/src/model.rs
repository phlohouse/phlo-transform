//! The semantic model produced by frontends and consumed by the compiler.
//!
//! This is the frontend-agnostic boundary described in the compiler-spike
//! roadmap. The native Phlo filesystem frontend lives in [`crate::discovery`]
//! and produces these types; future frontends (for example a dbt importer)
//! lower into the same representation. No source-system-specific concepts
//! (dbt, Jinja, YAML) appear here.

use std::path::PathBuf;

use phlo_transform_sql::{Directives, Materialization};

use crate::diagnostics::Diagnostic;
use crate::identity::{ModelId, Namespace};
use crate::semantic::ModelContract;

/// Identifies a transform root within a project.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransformRootId(pub u32);

/// How a transform root derives namespaces for the files it contains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootNamespaceStrategy {
    /// Every file shares a fixed namespace, e.g. `workflows/assay/transforms`.
    Fixed(Namespace),
    /// The first directory below the root supplies the namespace, e.g.
    /// `transforms/<namespace>/...`.
    FirstSegment,
}

impl RootNamespaceStrategy {
    pub fn label(&self) -> &'static str {
        match self {
            RootNamespaceStrategy::Fixed(_) => "fixed",
            RootNamespaceStrategy::FirstSegment => "first_segment",
        }
    }
}

/// The kind of transform root, used for reporting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    /// The top-level `transforms/` directory.
    GlobalTransforms,
    /// A `workflows/<name>/transforms/` directory.
    Workflow,
    /// A configured additional root.
    Custom,
}

impl RootKind {
    pub fn label(self) -> &'static str {
        match self {
            RootKind::GlobalTransforms => "global",
            RootKind::Workflow => "workflow",
            RootKind::Custom => "custom",
        }
    }
}

/// A discovered transform root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransformRoot {
    pub id: TransformRootId,
    /// Workspace-relative directory.
    pub path: PathBuf,
    pub strategy: RootNamespaceStrategy,
    pub kind: RootKind,
}

/// Where a semantic model came from. Kept deliberately generic so core types
/// never depend on a specific source system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelOrigin {
    pub frontend: FrontendKind,
    /// Workspace-relative path for file-backed models.
    pub path: Option<PathBuf>,
}

impl ModelOrigin {
    pub fn native(path: PathBuf) -> Self {
        Self {
            frontend: FrontendKind::Native,
            path: Some(path),
        }
    }

    pub fn in_memory() -> Self {
        Self {
            frontend: FrontendKind::InMemory,
            path: None,
        }
    }
}

/// The frontend that produced a model. Not source-system specific.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontendKind {
    /// Native Phlo files discovered from the filesystem.
    Native,
    /// Constructed directly in memory, e.g. by an importer or a test.
    InMemory,
}

/// A model reference to its transform root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRef {
    pub id: TransformRootId,
    /// Path segments relative to the root directory.
    pub relative_path: Vec<String>,
}

/// Effective, resolved configuration for a model.
///
/// The frontend computes this by applying the documented precedence
/// (workspace → transform root → folder → model directive). The compiler only
/// consumes the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelConfig {
    pub materialization: Materialization,
    pub tags: Vec<String>,
    pub owner: Option<String>,
    /// Per-model target schema override, when configured.
    pub schema: Option<String>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            materialization: Materialization::View,
            tags: Vec::new(),
            owner: None,
            schema: None,
        }
    }
}

/// Workspace-wide defaults that affect physical targeting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceDefaults {
    pub materialization: Materialization,
    pub catalog: Option<String>,
    pub schema: Option<String>,
}

impl Default for WorkspaceDefaults {
    fn default() -> Self {
        Self {
            materialization: Materialization::View,
            catalog: None,
            schema: None,
        }
    }
}

/// A resolved physical relation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Relation {
    pub catalog: Option<String>,
    pub schema: String,
    pub table: String,
}

impl Relation {
    /// The relation as a quoted dotted SQL identifier.
    pub fn sql(&self) -> String {
        match &self.catalog {
            Some(catalog) => format!(
                "{}.{}.{}",
                quote_identifier(catalog),
                quote_identifier(&self.schema),
                quote_identifier(&self.table)
            ),
            None => format!(
                "{}.{}",
                quote_identifier(&self.schema),
                quote_identifier(&self.table)
            ),
        }
    }

    /// The dotted display form.
    pub fn display(&self) -> String {
        match &self.catalog {
            Some(catalog) => format!("{}.{}.{}", catalog, self.schema, self.table),
            None => format!("{}.{}", self.schema, self.table),
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// A single semantic model, prior to dependency resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticModel {
    /// Logical identity (derived or pinned).
    pub id: ModelId,
    /// Physical namespace used for relative reference resolution.
    pub namespace: Namespace,
    /// Namespace-relative logical path.
    pub path: Vec<String>,
    /// Root membership, when the model came from a transform root.
    pub root: Option<RootRef>,
    pub sql: String,
    pub directives: Directives,
    pub config: ModelConfig,
    /// Explicit contract, when declared in `phlo.toml`.
    pub contract: Option<ModelContract>,
    pub origin: ModelOrigin,
}

impl SemanticModel {
    /// Construct an in-memory semantic model from an identity and SQL.
    ///
    /// This is the factory used by architecture tests and by importers. It
    /// deliberately bypasses filesystem discovery.
    pub fn in_memory(id: ModelId, sql: impl Into<String>) -> Self {
        let namespace = id.namespace().clone();
        let path = id.path().to_vec();
        Self {
            id,
            namespace,
            path,
            root: None,
            sql: sql.into(),
            directives: Directives::default(),
            config: ModelConfig::default(),
            contract: None,
            origin: ModelOrigin::in_memory(),
        }
    }
}

/// Identifies a workspace test.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TestId {
    name: String,
}

impl TestId {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uri(&self) -> String {
        format!("test://{}", self.name.replace('.', "/"))
    }
}

impl std::fmt::Display for TestId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.name)
    }
}

/// A custom SQL test, discovered from `tests/**/*.sql`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticTest {
    pub id: TestId,
    pub sql: String,
    pub origin: ModelOrigin,
}

/// The complete semantic input to the compiler.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SemanticProject {
    /// Workspace root as provided by the caller, when file-backed.
    pub workspace_root: Option<PathBuf>,
    pub roots: Vec<TransformRoot>,
    pub models: Vec<SemanticModel>,
    pub tests: Vec<SemanticTest>,
    pub defaults: WorkspaceDefaults,
    /// Diagnostics raised while loading (for example malformed metadata).
    /// The compiler carries these through to its own output.
    pub diagnostics: Vec<Diagnostic>,
}

impl SemanticProject {
    /// Build a project directly from models, for in-memory use.
    pub fn in_memory(models: Vec<SemanticModel>) -> Self {
        Self {
            workspace_root: None,
            roots: Vec::new(),
            models,
            tests: Vec::new(),
            defaults: WorkspaceDefaults::default(),
            diagnostics: Vec::new(),
        }
    }
}
