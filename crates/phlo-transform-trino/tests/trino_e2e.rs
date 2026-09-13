//! Live Trino end-to-end tests.
//!
//! These are ignored by default because they require Docker and pull the
//! `trinodb/trino` image. CI runs them explicitly with `--ignored` on a
//! Docker-enabled runner.

use std::sync::Arc;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

use phlo_transform_core::{
    compile, ColumnTolerance, Compilation, Materialization, ModelId, ModelOrigin, Relation,
    Selection, SemanticModel, SemanticProject, SemanticTest, TestId, WorkspaceDefaults,
};
use phlo_transform_engine::{
    diff, Adapter, DiffPolicy, DiffRequest, DiffStrategy, ExecutionStatus, PlanOptions, Planner,
    RunOptions, Runner,
};
use phlo_transform_trino::{TrinoAdapter, TrinoConfig};

async fn start_trino() -> (testcontainers::ContainerAsync<GenericImage>, TrinoAdapter) {
    let container = GenericImage::new("trinodb/trino", "latest")
        .with_wait_for(WaitFor::healthcheck())
        .with_exposed_port(8080.tcp())
        .with_startup_timeout(Duration::from_secs(300))
        .start()
        .await
        .expect("trino container starts");

    let port = container
        .get_host_port_ipv4(8080)
        .await
        .expect("mapped port");
    let endpoint = format!("http://127.0.0.1:{port}");
    let adapter = TrinoAdapter::new(
        TrinoConfig::new(endpoint)
            .with_catalog("memory")
            .with_schema("default"),
    )
    .expect("adapter builds");
    (container, adapter)
}

fn fixture_project() -> Compilation {
    let mut raw = SemanticModel::in_memory(
        ModelId::parse("assay.raw").unwrap(),
        "select 1 as id, 10 as value union all select 2, 20",
    );
    raw.config.materialization = Materialization::Table;

    let mut results = SemanticModel::in_memory(
        ModelId::parse("assay.results").unwrap(),
        "select * from assay.raw where value > 0",
    );
    results.config.materialization = Materialization::Table;

    let mut project = SemanticProject::in_memory(vec![raw, results]);
    project.defaults = WorkspaceDefaults {
        materialization: Materialization::Table,
        catalog: Some("memory".to_string()),
        schema: Some("default".to_string()),
    };
    project.tests = vec![SemanticTest {
        id: TestId::new("results_positive"),
        sql: "select * from assay.results where value < 0".to_string(),
        origin: ModelOrigin::in_memory(),
    }];

    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    compilation
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn executes_models_and_tests_against_trino() {
    let (_container, adapter) = start_trino().await;
    let adapter = Arc::new(adapter);

    // The memory connector needs its schema created explicitly.
    adapter
        .execute("CREATE SCHEMA IF NOT EXISTS memory.default")
        .await
        .expect("schema created");

    let compilation = fixture_project();
    let selected = Selection::all(&compilation);
    let plan = Planner::new(adapter.clone(), None)
        .plan(
            &compilation,
            &selected,
            Some("ci".to_string()),
            &PlanOptions::default(),
        )
        .await
        .expect("plan succeeds");
    assert!(!plan.blocked);
    assert_eq!(plan.model_count(), 2);
    assert_eq!(plan.test_count(), 1);

    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                environment: Some("ci".to_string()),
                concurrency: 2,
                run_tests: true,
                ..Default::default()
            },
        )
        .await
        .expect("run succeeds");

    assert_eq!(result.status, ExecutionStatus::Passed, "{result:?}");
    assert!(result
        .models
        .iter()
        .all(|model| model.status == ExecutionStatus::Passed));
    assert_eq!(result.tests.len(), 1);
    assert_eq!(result.tests[0].status, ExecutionStatus::Passed);

    // Both relations now exist and the compiled SQL produced real data.
    let results = relation("memory", "default", "assay__results");
    let exists = adapter.relation_exists(&results).await.unwrap();
    assert!(exists, "target relation should exist");

    let count = adapter
        .execute("SELECT count(*) FROM memory.default.assay__results")
        .await
        .expect("count query");
    assert_eq!(count.row_count, 1);
    assert_eq!(count.rows[0][0], "2");

    // Inferred schema matches the real Trino relation schema.
    let model = compilation
        .model(&ModelId::parse("assay.results").unwrap())
        .expect("model exists");
    assert!(model.schema.known, "{:?}", model.limitations);
    let inferred: Vec<String> = model
        .schema
        .columns
        .iter()
        .map(|column| column.name.clone())
        .collect();
    let real = adapter
        .relation_columns(&results)
        .await
        .expect("describe relation");
    let real_names: Vec<String> = real.iter().map(|column| column.name.clone()).collect();
    assert_eq!(
        inferred, real_names,
        "inferred={inferred:?} real={real_names:?}"
    );

    // A passing test returns no rows.
    let passing = adapter
        .execute("SELECT * FROM memory.default.assay__results WHERE value < 0")
        .await
        .unwrap();
    assert_eq!(passing.row_count, 0);
}

