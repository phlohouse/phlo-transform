//! Column resolution, type inference and lineage.
//!
//! This is a deliberately pragmatic resolver for the common Trino
//! transformation SQL. Anything it cannot reason about is marked
//! [`DataType::Unknown`] and recorded as a limitation rather than guessed.
//!
//! The analyzer works on the parsed AST plus the already-computed output
//! schemas of upstream models and a [`SchemaProvider`] for external sources.
//! It does not mutate the parser AST.
//!
//! Every [`ColumnInput`] it produces is AST-proven and therefore
//! [`LineageConfidence::Exact`]; when part of an expression cannot be
//! analysed the result is flagged incomplete so the column's lineage
//! confidence degrades to [`LineageConfidence::Unknown`] instead of
//! silently presenting partial inputs as exact.

use std::collections::{BTreeMap, BTreeSet};

use sqlparser::ast::Statement;
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, GroupByExpr,
    JoinConstraint, JoinOperator, OrderByKind, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, TableFactor, TableWithJoins, UnaryOperator,
    WindowType,
};

use crate::diagnostics::{codes, Diagnostic};
use crate::identity::{ModelId, SourceId};
use crate::resolve::{RegistryEntry, Resolution, Resolver};
use crate::schema::SchemaProvider;
use crate::semantic::{
    ColumnInput, ColumnRef, DataType, Directness, LineageConfidence, ModelSchema, Nullability,
    OutputColumn, Transformation,
};

/// The result of analysing one model.
#[derive(Clone, Debug, Default)]
pub struct Analysis {
    pub schema: ModelSchema,
    pub diagnostics: Vec<Diagnostic>,
    pub limitations: Vec<String>,
}

/// Analyzes models into typed schemas with lineage.
pub struct Analyzer<'a> {
    resolver: &'a Resolver,
    model_schemas: &'a BTreeMap<ModelId, ModelSchema>,
    provider: &'a dyn SchemaProvider,
}

impl<'a> Analyzer<'a> {
    pub fn new(
        resolver: &'a Resolver,
        model_schemas: &'a BTreeMap<ModelId, ModelSchema>,
        provider: &'a dyn SchemaProvider,
    ) -> Self {
        Self {
            resolver,
            model_schemas,
            provider,
        }
    }

    /// Analyze a model's statements.
    pub fn analyze(
        &self,
        current: &RegistryEntry,
        statements: &[Statement],
        model: &ModelId,
    ) -> Analysis {
        let mut state = AnalyzeState::default();
        let Some(Statement::Query(query)) = statements.first() else {
            if !statements.is_empty() {
                state
                    .limitations
                    .insert("model is not a SELECT query".to_string());
            }
            return Analysis {
                schema: ModelSchema::default(),
                diagnostics: state.diagnostics,
                limitations: state.limitations.into_iter().collect(),
            };
        };

        let mut ctes = CteEnv::default();
        let columns = self.analyze_query(query, current, &mut ctes, model, &mut state);
        let known = !state.unknown_relations;
        let schema = ModelSchema {
            columns: columns
                .into_iter()
                .map(|column| OutputColumn {
                    name: column.name,
                    data_type: column.data_type,
                    nullability: column.nullability,
                    inputs: column.inputs,
                    confidence: if column.complete {
                        LineageConfidence::Exact
                    } else {
                        LineageConfidence::Unknown
                    },
                })
                .collect(),
            known,
        };

        Analysis {
            schema,
            diagnostics: state.diagnostics,
            limitations: state.limitations.into_iter().collect(),
        }
    }

    fn analyze_query(
        &self,
        query: &Query,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> Vec<Column> {
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                let columns = self.analyze_query(&cte.query, current, ctes, model, state);
                ctes.insert(cte.alias.name.value.to_lowercase(), columns);
            }
        }
        let (mut columns, scope) = self.analyze_set_expr(&query.body, current, ctes, model, state);

