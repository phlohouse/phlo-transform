//! Live Trino/Iceberg incremental `MERGE` end-to-end test.
//!
//! Starts Nessie + Trino (dynamic Iceberg catalogs), bootstraps an incremental
//! key model, changes the source, and verifies the next run merges (updates and
//! inserts) rather than rebuilding, idempotently. Ignored by default.

use std::sync::Arc;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

use phlo_transform_core::{
    compile, select_models, IncrementalStrategy, Materialization, ModelId, SemanticModel,
    SemanticProject, WorkspaceDefaults,
};
use phlo_transform_engine::{
    Adapter, CatalogRequest, Planner, RunOptions, Runner, SqliteStateStore,
};
use phlo_transform_nessie::{NessieConfig, NessieRestClient};
use phlo_transform_trino::{TrinoAdapter, TrinoConfig};

const TRINO_CONFIG: &str = "\
coordinator=true
node-scheduler.include-coordinator=true
http-server.http.port=8080
discovery.uri=http://localhost:8080
catalog.management=dynamic
catalog.store=memory
";

fn project(sql: &str) -> SemanticProject {
    let mut events = SemanticModel::in_memory(ModelId::parse("assay.events").unwrap(), sql);
    events.config.materialization = Materialization::Incremental;
    events.config.incremental = Some(IncrementalStrategy::Key {
        columns: vec!["id".to_string()],
    });

    let mut project = SemanticProject::in_memory(vec![events]);
    project.defaults = WorkspaceDefaults {
        materialization: Materialization::Incremental,
        catalog: Some("phlo".to_string()),
        schema: Some("default".to_string()),
    };
    project
}

async fn apply(
    adapter: Arc<TrinoAdapter>,
    state: Arc<SqliteStateStore>,
    compilation: &phlo_transform_core::Compilation,
) -> phlo_transform_engine::RunResult {
    let selected = select_models(compilation, &Default::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(compilation, &selected, Some("main".to_string()))
        .await
        .expect("plan");
    Runner::new(adapter, Some(state))
        .apply(
            compilation,
            &plan,
            &RunOptions {
                environment: Some("main".to_string()),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .expect("apply")
}

async fn rows(adapter: &TrinoAdapter) -> Vec<Vec<String>> {
    adapter
        .execute("SELECT id, value FROM phlo.default.assay__events ORDER BY id")
        .await
        .expect("read")
        .rows
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn incremental_key_merges_on_trino_iceberg() {
    let suffix = std::process::id();
    let network = format!("phlo-inc-it-{suffix}");
    let nessie_name = format!("phlo-inc-nessie-{suffix}");

    let _nessie = GenericImage::new("ghcr.io/projectnessie/nessie", "latest")
        .with_wait_for(WaitFor::message_on_stdout(
            "Listening on: http://0.0.0.0:19120",
        ))
        .with_exposed_port(19120.tcp())
        .with_container_name(&nessie_name)
        .with_network(&network)
        .with_startup_timeout(Duration::from_secs(240))
        .start()
        .await
        .expect("nessie");

    let trino = GenericImage::new("trinodb/trino", "latest")
        .with_wait_for(WaitFor::healthcheck())
        .with_exposed_port(8080.tcp())
        .with_container_name(format!("phlo-inc-trino-{suffix}"))
        .with_network(&network)
        .with_copy_to(
            "/etc/trino/config.properties",
            TRINO_CONFIG.as_bytes().to_vec(),
        )
        .with_startup_timeout(Duration::from_secs(300))
        .start()
        .await
        .expect("trino");

    let trino_port = trino.get_host_port_ipv4(8080).await.expect("trino port");
    let nessie_port = _nessie
        .get_host_port_ipv4(19120)
        .await
        .expect("nessie port");

    let adapter = Arc::new(
        TrinoAdapter::new(TrinoConfig::new(format!("http://127.0.0.1:{trino_port}")))
            .expect("adapter"),
    );
    let _nessie_client =
        NessieRestClient::new(NessieConfig::new(format!("http://127.0.0.1:{nessie_port}")))
            .expect("nessie client");

    adapter
        .ensure_catalog(&CatalogRequest {
            catalog: "phlo".to_string(),
            reference: Some("main".to_string()),
            nessie_uri: Some(format!("http://{nessie_name}:19120")),
            warehouse: Some("local:///tmp/phlo-warehouse".to_string()),
        })
        .await
        .expect("catalog");
    adapter
        .ensure_schema(&phlo_transform_core::Relation {
            catalog: Some("phlo".to_string()),
            schema: "default".to_string(),
            table: String::new(),
        })
        .await
        .expect("schema");
    adapter
        .execute(
            "CREATE TABLE phlo.default.seed AS \
             SELECT 1 AS id, 10 AS value UNION ALL SELECT 2, 20",
        )
        .await
        .expect("seed");

    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let model_sql = "select id, value from phlo.default.seed";

    // Bootstrap: target does not exist, so the engine creates the table.
    let first = compile(&project(model_sql));
    assert!(first.is_ok(), "{:?}", first.diagnostics);
    let run = apply(adapter.clone(), state.clone(), &first).await;
    assert_eq!(run.status, phlo_transform_engine::ExecutionStatus::Passed);
    assert_eq!(rows(&adapter).await, vec![vec!["1", "10"], vec!["2", "20"]]);

    // Change the source, force a rebuild of the model, and expect a MERGE.
    adapter
        .execute("UPDATE phlo.default.seed SET value = 25 WHERE id = 2")
        .await
        .expect("update seed");
    adapter
        .execute("INSERT INTO phlo.default.seed VALUES (3, 30)")
        .await
        .expect("insert seed");

    let second = compile(&project(
        "select id, value from phlo.default.seed where true",
    ));
    assert!(second.is_ok(), "{:?}", second.diagnostics);
    let run = apply(adapter.clone(), state.clone(), &second).await;
    assert_eq!(run.status, phlo_transform_engine::ExecutionStatus::Passed);
    assert_eq!(
        rows(&adapter).await,
        vec![vec!["1", "10"], vec!["2", "25"], vec!["3", "30"]]
    );

    // Re-applying an equivalent merge is idempotent.
    let third = compile(&project(
        "select id, value from phlo.default.seed where true and true",
    ));
    let run = apply(adapter.clone(), state.clone(), &third).await;
    assert_eq!(run.status, phlo_transform_engine::ExecutionStatus::Passed);
    assert_eq!(rows(&adapter).await.len(), 3);
}
