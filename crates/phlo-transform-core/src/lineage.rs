//! The canonical lineage graph.
//!
//! Lineage is the shared semantic representation every consumer reads:
//! `lineage` and `impact` reports, planning and explanation, the
//! OpenLineage exporter and future ingestion, versioned-diff and agent
//! APIs. It is built once from a [`Compilation`] — the compiler parses each
//! model exactly once and the graph indexes the result, so no consumer ever
//! re-derives lineage by reparsing SQL.
//!
//! The graph is bipartite at the relation level:
//!
//! ```text
//!   dataset ──input──▶ model ──output──▶ dataset ──contains──▶ column
//!        ╲                                                    ╱
//!         ╰────────── derives (model·dataset·column) ────────╯
//!   dataset ──tests──▶ test
//! ```
//!
//! Every edge points upstream → downstream in data-flow terms. Edge kinds:
//!
//! * [`LineageEdgeKind::Input`] — `dataset → model`: the model reads the
//!   dataset. This is the canonical dependency edge: a source relation, a
//!   seed, or an upstream model's output dataset all connect identically,
//!   so a future `External` dataset contributed by an ingestion system
//!   needs no special case.
//! * [`LineageEdgeKind::Output`] — `model → dataset`: the model produces
//!   its output dataset.
//! * [`LineageEdgeKind::Derives`] — `model → model`, `dataset → dataset` or
//!   `column → column`: the target derives from the source. Model- and
//!   dataset-level `Derives` edges are rollups over the canonical
//!   `model → output → dataset → input → model` chain — convenient for
//!   one-hop queries, never a substitute for the `Input` edge. At column
//!   granularity `Derives` is the analyzer's column lineage.
//! * [`LineageEdgeKind::Contains`] — `dataset → column`: containment.
//! * [`LineageEdgeKind::Tests`] — `dataset → test`: the test consumes and
//!   asserts on the dataset, so `impact(dataset)` reaches its tests
//!   naturally.
//!
//! Column-level `Derives` edges carry the analyzer's
//! [`ColumnInput`] metadata — [`Directness`], [`Transformation`],
//! [`LineageConfidence`] and the SQL expression where available — so
//! consumers can distinguish identity pass-through from transformations,
//! aggregation, join keys, filters and grouping, and can see when recorded
//! lineage is incomplete rather than exact.
//!
//! Node identity is URI-shaped and deliberately not tied to transform
//! models: `dataset://external/raw_samples` is a dataset whether today's
//! producer is a CSV seed, a declared source, or — later — an ingestion
//! system such as dlt contributing nodes and edges into the same graph.
//! Likewise model nodes carry the content-addressed version so that a
//! future `dataset://assay/results@v1 → dataset://assay/results@v2` view
//! does not require a redesign.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::visit::EdgeRef;
use petgraph::Direction;
use serde::Serialize;

use crate::compiled::Compilation;
use crate::identity::{ModelId, SourceId};
use crate::model::TestId;
use crate::semantic::{ColumnInput, Directness, LineageConfidence, RelationRef, Transformation};

/// Logical identity of a dataset: a model output, an external source or a
/// seed. URI form `dataset://<part>/<part>...`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DatasetId {
    parts: Vec<String>,
}

impl DatasetId {
    /// The dataset a model produces, e.g. `dataset://assay/results`.
    pub fn model(id: &ModelId) -> Self {
        let mut parts = vec![id.namespace().as_str().to_string()];
        parts.extend(id.path().iter().cloned());
        Self { parts }
    }

    /// The dataset an external source reads, e.g.
    /// `dataset://external/raw_samples`.
    pub fn source(id: &SourceId) -> Self {
        Self {
            parts: id.parts().to_vec(),
        }
    }

    /// The dataset behind any relation reference.
    pub fn relation(relation: &RelationRef) -> Self {
        match relation {
            RelationRef::Model(id) => Self::model(id),
            RelationRef::Source(id) => Self::source(id),
        }
    }

    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// The dotted logical name, e.g. `assay.results`.
    pub fn name(&self) -> String {
        self.parts.join(".")
    }