        // `ORDER BY` keys do not produce values but do decide which rows of
        // the output are emitted in which order — an indirect input to every
        // output column. Resolution is lenient here: `ORDER BY` may reference
        // output aliases, which live outside the FROM scope.
        if let (Some(order_by), Some(scope)) = (&query.order_by, &scope) {
            if let OrderByKind::Expressions(order_exprs) = &order_by.kind {
                let mut sort_inputs = Vec::new();
                for order_expr in order_exprs {
                    sort_inputs.extend(self.lenient_inputs(
                        &order_expr.expr,
                        scope,
                        &columns,
                        Transformation::Sort,
                    ));
                }
                if !sort_inputs.is_empty() {
                    for column in &mut columns {
                        column.inputs.extend(sort_inputs.iter().cloned());
                    }
                }
            }
        }
        for column in &mut columns {
            dedup(&mut column.inputs);
        }
        columns
    }

    /// The body's output columns plus its FROM scope when it is a plain
    /// `SELECT` (needed to resolve `ORDER BY` against the source scope).
    fn analyze_set_expr(
        &self,
        body: &SetExpr,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> (Vec<Column>, Option<Scope>) {
        match body {
            SetExpr::Select(select) => self.analyze_select(select, current, ctes, model, state),
            SetExpr::Query(query) => (self.analyze_query(query, current, ctes, model, state), None),
            SetExpr::SetOperation { left, right, .. } => {
                let (left_columns, _) = self.analyze_set_expr(left, current, ctes, model, state);
                let (right_columns, _) = self.analyze_set_expr(right, current, ctes, model, state);
                if left_columns.len() != right_columns.len() {
                    state.diagnostics.push(
                        Diagnostic::error(
                            codes::TYPE_INCOMPATIBLE_UNION,
                            format!(
                                "set operation branches have {} and {} columns",
                                left_columns.len(),
                                right_columns.len()
                            ),
                        )
                        .with_path(model.logical_name()),
                    );
                }
                let mut combined = Vec::with_capacity(left_columns.len());
                for (index, left_column) in left_columns.into_iter().enumerate() {
                    let right = right_columns.get(index);
                    let data_type = match right {
                        Some(right) => DataType::widen(&left_column.data_type, &right.data_type),
                        None => left_column.data_type.clone(),
                    };
                    let nullability = right
                        .map(|right| left_column.nullability.merge(right.nullability))
                        .unwrap_or(left_column.nullability);
                    let complete =
                        left_column.complete && right.map(|right| right.complete).unwrap_or(true);
                    let mut inputs = left_column.inputs;
                    if let Some(right) = right {
                        inputs.extend(right.inputs.iter().cloned());
                    }
                    inputs.sort();
                    inputs.dedup();
                    combined.push(Column {
                        name: left_column.name,
                        data_type,
                        nullability,
                        inputs,
                        complete,
                    });
                }
                (combined, None)
            }
            SetExpr::Values(values) => {
                // Values rows: infer types from the first row.
                let columns = values
                    .rows
                    .first()
                    .map(|row| {
                        row.iter()
                            .enumerate()
                            .map(|(index, expr)| {
                                let inferred = self.infer(expr, &Scope::default(), model, state);
                                Column {
                                    name: format!("column{index}"),
                                    data_type: inferred.data_type,
                                    nullability: inferred.nullability,
                                    inputs: inferred.inputs,
                                    complete: inferred.complete,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                (columns, None)
            }
            _ => {
                state
                    .limitations
                    .insert("unsupported query body".to_string());
                (Vec::new(), None)
            }
        }
    }

    fn analyze_select(
        &self,
        select: &Select,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> (Vec<Column>, Option<Scope>) {
        let mut built = self.build_scope(&select.from, current, ctes, model, state);
        let scope = std::mem::take(&mut built.scope);

        // Clause columns become indirect inputs of every output column: they
        // decide which rows and groups exist without flowing into values.
        let mut indirect = std::mem::take(&mut built.join_inputs);
        let mut clauses_complete = built.complete;
        for (expr, kind) in [
            (&select.selection, Transformation::Filter),
            (&select.having, Transformation::Filter),
            (&select.qualify, Transformation::Filter),
        ]
        .into_iter()
        .filter_map(|(clause, kind)| clause.as_ref().map(|expr| (expr, kind)))
        {
            let inferred = self.infer(expr, &scope, model, state);
            clauses_complete &= inferred.complete;
            indirect.extend(mark_indirect(inferred.inputs, kind, expr.to_string()));
        }
        let mut grouping = Vec::new();
        if let GroupByExpr::Expressions(expressions, _) = &select.group_by {
            for expression in expressions {
                let inferred = self.infer(expression, &scope, model, state);
                clauses_complete &= inferred.complete;
                grouping.extend(mark_indirect(
                    inferred.inputs,
                    Transformation::GroupBy,
                    expression.to_string(),
                ));
            }
        }

        let mut outputs = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let inferred = self.infer(expr, &scope, model, state);
                    let mut inputs = inferred.inputs;
                    inputs.extend(indirect.iter().cloned());
                    if inferred.aggregates {
                        inputs.extend(grouping.iter().cloned());
                    }
                    outputs.push(Column {
                        name: expression_name(expr)
                            .unwrap_or_else(|| format!("column{}", outputs.len())),
                        data_type: inferred.data_type,
                        nullability: inferred.nullability,
                        inputs,
                        complete: inferred.complete && clauses_complete,
                    });
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let inferred = self.infer(expr, &scope, model, state);
                    let mut inputs = inferred.inputs;
                    inputs.extend(indirect.iter().cloned());
                    if inferred.aggregates {
                        inputs.extend(grouping.iter().cloned());
                    }
                    outputs.push(Column {
                        name: alias.value.clone(),
                        data_type: inferred.data_type,
                        nullability: inferred.nullability,
                        inputs,
                        complete: inferred.complete && clauses_complete,
                    });
                }
                SelectItem::Wildcard(_) => {
                    for column in scope.all_columns() {
                        let mut column = column;
                        column.inputs.extend(indirect.iter().cloned());
                        if column.complete {
                            column.complete = clauses_complete;
                        }
                        outputs.push(column);
                    }
                }
                SelectItem::QualifiedWildcard(kind, _) => {
                    let qualifier = match kind {
                        SelectItemQualifiedWildcardKind::ObjectName(name) => name
                            .0
                            .iter()
                            .filter_map(|part| part.as_ident().map(|ident| ident.value.clone()))
                            .collect::<Vec<_>>()
                            .join("."),
                        SelectItemQualifiedWildcardKind::Expr(expr) => expr.to_string(),
                    };
                    let matched = scope.columns_for_qualifier(&qualifier);
                    if matched.is_empty() {
                        if state.unknown_relations {
                            state.limitations.insert(format!(
                                "`{qualifier}.*` could not be expanded because an input schema is unknown"
                            ));
                        } else {
                            state.diagnostics.push(
                                Diagnostic::error(
                                    codes::TYPE_UNKNOWN_COLUMN,
                                    format!("unknown relation qualifier `{qualifier}` in wildcard"),
                                )
                                .with_path(model.logical_name()),
                            );
                        }
                    }
                    for column in matched {
                        let mut column = column;
                        column.inputs.extend(indirect.iter().cloned());
                        if column.complete {
                            column.complete = clauses_complete;
                        }
                        outputs.push(column);
                    }
                }
                SelectItem::ExprWithAliases { expr, aliases } => {
                    let inferred = self.infer(expr, &scope, model, state);
                    for alias in aliases {
                        let mut inputs = inferred.inputs.clone();
                        inputs.extend(indirect.iter().cloned());
                        if inferred.aggregates {
                            inputs.extend(grouping.iter().cloned());
                        }
                        outputs.push(Column {
                            name: alias.value.clone(),
                            data_type: inferred.data_type.clone(),
                            nullability: inferred.nullability,
                            inputs,
                            complete: inferred.complete && clauses_complete,
                        });
                    }
                }
            }
        }

        report_duplicate_outputs(&outputs, model, state);
        (outputs, Some(scope))
    }

    /// Indirect inputs contributed by join constraints, plus whether every
    /// constraint was fully analysed.
    fn build_scope(
        &self,
        from: &[TableWithJoins],
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> BuiltScope {
        let mut built = BuiltScope::default();
        for table in from {
            self.add_table_with_joins(table, &mut built, current, ctes, model, state);
        }
        built
    }

    fn add_table_with_joins(
        &self,
        table: &TableWithJoins,
        built: &mut BuiltScope,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) {
        if let Some(relation) =
            self.table_factor(&table.relation, built, current, ctes, model, state)
        {
            built.scope.relations.push(relation);
        }
        for join in &table.joins {
            let Some(right) = self.table_factor(&join.relation, built, current, ctes, model, state)
            else {
                continue;
            };
            let using = match &join.join_operator {
                JoinOperator::Join(constraint)
                | JoinOperator::Inner(constraint)
                | JoinOperator::Left(constraint)
                | JoinOperator::LeftOuter(constraint)
                | JoinOperator::Right(constraint)
                | JoinOperator::RightOuter(constraint)
                | JoinOperator::FullOuter(constraint)
                | JoinOperator::CrossJoin(constraint) => constraint.clone(),
                _ => JoinConstraint::None,
            };
            match using {
                JoinConstraint::On(expr) => {
                    // Infer against the right relation without polluting the
                    // outer scope's column list until the join is complete.
                    let mut with_right = built.scope.clone();
                    with_right.relations.push(right.clone());
                    let inferred = self.infer(&expr, &with_right, model, state);
                    built.complete &= inferred.complete;
                    built.join_inputs.extend(mark_indirect(
                        inferred.inputs,
                        Transformation::Join,
                        expr.to_string(),
                    ));
                    built.scope.relations.push(right);
                }
                JoinConstraint::Using(names) => {
                    let using: BTreeSet<String> = names
                        .iter()
                        .filter_map(|name| name.0.last().and_then(|part| part.as_ident()))
                        .map(|ident| ident.value.to_lowercase())
                        .collect();
                    // A `USING` column is the merge of both sides' columns:
                    // resolve it in the combined scope before the right copy
                    // is dropped so both contributing columns are recorded.
                    let mut combined = built.scope.clone();
                    combined.relations.push(right.clone());
                    let expression = format!(
                        "using ({})",
                        names
                            .iter()
                            .map(|name| name.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    for name in &using {
                        for column in combined.resolve(None, name) {
                            built.join_inputs.extend(mark_indirect(
                                column.inputs,
                                Transformation::Join,
                                expression.clone(),
                            ));
                        }
                    }
                    let mut filtered = right;
                    // `USING` columns are exposed once. Drop the right copy
                    // only when the left side already provides it.
                    filtered.columns.retain(|column| {
                        let name = column.name.to_lowercase();
                        !using.contains(&name) || built.scope.resolve(None, &name).is_empty()
                    });
                    built.scope.relations.push(filtered);
                    // Verify the using columns exist somewhere.
                    for name in &using {
                        if built.scope.resolve(None, name).is_empty() && !state.unknown_relations {
                            state.diagnostics.push(
                                Diagnostic::error(
                                    codes::TYPE_UNKNOWN_COLUMN,
                                    format!("USING column `{name}` was not found"),
                                )
                                .with_path(model.logical_name()),
                            );
                        }
                    }
                }
                JoinConstraint::Natural => {
                    // A natural join keys on the columns both sides share.
                    let mut combined = built.scope.clone();
                    combined.relations.push(right.clone());
                    for column in &right.columns {
                        if built
                            .scope
                            .resolve(None, &column.name.to_lowercase())
                            .is_empty()
                        {
                            continue;
                        }
                        for matched in combined.resolve(None, &column.name.to_lowercase()) {
                            built.join_inputs.extend(mark_indirect(
                                matched.inputs,
                                Transformation::Join,
                                "natural join".to_string(),
                            ));
                        }
                    }
                    built.scope.relations.push(right);
                }
                JoinConstraint::None => {
                    built.scope.relations.push(right);
                }
            }
        }
    }

    fn table_factor(
        &self,
        factor: &TableFactor,
        built: &mut BuiltScope,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> Option<ScopedRelation> {
        match factor {
            TableFactor::Table {
                name, alias, args, ..
            } => {
                if args.is_some() {
                    state
                        .limitations
                        .insert("table functions are not analysed".to_string());
                    // `FROM generate_series(...) AS t(c)` declares its output
                    // column names in the alias — resolve against those even
                    // though the function's types stay unknown.
                    return alias
                        .as_ref()
                        .filter(|alias| !alias.columns.is_empty())
                        .map(|alias| ScopedRelation {
                            qualifier: Some(alias.name.value.clone()),
                            columns: alias
                                .columns
                                .iter()
                                .map(|column| Column {
                                    name: column.name.value.clone(),
                                    data_type: DataType::Unknown,
                                    nullability: Nullability::Unknown,
                                    inputs: Vec::new(),
                                    // The function's inputs are not analysed,
                                    // so the column's lineage is incomplete.
                                    complete: false,
                                })
                                .collect(),
                        });
                }
                let parts: Vec<String> = name
                    .0
                    .iter()
                    .filter_map(|part| part.as_ident().map(|ident| ident.value.clone()))
                    .collect();
                let qualifier = alias
                    .as_ref()
                    .map(|alias| alias.name.value.clone())
                    .or_else(|| parts.last().cloned());

                // A CTE shadows workspace relations.
                if parts.len() == 1 {
                    if let Some(columns) = ctes.get(&parts[0].to_lowercase()) {
                        return Some(ScopedRelation {
                            qualifier,
                            columns: columns.clone(),
                        });
                    }
                }

                let relation_name = phlo_transform_sql::RelationName::new(parts.clone())?;
                let columns = match self.resolver.resolve(current, &relation_name) {
                    Resolution::Model(id) => self.model_columns(&id, state),
                    Resolution::External(source) => self.source_columns(&source, state),
                    Resolution::Ambiguous(_) => Vec::new(),
                };
                Some(ScopedRelation { qualifier, columns })
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let columns = self.analyze_query(subquery, current, ctes, model, state);
                Some(ScopedRelation {
                    qualifier: alias.as_ref().map(|alias| alias.name.value.clone()),
                    columns,
                })
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                let mut nested = BuiltScope::default();
                self.add_table_with_joins(
                    table_with_joins,
                    &mut nested,
                    current,
                    ctes,
                    model,
                    state,
                );
                let columns = nested.scope.all_columns();
                // A nested join's constraints constrain the relation's rows.
                built.join_inputs.extend(nested.join_inputs);
                built.complete &= nested.complete;
                Some(ScopedRelation {
                    qualifier: None,
                    columns,
                })
            }
            _ => {
                state
                    .limitations
                    .insert("unsupported FROM item".to_string());
                None
            }
        }
    }

    fn model_columns(&self, id: &ModelId, state: &mut AnalyzeState) -> Vec<Column> {
        match self.model_schemas.get(id) {
            Some(schema) => {
                if !schema.known {
                    state.unknown_relations = true;
                }
                schema
                    .columns
                    .iter()
                    .map(|column| Column {
                        name: column.name.clone(),
                        data_type: column.data_type.clone(),
                        nullability: column.nullability,
                        inputs: vec![ColumnInput::identity(ColumnRef::model(
                            id.clone(),
                            &column.name,
                        ))],
                        complete: true,
                    })
                    .collect()
            }
            None => {
                state.unknown_relations = true;
                state.limitations.insert(format!(
                    "schema for model `{}` is not available yet",
                    id.logical_name()
                ));
                Vec::new()
            }
        }
    }

    fn source_columns(&self, source: &SourceId, state: &mut AnalyzeState) -> Vec<Column> {
        match self.provider.source_schema(source) {
            Some(schema) => schema
                .columns
                .iter()
                .map(|column| Column {
                    name: column.name.clone(),
                    data_type: column.data_type.clone(),
                    nullability: column.nullability,
                    inputs: vec![ColumnInput::identity(ColumnRef::source(
                        source.clone(),
                        &column.name,
                    ))],
                    complete: true,
                })
                .collect(),
            None => {
                state.unknown_relations = true;
                state.limitations.insert(format!(
                    "schema for source `{}` is unknown",
                    source.logical_name()
                ));
                Vec::new()
            }
        }
    }

    /// Collect inputs for expressions that must not raise diagnostics —
    /// `ORDER BY` may reference output aliases and positional terms, so
    /// unresolved identifiers are skipped rather than reported.
    fn lenient_inputs(
        &self,
        expr: &Expr,
        scope: &Scope,
        outputs: &[Column],
        transformation: Transformation,
    ) -> Vec<ColumnInput> {
        let mut inputs = Vec::new();
        for (qualifier, name) in expression_identifiers(expr) {
            // An unqualified `ORDER BY` name resolves to the output alias
            // first under SQL semantics; ordering by an output adds nothing
            // to its own lineage.
            if qualifier.is_none()
                && outputs
                    .iter()
                    .any(|column| column.name.eq_ignore_ascii_case(&name))
            {
                continue;
            }
            for column in scope.resolve(qualifier.as_deref(), &name) {
                inputs.extend(mark_indirect(
                    column.inputs,
                    transformation,
                    expr.to_string(),
                ));
            }
        }
        inputs
    }

    /// Infer the type, nullability and lineage inputs of an expression.
    ///
    /// Composite expressions retag their direct inputs with the
    /// transformation they apply; indirect inputs (propagated from filtered
    /// subqueries) keep their original classification.
    fn infer(
        &self,
        expr: &Expr,
        scope: &Scope,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> Inferred {
        match expr {
            Expr::Identifier(ident) => self.resolve_column(scope, None, &ident.value, model, state),
            Expr::CompoundIdentifier(idents) => {
                if idents.len() >= 2 {
                    let name = &idents[idents.len() - 1].value;
                    let qualifier = &idents[idents.len() - 2].value;
                    self.resolve_column(scope, Some(qualifier), name, model, state)
                } else {
                    Inferred::unknown()
                }
            }
            Expr::Value(value) => infer_value(&value.value),
            Expr::TypedString(typed) => Inferred {
                data_type: DataType::parse_trino(&typed.data_type.to_string()),
                nullability: Nullability::NotNull,
                inputs: Vec::new(),
                complete: true,
                aggregates: false,
            },
            Expr::Nested(inner) => self.infer(inner, scope, model, state),
            Expr::Cast {
                kind,
                expr: inner,
                data_type,
                ..
            } => {
                let mut inner = self.infer(inner, scope, model, state);
                retag(&mut inner.inputs, Transformation::Transformation, expr);
                let nullability = if matches!(kind, sqlparser::ast::CastKind::TryCast) {
                    Nullability::Nullable
                } else {
                    inner.nullability
                };
                Inferred {
                    data_type: DataType::parse_trino(&data_type.to_string()),
                    nullability,
                    inputs: inner.inputs,
                    complete: inner.complete,
                    aggregates: inner.aggregates,
                }
            }
            Expr::UnaryOp { op, expr: inner } => {
                let mut inner = self.infer(inner, scope, model, state);
                retag(&mut inner.inputs, Transformation::Transformation, expr);
                match op {
                    UnaryOperator::Not => Inferred {
                        data_type: DataType::Boolean,
                        nullability: inner.nullability,
                        inputs: inner.inputs,
                        complete: inner.complete,
                        aggregates: inner.aggregates,
                    },
                    UnaryOperator::Minus | UnaryOperator::Plus => inner,
                    _ => Inferred::unknown_from(inner.inputs, inner.complete),
                }
            }
            Expr::BinaryOp { left, op, right } => {
                let left = self.infer(left, scope, model, state);
                let right = self.infer(right, scope, model, state);
                infer_binary(op, left, right, expr)
            }
            Expr::IsNull(inner)
            | Expr::IsNotNull(inner)
            | Expr::IsTrue(inner)
            | Expr::IsNotTrue(inner)
            | Expr::IsFalse(inner)
            | Expr::IsNotFalse(inner)
            | Expr::IsUnknown(inner)
            | Expr::IsNotUnknown(inner) => {
                let mut inner = self.infer(inner, scope, model, state);
                retag(&mut inner.inputs, Transformation::Transformation, expr);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::NotNull,
                    inputs: inner.inputs,
                    complete: inner.complete,
                    aggregates: inner.aggregates,
                }
            }
            Expr::InList {
                expr: inner, list, ..
            } => {
                let mut inferred = self.infer(inner, scope, model, state);
                let mut inputs = std::mem::take(&mut inferred.inputs);
                let mut complete = inferred.complete;
                let mut aggregates = inferred.aggregates;
                for item in list {
                    let item = self.infer(item, scope, model, state);
                    complete &= item.complete;
                    aggregates |= item.aggregates;
                    inputs.extend(item.inputs);
                }
                retag(&mut inputs, Transformation::Transformation, expr);
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::Unknown,
                    inputs,
                    complete,
                    aggregates,
                }
            }
            Expr::Between {
                expr: inner,
                low,
                high,
                ..
            } => {
                let mut inputs = self.infer(inner, scope, model, state);
                let low = self.infer(low, scope, model, state);
                let high = self.infer(high, scope, model, state);
                let mut merged = std::mem::take(&mut inputs.inputs);
                merged.extend(low.inputs);
                merged.extend(high.inputs);
                let complete = inputs.complete && low.complete && high.complete;
                let aggregates = inputs.aggregates || low.aggregates || high.aggregates;
                retag(&mut merged, Transformation::Transformation, expr);
                dedup(&mut merged);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::Unknown,
                    inputs: merged,
                    complete,
                    aggregates,
                }
            }
            Expr::Like {
                expr: inner,
                pattern,
                ..
            }
            | Expr::ILike {
                expr: inner,
                pattern,
                ..
            }
            | Expr::SimilarTo {
                expr: inner,
                pattern,
                ..
            } => {
                let inner_inferred = self.infer(inner, scope, model, state);
                let pattern_inferred = self.infer(pattern, scope, model, state);
                let mut inputs = inner_inferred.inputs;
                inputs.extend(pattern_inferred.inputs);
                retag(&mut inputs, Transformation::Transformation, expr);
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::Unknown,
                    inputs,
                    complete: inner_inferred.complete && pattern_inferred.complete,
                    aggregates: inner_inferred.aggregates || pattern_inferred.aggregates,
                }
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                let mut inputs = Vec::new();
                let mut complete = true;
                let mut aggregates = false;
                if let Some(operand) = operand {
                    let inferred = self.infer(operand, scope, model, state);
                    complete &= inferred.complete;
                    aggregates |= inferred.aggregates;
                    inputs.extend(inferred.inputs);
                }
                let mut data_type: Option<DataType> = None;
                let mut nullability = if else_result.is_some() {
                    Nullability::NotNull
                } else {
                    Nullability::Nullable
                };
                for case_when in conditions {
                    let condition = self.infer(&case_when.condition, scope, model, state);
                    complete &= condition.complete;
                    aggregates |= condition.aggregates;
                    inputs.extend(condition.inputs);
                    let inferred = self.infer(&case_when.result, scope, model, state);
                    complete &= inferred.complete;
                    aggregates |= inferred.aggregates;
                    merge_type(&mut data_type, &inferred.data_type);
                    nullability = nullability.merge(inferred.nullability);
                    inputs.extend(inferred.inputs);
                }
                if let Some(else_result) = else_result {
                    let inferred = self.infer(else_result, scope, model, state);
                    complete &= inferred.complete;
                    aggregates |= inferred.aggregates;
                    merge_type(&mut data_type, &inferred.data_type);
                    nullability = nullability.merge(inferred.nullability);
                    inputs.extend(inferred.inputs);
                }
                retag(&mut inputs, Transformation::Conditional, expr);
                dedup(&mut inputs);
                Inferred {
                    data_type: data_type.unwrap_or(DataType::Unknown),
                    nullability,
                    inputs,
                    complete,
                    aggregates,
                }
            }
            Expr::Function(function) => {
                let name = function
                    .name
                    .0
                    .last()
                    .and_then(|part| part.as_ident())
                    .map(|ident| ident.value.to_lowercase())
                    .unwrap_or_default();
                let mut inputs = Vec::new();
                let mut arg_types = Vec::new();
                let mut complete = true;
                let mut aggregates = false;
                match &function.args {
                    FunctionArguments::List(list) => {
                        for arg in &list.args {
                            if let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg_expr)) = arg {
                                let inferred = self.infer(arg_expr, scope, model, state);
                                complete &= inferred.complete;
                                aggregates |= inferred.aggregates;
                                inputs.extend(inferred.inputs);
                                arg_types.push(inferred.data_type);
                            }
                        }
                    }
                    FunctionArguments::Subquery(_) => {
                        state
                            .limitations
                            .insert("function subquery arguments are not analysed".to_string());
                        complete = false;
                    }
                    _ => {}
                }
                // Window partition/order keys influence the window's value
                // without flowing into it — indirect inputs.
                if let Some(WindowType::WindowSpec(spec)) = &function.over {
                    let mut window_inputs = Vec::new();
                    for partition in &spec.partition_by {
                        let inferred = self.infer(partition, scope, model, state);
                        complete &= inferred.complete;
                        window_inputs.extend(mark_indirect(
                            inferred.inputs,
                            Transformation::GroupBy,
                            expr.to_string(),
                        ));
                    }
                    for order in &spec.order_by {
                        let inferred = self.infer(&order.expr, scope, model, state);
                        complete &= inferred.complete;
                        window_inputs.extend(mark_indirect(
                            inferred.inputs,
                            Transformation::Sort,
                            expr.to_string(),
                        ));
                    }
                    inputs.extend(window_inputs);
                }
                let windowed = function.over.is_some();
                let aggregate = is_aggregate(&name);
                let kind = if windowed {
                    Transformation::Window
                } else if aggregate {
                    Transformation::Aggregation
                } else if matches!(name.as_str(), "if" | "ifnull" | "nullif" | "coalesce") {
                    Transformation::Conditional
                } else {
                    Transformation::Transformation
                };
                retag(&mut inputs, kind, expr);
                dedup(&mut inputs);
                let mut inferred = infer_function(&name, &arg_types, inputs, state);
                inferred.complete = complete;
                inferred.aggregates = aggregates || aggregate || windowed;
                inferred
            }
            Expr::Extract { expr: inner, .. } => {
                let mut inner = self.infer(inner, scope, model, state);
                retag(&mut inner.inputs, Transformation::Transformation, expr);
                Inferred {
                    data_type: DataType::BigInt,
                    nullability: Nullability::Nullable,
                    inputs: inner.inputs,
                    complete: inner.complete,
                    aggregates: inner.aggregates,
                }
            }
            Expr::Substring {
                expr: inner,
                substring_from,
                substring_for,
                ..
            } => {
                let mut inputs = Vec::new();
                let mut complete = true;
                let mut aggregates = false;
                for part in [Some(inner), substring_from.as_ref(), substring_for.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    let inferred = self.infer(part, scope, model, state);
                    complete &= inferred.complete;
                    aggregates |= inferred.aggregates;
                    inputs.extend(inferred.inputs);
                }
                retag(&mut inputs, Transformation::Transformation, expr);
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Varchar,
                    nullability: Nullability::Nullable,
                    inputs,
                    complete,
                    aggregates,
                }
            }
            Expr::Trim {
                expr: inner,
                trim_what,
                trim_characters,
                ..
            } => {
                let mut parts: Vec<&Expr> = vec![inner.as_ref()];
                if let Some(what) = trim_what {
                    parts.push(what.as_ref());
                }
                if let Some(characters) = trim_characters {
                    parts.extend(characters.iter());
                }
                let mut inputs = Vec::new();
                let mut complete = true;
                let mut aggregates = false;
                for part in parts {
                    let inferred = self.infer(part, scope, model, state);
                    complete &= inferred.complete;
                    aggregates |= inferred.aggregates;
                    inputs.extend(inferred.inputs);
                }
                retag(&mut inputs, Transformation::Transformation, expr);
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Varchar,
                    nullability: Nullability::Nullable,
                    inputs,
                    complete,
                    aggregates,
                }
            }
            Expr::Ceil { expr: inner, .. } | Expr::Floor { expr: inner, .. } => {
                let mut inner = self.infer(inner, scope, model, state);
                retag(&mut inner.inputs, Transformation::Transformation, expr);
                Inferred {
                    data_type: inner.data_type,
                    nullability: inner.nullability,
                    inputs: inner.inputs,
                    complete: inner.complete,
                    aggregates: inner.aggregates,
                }
            }
            Expr::Interval(_)
            | Expr::Tuple(_)
            | Expr::Array(_)
            | Expr::Map(_)
            | Expr::Struct { .. } => Inferred::unknown(),
            Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                state
                    .limitations
                    .insert("subquery expressions are not analysed".to_string());
                Inferred::unknown()
            }
            _ => {
                state
                    .limitations
                    .insert(format!("expression `{expr}` is not analysed"));
                Inferred::unknown()
            }
        }
    }

    fn resolve_column(
        &self,
        scope: &Scope,
        qualifier: Option<&str>,
        name: &str,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> Inferred {
        let matches = scope.resolve(qualifier, name);
        match matches.as_slice() {
            [] => {
                let identifier = match qualifier {
                    Some(qualifier) => format!("{qualifier}.{name}"),
                    None => name.to_string(),
                };
                if state.unknown_relations {
                    state.limitations.insert(format!(
                        "column `{identifier}` could not be resolved because an input schema is unknown"
                    ));
                } else {
                    state.diagnostics.push(
                        Diagnostic::error(
                            codes::TYPE_UNKNOWN_COLUMN,
                            format!("unknown column `{identifier}`"),
                        )
                        .with_path(model.logical_name()),
                    );
                }
                Inferred::unknown()
            }
            [column] => Inferred {
                data_type: column.data_type.clone(),
                nullability: column.nullability,
                inputs: column.inputs.clone(),
                complete: column.complete,
                aggregates: false,
            },
            columns => {
                state.diagnostics.push(
                    Diagnostic::error(
                        codes::TYPE_AMBIGUOUS_COLUMN,
                        format!("ambiguous column `{name}`"),
                    )
                    .with_path(model.logical_name())
                    .with_labels(columns.iter().map(|column| {
                        column
                            .inputs
                            .first()
                            .map(|input| input.column.display())
                            .unwrap_or_else(|| column.name.clone())
                    }))
                    .with_help("qualify the column with a table alias"),
                );
                Inferred::unknown()
            }
        }
    }
}

