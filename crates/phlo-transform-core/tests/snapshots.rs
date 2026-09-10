//! Snapshot tests for structured compiler output.
//!
//! These lock down JSON manifests and diagnostics so semantic changes are
//! visible in review.

use std::path::PathBuf;

use insta::assert_json_snapshot;
use phlo_transform_core::{
    compile, compile_with_provider, load_project, ColumnRef, Compilation, DataType, ModelId,
    Nullability, RelationSchema, SchemaColumn, SemanticModel, SemanticProject,
    StaticSchemaProvider,
};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from("../../fixtures").join(name)
}

fn compiled(name: &str) -> Compilation {
    let project = load_project(&fixture(name)).expect("workspace should load");
    compile(&project)
}

#[test]
fn basic_check_report() {
    assert_json_snapshot!("basic-check", compiled("basic-multi-root").check_report());
}

#[test]
fn basic_list_report() {
    assert_json_snapshot!("basic-list", compiled("basic-multi-root").list_report());
}

#[test]
fn basic_inspect_report() {
    let compilation = compiled("basic-multi-root");
    let id = ModelId::parse("assay.results").unwrap();
    assert_json_snapshot!(
        "basic-inspect",
        compilation.inspect_report(&id).expect("model exists")
    );
}

#[test]
fn basic_graph_artifact() {
    assert_json_snapshot!("basic-graph", compiled("basic-multi-root").graph_artifact());
}

#[test]
fn tests_suite_graph_artifact() {
    assert_json_snapshot!(
        "tests-suite-graph",
        compiled("tests-suite").graph_artifact()
    );
}

#[test]
fn custom_roots_check_report() {
    assert_json_snapshot!(
        "custom-roots-check",
        compiled("custom-roots").check_report()
    );
}

#[test]
fn trino_syntax_check_report() {
    assert_json_snapshot!("trino-check", compiled("trino-syntax").check_report());
}

#[test]
fn diagnostics_for_invalid_workspaces() {
    let cases = [
        ("ambiguous", "ambiguous"),
        ("cycle-two", "cycle-two"),
        ("cycle-three", "cycle-three"),
        ("self-reference", "self-reference"),
        ("invalid-sql", "invalid-sql"),
        ("malformed-directive", "malformed-directive"),
        ("unknown-directive", "unknown-directive"),
        ("duplicate-model-id", "duplicate-model-id"),
        ("duplicate-pinned-id", "duplicate-pinned-id"),
        ("duplicate-namespace", "duplicate-namespace"),
        ("contracts", "contracts"),
    ];
    for (fixture_name, snapshot_name) in cases {
        let compilation = compiled(fixture_name);
        assert_json_snapshot!(
            format!("diagnostics-{snapshot_name}"),
            compilation.diagnostics
        );
    }
}

#[test]
fn lineage_and_impact_reports() {
    let mut provider = StaticSchemaProvider::new();
    provider.insert(
        "external.raw_results",
        RelationSchema::new(vec![
            SchemaColumn {
                name: "sample_id".to_string(),
                data_type: DataType::Varchar,
                nullability: Nullability::NotNull,
            },
            SchemaColumn {
                name: "signal".to_string(),
                data_type: DataType::Double,
                nullability: Nullability::Nullable,
            },
        ]),
    );
    provider.insert(
        "external.samples",
        RelationSchema::new(vec![
            SchemaColumn {
                name: "sample_id".to_string(),
                data_type: DataType::Varchar,
                nullability: Nullability::NotNull,
            },
            SchemaColumn {
                name: "volume".to_string(),
                data_type: DataType::Double,
                nullability: Nullability::Nullable,
            },
        ]),
    );

    let models = vec![
        SemanticModel::in_memory(
            ModelId::parse("assay.raw_results").unwrap(),
            "select * from external.raw_results",
        ),
        SemanticModel::in_memory(
            ModelId::parse("assay.results").unwrap(),
            "select r.signal / s.volume as concentration \
             from assay.raw_results r \
             join external.samples s on r.sample_id = s.sample_id",
        ),
        SemanticModel::in_memory(
            ModelId::parse("assay.summary").unwrap(),
            "select concentration from assay.results",
        ),
    ];
    let compilation = compile_with_provider(&SemanticProject::in_memory(models), &provider);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let results = ModelId::parse("assay.results").unwrap();
    assert_json_snapshot!(
        "lineage",
        compilation
            .column_lineage_report(&results, "concentration")
            .unwrap()
    );
    assert_json_snapshot!(
        "impact",
        compilation.impact_report(&ColumnRef::model(results, "concentration"))
    );
}