    /// The canonical URI, e.g. `dataset://assay/results`.
    pub fn uri(&self) -> String {
        format!("dataset://{}", self.parts.join("/"))
    }
}

/// Where a dataset's data comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DatasetKind {
    /// Produced by a transform model.
    Model,
    /// A declared external source.
    Source,
    /// A CSV seed loaded into a relation.
    Seed,
    /// Contributed by a system outside the transform compiler — reserved
    /// for future ingestion lineage.
    External,
}

/// A column within a dataset — the endpoint of column-level `Derives`
/// edges.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DatasetColumn {
    pub dataset: DatasetId,
    pub name: String,
}

impl DatasetColumn {
    /// The column URI, e.g. `dataset://assay/results#concentration`.
    pub fn uri(&self) -> String {
        format!("{}#{}", self.dataset.uri(), self.name)
    }
}

/// A node in the lineage graph.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LineageNode {
    /// A transform model — the process that produces a dataset.
    Model(ModelId),
    /// A logical dataset.
    Dataset(DatasetId),
    /// A column of a dataset.
    Column(DatasetColumn),
    /// A data-quality test.
    Test(TestId),
}

impl LineageNode {
    /// The node's canonical URI.
    pub fn uri(&self) -> String {
        match self {
            LineageNode::Model(id) => id.uri(),
            LineageNode::Dataset(id) => id.uri(),
            LineageNode::Column(column) => format!("{}#{}", column.dataset.uri(), column.name),
            LineageNode::Test(id) => id.uri(),
        }
    }

    /// The node's kind tag for serialisation and display.
    pub fn kind(&self) -> &'static str {
        match self {
            LineageNode::Model(_) => "model",
            LineageNode::Dataset(_) => "dataset",
            LineageNode::Column(_) => "column",
            LineageNode::Test(_) => "test",
        }
    }
}

/// Optional metadata carried by a node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct NodeMeta {
    /// Workspace-relative source path, when the node is file-backed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Content-addressed version for models — the seam where a future
    /// versioned-lineage view attaches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What produces the dataset, for dataset nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset_kind: Option<DatasetKind>,
    /// Physical target relation, for model-output datasets.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Column type/nullability, for column nodes when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullability: Option<String>,
    /// How complete the column's recorded inputs are.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<LineageConfidence>,
    /// Whether a test was generated from assertions.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub generated: bool,
    /// The model's materialization, for model nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub materialization: Option<String>,
    /// Owning workflow, for models in workflow roots.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
}

/// What kind of relationship an edge records.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineageEdgeKind {
    /// `dataset → model`: the dataset is an input the model reads.
    Input,
    /// `model → dataset`: the dataset is the model's output.
    Output,
    /// `model → model`, `dataset → dataset` or `column → column`: the
    /// target derives from the source.
    Derives,
    /// `dataset → column`: the dataset contains the column.
    Contains,
    /// `dataset → test`: the test consumes and asserts on the dataset.
    Tests,
}

/// An edge in the lineage graph. Only column-level `Derives` edges carry
/// transformation metadata today; every other kind leaves it empty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LineageEdge {
    pub kind: LineageEdgeKind,
    /// Whether the input contributes values directly or only influences
    /// which rows/values are selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directness: Option<Directness>,
    /// The transformation applied between source and target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transformation: Option<Transformation>,
    /// How certain the recorded lineage is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<LineageConfidence>,
    /// The SQL expression responsible, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expression: Option<String>,
}

impl LineageEdge {
    fn plain(kind: LineageEdgeKind) -> Self {
        Self {
            kind,
            directness: None,
            transformation: None,
            confidence: None,
            expression: None,
        }
    }

    fn from_input(input: &ColumnInput) -> Self {
        Self {
            kind: LineageEdgeKind::Derives,
            directness: Some(input.directness),
            transformation: Some(input.transformation),
            confidence: Some(input.confidence),
            expression: input.expression.clone(),
        }
    }
}

