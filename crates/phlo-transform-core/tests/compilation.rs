//! Integration tests over fixture workspaces.
//!
//! Tests run with the crate root as the working directory, so fixtures are
//! referenced relative to the workspace. This keeps reported paths stable for
//! snapshot tests.

use std::path::PathBuf;

use phlo_transform_core::{compile, load_project, Dependency, IdentityError, ModelId};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from("../../fixtures").join(name)
}

fn compile_fixture(name: &str) -> phlo_transform_core::Compilation {
    let project = load_project(&fixture(name)).expect("workspace should load");
    compile(&project)
}

fn model_names(compilation: &phlo_transform_core::Compilation) -> Vec<String> {
    compilation
        .models
        .iter()
        .map(|model| model.id.logical_name())
        .collect()
}

fn dependency_names(compilation: &phlo_transform_core::Compilation, model: &str) -> Vec<String> {
    let id = ModelId::parse(model).unwrap();
    compilation
        .dependencies(&id)
        .into_iter()
        .map(|dependency| match dependency {
            Dependency::Model(id) => id.logical_name(),
            Dependency::Source(id) => id.logical_name(),
        })
        .collect()
}

fn codes(compilation: &phlo_transform_core::Compilation) -> Vec<String> {
    compilation
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.clone())
        .collect()
}

#[test]
fn discovers_multi_root_workspace_without_ref() {
    let compilation = compile_fixture("basic-multi-root");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["assay.raw", "assay.results", "reporting.monthly"]
    );
    assert_eq!(
        dependency_names(&compilation, "assay.raw"),
        vec!["external.raw_assay_results"]
    );
    assert_eq!(
        dependency_names(&compilation, "assay.results"),
        vec!["assay.raw"]
    );
    assert_eq!(
        dependency_names(&compilation, "reporting.monthly"),
        vec!["assay.results"]
    );
}

#[test]
fn topological_order_follows_discovered_edges() {
    let compilation = compile_fixture("basic-multi-root");
    let order: Vec<String> = compilation
        .topological_order()
        .expect("acyclic")
        .into_iter()
        .map(|id| id.logical_name())
        .collect();
    assert_eq!(
        order,
        vec!["assay.raw", "assay.results", "reporting.monthly"]
    );
}

#[test]
fn global_and_workflow_roots_share_one_graph() {
    let compilation = compile_fixture("global-and-workflow");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["assay.raw", "assay.results", "shared.dimensions.date"]
    );
    assert_eq!(
        dependency_names(&compilation, "assay.results"),
        vec!["assay.raw", "shared.dimensions.date"]
    );
}

#[test]
fn global_transforms_root_derives_first_segment_namespace() {
    let compilation = compile_fixture("global-only");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["assay.results", "shared.reference.sites"]
    );
    assert_eq!(
        dependency_names(&compilation, "assay.results"),
        vec!["shared.reference.sites"]
    );
}

#[test]
fn cross_workflow_references_are_ordinary_edges() {
    let compilation = compile_fixture("cross-workflow");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        dependency_names(&compilation, "analytics.monthly"),
        vec!["manufacturing.batches"]
    );
}

#[test]
fn configured_custom_roots_and_excludes_are_honoured() {
    let compilation = compile_fixture("custom-roots");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["assay.raw", "assay.results"]
    );
    // The excluded file must not appear anywhere in the graph.
    assert!(compilation
        .sources()
        .iter()
        .all(|source| !source.logical_name().contains("should_not_be_discovered")));
}

#[test]
fn pinned_id_overrides_physical_location() {
    let compilation = compile_fixture("pinned-id");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["assay.raw", "assay.results"]
    );
    assert_eq!(
        dependency_names(&compilation, "assay.results"),
        vec!["assay.raw"]
    );
}

#[test]
fn aliases_and_nested_queries_do_not_create_false_dependencies() {
    let aliases = compile_fixture("aliases");
    assert!(aliases.is_ok(), "{:?}", aliases.diagnostics);
    assert_eq!(
        dependency_names(&aliases, "assay.results"),
        vec!["external.results", "external.samples"]
    );

    let nested = compile_fixture("nested-queries");
    assert!(nested.is_ok(), "{:?}", nested.diagnostics);
    assert_eq!(
        dependency_names(&nested, "assay.results"),
        vec!["external.inner_results", "external.lookup"]
    );
}

