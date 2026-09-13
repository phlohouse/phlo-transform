//! The compiler: parse, resolve and build the dependency graph.
//!
//! Compilation is side-effect free and operates purely on the semantic
//! project representation. The native frontend is not required.

use std::collections::{BTreeMap, BTreeSet};

use phlo_transform_sql::{
    extract_relations, parse_statements, Dialect, DirectiveIssueKind, Materialization, RelationName,
};
use sqlparser::ast::{Query, Statement};

use crate::analyze::Analyzer;
use crate::compiled::{Compilation, CompiledModel, CompiledSeed, CompiledTest};
use crate::config::CrossWorkflowPolicy;
use crate::diagnostics::{codes, Diagnostic, Severity};
use crate::graph::Dependency;
use crate::identity::ModelId;
use crate::model::{
    FrontendKind, ModelOrigin, Relation, SemanticModel, SemanticProject, TestId, WorkspaceDefaults,
};
use crate::resolve::{RegistryEntry, Resolution, Resolver};
use crate::rewrite::rewrite_statements;
use crate::schema::{EmptySchemaProvider, SchemaProvider, SeedSchemaProvider};
use crate::semantic::{Assertion, ModelContract, ModelSchema, Nullability};
use crate::version::{
    model_version, EmptySourceStateProvider, ModelVersions, SourceStateProvider, VersionDetail,
    VersionInputs,
};

/// Compile a semantic project into models and a dependency graph.
pub fn compile(project: &SemanticProject) -> Compilation {
    compile_with_options(project, &EmptySchemaProvider, &EmptySourceStateProvider)
}

/// Compile a semantic project, using a schema provider for external sources.
pub fn compile_with_provider(
    project: &SemanticProject,
    provider: &dyn SchemaProvider,
) -> Compilation {
    compile_with_options(project, provider, &EmptySourceStateProvider)
}