/// A serialisable view of the whole graph — the `lineage --json` document
/// and the interchange format future exporters consume.
#[derive(Clone, Debug, Serialize)]
pub struct LineageDocument {
    pub nodes: Vec<NodeJson>,
    pub edges: Vec<EdgeJson>,
}

/// A node in the serialised graph document.
#[derive(Clone, Debug, Serialize)]
pub struct NodeJson {
    pub uri: String,
    pub kind: &'static str,
    pub name: String,
    #[serde(flatten)]
    pub meta: NodeMeta,
}

/// An edge in the serialised graph document.
#[derive(Clone, Debug, Serialize)]
pub struct EdgeJson {
    pub from: String,
    pub to: String,
    #[serde(flatten)]
    pub edge: LineageEdge,
}

/// The canonical lineage graph, built once per [`Compilation`].
#[derive(Clone, Debug, Default)]
pub struct LineageGraph {
    graph: DiGraph<LineageNode, LineageEdge>,
    nodes: BTreeMap<LineageNode, NodeIndex>,
    meta: BTreeMap<LineageNode, NodeMeta>,
}

impl LineageGraph {
    /// Build the canonical graph from a completed compilation.
    ///
    /// Nodes are inserted in sorted order and edges deduplicated, so the
    /// graph and every traversal over it are deterministic.
    pub fn build(compilation: &Compilation) -> Self {
        let mut graph = LineageGraph::default();
        let seeds: Vec<&crate::compiled::CompiledSeed> = {
            let mut seeds: Vec<_> = compilation.seeds.iter().collect();
            seeds.sort_by(|left, right| left.name.cmp(&right.name));
            seeds
        };

        // Model nodes and their output datasets.
        for model in &compilation.models {
            graph.ensure_node(
                LineageNode::Model(model.id.clone()),
                NodeMeta {
                    path: model.path_display(),
                    version: Some(model.version.hash.clone()),
                    materialization: Some(model.config.materialization.to_string()),
                    workflow: model.workflow.clone(),
                    ..NodeMeta::default()
                },
            );
            let output = DatasetId::model(&model.id);
            // Only materializations that create a physical relation get a
            // target — an ephemeral model must never advertise one.
            let physical = !matches!(
                model.config.materialization,
                phlo_transform_sql::Materialization::Ephemeral
            );
            graph.ensure_node(
                LineageNode::Dataset(output.clone()),
                NodeMeta {
                    dataset_kind: Some(DatasetKind::Model),
                    target: physical.then(|| model.target.display()),
                    materialization: Some(model.config.materialization.to_string()),
                    ..NodeMeta::default()
                },
            );
            graph.add_edge(
                LineageNode::Model(model.id.clone()),
                LineageNode::Dataset(output),
                LineageEdge::plain(LineageEdgeKind::Output),
            );
        }

        // Source and seed datasets.
        for source in compilation.sources() {
            let seed = seeds.iter().find(|seed| seed_matches(&source, seed));
            let meta = match seed {
                Some(seed) => NodeMeta {
                    dataset_kind: Some(DatasetKind::Seed),
                    path: Some(seed.path.to_string_lossy().replace('\\', "/")),
                    ..NodeMeta::default()
                },
                None => NodeMeta {
                    dataset_kind: Some(DatasetKind::Source),
                    ..NodeMeta::default()
                },
            };
            graph.ensure_node(LineageNode::Dataset(DatasetId::source(&source)), meta);
        }

        // Model-level edges from the resolved dependencies — the same data
        // the scheduling graph uses, in data-flow direction. Every input is
        // a dataset: the canonical `dataset ──input──▶ model` edge exists
        // for sources and upstream model outputs alike. The `model → model`
        // and `dataset → dataset` edges are rollups for one-hop queries.
        for model in &compilation.models {
            for dependency in &model.dependencies {
                let input_dataset = match dependency {
                    crate::graph::Dependency::Model(id) => DatasetId::model(id),
                    crate::graph::Dependency::Source(id) => DatasetId::source(id),
                };
                graph.add_edge(
                    LineageNode::Dataset(input_dataset.clone()),
                    LineageNode::Model(model.id.clone()),
                    LineageEdge::plain(LineageEdgeKind::Input),
                );
                if let crate::graph::Dependency::Model(id) = dependency {
                    graph.add_edge(
                        LineageNode::Model(id.clone()),
                        LineageNode::Model(model.id.clone()),
                        LineageEdge::plain(LineageEdgeKind::Derives),
                    );
                }
                graph.add_edge(
                    LineageNode::Dataset(input_dataset),
                    LineageNode::Dataset(DatasetId::model(&model.id)),
                    LineageEdge::plain(LineageEdgeKind::Derives),
                );
            }
        }

        // Column nodes and column-level derives edges.
        for model in &compilation.models {
            let output_dataset = DatasetId::model(&model.id);
            for column in &model.schema.columns {
                let target = LineageNode::Column(DatasetColumn {
                    dataset: output_dataset.clone(),
                    name: column.name.clone(),
                });
                graph.ensure_node(
                    target.clone(),
                    NodeMeta {
                        data_type: Some(column.data_type.to_string()),
                        nullability: Some(column.nullability.to_string()),
                        confidence: Some(column.confidence),
                        ..NodeMeta::default()
                    },
                );
                graph.add_edge(
                    LineageNode::Dataset(output_dataset.clone()),
                    target.clone(),
                    LineageEdge::plain(LineageEdgeKind::Contains),
                );
                for input in &column.inputs {
                    let source_column = DatasetColumn {
                        dataset: DatasetId::relation(&input.column.relation),
                        name: input.column.column.clone(),
                    };
                    let source = LineageNode::Column(source_column.clone());
                    // Source columns are created on first reference — the
                    // compiler only learns about the columns a model reads.
                    graph.ensure_node(source.clone(), NodeMeta::default());
                    graph.add_edge(
                        LineageNode::Dataset(source_column.dataset.clone()),
                        source.clone(),
                        LineageEdge::plain(LineageEdgeKind::Contains),
                    );
                    graph.add_edge(source, target.clone(), LineageEdge::from_input(input));
                }
            }
        }

        // Test nodes and their tested datasets.
        for test in &compilation.tests {
            let node = LineageNode::Test(test.id.clone());
            graph.ensure_node(
                node.clone(),
                NodeMeta {
                    path: test.path_display(),
                    generated: test.generated,
                    ..NodeMeta::default()
                },
            );
            for target in &test.targets {
                graph.add_edge(
                    LineageNode::Dataset(DatasetId::model(target)),
                    node.clone(),
                    LineageEdge::plain(LineageEdgeKind::Tests),
                );
            }
            for source in &test.sources {
                graph.add_edge(
                    LineageNode::Dataset(DatasetId::source(source)),
                    node.clone(),
                    LineageEdge::plain(LineageEdgeKind::Tests),
                );
            }
        }

        graph
    }

