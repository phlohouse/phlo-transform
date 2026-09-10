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
    compile, select_models, Compilation, Materialization, ModelId, ModelOrigin, SemanticModel,
    SemanticProject, SemanticTest, TestId, WorkspaceDefaults,
};
use phlo_transform_engine::{Adapter, ExecutionStatus, Planner, RunOptions, Runner};
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
    let selected = select_models(&compilation, &Default::default());
    let plan = Planner::new(adapter.clone())
        .plan(&compilation, &selected, Some("ci".to_string()))
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
                cancel: Default::default(),
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
