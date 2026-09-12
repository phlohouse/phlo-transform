//! Type, column-resolution and lineage tests using a static schema provider.

use std::collections::BTreeSet;

use phlo_transform_core::{
    compile_with_provider, ColumnContract, ColumnRef, Compilation, DataType, ModelContract,
    ModelId, Nullability, RelationSchema, SchemaColumn, SemanticModel, SemanticProject, SourceId,
    StaticSchemaProvider,
};

fn column(name: &str, data_type: DataType, nullability: Nullability) -> SchemaColumn {
    SchemaColumn {
        name: name.to_string(),
        data_type,
        nullability,
    }
}

fn provider() -> StaticSchemaProvider {
    let mut provider = StaticSchemaProvider::new();
    provider.insert(
        "external.raw_results",
        RelationSchema::new(vec![
            column("experiment_id", DataType::Varchar, Nullability::NotNull),
            column("sample_id", DataType::Varchar, Nullability::NotNull),
            column("signal", DataType::Double, Nullability::Nullable),
        ]),
    );
    provider.insert(
        "external.samples",
        RelationSchema::new(vec![
            column("sample_id", DataType::Varchar, Nullability::NotNull),
            column("volume", DataType::Double, Nullability::Nullable),
        ]),
    );
    provider
}

fn model(name: &str, sql: &str) -> SemanticModel {
    SemanticModel::in_memory(ModelId::parse(name).unwrap(), sql)
}

fn compile(models: Vec<SemanticModel>) -> Compilation {
    let project = SemanticProject::in_memory(models);
    compile_with_provider(&project, &provider())
}

fn id(name: &str) -> ModelId {
    ModelId::parse(name).unwrap()
}

fn column_names(compilation: &Compilation, model: &str) -> Vec<String> {
    compilation
        .model(&id(model))
        .unwrap()
        .schema
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect()
}

fn source(parts: &[&str], column: &str) -> ColumnRef {
    ColumnRef::source(
        SourceId::new(parts.iter().map(|part| part.to_string()).collect()).unwrap(),
        column,
    )
}

fn input_names(compilation: &Compilation, model: &str, column_name: &str) -> BTreeSet<String> {
    compilation
        .model(&id(model))
        .unwrap()
        .schema
        .column(column_name)
        .unwrap()
        .inputs
        .iter()
        .map(ColumnRef::display)
        .collect()
}

