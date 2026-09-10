//! The compiler: parse, resolve and build the dependency graph.
//!
//! Compilation is side-effect free and operates purely on the semantic
//! project representation. The native frontend is not required.

use phlo_transform_sql::{
    extract_relations, parse_statements, Dialect, DirectiveIssueKind, RelationName,
};

use crate::compiled::{Compilation, CompiledModel};
use crate::diagnostics::{codes, Diagnostic, Severity};
use crate::graph::Dependency;
use crate::identity::ModelId;
use crate::model::{SemanticModel, SemanticProject};
use crate::resolve::{RegistryEntry, Resolution, Resolver};

/// Compile a semantic project into models and a dependency graph.
pub fn compile(project: &SemanticProject) -> Compilation {
    let mut diagnostics: Vec<Diagnostic> = project.diagnostics.clone();

    // Lower directives: validate pinned identity and surface metadata issues.
    let mut lowered: Vec<LoweredModel> = Vec::with_capacity(project.models.len());
    for model in &project.models {
        let path = model_path(model);
        emit_directive_diagnostics(model, path.as_deref(), &mut diagnostics);
        let pinned_id = lower_pinned_id(model, path.as_deref(), &mut diagnostics);

        let relations = match parse_statements(&model.sql, Dialect::Generic) {
            Ok(statements) => extract_relations(&statements)
                .into_iter()
                .map(|relation| relation.name)
                .collect(),
            Err(error) => {
                let mut diagnostic = Diagnostic::error(
                    codes::PARSE_INVALID_SQL,
                    format!("could not parse SQL: {}", error.message()),
                );
                if let Some(path) = &path {
                    diagnostic = diagnostic.with_path(path.clone());
                }
                diagnostics.push(diagnostic);
                Vec::new()
            }
        };

        lowered.push(LoweredModel {
            model,
            path,
            pinned_id,
            relations,
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
    let entries: Vec<RegistryEntry> = unique
        .iter()
        .map(|model| RegistryEntry {
            id: model.model.id.clone(),
            namespace: model.model.namespace.clone(),
            path: model.model.path.clone(),
            root: model.model.root.clone(),
        })
        .collect();
    let resolver = Resolver::new(entries);

    // Resolve dependencies for each unique model.
    let mut compiled_models: Vec<CompiledModel> = Vec::with_capacity(unique.len());
    for entry in &unique {
        let registry_entry = RegistryEntry {
            id: entry.model.id.clone(),
            namespace: entry.model.namespace.clone(),
            path: entry.model.path.clone(),
            root: entry.model.root.clone(),
        };
        let mut dependencies: Vec<Dependency> = Vec::new();
        for relation in &entry.relations {
            match resolver.resolve(&registry_entry, relation) {
                Resolution::Model(id) => dependencies.push(Dependency::Model(id)),
                Resolution::External(source) => dependencies.push(Dependency::Source(source)),
                Resolution::Ambiguous(candidates) => {
                    diagnostics.push(ambiguous_diagnostic(entry, relation, &candidates));
                }
            }
        }
        dependencies.sort();
        dependencies.dedup();

        compiled_models.push(CompiledModel {
            id: entry.model.id.clone(),
            namespace: entry.model.namespace.clone(),
            path: entry.model.path.clone(),
            origin: entry.model.origin.clone(),
            sql: entry.model.sql.clone(),
            pinned_id: entry.pinned_id.clone(),
            dependencies,
        });
    }

    let mut compilation = Compilation::new(
        project.workspace_root.clone(),
        project.roots.clone(),
        compiled_models,
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
    relations: Vec<RelationName>,
}

fn model_path(model: &SemanticModel) -> Option<String> {
    model
        .origin
        .path
        .as_ref()
        .map(|path| path.to_string_lossy().replace('\\', "/"))
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
            DirectiveIssueKind::DuplicateId => (
                Severity::Error,
                codes::PARSE_MALFORMED_DIRECTIVE,
                format!("directive `{}` is declared more than once", issue.name),
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
    model: &LoweredModel<'_>,
    relation: &RelationName,
    candidates: &[ModelId],
) -> Diagnostic {
    let mut diagnostic = Diagnostic::error(
        codes::RESOLUTION_AMBIGUOUS,
        format!("ambiguous relation `{}`", relation.as_dotted()),
    );
    if let Some(path) = &model.path {
        diagnostic = diagnostic.with_path(path.clone());
    }
    diagnostic =
        diagnostic.with_labels(candidates.iter().map(|candidate| candidate.logical_name()));
    diagnostic.with_help("use a qualified model name")
}
