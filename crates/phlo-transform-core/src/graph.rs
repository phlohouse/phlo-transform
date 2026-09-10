//! The transform dependency graph.
//!
//! Edges point from a model to each relation it references, so traversing
//! outgoing edges walks "what does this depend on" and incoming edges walk
//! "what depends on this". The graph supports deterministic topological
//! ordering and concrete cycle-path reporting.

use std::collections::{BTreeMap, BTreeSet};

use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;

use crate::compiled::CompiledModel;
use crate::identity::{ModelId, SourceId};

/// A resolved dependency of a model.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dependency {
    Model(ModelId),
    Source(SourceId),
}

impl Dependency {
    pub fn is_source(&self) -> bool {
        matches!(self, Dependency::Source(_))
    }
}

/// A node in the transform graph.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GraphNode {
    Model(ModelId),
    Source(SourceId),
}

/// The kind of edge between a model and a dependency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeKind {
    Model,
    Source,
}

/// A directed acyclic graph of models and the sources they read.
#[derive(Clone, Debug, Default)]
pub struct TransformGraph {
    graph: DiGraph<GraphNode, EdgeKind>,
    nodes: BTreeMap<GraphNode, NodeIndex>,
}

impl TransformGraph {
    /// Build the graph from compiled models.
    ///
    /// Nodes and edges are inserted in sorted order so that the graph and all
    /// traversals that iterate node indices are deterministic.
    pub fn build(models: &[CompiledModel]) -> Self {
        let mut graph = TransformGraph::default();

        // Insert model nodes first (sorted by the caller), then source nodes.
        for model in models {
            graph.ensure_node(GraphNode::Model(model.id.clone()));
        }
        let mut source_ids: BTreeSet<SourceId> = BTreeSet::new();
        for model in models {
            for dependency in &model.dependencies {
                if let Dependency::Source(id) = dependency {
                    source_ids.insert(id.clone());
                }
            }
        }
        for source in &source_ids {
            graph.ensure_node(GraphNode::Source(source.clone()));
        }

        for model in models {
            let from = graph.nodes[&GraphNode::Model(model.id.clone())];
            for dependency in &model.dependencies {
                let (node, kind) = match dependency {
                    Dependency::Model(id) => (GraphNode::Model(id.clone()), EdgeKind::Model),
                    Dependency::Source(id) => (GraphNode::Source(id.clone()), EdgeKind::Source),
                };
                let to = graph.nodes[&node];
                if graph.graph.find_edge(from, to).is_none() {
                    graph.graph.add_edge(from, to, kind);
                }
            }
        }

        graph
    }

    fn ensure_node(&mut self, node: GraphNode) -> NodeIndex {
        if let Some(index) = self.nodes.get(&node) {
            *index
        } else {
            let index = self.graph.add_node(node.clone());
            self.nodes.insert(node, index);
            index
        }
    }

    fn index_of(&self, id: &ModelId) -> Option<NodeIndex> {
        self.nodes.get(&GraphNode::Model(id.clone())).copied()
    }

    /// The model logical names, sorted.
    pub fn model_ids(&self) -> Vec<ModelId> {
        self.nodes
            .keys()
            .filter_map(|node| match node {
                GraphNode::Model(id) => Some(id.clone()),
                GraphNode::Source(_) => None,
            })
            .collect()
    }

    /// The external source ids, sorted.
    pub fn source_ids(&self) -> Vec<SourceId> {
        self.nodes
            .keys()
            .filter_map(|node| match node {
                GraphNode::Source(id) => Some(id.clone()),
                GraphNode::Model(_) => None,
            })
            .collect()
    }

    /// Direct dependencies of a model, sorted.
    pub fn dependencies(&self, id: &ModelId) -> Vec<Dependency> {
        let Some(index) = self.index_of(id) else {
            return Vec::new();
        };
        let mut result: Vec<Dependency> = self
            .graph
            .neighbors_directed(index, Direction::Outgoing)
            .map(|neighbour| match &self.graph[neighbour] {
                GraphNode::Model(dependency) => Dependency::Model(dependency.clone()),
                GraphNode::Source(dependency) => Dependency::Source(dependency.clone()),
            })
            .collect();
        result.sort();
        result
    }

    /// Models that directly depend on the given model, sorted.
    pub fn dependents(&self, id: &ModelId) -> Vec<ModelId> {
        let Some(index) = self.index_of(id) else {
            return Vec::new();
        };
        let mut result: Vec<ModelId> = self
            .graph
            .neighbors_directed(index, Direction::Incoming)
            .filter_map(|neighbour| match &self.graph[neighbour] {
                GraphNode::Model(dependent) => Some(dependent.clone()),
                GraphNode::Source(_) => None,
            })
            .collect();
        result.sort();
        result
    }

