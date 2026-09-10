//! Engine tests using a fake adapter.
//!
//! These verify planning, dependency-ordered execution, bounded concurrency,
//! partial-failure blocking, tests and state persistence without a live
//! warehouse.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use phlo_transform_core::{
    compile, select_models, Compilation, IncrementalStrategy, Materialization, ModelId,
    ModelOrigin, Relation, SelectionOptions, SemanticModel, SemanticProject, SemanticTest, TestId,
};
use phlo_transform_engine::{
    Adapter, AdapterError, ArtifactWriter, CancelHandle, CatalogRequest, ChangeReason, ColumnInfo,
    ExecutionStatus, Plan, PlanAction, Planner, QueryResult, RunOptions, Runner, SqliteStateStore,
    StateStore,
};

#[derive(Default)]
struct FakeAdapter {
    existing: Mutex<BTreeSet<String>>,
    fail_targets: Mutex<BTreeSet<String>>,
    created: Mutex<Vec<String>>,
    appends: Mutex<Vec<String>>,
    merges: Mutex<Vec<String>>,
    test_rows: Mutex<u64>,
    delay_ms: u64,
    current: AtomicUsize,
    max_concurrent: AtomicUsize,
}

impl FakeAdapter {
    fn with_delay(delay_ms: u64) -> Self {
        Self {
            delay_ms,
            ..Default::default()
        }
    }

    fn failing(target: &str) -> Self {
        let adapter = Self::default();
        adapter
            .fail_targets
            .lock()
            .unwrap()
            .insert(target.to_string());
        adapter
    }

    fn set_test_rows(&self, rows: u64) {
        *self.test_rows.lock().unwrap() = rows;
    }

    async fn create(
        &self,
        relation: &Relation,
        query_id: &str,
    ) -> Result<QueryResult, AdapterError> {
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now, Ordering::SeqCst);
        if self.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
        }
        self.current.fetch_sub(1, Ordering::SeqCst);

        let display = relation.display();
        if self.fail_targets.lock().unwrap().contains(&display) {
            return Err(AdapterError::new(
                "FAKE001",
                format!("simulated failure for {display}"),
            ));
        }
        self.created.lock().unwrap().push(display.clone());
        self.existing.lock().unwrap().insert(display);
        Ok(QueryResult {
            query_id: Some(query_id.to_string()),
            ..Default::default()
        })
    }
}

#[async_trait]
impl Adapter for FakeAdapter {
    fn name(&self) -> &str {
        "fake"
    }

    async fn relation_exists(&self, relation: &Relation) -> Result<bool, AdapterError> {
        Ok(self.existing.lock().unwrap().contains(&relation.display()))
    }

    async fn execute(&self, _sql: &str) -> Result<QueryResult, AdapterError> {
        let rows = *self.test_rows.lock().unwrap();
        Ok(QueryResult {
            query_id: Some("test-query".to_string()),
            row_count: rows,
            ..Default::default()
        })
    }

    async fn create_or_replace_view(
        &self,
        relation: &Relation,
        _sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        self.create(relation, "view-query").await
    }

    async fn create_or_replace_table(
        &self,
        relation: &Relation,
        _sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        self.create(relation, "table-query").await
    }

    async fn append(&self, relation: &Relation, _sql: &str) -> Result<QueryResult, AdapterError> {
        let display = relation.display();
        self.appends.lock().unwrap().push(display.clone());
        self.existing.lock().unwrap().insert(display);
        Ok(QueryResult {
            query_id: Some("append-query".to_string()),
            ..Default::default()
        })
    }

    async fn merge(
        &self,
        relation: &Relation,
        _key_columns: &[String],
        _sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let display = relation.display();
        self.merges.lock().unwrap().push(display.clone());
        self.existing.lock().unwrap().insert(display);
        Ok(QueryResult {
            query_id: Some("merge-query".to_string()),
            ..Default::default()
        })
    }

    async fn cancel(&self, _query_id: &str) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn relation_columns(
        &self,
        _relation: &Relation,
    ) -> Result<Vec<ColumnInfo>, AdapterError> {
        Ok(Vec::new())
    }

    async fn ensure_catalog(&self, _request: &CatalogRequest) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn ensure_schema(&self, _relation: &Relation) -> Result<(), AdapterError> {
        Ok(())
    }
}

fn model(name: &str, sql: &str) -> SemanticModel {
    SemanticModel::in_memory(ModelId::parse(name).unwrap(), sql)
}