#[test]
fn cte_shadowing_a_workspace_model_is_not_a_dependency() {
    let compilation = compile_fixture("cte-shadowing");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        dependency_names(&compilation, "assay.results"),
        vec!["external.local_raw"]
    );
}

#[test]
fn ambiguous_relation_fails_with_candidates() {
    let compilation = compile_fixture("ambiguous");
    assert!(!compilation.is_ok());
    let diagnostic = compilation
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == "RESOLUTION001")
        .expect("ambiguity diagnostic");
    assert!(diagnostic.message.contains("ambiguous relation `results`"));
    assert_eq!(diagnostic.labels.len(), 2);
}

#[test]
fn cycles_fail_with_concrete_paths() {
    for (name, expected_len) in [("cycle-two", 3), ("cycle-three", 4)] {
        let compilation = compile_fixture(name);
        assert!(!compilation.is_ok(), "{name} should fail");
        let diagnostic = compilation
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == "GRAPH001")
            .unwrap_or_else(|| panic!("{name} missing cycle diagnostic"));
        let cycle = &diagnostic.labels[0];
        let nodes: Vec<&str> = cycle.split(" -> ").collect();
        assert_eq!(nodes.len(), expected_len, "{name}: {cycle}");
        assert_eq!(nodes.first(), nodes.last(), "{name}: {cycle}");
    }
}

#[test]
fn self_reference_is_a_cycle() {
    let compilation = compile_fixture("self-reference");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"GRAPH001".to_string()));
}

#[test]
fn invalid_sql_reports_a_parse_error() {
    let compilation = compile_fixture("invalid-sql");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"PARSE001".to_string()));
}

#[test]
fn malformed_directive_reports_a_metadata_error() {
    let compilation = compile_fixture("malformed-directive");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"PARSE002".to_string()));
}

#[test]
fn unknown_directive_is_only_a_warning() {
    let compilation = compile_fixture("unknown-directive");
    assert!(compilation.is_ok());
    assert!(codes(&compilation).contains(&"PARSE003".to_string()));
}

#[test]
fn duplicate_model_id_is_reported() {
    let compilation = compile_fixture("duplicate-model-id");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"PROJECT002".to_string()));
}

#[test]
fn duplicate_pinned_id_is_reported() {
    let compilation = compile_fixture("duplicate-pinned-id");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"PROJECT002".to_string()));
}

#[test]
fn duplicate_namespace_is_reported() {
    let compilation = compile_fixture("duplicate-namespace");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"PROJECT003".to_string()));
}

#[test]
fn unresolved_relations_are_registered_as_sources() {
    let compilation = compile_fixture("basic-multi-root");
    let sources: Vec<String> = compilation
        .sources()
        .into_iter()
        .map(|source| source.logical_name())
        .collect();
    assert_eq!(sources, vec!["external.raw_assay_results"]);
}

#[test]
fn trino_syntax_workspace_compiles() {
    let compilation = compile_fixture("trino-syntax");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["trino.items", "trino.marts", "trino.raw"]
    );
    assert_eq!(
        dependency_names(&compilation, "trino.raw"),
        vec!["tpch.tiny.orders"]
    );
    assert_eq!(
        dependency_names(&compilation, "trino.items"),
        vec!["trino.raw"]
    );
    assert_eq!(
        dependency_names(&compilation, "trino.marts"),
        vec!["trino.items"]
    );
    let order: Vec<String> = compilation
        .topological_order()
        .expect("acyclic")
        .into_iter()
        .map(|id| id.logical_name())
        .collect();
    assert_eq!(order, vec!["trino.raw", "trino.items", "trino.marts"]);
}

