//! Snapshot tests for structured compiler output.
//!
//! These lock down JSON manifests and diagnostics so semantic changes are
//! visible in review.

use std::path::PathBuf;

use insta::assert_json_snapshot;
use phlo_transform_core::{compile, load_project, Compilation, ModelId};

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
    ];
    for (fixture_name, snapshot_name) in cases {
        let compilation = compiled(fixture_name);
        assert_json_snapshot!(
            format!("diagnostics-{snapshot_name}"),
            compilation.diagnostics
        );
    }
}
