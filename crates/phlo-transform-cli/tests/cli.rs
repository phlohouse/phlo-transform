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

#[test]
fn resume_continues_the_same_run() {
    let dir = resilience_workspace();
    let base = resilience_args(&dir);
    let mut args = base.clone();
    args.push("run".into());
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let run_id = last_run_id(&dir);
    let short = &run_id[..8];

    // Resume while still broken: passed work is reused as `cached`, the
    // failed model reruns and fails again, under the same run id.
    let mut args = base.clone();
    args.extend(["run".into(), "--resume".into(), short.into()]);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(!output.status.success());
    let body = stdout(&output);
    assert!(body.contains(&format!("Run {short}")), "{body}");
    assert!(body.contains("cached"), "{body}");
    assert_eq!(last_run_id(&dir), run_id, "resume keeps the run id");

    // Materialise the source, resume again: the run goes green.
    heal_workspace(&dir);
    let output = run_unchecked(&args.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(output.status.success(), "{}", stdout(&output));
    let body = stdout(&output);
    assert!(body.contains(&format!("Run {short}  PASSED")), "{body}");
    assert_eq!(last_run_id(&dir), run_id);
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