    fn ensure_node(&mut self, node: LineageNode, meta: NodeMeta) -> NodeIndex {
        if let Some(index) = self.nodes.get(&node) {
            let index = *index;
            // Merge metadata: a lazily created node (e.g. a source column)
            // may gain details when a later pass knows more about it.
            let slot = self.meta.entry(node).or_default();
            slot.merge(meta);
            return index;
        }
        let index = self.graph.add_node(node.clone());
        self.meta.insert(node.clone(), meta);
        self.nodes.insert(node, index);
        index
    }

    fn add_edge(&mut self, from: LineageNode, to: LineageNode, edge: LineageEdge) {
        let (Some(from_index), Some(to_index)) =
            (self.nodes.get(&from).copied(), self.nodes.get(&to).copied())
        else {
            return;
        };
        let duplicate = self
            .graph
            .edges_connecting(from_index, to_index)
            .any(|existing| *existing.weight() == edge);
        if !duplicate {
            self.graph.add_edge(from_index, to_index, edge);
        }
    }

    /// The node record — identity plus metadata — when present.
    pub fn node(&self, node: &LineageNode) -> Option<(&LineageNode, &NodeMeta)> {
        self.nodes
            .get_key_value(node)
            .map(|(node, _)| (node, self.meta.get(node).unwrap_or(&EMPTY_META)))
    }

