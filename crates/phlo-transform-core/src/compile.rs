//! The compiler: parse, resolve and build the dependency graph.
//!
//! Compilation is side-effect free and operates purely on the semantic
//! project representation. The native frontend is not required.

use std::collections::{BTreeMap, BTreeSet};

use phlo_transform_sql::{
    extract_relations, parse_statements, Dialect, DirectiveIssueKind, RelationName,
};
use sqlparser::ast::Statement;

use crate::compiled::{Compilation, CompiledModel, CompiledTest};
use crate::diagnostics::{codes, Diagnostic, Severity};
use crate::graph::Dependency;
use crate::identity::ModelId;
use crate::model::{Relation, SemanticModel, SemanticProject, WorkspaceDefaults};
use crate::resolve::{RegistryEntry, Resolution, Resolver};
use crate::rewrite::rewrite_statements;

/// Compile a semantic project into models and a dependency graph.
pub fn compile(project: &SemanticProject) -> Compilation {
    let mut diagnostics: Vec<Diagnostic> = project.diagnostics.clone();

    // Lower directives and parse SQL for each model.
    let mut lowered: Vec<LoweredModel> = Vec::with_capacity(project.models.len());
    for model in &project.models {
        let path = model_path(&model.origin);
        emit_directive_diagnostics(model, path.as_deref(), &mut diagnostics);
        let pinned_id = lower_pinned_id(model, path.as_deref(), &mut diagnostics);

        let statements = match parse_dialect(&model.sql) {
            Ok(statements) => statements,
            Err(error) => {
                diagnostics.push(parse_diagnostic(
                    &error,
                    path.as_deref(),
                    "could not parse model SQL",
                ));
                Vec::new()
            }
        };

        lowered.push(LoweredModel {
            model,
            path,
            pinned_id,
            statements,
        });
    }

    // Sort by identity so output is deterministic regardless of walk order.
    lowered.sort_by(|left, right| left.model.id.cmp(&right.model.id));

    // Detect duplicate identities; keep the first deterministically.
    let mut unique: Vec<&LoweredModel> = Vec::with_capacity(lowered.len());
    let mut duplicates: Vec<Vec<&LoweredModel>> = Vec::new();
    for lowered_model in &lowered {
        if let Some(previous) = unique
            .iter()
            .find(|entry| entry.model.id == lowered_model.model.id)
        {
            if let Some(group) = duplicates
                .iter_mut()
                .find(|group| group[0].model.id == previous.model.id)
            {
                group.push(lowered_model);
            } else {
                duplicates.push(vec![*previous, lowered_model]);
            }
        } else {
            unique.push(lowered_model);
        }
    }
    for group in &duplicates {
        let mut diagnostic = Diagnostic::error(
            codes::PROJECT_DUPLICATE_MODEL,
            format!("duplicate model id `{}`", group[0].model.id.logical_name()),
        );
        for model in group {
            if let Some(path) = &model.path {
                diagnostic.labels.push(path.clone());
            } else {
                diagnostic
                    .labels
                    .push(format!("({})", model.model.id.uri()));
            }
        }
        diagnostics.push(diagnostic);
    }

    // Build the resolver over unique models.
    let entries: Vec<RegistryEntry> = unique.iter().map(|model| entry_for(model)).collect();
    let resolver = Resolver::new(entries);

    // Physical targets.
    let mut targets: BTreeMap<ModelId, Relation> = BTreeMap::new();
    for entry in &unique {
        targets.insert(
            entry.model.id.clone(),
            target_relation(entry.model, &project.defaults),
        );
    }
    detect_target_collisions(&unique, &targets, &mut diagnostics);

    // Resolve dependencies and compile SQL for each unique model.
    let mut compiled_models: Vec<CompiledModel> = Vec::with_capacity(unique.len());
    for entry in &unique {
        let registry_entry = entry_for(entry);
        let mut dependencies: Vec<Dependency> = Vec::new();
        for relation in extract_relations(&entry.statements)
            .into_iter()
            .map(|relation| relation.name)
        {
            match resolver.resolve(&registry_entry, &relation) {
                Resolution::Model(id) => dependencies.push(Dependency::Model(id)),
                Resolution::External(source) => dependencies.push(Dependency::Source(source)),
                Resolution::Ambiguous(candidates) => {
                    diagnostics.push(ambiguous_diagnostic(
                        entry.path.as_deref(),
                        &relation,
                        &candidates,
                    ));
                }
            }
        }
        dependencies.sort();
        dependencies.dedup();

        let mut statements = entry.statements.clone();
        let compiled_sql =
            rewrite_statements(&mut statements, Some(&registry_entry), &resolver, &targets);

        compiled_models.push(CompiledModel {
            id: entry.model.id.clone(),
            namespace: entry.model.namespace.clone(),
            path: entry.model.path.clone(),
            origin: entry.model.origin.clone(),
            sql: entry.model.sql.clone(),
            config: entry.model.config.clone(),
            target: targets[&entry.model.id].clone(),
            compiled_sql,
            pinned_id: entry.pinned_id.clone(),
            dependencies,
        });
    }

    let compiled_tests = compile_tests(project, &resolver, &targets, &mut diagnostics);

    let mut compilation = Compilation::new(
        project.workspace_root.clone(),
        project.roots.clone(),
        compiled_models,
        compiled_tests,
        diagnostics,
    );

    if let Some(cycle) = compilation.graph.cycle() {
        let path = cycle
            .iter()
            .map(|id| id.logical_name())
            .collect::<Vec<_>>()
            .join(" -> ");
        compilation.diagnostics.push(
            Diagnostic::error(codes::GRAPH_CYCLE, "transformation cycle detected")
                .with_labels([path])
                .with_help("break the cycle by removing one of the references"),
        );
    }

    compilation
}