/// A resolved input column with lineage inputs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Column {
    pub name: String,
    pub data_type: DataType,
    pub nullability: Nullability,
    pub inputs: Vec<ColumnInput>,
    /// False when part of what produced this column could not be analysed.
    pub complete: bool,
}

/// Inferred attributes of an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Inferred {
    data_type: DataType,
    nullability: Nullability,
    inputs: Vec<ColumnInput>,
    /// False when part of the expression was not analysed, so `inputs` may
    /// be incomplete.
    complete: bool,
    /// True when the expression contains an aggregate or window function —
    /// used to attach grouping keys as indirect inputs.
    aggregates: bool,
}

impl Inferred {
    fn unknown() -> Self {
        Self {
            data_type: DataType::Unknown,
            nullability: Nullability::Unknown,
            inputs: Vec::new(),
            complete: false,
            aggregates: false,
        }
    }

    fn unknown_from(inputs: Vec<ColumnInput>, complete: bool) -> Self {
        Self {
            data_type: DataType::Unknown,
            nullability: Nullability::Unknown,
            inputs,
            complete,
            aggregates: false,
        }
    }
}

/// A relation visible to column resolution.
#[derive(Clone, Debug)]
struct ScopedRelation {
    qualifier: Option<String>,
    columns: Vec<Column>,
}

