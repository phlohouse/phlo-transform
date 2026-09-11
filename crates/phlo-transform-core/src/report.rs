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
use crate::semantic::{ColumnRef, RelationRef};

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
    pub seed_count: usize,
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
    /// Short desired version hash.
    pub version: String,
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
    #[serde(skip_serializing_if = "is_false")]
    pub generated: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// An external source in `list`.
#[derive(Clone, Debug, Serialize)]
pub struct SourceSummary {
    pub id: String,
    pub name: String,
}

/// A CSV seed in `list`.
#[derive(Clone, Debug, Serialize)]
pub struct SeedSummary {
    pub name: String,
    pub path: String,
    /// Target schema when resolved by configuration; the adapter default
    /// applies otherwise.
    pub schema: Option<String>,
}

/// The result of `list`.
#[derive(Clone, Debug, Serialize)]
pub struct ListReport {
    pub models: Vec<ModelSummary>,
    pub sources: Vec<SourceSummary>,
    pub seeds: Vec<SeedSummary>,
    pub tests: Vec<TestSummary>,
}

/// A column in a model's inferred schema.
#[derive(Clone, Debug, Serialize)]
pub struct ColumnReport {
    pub name: String,
    pub data_type: String,
    pub nullability: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<String>,
}

/// Full detail for a single model in `inspect`.
#[derive(Clone, Debug, Serialize)]
pub struct ModelDetail {
    pub id: String,
    pub name: String,
    pub namespace: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    pub materialization: String,
    pub target: String,
    /// Short desired version hash.
    pub version: String,
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
    /// Inferred output columns.
    pub columns: Vec<ColumnReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub limitations: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<String>,
    pub sql: String,
}

/// Model-level lineage.
#[derive(Clone, Debug, Serialize)]
pub struct ModelLineageReport {
    pub model: String,
    pub upstream: Vec<String>,
    pub downstream: Vec<String>,
}

/// Column-level lineage.
#[derive(Clone, Debug, Serialize)]
pub struct ColumnLineageReport {
    pub column: String,
    pub direct: Vec<String>,
    pub transitive: Vec<String>,
}