    /// Every node in deterministic order.
    pub fn nodes(&self) -> impl Iterator<Item = (&LineageNode, &NodeMeta)> {
        self.nodes
            .keys()
            .map(move |node| (node, self.meta.get(node).unwrap_or(&EMPTY_META)))
    }

    /// Every edge in deterministic order.
    pub fn edges(&self) -> Vec<(&LineageNode, &LineageNode, &LineageEdge)> {
        self.graph
            .edge_indices()
            .filter_map(|index| {
                let (from, to) = self.graph.edge_endpoints(index)?;
                Some((
                    self.graph.node_weight(from)?,
                    self.graph.node_weight(to)?,
                    self.graph.edge_weight(index)?,
                ))
            })
            .collect()
    }

    /// The output dataset a model produces.
    pub fn output_dataset(&self, model: &ModelId) -> DatasetId {
        DatasetId::model(model)
    }

    /// The datasets a model reads (models' outputs and sources), sorted.
    /// Every input is a `dataset ──input──▶ model` edge — source datasets
    /// and upstream model outputs connect identically.
    pub fn input_datasets(&self, model: &ModelId) -> Vec<DatasetId> {
        let Some(index) = self.nodes.get(&LineageNode::Model(model.clone())) else {
            return Vec::new();
        };
        let mut datasets: Vec<DatasetId> = self
            .graph
            .edges_directed(*index, Direction::Incoming)
            .filter_map(
                |edge| match (edge.weight().kind, self.graph.node_weight(edge.source())) {
                    (LineageEdgeKind::Input, Some(LineageNode::Dataset(dataset))) => {
                        Some(dataset.clone())
                    }
                    _ => None,
                },
            )
            .collect();
        datasets.sort();
        datasets.dedup();
        datasets
    }

    /// One hop upstream, sorted.
    pub fn upstream(&self, node: &LineageNode) -> Vec<LineageNode> {
        self.neighbors(node, Direction::Incoming)
    }

    /// One hop downstream, sorted.
    pub fn downstream(&self, node: &LineageNode) -> Vec<LineageNode> {
        self.neighbors(node, Direction::Outgoing)
    }

    /// All transitive upstream nodes in deterministic order — the nodes
    /// this node derives from, directly or transitively.
    pub fn upstream_transitive(&self, node: &LineageNode) -> Vec<LineageNode> {
        self.walk(node, Direction::Incoming)
    }

    /// All transitive downstream nodes in deterministic order — the
    /// impact set of the node.
    pub fn downstream_transitive(&self, node: &LineageNode) -> Vec<LineageNode> {
        self.walk(node, Direction::Outgoing)
    }

    /// The impact set of a node: everything transitively downstream.
    pub fn impact(&self, node: &LineageNode) -> Vec<LineageNode> {
        self.downstream_transitive(node)
    }

    /// The columns a column derives from, with their edge metadata.
    ///
    /// `include_indirect` controls whether edges that only influence row
    /// or value selection (filters, join keys, grouping) are returned
    /// alongside direct value contributions.
    pub fn column_upstream(
        &self,
        column: &DatasetColumn,
        include_indirect: bool,
    ) -> Vec<(DatasetColumn, LineageEdge)> {
        self.column_walk_one(column, Direction::Incoming, include_indirect)
    }

    /// The columns that derive from a column, with their edge metadata.
    pub fn column_downstream(
        &self,
        column: &DatasetColumn,
        include_indirect: bool,
    ) -> Vec<(DatasetColumn, LineageEdge)> {
        self.column_walk_one(column, Direction::Outgoing, include_indirect)
    }

    /// All columns transitively upstream of a column.
    pub fn column_upstream_transitive(
        &self,
        column: &DatasetColumn,
        include_indirect: bool,
    ) -> Vec<(DatasetColumn, LineageEdge)> {
        self.column_walk(column, Direction::Incoming, include_indirect)
    }