fn project_with_tests() -> Compilation {
    let mut project = SemanticProject::in_memory(vec![
        model("assay.raw", "select * from external.raw_assay_results"),
        model("assay.results", "select * from assay.raw"),
        model("reporting.monthly", "select * from assay.results"),
        model("analytics.d", "select * from external.d"),
        model("analytics.e", "select * from analytics.d"),
    ]);
    project.tests = vec![
        SemanticTest {
            id: TestId::new("results_positive"),
            sql: "select * from assay.results where result < 0".to_string(),
            origin: ModelOrigin::in_memory(),
        },
        SemanticTest {
            id: TestId::new("raw_not_null"),
            sql: "select * from assay.raw where sample_id is null".to_string(),
            origin: ModelOrigin::in_memory(),
        },
    ];
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    compilation
}

async fn plan_all(compilation: &Compilation, adapter: Arc<FakeAdapter>) -> Plan {
    let selected = select_models(compilation, &SelectionOptions::default());
    Planner::new(adapter, None)
        .plan(compilation, &selected, None)
        .await
        .expect("plan succeeds")
}

#[tokio::test]
async fn executes_in_dependency_order_and_runs_tests() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let plan = plan_all(&compilation, adapter.clone()).await;
    assert!(!plan.blocked);
    assert_eq!(plan.model_count(), 5);
    assert_eq!(plan.test_count(), 2);

    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .expect("run succeeds");

    assert_eq!(result.status, ExecutionStatus::Passed);
    assert!(result
        .models
        .iter()
        .all(|model| model.status == ExecutionStatus::Passed));
    assert_eq!(result.tests.len(), 2);
    assert!(result
        .tests
        .iter()
        .all(|test| test.status == ExecutionStatus::Passed));

    // Dependencies are created before dependents.
    let created = adapter.created.lock().unwrap().clone();
    let position = |needle: &str| created.iter().position(|value| value == needle).unwrap();
    assert!(position("assay.raw") < position("assay.results"));
    assert!(position("assay.results") < position("reporting.monthly"));
    assert!(position("analytics.d") < position("analytics.e"));
}

#[tokio::test]
async fn upstream_failure_blocks_dependents_but_not_independent_branches() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::failing("assay.results"));
    let plan = plan_all(&compilation, adapter.clone()).await;

    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .expect("run completes");

    let status = |name: &str| {
        result
            .models
            .iter()
            .find(|model| model.model == name)
            .map(|model| model.status)
            .unwrap()
    };
    assert_eq!(status("assay.raw"), ExecutionStatus::Passed);
    assert_eq!(status("assay.results"), ExecutionStatus::Failed);
    assert_eq!(status("reporting.monthly"), ExecutionStatus::Blocked);
    assert_eq!(status("analytics.d"), ExecutionStatus::Passed);
    assert_eq!(status("analytics.e"), ExecutionStatus::Passed);
    assert_eq!(result.status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn failing_tests_fail_the_run() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(1);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result
        .tests
        .iter()
        .all(|test| test.status == ExecutionStatus::Failed));
}

#[tokio::test]
async fn bounded_concurrency_is_respected() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::with_delay(30));
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                concurrency: 2,
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        adapter.max_concurrent.load(Ordering::SeqCst) <= 2,
        "max concurrency was {}",
        adapter.max_concurrent.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn persists_run_history_and_writes_artifacts() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let plan = plan_all(&compilation, adapter.clone()).await;

    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .unwrap();

    let runs = state.runs().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run_id, result.run_id);

    let directory = tempfile::tempdir().unwrap();
    let writer = ArtifactWriter::new(directory.path());
    writer.write_project(&compilation).unwrap();
    writer.write_plan(&plan).unwrap();
    writer.write_run(&result).unwrap();
    for name in [
        "manifest.json",
        "graph.json",
        "lineage.json",
        "plan.json",
        "run.json",
    ] {
        assert!(
            directory.path().join(name).is_file(),
            "missing artifact {name}"
        );
    }
}

