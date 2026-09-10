//! End-to-end CLI tests.
//!
//! Output paths are relative because the commands run from the workspace root.

use std::path::PathBuf;
use std::process::Output;

use assert_cmd::Command;
use insta::assert_snapshot;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(args: &[&str]) -> Output {
    Command::cargo_bin("phlo-transform")
        .expect("binary builds")
        .current_dir(workspace_root())
        .args(args)
        .output()
        .expect("command runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

#[test]
fn check_human_output_succeeds_for_valid_workspace() {
    let output = run(&["--root", "fixtures/basic-multi-root", "check"]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn check_fails_for_ambiguous_workspace() {
    let output = run(&["--root", "fixtures/ambiguous", "check"]);
    assert!(!output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn list_human_output() {
    let output = run(&["--root", "fixtures/basic-multi-root", "list"]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn inspect_human_output() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "inspect",
        "assay.results",
    ]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn check_json_output() {
    let output = run(&["--root", "fixtures/basic-multi-root", "--json", "check"]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn inspect_json_output() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--json",
        "inspect",
        "reporting.monthly",
    ]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn inspect_unknown_model_fails() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "inspect",
        "missing.model",
    ]);
    assert!(!output.status.success());
    assert!(stdout(&output).is_empty());
}

#[test]
fn missing_workspace_reports_json_error() {
    let output = run(&["--root", "fixtures/does-not-exist", "--json", "check"]);
    assert!(!output.status.success());
    let body = stdout(&output);
    assert!(body.contains("PROJECT001"), "{body}");
}