    /// All columns transitively downstream of a column.
    pub fn column_downstream_transitive(
        &self,
        column: &DatasetColumn,
        include_indirect: bool,
    ) -> Vec<(DatasetColumn, LineageEdge)> {
        self.column_walk(column, Direction::Outgoing, include_indirect)
    }

    /// The columns a dataset contains, sorted.
    pub fn dataset_columns(&self, dataset: &DatasetId) -> Vec<DatasetColumn> {
        let Some(index) = self.nodes.get(&LineageNode::Dataset(dataset.clone())) else {
            return Vec::new();
        };
        let mut columns: Vec<DatasetColumn> = self
            .graph
            .edges_directed(*index, Direction::Outgoing)
            .filter(|edge| edge.weight().kind == LineageEdgeKind::Contains)
            .filter_map(|edge| match self.graph.node_weight(edge.target()) {
                Some(LineageNode::Column(column)) => Some(column.clone()),
                _ => None,
            })
            .collect();
        columns.sort();
        columns
    }

    /// The dataset that contains a column, when present.
    pub fn column_dataset(&self, column: &DatasetColumn) -> Option<DatasetId> {
        let index = self.nodes.get(&LineageNode::Column(column.clone()))?;
        self.graph
            .edges_directed(*index, Direction::Incoming)
            .filter(|edge| edge.weight().kind == LineageEdgeKind::Contains)
            .filter_map(|edge| match self.graph.node_weight(edge.source()) {
                Some(LineageNode::Dataset(dataset)) => Some(dataset.clone()),
                _ => None,
            })
            .next()
    }

    /// A dataset by its dotted logical name (`external.samples`,
    /// `assay.results`) — how CLI users reference source datasets.
    pub fn dataset_by_name(&self, name: &str) -> Option<DatasetId> {
        self.nodes
            .keys()
            .filter_map(|node| match node {
                LineageNode::Dataset(id) => Some(id),
                _ => None,
            })
            .find(|id| id.name() == name)
            .cloned()
    }

    /// The model producing a dataset, when it is a model output.
    pub fn dataset_producer(&self, dataset: &DatasetId) -> Option<ModelId> {
        let index = self.nodes.get(&LineageNode::Dataset(dataset.clone()))?;
        self.graph
            .edges_directed(*index, Direction::Incoming)
            .filter(|edge| edge.weight().kind == LineageEdgeKind::Output)
            .filter_map(|edge| match self.graph.node_weight(edge.source()) {
                Some(LineageNode::Model(id)) => Some(id.clone()),
                _ => None,
            })
            .next()
    }

    /// The tests asserting on a dataset, sorted. Tests are consumers —
    /// `dataset ──tests──▶ test` — so they are also reachable through
    /// `impact(dataset)`.
    pub fn tests_for_dataset(&self, dataset: &DatasetId) -> Vec<TestId> {
        let Some(index) = self.nodes.get(&LineageNode::Dataset(dataset.clone())) else {
            return Vec::new();
        };
        let mut tests: Vec<TestId> = self
            .graph
            .edges_directed(*index, Direction::Outgoing)
            .filter(|edge| edge.weight().kind == LineageEdgeKind::Tests)
            .filter_map(|edge| match self.graph.node_weight(edge.target()) {
                Some(LineageNode::Test(id)) => Some(id.clone()),
                _ => None,
            })
            .collect();
        tests.sort();
        tests
    }

    /// A serialisable, deterministically ordered view of the graph.
    pub fn document(&self) -> LineageDocument {
        let nodes = self
            .nodes()
            .map(|(node, meta)| NodeJson {
                uri: node.uri(),
                kind: node.kind(),
                name: node_name(node),
                meta: meta.clone(),
            })
            .collect();
        let mut edges: Vec<EdgeJson> = self
            .edges()
            .into_iter()
            .map(|(from, to, edge)| EdgeJson {
                from: from.uri(),
                to: to.uri(),
                edge: edge.clone(),
            })
            .collect();
        edges.sort_by(|left, right| (&left.from, &left.to).cmp(&(&right.from, &right.to)));
        LineageDocument { nodes, edges }
    }