/// The set of relations in scope for a SELECT.
#[derive(Clone, Debug, Default)]
struct Scope {
    relations: Vec<ScopedRelation>,
}

impl Scope {
    fn all_columns(&self) -> Vec<Column> {
        self.relations
            .iter()
            .flat_map(|relation| relation.columns.iter().cloned())
            .collect()
    }

    fn columns_for_qualifier(&self, qualifier: &str) -> Vec<Column> {
        self.relations
            .iter()
            .filter(|relation| {
                relation
                    .qualifier
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(qualifier))
            })
            .flat_map(|relation| relation.columns.iter().cloned())
            .collect()
    }

    fn resolve(&self, qualifier: Option<&str>, name: &str) -> Vec<Column> {
        let mut matches = Vec::new();
        for relation in &self.relations {
            if let Some(qualifier) = qualifier {
                let relation_matches = relation
                    .qualifier
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(qualifier));
                if !relation_matches {
                    continue;
                }
            }
            for column in &relation.columns {
                if column.name.eq_ignore_ascii_case(name) {
                    matches.push(column.clone());
                }
            }
        }
        matches
    }
}

/// The FROM scope plus what join constraints contributed to lineage.
struct BuiltScope {
    scope: Scope,
    /// Indirect inputs from join constraints, already classified.
    join_inputs: Vec<ColumnInput>,
    /// False when a join constraint could not be fully analysed.
    complete: bool,
}

