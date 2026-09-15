//! Semantic diff between two lineage graphs.
//!
//! `lineage_diff` compares the base graph (what another ref or environment
//! compiled to) against the candidate graph (the current workspace) and
//! reports the structural delta: nodes added, removed or metadata-changed,
//! dependency edges gained or lost, and column-lineage edges whose
//! transformation metadata moved. Removed nodes carry the base-side
//! downstream consumers they orphan — the semantic half of impact
//! analysis, complementing the data-side `branch_diff` report.
//!
//! The report is plain serialisable data — the CLI prints it, the
//! `lineage_diff.json` artifact persists it, and agent tooling consumes
//! the same shape.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::lineage::{LineageEdge, LineageEdgeKind, LineageGraph, LineageNode, NodeMeta};

/// One node in the diff — identified by URI, labelled by kind and name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffNode {
    pub uri: String,
    pub kind: String,
    pub name: String,
}

/// A node present on both sides whose metadata changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeChange {
    pub uri: String,
    pub kind: String,
    pub name: String,
    /// `field: old -> new` descriptions, in field order.
    pub changes: Vec<String>,
}

/// One edge in the diff — identity is `(from, to, kind)` plus, for edges
/// carrying metadata, a `detail` summary distinguishing parallel edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffEdge {
    pub from: String,
    pub to: String,
    pub kind: String,
    /// Directness/transformation/expression summary — present only on edges
    /// that carry metadata (column `derives` edges), so two parallel edges
    /// between the same nodes remain distinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// An edge present on both sides whose recorded metadata changed —
/// a column-level `derives` edge whose transformation, directness,
/// confidence or expression moved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeChange {
    pub from: String,
    pub to: String,
    pub kind: String,
    /// `field: old -> new` descriptions.
    pub changes: Vec<String>,
}

/// What a removed node leaves behind: the nodes that consumed it on the
/// base side and are now reading a dataset or model that no longer exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageDiffImpact {
    /// URI of the removed node.
    pub node: String,
    /// Dotted names of its transitive downstream consumers on the base
    /// side (models, datasets and tests — not the node's own columns).
    pub orphans: Vec<String>,
}

/// A lineage path that disappeared or moved: `to` lost — or changed — its
/// `from` input on the base side, and everything downstream of `to` loses
/// or alters that contribution.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeImpact {
    pub from: String,
    pub to: String,
    pub kind: String,
    /// `removed` when the edge is gone entirely, `changed` when only its
    /// metadata (directness, transformation, expression) moved.
    pub change: String,
    /// Dotted names of `to`'s transitive downstream consumers on the base
    /// side — the nodes whose data no longer includes this path, or
    /// includes it altered.
    pub downstream: Vec<String>,
}

/// The structural delta between two lineage graphs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineageDiff {
    /// Label for the side the diff compares against (a git ref, `main`,
    /// a commit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
    /// Nodes only in the candidate graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes_added: Vec<DiffNode>,
    /// Nodes only in the base graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes_removed: Vec<DiffNode>,
    /// Nodes on both sides whose metadata changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes_changed: Vec<NodeChange>,
    /// Edges only in the candidate graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges_added: Vec<DiffEdge>,
    /// Edges only in the base graph.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges_removed: Vec<DiffEdge>,
    /// Edges on both sides whose metadata changed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edges_changed: Vec<EdgeChange>,
    /// Downstream consumers orphaned by each removed node.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub impacts: Vec<LineageDiffImpact>,
    /// Lineage paths that disappeared or changed — each removed or
    /// metadata-moved edge and the base-side downstream it fed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge_impacts: Vec<EdgeImpact>,
}

impl LineageDiff {
    /// Whether the diff reports any change at all.
    pub fn is_empty(&self) -> bool {
        self.nodes_added.is_empty()
            && self.nodes_removed.is_empty()
            && self.nodes_changed.is_empty()
            && self.edges_added.is_empty()
            && self.edges_removed.is_empty()
            && self.edges_changed.is_empty()
    }

    /// Total count of changed entries — for one-line summaries.
    pub fn change_count(&self) -> usize {
        self.nodes_added.len()
            + self.nodes_removed.len()
            + self.nodes_changed.len()
            + self.edges_added.len()
            + self.edges_removed.len()
            + self.edges_changed.len()
    }
}

