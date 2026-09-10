//! Relation extraction from parsed SQL.
//!
//! The extractor walks the real AST rather than searching text. It tracks CTE
//! scope so that a CTE name is never mistaken for a workspace relation, and
//! because it only looks at table references, column aliases and table aliases
//! cannot create false dependencies.

use std::ops::ControlFlow;

use sqlparser::ast::TableFactor;
use sqlparser::ast::{ObjectName, Query, Statement, Visit, Visitor};

/// A relation name as written in SQL, split into its dotted components.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelationName {
    parts: Vec<String>,
}

impl RelationName {
    /// Construct a relation name from dotted components.
    ///
    /// Returns `None` when there are no components or any component is empty.
    pub fn new(parts: Vec<String>) -> Option<Self> {
        if parts.is_empty() || parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        Some(Self { parts })
    }

    pub fn parts(&self) -> &[String] {
        &self.parts
    }

    /// Whether the name has no schema qualifier, e.g. `results`.
    pub fn is_simple(&self) -> bool {
        self.parts.len() == 1
    }

    /// The name as written, e.g. `assay.results`.
    pub fn as_dotted(&self) -> String {
        self.parts.join(".")
    }

    /// The final component, e.g. `results`.
    pub fn last(&self) -> &str {
        self.parts.last().expect("relation names are non-empty")
    }
}

impl std::fmt::Display for RelationName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.as_dotted())
    }
}

/// A relation reference found in a model's SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedRelation {
    pub name: RelationName,
}

impl ExtractedRelation {
    pub fn new(name: RelationName) -> Self {
        Self { name }
    }
}

/// Extract all table relations referenced by a sequence of statements.
///
/// Relations are returned in source order. CTE names are excluded.
pub fn extract_relations(statements: &[Statement]) -> Vec<ExtractedRelation> {
    let mut visitor = RelationVisitor::default();
    for statement in statements {
        // The visitor cannot fail; `Break` is the unit type.
        let _ = statement.visit(&mut visitor);
    }
    visitor
        .relations
        .into_iter()
        .map(ExtractedRelation::new)
        .collect()
}

#[derive(Default)]
struct RelationVisitor {
    /// Stack of CTE-name scopes, one entry per enclosing query.
    cte_scopes: Vec<Vec<String>>,
    relations: Vec<RelationName>,
}

impl RelationVisitor {
    fn is_cte(&self, name: &RelationName) -> bool {
        if !name.is_simple() {
            return false;
        }
        let needle = name.last();
        self.cte_scopes
            .iter()
            .rev()
            .any(|scope| scope.iter().any(|candidate| candidate == needle))
    }
}

impl Visitor for RelationVisitor {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        let names = query
            .with
            .as_ref()
            .map(|with| {
                with.cte_tables
                    .iter()
                    .map(|cte| cte.alias.name.value.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.cte_scopes.push(names);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _query: &Query) -> ControlFlow<Self::Break> {
        self.cte_scopes.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, args, .. } = factor {
            // Table-valued functions (`FROM unnest(...)`) are not relations.
            if args.is_none() {
                self.record(name);
            }
        }
        ControlFlow::Continue(())
    }
}

impl RelationVisitor {
    fn record(&mut self, name: &ObjectName) {
        let parts: Vec<String> = name
            .0
            .iter()
            .filter_map(|part| part.as_ident().map(|ident| ident.value.clone()))
            .collect();
        let Some(relation) = RelationName::new(parts) else {
            return;
        };
        if self.is_cte(&relation) {
            return;
        }
        self.relations.push(relation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::{parse_statements, Dialect};

    fn relations(sql: &str) -> Vec<String> {
        let statements = parse_statements(sql, Dialect::Generic).expect("valid sql");
        extract_relations(&statements)
            .into_iter()
            .map(|relation| relation.name.as_dotted())
            .collect()
    }

    #[test]
    fn extracts_from_and_join_relations() {
        assert_eq!(
            relations("select * from assay.results r join shared.samples s using (sample_id)"),
            vec!["assay.results", "shared.samples"]
        );
    }

    #[test]
    fn ignores_table_aliases() {
        assert_eq!(
            relations("select r.sample_id from assay.results as r"),
            vec!["assay.results"]
        );
    }

    #[test]
    fn ignores_cte_names() {
        assert_eq!(
            relations(
                "with raw as (select * from external.raw_results) \
                 select * from raw"
            ),
            vec!["external.raw_results"]
        );
    }

    #[test]
    fn nested_query_shadowing_workspace_model_is_ignored() {
        // `assay.raw` is a real relation, but the local CTE named `raw` must
        // not be treated as one.
        assert_eq!(
            relations("select * from (with raw as (select * from external.x) select * from raw) t"),
            vec!["external.x"]
        );
    }

    #[test]
    fn extracts_subqueries_in_expressions() {
        assert_eq!(
            relations(
                "select * from assay.results \
                 where sample_id in (select sample_id from lims.samples)"
            ),
            vec!["assay.results", "lims.samples"]
        );
    }

    #[test]
    fn extracts_schema_qualified_names() {
        assert_eq!(
            relations("select * from catalog.schema.table_name"),
            vec!["catalog.schema.table_name"]
        );
    }

    #[test]
    fn ignores_table_functions() {
        assert_eq!(
            relations("select * from unnest(array[1, 2, 3])"),
            Vec::<String>::new()
        );
    }
}