struct LoweredModel<'a> {
    model: &'a SemanticModel,
    path: Option<String>,
    pinned_id: Option<ModelId>,
    statements: Vec<Statement>,
}

fn entry_for(model: &LoweredModel<'_>) -> RegistryEntry {
    RegistryEntry {
        id: model.model.id.clone(),
        namespace: model.model.namespace.clone(),
        path: model.model.path.clone(),
        root: model.model.root.clone(),
    }
}

fn parse_dialect(sql: &str) -> Result<Vec<Statement>, phlo_transform_sql::SqlParseError> {
    parse_statements(sql, Dialect::Generic)
}

fn target_relation(model: &SemanticModel, defaults: &WorkspaceDefaults) -> Relation {
    let namespace = model.namespace.as_str();
    let schema = model
        .config
        .schema
        .clone()
        .or_else(|| defaults.schema.clone())
        .unwrap_or_else(|| namespace.to_string());
    // When models share a schema, the namespace is folded into the table name
    // so that targets stay unique. This is an intentional MVP simplification;
    // schema contracts and per-namespace schemas come later.
    let table = if schema == namespace {
        model.path.join("__")
    } else {
        format!("{namespace}__{}", model.path.join("__"))
    };
    Relation {
        catalog: defaults.catalog.clone(),
        schema,
        table,
    }
}

fn detect_target_collisions(
    unique: &[&LoweredModel<'_>],
    targets: &BTreeMap<ModelId, Relation>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut by_target: BTreeMap<Relation, Vec<&LoweredModel<'_>>> = BTreeMap::new();
    for entry in unique {
        if let Some(target) = targets.get(&entry.model.id) {
            by_target.entry(target.clone()).or_default().push(entry);
        }
    }
    for (target, models) in by_target {
        if models.len() > 1 {
            let mut diagnostic = Diagnostic::error(
                codes::PROJECT_TARGET_COLLISION,
                format!("multiple models target the relation `{}`", target.display()),
            );
            for model in models {
                diagnostic.labels.push(format!(
                    "{} ({})",
                    model.model.id.logical_name(),
                    model.path.as_deref().unwrap_or("in memory")
                ));
            }
            diagnostics.push(diagnostic);
        }
    }
}

fn compile_tests(
    project: &SemanticProject,
    resolver: &Resolver,
    targets: &BTreeMap<ModelId, Relation>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<CompiledTest> {
    let mut tests = Vec::with_capacity(project.tests.len());
    for test in &project.tests {
        let path = model_path(&test.origin);
        let mut statements = match parse_dialect(&test.sql) {
            Ok(statements) => statements,
            Err(error) => {
                diagnostics.push(parse_diagnostic(
                    &error,
                    path.as_deref(),
                    "could not parse test SQL",
                ));
                continue;
            }
        };

        let mut model_targets: BTreeSet<ModelId> = BTreeSet::new();
        let mut sources: BTreeSet<crate::identity::SourceId> = BTreeSet::new();
        for relation in extract_relations(&statements)
            .into_iter()
            .map(|relation| relation.name)
        {
            match resolver.resolve_global(&relation) {
                Resolution::Model(id) => {
                    model_targets.insert(id);
                }
                Resolution::External(source) => {
                    sources.insert(source);
                }
                Resolution::Ambiguous(candidates) => {
                    diagnostics.push(ambiguous_diagnostic(
                        path.as_deref(),
                        &relation,
                        &candidates,
                    ));
                }
            }
        }

        let compiled_sql = rewrite_statements(&mut statements, None, resolver, targets);

        tests.push(CompiledTest {
            id: test.id.clone(),
            origin: test.origin.clone(),
            sql: test.sql.clone(),
            compiled_sql,
            targets: model_targets.into_iter().collect(),
            sources: sources.into_iter().collect(),
        });
    }
    tests.sort_by(|left, right| left.id.cmp(&right.id));
    tests
}

fn model_path(origin: &crate::model::ModelOrigin) -> Option<String> {
    origin
        .path
        .as_ref()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
}

fn parse_diagnostic(
    error: &phlo_transform_sql::SqlParseError,
    path: Option<&str>,
    context: &str,
) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
        codes::PARSE_INVALID_SQL,
        format!("{context}: {}", error.message()),
    );
    if let Some(path) = path {
        diagnostic = diagnostic.with_path(path.to_string());
    }
    diagnostic
}

