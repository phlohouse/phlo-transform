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
    branch_diff, catalog_name, diff, ensure_environment, evaluate_gates, promote, Adapter,
    BranchDiffRequest, CatalogRequest, CatalogStatus, DatasetStatus, DiffPolicy, DiffRequest,
    DiffStrategy, EnvironmentSpec, ExecutionStatus, GateInput, PlanAction, PlanOptions, Planner,
    PromotionEvidenceIds, PromotionRequest, RunOptions, Runner, SqliteStateStore, StateStore,
};
use phlo_transform_nessie::{NessieClient, NessieConfig, NessieRestClient};
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
    state: Option<Arc<dyn phlo_transform_engine::StateStore>>,
) -> phlo_transform_engine::RunResult {
    let selected = Selection::all(compilation);
    let plan = Planner::new(adapter.clone(), state.clone())
        .plan(
            compilation,
            &selected,
            Some(environment.to_string()),
            &PlanOptions::default(),
        )
        .await
        .expect("plan");
    Runner::new(adapter, state)
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

/// A running Nessie + Trino pair wired for dynamic Iceberg catalogs. The
/// containers stay alive for the test that created them and are dropped
/// with the fixture.
struct Infra {
    _nessie: testcontainers::ContainerAsync<GenericImage>,
    _trino: testcontainers::ContainerAsync<GenericImage>,
    adapter: Arc<TrinoAdapter>,
    nessie_client: Arc<NessieRestClient>,
    nessie_internal: String,
}

async fn start_infra(tag: &str) -> Infra {
    let suffix = format!("{}-{}", tag, std::process::id());
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

    Infra {
        _nessie: nessie,
        _trino: trino,
        adapter: Arc::new(
            TrinoAdapter::new(TrinoConfig::new(format!("http://127.0.0.1:{trino_port}")))
                .expect("adapter"),
        ),
        nessie_client: Arc::new(
            NessieRestClient::new(NessieConfig::new(format!("http://127.0.0.1:{nessie_port}")))
                .expect("nessie client"),
        ),
        nessie_internal: format!("http://{nessie_name}:19120"),
    }
}

#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn wap_candidate_on_nessie_branch_is_promoted() {
    let infra = start_infra("wap").await;
    let adapter = infra.adapter.clone();
    let nessie_client = infra.nessie_client.clone();
    let nessie_internal = infra.nessie_internal.clone();

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

    // Runs record against a real state store so promotion gates evaluate
    // stored evidence — not hand-constructed inputs.
    let state: Arc<dyn StateStore> = Arc::new(SqliteStateStore::in_memory().expect("state"));

    // Seed base state on `main` before branching, so the candidate starts from
    // the audited base and promotion is a clean fast-forward-style merge.
    let base = compile(&project("phlo_main", 10));
    let base_run = apply(adapter.clone(), &base, "main", Some(state.clone())).await;
    assert_eq!(base_run.status, ExecutionStatus::Passed);

    // Provision the candidate branch and its catalog from the seeded `main`.
    // `catalog: None` resolves the generated convention — a readable prefix
    // plus the ref's hash, so `ci/pr-1` and `ci_pr_1` can never share one
    // physical catalog.
    let setup = ensure_environment(
        nessie_client.as_ref(),
        adapter.as_ref(),
        &EnvironmentSpec {
            base_ref: "main".to_string(),
            candidate_ref: "ci/pr-1".to_string(),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
            catalog: None,
        },
    )
    .await
    .expect("environment provisioned");
    assert!(setup.created_branch);
    assert_eq!(setup.catalog_status, CatalogStatus::Created);
    assert_eq!(setup.catalog, catalog_name("ci/pr-1"));
    assert_ne!(
        setup.catalog,
        catalog_name("ci_pr_1"),
        "punctuation-equivalent refs must not collide on one catalog"
    );
    // The branch was just created: its cut-from provenance is provable.
    assert_eq!(
        setup.created_from.as_ref().map(|base| base.name.as_str()),
        Some("main")
    );

    // Write changed data to the candidate.
    let candidate_catalog = setup.catalog.clone();
    let candidate = compile(&project(&candidate_catalog, 25));
    let candidate_run = apply(adapter.clone(), &candidate, "ci/pr-1", Some(state.clone())).await;
    assert_eq!(
        candidate_run.status,
        ExecutionStatus::Passed,
        "{:?}",
        candidate_run.models
    );

    // Orchestration binding: a passed run is bound to the branch's post-run
    // head — the commit its writes produced, not the provisioning snapshot.
    // The stored run is what promotion later evaluates.
    let post_run_head = nessie_client
        .get_reference("ci/pr-1")
        .await
        .expect("candidate")
        .expect("candidate exists")
        .hash;
    state
        .bind_run_reference_hash(&candidate_run.run_id, &post_run_head)
        .expect("bind run reference");
    let stored_run = state
        .latest_run(Some("ci/pr-1"))
        .expect("latest run")
        .expect("run recorded");
    assert_eq!(
        stored_run.reference_hash.as_deref(),
        Some(post_run_head.as_str()),
        "the stored run must carry the post-run candidate head"
    );

    // Main is unchanged; candidate has the new value.
    let candidate_value = adapter
        .execute(&format!(
            "SELECT value FROM {candidate_catalog}.default.assay__results"
        ))
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
            candidate_relation: relation(&candidate_catalog, "assay__results"),
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
            renames: Default::default(),
        },
    )
    .await
    .expect("diff");
    assert_eq!(report.row_summary.modified, 1);
    assert!(report.passed);

    // Branch-level diff: the candidate rewrote both models — snapshot ids
    // differ from the base's even without shared state records. The resolved
    // heads bind the report to the exact commits it audited.
    let candidate_head = nessie_client
        .get_reference("ci/pr-1")
        .await
        .expect("candidate")
        .expect("candidate exists")
        .hash;
    let base_head = nessie_client
        .get_reference("main")
        .await
        .expect("base")
        .expect("main exists")
        .hash;
    let branch = branch_diff(
        adapter.clone(),
        None,
        &candidate,
        &BranchDiffRequest {
            candidate_ref: "ci/pr-1".to_string(),
            base_ref: "main".to_string(),
            candidate_catalog: Some(candidate_catalog.clone()),
            base_catalog: Some("phlo_main".to_string()),
            deep: false,
            default_schema: None,
            candidate_hash: Some(candidate_head.clone()),
            base_hash: Some(base_head.clone()),
        },
    )
    .await
    .expect("branch diff");
    let status = |name: &str| {
        branch
            .datasets
            .iter()
            .find(|dataset| dataset.dataset == name)
            .map(|dataset| dataset.status)
            .unwrap_or_else(|| panic!("dataset {name} missing from branch diff"))
    };
    assert_eq!(status("assay.raw"), DatasetStatus::Changed);
    assert_eq!(status("assay.results"), DatasetStatus::Changed);
    // Row counts come straight from the catalogs.
    let rows = branch
        .rows
        .iter()
        .find(|row| row.dataset == "assay.results")
        .expect("assay.results row diff");
    assert_eq!(rows.base_rows, Some(1));
    assert_eq!(rows.candidate_rows, Some(1));

    // Gate evaluation against real Nessie state and the stored run record:
    // the run gate sees the run bound to the head being promoted, the base
    // hash matches the provisioned target, and the merge check is clean.
    let target_now = nessie_client
        .get_reference("main")
        .await
        .expect("target")
        .expect("main exists");
    let gates = evaluate_gates(&GateInput {
        run: state.latest_run(Some("ci/pr-1")).expect("latest run"),
        model_runs: state.model_runs(&stored_run.run_id).expect("model runs"),
        seed_runs: state.seed_runs(&stored_run.run_id).expect("seed runs"),
        test_runs: state.test_runs(&stored_run.run_id).expect("test runs"),
        require_diff: true,
        diff_passed: Some(report.passed),
        expected_target_hash: Some(setup.base.hash.clone()),
        actual_target_hash: Some(target_now.hash.clone()),
        actual_candidate_hash: Some(candidate_head.clone()),
        schema_audited: true,
        merge_check: Some(
            nessie_client
                .can_merge("ci/pr-1", "main")
                .await
                .expect("merge check"),
        ),
        ..Default::default()
    });
    let gate = |name: &str| {
        gates
            .results
            .iter()
            .find(|result| result.name == name)
            .unwrap_or_else(|| panic!("gate {name} missing"))
    };
    assert!(gate("base").passed, "{}", gate("base").detail);
    assert!(gate("data_diff").passed);
    assert!(gate("conflicts").passed, "{}", gate("conflicts").detail);
    // The stored run is bound to the head being promoted — the run gate now
    // passes on real evidence, not a constructed input.
    assert!(gate("run").passed, "{}", gate("run").detail);
    assert!(gate("tests").passed, "{}", gate("tests").detail);
    assert!(gate("blocked").passed, "{}", gate("blocked").detail);
    assert!(gate("schema").passed, "{}", gate("schema").detail);
    assert!(gates.passed, "every gate has real evidence and passes");

    // Reference management: both refs resolve, sorted by name.
    let refs = nessie_client.list_references().await.expect("list refs");
    let names: Vec<&str> = refs
        .iter()
        .map(|reference| reference.name.as_str())
        .collect();
    assert!(names.contains(&"ci/pr-1"), "{names:?}");
    assert!(names.contains(&"main"), "{names:?}");

    // Promote the audited candidate against the base it was planned from.
    // `candidate_hash` pins the head the gates were evaluated against —
    // `setup.candidate.hash` is the provisioning-time hash, which the run's
    // commits have already advanced.
    let audited_candidate = nessie_client
        .get_reference("ci/pr-1")
        .await
        .expect("candidate")
        .expect("candidate exists")
        .hash;
    let record = promote(
        nessie_client.as_ref(),
        &PromotionRequest {
            candidate_ref: "ci/pr-1".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: Some(audited_candidate.clone()),
            expected_target_hash: Some(setup.base.hash.clone()),
            plan_id: Some(stored_run.plan_id.clone()),
            run_id: Some(stored_run.run_id.clone()),
            quality_gates_passed: true,
            diff_passed: Some(report.passed),
            require_diff: true,
            breaking_schema_changes: Vec::new(),
            allow_breaking_schema: false,
            dry_run: false,
            actor: None,
            gates: Vec::new(),
            evidence: PromotionEvidenceIds::default(),
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
    // rejected: the audited base is no longer current. The candidate hash
    // still matches (the merge did not move it), so the stale-target check
    // is what fires.
    let stale = promote(
        nessie_client.as_ref(),
        &PromotionRequest {
            candidate_ref: "ci/pr-1".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: Some(audited_candidate),
            expected_target_hash: Some(setup.base.hash.clone()),
            plan_id: Some(stored_run.plan_id.clone()),
            run_id: Some(stored_run.run_id.clone()),
            quality_gates_passed: true,
            diff_passed: Some(true),
            require_diff: true,
            breaking_schema_changes: Vec::new(),
            allow_breaking_schema: false,
            dry_run: false,
            actor: None,
            gates: Vec::new(),
            evidence: PromotionEvidenceIds::default(),
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

    // Cleanup: deleting the promoted branch removes it from the ref list.
    nessie_client
        .delete_branch("ci/pr-1")
        .await
        .expect("delete candidate");
    let refs = nessie_client.list_references().await.expect("list refs");
    assert!(!refs.iter().any(|reference| reference.name == "ci/pr-1"));
}

/// The cross-environment cache contract end to end: a candidate branch
/// inherits main's Iceberg tables, so identical content must plan `Cached`,
/// adopt the source materialisation into the candidate's state without
/// issuing model SQL, and fall back to `Build` the moment the inherited
/// table's identity drifts.
#[tokio::test]
#[ignore = "requires Docker; run with --ignored"]
async fn candidate_reuses_inherited_materialisations_without_rebuilding() {
    let infra = start_infra("cache").await;
    let adapter = infra.adapter.clone();
    let nessie_client = infra.nessie_client.clone();
    let nessie_internal = infra.nessie_internal.clone();

    adapter
        .ensure_catalog(&CatalogRequest {
            catalog: "phlo_main".to_string(),
            reference: Some("main".to_string()),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
        })
        .await
        .expect("main catalog");

    let state: Arc<dyn StateStore> = Arc::new(SqliteStateStore::in_memory().expect("state"));

    // Materialise both models on main: phlo_main.default.{raw,results}.
    let base = compile(&project("phlo_main", 10));
    let base_run = apply(adapter.clone(), &base, "main", Some(state.clone())).await;
    assert_eq!(base_run.status, ExecutionStatus::Passed);
    let main_results = state
        .materialized_version("assay.results", Some("main"))
        .expect("main record")
        .expect("assay.results recorded on main");

    // A fresh candidate inherits main's tables untouched.
    let setup = ensure_environment(
        nessie_client.as_ref(),
        adapter.as_ref(),
        &EnvironmentSpec {
            base_ref: "main".to_string(),
            candidate_ref: "ci/reuse".to_string(),
            nessie_uri: Some(nessie_internal.clone()),
            warehouse: Some(WAREHOUSE.to_string()),
            catalog: None,
        },
    )
    .await
    .expect("environment provisioned");
    assert_eq!(setup.catalog_status, CatalogStatus::Created);
    let candidate_catalog = setup.catalog.clone();

    // Identical content compiled against the candidate's own catalog: the
    // version hash is catalog-independent, so the planner can match it to
    // main's records.
    let candidate = compile(&project(&candidate_catalog, 10));
    for model in &candidate.models {
        let recorded = state
            .materialized_version(&model.id.logical_name(), Some("main"))
            .expect("main record")
            .expect("recorded on main");
        assert_eq!(
            model.version.hash, recorded.version.hash,
            "{} must hash identically across catalogs",
            model.id
        );
    }

    let selected = Selection::all(&candidate);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &candidate,
            &selected,
            Some("ci/reuse".to_string()),
            &PlanOptions::default(),
        )
        .await
        .expect("candidate plan");
    assert_eq!(plan.models.len(), 2);
    for model in &plan.models {
        assert_eq!(
            model.action,
            PlanAction::Cached,
            "{} must plan Cached, got {:?}: {:?}",
            model.id,
            model.action,
            model.reasons
        );
        let source = state
            .materialized_version(&model.id, Some("main"))
            .expect("main record")
            .expect("recorded on main");
        let reuse = model.reuse.as_ref().expect("cache source recorded");
        assert_eq!(reuse.environment.as_deref(), Some("main"));
        assert_eq!(reuse.run_id, source.run_id);
        assert_eq!(
            reuse.output_identity,
            source.output_identity.clone().expect("identity")
        );
    }

    // Executing the cached plan issues no model SQL — it records the
    // adoption against the candidate environment.
    let run = Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &candidate,
            &plan,
            &RunOptions {
                environment: Some("ci/reuse".to_string()),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .expect("cached run");
    assert_eq!(run.status, ExecutionStatus::Passed);
    assert_eq!(run.counts.passed, 0, "a cache hit never builds: {run:?}");
    assert_eq!(run.counts.cached, 2);
    for result in &run.models {
        assert_eq!(result.status, ExecutionStatus::Cached, "{result:?}");
        assert!(result.query_id.is_none(), "no SQL was issued: {result:?}");
        assert_eq!(result.action, "cached");
    }

    // The candidate environment now owns records carrying the producing
    // run's provenance — not fabricated timestamps.
    let adopted = state
        .materialized_version("assay.results", Some("ci/reuse"))
        .expect("candidate record")
        .expect("assay.results adopted into ci/reuse");
    assert_eq!(adopted.run_id, main_results.run_id);
    assert_eq!(adopted.materialized_at, main_results.materialized_at);
    assert_eq!(adopted.output_identity, main_results.output_identity);
    assert_eq!(
        adopted.target,
        format!("{candidate_catalog}.default.assay__results")
    );

    // The inherited table is genuinely readable through the candidate
    // catalog — adoption claimed a real, visible output.
    let rows = adapter
        .execute(&format!(
            "SELECT value FROM {candidate_catalog}.default.assay__results"
        ))
        .await
        .expect("candidate read");
    assert_eq!(rows.rows[0][0], "10");

    // Drift: rewriting the inherited table on the candidate branch moves
    // its snapshot. The untouched model now reads as a plain Skip — the
    // adoption above left an environment-local record that still verifies —
    // while the rewritten table's recorded identity no longer matches live,
    // so it must rebuild.
    adapter
        .execute(&format!(
            "INSERT INTO {candidate_catalog}.default.assay__results VALUES (2, 77)"
        ))
        .await
        .expect("candidate-side rewrite");
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &candidate,
            &selected,
            Some("ci/reuse".to_string()),
            &PlanOptions::default(),
        )
        .await
        .expect("post-drift plan");
    let action = |id: &str| {
        plan.models
            .iter()
            .find(|model| model.id == id)
            .map(|model| model.action)
            .unwrap_or_else(|| panic!("{id} missing from plan"))
    };
    assert_eq!(action("assay.raw"), PlanAction::Skip);
    assert_eq!(
        action("assay.results"),
        PlanAction::Build,
        "an externally rewritten table invalidates the adoption"
    );
    let run = Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &candidate,
            &plan,
            &RunOptions {
                environment: Some("ci/reuse".to_string()),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .expect("post-drift run");
    assert_eq!(run.status, ExecutionStatus::Passed);
    assert_eq!(run.counts.passed, 1);
    assert_eq!(run.counts.skipped, 1);
    let rows = adapter
        .execute(&format!(
            "SELECT COUNT(*) FROM {candidate_catalog}.default.assay__results"
        ))
        .await
        .expect("candidate recount");
    assert_eq!(
        rows.rows[0][0], "1",
        "the rebuild replaced the drifted table"
    );

    nessie_client
        .delete_branch("ci/reuse")
        .await
        .expect("delete candidate");
}
