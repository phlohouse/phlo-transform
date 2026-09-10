//! Model SQL compilation: rewrite workspace relations to their physical
//! targets.
//!
//! Native SQL refers to models by logical name (`assay.raw`). Before
//! execution the compiler replaces every relation that resolves to a workspace
//! model with that model's physical target relation. External sources are left
//! untouched.
//!
//! The rewrite mirrors relation extraction exactly, including CTE scope, so a
//! CTE that shadows a workspace model name is never rewritten.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use phlo_transform_sql::RelationName;
use sqlparser::ast::Statement;
use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, Query, TableFactor, VisitMut, VisitorMut};

use crate::identity::ModelId;
use crate::model::Relation;
use crate::resolve::{RegistryEntry, Resolution, Resolver};

/// Rewrite a model's statements into compiled SQL.
///
/// When `current` is `None` the rewrite uses global resolution (used for
/// tests, which have no namespace context).
pub(crate) fn rewrite_statements(
    statements: &mut [Statement],
    current: Option<&RegistryEntry>,
    resolver: &Resolver,
    targets: &BTreeMap<ModelId, Relation>,
) -> String {
    for statement in statements.iter_mut() {
        let mut rewriter = RelationRewriter {
            cte_scopes: Vec::new(),
            current,
            resolver,
            targets,
        };
        let _ = statement.visit(&mut rewriter);
    }

    statements
        .iter()
        .map(|statement| statement.to_string())
        .collect::<Vec<_>>()
        .join(";\n")
}

struct RelationRewriter<'a> {
    cte_scopes: Vec<Vec<String>>,
    current: Option<&'a RegistryEntry>,
    resolver: &'a Resolver,
    targets: &'a BTreeMap<ModelId, Relation>,
}

impl RelationRewriter<'_> {
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

impl VisitorMut for RelationRewriter<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, query: &mut Query) -> ControlFlow<Self::Break> {
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

    fn post_visit_query(&mut self, _query: &mut Query) -> ControlFlow<Self::Break> {
        self.cte_scopes.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, factor: &mut TableFactor) -> ControlFlow<Self::Break> {
        if let TableFactor::Table { name, args, .. } = factor {
            if args.is_none() {
                self.maybe_rewrite(name);
            }
        }
        ControlFlow::Continue(())
    }
}

impl RelationRewriter<'_> {
    fn maybe_rewrite(&mut self, name: &mut ObjectName) {
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
        let resolution = match self.current {
            Some(current) => self.resolver.resolve(current, &relation),
            None => self.resolver.resolve_global(&relation),
        };
        if let Resolution::Model(id) = resolution {
            if let Some(target) = self.targets.get(&id) {
                *name = object_name(target);
            }
        }
    }
}

fn object_name(relation: &Relation) -> ObjectName {
    let mut parts = Vec::with_capacity(3);
    if let Some(catalog) = &relation.catalog {
        parts.push(ObjectNamePart::Identifier(Ident::new(catalog)));
    }
    parts.push(ObjectNamePart::Identifier(Ident::new(&relation.schema)));
    parts.push(ObjectNamePart::Identifier(Ident::new(&relation.table)));
    ObjectName(parts)
}