/// Downstream impact of a column.
#[derive(Clone, Debug, Serialize)]
pub struct ImpactReport {
    pub column: String,
    pub downstream_columns: Vec<String>,
    pub downstream_models: Vec<String>,
    pub tests: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub consumers: Vec<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
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
            seed_count: self.seeds.len(),
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
        let seeds = self
            .seeds
            .iter()
            .map(|seed| SeedSummary {
                name: seed.name.clone(),
                path: seed.path.to_string_lossy().replace('\\', "/"),
                schema: seed.schema.clone(),
            })
            .collect();
        let tests = self.tests.iter().map(test_summary).collect();
        ListReport {
            models,
            sources,
            seeds,
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
                workflow: model.workflow.clone(),
                materialization: model.config.materialization.to_string(),
                target: model.target.display(),
                version: model.version.short().to_string(),
                path: model.path_display(),
                tags: model.config.tags.clone(),
                owner: model.config.owner.clone(),
                pinned_id: model.pinned_id.as_ref().map(ModelId::logical_name),
                depends_on,
                sources,
                used_by,
                tests,
                columns: model
                    .schema
                    .columns
                    .iter()
                    .map(|column| ColumnReport {
                        name: column.name.clone(),
                        data_type: column.data_type.to_string(),
                        nullability: column.nullability.to_string(),
                        inputs: column.inputs.iter().map(ColumnRef::display).collect(),
                    })
                    .collect(),
                limitations: model.limitations.clone(),
                assertions: model
                    .assertions
                    .iter()
                    .map(|assertion| assertion.describe())
                    .collect(),
                sql: model.sql.clone(),
            },
        })
    }

    /// Model-level upstream/downstream lineage.
    pub fn model_lineage_report(&self, id: &ModelId) -> Option<ModelLineageReport> {
        self.model(id)?;
        let upstream = transitive_models(self, id, true);
        let downstream = transitive_models(self, id, false);
        Some(ModelLineageReport {
            model: id.logical_name(),
            upstream: upstream.into_iter().map(|id| id.logical_name()).collect(),
            downstream: downstream.into_iter().map(|id| id.logical_name()).collect(),
        })
    }

    /// Direct and transitive lineage for one output column.
    pub fn column_lineage_report(&self, id: &ModelId, column: &str) -> Option<ColumnLineageReport> {
        let model = self.model(id)?;
        let output = model.schema.column(column)?;
        let direct: Vec<String> = output.inputs.iter().map(ColumnRef::display).collect();
        let mut transitive: Vec<String> = self
            .transitive_inputs(&ColumnRef::model(id.clone(), column))
            .into_iter()
            .map(|input| input.display())
            .collect();
        transitive.sort();
        transitive.dedup();
        Some(ColumnLineageReport {
            column: format!("{}.{}", id.logical_name(), column),
            direct,
            transitive,
        })
    }

    /// Downstream impact of a column.
    pub fn impact_report(&self, target: &ColumnRef) -> ImpactReport {
        self.impact_report_with(target, &crate::consumers::EmptyConsumerRegistry)
    }

    /// Impact including registered non-transform consumers.
    pub fn impact_report_with(
        &self,
        target: &ColumnRef,
        consumers: &dyn crate::consumers::ConsumerRegistry,
    ) -> ImpactReport {
        let mut dependents: std::collections::BTreeMap<ColumnRef, Vec<ColumnRef>> =
            std::collections::BTreeMap::new();
        for model in &self.models {
            for column in &model.schema.columns {
                let output = ColumnRef::model(model.id.clone(), &column.name);
                for input in &column.inputs {
                    dependents
                        .entry(input.clone())
                        .or_default()
                        .push(output.clone());
                }
            }
        }

        let mut affected: std::collections::BTreeSet<ColumnRef> = std::collections::BTreeSet::new();
        let mut frontier = vec![target.clone()];
        let mut visited = std::collections::BTreeSet::new();
        while let Some(column) = frontier.pop() {
            if !visited.insert(column.clone()) {
                continue;
            }
            if let Some(children) = dependents.get(&column) {
                for child in children {
                    if affected.insert(child.clone()) {
                        frontier.push(child.clone());
                    }
                }
            }
        }

        let mut models: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for column in &affected {
            if let RelationRef::Model(id) = &column.relation {
                models.insert(id.logical_name());
            }
        }
        let mut tests: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for model in &models {
            if let Ok(id) = ModelId::parse(model) {
                for test in self.tests_for(&id) {
                    tests.insert(test.id.to_string());
                }
            }
        }

        ImpactReport {
            column: target.display(),
            downstream_columns: affected.iter().map(ColumnRef::display).collect(),
            downstream_models: models.into_iter().collect(),
            tests: tests.into_iter().collect(),
            consumers: consumers.consumers(target),
        }
    }

    /// Recursively expand a column's inputs to leaf columns.
    fn transitive_inputs(&self, root: &ColumnRef) -> Vec<ColumnRef> {
        let mut visited = std::collections::BTreeSet::new();
        let mut leaves = std::collections::BTreeSet::new();
        let mut stack = vec![root.clone()];
        while let Some(column) = stack.pop() {
            if !visited.insert(column.clone()) {
                continue;
            }
            let inputs = self.direct_inputs(&column);
            if inputs.is_empty() {
                if &column != root {
                    leaves.insert(column);
                }
            } else {
                stack.extend(inputs);
            }
        }
        if leaves.is_empty() {
            leaves.insert(root.clone());
        }
        leaves.into_iter().collect()
    }

    fn direct_inputs(&self, column: &ColumnRef) -> Vec<ColumnRef> {
        match &column.relation {
            RelationRef::Model(id) => self
                .model(id)
                .and_then(|model| model.schema.column(&column.column))
                .map(|output| output.inputs.clone())
                .unwrap_or_default(),
            RelationRef::Source(_) => Vec::new(),
        }
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
                workflow: None,
            });
        }
        for test in &self.tests {
            nodes.push(GraphNodeArtifact {
                id: test.id.uri(),
                kind: "quality_gate",
                name: test.id.to_string(),
                workflow: None,
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
            for test in self.tests_for(&model.id) {
                edges.push(GraphEdgeArtifact {
                    from: model.id.uri(),
                    to: test.id.uri(),
                    kind: "quality_gate",
                });
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
        version: model.version.short().to_string(),
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
        generated: test.generated,
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
        workflow: model.workflow.clone(),
    }
}

fn transitive_models(compilation: &Compilation, id: &ModelId, upstream: bool) -> Vec<ModelId> {
    use std::collections::BTreeSet;
    let mut visited: BTreeSet<ModelId> = BTreeSet::new();
    let mut frontier: Vec<ModelId> = if upstream {
        compilation
            .dependencies(id)
            .into_iter()
            .filter_map(|dependency| match dependency {
                Dependency::Model(id) => Some(id),
                Dependency::Source(_) => None,
            })
            .collect()
    } else {
        compilation.dependents(id)
    };
    while let Some(current) = frontier.pop() {
        if !visited.insert(current.clone()) {
            continue;
        }
        let next: Vec<ModelId> = if upstream {
            compilation
                .dependencies(&current)
                .into_iter()
                .filter_map(|dependency| match dependency {
                    Dependency::Model(id) => Some(id),
                    Dependency::Source(_) => None,
                })
                .collect()
        } else {
            compilation.dependents(&current)
        };
        frontier.extend(next);
    }
    let mut result: Vec<ModelId> = visited.into_iter().collect();
    result.sort();
    result
}