impl Default for BuiltScope {
    fn default() -> Self {
        Self {
            scope: Scope::default(),
            join_inputs: Vec::new(),
            complete: true,
        }
    }
}

/// CTE output columns, keyed by lower-case CTE name.
#[derive(Default)]
struct CteEnv {
    entries: BTreeMap<String, Vec<Column>>,
}

impl CteEnv {
    fn insert(&mut self, name: String, columns: Vec<Column>) {
        self.entries.insert(name, columns);
    }

    fn get(&self, name: &str) -> Option<&Vec<Column>> {
        self.entries.get(name)
    }
}

#[derive(Default)]
struct AnalyzeState {
    diagnostics: Vec<Diagnostic>,
    limitations: BTreeSet<String>,
    /// True when at least one input relation's schema was unavailable, so
    /// unresolved column references may be false positives.
    unknown_relations: bool,
}

/// Retag every direct input with the transformation `expr` applies, and
/// record the expression as provenance. Indirect inputs keep their original
/// classification — a filter on a subquery stays a filter upstream of the
/// expression consuming its output.
fn retag(inputs: &mut [ColumnInput], transformation: Transformation, expr: &Expr) {
    for input in inputs.iter_mut() {
        if input.directness == Directness::Direct {
            input.transformation = transformation;
            input.expression = Some(expr.to_string());
        }
    }
}