#[tokio::test]
async fn cancellation_marks_unfinished_models_cancelled() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::with_delay(80));
    let plan = plan_all(&compilation, adapter.clone()).await;

    let cancel = CancelHandle::default();
    let cancel_signal = cancel.clone();
    let runner = Runner::new(adapter.clone(), None);
    let options = RunOptions {
        concurrency: 1,
        run_tests: false,
        cancel: cancel.clone(),
        ..Default::default()
    };

    let handle = tokio::spawn(async move { runner.apply(&compilation, &plan, &options).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    cancel_signal.cancel();

    let result = handle.await.unwrap().unwrap();
    assert_eq!(result.status, ExecutionStatus::Cancelled);
    assert!(
        result
            .models
            .iter()
            .any(|model| model.status == ExecutionStatus::Cancelled),
        "{:?}",
        result.models
    );
    assert!(
        result
            .tests
            .iter()
            .all(|test| test.status != ExecutionStatus::Passed),
        "tests should not pass after cancellation"
    );
}

#[tokio::test]
async fn state_aware_second_run_skips_unchanged_models() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let selected = select_models(&compilation, &SelectionOptions::default());
    let environment = Some("dev".to_string());

    let first = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, environment.clone())
        .await
        .unwrap();
    assert!(
        first
            .models
            .iter()
            .all(|model| model.action == PlanAction::Build),
        "{:?}",
        first.models
    );

    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let run = runner
        .apply(
            &compilation,
            &first,
            &RunOptions {
                environment: environment.clone(),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(run.status, ExecutionStatus::Passed);

    let second = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, environment.clone())
        .await
        .unwrap();
    assert!(
        second
            .models
            .iter()
            .all(|model| model.action == PlanAction::Skip),
        "{:?}",
        second.models
    );

    let rerun = runner
        .apply(
            &compilation,
            &second,
            &RunOptions {
                environment,
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        rerun
            .models
            .iter()
            .all(|model| model.status == ExecutionStatus::Skipped),
        "{:?}",
        rerun.models
    );
}

#[tokio::test]
async fn stale_plan_is_rejected() {
    let first_compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    let plan = plan_all(&first_compilation, adapter.clone()).await;

    // A different workspace (changed SQL) makes the plan stale.
    let mut changed = SemanticProject::in_memory(vec![
        model("assay.raw", "select 1 as id"),
        model("assay.results", "select * from assay.raw"),
    ]);
    changed.tests = Vec::new();
    let changed_compilation = compile(&changed);
    assert!(changed_compilation.is_ok());

    let runner = Runner::new(adapter, None);
    let error = runner
        .apply(&changed_compilation, &plan, &RunOptions::default())
        .await
        .expect_err("stale plan rejected");
    assert!(matches!(
        error,
        phlo_transform_engine::EngineError::StalePlan(_)
    ));
}

fn incremental_model(sql: &str, key: &str) -> SemanticModel {
    let mut model = model("assay.events", sql);
    model.config.materialization = Materialization::Incremental;
    model.config.incremental = Some(IncrementalStrategy::Key {
        columns: vec![key.to_string()],
    });
    model
}

fn compile_models(models: Vec<SemanticModel>) -> Compilation {
    let compilation = compile(&SemanticProject::in_memory(models));
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    compilation
}

#[tokio::test]
async fn incremental_key_bootstraps_then_merges() {
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let environment = Some("dev".to_string());

    let first = compile_models(vec![incremental_model("select 1 as id, 10 as value", "id")]);
    let selected = select_models(&first, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&first, &selected, environment.clone())
        .await
        .unwrap();
    assert!(plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &first,
            &plan,
            &RunOptions {
                environment: environment.clone(),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // Bootstrap is a full create.
    assert_eq!(adapter.created.lock().unwrap().len(), 1);
    assert!(adapter.merges.lock().unwrap().is_empty());

    // Changed SQL, same key: merge instead of full rebuild.
    let second = compile_models(vec![incremental_model("select 2 as id, 20 as value", "id")]);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&second, &selected, environment.clone())
        .await
        .unwrap();
    assert_eq!(plan.models[0].action, PlanAction::Build);
    assert!(!plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &second,
            &plan,
            &RunOptions {
                environment,
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(adapter.merges.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn changing_incremental_key_requires_full_rebuild() {
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let environment = Some("dev".to_string());

    let first = compile_models(vec![incremental_model("select 1 as id, 10 as value", "id")]);
    let selected = select_models(&first, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&first, &selected, environment.clone())
        .await
        .unwrap();
    Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &first,
            &plan,
            &RunOptions {
                environment: environment.clone(),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let second = compile_models(vec![incremental_model(
        "select 1 as id, 10 as value",
        "value",
    )]);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&second, &selected, environment)
        .await
        .unwrap();
    assert!(plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    assert!(plan.models[0]
        .reasons
        .contains(&ChangeReason::IncrementalChange));
}