/// Compile a semantic project with schema and source-state providers.
pub fn compile_with_options(
    project: &SemanticProject,
    provider: &dyn SchemaProvider,
    source_states: &dyn SourceStateProvider,
) -> Compilation {
    // Seeds contribute their CSV header columns as varchar schemas when the
    // catalogue does not know the source relation.
    let provider = SeedSchemaProvider::new(provider, &project.seeds);
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
    let mut seen: std::collections::HashMap<&ModelId, usize> =
        std::collections::HashMap::with_capacity(lowered.len());
    let mut duplicate_group: std::collections::HashMap<&ModelId, usize> =
        std::collections::HashMap::new();
    let mut duplicates: Vec<Vec<&LoweredModel>> = Vec::new();
    for lowered_model in &lowered {
        if let Some(&first) = seen.get(&lowered_model.model.id) {
            if let Some(&group_index) = duplicate_group.get(&lowered_model.model.id) {
                duplicates[group_index].push(lowered_model);
            } else {
                duplicate_group.insert(&lowered_model.model.id, duplicates.len());
                duplicates.push(vec![unique[first], lowered_model]);
            }
        } else {
            seen.insert(&lowered_model.model.id, unique.len());
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

    // Resolve dependencies for every model first: ephemeral expansion needs
    // each model's graph edges regardless of iteration order.
    let workflows: BTreeMap<ModelId, Option<String>> = unique
        .iter()
        .map(|entry| (entry.model.id.clone(), entry.model.workflow.clone()))
        .collect();
    let lowered_by_id: std::collections::HashMap<&ModelId, &LoweredModel> = unique
        .iter()
        .map(|entry| (&entry.model.id, *entry))
        .collect();

    let mut dependencies_by_id: BTreeMap<ModelId, Vec<Dependency>> = BTreeMap::new();
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

        // Cross-workflow dependency policy.
        if let Some(current_workflow) = &entry.model.workflow {
            for dependency in &dependencies {
                if let Dependency::Model(dependency_id) = dependency {
                    if let Some(Some(dependency_workflow)) = workflows.get(dependency_id) {
                        if dependency_workflow != current_workflow {
                            let message = format!(
                                "model `{}` in workflow `{current_workflow}` depends on `{}` in workflow `{dependency_workflow}`",
                                entry.model.id.logical_name(),
                                dependency_id.logical_name()
                            );
                            match project.cross_workflow {
                                CrossWorkflowPolicy::Allow => {}
                                CrossWorkflowPolicy::Warn => diagnostics.push(
                                    Diagnostic::warning(
                                        codes::DEPENDENCIES_CROSS_WORKFLOW,
                                        message,
                                    )
                                    .with_path(entry.path.clone().unwrap_or_default()),
                                ),
                                CrossWorkflowPolicy::Error => diagnostics.push(
                                    Diagnostic::error(codes::DEPENDENCIES_CROSS_WORKFLOW, message)
                                        .with_path(entry.path.clone().unwrap_or_default()),
                                ),
                            }
                        }
                    }
                }
            }
        }
        dependencies_by_id.insert(entry.model.id.clone(), dependencies);
    }

    let mut expansion = Expansion {
        lowered: lowered_by_id.clone(),
        resolver: &resolver,
        targets: &targets,
        dependencies: &dependencies_by_id,
        memo: BTreeMap::new(),
        visiting: BTreeSet::new(),
    };

    let mut compiled_models: Vec<CompiledModel> = Vec::with_capacity(unique.len());
    for entry in &unique {
        let registry_entry = entry_for(entry);
        let dependencies = dependencies_by_id[&entry.model.id].clone();

        // References to ephemeral models are inlined as derived tables whose
        // own ephemeral dependencies are already expanded.
        let mut ephemerals: BTreeMap<ModelId, Query> = BTreeMap::new();
        for dependency in &dependencies {
            if let Dependency::Model(dependency_id) = dependency {
                if expansion.is_ephemeral(dependency_id) {
                    if let Some(query) = expansion.expand(dependency_id, &mut diagnostics) {
                        ephemerals.insert(dependency_id.clone(), query);
                    }
                }
            }
        }

        let mut statements = entry.statements.clone();
        let compiled_sql = rewrite_statements(
            &mut statements,
            Some(&registry_entry),
            &resolver,
            &targets,
            &ephemerals,
        );

        compiled_models.push(CompiledModel {
            id: entry.model.id.clone(),
            namespace: entry.model.namespace.clone(),
            path: entry.model.path.clone(),
            origin: entry.model.origin.clone(),
            sql: entry.model.sql.clone(),
            workflow: entry.model.workflow.clone(),
            config: entry.model.config.clone(),
            target: targets[&entry.model.id].clone(),
            compiled_sql,
            schema: ModelSchema::default(),
            limitations: Vec::new(),
            assertions: Vec::new(),
            contract: entry.model.contract.clone(),
            version: Default::default(),
            version_detail: Default::default(),
            pinned_id: entry.pinned_id.clone(),
            dependencies,
        });
    }

    let compiled_tests = compile_tests(
        project,
        &resolver,
        &targets,
        &mut expansion,
        &mut diagnostics,
    );

    let compiled_seeds: Vec<CompiledSeed> = project
        .seeds
        .iter()
        .map(|seed| CompiledSeed {
            name: seed.name.clone(),
            path: seed.path.clone(),
            schema: seed
                .schema
                .clone()
                .or_else(|| project.defaults.schema.clone()),
            content_hash: seed.content_hash.clone(),
            columns: seed.columns.clone(),
        })
        .collect();

    let mut compilation = Compilation::new(
        project.workspace_root.clone(),
        project.roots.clone(),
        compiled_models,
        compiled_tests,
        compiled_seeds,
        project.defaults.clone(),
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

    // Type, column and lineage analysis in dependency order. A cyclic graph is
    // already an error, so analysis is skipped in that case.
    if let Some(order) = compilation.topological_order() {
        let mut model_schemas: BTreeMap<ModelId, ModelSchema> = BTreeMap::new();
        let mut model_versions: ModelVersions = BTreeMap::new();
        let mut generated_tests: Vec<CompiledTest> = Vec::new();
        for id in order {
            let Some(lowered) = lowered_by_id.get(&id).copied() else {
                continue;
            };
            let entry = entry_for(lowered);
            let analyzer = Analyzer::new(&resolver, &model_schemas, &provider);
            let analysis = analyzer.analyze(&entry, &lowered.statements, &id);
            let assertions = assertions_for(lowered.model);

            if let Some(contract) = &lowered.model.contract {
                validate_contract(
                    &id,
                    &analysis.schema,
                    contract,
                    &mut compilation.diagnostics,
                );
            }
            // Assertions on ephemeral models test the expanded query — the
            // model is never materialised, so its physical target does not
            // exist. Expansion failures already raised diagnostics above.
            let (is_ephemeral, target_sql) = compilation
                .model(&id)
                .map(|model| {
                    (
                        model.config.materialization == Materialization::Ephemeral,
                        model.target.sql(),
                    )
                })
                .unwrap_or((false, String::new()));
            let subject = if is_ephemeral {
                expansion
                    .expand(&id, &mut compilation.diagnostics)
                    .map(|query| format!("({query}) AS {}", quote_ident(id.last_segment())))
            } else {
                Some(target_sql)
            };
            if let Some(subject) = subject {
                generated_tests.extend(generate_tests(&id, &assertions, &subject));
            }

            // Compute the content-addressed version from upstream versions.
            let inputs = match compilation.model(&id) {
                Some(model) => version_inputs(lowered, model, source_states, &model_versions),
                None => VersionInputs::default(),
            };
            let detail = VersionDetail {
                dependencies: inputs.dependencies.iter().cloned().collect(),
                sources: inputs.sources.iter().cloned().collect(),
            };
            let version = model_version(&inputs);
            model_versions.insert(id.clone(), version.clone());

            if let Some(position) = compilation.model_position(&id) {
                compilation.models[position].schema = analysis.schema.clone();
                compilation.models[position].limitations = analysis.limitations;
                compilation.models[position].assertions = assertions;
                compilation.models[position].version = version;
                compilation.models[position].version_detail = detail;
            }
            compilation.diagnostics.extend(analysis.diagnostics);
            model_schemas.insert(id, analysis.schema);
        }
        compilation.add_generated_tests(generated_tests);
    }

    // The canonical lineage graph is built once, after analysis has filled
    // in schemas, versions and generated tests. Every downstream consumer —
    // reports, planning, OpenLineage — reads this structure rather than
    // re-deriving lineage.
    compilation.lineage = crate::lineage::LineageGraph::build(&compilation);

    compilation
}

/// Assemble the version inputs for a model.
fn version_inputs(
    lowered: &LoweredModel<'_>,
    model: &CompiledModel,
    source_states: &dyn SourceStateProvider,
    model_versions: &ModelVersions,
) -> VersionInputs {
    let canonical_sql = lowered
        .statements
        .iter()
        .map(|statement| statement.to_string())
        .collect::<Vec<_>>()
        .join(";\n");

    // Only semantics-affecting configuration participates; tags and owner do
    // not change the physical output.
    let config = format!(
        "materialization={};schema={};incremental={:?}",
        model.config.materialization,
        model.config.schema.clone().unwrap_or_default(),
        model.config.incremental
    );

    let contract = model
        .contract
        .as_ref()
        .map(|contract| {
            let columns: Vec<String> = contract
                .columns
                .iter()
                .map(|column| {
                    format!(
                        "{}:{:?}:{:?}",
                        column.name, column.data_type, column.nullable
                    )
                })
                .collect();
            let renames: Vec<String> = contract
                .renames
                .iter()
                .map(|(new, old)| format!("{new}={old}"))
                .collect();
            format!(
                "enforced={};columns={};renames={}",
                contract.enforced,
                columns.join(","),
                renames.join(",")
            )
        })
        .unwrap_or_default();
    let assertions: Vec<String> = model
        .assertions
        .iter()
        .map(|assertion| assertion.describe())
        .collect();
    let contract = format!("{contract};assertions={}", assertions.join(","));

    let dependencies = model
        .model_dependencies()
        .map(|dependency| {
            (
                dependency.logical_name(),
                model_versions
                    .get(dependency)
                    .map(|version| version.hash.clone())
                    .unwrap_or_default(),
            )
        })
        .collect();

    let sources = model
        .source_dependencies()
        .map(|source| {
            (
                source.logical_name(),
                source_states.source_state(source).unwrap_or_default(),
            )
        })
        .collect();

    let target = format!(
        "{}|{}",
        model.target.display(),
        model.config.materialization
    );

    VersionInputs {
        canonical_sql,
        config,
        contract,
        dependencies,
        sources,
        target,
    }
}

/// Derive logical assertions from model directives.
fn assertions_for(model: &SemanticModel) -> Vec<Assertion> {
    let mut assertions = Vec::new();
    for column in &model.directives.keys {
        assertions.push(Assertion::NotNull {
            column: column.clone(),
        });
        assertions.push(Assertion::Unique {
            columns: vec![column.clone()],
        });
    }
    for column in &model.directives.not_null {
        let assertion = Assertion::NotNull {
            column: column.clone(),
        };
        if !assertions.contains(&assertion) {
            assertions.push(assertion);
        }
    }
    assertions
}

/// Validate an explicit contract against an inferred schema.
fn validate_contract(
    id: &ModelId,
    schema: &ModelSchema,
    contract: &ModelContract,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let severity = if contract.enforced {
        Severity::Error
    } else {
        Severity::Warning
    };
    if !schema.known {
        diagnostics.push(
            Diagnostic::warning(
                codes::TYPE_CONTRACT,
                "contract could not be fully validated because the schema is unknown",
            )
            .with_path(id.logical_name()),
        );
    }

    let mut push = |message: String| {
        let diagnostic = match severity {
            Severity::Error => Diagnostic::error(codes::TYPE_CONTRACT, message),
            _ => Diagnostic::warning(codes::TYPE_CONTRACT, message),
        };
        diagnostics.push(diagnostic.with_path(id.logical_name()));
    };

    for column in &contract.columns {
        match schema.column(&column.name) {
            None => {
                if schema.known {
                    push(format!("contract column `{}` is missing", column.name));
                }
            }
            Some(output) => {
                if let Some(expected) = &column.data_type {
                    if output.data_type.is_known() && &output.data_type != expected {
                        push(format!(
                            "column `{}` has type {} but contract expects {}",
                            column.name, output.data_type, expected
                        ));
                    }
                }
                if column.nullable == Some(false) && output.nullability == Nullability::Nullable {
                    push(format!(
                        "column `{}` is nullable but the contract requires not null",
                        column.name
                    ));
                }
            }
        }
    }
}

/// Generate logical runtime tests from assertions. `subject` is the FROM
/// target: the physical relation for materialised models, or the expanded
/// derived table for ephemeral ones.
fn generate_tests(id: &ModelId, assertions: &[Assertion], subject: &str) -> Vec<CompiledTest> {
    assertions
        .iter()
        .map(|assertion| {
            let slug = assertion_slug(assertion);
            let compiled_sql = assertion_sql(assertion, subject);
            CompiledTest {
                id: TestId::new(format!("{}.generated.{slug}", id.logical_name())),
                origin: ModelOrigin {
                    frontend: FrontendKind::InMemory,
                    path: None,
                },
                sql: compiled_sql.clone(),
                compiled_sql,
                targets: vec![id.clone()],
                sources: Vec::new(),
                generated: true,
            }
        })
        .collect()
}

fn assertion_slug(assertion: &Assertion) -> String {
    match assertion {
        Assertion::NotNull { column } => format!("not_null_{column}"),
        Assertion::Unique { columns } => format!("unique_{}", columns.join("_")),
    }
}

fn assertion_sql(assertion: &Assertion, subject: &str) -> String {
    match assertion {
        Assertion::NotNull { column } => format!(
            "select * from {} where {} is null",
            subject,
            quote_ident(column)
        ),
        Assertion::Unique { columns } => {
            let quoted: Vec<String> = columns.iter().map(|column| quote_ident(column)).collect();
            format!(
                "select {}, count(*) as __phlo_count from {} group by {} having count(*) > 1",
                quoted.join(", "),
                subject,
                quoted.join(", ")
            )
        }
    }
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

struct LoweredModel<'a> {
    model: &'a SemanticModel,
    path: Option<String>,
    pinned_id: Option<ModelId>,
    statements: Vec<Statement>,
}

/// Memoised expansion of ephemeral models into single queries with their own
/// ephemeral dependencies already inlined.
struct Expansion<'a> {
    lowered: std::collections::HashMap<&'a ModelId, &'a LoweredModel<'a>>,
    resolver: &'a Resolver,
    targets: &'a BTreeMap<ModelId, Relation>,
    dependencies: &'a BTreeMap<ModelId, Vec<Dependency>>,
    memo: BTreeMap<ModelId, Option<Query>>,
    visiting: BTreeSet<ModelId>,
}

impl<'a> Expansion<'a> {
    fn is_ephemeral(&self, id: &ModelId) -> bool {
        self.lowered
            .get(id)
            .map(|lowered| lowered.model.config.materialization == Materialization::Ephemeral)
            .unwrap_or(false)
    }

    /// Compile an ephemeral model to its expanded query, or `None` when it
    /// cannot be inlined (not a single query, or part of a dependency cycle).
    /// Failures leave the reference rewritten to a physical target, but the
    /// error diagnostics they raise block execution.
    fn expand(&mut self, id: &ModelId, diagnostics: &mut Vec<Diagnostic>) -> Option<Query> {
        if let Some(cached) = self.memo.get(id) {
            return cached.clone();
        }
        let lowered = self.lowered.get(id).copied()?;
        if !self.visiting.insert(id.clone()) {
            // A cycle here is also reported by the graph cycle check.
            return None;
        }
        let registry_entry = entry_for(lowered);
        let mut statements = lowered.statements.clone();

        let nested: Vec<ModelId> = self
            .dependencies
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|dependency| match dependency {
                Dependency::Model(dep_id) => Some(dep_id.clone()),
                Dependency::Source(_) => None,
            })
            .filter(|dep_id| self.is_ephemeral(dep_id))
            .collect();
        let mut ephemerals: BTreeMap<ModelId, Query> = BTreeMap::new();
        for dep_id in nested {
            if let Some(query) = self.expand(&dep_id, diagnostics) {
                ephemerals.insert(dep_id, query);
            }
        }

        rewrite_statements(
            &mut statements,
            Some(&registry_entry),
            self.resolver,
            self.targets,
            &ephemerals,
        );
        self.visiting.remove(id);

        let expanded = match statements.as_slice() {
            [Statement::Query(query)] => Some((**query).clone()),
            _ => {
                let mut diagnostic = Diagnostic::error(
                    codes::MODEL_EPHEMERAL,
                    format!(
                        "ephemeral model `{}` must be a single query to be inlined",
                        id.logical_name()
                    ),
                );
                if let Some(path) = &lowered.path {
                    diagnostic.labels.push(path.clone());
                }
                diagnostics.push(diagnostic);
                None
            }
        };
        self.memo.insert(id.clone(), expanded.clone());
        expanded
    }
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
    expansion: &mut Expansion<'_>,
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

        let mut ephemerals: BTreeMap<ModelId, Query> = BTreeMap::new();
        for id in &model_targets {
            if expansion.is_ephemeral(id) {
                if let Some(query) = expansion.expand(id, diagnostics) {
                    ephemerals.insert(id.clone(), query);
                }
            }
        }
        let compiled_sql =
            rewrite_statements(&mut statements, None, resolver, targets, &ephemerals);

        tests.push(CompiledTest {
            id: test.id.clone(),
            origin: test.origin.clone(),
            sql: test.sql.clone(),
            compiled_sql,
            targets: model_targets.into_iter().collect(),
            sources: sources.into_iter().collect(),
            generated: false,
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
    let (message, location) = split_parser_location(error.message());
    let mut diagnostic =
        Diagnostic::error(codes::PARSE_INVALID_SQL, format!("{context}: {message}"));
    if let Some(path) = path {
        diagnostic = diagnostic.with_path(path.to_string());
    }
    if let Some((line, column)) = location {
        diagnostic = diagnostic.with_location(line, column);
    }
    diagnostic
}

/// sqlparser formats positions as a trailing ` at Line: N, Column: M`. Split
/// that tail into a structured location so CLI output can render
/// `path:line:column` for editor navigation.
fn split_parser_location(message: &str) -> (String, Option<(usize, usize)>) {
    let Some(line_marker) = message.rfind("Line: ") else {
        return (message.to_string(), None);
    };
    let tail = &message[line_marker + "Line: ".len()..];
    let Some(comma) = tail.find(',') else {
        return (message.to_string(), None);
    };
    let Ok(line) = tail[..comma].trim().parse::<usize>() else {
        return (message.to_string(), None);
    };
    let Some(column) = tail[comma + 1..].strip_prefix(" Column: ") else {
        return (message.to_string(), None);
    };
    let digits: String = column
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    let Ok(column) = digits.parse::<usize>() else {
        return (message.to_string(), None);
    };
    // Only strip when the location is the message tail (optionally preceded
    // by ` at` / `at`), not a position mentioned mid-message.
    let prefix = message[..line_marker].trim_end();
    let stripped = prefix.strip_suffix(" at").unwrap_or(prefix);
    (stripped.trim_end().to_string(), Some((line, column)))
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