    /// Model-only dependency edges, used for scheduling.
    fn model_edges(&self) -> (BTreeMap<ModelId, usize>, BTreeMap<ModelId, Vec<ModelId>>) {
        let mut remaining: BTreeMap<ModelId, usize> = self
            .model_ids()
            .into_iter()
            .map(|id| (id, 0usize))
            .collect();
        let mut dependents: BTreeMap<ModelId, Vec<ModelId>> = BTreeMap::new();

        for node in self.nodes.keys() {
            let GraphNode::Model(id) = node else {
                continue;
            };
            for dependency in self.dependencies(id) {
                if let Dependency::Model(dependency_id) = dependency {
                    *remaining.get_mut(id).expect("model node exists") += 1;
                    dependents
                        .entry(dependency_id)
                        .or_default()
                        .push(id.clone());
                }
            }
        }

        for list in dependents.values_mut() {
            list.sort();
        }

        (remaining, dependents)
    }

    /// Topological model order with dependencies before dependents.
    ///
    /// Returns `None` when the model graph contains a cycle. The order is
    /// deterministic: among ready models the lexicographically smallest is
    /// chosen first.
    pub fn topological_order(&self) -> Option<Vec<ModelId>> {
        let (mut remaining, dependents) = self.model_edges();
        let mut ready: BTreeSet<ModelId> = remaining
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| id.clone())
            .collect();

        let mut order = Vec::with_capacity(remaining.len());
        while let Some(id) = ready.pop_first() {
            order.push(id.clone());
            if let Some(children) = dependents.get(&id) {
                for child in children {
                    let count = remaining.get_mut(child).expect("dependent exists");
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(child.clone());
                    }
                }
            }
        }

        if order.len() == remaining.len() {
            Some(order)
        } else {
            None
        }
    }

    /// Return a concrete cycle path, if the graph contains one.
    ///
    /// Example: `[assay.a, assay.b, assay.a]`.
    pub fn cycle(&self) -> Option<Vec<ModelId>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Grey,
            Black,
        }

        fn visit(
            graph: &TransformGraph,
            node: &ModelId,
            colors: &mut BTreeMap<ModelId, Color>,
            stack: &mut Vec<ModelId>,
        ) -> Option<Vec<ModelId>> {
            colors.insert(node.clone(), Color::Grey);
            stack.push(node.clone());

            for dependency in graph.dependencies(node) {
                let Dependency::Model(dependency_id) = dependency else {
                    continue;
                };
                match colors.get(&dependency_id).copied().unwrap_or(Color::White) {
                    Color::Grey => {
                        let start = stack
                            .iter()
                            .position(|candidate| candidate == &dependency_id)
                            .expect("grey node is on the stack");
                        let mut cycle = stack[start..].to_vec();
                        cycle.push(dependency_id);
                        return Some(cycle);
                    }
                    Color::White => {
                        if let Some(cycle) = visit(graph, &dependency_id, colors, stack) {
                            return Some(cycle);
                        }
                    }
                    Color::Black => {}
                }
            }

            stack.pop();
            colors.insert(node.clone(), Color::Black);
            None
        }

        let mut colors: BTreeMap<ModelId, Color> = BTreeMap::new();
        let mut stack: Vec<ModelId> = Vec::new();
        for id in self.model_ids() {
            if colors.get(&id).copied().unwrap_or(Color::White) == Color::White {
                if let Some(cycle) = visit(self, &id, &mut colors, &mut stack) {
                    return Some(cycle);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiled::CompiledModel;
    use crate::model::{FrontendKind, ModelOrigin};

    fn id(name: &str) -> ModelId {
        ModelId::parse(name).unwrap()
    }

    fn model(name: &str, dependencies: Vec<Dependency>) -> CompiledModel {
        let id = id(name);
        CompiledModel {
            namespace: id.namespace().clone(),
            path: id.path().to_vec(),
            origin: ModelOrigin {
                frontend: FrontendKind::InMemory,
                path: None,
            },
            sql: String::new(),
            pinned_id: None,
            dependencies,
            id,
        }
    }

    #[test]
    fn topological_order_puts_dependencies_first() {
        let models = vec![
            model(
                "reporting.monthly",
                vec![Dependency::Model(id("assay.results"))],
            ),
            model("assay.results", vec![Dependency::Model(id("assay.raw"))]),
            model("assay.raw", vec![]),
        ];
        let graph = TransformGraph::build(&models);
        let order: Vec<String> = graph
            .topological_order()
            .unwrap()
            .into_iter()
            .map(|id| id.logical_name())
            .collect();
        assert_eq!(
            order,
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }

    #[test]
    fn detects_cycle_path() {
        let models = vec![
            model("assay.a", vec![Dependency::Model(id("assay.c"))]),
            model("assay.b", vec![Dependency::Model(id("assay.a"))]),
            model("assay.c", vec![Dependency::Model(id("assay.b"))]),
        ];
        let graph = TransformGraph::build(&models);
        assert!(graph.topological_order().is_none());
        let cycle: Vec<String> = graph
            .cycle()
            .unwrap()
            .into_iter()
            .map(|id| id.logical_name())
            .collect();
        assert_eq!(cycle.first(), cycle.last());
        assert_eq!(cycle.len(), 4);
    }
}
