//! Column resolution, type inference and lineage.
//!
//! This is a deliberately pragmatic resolver for the common Trino
//! transformation SQL. Anything it cannot reason about is marked
//! [`DataType::Unknown`] and recorded as a limitation rather than guessed.
//!
//! The analyzer works on the parsed AST plus the already-computed output
//! schemas of upstream models and a [`SchemaProvider`] for external sources.
//! It does not mutate the parser AST.

use std::collections::{BTreeMap, BTreeSet};

use sqlparser::ast::Statement;
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArguments, GroupByExpr, JoinConstraint,
    JoinOperator, Query, Select, SelectItem, SelectItemQualifiedWildcardKind, SetExpr, TableFactor,
    TableWithJoins, UnaryOperator,
};

use crate::diagnostics::{codes, Diagnostic};
use crate::identity::{ModelId, SourceId};
use crate::resolve::{RegistryEntry, Resolution, Resolver};
use crate::schema::SchemaProvider;
use crate::semantic::{ColumnRef, DataType, ModelSchema, Nullability, OutputColumn};

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
        self.analyze_set_expr(&query.body, current, ctes, model, state)
    }

    fn analyze_set_expr(
        &self,
        body: &SetExpr,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> Vec<Column> {
        match body {
            SetExpr::Select(select) => self.analyze_select(select, current, ctes, model, state),
            SetExpr::Query(query) => self.analyze_query(query, current, ctes, model, state),
            SetExpr::SetOperation { left, right, .. } => {
                let left_columns = self.analyze_set_expr(left, current, ctes, model, state);
                let right_columns = self.analyze_set_expr(right, current, ctes, model, state);
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
                    });
                }
                combined
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
                                let inferred =
                                    self.infer(expr, &Scope::default(), ctes, model, state);
                                Column {
                                    name: format!("column{index}"),
                                    data_type: inferred.data_type,
                                    nullability: inferred.nullability,
                                    inputs: inferred.inputs,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                columns
            }
            _ => {
                state
                    .limitations
                    .insert("unsupported query body".to_string());
                Vec::new()
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
    ) -> Vec<Column> {
        let scope = self.build_scope(&select.from, current, ctes, model, state);

        // Validate predicates so unknown columns are caught.
        if let Some(selection) = &select.selection {
            let _ = self.infer(selection, &scope, ctes, model, state);
        }
        if let Some(having) = &select.having {
            let _ = self.infer(having, &scope, ctes, model, state);
        }
        if let GroupByExpr::Expressions(expressions, _) = &select.group_by {
            for expression in expressions {
                let _ = self.infer(expression, &scope, ctes, model, state);
            }
        }

        let mut outputs = Vec::new();
        for item in &select.projection {
            match item {
                SelectItem::UnnamedExpr(expr) => {
                    let inferred = self.infer(expr, &scope, ctes, model, state);
                    outputs.push(Column {
                        name: expression_name(expr)
                            .unwrap_or_else(|| format!("column{}", outputs.len())),
                        data_type: inferred.data_type,
                        nullability: inferred.nullability,
                        inputs: inferred.inputs,
                    });
                }
                SelectItem::ExprWithAlias { expr, alias } => {
                    let inferred = self.infer(expr, &scope, ctes, model, state);
                    outputs.push(Column {
                        name: alias.value.clone(),
                        data_type: inferred.data_type,
                        nullability: inferred.nullability,
                        inputs: inferred.inputs,
                    });
                }
                SelectItem::Wildcard(_) => {
                    for column in scope.all_columns() {
                        outputs.push(column.clone());
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
                        outputs.push(column);
                    }
                }
                SelectItem::ExprWithAliases { expr, aliases } => {
                    let inferred = self.infer(expr, &scope, ctes, model, state);
                    for alias in aliases {
                        outputs.push(Column {
                            name: alias.value.clone(),
                            data_type: inferred.data_type.clone(),
                            nullability: inferred.nullability,
                            inputs: inferred.inputs.clone(),
                        });
                    }
                }
            }
        }

        report_duplicate_outputs(&outputs, model, state);
        outputs
    }

    fn build_scope(
        &self,
        from: &[TableWithJoins],
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) -> Scope {
        let mut scope = Scope::default();
        for table in from {
            self.add_table_with_joins(table, &mut scope, current, ctes, model, state);
        }
        scope
    }

    fn add_table_with_joins(
        &self,
        table: &TableWithJoins,
        scope: &mut Scope,
        current: &RegistryEntry,
        ctes: &mut CteEnv,
        model: &ModelId,
        state: &mut AnalyzeState,
    ) {
        if let Some(relation) = self.table_factor(&table.relation, current, ctes, model, state) {
            scope.relations.push(relation);
        }
        for join in &table.joins {
            let Some(right) = self.table_factor(&join.relation, current, ctes, model, state) else {
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
                    let mut with_right = scope.clone();
                    with_right.relations.push(right.clone());
                    let _ = self.infer(&expr, &with_right, ctes, model, state);
                    scope.relations.push(right);
                }
                JoinConstraint::Using(names) => {
                    let using: BTreeSet<String> = names
                        .iter()
                        .filter_map(|name| name.0.last().and_then(|part| part.as_ident()))
                        .map(|ident| ident.value.to_lowercase())
                        .collect();
                    let mut filtered = right;
                    // `USING` columns are exposed once. Drop the right copy
                    // only when the left side already provides it.
                    filtered.columns.retain(|column| {
                        let name = column.name.to_lowercase();
                        !using.contains(&name) || scope.resolve(None, &name).is_empty()
                    });
                    scope.relations.push(filtered);
                    // Verify the using columns exist somewhere.
                    for name in &using {
                        if scope.resolve(None, name).is_empty() && !state.unknown_relations {
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
                JoinConstraint::Natural | JoinConstraint::None => {
                    scope.relations.push(right);
                }
            }
        }
    }

    fn table_factor(
        &self,
        factor: &TableFactor,
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
                    return None;
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
                let mut nested = Scope::default();
                self.add_table_with_joins(
                    table_with_joins,
                    &mut nested,
                    current,
                    ctes,
                    model,
                    state,
                );
                let columns = nested.all_columns();
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
                        inputs: vec![ColumnRef::model(id.clone(), &column.name)],
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
                    inputs: vec![ColumnRef::source(source.clone(), &column.name)],
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

    /// Infer the type, nullability and lineage inputs of an expression.
    ///
    /// `ctes` is threaded through for symmetry with the resolver and future
    /// expression forms that may need CTE lookup.
    #[allow(clippy::only_used_in_recursion)]
    fn infer(
        &self,
        expr: &Expr,
        scope: &Scope,
        ctes: &mut CteEnv,
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
            },
            Expr::Nested(inner) => self.infer(inner, scope, ctes, model, state),
            Expr::Cast {
                kind,
                expr,
                data_type,
                ..
            } => {
                let inner = self.infer(expr, scope, ctes, model, state);
                let nullability = if matches!(kind, sqlparser::ast::CastKind::TryCast) {
                    Nullability::Nullable
                } else {
                    inner.nullability
                };
                Inferred {
                    data_type: DataType::parse_trino(&data_type.to_string()),
                    nullability,
                    inputs: inner.inputs,
                }
            }
            Expr::UnaryOp { op, expr } => {
                let inner = self.infer(expr, scope, ctes, model, state);
                match op {
                    UnaryOperator::Not => Inferred {
                        data_type: DataType::Boolean,
                        nullability: inner.nullability,
                        inputs: inner.inputs,
                    },
                    UnaryOperator::Minus | UnaryOperator::Plus => inner,
                    _ => Inferred::unknown_from(inner.inputs),
                }
            }
            Expr::BinaryOp { left, op, right } => {
                let left = self.infer(left, scope, ctes, model, state);
                let right = self.infer(right, scope, ctes, model, state);
                infer_binary(op, left, right)
            }
            Expr::IsNull(inner)
            | Expr::IsNotNull(inner)
            | Expr::IsTrue(inner)
            | Expr::IsNotTrue(inner)
            | Expr::IsFalse(inner)
            | Expr::IsNotFalse(inner)
            | Expr::IsUnknown(inner)
            | Expr::IsNotUnknown(inner) => {
                let inner = self.infer(inner, scope, ctes, model, state);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::NotNull,
                    inputs: inner.inputs,
                }
            }
            Expr::InList { expr, list, .. } => {
                let mut inputs = self.infer(expr, scope, ctes, model, state).inputs;
                for item in list {
                    inputs.extend(self.infer(item, scope, ctes, model, state).inputs);
                }
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::Unknown,
                    inputs,
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                let mut inputs = self.infer(expr, scope, ctes, model, state).inputs;
                inputs.extend(self.infer(low, scope, ctes, model, state).inputs);
                inputs.extend(self.infer(high, scope, ctes, model, state).inputs);
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::Unknown,
                    inputs,
                }
            }
            Expr::Like { expr, pattern, .. } => {
                let mut inputs = self.infer(expr, scope, ctes, model, state).inputs;
                inputs.extend(self.infer(pattern, scope, ctes, model, state).inputs);
                dedup(&mut inputs);
                Inferred {
                    data_type: DataType::Boolean,
                    nullability: Nullability::Unknown,
                    inputs,
                }
            }
            Expr::Case {
                operand,
                conditions,
                else_result,
                ..
            } => {
                let mut inputs = Vec::new();
                if let Some(operand) = operand {
                    inputs.extend(self.infer(operand, scope, ctes, model, state).inputs);
                }
                let mut data_type: Option<DataType> = None;
                let mut nullability = if else_result.is_some() {
                    Nullability::NotNull
                } else {
                    Nullability::Nullable
                };
                for case_when in conditions {
                    inputs.extend(
                        self.infer(&case_when.condition, scope, ctes, model, state)
                            .inputs,
                    );
                    let inferred = self.infer(&case_when.result, scope, ctes, model, state);
                    merge_type(&mut data_type, &inferred.data_type);
                    nullability = nullability.merge(inferred.nullability);
                    inputs.extend(inferred.inputs);
                }
                if let Some(else_result) = else_result {
                    let inferred = self.infer(else_result, scope, ctes, model, state);
                    merge_type(&mut data_type, &inferred.data_type);
                    nullability = nullability.merge(inferred.nullability);
                    inputs.extend(inferred.inputs);
                }
                dedup(&mut inputs);
                Inferred {
                    data_type: data_type.unwrap_or(DataType::Unknown),
                    nullability,
                    inputs,
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
                if let FunctionArguments::List(list) = &function.args {
                    for arg in &list.args {
                        if let FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                            arg
                        {
                            let inferred = self.infer(expr, scope, ctes, model, state);
                            inputs.extend(inferred.inputs);
                            arg_types.push(inferred.data_type);
                        }
                    }
                }
                dedup(&mut inputs);
                infer_function(&name, &arg_types, inputs, state)
            }
            Expr::Extract { .. } => Inferred {
                data_type: DataType::BigInt,
                nullability: Nullability::Nullable,
                inputs: Vec::new(),
            },
            Expr::Substring { .. } | Expr::Trim { .. } => Inferred {
                data_type: DataType::Varchar,
                nullability: Nullability::Nullable,
                inputs: Vec::new(),
            },
            Expr::Ceil { expr, .. } | Expr::Floor { expr, .. } => {
                let inner = self.infer(expr, scope, ctes, model, state);
                Inferred {
                    data_type: inner.data_type,
                    nullability: inner.nullability,
                    inputs: inner.inputs,
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
                            .map(ColumnRef::display)
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
    pub inputs: Vec<ColumnRef>,
}

/// Inferred attributes of an expression.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Inferred {
    data_type: DataType,
    nullability: Nullability,
    inputs: Vec<ColumnRef>,
}

impl Inferred {
    fn unknown() -> Self {
        Self {
            data_type: DataType::Unknown,
            nullability: Nullability::Unknown,
            inputs: Vec::new(),
        }
    }

    fn unknown_from(inputs: Vec<ColumnRef>) -> Self {
        Self {
            data_type: DataType::Unknown,
            nullability: Nullability::Unknown,
            inputs,
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

fn infer_value(value: &sqlparser::ast::Value) -> Inferred {
    use sqlparser::ast::Value;
    match value {
        Value::Number(text, _) => Inferred {
            data_type: if text.contains('.') || text.contains('e') || text.contains('E') {
                DataType::Double
            } else {
                DataType::Integer
            },
            nullability: Nullability::NotNull,
            inputs: Vec::new(),
        },
        Value::Boolean(_) => Inferred {
            data_type: DataType::Boolean,
            nullability: Nullability::NotNull,
            inputs: Vec::new(),
        },
        Value::Null => Inferred {
            data_type: DataType::Unknown,
            nullability: Nullability::Nullable,
            inputs: Vec::new(),
        },
        Value::SingleQuotedString(_)
        | Value::DoubleQuotedString(_)
        | Value::TripleSingleQuotedString(_)
        | Value::TripleDoubleQuotedString(_)
        | Value::EscapedStringLiteral(_)
        | Value::UnicodeStringLiteral(_)
        | Value::NationalStringLiteral(_)
        | Value::SingleQuotedRawStringLiteral(_)
        | Value::DoubleQuotedRawStringLiteral(_) => Inferred {
            data_type: DataType::Varchar,
            nullability: Nullability::NotNull,
            inputs: Vec::new(),
        },
        _ => Inferred::unknown(),
    }
}

fn merge_type(slot: &mut Option<DataType>, incoming: &DataType) {
    *slot = Some(match slot.take() {
        Some(existing) => DataType::widen(&existing, incoming),
        None => incoming.clone(),
    });
}

fn infer_binary(op: &BinaryOperator, left: Inferred, right: Inferred) -> Inferred {
    let mut inputs = left.inputs;
    inputs.extend(right.inputs);
    dedup(&mut inputs);
    let nullability = left.nullability.merge(right.nullability);

    use BinaryOperator::*;
    match op {
        Plus | Minus | Multiply | Modulo => Inferred {
            data_type: DataType::widen(&left.data_type, &right.data_type),
            nullability,
            inputs,
        },
        Divide => Inferred {
            data_type: if left.data_type.is_numeric() && right.data_type.is_numeric() {
                DataType::Double
            } else {
                DataType::Unknown
            },
            nullability,
            inputs,
        },
        StringConcat => Inferred {
            data_type: DataType::Varchar,
            nullability,
            inputs,
        },
        Gt | Lt | GtEq | LtEq | Eq | NotEq | And | Or | Xor => Inferred {
            data_type: DataType::Boolean,
            nullability,
            inputs,
        },
        _ => Inferred::unknown_from(inputs),
    }
}

fn infer_function(
    name: &str,
    args: &[DataType],
    inputs: Vec<ColumnRef>,
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

fn dedup(inputs: &mut Vec<ColumnRef>) {
    sort_and_dedup(inputs);
}

fn sort_and_dedup(inputs: &mut Vec<ColumnRef>) {
    inputs.sort();
    inputs.dedup();
}
