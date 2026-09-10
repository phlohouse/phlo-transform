//! Model version invalidation tests.

use phlo_transform_core::{compile, Materialization, ModelId, SemanticModel, SemanticProject};

fn model(name: &str, sql: &str) -> SemanticModel {
    SemanticModel::in_memory(ModelId::parse(name).unwrap(), sql)
}

fn version_of(models: Vec<SemanticModel>, name: &str) -> phlo_transform_core::ModelVersion {
    let compilation = compile(&SemanticProject::in_memory(models));
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    compilation
        .model(&ModelId::parse(name).unwrap())
        .unwrap()
        .version
        .clone()
}

#[test]
fn formatting_and_comments_do_not_change_version() {
    let plain = version_of(
        vec![model("assay.raw", "select * from external.raw_results")],
        "assay.raw",
    );
    let formatted = version_of(
        vec![model(
            "assay.raw",
            "SELECT\n    *\nFROM external.raw_results\n-- an unrelated comment\n",
        )],
        "assay.raw",
    );
    assert_eq!(plain.hash, formatted.hash);
}

#[test]
fn sql_change_changes_version() {
    let before = version_of(vec![model("assay.raw", "select 1 as x")], "assay.raw");
    let after = version_of(vec![model("assay.raw", "select 2 as x")], "assay.raw");
    assert_ne!(before.sql_hash, after.sql_hash);
    assert_ne!(before.hash, after.hash);
}

#[test]
fn owner_and_tag_changes_do_not_rebuild() {
    let mut a = model("assay.raw", "select * from external.raw_results");
    a.config.tags = vec!["gold".to_string()];
    a.config.owner = Some("team-a".to_string());

    let mut b = model("assay.raw", "select * from external.raw_results");
    b.config.tags = vec!["silver".to_string()];
    b.config.owner = Some("team-b".to_string());

    assert_eq!(
        version_of(vec![a], "assay.raw").hash,
        version_of(vec![b], "assay.raw").hash
    );
}

#[test]
fn materialization_change_changes_version() {
    let mut a = model("assay.raw", "select * from external.raw_results");
    a.config.materialization = Materialization::View;
    let mut b = model("assay.raw", "select * from external.raw_results");
    b.config.materialization = Materialization::Table;
    assert_ne!(
        version_of(vec![a], "assay.raw").hash,
        version_of(vec![b], "assay.raw").hash
    );
}

#[test]
fn upstream_change_invalidates_downstream() {
    let build = |raw_sql: &str| {
        version_of(
            vec![
                model("assay.raw", raw_sql),
                model("assay.results", "select * from assay.raw"),
            ],
            "assay.results",
        )
    };

    let before = build("select 1 as id");
    let after = build("select 2 as id");
    // The downstream SQL is unchanged but its desired version changes.
    assert_eq!(before.sql_hash, after.sql_hash);
    assert_ne!(before.dependency_hash, after.dependency_hash);
    assert_ne!(before.hash, after.hash);
}