#[test]
fn expands_star_and_tracks_source_lineage() {
    let compilation = compile(vec![model(
        "assay.raw_results",
        "select * from external.raw_results",
    )]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    let raw = compilation.model(&id("assay.raw_results")).unwrap();
    assert!(raw.schema.known);
    assert_eq!(
        column_names(&compilation, "assay.raw_results"),
        vec!["experiment_id", "sample_id", "signal"]
    );
    assert_eq!(
        raw.schema.column("signal").unwrap().data_type,
        DataType::Double
    );
    assert_eq!(
        raw.schema.column("signal").unwrap().inputs,
        vec![source(&["external", "raw_results"], "signal")]
    );
}

#[test]
fn infers_expression_types_and_expression_lineage() {
    let compilation = compile(vec![
        model("assay.raw_results", "select * from external.raw_results"),
        model(
            "assay.results",
            "select r.sample_id, r.signal / s.volume as concentration \
             from assay.raw_results r \
             join external.samples s using (sample_id)",
        ),
    ]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    assert_eq!(
        column_names(&compilation, "assay.results"),
        vec!["sample_id", "concentration"]
    );
    let results = compilation.model(&id("assay.results")).unwrap();
    assert!(results.schema.known);
    assert_eq!(
        results.schema.column("concentration").unwrap().data_type,
        DataType::Double
    );
    assert_eq!(
        input_names(&compilation, "assay.results", "concentration"),
        BTreeSet::from([
            "assay.raw_results.signal".to_string(),
            "external.samples.volume".to_string(),
        ])
    );
}

#[test]
fn expands_star_through_ctes() {
    let compilation = compile(vec![model(
        "assay.results",
        "with base as (select sample_id, signal from external.raw_results) \
         select * from base",
    )]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        column_names(&compilation, "assay.results"),
        vec!["sample_id", "signal"]
    );
    assert_eq!(
        input_names(&compilation, "assay.results", "signal"),
        BTreeSet::from(["external.raw_results.signal".to_string()])
    );
}

#[test]
fn combines_union_branches() {
    let compilation = compile(vec![model(
        "assay.results",
        "select sample_id from external.raw_results \
         union all select sample_id from external.samples",
    )]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    assert_eq!(
        column_names(&compilation, "assay.results"),
        vec!["sample_id"]
    );
    assert_eq!(
        input_names(&compilation, "assay.results", "sample_id"),
        BTreeSet::from([
            "external.raw_results.sample_id".to_string(),
            "external.samples.sample_id".to_string(),
        ])
    );
}

#[test]
fn infers_case_and_coalesce() {
    let compilation = compile(vec![model(
        "assay.results",
        "select coalesce(volume, 0.0) as v, \
         case when volume is null then 0.0 else volume end as w \
         from external.samples",
    )]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    let results = compilation.model(&id("assay.results")).unwrap();
    assert_eq!(
        results.schema.column("v").unwrap().data_type,
        DataType::Double
    );
    assert_eq!(
        results.schema.column("w").unwrap().data_type,
        DataType::Double
    );
}

#[test]
fn unknown_column_is_an_error_when_schemas_are_known() {
    let compilation = compile(vec![model(
        "assay.results",
        "select r.nonexistent from external.raw_results r",
    )]);
    assert!(!compilation.is_ok());
    let codes: Vec<&str> = compilation
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect();
    assert!(codes.contains(&"TYPE001"), "{:?}", compilation.diagnostics);
}

#[test]
fn ambiguous_column_is_an_error() {
    let compilation = compile(vec![model(
        "assay.results",
        "select sample_id from external.raw_results a \
         join external.samples b on a.sample_id = b.sample_id",
    )]);
    assert!(!compilation.is_ok());
    let codes: Vec<&str> = compilation
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.code.as_str())
        .collect();
    assert!(codes.contains(&"TYPE002"), "{:?}", compilation.diagnostics);
}

#[test]
fn unknown_source_schema_suppresses_false_column_errors() {
    // No provider: the source schema is unknown, so unresolved columns are
    // limitations, not errors.
    let project = SemanticProject::in_memory(vec![model(
        "assay.results",
        "select r.sample_id from external.raw_results r",
    )]);
    let compilation = phlo_transform_core::compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    let results = compilation.model(&id("assay.results")).unwrap();
    assert!(!results.schema.known);
    assert!(!results.limitations.is_empty());
}

#[test]
fn key_and_not_null_directives_become_assertions() {
    let mut raw = model("assay.raw_results", "select * from external.raw_results");
    raw.directives.keys = vec!["sample_id".to_string()];
    raw.directives.not_null = vec!["signal".to_string()];
    let compilation = compile(vec![raw]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    let model = compilation.model(&id("assay.raw_results")).unwrap();
    let described: BTreeSet<String> = model
        .assertions
        .iter()
        .map(|assertion| assertion.describe())
        .collect();
    assert!(described.contains("unique sample_id"));
    assert!(described.contains("not_null sample_id"));
    assert!(described.contains("not_null signal"));
}

#[test]
fn selection_options_are_unaffected_by_analysis() {
    let compilation = compile(vec![
        model("assay.raw_results", "select * from external.raw_results"),
        model("assay.results", "select sample_id from assay.raw_results"),
    ]);
    let selected = phlo_transform_core::Selection::all(&compilation);
    assert_eq!(selected.members.len(), 2);
}

#[test]
fn computes_column_lineage_and_impact() {
    let compilation = compile(vec![
        model("assay.raw_results", "select * from external.raw_results"),
        model(
            "assay.results",
            "select r.sample_id, r.signal / s.volume as concentration \
             from assay.raw_results r \
             join external.samples s using (sample_id)",
        ),
        model("assay.summary", "select concentration from assay.results"),
    ]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let lineage = compilation
        .column_lineage_report(&id("assay.results"), "concentration")
        .expect("lineage");
    assert_eq!(
        lineage.direct,
        vec![
            "assay.raw_results.signal".to_string(),
            "external.samples.volume".to_string(),
        ]
    );
    assert!(
        lineage
            .transitive
            .contains(&"external.raw_results.signal".to_string()),
        "{:?}",
        lineage.transitive
    );

    let model_lineage = compilation
        .model_lineage_report(&id("assay.results"))
        .expect("model lineage");
    assert_eq!(model_lineage.upstream, vec!["assay.raw_results"]);
    assert_eq!(model_lineage.downstream, vec!["assay.summary"]);

    let impact = compilation.impact_report(&ColumnRef::model(id("assay.results"), "concentration"));
    assert_eq!(impact.downstream_models, vec!["assay.summary"]);
    assert_eq!(
        impact.downstream_columns,
        vec!["assay.summary.concentration"]
    );
}

#[test]
fn enforced_contract_violation_is_an_error() {
    let mut model = model("assay.raw_results", "select * from external.raw_results");
    model.contract = Some(ModelContract {
        enforced: true,
        columns: vec![ColumnContract {
            name: "missing".to_string(),
            data_type: None,
            nullable: None,
        }],
    });
    let compilation = compile(vec![model]);
    assert!(!compilation.is_ok());
    assert!(compilation
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "TYPE005"));
}

#[test]
fn contract_type_mismatch_is_reported() {
    let mut model = model("assay.raw_results", "select * from external.raw_results");
    model.contract = Some(ModelContract {
        enforced: true,
        columns: vec![ColumnContract {
            name: "signal".to_string(),
            data_type: Some(DataType::Varchar),
            nullable: Some(false),
        }],
    });
    let compilation = compile(vec![model]);
    assert!(!compilation.is_ok());
    let messages: Vec<&str> = compilation
        .diagnostics
        .iter()
        .map(|diagnostic| diagnostic.message.as_str())
        .collect();
    assert!(messages
        .iter()
        .any(|message| message.contains("contract expects")));
}

#[test]
fn key_directive_generates_runtime_tests() {
    let mut model = model("assay.raw_results", "select * from external.raw_results");
    model.directives.keys = vec!["sample_id".to_string()];
    let compilation = compile(vec![model]);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let generated: Vec<&phlo_transform_core::CompiledTest> = compilation
        .tests
        .iter()
        .filter(|test| test.generated)
        .collect();
    assert_eq!(generated.len(), 2, "expected not_null + unique");
    assert!(generated
        .iter()
        .any(|test| test.compiled_sql.contains("is null")));
    assert!(generated
        .iter()
        .any(|test| test.compiled_sql.contains("having count(*) > 1")));
}

#[test]
fn impact_includes_registered_consumers() {
    let compilation = compile(vec![
        model("assay.raw_results", "select * from external.raw_results"),
        model("assay.results", "select sample_id from assay.raw_results"),
    ]);
    let target = ColumnRef::model(id("assay.results"), "sample_id");
    let mut registry = phlo_transform_core::StaticConsumerRegistry::new();
    registry.insert(&target, vec!["api: assay-results".to_string()]);
    let report = compilation.impact_report_with(&target, &registry);
    assert_eq!(report.consumers, vec!["api: assay-results"]);
}
