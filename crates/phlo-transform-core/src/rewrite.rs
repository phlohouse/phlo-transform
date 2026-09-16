//! Model SQL compilation: rewrite workspace relations to their physical
//! targets.
//!
//! Native SQL refers to models by logical name (`assay.raw`). Before
//! execution the compiler replaces every relation that resolves to a workspace
//! model with that model's physical target relation.
//!
//! External sources are left untouched unless a default catalog is
//! configured: a source named `schema.table` would otherwise resolve through
//! the session catalog, which under a candidate environment is the wrong
//! branch — or no catalog at all. When `default_catalog` is set, sources with
//! fewer than three name parts are rewritten to the same physical relation
//! [`relation_for_source`] maps them to, so compiled SQL names the relation
//! the engine actually loads seeds into and probes.
//!
//! The rewrite mirrors relation extraction exactly, including CTE scope, so a
//! CTE that shadows a workspace model name is never rewritten.

use std::collections::BTreeMap;
use std::ops::ControlFlow;

use phlo_transform_sql::RelationName;
use sqlparser::ast::Statement;
use sqlparser::ast::{
    Ident, ObjectName, ObjectNamePart, Query, TableAlias, TableFactor, VisitMut, VisitorMut,
};

use crate::identity::ModelId;
use crate::model::Relation;
use crate::resolve::{relation_for_source, RegistryEntry, Resolution, Resolver};

/// Rewrite a model's statements into compiled SQL.
///
/// When `current` is `None` the rewrite uses global resolution (used for
/// tests, which have no namespace context). `ephemerals` maps ephemeral model
/// ids to their already-expanded queries; references to them are inlined as
/// derived tables rather than rewritten to a physical target.
pub(crate) fn rewrite_statements(
    statements: &mut [Statement],
    current: Option<&RegistryEntry>,
    resolver: &Resolver,
    targets: &BTreeMap<ModelId, Relation>,
    ephemerals: &BTreeMap<ModelId, Query>,
    default_catalog: Option<&str>,
    default_schema: Option<&str>,
) -> String {
    for statement in statements.iter_mut() {
        let mut rewriter = RelationRewriter {
            cte_scopes: Vec::new(),
            current,
            resolver,
            targets,
            ephemerals,
            default_catalog,
            default_schema,
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
    ephemerals: &'a BTreeMap<ModelId, Query>,
    default_catalog: Option<&'a str>,
    default_schema: Option<&'a str>,
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
        let TableFactor::Table {
            name,
            args,
            alias: existing_alias,
            ..
        } = factor
        else {
            return ControlFlow::Continue(());
        };
        if args.is_some() {
            return ControlFlow::Continue(());
        }
        let parts: Vec<String> = name
            .0
            .iter()
            .filter_map(|part| part.as_ident().map(|ident| ident.value.clone()))
            .collect();
        let Some(relation) = RelationName::new(parts) else {
            return ControlFlow::Continue(());
        };
        if self.is_cte(&relation) {
            return ControlFlow::Continue(());
        }
        let resolution = match self.current {
            Some(current) => self.resolver.resolve(current, &relation),
            None => self.resolver.resolve_global(&relation),
        };
        let id = match resolution {
            Resolution::Model(id) => id,
            Resolution::External(source) => {
                // Only a configured catalog changes a source reference: a
                // one/two-part name would otherwise resolve through the
                // session catalog instead of the workspace's.
                if self.default_catalog.is_some() && source.parts().len() < 3 {
                    *name = object_name(&relation_for_source(
                        &source,
                        self.default_catalog,
                        self.default_schema,
                    ));
                }
                return ControlFlow::Continue(());
            }
            Resolution::Ambiguous(_) => return ControlFlow::Continue(()),
        };
        if let Some(subquery) = self.ephemerals.get(&id) {
            // Keep the user's alias (`from x as y`) — outer column refs bind
            // to it; otherwise name the derived table after the model.
            let alias = existing_alias.take().or(Some(TableAlias {
                explicit: true,
                name: Ident::new(relation.last()),
                columns: Vec::new(),
                at: None,
            }));
            *factor = TableFactor::Derived {
                lateral: false,
                subquery: Box::new(subquery.clone()),
                alias,
                sample: None,
            };
        } else if let Some(target) = self.targets.get(&id) {
            *name = object_name(target);
        }
        ControlFlow::Continue(())
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