/// Diff `candidate` against `base`. Deterministic: both graphs iterate in
/// sorted order and every report list is sorted by its identity fields.
pub fn lineage_diff(base: &LineageGraph, candidate: &LineageGraph) -> LineageDiff {
    let mut diff = LineageDiff::default();

    // --- Nodes ---------------------------------------------------------
    let base_nodes: BTreeMap<LineageNode, NodeMeta> =
        base.nodes().map(|(n, m)| (n.clone(), m.clone())).collect();
    let candidate_nodes: BTreeMap<LineageNode, NodeMeta> = candidate
        .nodes()
        .map(|(n, m)| (n.clone(), m.clone()))
        .collect();

    for (node, meta) in &candidate_nodes {
        match base_nodes.get(node) {
            None => diff.nodes_added.push(diff_node(node)),
            Some(base_meta) => {
                let changes = meta_changes(base_meta, meta);
                if !changes.is_empty() {
                    let entry = diff_node(node);
                    diff.nodes_changed.push(NodeChange {
                        uri: entry.uri,
                        kind: entry.kind,
                        name: entry.name,
                        changes,
                    });
                }
            }
        }
    }
    for node in base_nodes.keys() {
        if !candidate_nodes.contains_key(node) {
            diff.nodes_removed.push(diff_node(node));
        }
    }

    // --- Edges ---------------------------------------------------------
    // Edges are a multiset, not a map: `LineageGraph` permits parallel
    // edges between the same nodes when their metadata differs (two column
    // `derives` edges with different directness or expressions). Keying by
    // `(from, to, kind)` alone would collapse them.
    type EdgeKey = (LineageNode, LineageNode, LineageEdgeKind);
    let edge_map = |graph: &LineageGraph| -> BTreeMap<EdgeKey, Vec<LineageEdge>> {
        let mut map: BTreeMap<EdgeKey, Vec<LineageEdge>> = BTreeMap::new();
        for (from, to, edge) in graph.edges() {
            map.entry((from.clone(), to.clone(), edge.kind))
                .or_default()
                .push(edge.clone());
        }
        // Sorted bags make the pairwise comparison deterministic.
        for edges in map.values_mut() {
            edges.sort_by(|left, right| edge_sort_key(left).cmp(&edge_sort_key(right)));
        }
        map
    };
    let base_edges = edge_map(base);
    let candidate_edges = edge_map(candidate);

    for (key, candidate_bag) in &candidate_edges {
        let (from, to, kind) = key;
        let Some(base_bag) = base_edges.get(key) else {
            for edge in candidate_bag {
                diff.edges_added.push(diff_edge(from, to, *kind, edge));
            }
            continue;
        };
        // Cancel identical edges pairwise; leftovers on each side are then
        // paired positionally as metadata changes, with any excess reported
        // as added/removed.
        let mut base_left: Vec<&LineageEdge> = Vec::new();
        let mut candidate_left: Vec<&LineageEdge> = Vec::new();
        let mut base_iter = base_bag.iter().peekable();
        let mut candidate_iter = candidate_bag.iter().peekable();
        loop {
            match (base_iter.peek(), candidate_iter.peek()) {
                (Some(base_edge), Some(candidate_edge)) if base_edge == candidate_edge => {
                    base_iter.next();
                    candidate_iter.next();
                }
                (Some(base_edge), Some(candidate_edge)) => {
                    // Both are sorted by metadata — drop the smaller side.
                    if edge_sort_key(base_edge) <= edge_sort_key(candidate_edge) {
                        base_left.push(base_iter.next().expect("peeked"));
                    } else {
                        candidate_left.push(candidate_iter.next().expect("peeked"));
                    }
                }
                (Some(_), None) => base_left.push(base_iter.next().expect("peeked")),
                (None, Some(_)) => candidate_left.push(candidate_iter.next().expect("peeked")),
                (None, None) => break,
            }
        }
        // Equal leftovers pair as metadata changes; an excess reports as
        // genuinely added or removed edges.
        for (base_edge, candidate_edge) in base_left.iter().zip(candidate_left.iter()) {
            let changes = edge_changes(base_edge, candidate_edge);
            debug_assert!(
                !changes.is_empty(),
                "leftover edges always differ in metadata"
            );
            diff.edges_changed.push(EdgeChange {
                from: from.uri(),
                to: to.uri(),
                kind: edge_kind_label(*kind).to_string(),
                changes,
            });
        }
        for edge in base_left.iter().skip(candidate_left.len()) {
            diff.edges_removed.push(diff_edge(from, to, *kind, edge));
        }
        for edge in candidate_left.iter().skip(base_left.len()) {
            diff.edges_added.push(diff_edge(from, to, *kind, edge));
        }
    }
    for (key, base_bag) in &base_edges {
        if !candidate_edges.contains_key(key) {
            let (from, to, kind) = key;
            for edge in base_bag {
                diff.edges_removed.push(diff_edge(from, to, *kind, edge));
            }
        }
    }

    // --- Impacts ---------------------------------------------------------
    // A removed node's orphans are its base-side downstream consumers —
    // the things that were reading it and now face a gap.
    for node in &diff.nodes_removed {
        let base_node = node_key(base, &node.uri);
        let Some(base_node) = base_node else {
            continue;
        };
        // Model and output dataset share a logical name — dedupe so a
        // consumer isn't listed twice.
        let mut seen = std::collections::BTreeSet::new();
        let orphans: Vec<String> = base
            .downstream_transitive(&base_node)
            .into_iter()
            .filter_map(|node| node_name(&node))
            .filter(|name| seen.insert(name.clone()))
            .collect();
        if !orphans.is_empty() {
            diff.impacts.push(LineageDiffImpact {
                node: node.uri.clone(),
                orphans,
            });
        }
    }

    // A removed or metadata-moved edge is a lineage path that disappeared
    // or changed: everything downstream of the edge's consumer loses — or
    // alters — that contribution. Base-side traversal only: the path no
    // longer exists on the candidate.
    let mut moved: Vec<(String, String, String, &str)> = Vec::new();
    for edge in &diff.edges_removed {
        moved.push((
            edge.from.clone(),
            edge.to.clone(),
            edge.kind.clone(),
            "removed",
        ));
    }
    for edge in &diff.edges_changed {
        moved.push((
            edge.from.clone(),
            edge.to.clone(),
            edge.kind.clone(),
            "changed",
        ));
    }
    for (from, to, kind, change) in moved {
        let Some(to_node) = node_key(base, &to) else {
            continue;
        };
        let mut seen = std::collections::BTreeSet::new();
        let downstream: Vec<String> = base
            .downstream_transitive(&to_node)
            .into_iter()
            .filter_map(|node| node_name(&node))
            .filter(|name| seen.insert(name.clone()))
            .collect();
        if !downstream.is_empty() {
            diff.edge_impacts.push(EdgeImpact {
                from,
                to,
                kind,
                change: change.to_string(),
                downstream,
            });
        }
    }

    diff
}