/// Reclassify clause inputs as indirect contributors of the given kind.
fn mark_indirect(
    inputs: impl IntoIterator<Item = ColumnInput>,
    transformation: Transformation,
    expression: String,
) -> Vec<ColumnInput> {
    inputs
        .into_iter()
        .map(|mut input| {
            input.directness = Directness::Indirect;
            input.transformation = transformation;
            input.expression = Some(expression.clone());
            input
        })
        .collect()
}

/// Function names treated as aggregates for lineage purposes.
fn is_aggregate(name: &str) -> bool {
    matches!(
        name,
        "count"
            | "count_if"
            | "sum"
            | "avg"
            | "min"
            | "max"
            | "arbitrary"
            | "any_value"
            | "approx_distinct"
            | "approx_percentile"
            | "approx_set"
            | "stddev"
            | "stddev_samp"
            | "stddev_pop"
            | "variance"
            | "var_samp"
            | "var_pop"
            | "bool_and"
            | "bool_or"
            | "every"
            | "array_agg"
            | "listagg"
            | "checksum"
            | "merge"
    )
}

/// The bare and qualified identifiers an expression references, for
/// diagnostics-free resolution (`ORDER BY` keys, window clauses).
fn expression_identifiers(expr: &Expr) -> Vec<(Option<String>, String)> {
    let mut found = Vec::new();
    collect_identifiers(expr, &mut found);
    found
}