    /// A content fingerprint of the whole graph — nodes, edges and their
    /// metadata in canonical order, so two compilations that produce the
    /// same lineage hash identically. Promotion uses it to prove a lineage
    /// diff artifact still describes the candidate being promoted.
    pub fn fingerprint(&self) -> String {
        let mut document = self.document();
        // The document's edge order is only (from, to)-canonical; parallel
        // edges need their metadata in the sort key for a total order.
        document.edges.sort_by(|left, right| {
            (
                &left.from,
                &left.to,
                &left.edge.kind,
                &left.edge.directness,
                &left.edge.transformation,
                &left.edge.confidence,
                &left.edge.expression,
            )
                .cmp(&(
                    &right.from,
                    &right.to,
                    &right.edge.kind,
                    &right.edge.directness,
                    &right.edge.transformation,
                    &right.edge.confidence,
                    &right.edge.expression,
                ))
        });
        let canonical = serde_json::to_string(&document).expect("a lineage document serialises");
        crate::version::sha256_hex(&canonical)
    }

    /// A document restricted to a selection of models: the models, their
    /// output datasets and columns, the datasets they read, and the tests
    /// asserting on them. Edges are restricted to those in-scope nodes —
    /// the document answers "the selected subgraph", matching how selector
    /// scoping works elsewhere.
    pub fn document_for(&self, models: &BTreeSet<ModelId>) -> LineageDocument {
        self.subgraph(models).document()
    }

    /// The subgraph containing a selection of models — the models, their
    /// output datasets and columns, the datasets they read, the columns
    /// those outputs derive from, and the tests asserting on them.
    pub fn subgraph(&self, models: &BTreeSet<ModelId>) -> LineageGraph {
        let mut scope: BTreeSet<LineageNode> = BTreeSet::new();
        for id in models {
            let model_node = LineageNode::Model(id.clone());
            if !self.nodes.contains_key(&model_node) {
                continue;
            }
            scope.insert(model_node);
            let output = self.output_dataset(id);
            scope.insert(LineageNode::Dataset(output.clone()));
            for column in self.dataset_columns(&output) {
                scope.insert(LineageNode::Column(column));
            }
            for test in self.tests_for_dataset(&output) {
                scope.insert(LineageNode::Test(test));
            }
            for input in self.input_datasets(id) {
                scope.insert(LineageNode::Dataset(input));
            }
        }
        // Keep the columns a selected model's columns derive from, so the
        // scoped graph still shows where values come from.
        let columns: Vec<DatasetColumn> = scope
            .iter()
            .filter_map(|node| match node {
                LineageNode::Column(column) => Some(column.clone()),
                _ => None,
            })
            .collect();
        for column in columns {
            for (upstream, _) in self.column_upstream(&column, true) {
                scope.insert(LineageNode::Column(upstream));
            }
        }

        let mut graph = LineageGraph::default();
        for node in &scope {
            if self.nodes.contains_key(node) {
                let meta = self.meta.get(node).cloned().unwrap_or_default();
                graph.ensure_node(node.clone(), meta);
            }
        }
        for (from, to, edge) in self.edges() {
            if scope.contains(from) && scope.contains(to) {
                graph.add_edge(from.clone(), to.clone(), edge.clone());
            }
        }
        graph
    }

    fn neighbors(&self, node: &LineageNode, direction: Direction) -> Vec<LineageNode> {
        let Some(index) = self.nodes.get(node) else {
            return Vec::new();
        };
        let mut neighbors: Vec<LineageNode> = self
            .graph
            .neighbors_directed(*index, direction)
            .filter_map(|index| self.graph.node_weight(index).cloned())
            .collect();
        neighbors.sort();
        neighbors.dedup();
        neighbors
    }