fn diff_node(node: &LineageNode) -> DiffNode {
    DiffNode {
        uri: node.uri(),
        kind: node.kind().to_string(),
        name: node_name(node).unwrap_or_else(|| node.uri()),
    }
}

/// The human label for a node — logical names where they exist.
fn node_name(node: &LineageNode) -> Option<String> {
    match node {
        LineageNode::Model(id) => Some(id.logical_name()),
        LineageNode::Dataset(id) => Some(id.name()),
        LineageNode::Column(column) => Some(format!("{}.{}", column.dataset.name(), column.name)),
        LineageNode::Test(id) => Some(id.name().to_string()),
    }
}

fn node_key(graph: &LineageGraph, uri: &str) -> Option<LineageNode> {
    graph
        .nodes()
        .map(|(node, _)| node.clone())
        .find(|node| node.uri() == uri)
}

fn diff_edge(
    from: &LineageNode,
    to: &LineageNode,
    kind: LineageEdgeKind,
    edge: &LineageEdge,
) -> DiffEdge {
    DiffEdge {
        from: from.uri(),
        to: to.uri(),
        kind: edge_kind_label(kind).to_string(),
        detail: edge_detail(edge),
    }
}

/// Total order over an edge's metadata — the bag-sort key for the multiset
/// diff. Identity fields (`from`, `to`, `kind`) are the map key; this orders
/// within it.
fn edge_sort_key(
    edge: &LineageEdge,
) -> (
    Option<crate::semantic::Directness>,
    Option<crate::semantic::Transformation>,
    Option<crate::semantic::LineageConfidence>,
    Option<&str>,
) {
    (
        edge.directness,
        edge.transformation,
        edge.confidence,
        edge.expression.as_deref(),
    )
}

