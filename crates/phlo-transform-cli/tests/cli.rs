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
fn lineage_graph_format_emits_canonical_document() {
    let output = run(&[
        "--root",
        "fixtures/ephemeral",
        "lineage",
        "--format",
        "graph",
    ]);
    assert!(output.status.success());
    let body = stdout(&output);
    let document: serde_json::Value = serde_json::from_str(&body).expect("graph output is JSON");
    let nodes = document["nodes"].as_array().expect("nodes array");
    let edges = document["edges"].as_array().expect("edges array");
    let uris: Vec<&str> = nodes
        .iter()
        .map(|node| node["uri"].as_str().unwrap())
        .collect();
    for expected in [
        "model://main/customers",
        "model://main/order_filters",
        "model://main/stg_orders",
        "dataset://main/customers",
        "dataset://external/raw_orders",
    ] {
        assert!(uris.contains(&expected), "missing node {expected}");
    }
    // Columns join the graph and tests attach to datasets.
    assert!(uris.contains(&"dataset://main/customers#total"));
    assert!(uris.iter().any(|uri| uri.starts_with("test://")));
    let pairs: Vec<(&str, &str)> = edges
        .iter()
        .map(|edge| (edge["from"].as_str().unwrap(), edge["to"].as_str().unwrap()))
        .collect();
    assert!(pairs.contains(&("dataset://external/raw_orders", "model://main/stg_orders")));
}

