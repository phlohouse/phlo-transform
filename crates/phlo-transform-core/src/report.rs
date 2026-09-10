//! Structured, serialisable reports for the semantic CLI commands.
//!
//! Human output and `--json` output are both derived from these structures so
//! that no semantic information is only available in formatted text.

use serde::Serialize;

use crate::compiled::{Compilation, CompiledModel};
use crate::diagnostics::Diagnostic;
use crate::graph::Dependency;
use crate::identity::ModelId;
use crate::model::RootNamespaceStrategy;

/// A transform root in a report.
#[derive(Clone, Debug, Serialize)]
pub struct RootReport {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub strategy: &'static str,
    pub kind: &'static str,
}

/// The result of `check`.
#[derive(Clone, Debug, Serialize)]
pub struct CheckReport {
    pub ok: bool,
    pub workspace_root: String,
    pub roots: Vec<RootReport>,
    pub model_count: usize,
    pub source_count: usize,
    pub test_count: usize,
    pub diagnostics: Vec<Diagnostic>,
}

/// A model in `list`.
#[derive(Clone, Debug, Serialize)]
pub struct ModelSummary {
    /// Canonical URI, e.g. `model://assay/staging/raw`.
    pub id: String,
    /// Dotted logical name, e.g. `assay.staging.raw`.
    pub name: String,
    pub namespace: String,
    pub materialization: String,
    /// Physical target relation.
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub depends_on: Vec<String>,
    pub sources: Vec<String>,
}

/// A custom test in `list`.
#[derive(Clone, Debug, Serialize)]
pub struct TestSummary {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

/// An external source in `list`.
#[derive(Clone, Debug, Serialize)]
pub struct SourceSummary {
    pub id: String,
    pub name: String,
}

/// The result of `list`.
#[derive(Clone, Debug, Serialize)]
pub struct ListReport {
    pub models: Vec<ModelSummary>,
    pub sources: Vec<SourceSummary>,
    pub tests: Vec<TestSummary>,
}

/// Full detail for a single model in `inspect`.
#[derive(Clone, Debug, Serialize)]
pub struct ModelDetail {
    pub id: String,
    pub name: String,
    pub namespace: String,
    pub materialization: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_id: Option<String>,
    pub depends_on: Vec<String>,
    pub sources: Vec<String>,
    pub used_by: Vec<String>,
    pub tests: Vec<String>,
    pub sql: String,
}

/// The result of `inspect`.
#[derive(Clone, Debug, Serialize)]
pub struct InspectReport {
    pub model: ModelDetail,
}

/// A node in the serialised graph artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphNodeArtifact {
    pub id: String,
    pub kind: &'static str,
    pub name: String,
}

/// An edge in the serialised graph artifact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphEdgeArtifact {
    /// Dependent model.
    pub from: String,
    /// Dependency (model or source).
    pub to: String,
    pub kind: &'static str,
}

/// A machine-readable graph artifact, analogous to `graph.json`.
#[derive(Clone, Debug, Serialize)]
pub struct GraphArtifact {
    pub nodes: Vec<GraphNodeArtifact>,
    pub edges: Vec<GraphEdgeArtifact>,
}

impl Compilation {
    pub fn check_report(&self) -> CheckReport {
        let roots = self
            .roots
            .iter()
            .map(|root| RootReport {
                path: root.path.to_string_lossy().replace('\\', "/"),
                namespace: match &root.strategy {
                    RootNamespaceStrategy::Fixed(namespace) => Some(namespace.to_string()),
                    RootNamespaceStrategy::FirstSegment => None,
                },
                strategy: root.strategy.label(),
                kind: root.kind.label(),
            })
            .collect();

        CheckReport {
            ok: self.is_ok(),
            workspace_root: self
                .workspace_root
                .as_ref()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default(),
            roots,
            model_count: self.models.len(),
            source_count: self.sources().len(),
            test_count: self.tests.len(),
            diagnostics: self.diagnostics.clone(),
        }
    }

    pub fn list_report(&self) -> ListReport {
        let models = self.models.iter().map(model_summary).collect();
        let sources = self
            .sources()
            .iter()
            .map(|source| SourceSummary {
                id: source.uri(),
                name: source.logical_name(),
            })
            .collect();
        let tests = self.tests.iter().map(test_summary).collect();
        ListReport {
            models,
            sources,
            tests,
        }
    }

    pub fn inspect_report(&self, id: &ModelId) -> Option<InspectReport> {
        let model = self.model(id)?;
        let depends_on = model
            .model_dependencies()
            .map(|dependency| dependency.logical_name())
            .collect();
        let sources = model
            .source_dependencies()
            .map(|dependency| dependency.logical_name())
            .collect();
        let used_by = self
            .dependents(id)
            .into_iter()
            .map(|dependent| dependent.logical_name())
            .collect();
        let tests = self
            .tests_for(id)
            .into_iter()
            .map(|test| test.id.to_string())
            .collect();

        Some(InspectReport {
            model: ModelDetail {
                id: model.id.uri(),
                name: model.id.logical_name(),
                namespace: model.namespace.to_string(),
                materialization: model.config.materialization.to_string(),
                target: model.target.display(),
                path: model.path_display(),
                tags: model.config.tags.clone(),
                owner: model.config.owner.clone(),
                pinned_id: model.pinned_id.as_ref().map(ModelId::logical_name),
                depends_on,
                sources,
                used_by,
                tests,
                sql: model.sql.clone(),
            },
        })
    }

    pub fn graph_artifact(&self) -> GraphArtifact {
        let mut nodes: Vec<GraphNodeArtifact> = Vec::new();
        for model in &self.models {
            nodes.push(model_node(model));
        }
        for source in self.sources() {
            nodes.push(GraphNodeArtifact {
                id: source.uri(),
                kind: "source",
                name: source.logical_name(),
            });
        }

        let mut edges = Vec::new();
        for model in &self.models {
            for dependency in self.graph.dependencies(&model.id) {
                match dependency {
                    Dependency::Model(id) => edges.push(GraphEdgeArtifact {
                        from: model.id.uri(),
                        to: id.uri(),
                        kind: "model",
                    }),
                    Dependency::Source(id) => edges.push(GraphEdgeArtifact {
                        from: model.id.uri(),
                        to: id.uri(),
                        kind: "source",
                    }),
                }
            }
        }

        GraphArtifact { nodes, edges }
    }
}

fn model_summary(model: &CompiledModel) -> ModelSummary {
    ModelSummary {
        id: model.id.uri(),
        name: model.id.logical_name(),
        namespace: model.id.namespace().to_string(),
        materialization: model.config.materialization.to_string(),
        target: model.target.display(),
        path: model.path_display(),
        tags: model.config.tags.clone(),
        depends_on: model
            .model_dependencies()
            .map(|dependency| dependency.logical_name())
            .collect(),
        sources: model
            .source_dependencies()
            .map(|dependency| dependency.logical_name())
            .collect(),
    }
}

fn test_summary(test: &crate::compiled::CompiledTest) -> TestSummary {
    TestSummary {
        id: test.id.uri(),
        name: test.id.to_string(),
        targets: test
            .targets
            .iter()
            .map(|target| target.logical_name())
            .collect(),
        sources: test
            .sources
            .iter()
            .map(|source| source.logical_name())
            .collect(),
    }
}

fn model_node(model: &CompiledModel) -> GraphNodeArtifact {
    GraphNodeArtifact {
        id: model.id.uri(),
        kind: "model",
        name: model.id.logical_name(),
    }
}
