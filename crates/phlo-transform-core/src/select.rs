//! Small, orthogonal model selection.
//!
//! Phase 1 deliberately avoids a selector expression language. Criteria are
//! combined by intersection; `upstream`/`downstream` then expand the matched
//! set through the dependency graph.

use std::collections::BTreeSet;

use crate::compiled::Compilation;
use crate::identity::ModelId;

/// Selection criteria supplied by the CLI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelectionOptions {
    /// Exact model names/URIs or namespace globs such as `assay.*`.
    pub select: Vec<String>,
    /// Include every transitive dependency of the selected models.
    pub upstream: bool,
    /// Include every transitive dependent of the selected models.
    pub downstream: bool,
    pub tag: Option<String>,
    pub workflow: Option<String>,
}

impl SelectionOptions {
    pub fn is_empty(&self) -> bool {
        self.select.is_empty()
            && !self.upstream
            && !self.downstream
            && self.tag.is_none()
            && self.workflow.is_none()
    }
}

/// Select models, returned in sorted dependency order when acyclic.
pub fn select_models(compilation: &Compilation, options: &SelectionOptions) -> Vec<ModelId> {
    let mut selected: BTreeSet<ModelId> = compilation
        .models
        .iter()
        .filter(|model| matches_filters(compilation, &model.id, options))
        .map(|model| model.id.clone())
        .collect();

    if options.upstream {
        expand(&mut selected, compilation, true);
    }
    if options.downstream {
        expand(&mut selected, compilation, false);
    }

    // Return in topological order when possible so callers get dependencies
    // before dependents; fall back to sorted identity for cyclic graphs.
    match compilation.topological_order() {
        Some(order) => order
            .into_iter()
            .filter(|id| selected.contains(id))
            .collect(),
        None => selected.into_iter().collect(),
    }
}

fn matches_filters(compilation: &Compilation, id: &ModelId, options: &SelectionOptions) -> bool {
    if !options.select.is_empty()
        && !options
            .select
            .iter()
            .any(|pattern| matches_pattern(id, pattern))
    {
        return false;
    }
    if let Some(workflow) = &options.workflow {
        if id.namespace().as_str() != workflow {
            return false;
        }
    }
    if let Some(tag) = &options.tag {
        let has_tag = compilation
            .model(id)
            .map(|model| model.config.tags.iter().any(|model_tag| model_tag == tag))
            .unwrap_or(false);
        if !has_tag {
            return false;
        }
    }
    true
}

fn matches_pattern(id: &ModelId, pattern: &str) -> bool {
    let pattern = pattern.trim();
    if let Some(prefix) = pattern.strip_suffix(".*") {
        return id.logical_name() == prefix || id.logical_name().starts_with(&format!("{prefix}."));
    }
    pattern == id.logical_name() || pattern == id.uri()
}

fn expand(selected: &mut BTreeSet<ModelId>, compilation: &Compilation, upstream: bool) {
    let mut frontier: Vec<ModelId> = selected.iter().cloned().collect();
    while let Some(id) = frontier.pop() {
        let neighbours: Vec<ModelId> = if upstream {
            compilation
                .dependencies(&id)
                .into_iter()
                .filter_map(|dependency| match dependency {
                    crate::graph::Dependency::Model(id) => Some(id),
                    crate::graph::Dependency::Source(_) => None,
                })
                .collect()
        } else {
            compilation.dependents(&id)
        };
        for neighbour in neighbours {
            if selected.insert(neighbour.clone()) {
                frontier.push(neighbour);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::compile;
    use crate::model::{SemanticModel, SemanticProject};

    fn workspace() -> Compilation {
        let models = vec![
            SemanticModel::in_memory(
                ModelId::parse("assay.raw").unwrap(),
                "select * from external.raw_assay_results",
            ),
            SemanticModel::in_memory(
                ModelId::parse("assay.results").unwrap(),
                "select * from assay.raw",
            ),
            SemanticModel::in_memory(
                ModelId::parse("reporting.monthly").unwrap(),
                "select * from assay.results",
            ),
        ];
        let compilation = compile(&SemanticProject::in_memory(models));
        assert!(compilation.is_ok());
        compilation
    }

    fn names(ids: Vec<ModelId>) -> Vec<String> {
        ids.into_iter().map(|id| id.logical_name()).collect()
    }

    #[test]
    fn selects_all_by_default() {
        let compilation = workspace();
        assert_eq!(
            names(select_models(&compilation, &SelectionOptions::default())),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }

    #[test]
    fn selects_by_namespace_glob() {
        let compilation = workspace();
        let options = SelectionOptions {
            select: vec!["assay.*".to_string()],
            ..Default::default()
        };
        assert_eq!(
            names(select_models(&compilation, &options)),
            vec!["assay.raw", "assay.results"]
        );
    }

    #[test]
    fn expands_downstream() {
        let compilation = workspace();
        let options = SelectionOptions {
            select: vec!["assay.raw".to_string()],
            downstream: true,
            ..Default::default()
        };
        assert_eq!(
            names(select_models(&compilation, &options)),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }

    #[test]
    fn expands_upstream() {
        let compilation = workspace();
        let options = SelectionOptions {
            select: vec!["reporting.monthly".to_string()],
            upstream: true,
            ..Default::default()
        };
        assert_eq!(
            names(select_models(&compilation, &options)),
            vec!["assay.raw", "assay.results", "reporting.monthly"]
        );
    }
}