#[test]
fn lineage_graph_format_scopes_to_a_model() {
    let output = run(&[
        "--root",
        "fixtures/ephemeral",
        "lineage",
        "main.customers",
        "--format",
        "graph",
    ]);
    assert!(output.status.success());
    let body = stdout(&output);
    let document: serde_json::Value = serde_json::from_str(&body).expect("graph output is JSON");
    let uris: Vec<&str> = document["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"model://main/customers"));
    assert!(uris.contains(&"dataset://main/order_filters"));
    assert!(!uris.contains(&"model://main/stg_orders"));
}

#[test]
fn lineage_openlineage_format_exports_events() {
    let output = run(&[
        "--root",
        "fixtures/ephemeral",
        "lineage",
        "--format",
        "openlineage",
    ]);
    assert!(output.status.success());
    let body = stdout(&output);
    // The document is a bare JSON array — a valid batch-endpoint payload.
    let document: serde_json::Value =
        serde_json::from_str(&body).expect("openlineage output is JSON");
    let events = document.as_array().expect("event array");
    // Every event is a complete OpenLineage event with the required fields.
    for event in events {
        assert!(event["eventTime"].is_string(), "{event}");
        assert_eq!(
            event["producer"],
            "https://github.com/phlohouse/phlo-transform"
        );
        assert!(event["schemaURL"].is_string(), "{event}");
    }
    let jobs: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| event.get("job").is_some())
        .collect();
    let names: Vec<&str> = jobs
        .iter()
        .map(|job| job["job"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        vec!["main.customers", "main.order_filters", "main.stg_orders"]
    );
    let customers = jobs
        .iter()
        .find(|job| job["job"]["name"] == "main.customers")
        .unwrap();
    assert_eq!(customers["job"]["facets"]["jobType"]["jobType"], "MODEL");
    // The output dataset carries the columnLineage facet.
    let lineage = &customers["outputs"][0]["facets"]["columnLineage"]["fields"];
    assert!(lineage["total"]["inputFields"].is_array(), "{lineage}");
    // The ephemeral model is a temporary job output, not a physical table;
    // the view model is a VIEW with a physical symlink.
    let dataset = |name: &str| {
        events
            .iter()
            .filter_map(|event| event.get("dataset"))
            .find(|dataset| dataset["name"] == name)
            .unwrap_or_else(|| panic!("no dataset event for {name}"))
    };
    let staged = dataset("main.stg_orders");
    assert_eq!(staged["facets"]["datasetType"]["datasetType"], "JOB_OUTPUT");
    assert_eq!(staged["facets"]["datasetType"]["subType"], "TEMPORARY");
    assert!(staged["facets"]["symlinks"].is_null());
    let customers_dataset = dataset("main.customers");
    assert_eq!(
        customers_dataset["facets"]["datasetType"]["datasetType"],
        "VIEW"
    );
    assert!(customers_dataset["facets"]["symlinks"].is_object());
    assert!(events.iter().any(|event| {
        event
            .get("dataset")
            .is_some_and(|dataset| dataset["name"] == "external.raw_orders")
    }));
}

#[test]
fn impact_accepts_a_source_column() {
    // `raw.raw_events` is a seed — its CSV header supplies the schema, so
    // column-level impact works without an adapter.
    let output = run(&[
        "--root",
        "fixtures/native-seeds",
        "impact",
        "raw.raw_events.status",
    ]);
    assert!(output.status.success());
    let body = stdout(&output);
    assert!(body.contains("main.stg_events"), "{body}");
}

#[test]
fn column_lineage_reports_indirect_inputs() {
    let output = run(&[
        "--root",
        "fixtures/ephemeral",
        "lineage",
        "main.customers.total",
    ]);
    assert!(output.status.success());
    let body = stdout(&output);
    // `o.amount` is the direct aggregation input; `customer_id` join keys
    // show up as indirect lineage.
    assert!(body.contains("Indirect:"), "{body}");
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
    assert_eq!(class_of("labelled"), "CLEAN");
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
    // dbt-dynamic contains a dynamic `{% for %}` loop whose residual Jinja
    // cannot compile, so --verify must surface that as a failing exit code.
    let out_dir = tempfile::tempdir().expect("tempdir");
    let out = out_dir.path().join("generated");
    let output = run(&[
        "--root",
        "fixtures/dbt-dynamic",
        "translate",
        "--from",
        "dbt",
        "--out",
        out.to_str().expect("utf-8"),
        "--verify",
    ]);
    assert!(!output.status.success());
    assert!(out.join("transforms/probe.sql").exists());
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

/// Full pipeline on a real warehouse: a CSV seed read only through a nested
/// ephemeral chain must be loaded, the ephemeral models must never
/// materialise, the table must read the expanded subquery, and both the
/// generated `-- @not-null` test and the explicit `tests/` query must run
/// against the expansion — not against a relation that does not exist.
#[test]
fn seed_through_nested_ephemeral_runs_on_duckdb() {
    fn copy_dir(src: &std::path::Path, dest: &std::path::Path) {
        std::fs::create_dir_all(dest).unwrap();
        for entry in std::fs::read_dir(src).unwrap() {
            let entry = entry.unwrap();
            let target = dest.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).unwrap();
            }
        }
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("workspace");
    copy_dir(&workspace_root().join("fixtures/ephemeral-seed"), &root);
    let duckdb_path = dir.path().join("local.duckdb");

    let duckdb_arg = duckdb_path.to_str().expect("utf-8").to_string();
    let root_arg = root.to_str().expect("utf-8").to_string();
    let output = run(&[
        "--root",
        root_arg.as_str(),
        "--adapter",
        "duckdb",
        "--duckdb-path",
        duckdb_arg.as_str(),
        "run",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));

    let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
    // The materialised model saw the seed rows through both ephemeral hops.
    let mut stmt = connection
        .prepare("select id, doubled from main.events order by id")
        .expect("events table exists");
    let rows: Vec<(i64, f64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query events")
        .collect::<Result<_, _>>()
        .expect("rows");
    assert_eq!(rows, vec![(1, 20.0), (3, 30.0)]);

    // The seed table exists; the ephemeral relations never did.
    let seeded: i64 = connection
        .query_row("select count(*) from raw.raw_events", [], |row| row.get(0))
        .expect("seed loaded");
    assert_eq!(seeded, 3);
    for phantom in ["main.stg_events", "main.stg_placed"] {
        assert!(
            connection
                .query_row(&format!("select count(*) from {phantom}"), [], |row| {
                    row.get::<usize, i64>(0)
                })
                .is_err(),
            "{phantom} must not be materialised"
        );
    }
}

#[test]
fn lineage_without_target_prints_the_whole_graph() {
    let output = run(&["--root", "fixtures/basic-multi-root", "lineage"]);
    assert!(output.status.success());
    let body = stdout(&output);
    assert!(body.contains("assay.raw"), "{body}");
    assert!(body.contains("-> assay.results"), "{body}");
    assert!(body.contains("<- assay.results"), "{body}");
}

#[test]
fn empty_directory_warns_instead_of_passing_silently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().to_str().expect("utf-8 path");
    let output = run(&["--root", root, "check"]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("PROJECT009"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn parse_diagnostics_carry_path_line_and_column() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("transforms")).expect("mkdir");
    std::fs::write(
        root.join("transforms/bad.sql"),
        "select a\nfrom t\nselect oops\n",
    )
    .expect("write model");

    let output = run(&["--root", root.to_str().expect("utf-8"), "check"]);
    assert!(!output.status.success());
    let body = stdout(&output);
    assert!(body.contains("--> transforms/bad.sql:3:1"), "{body}");
    assert!(
        !body.contains("Line: 3"),
        "location tail not stripped: {body}"
    );
}

#[test]
fn inspect_surfaces_model_diagnostics() {
    let output = run(&["--root", "fixtures/invalid-sql", "inspect", "assay.broken"]);
    assert!(output.status.success());
    let body = stdout(&output);
    assert!(body.contains("PARSE001"), "{body}");
}

#[test]
fn doctor_compile_failure_names_the_first_error() {
    let output = run(&["--root", "fixtures/invalid-sql", "--json", "doctor"]);
    assert!(!output.status.success());
    let body = stdout(&output);
    assert!(body.contains("PARSE001"), "{body}");
}

/// Regression: an unqualified source (`from raw_events`) resolves through
/// DuckDB's search path to `main.raw_events`. Source-state enrichment must
/// use the same resolution or appended source rows stay invisible and plans
/// SKIP forever.
#[test]
fn unqualified_source_appends_trigger_rebuilds_on_duckdb() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let duckdb_path = root.join("local.duckdb");

    std::fs::create_dir_all(root.join("transforms/shop")).expect("mkdir");
    std::fs::write(
        root.join("transforms/shop/events.sql"),
        "select * from raw_events\n",
    )
    .expect("write model");

    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        connection
            .execute_batch(
                "create table main.raw_events as
                     select * from (values (1,'placed'),(2,'shipped')) t(id,status);",
            )
            .expect("seed source");
    }

    let duckdb_arg = duckdb_path.to_str().expect("utf-8").to_string();
    let root_arg = root.to_str().expect("utf-8").to_string();
    let run_args = |extra: &[&'static str]| {
        let mut args = vec![
            "--root",
            root_arg.as_str(),
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

    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        connection
            .execute_batch("insert into main.raw_events values (3,'returned')")
            .expect("append source row");
    }

    // The appended row must reclassify the model — a SKIP here means the
    // source-state fingerprint missed the change.
    let output = run_unchecked(&run_args(&["plan"]));
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(
        body.contains("BUILD  shop.events"),
        "expected a rebuild after source append: {body}"
    );
}

/// Positional selectors scope the plan; dependency closure pulls in what
/// the selection needs and says so.
#[test]
fn plan_positional_selector_scopes_the_plan() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--adapter",
        "duckdb",
        "--duckdb-path",
        ":memory:",
        "plan",
        "assay.results",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("assay.results"), "{body}");
    // Dependency closure pulls assay.raw into the plan.
    assert!(body.contains("assay.raw"), "{body}");
    assert!(body.contains("required by selected model"), "{body}");
    // reporting.monthly is downstream — not part of this plan.
    assert!(!body.contains("reporting.monthly"), "{body}");
    // Every decided model carries a human-readable reason.
    assert!(body.contains("does not exist"), "{body}");
}