fn emit_directive_diagnostics(
    model: &SemanticModel,
    path: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for issue in &model.directives.issues {
        let (severity, code, message) = match issue.kind {
            DirectiveIssueKind::MissingValue => (
                Severity::Error,
                codes::PARSE_MALFORMED_DIRECTIVE,
                format!("directive `{}` requires a value", issue.name),
            ),
            DirectiveIssueKind::InvalidValue => (
                Severity::Error,
                codes::PARSE_MALFORMED_DIRECTIVE,
                format!("directive `{}` has an invalid value", issue.name),
            ),
            DirectiveIssueKind::DuplicateId => (
                Severity::Error,
                codes::PARSE_MALFORMED_DIRECTIVE,
                format!("directive `{}` is declared more than once", issue.name),
            ),
            DirectiveIssueKind::ConflictingMaterialization => (
                Severity::Error,
                codes::PARSE_MALFORMED_DIRECTIVE,
                format!("conflicting materialisation directive `{}`", issue.name),
            ),
            DirectiveIssueKind::UnknownDirective => (
                Severity::Warning,
                codes::PARSE_UNKNOWN_DIRECTIVE,
                format!("unknown directive `{}`", issue.name),
            ),
        };
        let mut diagnostic = match severity {
            Severity::Error => Diagnostic::error(code, message),
            Severity::Warning => Diagnostic::warning(code, message),
            Severity::Info => Diagnostic::info(code, message),
        };
        if let Some(path) = path {
            diagnostic = diagnostic.with_path(format!("{path}:{}", issue.line));
        }
        diagnostics.push(diagnostic);
    }
}

fn lower_pinned_id(
    model: &SemanticModel,
    path: Option<&str>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ModelId> {
    let raw = model.directives.pinned_id.as_ref()?;
    if model.directives.issues.iter().any(|issue| {
        issue.kind == DirectiveIssueKind::DuplicateId
            || issue.kind == DirectiveIssueKind::MissingValue
    }) {
        // Do not trust a conflicting directive; the metadata error is already
        // reported.
        return None;
    }
    match ModelId::parse(raw) {
        Ok(id) => Some(id),
        Err(error) => {
            let mut diagnostic = Diagnostic::error(
                codes::PROJECT_INVALID_MODEL_ID,
                format!("invalid pinned model id `{raw}`: {error}"),
            );
            if let Some(path) = path {
                diagnostic = diagnostic.with_path(path.to_string());
            }
            diagnostics.push(diagnostic);
            None
        }
    }
}

fn ambiguous_diagnostic(
    path: Option<&str>,
    relation: &RelationName,
    candidates: &[ModelId],
) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
        codes::RESOLUTION_AMBIGUOUS,
        format!("ambiguous relation `{}`", relation.as_dotted()),
    );
    if let Some(path) = path {
        diagnostic = diagnostic.with_path(path.to_string());
    }
    diagnostic =
        diagnostic.with_labels(candidates.iter().map(|candidate| candidate.logical_name()));
    diagnostic.with_help("use a qualified model name")
}