    fn walk(&self, node: &LineageNode, direction: Direction) -> Vec<LineageNode> {
        let mut visited: BTreeSet<LineageNode> = BTreeSet::new();
        let mut frontier: VecDeque<LineageNode> = VecDeque::from([node.clone()]);
        while let Some(current) = frontier.pop_front() {
            for next in self.neighbors(&current, direction) {
                if visited.insert(next.clone()) {
                    frontier.push_back(next);
                }
            }
        }
        visited.into_iter().collect()
    }

    fn column_walk_one(
        &self,
        column: &DatasetColumn,
        direction: Direction,
        include_indirect: bool,
    ) -> Vec<(DatasetColumn, LineageEdge)> {
        let Some(index) = self.nodes.get(&LineageNode::Column(column.clone())) else {
            return Vec::new();
        };
        let mut found: Vec<(DatasetColumn, LineageEdge)> = self
            .graph
            .edges_directed(*index, direction)
            .filter(|edge| edge.weight().kind == LineageEdgeKind::Derives)
            .filter(|edge| {
                include_indirect || edge.weight().directness != Some(Directness::Indirect)
            })
            .filter_map(|edge| {
                let other = match direction {
                    Direction::Incoming => edge.source(),
                    Direction::Outgoing => edge.target(),
                };
                match self.graph.node_weight(other) {
                    Some(LineageNode::Column(column)) => {
                        Some((column.clone(), edge.weight().clone()))
                    }
                    _ => None,
                }
            })
            .collect();
        found.sort_by(|left, right| left.0.cmp(&right.0));
        found
    }

    fn column_walk(
        &self,
        column: &DatasetColumn,
        direction: Direction,
        include_indirect: bool,
    ) -> Vec<(DatasetColumn, LineageEdge)> {
        // Breadth-first over Derives edges; for each visited column keep the
        // edge by which it was first reached.
        let mut reached: BTreeMap<DatasetColumn, LineageEdge> = BTreeMap::new();
        let mut frontier: VecDeque<DatasetColumn> = VecDeque::from([column.clone()]);
        let mut visited: BTreeSet<DatasetColumn> = BTreeSet::from([column.clone()]);
        while let Some(current) = frontier.pop_front() {
            for (next, edge) in self.column_walk_one(&current, direction, include_indirect) {
                if visited.insert(next.clone()) {
                    reached.insert(next.clone(), edge);
                    frontier.push_back(next);
                }
            }
        }
        reached.into_iter().collect()
    }
}

static EMPTY_META: NodeMeta = NodeMeta {
    path: None,
    version: None,
    dataset_kind: None,
    target: None,
    data_type: None,
    nullability: None,
    confidence: None,
    generated: false,
    materialization: None,
    workflow: None,
};

impl NodeMeta {
    fn merge(&mut self, other: NodeMeta) {
        if other.path.is_some() {
            self.path = other.path;
        }
        if other.version.is_some() {
            self.version = other.version;
        }
        if other.dataset_kind.is_some() {
            self.dataset_kind = other.dataset_kind;
        }
        if other.target.is_some() {
            self.target = other.target;
        }
        if other.data_type.is_some() {
            self.data_type = other.data_type;
        }
        if other.nullability.is_some() {
            self.nullability = other.nullability;
        }
        if other.confidence.is_some() {
            self.confidence = other.confidence;
        }
        if other.materialization.is_some() {
            self.materialization = other.materialization;
        }
        if other.workflow.is_some() {
            self.workflow = other.workflow;
        }
        self.generated |= other.generated;
    }
}

/// Whether a source relation is the table a seed loads into — the same
/// matching rule `git` change detection uses.
fn seed_matches(source: &SourceId, seed: &crate::compiled::CompiledSeed) -> bool {
    let name = source.logical_name();
    name == seed.name || name.ends_with(&format!(".{}", seed.name))
}

fn node_name(node: &LineageNode) -> String {
    match node {
        LineageNode::Model(id) => id.logical_name(),
        LineageNode::Dataset(id) => id.name(),
        LineageNode::Column(column) => column.name.clone(),
        LineageNode::Test(id) => id.to_string(),
    }
}