/// `--exclude` subtracts even from dependency closure and warns about it —
/// when the excluded relation exists to be read.
#[test]
fn plan_exclude_beats_dependency_closure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let duckdb_path = dir.path().join("local.duckdb");
    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        connection
            .execute_batch(
                "create schema assay;
                 create table assay.raw as select 1 as id;",
            )
            .expect("materialise the excluded model");
    }
    let duckdb_arg = duckdb_path.to_str().expect("utf-8").to_string();
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--adapter",
        "duckdb",
        "--duckdb-path",
        duckdb_arg.as_str(),
        "plan",
        "assay.results",
        "--exclude",
        "assay.raw",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("excluding assay.raw"), "{body}");
    assert!(body.contains("warning:"), "{body}");
    assert!(body.contains("assay.results"), "{body}");
    assert!(!body.contains("BUILD  assay.raw"), "{body}");
}

/// Excluding a required dependency that was never materialised must fail —
/// the plan would otherwise schedule a read from a relation that does not
/// exist.
#[test]
fn plan_exclude_missing_dependency_fails() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--adapter",
        "duckdb",
        "--duckdb-path",
        ":memory:",
        "plan",
        "assay.results",
        "--exclude",
        "assay.raw",
    ]);
    assert!(!output.status.success());
    let body = stdout(&output);
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(
        body.contains("never been materialised") || stderr.contains("never been materialised"),
        "stdout: {body}\nstderr: {stderr}"
    );
}

/// The JSON plan exposes the resolved selection and structured reasons.
#[test]
fn plan_json_exposes_selection_and_reasons() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--adapter",
        "duckdb",
        "--duckdb-path",
        ":memory:",
        "--json",
        "plan",
        "assay.results+",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let plan: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("plan JSON");
    assert_eq!(plan["selection"]["matched"][0], "assay.results");
    assert_eq!(plan["selection"]["expanded"][0], "reporting.monthly");
    let models = plan["models"].as_array().expect("models");
    let monthly = models
        .iter()
        .find(|model| model["id"] == "reporting.monthly")
        .expect("monthly planned");
    assert_eq!(monthly["membership"], "expanded");
    assert!(
        monthly["reasons"]
            .as_array()
            .expect("reasons")
            .iter()
            .any(|reason| reason["kind"] == "selection_expansion"),
        "{monthly}"
    );
    let raw = models
        .iter()
        .find(|model| model["id"] == "assay.raw")
        .expect("raw planned by closure");
    assert_eq!(raw["membership"], "dependency");
}

/// `--changed` works with no recorded state: nothing can be proven
/// unchanged, so everything builds.
#[test]
fn plan_changed_selector_without_state_builds_all() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--adapter",
        "duckdb",
        "--duckdb-path",
        ":memory:",
        "plan",
        "--changed",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    for model in ["assay.raw", "assay.results", "reporting.monthly"] {
        assert!(body.contains(&format!("BUILD  {model}")), "{body}");
    }
}

/// Positional globs work on `list` too — one engine everywhere.
#[test]
fn list_positional_glob_filters_models() {
    let output = run(&["--root", "fixtures/basic-multi-root", "list", "assay.*"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("assay.raw"), "{body}");
    assert!(body.contains("assay.results"), "{body}");
    assert!(!body.contains("reporting.monthly"), "{body}");
}

/// `lineage --select` prints the selected subgraph.
#[test]
fn lineage_select_scopes_the_graph() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "lineage",
        "--select",
        "assay.results+",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("assay.results"), "{body}");
    assert!(body.contains("reporting.monthly"), "{body}");
    assert!(!body.contains("assay.raw"), "{body}");
}

/// `impact --select` reports the blast radius of a selection.
#[test]
fn impact_select_reports_blast_radius() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "impact",
        "--select",
        "assay.raw",
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("assay.results"), "{body}");
    assert!(body.contains("reporting.monthly"), "{body}");
}

/// `explain` resolves a unique suffix and reports the state comparison even
/// without a configured adapter.
#[test]
fn explain_resolves_suffix_and_reports_state() {
    let output = run(&["--root", "fixtures/basic-multi-root", "explain", "results"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("assay.results"), "{body}");
    assert!(body.contains("Versus recorded:"), "{body}");
}

/// Unknown `kind:` terms and unmatched names are errors, not silent
/// selection of everything.
#[test]
fn invalid_and_unmatched_selectors_fail() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "plan",
        "--select",
        "bogus:xyz",
    ]);
    assert!(!output.status.success());

    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "plan",
        "assay.reslts",
    ]);
    assert!(!output.status.success());
}

// --- Git-aware change detection (`--since`) -------------------------------

