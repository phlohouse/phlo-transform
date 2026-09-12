//! Live Nessie + Trino/Iceberg Write-Audit-Publish end-to-end test.
//!
//! Starts a Nessie server and a Trino coordinator with dynamic catalogs on a
//! shared Docker network, provisions a candidate branch and a branch-scoped
//! Iceberg catalog, writes candidate data without touching `main`, audits it,
//! diffs it, and promotes it. Ignored by default; run in CI with `--ignored`.

use std::sync::Arc;
use std::time::Duration;

use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{GenericImage, ImageExt};

use phlo_transform_core::{
    compile, Compilation, Materialization, ModelId, ModelOrigin, Relation, Selection,
    SemanticModel, SemanticProject, SemanticTest, TestId, WorkspaceDefaults,
};
use phlo_transform_engine::{
    diff, ensure_environment, promote, Adapter, CatalogRequest, DiffPolicy, DiffRequest,
    DiffStrategy, EnvironmentSpec, ExecutionStatus, PlanOptions, Planner, PromotionRequest,
    RunOptions, Runner,
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

const WAREHOUSE: &str = "local:///tmp/phlo-warehouse";

fn relation(catalog: &str, table: &str) -> Relation {
    Relation {
        catalog: Some(catalog.to_string()),
        schema: "default".to_string(),
        table: table.to_string(),
    }
}

fn project(catalog: &str, value: i32) -> SemanticProject {
    let mut raw = SemanticModel::in_memory(
        ModelId::parse("assay.raw").unwrap(),
        format!("select 1 as id, {value} as value"),
    );
    raw.config.materialization = Materialization::Table;

    let mut results = SemanticModel::in_memory(
        ModelId::parse("assay.results").unwrap(),
        "select * from assay.raw",
    );
    results.config.materialization = Materialization::Table;

    let mut project = SemanticProject::in_memory(vec![raw, results]);
    project.defaults = WorkspaceDefaults {
        materialization: Materialization::Table,
        catalog: Some(catalog.to_string()),
        schema: Some("default".to_string()),
    };
    project.tests = vec![SemanticTest {
        id: TestId::new("results_positive"),
        sql: "select * from assay.results where value < 0".to_string(),
        origin: ModelOrigin::in_memory(),
    }];
    project
}

async fn apply(
    adapter: Arc<TrinoAdapter>,
    compilation: &Compilation,
    environment: &str,
) -> phlo_transform_engine::RunResult {
    let selected = Selection::all(compilation);
    let plan = Planner::new(adapter.clone(), None)
        .plan(
            compilation,
            &selected,
            Some(environment.to_string()),
            &PlanOptions::default(),
        )
        .await
        .expect("plan");
    Runner::new(adapter, None)
        .apply(
            compilation,
            &plan,
            &RunOptions {
                environment: Some(environment.to_string()),
                run_tests: true,
                ..Default::default()
            },
        )
        .await
        .expect("apply")
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn wap_candidate_on_nessie_branch_is_promoted() {
    let suffix = std::process::id();
    let network = format!("phlo-nessie-it-{suffix}");
    let nessie_name = format!("phlo-nessie-it-{suffix}");

    let nessie = GenericImage::new("ghcr.io/projectnessie/nessie", "latest")
        .with_wait_for(WaitFor::message_on_stdout(
            "Listening on: http://0.0.0.0:19120",
        ))
        .with_exposed_port(19120.tcp())
        .with_container_name(&nessie_name)
        .with_network(&network)
        .with_startup_timeout(Duration::from_secs(240))
        .start()
        .await
        .expect("nessie starts");

    let trino = GenericImage::new("trinodb/trino", "latest")
        .with_wait_for(WaitFor::healthcheck())
        .with_exposed_port(8080.tcp())
        .with_container_name(format!("phlo-trino-it-{suffix}"))
        .with_network(&network)
        .with_copy_to(
            "/etc/trino/config.properties",
            TRINO_CONFIG.as_bytes().to_vec(),
        )
        .with_startup_timeout(Duration::from_secs(300))
        .start()
        .await
        .expect("trino starts");

    let trino_port = trino.get_host_port_ipv4(8080).await.expect("trino port");
    let nessie_port = nessie.get_host_port_ipv4(19120).await.expect("nessie port");
    let nessie_internal = format!("http://{nessie_name}:19120");

    let adapter = Arc::new(
        TrinoAdapter::new(TrinoConfig::new(format!("http://127.0.0.1:{trino_port}")))
            .expect("adapter"),
    );
    let nessie_client =
        NessieRestClient::new(NessieConfig::new(format!("http://127.0.0.1:{nessie_port}")))
            .expect("nessie client");

    // Base catalog points at `main`.
    adapter
        .ensure_catalog(&CatalogRequest {
            catalog: "phlo_main".to_string(),
            reference: Some("main".to_string()),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
        })
        .await
        .expect("main catalog");

    // Seed base state on `main` before branching, so the candidate starts from
    // the audited base and promotion is a clean fast-forward-style merge.
    let base = compile(&project("phlo_main", 10));
    let base_run = apply(adapter.clone(), &base, "main").await;
    assert_eq!(base_run.status, ExecutionStatus::Passed);

    // Provision the candidate branch and its catalog from the seeded `main`.
    let setup = ensure_environment(
        &nessie_client,
        adapter.as_ref(),
        &EnvironmentSpec {
            base_ref: "main".to_string(),
            candidate_ref: "ci/pr-1".to_string(),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
            catalog: "phlo_ci_pr_1".to_string(),
        },
    )
    .await
    .expect("environment provisioned");
    assert!(setup.created_branch);

    // Write changed data to the candidate.
    let candidate = compile(&project("phlo_ci_pr_1", 25));
    let candidate_run = apply(adapter.clone(), &candidate, "ci/pr-1").await;
    assert_eq!(
        candidate_run.status,
        ExecutionStatus::Passed,
        "{:?}",
        candidate_run.models
    );

    // Main is unchanged; candidate has the new value.
    let candidate_value = adapter
        .execute("SELECT value FROM phlo_ci_pr_1.default.assay__results")
        .await
        .expect("candidate read");
    assert_eq!(candidate_value.rows[0][0], "25");
    let main_value = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("main read");
    assert_eq!(main_value.rows[0][0], "10");

    // Audit: data diff of candidate against base.
    let report = diff(
        adapter.clone(),
        &DiffRequest {
            model: "assay.results".to_string(),
            candidate_relation: relation("phlo_ci_pr_1", "assay__results"),
            base_relation: relation("phlo_main", "assay__results"),
            candidate_ref: Some("ci/pr-1".to_string()),
            base_ref: Some("main".to_string()),
            candidate_version: None,
            base_version: None,
            key_columns: vec!["id".to_string()],
            columns: vec!["value".to_string()],
            strategy: DiffStrategy::Keyed,
            policy: DiffPolicy::default(),
            sample_fraction: None,
        },
    )
    .await
    .expect("diff");
    assert_eq!(report.row_summary.modified, 1);
    assert!(report.passed);

    // Promote the audited candidate against the base it was planned from.
    let record = promote(
        &nessie_client,
        &PromotionRequest {
            candidate_ref: "ci/pr-1".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: Some(setup.candidate.hash.clone()),
            expected_target_hash: Some(setup.base.hash.clone()),
            plan_id: None,
            run_id: None,
            quality_gates_passed: true,
            diff_passed: Some(report.passed),
            require_diff: true,
            breaking_schema_changes: Vec::new(),
            allow_breaking_schema: false,
            dry_run: false,
            actor: None,
        },
    )
    .await
    .expect("promotion");
    assert!(record.merged);

    // Main now exposes the candidate data.
    let promoted = adapter
        .execute("SELECT value FROM phlo_main.default.assay__results")
        .await
        .expect("promoted read");
    assert_eq!(promoted.rows[0][0], "25");

    // Re-promoting the same candidate against the now-advanced target is
    // rejected: the audited base is no longer current.
    let stale = promote(
        &nessie_client,
        &PromotionRequest {
            candidate_ref: "ci/pr-1".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: Some(setup.candidate.hash.clone()),
            expected_target_hash: Some(setup.base.hash.clone()),
            plan_id: None,
            run_id: None,
            quality_gates_passed: true,
            diff_passed: Some(true),
            require_diff: true,
            breaking_schema_changes: Vec::new(),
            allow_breaking_schema: false,
            dry_run: false,
            actor: None,
        },
    )
    .await;
    assert!(stale.is_err(), "stale promotion must be rejected");

    // Iceberg snapshot state is observable and changes with the data.
    let state_before = adapter
        .source_state(&relation("phlo_main", "assay__results"))
        .await
        .expect("source state")
        .expect("snapshot id");
    adapter
        .execute("INSERT INTO phlo_main.default.assay__results VALUES (2, 99)")
        .await
        .expect("insert");
    let state_after = adapter
        .source_state(&relation("phlo_main", "assay__results"))
        .await
        .expect("source state")
        .expect("snapshot id");
    assert_ne!(state_before, state_after);
}