/// A one-line summary of an edge's metadata, so parallel edges between the
/// same two nodes stay distinguishable in the report.
fn edge_detail(edge: &LineageEdge) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(directness) = &edge.directness {
        parts.push(format!("{directness:?}").to_lowercase());
    }
    if let Some(transformation) = &edge.transformation {
        parts.push(format!("{transformation:?}").to_lowercase());
    }
    if let Some(expression) = &edge.expression {
        parts.push(format!("`{}`", truncate(expression, 60)));
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn edge_kind_label(kind: LineageEdgeKind) -> &'static str {
    match kind {
        LineageEdgeKind::Input => "input",
        LineageEdgeKind::Output => "output",
        LineageEdgeKind::Derives => "derives",
        LineageEdgeKind::Contains => "contains",
        LineageEdgeKind::Tests => "tests",
    }
}

fn opt(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("-")
}

/// `field: old -> new` descriptions for every NodeMeta field that differs.
fn meta_changes(base: &NodeMeta, candidate: &NodeMeta) -> Vec<String> {
    let mut changes = Vec::new();
    if base.path != candidate.path {
        changes.push(format!(
            "path: {} -> {}",
            opt(&base.path),
            opt(&candidate.path)
        ));
    }
    if base.version != candidate.version {
        // Versions are content hashes — shorten for readability.
        changes.push(format!(
            "version: {} -> {}",
            short(opt(&base.version)),
            short(opt(&candidate.version))
        ));
    }
    if base.dataset_kind != candidate.dataset_kind {
        changes.push(format!(
            "dataset kind: {} -> {}",
            dataset_kind_label(base.dataset_kind),
            dataset_kind_label(candidate.dataset_kind)
        ));
    }
    if base.target != candidate.target {
        changes.push(format!(
            "target: {} -> {}",
            opt(&base.target),
            opt(&candidate.target)
        ));
    }
    if base.data_type != candidate.data_type {
        changes.push(format!(
            "type: {} -> {}",
            opt(&base.data_type),
            opt(&candidate.data_type)
        ));
    }
    if base.nullability != candidate.nullability {
        changes.push(format!(
            "nullability: {} -> {}",
            opt(&base.nullability),
            opt(&candidate.nullability)
        ));
    }
    if base.confidence != candidate.confidence {
        changes.push(format!(
            "confidence: {:?} -> {:?}",
            base.confidence, candidate.confidence
        ));
    }
    if base.generated != candidate.generated {
        changes.push(format!(
            "generated: {} -> {}",
            base.generated, candidate.generated
        ));
    }
    if base.materialization != candidate.materialization {
        changes.push(format!(
            "materialization: {} -> {}",
            opt(&base.materialization),
            opt(&candidate.materialization)
        ));
    }
    if base.workflow != candidate.workflow {
        changes.push(format!(
            "workflow: {} -> {}",
            opt(&base.workflow),
            opt(&candidate.workflow)
        ));
    }
    changes
}

fn dataset_kind_label(kind: Option<crate::lineage::DatasetKind>) -> &'static str {
    match kind {
        Some(crate::lineage::DatasetKind::Model) => "model",
        Some(crate::lineage::DatasetKind::Source) => "source",
        Some(crate::lineage::DatasetKind::Seed) => "seed",
        Some(crate::lineage::DatasetKind::External) => "external",
        None => "-",
    }
}

/// `field: old -> new` descriptions for every edge field that differs.
fn edge_changes(base: &LineageEdge, candidate: &LineageEdge) -> Vec<String> {
    let mut changes = Vec::new();
    if base.directness != candidate.directness {
        changes.push(format!(
            "directness: {:?} -> {:?}",
            base.directness, candidate.directness
        ));
    }
    if base.transformation != candidate.transformation {
        changes.push(format!(
            "transformation: {:?} -> {:?}",
            base.transformation, candidate.transformation
        ));
    }
    if base.confidence != candidate.confidence {
        changes.push(format!(
            "confidence: {:?} -> {:?}",
            base.confidence, candidate.confidence
        ));
    }
    if base.expression != candidate.expression {
        changes.push(format!(
            "expression: {} -> {}",
            truncate(base.expression.as_deref().unwrap_or("-"), 60),
            truncate(candidate.expression.as_deref().unwrap_or("-"), 60)
        ));
    }
    changes
}

/// `text` truncated to `max` characters — character-safe, never slicing
/// inside a UTF-8 sequence.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    format!("{}…", text.chars().take(max).collect::<String>())
}