fn git(dir: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A git repo holding a two-model workspace: `assay.raw` reads source
/// `raw.events`, `assay.results` reads `assay.raw`. Returns the tempdir.
fn git_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("transforms/assay")).expect("mkdir");
    std::fs::create_dir_all(root.join("seeds")).expect("mkdir");
    std::fs::write(
        root.join("phlo.toml"),
        "[transform]\nroots = [\"transforms\"]\n",
    )
    .expect("phlo.toml");
    std::fs::write(
        root.join("transforms/assay/raw.sql"),
        "select * from raw.events\n",
    )
    .expect("raw.sql");
    std::fs::write(
        root.join("transforms/assay/results.sql"),
        "select * from assay.raw\n",
    )
    .expect("results.sql");
    std::fs::write(root.join("seeds/events.csv"), "id\n1\n").expect("seed");
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "test@phlo.dev"]);
    git(root, &["config", "user.name", "Phlo Test"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    git(root, &["branch", "-M", "main"]);
    dir
}

fn plan_since(root: &std::path::Path, args: &[&str]) -> Output {
    let root_arg = root.to_str().expect("utf-8").to_string();
    let mut full = vec![
        "--root",
        root_arg.as_str(),
        "--adapter",
        "duckdb",
        "--duckdb-path",
        ":memory:",
    ];
    full.extend_from_slice(args);
    run_unchecked(&full)
}

/// `--since` alone implies `changed`: a clean repo plans nothing.
#[test]
fn plan_since_clean_repo_selects_nothing() {
    let dir = git_workspace();
    let output = plan_since(dir.path(), &["plan", "--since", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("Git changes since main"), "{body}");
    assert!(body.contains("none"), "{body}");
    assert!(body.contains("Models (0)"), "{body}");
}

/// A modified model is a direct Git change carrying file provenance.
#[test]
fn plan_since_marks_modified_model() {
    let dir = git_workspace();
    std::fs::write(
        dir.path().join("transforms/assay/raw.sql"),
        "select id from raw.events\n",
    )
    .expect("edit");
    let output = plan_since(dir.path(), &["plan", "--since", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(
        body.contains("transforms/assay/raw.sql modified since main"),
        "{body}"
    );
    assert!(body.contains("BUILD  assay.raw"), "{body}");
    // Downstream results is not a direct change.
    assert!(!body.contains("BUILD  assay.results"), "{body}");
}

/// `lineage --diff` on a clean repo reports no changes.
#[test]
fn lineage_diff_clean_repo_reports_nothing() {
    let dir = git_workspace();
    let output = plan_since(dir.path(), &["lineage", "--diff", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("No lineage changes"),
        "{}",
        stdout(&output)
    );
}

/// A deleted model shows as removed and names the consumer it orphans.
#[test]
fn lineage_diff_reports_removed_model_and_orphans() {
    let dir = git_workspace();
    std::fs::remove_file(dir.path().join("transforms/assay/raw.sql")).expect("delete");
    let output = plan_since(dir.path(), &["lineage", "--diff", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("- assay.raw (model)"), "{body}");
    assert!(body.contains("orphans"), "{body}");
    assert!(body.contains("assay.results"), "{body}");
}

/// An edited model shows as changed with its version delta.
#[test]
fn lineage_diff_reports_changed_model() {
    let dir = git_workspace();
    std::fs::write(
        dir.path().join("transforms/assay/results.sql"),
        "select id, id + 1 as extra from assay.raw\n",
    )
    .expect("edit");
    let output = plan_since(dir.path(), &["lineage", "--diff", "main", "--json"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    let json: serde_json::Value = serde_json::from_str(&body).expect("json");
    let changed = json["nodes_changed"].as_array().expect("nodes_changed");
    assert!(
        changed.iter().any(|node| node["name"] == "assay.results"),
        "{body}"
    );
}

/// `lineage --diff` writes the `lineage_diff.json` artifact.
#[test]
fn lineage_diff_writes_artifact() {
    let dir = git_workspace();
    let output = plan_since(dir.path(), &["lineage", "--diff", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let artifact = dir.path().join(".phlo/transform/lineage_diff.json");
    let text = std::fs::read_to_string(&artifact).expect("artifact written");
    let json: serde_json::Value = serde_json::from_str(&text).expect("artifact json");
    assert_eq!(json["base_ref"], "main");
    assert!(json["base_commit"].is_string(), "{text}");
    // Provenance: the base is the merge-base, and the candidate records the
    // git head + worktree state the diff was produced against.
    assert_eq!(json["base_kind"], "merge-base", "{text}");
    let head = String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(dir.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .trim()
    .to_string();
    assert_eq!(json["candidate"]["head"], head, "{text}");
    assert_eq!(json["candidate"]["dirty"], false, "{text}");
    // The candidate's lineage fingerprint binds the artifact to these
    // definitions — promotion audits it against the compiled workspace.
    assert!(json["candidate"]["lineage_hash"].is_string(), "{text}");
    assert!(
        json["candidate"]["model_versions"]["assay.raw"].is_string(),
        "{text}"
    );
}

/// `lineage --diff main` uses merge-base semantics: work committed on
/// `main` after the branch diverged is not part of the comparison.
#[test]
fn lineage_diff_uses_merge_base_not_ref_head() {
    let dir = git_workspace();
    let root = dir.path();
    // A ── feature; then main advances to B with a model the feature
    // never had.
    git(root, &["checkout", "-q", "-b", "feature"]);
    git(root, &["checkout", "-q", "main"]);
    std::fs::write(
        root.join("transforms/assay/main_only.sql"),
        "select 1 as id\n",
    )
    .expect("main-only model");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "main moves on"]);
    git(root, &["checkout", "-q", "feature"]);
    // The feature's own change, uncommitted in the worktree.
    std::fs::write(
        root.join("transforms/assay/results.sql"),
        "select id, id + 1 as extra from assay.raw\n",
    )
    .expect("edit");

    let output = plan_since(root, &["lineage", "--diff", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    // merge-base(main, feature) = A: `main_only` is on neither side — with
    // direct `main^{commit}` semantics it would report as removed.
    assert!(!body.contains("main_only"), "{body}");
    // The feature's real change still reports, labelled as a merge-base diff.
    assert!(body.contains("assay.results"), "{body}");
    assert!(body.contains("merge-base"), "{body}");
}

/// `--diff base candidate` compares the exact refs — the dirty worktree
/// is not part of either side.
#[test]
fn lineage_diff_two_refs_compares_exactly() {
    let dir = git_workspace();
    let root = dir.path();
    git(root, &["checkout", "-q", "-b", "feature"]);
    std::fs::write(
        root.join("transforms/assay/feature_only.sql"),
        "select 1 as id\n",
    )
    .expect("feature model");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "feature work"]);
    // An uncommitted model — invisible to an exact ref comparison.
    std::fs::write(root.join("transforms/assay/dirty.sql"), "select 1 as id\n")
        .expect("uncommitted model");

    let output = plan_since(root, &["lineage", "--diff", "main", "feature"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("+ assay.feature_only (model)"), "{body}");
    assert!(!body.contains("dirty"), "{body}");
    // Exact refs name the resolved head, not a merge-base.
    let artifact = root.join(".phlo/transform/lineage_diff.json");
    let text = std::fs::read_to_string(&artifact).expect("artifact written");
    let json: serde_json::Value = serde_json::from_str(&text).expect("artifact json");
    assert_eq!(json["base_kind"], "ref", "{text}");
    assert_eq!(json["candidate"]["git_ref"], "feature", "{text}");
}

/// `changed+` expands the Git change set downstream through the graph.
#[test]
fn plan_since_changed_plus_expands_downstream() {
    let dir = git_workspace();
    std::fs::write(
        dir.path().join("transforms/assay/raw.sql"),
        "select id from raw.events\n",
    )
    .expect("edit");
    let output = plan_since(dir.path(), &["plan", "--since", "main", "changed+"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("BUILD  assay.raw"), "{body}");
    assert!(body.contains("BUILD  assay.results"), "{body}");
    assert!(body.contains("selected by `changed+`"), "{body}");
}

/// A changed seed selects the models that consume it.
#[test]
fn plan_since_seed_change_marks_consumers() {
    let dir = git_workspace();
    std::fs::write(dir.path().join("seeds/events.csv"), "id\n1\n2\n").expect("edit");
    let output = plan_since(dir.path(), &["plan", "--since", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("seed events"), "{body}");
    assert!(body.contains("BUILD  assay.raw"), "{body}");
}

/// `--since` with include terms that don't use `changed` is an error —
/// the flag must not silently warp an unrelated selection.
#[test]
fn plan_since_without_changed_term_errors() {
    let dir = git_workspace();
    let output = plan_since(dir.path(), &["plan", "--since", "main", "assay.raw"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(
        stderr.contains("`--since` supplies the `changed` selector"),
        "{stderr}"
    );
}

#[test]
fn plan_since_unknown_ref_errors() {
    let dir = git_workspace();
    let output = plan_since(dir.path(), &["plan", "--since", "no-such-ref"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(stderr.contains("unknown Git ref"), "{stderr}");
}

#[test]
fn plan_since_outside_a_repository_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("transforms/assay")).expect("mkdir");
    std::fs::write(
        root.join("phlo.toml"),
        "[transform]\nroots = [\"transforms\"]\n",
    )
    .expect("phlo.toml");
    std::fs::write(root.join("transforms/assay/raw.sql"), "select 1\n").expect("raw.sql");
    let output = plan_since(root, &["plan", "--since", "main"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(stderr.contains("not inside"), "{stderr}");
}

/// The JSON plan carries the structured Git change set and per-model
/// `git_change` provenance reasons.
#[test]
fn plan_since_json_exposes_git_change_set() {
    let dir = git_workspace();
    std::fs::write(
        dir.path().join("transforms/assay/raw.sql"),
        "select id from raw.events\n",
    )
    .expect("edit");
    let output = plan_since(dir.path(), &["--json", "plan", "--since", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let plan: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("plan JSON");
    assert_eq!(plan["git"]["since"], "main");
    assert_eq!(
        plan["git"]["merge_base"].as_str().unwrap_or_default().len(),
        40
    );
    assert_eq!(
        plan["git"]["models"][0]["model"], "assay.raw",
        "{}",
        plan["git"]["models"]
    );
    assert_eq!(
        plan["git"]["paths"][0]["path"], "transforms/assay/raw.sql",
        "{}",
        plan["git"]["paths"]
    );
    let models = plan["models"].as_array().expect("models");
    let raw = models
        .iter()
        .find(|model| model["id"] == "assay.raw")
        .expect("raw planned");
    assert!(
        raw["reasons"]
            .as_array()
            .expect("reasons")
            .iter()
            .any(|reason| reason["kind"] == "git_change"),
        "{raw}"
    );
}

/// A dirty working tree counts: uncommitted edits are Git changes.
#[test]
fn plan_since_includes_working_tree_changes() {
    let dir = git_workspace();
    // Unstaged edit — never committed.
    std::fs::write(
        dir.path().join("transforms/assay/raw.sql"),
        "select id as event_id from raw.events\n",
    )
    .expect("edit");
    let output = plan_since(dir.path(), &["plan", "--since", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("BUILD  assay.raw"),
        "{}",
        stdout(&output)
    );
}

/// A deleted dependency marks its dependents.
#[test]
fn plan_since_deleted_dependency_marks_dependents() {
    let dir = git_workspace();
    git(dir.path(), &["rm", "-q", "transforms/assay/raw.sql"]);
    let output = plan_since(dir.path(), &["plan", "--since", "main"]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("deleted assay.raw"), "{body}");
    assert!(
        body.contains("dependency assay.raw was removed since main"),
        "{body}"
    );
    assert!(body.contains("BUILD  assay.results"), "{body}");
}

// ---------------------------------------------------------------------
// Execution resilience: resume, retry-failed, fail-fast, summary output.
// ---------------------------------------------------------------------

/// A workspace with one healthy model, one model whose external source is
/// missing at run time, and a dependent of the failing model.
fn resilience_workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let transforms = dir.path().join("workflows/main/transforms");
    std::fs::create_dir_all(&transforms).unwrap();
    std::fs::write(transforms.join("ok.sql"), "select 1 as id\n").unwrap();
    std::fs::write(
        transforms.join("broken.sql"),
        "select * from external.missing_table\n",
    )
    .unwrap();
    std::fs::write(transforms.join("child.sql"), "select * from main.broken\n").unwrap();
    dir
}

/// Args shared by every invocation of a resilience test.
fn resilience_args(dir: &tempfile::TempDir) -> Vec<String> {
    let duckdb = dir.path().join("run.duckdb");
    vec![
        "--root".into(),
        dir.path().to_str().expect("utf-8").into(),
        "--adapter".into(),
        "duckdb".into(),
        "--duckdb-path".into(),
        duckdb.to_str().expect("utf-8").into(),
    ]
}

/// The run id of the last run, read from the `run.json` artifact.
fn last_run_id(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join(".phlo/transform/run.json");
    let artifact: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("run.json")).unwrap();
    artifact["run"]["run_id"]
        .as_str()
        .expect("run_id")
        .to_string()
}

/// Materialise the missing source so the broken model can succeed.
fn heal_workspace(dir: &tempfile::TempDir) {
    let connection = duckdb::Connection::open(dir.path().join("run.duckdb")).expect("open duckdb");
    connection
        .execute_batch(
            "create schema if not exists external;
             create or replace table external.missing_table as
                 select * from (values (1,'x')) t(id,label);",
        )
        .expect("create source");
}

#[test]
fn run_fails_cleanly_and_reports_the_summary() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let body = stdout(&output);
    // The run summary names counts and per-node outcomes.
    assert!(body.contains("FAILED"), "{body}");
    assert!(body.contains("1 failed"), "{body}");
    assert!(body.contains("1 blocked"), "{body}");
    assert!(body.contains("passed"), "{body}");
    assert!(body.contains("main.broken"), "{body}");
    assert!(body.contains("main.child"), "{body}");
    assert!(body.contains("--retry-failed"), "{body}");
    assert!(body.contains("--resume"), "{body}");

    // The JSON run result carries the same information structurally.
    let mut json_args = base.clone();
    json_args.extend(["--json".into(), "run".into()]);
    let output = run_unchecked(&json_args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json run");
    assert_eq!(result["status"], "failed");
    assert_eq!(result["counts"]["failed"], 1);
    assert_eq!(result["counts"]["blocked"], 1);
    let broken = result["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == "main.broken")
        .expect("broken result");
    assert_eq!(broken["status"], "failed");
    assert!(broken["failure"]["category"].is_string(), "{broken}");
    let child = result["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == "main.child")
        .expect("child result");
    assert_eq!(child["status"], "blocked");
    assert_eq!(child["failure"]["category"], "dependency");
}

/// The id of the still-`running` run — waits until the run has actually
/// started, so a test can kill the process mid-flight.
fn running_run_id(dir: &tempfile::TempDir) -> String {
    let db = dir.path().join(".phlo/transform/state.db");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if db.exists() {
            if let Ok(connection) = rusqlite::Connection::open(&db) {
                // Read-only while the run holds the writer — retry on busy.
                if let Ok(id) = connection.query_row(
                    "select run_id from runs where status = 'running' order by started_at desc limit 1",
                    [],
                    |row| row.get::<_, String>(0),
                ) {
                    return id;
                }
            }
        }
        assert!(std::time::Instant::now() < deadline, "run never started");
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

#[test]
fn resume_continues_an_interrupted_run() {
    let dir = resilience_workspace();
    // Make `main.broken` heavy rather than broken so the run is reliably
    // in-flight when we kill it — `@table` materialisation forces the query
    // to execute (~3s); `main.child` queues behind it.
    std::fs::write(
        dir.path()
            .join("workflows/main/transforms/broken.sql"),
        "-- @table\nselect sum(t1.x * t2.y) as total from range(0,300000) t1(x), range(0,3000) t2(y)\n",
    )
    .unwrap();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_phlo-transform"))
        .current_dir(workspace_root())
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn run");

    // Kill the process mid-run — the state store keeps an honest partial
    // record: `main.ok` passed, `main.broken` still `running`.
    let run_id = running_run_id(&dir);
    // Give the run a moment to record the in-flight model.
    std::thread::sleep(std::time::Duration::from_millis(200));
    child.kill().expect("kill mid-run");
    child.wait().unwrap();
    let short = &run_id[..8];

    // Resume recompiles the workspace — make the heavy model trivial so the
    // resumed build is fast. `main.ok` is reused; `main.broken` and
    // `main.child` run, all under the same run id.
    std::fs::write(
        dir.path().join("workflows/main/transforms/broken.sql"),
        "select 1 as id\n",
    )
    .unwrap();
    let mut args = base.clone();
    args.extend(["run".into(), "--resume".into(), short.into()]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains(&format!("Run {short}  PASSED")), "{body}");
    assert!(body.contains("cached"), "{body}");
    assert_eq!(last_run_id(&dir), run_id, "resume keeps the run id");
}

#[test]
fn resume_redirects_a_finished_run_to_retry_failed() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let run_id = last_run_id(&dir);

    // A finished run's history is immutable — resume refuses and points at
    // --retry-failed.
    let mut args = base.clone();
    args.extend(["run".into(), "--resume".into(), run_id[..8].into()]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(stderr.contains("--retry-failed"), "{stderr}");
}

#[test]
fn retry_failed_reruns_the_failed_portion_as_a_new_run() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let run_id = last_run_id(&dir);
    let short = &run_id[..8];

    heal_workspace(&dir);
    let mut args = base.clone();
    args.extend(["run".into(), "--retry-failed".into(), short.into()]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains(&format!("continues run {short}")), "{body}");
    // A new run id was recorded.
    assert_ne!(last_run_id(&dir), run_id);
    // The healthy model was not part of the retry.
    assert!(!body.contains("main.ok"), "{body}");
}

#[test]
fn fail_fast_cancels_queued_work() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    // --jobs 1 makes the outcome deterministic: `main.broken` is first in
    // plan order, fails, and fail-fast then leaves `main.ok` unscheduled.
    let mut json_args = base.clone();
    json_args.extend([
        "--json".into(),
        "run".into(),
        "--fail-fast".into(),
        "--jobs".into(),
        "1".into(),
    ]);
    let output = run_unchecked(&json_args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json run");
    assert_eq!(result["counts"]["failed"], 1);
    assert_eq!(result["counts"]["blocked"], 1);
    assert_eq!(result["counts"]["cancelled"], 1);
    let child = result["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == "main.child")
        .expect("child result");
    assert_eq!(child["status"], "blocked");
    let ok = result["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["model"] == "main.ok")
        .expect("ok result");
    assert_eq!(ok["status"], "cancelled");
}

#[test]
fn retries_and_timeout_flags_are_accepted() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.extend([
        "run".into(),
        "--retries".into(),
        "2".into(),
        "--jobs".into(),
        "2".into(),
        "--model-timeout".into(),
        "30s".into(),
    ]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    // The flags wire through; the run itself still fails on the missing
    // source.
    assert!(!output.status.success());
    assert!(stdout(&output).contains("FAILED"), "{}", stdout(&output));
}

#[test]
fn resume_and_retry_failed_reject_selector_flags() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    let run_id = last_run_id(&dir);
    let short = &run_id[..8];

    let mut args = base.clone();
    args.extend([
        "run".into(),
        "--resume".into(),
        short.into(),
        "--select".into(),
        "main.ok".into(),
    ]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
}

#[test]
fn resume_with_a_mismatched_environment_is_refused_before_provisioning() {
    let dir = resilience_workspace();
    // A run recorded under environment label `a` (no Nessie — label only).
    let mut args = resilience_args(&dir);
    args.extend(["run".into(), "--environment".into(), "a".into()]);
    run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    let run_id = last_run_id(&dir);

    // `--environment b` disagrees with the stored run's environment — the
    // refusal must happen when the environment is resolved, before any
    // provisioning, not only inside the runner.
    let mut args = resilience_args(&dir);
    args.extend([
        "run".into(),
        "--resume".into(),
        run_id[..8].into(),
        "--environment".into(),
        "b".into(),
    ]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(stderr.contains("refusing to continue into `b`"), "{stderr}");
}

#[test]
fn resume_with_an_unknown_run_id_fails_before_provisioning() {
    let dir = resilience_workspace();
    let mut args = resilience_args(&dir);
    args.push("run".into());
    run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());

    let mut args = resilience_args(&dir);
    args.extend([
        "run".into(),
        "--resume".into(),
        "deadbeef".into(),
        "--environment".into(),
        "ci/x".into(),
    ]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
    assert!(stderr.contains("no run matches"), "{stderr}");
    // The run-id lookup failed before the environment could provision, so
    // no provisioning artifact was recorded for `ci/x`.
    assert!(
        !dir.path()
            .join(".phlo/transform")
            .read_dir()
            .map(|mut entries| entries.any(|entry| entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .starts_with("environment")))
            .unwrap_or(false),
        "no environment artifacts should exist"
    );
}

#[test]
fn continuations_only_apply_to_run_and_apply() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    let short = &last_run_id(&dir)[..8];

    // plan/test resolve their environment from --environment/--ref only —
    // inheriting a stored run's environment would retarget the compilation
    // while state scoping and labels still came from the flags.
    for command in ["plan", "test", "diff", "check", "list"] {
        for flag in ["--resume", "--retry-failed"] {
            let mut args = base.clone();
            args.extend([command.into(), flag.into(), short.into()]);
            let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
            assert!(
                !output.status.success(),
                "{command} {flag} should be rejected"
            );
            let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
            assert!(stderr.contains("run`/`apply"), "{command} {flag}: {stderr}");
        }
    }
}

// ---------------------------------------------------------------------------
// Bundle 5: refs, branch diff, promotion gates
// ---------------------------------------------------------------------------

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
}

#[test]
fn ref_commands_require_nessie() {
    // `ref` is dispatched before workspace load — no --root needed.
    for args in [
        vec!["ref", "list"],
        vec!["ref", "show", "ci/x"],
        vec!["ref", "create", "ci/x"],
        vec!["ref", "delete", "ci/x"],
    ] {
        let output = Command::cargo_bin("phlo-transform")
            .expect("binary builds")
            .current_dir(workspace_root())
            .env_remove("PHLO_NESSIE_ENDPOINT")
            .args(&args)
            .output()
            .expect("command runs");
        assert!(!output.status.success(), "{args:?}");
        assert!(
            stderr(&output).contains("Nessie"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn promote_requires_a_candidate() {
    let output = Command::cargo_bin("phlo-transform")
        .expect("binary builds")
        .current_dir(workspace_root())
        .env_remove("PHLO_NESSIE_ENDPOINT")
        .args(["promote", "--to", "main"])
        .output()
        .expect("command runs");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("candidate"), "{}", stderr(&output));
}

#[test]
fn promote_from_alias_reaches_nessie() {
    // `promote --from <ref>` resolves the candidate before touching Nessie —
    // with no endpoint configured it fails on the endpoint, proving `--from`
    // was accepted as the candidate.
    let output = Command::cargo_bin("phlo-transform")
        .expect("binary builds")
        .current_dir(workspace_root())
        .env_remove("PHLO_NESSIE_ENDPOINT")
        .args(["promote", "--from", "ci/x", "--to", "main"])
        .output()
        .expect("command runs");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("Nessie"), "{}", stderr(&output));
}

#[test]
fn branch_diff_needs_a_candidate() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--adapter",
        "duckdb",
        "--duckdb-path",
        ":memory:",
        "diff",
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--from"), "{}", stderr(&output));
}

/// A full branch-diff pass on DuckDB: run the workspace under `--ref main`,
/// then diff a candidate that has no records — every materialised dataset is
/// missing on the candidate, so it reports `removed`. Deterministic and
/// Docker-free; the Nessie e2e covers the real branch topology.
#[test]
fn branch_diff_reports_datasets_missing_on_candidate() {
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

    // Materialise under the `main` environment label.
    let output = run_unchecked(&run_args(&["run", "--ref", "main"]));
    assert!(output.status.success(), "{}", stdout(&output));

    // The candidate never ran: every base dataset reports `removed`.
    let output = run_unchecked(&run_args(&[
        "--json", "diff", "--from", "ci/x", "--to", "main",
    ]));
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let report: serde_json::Value =
        serde_json::from_str(&stdout(&output)).expect("branch diff json");
    assert_eq!(report["candidate_ref"], "ci/x");
    assert_eq!(report["base_ref"], "main");
    let datasets = report["datasets"].as_array().expect("datasets array");
    assert!(!datasets.is_empty());
    for dataset in datasets {
        assert_eq!(
            dataset["status"],
            "removed",
            "{}",
            dataset["dataset"].as_str().unwrap_or("?")
        );
        assert_eq!(dataset["kind"], "model");
    }
    // Deterministic: sorted by dataset name, and a second run is identical
    // modulo timestamps.
    let names: Vec<&str> = datasets
        .iter()
        .map(|dataset| dataset["dataset"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    let again = run_unchecked(&run_args(&[
        "--json", "diff", "--from", "ci/x", "--to", "main",
    ]));
    assert!(again.status.success());
    let report2: serde_json::Value =
        serde_json::from_str(&stdout(&again)).expect("branch diff json");
    assert_eq!(report["datasets"], report2["datasets"]);
    assert_eq!(report["rows"], report2["rows"]);
}

#[test]
fn model_diff_rejects_from_flag() {
    // `--from` means the branch-diff candidate; a model diff's candidate comes
    // from `--ref`.
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "diff",
        "assay.results",
        "--from",
        "ci/x",
    ]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--ref"), "{}", stderr(&output));
}

#[test]
fn branch_diff_rejects_model_only_flags() {
    for (flag, value) in [
        ("--base", "dev"),
        ("--base-relation", "cat.sch.tbl"),
        ("--partition", "dt"),
        ("--sample", "0.5"),
    ] {
        let output = run(&[
            "--root",
            "fixtures/basic-multi-root",
            "diff",
            "--from",
            "ci/x",
            flag,
            value,
        ]);
        assert!(!output.status.success(), "{flag}");
        assert!(
            stderr(&output).contains("model diffs"),
            "{flag}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn promote_rejects_disagreeing_candidates() {
    let output = Command::cargo_bin("phlo-transform")
        .expect("binary builds")
        .current_dir(workspace_root())
        .env_remove("PHLO_NESSIE_ENDPOINT")
        .args(["promote", "ci/a", "--from", "ci/b", "--to", "main"])
        .output()
        .expect("command runs");
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("candidate given twice"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn diff_rejects_disagreeing_candidates() {
    // `--from` and `--ref` naming different candidates is an error, matching
    // `promote` — never a silent pick.
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "diff",
        "--from",
        "ci/a",
        "--ref",
        "ci/b",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("candidate given twice"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn ref_delete_refuses_main() {
    // `main` is every environment's default base — it must not go under the
    // same verb as a scratch branch. The guard fires before Nessie is even
    // consulted.
    let output = Command::cargo_bin("phlo-transform")
        .expect("binary builds")
        .current_dir(workspace_root())
        .env_remove("PHLO_NESSIE_ENDPOINT")
        .args(["ref", "delete", "main"])
        .output()
        .expect("command runs");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("main"), "{}", stderr(&output));
}

/// `state runs`/`state show`/`state model` inspect what `run` recorded.
#[test]
fn state_commands_inspect_recorded_runs_on_duckdb() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let duckdb_path = root.join("local.duckdb");

    std::fs::create_dir_all(root.join("transforms/shop")).expect("mkdir");
    std::fs::write(
        root.join("transforms/shop/events.sql"),
        "select * from raw_events\n",
    )
    .expect("write model");

    {
        let connection = duckdb::Connection::open(&duckdb_path).expect("open duckdb");
        connection
            .execute_batch("create table main.raw_events as select 1 as id")
            .expect("seed source");
    }

    let duckdb_arg = duckdb_path.to_str().expect("utf-8").to_string();
    let root_arg = root.to_str().expect("utf-8").to_string();
    let args = |extra: &[&'static str]| {
        let mut args = vec![
            "--root",
            root_arg.as_str(),
            "--adapter",
            "duckdb",
            "--duckdb-path",
            duckdb_arg.as_str(),
        ];
        args.extend_from_slice(extra);
        args
    };

    let output = run_unchecked(&args(&["run"]));
    assert!(output.status.success(), "{}", stdout(&output));

    // `state runs` lists the recorded run with its environment column.
    let output = run_unchecked(&args(&["state", "runs"]));
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("passed"), "{body}");

    // `state show` resolves a run-id prefix to its model records.
    let output = run_unchecked(&args(&["--json", "state", "runs"]));
    assert!(output.status.success(), "{}", stdout(&output));
    let runs: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("json");
    let run_id = runs[0]["run_id"].as_str().expect("run id").to_string();
    let output = run_unchecked(&[
        "--root",
        root_arg.as_str(),
        "--adapter",
        "duckdb",
        "--duckdb-path",
        duckdb_arg.as_str(),
        "state",
        "show",
        &run_id[..8],
    ]);
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("shop.events"), "{body}");

    // `state model` shows the recorded materialisation incl. the adapter
    // that produced it.
    let output = run_unchecked(&args(&["state", "model", "shop.events"]));
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains("duckdb"), "{body}");
}

/// `--state <path>` redirects the SQLite state store location.
#[test]
fn state_flag_selects_sqlite_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("ws");
    let state_path = dir.path().join("shared-state.db");
    let duckdb_path = root.join("local.duckdb");

    std::fs::create_dir_all(root.join("transforms/shop")).expect("mkdir");
    std::fs::write(root.join("transforms/shop/events.sql"), "select 1 as id\n")
        .expect("write model");

    let state_arg = state_path.to_str().expect("utf-8").to_string();
    let duckdb_arg = duckdb_path.to_str().expect("utf-8").to_string();
    let root_arg = root.to_str().expect("utf-8").to_string();
    let args = |extra: &[&'static str]| {
        let mut args = vec![
            "--root",
            root_arg.as_str(),
            "--adapter",
            "duckdb",
            "--duckdb-path",
            duckdb_arg.as_str(),
            "--state",
            state_arg.as_str(),
        ];
        args.extend_from_slice(extra);
        args
    };

    let output = run_unchecked(&args(&["run"]));
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(state_path.exists(), "state file created at --state path");
    // The default location must not be used.
    assert!(!root.join(".phlo/transform/state.db").exists());

    let output = run_unchecked(&args(&["state", "runs"]));
    assert!(output.status.success(), "{}", stdout(&output));
    assert!(stdout(&output).contains("passed"), "{}", stdout(&output));
}

/// A `--state postgres://...` URL that cannot connect is a hard error — it
/// must never silently fall back to local state.
#[test]
fn unreachable_postgres_state_url_errors() {
    let output = run(&[
        "--root",
        "fixtures/basic-multi-root",
        "--state",
        "postgres://user:secret@127.0.0.1:1/phlo",
        "state",
        "runs",
    ]);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("could not connect"), "{message}");
    // Credentials in the URL must not echo back in the error.
    assert!(!message.contains("secret"), "{message}");
}
