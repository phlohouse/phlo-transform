//! Compiled workspace output.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::diagnostics::{Diagnostic, Severity};
use crate::graph::{Dependency, TransformGraph};
use crate::identity::{ModelId, Namespace, SourceId};
use crate::model::{ModelConfig, ModelOrigin, Relation, TestId, TransformRoot};

/// A fully resolved model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledModel {
    pub id: ModelId,
    /// Physical namespace used for relative resolution.
    pub namespace: Namespace,
    /// Namespace-relative physical path.
    pub path: Vec<String>,
    pub origin: ModelOrigin,
    pub sql: String,
    /// Owning workflow, when the model lives in a workflow transform root.
    pub workflow: Option<String>,
    /// Effective configuration after precedence resolution.
    pub config: ModelConfig,
    /// Physical target relation.
    pub target: Relation,
    /// SQL with workspace relations rewritten to physical targets.
    pub compiled_sql: String,
    /// Inferred output schema, when analysis succeeded.
    pub schema: crate::semantic::ModelSchema,
    /// Explicit compiler limitations encountered while analysing this model.
    pub limitations: Vec<String>,
    /// Logical assertions derived from directives/contracts.
    pub assertions: Vec<crate::semantic::Assertion>,
    /// Explicit contract, when declared.
    pub contract: Option<crate::semantic::ModelContract>,
    /// Content-addressed desired version.
    pub version: crate::version::ModelVersion,
    /// Pinned identity from `-- @id`, when present and valid.
    pub pinned_id: Option<ModelId>,
    /// Resolved dependencies, sorted and deduplicated.
    pub dependencies: Vec<Dependency>,
}

impl CompiledModel {
    /// The workspace-relative path, if file-backed.
    pub fn path_display(&self) -> Option<String> {
        self.origin
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
    }

    pub fn model_dependencies(&self) -> impl Iterator<Item = &ModelId> {
        self.dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                Dependency::Model(id) => Some(id),
                Dependency::Source(_) => None,
            })
    }

    pub fn source_dependencies(&self) -> impl Iterator<Item = &SourceId> {
        self.dependencies
            .iter()
            .filter_map(|dependency| match dependency {
                Dependency::Source(id) => Some(id),
                Dependency::Model(_) => None,
            })
    }
}

/// A fully resolved custom test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompiledTest {
    pub id: TestId,
    pub origin: ModelOrigin,
    pub sql: String,
    /// SQL with workspace relations rewritten to physical targets.
    pub compiled_sql: String,
    /// Workspace models the test reads.
    pub targets: Vec<ModelId>,
    /// External relations the test reads.
    pub sources: Vec<SourceId>,
    /// True when the test was generated from assertions/contracts.
    pub generated: bool,
}

impl CompiledTest {
    pub fn path_display(&self) -> Option<String> {
        self.origin
            .path
            .as_ref()
            .map(|path| path.to_string_lossy().replace('\\', "/"))
    }
}

/// The result of compiling a semantic project.
///
/// Compilation is best-effort: even when diagnostics are present the compiler
/// returns the models and graph it was able to construct, so `list` and
/// `inspect` remain useful. `is_ok` is the single source of truth for whether
/// `check` should fail.
#[derive(Clone, Debug)]
pub struct Compilation {
    pub workspace_root: Option<PathBuf>,
    pub roots: Vec<TransformRoot>,
    pub models: Vec<CompiledModel>,
    pub tests: Vec<CompiledTest>,
    pub graph: TransformGraph,
    pub diagnostics: Vec<Diagnostic>,
    index: BTreeMap<ModelId, usize>,
    test_index: BTreeMap<TestId, usize>,
}

impl Compilation {
    pub(crate) fn new(
        workspace_root: Option<PathBuf>,
        roots: Vec<TransformRoot>,
        models: Vec<CompiledModel>,
        tests: Vec<CompiledTest>,
        diagnostics: Vec<Diagnostic>,
    ) -> Self {
        let index = models
            .iter()
            .enumerate()
            .map(|(position, model)| (model.id.clone(), position))
            .collect();
        let test_index = tests
            .iter()
            .enumerate()
            .map(|(position, test)| (test.id.clone(), position))
            .collect();
        let graph = TransformGraph::build(&models);
        Self {
            workspace_root,
            roots,
            models,
            tests,
            graph,
            diagnostics,
            index,
            test_index,
        }
    }

    /// True when no error-severity diagnostics were produced.
    pub fn is_ok(&self) -> bool {
        !self
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
    }

    pub fn model(&self, id: &ModelId) -> Option<&CompiledModel> {
        self.index.get(id).map(|position| &self.models[*position])
    }

    /// Look a model up by dotted name or `model://` URI.
    pub fn model_by_name(&self, name: &str) -> Option<&CompiledModel> {
        let id = ModelId::parse(name).ok()?;
        self.model(&id)
    }

    pub fn test(&self, id: &TestId) -> Option<&CompiledTest> {
        self.test_index
            .get(id)
            .map(|position| &self.tests[*position])
    }

    /// Append generated tests and rebuild the test index.
    pub(crate) fn add_generated_tests(&mut self, generated: Vec<CompiledTest>) {
        if generated.is_empty() {
            return;
        }
        self.tests.extend(generated);
        self.tests.sort_by(|left, right| left.id.cmp(&right.id));
        self.test_index = self
            .tests
            .iter()
            .enumerate()
            .map(|(position, test)| (test.id.clone(), position))
            .collect();
    }

    /// Tests that read the given model.
    pub fn tests_for(&self, model: &ModelId) -> Vec<&CompiledTest> {
        self.tests
            .iter()
            .filter(|test| test.targets.iter().any(|target| target == model))
            .collect()
    }

    pub fn sources(&self) -> Vec<SourceId> {
        self.graph.source_ids()
    }

    /// Topological model order, or `None` when a cycle exists.
    pub fn topological_order(&self) -> Option<Vec<ModelId>> {
        self.graph.topological_order()
    }

    pub fn dependencies(&self, id: &ModelId) -> Vec<Dependency> {
        self.graph.dependencies(id)
    }

    pub fn dependents(&self, id: &ModelId) -> Vec<ModelId> {
        self.graph.dependents(id)
    }
}