fn collect_identifiers(expr: &Expr, found: &mut Vec<(Option<String>, String)>) {
    match expr {
        Expr::Identifier(ident) => found.push((None, ident.value.clone())),
        Expr::CompoundIdentifier(idents) if idents.len() >= 2 => {
            found.push((
                Some(idents[idents.len() - 2].value.clone()),
                idents[idents.len() - 1].value.clone(),
            ));
        }
        Expr::Nested(inner) => collect_identifiers(inner, found),
        Expr::Cast { expr, .. } => collect_identifiers(expr, found),
        Expr::UnaryOp { expr, .. } => collect_identifiers(expr, found),
        Expr::BinaryOp { left, right, .. } => {
            collect_identifiers(left, found);
            collect_identifiers(right, found);
        }
        Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner) => collect_identifiers(inner, found),
        Expr::InList { expr, list, .. } => {
            collect_identifiers(expr, found);
            for item in list {
                collect_identifiers(item, found);
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_identifiers(expr, found);
            collect_identifiers(low, found);
            collect_identifiers(high, found);
        }
        Expr::Like { expr, pattern, .. }
        | Expr::ILike { expr, pattern, .. }
        | Expr::SimilarTo { expr, pattern, .. } => {
            collect_identifiers(expr, found);
            collect_identifiers(pattern, found);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect_identifiers(operand, found);
            }
            for case_when in conditions {
                collect_identifiers(&case_when.condition, found);
                collect_identifiers(&case_when.result, found);
            }
            if let Some(else_result) = else_result {
                collect_identifiers(else_result, found);
            }
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(list) = &function.args {
                for arg in &list.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) = arg {
                        collect_identifiers(arg, found);
                    }
                }
            }
            if let Some(WindowType::WindowSpec(spec)) = &function.over {
                for partition in &spec.partition_by {
                    collect_identifiers(partition, found);
                }
                for order in &spec.order_by {
                    collect_identifiers(&order.expr, found);
                }
            }
        }
        Expr::Extract { expr, .. } | Expr::Ceil { expr, .. } | Expr::Floor { expr, .. } => {
            collect_identifiers(expr, found)
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            collect_identifiers(expr, found);
            for part in [substring_from.as_ref(), substring_for.as_ref()]
                .into_iter()
                .flatten()
            {
                collect_identifiers(part, found);
            }
        }
        Expr::Trim {
            expr,
            trim_what,
            trim_characters,
            ..
        } => {
            collect_identifiers(expr, found);
            if let Some(what) = trim_what {
                collect_identifiers(what, found);
            }
            if let Some(characters) = trim_characters {
                for part in characters {
                    collect_identifiers(part, found);
                }
            }
        }
        _ => {}
    }
}