fn relation(catalog: &str, schema: &str, table: &str) -> phlo_transform_core::Relation {
    phlo_transform_core::Relation {
        catalog: Some(catalog.to_string()),
        schema: schema.to_string(),
        table: table.to_string(),
    }
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn computes_keyed_data_diff_against_trino() {
    let (_container, adapter) = start_trino().await;
    let adapter = Arc::new(adapter);
    adapter
        .execute("CREATE SCHEMA IF NOT EXISTS memory.default")
        .await
        .expect("schema created");

    let tables = [
        (
            "diff_base",
            "select 1 as id, 10 as value \
             union all select 2, 20 union all select 3, 30",
        ),
        (
            "diff_candidate",
            "select 1 as id, 10 as value \
             union all select 2, 25 union all select 4, 40",
        ),
    ];
    for (name, sql) in tables {
        let _ = adapter
            .execute(&format!("DROP TABLE IF EXISTS memory.default.{name}"))
            .await;
        adapter
            .execute(&format!("CREATE TABLE memory.default.{name} AS {sql}"))
            .await
            .expect("table created");
    }

    let relation = |table: &str| Relation {
        catalog: Some("memory".to_string()),
        schema: "default".to_string(),
        table: table.to_string(),
    };
    let request = DiffRequest {
        model: "assay.results".to_string(),
        candidate_relation: relation("diff_candidate"),
        base_relation: relation("diff_base"),
        candidate_ref: Some("candidate".to_string()),
        base_ref: Some("base".to_string()),
        candidate_version: None,
        base_version: None,
        key_columns: vec!["id".to_string()],
        columns: vec!["value".to_string()],
        strategy: DiffStrategy::Keyed,
        policy: DiffPolicy::default(),
        sample_fraction: None,
        renames: Default::default(),
    };

    let report = diff(adapter, &request).await.expect("diff runs");
    assert_eq!(report.row_summary.base_rows, 3);
    assert_eq!(report.row_summary.candidate_rows, 3);
    assert_eq!(report.row_summary.added, 1);
    assert_eq!(report.row_summary.removed, 1);
    assert_eq!(report.row_summary.modified, 1);
    assert_eq!(report.row_summary.unchanged, 1);
    assert_eq!(report.column_changes.get("value"), Some(&1));
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn diff_supports_partitions_tolerances_schema_and_sampling() {
    let (_container, adapter) = start_trino().await;
    let adapter = Arc::new(adapter);
    adapter
        .execute("CREATE SCHEMA IF NOT EXISTS memory.default")
        .await
        .expect("schema");

    for (name, sql) in [
        (
            "diff_base",
            "select 1 as id, 10.0 as value, DATE '2026-09-09' as d \
             union all select 2, 20.0, DATE '2026-09-10'",
        ),
        (
            "diff_candidate",
            "select 1 as id, 10.0000001 as value, DATE '2026-09-09' as d \
             union all select 3, 30.0, DATE '2026-09-11'",
        ),
    ] {
        let _ = adapter
            .execute(&format!("DROP TABLE IF EXISTS memory.default.{name}"))
            .await;
        adapter
            .execute(&format!("CREATE TABLE memory.default.{name} AS {sql}"))
            .await
            .expect("create");
    }

    let relation = |table: &str| Relation {
        catalog: Some("memory".to_string()),
        schema: "default".to_string(),
        table: table.to_string(),
    };
    let request = |strategy: DiffStrategy, policy: DiffPolicy| DiffRequest {
        model: "assay.results".to_string(),
        candidate_relation: relation("diff_candidate"),
        base_relation: relation("diff_base"),
        candidate_ref: None,
        base_ref: None,
        candidate_version: None,
        base_version: None,
        key_columns: vec!["id".to_string()],
        columns: vec!["value".to_string()],
        strategy,
        policy,
        sample_fraction: None,
        renames: Default::default(),
    };

    // Tolerance: the tiny difference is within bounds.
    let mut policy = DiffPolicy::default();
    policy.tolerances.insert(
        "value".to_string(),
        ColumnTolerance {
            absolute: Some(0.001),
            relative: None,
        },
    );
    let report = diff(adapter.clone(), &request(DiffStrategy::Keyed, policy))
        .await
        .expect("tolerance diff");
    assert_eq!(report.row_summary.modified, 0);

    // Without tolerance the same difference counts as modified.
    let report = diff(
        adapter.clone(),
        &request(DiffStrategy::Keyed, DiffPolicy::default()),
    )
    .await
    .expect("strict diff");
    assert_eq!(report.row_summary.modified, 1);

    // Partition-aware: one partition removed, one added.
    let report = diff(
        adapter.clone(),
        &request(
            DiffStrategy::Partition {
                columns: vec!["d".to_string()],
            },
            DiffPolicy::default(),
        ),
    )
    .await
    .expect("partition diff");
    assert_eq!(report.partitions_added.len(), 1);
    assert_eq!(report.partitions_removed.len(), 1);

    // Sampling at 100% still executes and reports counts.
    let mut sampled = request(DiffStrategy::Sampled, DiffPolicy::default());
    sampled.sample_fraction = Some(1.0);
    let report = diff(adapter.clone(), &sampled).await.expect("sampled diff");
    assert!(report.coverage.contains("sampled"));
}
