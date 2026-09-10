//! Deterministic relation resolution.
//!
//! Resolution never guesses. If more than one model matches at a given
//! precedence level the result is [`Resolution::Ambiguous`], which the
//! compiler turns into an error listing every candidate.

use std::collections::HashMap;

use phlo_transform_sql::RelationName;

use crate::identity::{ModelId, Namespace, SourceId};
use crate::model::{RootRef, TransformRootId};

/// A model known to the resolver, before its own dependencies are resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryEntry {
    pub id: ModelId,
    pub namespace: Namespace,
    pub path: Vec<String>,
    pub root: Option<RootRef>,
}

/// The outcome of resolving a relation name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Resolves to a workspace model.
    Model(ModelId),
    /// Not produced by the workspace, so treated as an external source.
    External(SourceId),
    /// More than one model matched; this is a compilation error.
    Ambiguous(Vec<ModelId>),
}

/// Resolves relations against a fixed set of models.
///
/// Resolution runs once per relation reference in every model, so the
/// precedence levels are indexed rather than scanned — a linear scan here is
/// O(models²) on large workspaces.
#[derive(Clone, Debug, Default)]
pub struct Resolver {
    by_logical: HashMap<String, Vec<ModelId>>,
    by_namespace: HashMap<(Namespace, String), Vec<ModelId>>,
    by_root: HashMap<(TransformRootId, String), Vec<ModelId>>,
    /// Every dotted suffix of each logical name → the models carrying it.
    by_suffix: HashMap<String, Vec<ModelId>>,
}

impl Resolver {
    pub fn new(entries: Vec<RegistryEntry>) -> Self {
        let mut resolver = Self::default();
        for entry in entries {
            let logical = entry.id.logical_name();
            resolver
                .by_logical
                .entry(logical.clone())
                .or_default()
                .push(entry.id.clone());
            resolver
                .by_namespace
                .entry((entry.namespace.clone(), entry.id.local_name()))
                .or_default()
                .push(entry.id.clone());
            if let Some(root) = &entry.root {
                resolver
                    .by_root
                    .entry((root.id, root.relative_path.join(".")))
                    .or_default()
                    .push(entry.id.clone());
            }
            // `logical == name` and `logical.ends_with(".{name}")` both mean
            // `name` is a dotted suffix of the logical name.
            resolver
                .by_suffix
                .entry(logical.clone())
                .or_default()
                .push(entry.id.clone());
            for (position, _) in logical.match_indices('.') {
                resolver
                    .by_suffix
                    .entry(logical[position + 1..].to_string())
                    .or_default()
                    .push(entry.id.clone());
            }
        }
        resolver
    }

    /// Resolve `name` in the context of `current`.
    ///
    /// Precedence:
    /// 1. exact fully-qualified workspace model;
    /// 2. current namespace;
    /// 3. current transform root;
    /// 4. globally unique workspace model (suffix match);
    /// 5. external source.
    pub fn resolve(&self, current: &RegistryEntry, name: &RelationName) -> Resolution {
        let full = name.as_dotted();

        if let Some(resolution) = self.unique(self.exact(&full)) {
            return resolution;
        }

        if let Some(resolution) = self.unique(self.in_namespace(current, &full)) {
            return resolution;
        }

        if let Some(resolution) = self.unique(self.in_root(current, &full)) {
            return resolution;
        }

        self.resolve_global(name)
    }

    /// Resolve `name` without model-local context (used by tests).
    pub fn resolve_global(&self, name: &RelationName) -> Resolution {
        let full = name.as_dotted();
        if let Some(resolution) = self.unique(self.exact(&full)) {
            return resolution;
        }
        if let Some(resolution) = self.unique(self.suffix(&full)) {
            return resolution;
        }
        // Any relation not produced by the workspace is an external source
        // candidate. Live catalogue introspection is out of scope for Phase 1.
        let parts = name.parts().to_vec();
        Resolution::External(SourceId::new(parts).expect("relation names are non-empty"))
    }

    fn exact(&self, full: &str) -> Vec<ModelId> {
        self.by_logical.get(full).cloned().unwrap_or_default()
    }

    fn in_namespace(&self, current: &RegistryEntry, full: &str) -> Vec<ModelId> {
        self.by_namespace
            .get(&(current.namespace.clone(), full.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn in_root(&self, current: &RegistryEntry, full: &str) -> Vec<ModelId> {
        let Some(current_root) = &current.root else {
            return Vec::new();
        };
        self.by_root
            .get(&(current_root.id, full.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    fn suffix(&self, full: &str) -> Vec<ModelId> {
        self.by_suffix.get(full).cloned().unwrap_or_default()
    }

    /// Turn a candidate set into a resolution. Zero candidates means "no
    /// match at this level" (continue); one means success; more than one is
    /// an ambiguity that must fail.
    fn unique(&self, mut candidates: Vec<ModelId>) -> Option<Resolution> {
        candidates.sort();
        candidates.dedup();
        match candidates.len() {
            0 => None,
            1 => Some(Resolution::Model(candidates.pop().expect("one candidate"))),
            _ => Some(Resolution::Ambiguous(candidates)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> RegistryEntry {
        let id = ModelId::parse(name).unwrap();
        RegistryEntry {
            namespace: id.namespace().clone(),
            path: id.path().to_vec(),
            id,
            root: None,
        }
    }

    fn resolver(names: &[&str]) -> Resolver {
        Resolver::new(names.iter().map(|name| entry(name)).collect())
    }

    fn relation(name: &str) -> RelationName {
        RelationName::new(name.split('.').map(str::to_string).collect()).unwrap()
    }

    #[test]
    fn resolves_exact_qualified_name() {
        let resolver = resolver(&["assay.raw", "reporting.monthly"]);
        let current = entry("reporting.monthly");
        assert_eq!(
            resolver.resolve(&current, &relation("assay.raw")),
            Resolution::Model(ModelId::parse("assay.raw").unwrap())
        );
    }

    #[test]
    fn resolves_in_current_namespace() {
        let resolver = resolver(&["assay.raw", "assay.results"]);
        let current = entry("assay.results");
        assert_eq!(
            resolver.resolve(&current, &relation("raw")),
            Resolution::Model(ModelId::parse("assay.raw").unwrap())
        );
    }

    #[test]
    fn resolves_globally_unique_suffix() {
        let resolver = resolver(&["assay.raw", "shared.samples"]);
        let current = entry("reporting.monthly");
        assert_eq!(
            resolver.resolve(&current, &relation("samples")),
            Resolution::Model(ModelId::parse("shared.samples").unwrap())
        );
    }

    #[test]
    fn ambiguous_suffix_is_an_error() {
        let resolver = resolver(&["assay.results", "manufacturing.results"]);
        let current = entry("reporting.monthly");
        match resolver.resolve(&current, &relation("results")) {
            Resolution::Ambiguous(mut candidates) => {
                candidates.sort();
                assert_eq!(candidates.len(), 2);
            }
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn unknown_name_becomes_external() {
        let resolver = resolver(&["assay.raw"]);
        let current = entry("assay.raw");
        assert_eq!(
            resolver.resolve(&current, &relation("lims.samples")),
            Resolution::External(SourceId::new(vec!["lims".into(), "samples".into()]).unwrap())
        );
    }
}