fn infer_value(value: &sqlparser::ast::Value) -> Inferred {
    use sqlparser::ast::Value;
    let (data_type, nullability) = match value {
        Value::Number(text, _) => (
            if text.contains('.') || text.contains('e') || text.contains('E') {
                DataType::Double
            } else {
                DataType::Integer
            },
            Nullability::NotNull,
        ),
        Value::Boolean(_) => (DataType::Boolean, Nullability::NotNull),
        Value::Null => (DataType::Unknown, Nullability::Nullable),
        Value::SingleQuotedString(_)
        | Value::DoubleQuotedString(_)
        | Value::TripleSingleQuotedString(_)
        | Value::TripleDoubleQuotedString(_)
        | Value::EscapedStringLiteral(_)
        | Value::UnicodeStringLiteral(_)
        | Value::NationalStringLiteral(_)
        | Value::SingleQuotedRawStringLiteral(_)
        | Value::DoubleQuotedRawStringLiteral(_) => (DataType::Varchar, Nullability::NotNull),
        _ => {
            return Inferred {
                data_type: DataType::Unknown,
                nullability: Nullability::Unknown,
                inputs: Vec::new(),
                complete: true,
                aggregates: false,
            }
        }
    };
    Inferred {
        data_type,
        nullability,
        inputs: Vec::new(),
        complete: true,
        aggregates: false,
    }
}

fn merge_type(slot: &mut Option<DataType>, incoming: &DataType) {
    *slot = Some(match slot.take() {
        Some(existing) => DataType::widen(&existing, incoming),
        None => incoming.clone(),
    });
}

fn infer_binary(op: &BinaryOperator, left: Inferred, right: Inferred, expr: &Expr) -> Inferred {
    let complete = left.complete && right.complete;
    let aggregates = left.aggregates || right.aggregates;
    let mut inputs = left.inputs;
    inputs.extend(right.inputs);
    retag(&mut inputs, Transformation::Transformation, expr);
    dedup(&mut inputs);
    let nullability = left.nullability.merge(right.nullability);

    use BinaryOperator::*;
    match op {
        Plus | Minus | Multiply | Modulo => Inferred {
            data_type: DataType::widen(&left.data_type, &right.data_type),
            nullability,
            inputs,
            complete,
            aggregates,
        },
        Divide => Inferred {
            data_type: if left.data_type.is_numeric() && right.data_type.is_numeric() {
                DataType::Double
            } else {
                DataType::Unknown
            },
            nullability,
            inputs,
            complete,
            aggregates,
        },
        StringConcat => Inferred {
            data_type: DataType::Varchar,
            nullability,
            inputs,
            complete,
            aggregates,
        },
        Gt | Lt | GtEq | LtEq | Eq | NotEq | And | Or | Xor => Inferred {
            data_type: DataType::Boolean,
            nullability,
            inputs,
            complete,
            aggregates,
        },
        _ => Inferred::unknown_from(inputs, complete),
    }
}

fn infer_function(
    name: &str,
    args: &[DataType],
    inputs: Vec<ColumnInput>,
    state: &mut AnalyzeState,
) -> Inferred {
    let first = args.first().cloned().unwrap_or(DataType::Unknown);
    let nullability = Nullability::Unknown;
    let result = match name {
        "count" | "count_if" => DataType::BigInt,
        "sum" => first,
        "avg" | "approx_percentile" | "stddev" | "variance" => DataType::Double,
        "min" | "max" | "arbitrary" | "any_value" => first,
        "approx_distinct" => DataType::BigInt,
        "bool_and" | "bool_or" | "every" => DataType::Boolean,
        "array_agg" => DataType::Array(Box::new(first)),
        "coalesce" | "if" | "greatest" | "least" | "nullif" => args
            .iter()
            .cloned()
            .reduce(|left, right| DataType::widen(&left, &right))
            .unwrap_or(DataType::Unknown),
        "concat"
        | "lower"
        | "upper"
        | "substr"
        | "substring"
        | "trim"
        | "ltrim"
        | "rtrim"
        | "replace"
        | "format"
        | "format_datetime"
        | "json_extract_scalar" => DataType::Varchar,
        "length" | "cardinality" => DataType::BigInt,
        "abs" | "round" | "truncate" | "ceil" | "floor" | "sign" => first,
        "date" | "current_date" => DataType::Date,
        "now" | "current_timestamp" | "localtimestamp" => DataType::TimestampTz,
        "date_trunc" => DataType::Timestamp,
        "cast" | "try_cast" => first,
        "row_number" | "rank" | "dense_rank" | "ntile" => DataType::BigInt,
        "json_extract" => DataType::Unknown,
        _ => {
            state
                .limitations
                .insert(format!("function `{name}` has unknown result type"));
            DataType::Unknown
        }
    };
    let nullability = match name {
        "count" | "count_if" | "approx_distinct" | "row_number" | "rank" | "dense_rank" => {
            Nullability::NotNull
        }
        "current_date" | "now" | "current_timestamp" | "localtimestamp" => Nullability::NotNull,
        _ => nullability,
    };
    Inferred {
        data_type: result,
        nullability,
        inputs,
        complete: true,
        aggregates: false,
    }
}

fn expression_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(idents) => idents.last().map(|ident| ident.value.clone()),
        _ => None,
    }
}

fn report_duplicate_outputs(columns: &[Column], model: &ModelId, state: &mut AnalyzeState) {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for column in columns {
        *seen.entry(column.name.to_lowercase()).or_default() += 1;
    }
    for (name, count) in seen {
        if count > 1 {
            state.diagnostics.push(
                Diagnostic::warning(
                    codes::TYPE_DUPLICATE_COLUMN,
                    format!("output column `{name}` appears {count} times"),
                )
                .with_path(model.logical_name())
                .with_help("alias duplicate output columns uniquely"),
            );
        }
    }
}

/// Sort and deduplicate inputs. Two entries for the same upstream column
/// collapse only when they agree on how the column contributes.
fn dedup(inputs: &mut Vec<ColumnInput>) {
    inputs.sort();
    inputs.dedup_by(|left, right| {
        left.column == right.column
            && left.directness == right.directness
            && left.transformation == right.transformation
    });
}