fn short(hash: &str) -> String {
    hash.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::identity::ModelId;
    use crate::model::{SemanticModel, SemanticProject};

    fn graph(models: Vec<SemanticModel>) -> LineageGraph {
        let compilation = compile(&SemanticProject::in_memory(models));
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        LineageGraph::build(&compilation)
    }

    fn model(name: &str, sql: &str) -> SemanticModel {
        SemanticModel::in_memory(ModelId::parse(name).expect("model id"), sql)
    }

    /// names of all entries in a node list.
    fn names(nodes: &[DiffNode]) -> Vec<&str> {
        nodes.iter().map(|node| node.name.as_str()).collect()
    }

    #[test]
    fn identical_graphs_diff_empty() {
        let base = graph(vec![model("a.one", "select 1 as id")]);
        let candidate = graph(vec![model("a.one", "select 1 as id")]);
        let diff = lineage_diff(&base, &candidate);
        assert!(diff.is_empty(), "{diff:?}");
    }

    /// The fingerprint is the candidate identity promotion audits against:
    /// identical definitions hash identically, and any change to the graph
    /// — here a changed column expression — moves it.
    #[test]
    fn fingerprint_tracks_graph_identity() {
        let one = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.down", "select id from a.up"),
        ]);
        let same = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.down", "select id from a.up"),
        ]);
        assert_eq!(one.fingerprint(), same.fingerprint());

        let changed = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.down", "select id, id + 1 as next from a.up"),
        ]);
        assert_ne!(one.fingerprint(), changed.fingerprint());
    }

    #[test]
    fn added_and_removed_models_report() {
        let base = graph(vec![model("a.one", "select 1 as id")]);
        let candidate = graph(vec![model("a.two", "select 1 as id")]);
        let diff = lineage_diff(&base, &candidate);
        assert!(names(&diff.nodes_added).contains(&"a.two"));
        assert!(names(&diff.nodes_removed).contains(&"a.one"));
        // The output datasets move with their models.
        assert!(names(&diff.nodes_added).contains(&"a.two"));
        assert!(names(&diff.nodes_removed).contains(&"a.one"));
    }

    #[test]
    fn changed_model_reports_version_change() {
        let base = graph(vec![model("a.one", "select 1 as id")]);
        let candidate = graph(vec![model("a.one", "select 2 as id")]);
        let diff = lineage_diff(&base, &candidate);
        assert_eq!(diff.nodes_changed.len(), 1);
        assert_eq!(diff.nodes_changed[0].name, "a.one");
        assert!(
            diff.nodes_changed[0]
                .changes
                .iter()
                .any(|change| change.starts_with("version:")),
            "{:?}",
            diff.nodes_changed[0].changes
        );
    }

    #[test]
    fn new_dependency_edge_reports() {
        let base = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.down", "select 1 as id"),
        ]);
        let candidate = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.down", "select * from a.up"),
        ]);
        let diff = lineage_diff(&base, &candidate);
        // `a.down` now reads `a.up`'s output dataset — an `input` edge and
        // the `derives` rollups appear on the candidate side only.
        let added: Vec<(&str, &str)> = diff
            .edges_added
            .iter()
            .map(|edge| (edge.kind.as_str(), edge.to.as_str()))
            .collect();
        assert!(
            added
                .iter()
                .any(|(kind, to)| *kind == "input" && to.contains("model://a/down")),
            "{:?}",
            diff.edges_added
        );
        assert!(diff.edges_removed.is_empty());
    }

    #[test]
    fn removed_model_orphans_its_consumers() {
        // Base: down reads up. Candidate: up is gone — down remains and is
        // orphaned.
        let base = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.down", "select * from a.up"),
        ]);
        let candidate = graph(vec![model("a.down", "select * from a.up")]);
        let diff = lineage_diff(&base, &candidate);
        let impact = diff
            .impacts
            .iter()
            .find(|impact| impact.node.contains("a/up"))
            .expect("impact for a.up");
        assert!(
            impact.orphans.iter().any(|name| name == "a.down"),
            "{:?}",
            impact
        );
    }

    #[test]
    fn column_lineage_change_reports() {
        // Same model, different expression for the same output column —
        // the derives edge's expression metadata changes. Exact
        // assertions: a node `version:` change alone must not satisfy this.
        let base = graph(vec![
            model("a.src", "select 1 as x"),
            model("a.down", "select x + 1 as y from a.src"),
        ]);
        let candidate = graph(vec![
            model("a.src", "select 1 as x"),
            model("a.down", "select x + 2 as y from a.src"),
        ]);
        let diff = lineage_diff(&base, &candidate);
        let change = diff
            .edges_changed
            .iter()
            .find(|edge| edge.kind == "derives" && edge.to.contains("a/down#y"))
            .expect("changed derives edge into a.down.y");
        assert!(
            change.from.contains("a/src#x"),
            "expected the src.x -> down.y edge, got {change:?}"
        );
        assert!(
            change
                .changes
                .iter()
                .any(|field| field.starts_with("expression:")),
            "expected an expression change, got {:?}",
            change.changes
        );
    }

    #[test]
    fn parallel_edges_same_endpoints_compare_as_a_bag() {
        // `id` feeds `y` twice: directly through the SELECT and indirectly
        // through the WHERE filter — two `derives` edges between the same
        // column nodes, distinguished only by metadata.
        let base = graph(vec![
            model("a.src", "select 1 as id"),
            model("a.down", "select id as y from a.src where id > 0"),
        ]);
        let derives = |graph: &LineageGraph| {
            graph
                .edges()
                .into_iter()
                .filter(|(from, to, edge)| {
                    edge.kind == LineageEdgeKind::Derives
                        && from.uri().ends_with("a/src#id")
                        && to.uri().ends_with("a/down#y")
                })
                .count()
        };
        assert_eq!(derives(&base), 2, "base should carry parallel edges");

        // Tighten only the filter: the direct edge is untouched, the
        // indirect edge's expression moves.
        let candidate = graph(vec![
            model("a.src", "select 1 as id"),
            model("a.down", "select id as y from a.src where id > 5"),
        ]);
        assert_eq!(derives(&candidate), 2);
        let diff = lineage_diff(&base, &candidate);
        let changed: Vec<&EdgeChange> = diff
            .edges_changed
            .iter()
            .filter(|edge| edge.kind == "derives" && edge.to.contains("a/down#y"))
            .collect();
        assert_eq!(
            changed.len(),
            1,
            "exactly the filter edge should move, got {:?}",
            diff.edges_changed
        );
        assert!(
            changed[0]
                .changes
                .iter()
                .any(|field| field.starts_with("expression:")),
            "{:?}",
            changed[0].changes
        );
        // The surviving direct edge is not reported removed or re-added.
        assert!(
            !diff
                .edges_removed
                .iter()
                .chain(diff.edges_added.iter())
                .any(|edge| edge.to.contains("a/down#y")),
            "{:?} / {:?}",
            diff.edges_removed,
            diff.edges_added
        );
    }

    #[test]
    fn removed_dependency_edge_reports_impact() {
        // `a.down` stops reading `a.up`: every node survives, but the
        // lineage path disappears — an edge removal with downstream impact.
        let base = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.mid", "select id from a.up"),
            model("a.down", "select id from a.mid"),
        ]);
        let candidate = graph(vec![
            model("a.up", "select 1 as id"),
            model("a.mid", "select 1 as id"),
            model("a.down", "select id from a.mid"),
        ]);
        let diff = lineage_diff(&base, &candidate);
        let removed = diff
            .edges_removed
            .iter()
            .find(|edge| {
                edge.kind == "input" && edge.from.contains("a/up") && edge.to.contains("a/mid")
            })
            .expect("a.up -> a.mid input edge removed");
        assert!(removed.detail.is_none(), "input edges carry no metadata");
        let impact = diff
            .edge_impacts
            .iter()
            .find(|impact| {
                impact.from.contains("a/up")
                    && impact.to.contains("a/mid")
                    && impact.change == "removed"
            })
            .expect("edge impact for the removed a.up -> a.mid path");
        assert!(
            impact.downstream.iter().any(|name| name == "a.down"),
            "a.down loses the a.up contribution, got {:?}",
            impact.downstream
        );
    }

    #[test]
    fn utf8_expressions_truncate_without_panicking() {
        // A multibyte expression long enough to hit the 60-character
        // shortening path — byte slicing would split a code point.
        let padding = "é".repeat(40);
        let sql = format!("select '{padding}' || x as y from a.src");
        let base = graph(vec![
            model("a.src", "select 1 as x"),
            model("a.down", "select x as y from a.src"),
        ]);
        let candidate = graph(vec![model("a.src", "select 1 as x"), model("a.down", &sql)]);
        let diff = lineage_diff(&base, &candidate);
        assert!(
            !diff.edges_changed.is_empty() || !diff.edges_added.is_empty(),
            "expected the derives edge to move, got {diff:?}"
        );
    }
}