/// Architecture test: the resolver and DAG are usable without filesystem
/// discovery, proving the semantic core is not coupled to the native
/// frontend.
#[test]
fn compiles_from_an_in_memory_semantic_project() {
    use phlo_transform_core::SemanticModel;
    use phlo_transform_core::SemanticProject;

    let raw = SemanticModel::in_memory(
        ModelId::parse("assay.raw").unwrap(),
        "select * from external.raw_assay_results",
    );
    let results = SemanticModel::in_memory(
        ModelId::parse("assay.results").unwrap(),
        "select * from assay.raw",
    );
    let monthly = SemanticModel::in_memory(
        ModelId::parse("reporting.monthly").unwrap(),
        "select * from assay.results",
    );

    let project = SemanticProject::in_memory(vec![monthly, raw, results]);
    let compilation = compile(&project);

    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        model_names(&compilation),
        vec!["assay.raw", "assay.results", "reporting.monthly"]
    );
    assert_eq!(
        dependency_names(&compilation, "reporting.monthly"),
        vec!["assay.results"]
    );
}

#[test]
fn identity_helpers_reject_malformed_ids() {
    assert_eq!(
        ModelId::parse("assay"),
        Err(IdentityError::MissingPath("assay".to_string()))
    );
}

#[test]
fn materialization_and_metadata_follow_configured_precedence() {
    use phlo_transform_core::Materialization;

    let compilation = compile_fixture("materialization");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let config = |name: &str| {
        compilation
            .model(&ModelId::parse(name).unwrap())
            .unwrap()
            .config
            .clone()
    };

    // Root default (table) with root owner and tags.
    let staging = config("assay.staging.raw");
    assert_eq!(staging.materialization, Materialization::Table);
    assert_eq!(staging.tags, vec!["assay"]);
    assert_eq!(staging.owner.as_deref(), Some("assay-team"));
    assert_eq!(staging.schema, None);

    // Folder override: view, extra tag, folder schema.
    let results = config("assay.marts.results");
    assert_eq!(results.materialization, Materialization::View);
    assert_eq!(results.tags, vec!["assay", "gold"]);
    assert_eq!(results.schema.as_deref(), Some("marts"));

    // Model directive beats the folder default.
    let direct = config("assay.marts.direct");
    assert_eq!(direct.materialization, Materialization::Table);
}

#[test]
fn physical_targets_reflect_workspace_defaults() {
    let compilation = compile_fixture("materialization");
    let staging = compilation
        .model(&ModelId::parse("assay.staging.raw").unwrap())
        .unwrap();
    assert_eq!(staging.target.catalog.as_deref(), Some("memory"));
    assert_eq!(staging.target.schema, "analytics");
    assert_eq!(staging.target.table, "assay__staging__raw");

    // A folder schema override changes the target schema.
    let results = compilation
        .model(&ModelId::parse("assay.marts.results").unwrap())
        .unwrap();
    assert_eq!(results.target.schema, "marts");
}

#[test]
fn compiled_sql_rewrites_models_and_preserves_sources() {
    let compilation = compile_fixture("compiled-sql");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let results = compilation
        .model(&ModelId::parse("assay.results").unwrap())
        .unwrap();
    // External sources are left alone.
    assert!(results.compiled_sql.contains("external.local_raw"));
    // Workspace relations become physical targets.
    assert!(
        results.compiled_sql.contains("memory.analytics.assay__raw"),
        "{}",
        results.compiled_sql
    );
    assert_eq!(
        results.compiled_sql.matches("assay__raw").count(),
        1,
        "{}",
        results.compiled_sql
    );
    // The CTE named `raw` is not rewritten.
    assert!(
        results.compiled_sql.contains("FROM raw"),
        "{}",
        results.compiled_sql
    );
}

#[test]
fn discovers_and_compiles_custom_tests() {
    let compilation = compile_fixture("tests-suite");
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let names: Vec<String> = compilation
        .tests
        .iter()
        .map(|test| test.id.to_string())
        .collect();
    assert_eq!(names, vec!["raw_not_null", "results_positive"]);

    let test = compilation
        .tests
        .iter()
        .find(|test| test.id.to_string() == "results_positive")
        .unwrap();
    assert_eq!(test.targets, vec![ModelId::parse("assay.results").unwrap()]);
    assert!(
        test.compiled_sql.contains("assay.results"),
        "{}",
        test.compiled_sql
    );
    assert_eq!(
        compilation
            .tests_for(&ModelId::parse("assay.results").unwrap())
            .len(),
        1
    );
}

#[test]
fn colliding_physical_targets_are_reported() {
    let compilation = compile_fixture("target-collision");
    assert!(!compilation.is_ok());
    assert!(codes(&compilation).contains(&"PROJECT007".to_string()));
}
