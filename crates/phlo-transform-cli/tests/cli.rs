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
    // Keep tests isolated from any local `.phlo` state left by manual runs.
    if let Some(position) = args.iter().position(|arg| *arg == "--root") {
        if let Some(root) = args.get(position + 1) {
            let _ = std::fs::remove_dir_all(PathBuf::from(root).join(".phlo"));
        }
    }
    run_unchecked(args)
}

/// Like `run`, but leaves `.phlo` state alone so successive invocations share
/// run history and materialised-version state.
fn run_unchecked(args: &[&str]) -> Output {
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

#[test]
fn lineage_model_human_output() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "lineage",
        "assay.results",
    ]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn impact_json_output() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--json",
        "impact",
        "assay.raw.sample_id",
    ]);
    assert!(output.status.success());
    assert_snapshot!(stdout(&output));
}

#[test]
fn lineage_upstream_flag_filters_direction() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--upstream",
        "lineage",
        "reporting.monthly",
    ]);
    assert!(output.status.success());
    let body = stdout(&output);
    assert!(body.contains("Upstream:"), "{body}");
    assert!(body.contains("assay.results"), "{body}");
    assert!(body.contains("Downstream: (none)"), "{body}");
}

#[test]
fn impact_accepts_a_model_argument() {
    let output = run(&["--root", "fixtures/basic-multi-root", "impact", "assay.raw"]);
    assert!(output.status.success());
    let body = stdout(&output);
    assert!(body.contains("assay.results"), "{body}");
    assert!(body.contains("reporting.monthly"), "{body}");
}

#[test]
fn init_scaffolds_a_runnable_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_str().expect("utf-8 path");
    let output = run(&["--root", root, "init"]);
    assert!(output.status.success(), "{}", stdout(&output));

    let output = run(&["--root", root, "check"]);
    assert!(output.status.success(), "{}", stdout(&output));

    let output = run(&["--root", root, "--json", "list"]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("example.raw_events"));
}

#[test]
fn doctor_reports_on_a_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_str().expect("utf-8 path");
    // Uninitialised directory: workspace check fails.
    let output = run(&["--root", root, "--json", "doctor"]);
    assert!(!output.status.success());
    assert!(stdout(&output).contains("\"workspace\""));
}

#[test]
fn translate_dbt_check_reports_classification() {
    let output = run(&[
        "--root",
        "fixtures/dbt-jaffle",
        "--json",
        "translate",
        "--from",
        "dbt",
        "--check",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let report: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("report JSON");
    let resources = report["resources"].as_array().expect("resources");
    let class_of = |name: &str| {
        resources
            .iter()
            .find(|r| r["name"].as_str().unwrap_or_default().ends_with(name))
            .map(|r| r["classification"].as_str().unwrap_or_default().to_string())
            .unwrap_or_else(|| format!("missing {name}"))
    };
    assert_eq!(class_of("customers"), "CLEAN");
    assert_eq!(class_of("orders_incremental"), "CLEAN");
    assert_eq!(class_of("labelled"), "REVIEW");
    assert_eq!(class_of("orders_snapshot"), "UNSUPPORTED");
}

#[test]
fn translate_dbt_writes_and_verifies() {
    let out_dir = tempfile::tempdir().expect("tempdir");
    let out = out_dir.path().join("generated");
    let output = run(&[
        "--root",
        "fixtures/dbt-clean",
        "translate",
        "--from",
        "dbt",
        "--out",
        out.to_str().expect("utf-8"),
        "--verify",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(out.join("transforms/staging/stg_events.sql").exists());
    assert!(out.join(".phlo/migration/dbt-translation.json").exists());

    // Rerun without --overwrite must refuse.
    let output = run(&[
        "--root",
        "fixtures/dbt-clean",
        "translate",
        "--from",
        "dbt",
        "--out",
        out.to_str().expect("utf-8"),
    ]);
    assert!(!output.status.success());
}

#[test]
fn translate_verify_failure_exits_nonzero() {
    // dbt-jaffle contains REVIEW models whose residual Jinja cannot compile, so
    // --verify must surface that as a failing exit code.
    let out_dir = tempfile::tempdir().expect("tempdir");
    let out = out_dir.path().join("generated");
    let output = run(&[
        "--root",
        "fixtures/dbt-jaffle",
        "translate",
        "--from",
        "dbt",
        "--out",
        out.to_str().expect("utf-8"),
        "--verify",
    ]);
    assert!(!output.status.success());
    assert!(out.join("transforms/marts/customers.sql").exists());
}

/// Full migration lifecycle: translate a dbt project, seed the sources in a
/// DuckDB file, then run it — including an incremental second run.
#[test]
fn translated_dbt_project_runs_on_duckdb() {
    let dir = tempfile::tempdir().expect("tempdir");
    let out = dir.path().join("generated");
    let duckdb_path = dir.path().join("shop.duckdb");

    let output = run(&[
        "--root",
        "fixtures/dbt-shop",
        "translate",
        "--from",
        "dbt",
        "--out",
        out.to_str().expect("utf-8"),
        "--verify",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));

    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        connection
            .execute_batch(
                "create schema raw;
                 create table raw.customers as
                     select * from (values (1,'Ada','eu'),(2,'Grace',null)) t(id,name,region);
                 create table raw.orders as
                     select * from (values
                         (10,1,50.0,'placed',timestamp '2024-01-01 10:00:00'),
                         (11,2,25.0,'shipped',timestamp '2024-01-02 11:00:00'))
                     t(id,customer_id,amount,status,ordered_at);",
            )
            .expect("seed sources");
    }

    let duckdb_arg = duckdb_path.to_str().expect("utf-8").to_string();
    let out_arg = out.to_str().expect("utf-8").to_string();
    let run_args = |extra: &[&'static str]| {
        let mut args = vec![
            "--root",
            out_arg.as_str(),
            "--adapter",
            "duckdb",
            "--duckdb-path",
            duckdb_arg.as_str(),
        ];
        args.extend_from_slice(extra);
        args
    };

    let output = run_unchecked(&run_args(&["run"]));
    assert!(output.status.success(), "{}", stdout(&output));

    // New and updated source rows must be picked up: the keyed model merges
    // and the time-window model appends past its watermark.
    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        connection
            .execute_batch(
                "insert into raw.orders values
                     (12,2,99.0,'placed',timestamp '2024-01-03 08:00:00');
                 update raw.orders set amount = 55.0 where id = 10;",
            )
            .expect("mutate sources");
    }

    let output = run_unchecked(&run_args(&["run"]));
    assert!(output.status.success(), "{}", stdout(&output));

    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        let merged: i64 = connection
            .query_row("select count(*) from marts.orders_incremental", [], |row| {
                row.get(0)
            })
            .expect("count merged");
        assert_eq!(merged, 3);
        let windowed: i64 = connection
            .query_row("select count(*) from marts.daily_revenue", [], |row| {
                row.get(0)
            })
            .expect("count windowed");
        assert_eq!(windowed, 3);
    }

    // A third run with no upstream change is a no-op.
    let output = run_unchecked(&run_args(&["plan"]));
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("SKIP   marts.orders_incremental"), "{body}");

    // inspect agrees with the recorded state.
    let output = run_unchecked(&run_args(&["inspect", "marts.orders_incremental"]));
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("status:   unchanged"));
}
