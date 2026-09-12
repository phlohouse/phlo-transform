//! Engine tests using a fake adapter.
//!
//! These verify planning, dependency-ordered execution, bounded concurrency,
//! partial-failure blocking, tests and state persistence without a live
//! warehouse.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use phlo_transform_core::{
    compile, compile_with_options, select_models, Compilation, DataType, EmptySchemaProvider,
    EmptySourceStateProvider, IncrementalStrategy, Materialization, ModelId, ModelOrigin,
    Nullability, Relation, RelationSchema, SchemaColumn, SelectionOptions, SemanticModel,
    SemanticProject, SemanticSeed, SemanticTest, SourceId, SourceStateProvider,
    StaticSchemaProvider, StaticSourceStateProvider, TestId,
};
use phlo_transform_engine::{
    collect_source_states, Adapter, AdapterError, ArtifactWriter, CancelHandle, CatalogRequest,
    ChangeReason, ColumnInfo, ExecutionStatus, Plan, PlanAction, Planner, QueryResult, RunOptions,
    Runner, SqliteStateStore, StateStore,
};

#[derive(Default)]
struct FakeAdapter {
    existing: Mutex<BTreeSet<String>>,
    fail_targets: Mutex<BTreeSet<String>>,
    created: Mutex<Vec<String>>,
    appends: Mutex<Vec<String>>,
    append_sqls: Mutex<Vec<String>>,
    merges: Mutex<Vec<String>>,
    replaced_partitions: Mutex<Vec<String>>,
    columns: Mutex<Vec<ColumnInfo>>,
    loaded_csvs: Mutex<Vec<String>>,
    fail_loads: Mutex<BTreeSet<String>>,
    source_states: Mutex<BTreeMap<String, String>>,
    max_value: Mutex<Option<String>>,
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

    fn set_source_state(&self, relation: &str, state: &str) {
        self.source_states
            .lock()
            .unwrap()
            .insert(relation.to_string(), state.to_string());
    }

    fn set_columns(&self, columns: Vec<ColumnInfo>) {
        *self.columns.lock().unwrap() = columns;
    }

    fn set_max_value(&self, value: &str) {
        *self.max_value.lock().unwrap() = Some(value.to_string());
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

    async fn execute(&self, sql: &str) -> Result<QueryResult, AdapterError> {
        if sql.to_ascii_lowercase().contains("max(") {
            if let Some(value) = self.max_value.lock().unwrap().clone() {
                return Ok(QueryResult {
                    query_id: Some("max-query".to_string()),
                    columns: vec!["max".to_string()],
                    rows: vec![vec![value]],
                    row_count: 1,
                });
            }
        }
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

    async fn append(&self, relation: &Relation, sql: &str) -> Result<QueryResult, AdapterError> {
        let display = relation.display();
        self.appends.lock().unwrap().push(display.clone());
        self.append_sqls.lock().unwrap().push(sql.to_string());
        self.existing.lock().unwrap().insert(display);
        Ok(QueryResult {
            query_id: Some("append-query".to_string()),
            ..Default::default()
        })
    }

    async fn load_csv(
        &self,
        relation: &Relation,
        path: &Path,
    ) -> Result<QueryResult, AdapterError> {
        let display = relation.display();
        if self.fail_loads.lock().unwrap().contains(&display) {
            return Err(AdapterError::new(
                "FAKE001",
                format!("simulated seed failure for {display}"),
            ));
        }
        self.loaded_csvs
            .lock()
            .unwrap()
            .push(format!("{display} <- {}", path.display()));
        self.existing.lock().unwrap().insert(display);
        Ok(QueryResult {
            query_id: Some("load-csv".to_string()),
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

    async fn replace_partitions(
        &self,
        relation: &Relation,
        _partition_columns: &[String],
        _sql: &str,
    ) -> Result<QueryResult, AdapterError> {
        let display = relation.display();
        self.replaced_partitions
            .lock()
            .unwrap()
            .push(display.clone());
        self.existing.lock().unwrap().insert(display);
        Ok(QueryResult {
            query_id: Some("replace-partitions-query".to_string()),
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
        Ok(self.columns.lock().unwrap().clone())
    }

    async fn ensure_catalog(&self, _request: &CatalogRequest) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn ensure_schema(&self, _relation: &Relation) -> Result<(), AdapterError> {
        Ok(())
    }

    async fn source_state(&self, relation: &Relation) -> Result<Option<String>, AdapterError> {
        Ok(self
            .source_states
            .lock()
            .unwrap()
            .get(&relation.display())
            .cloned())
    }

    async fn partition_counts(
        &self,
        _relation: &Relation,
        _partition_columns: &[String],
    ) -> Result<Option<Vec<(String, i64)>>, AdapterError> {
        Ok(None)
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

#[tokio::test]
async fn cache_reuse_across_environments_is_reported_as_cached() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let selected = select_models(&compilation, &SelectionOptions::default());

    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                environment: Some("dev".to_string()),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // `prod` has no recorded materialisation, but the exact desired versions
    // exist in `dev`, so they are cache candidates.
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, Some("prod".to_string()))
        .await
        .unwrap();
    assert!(
        plan.models
            .iter()
            .all(|model| model.action == PlanAction::Cached),
        "{:?}",
        plan.models
            .iter()
            .map(|model| (model.id.as_str(), model.action))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn source_state_change_marks_model_for_rebuild() {
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());

    let build = |snapshot: &str| {
        let mut provider = StaticSourceStateProvider::new();
        provider.insert("external.raw_assay_results", snapshot);
        let project = SemanticProject::in_memory(vec![model(
            "assay.raw",
            "select * from external.raw_assay_results",
        )]);
        let compilation = compile_with_options(&project, &EmptySchemaProvider, &provider);
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let first = build("snapshot-1");
    let selected = select_models(&first, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&first, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    Runner::new(adapter.clone(), Some(state.clone()))
        .apply(
            &first,
            &plan,
            &RunOptions {
                environment: Some("dev".to_string()),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let second = build("snapshot-2");
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&second, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    assert_eq!(plan.models[0].action, PlanAction::Build);
    assert!(plan.models[0].reasons.contains(&ChangeReason::SourceChange));
}

#[tokio::test]
async fn collects_source_states_from_adapter() {
    let adapter = FakeAdapter::default();
    adapter.set_source_state("external.raw_assay_results", "snap-9");
    let source = SourceId::new(vec![
        "external".to_string(),
        "raw_assay_results".to_string(),
    ])
    .unwrap();

    let provider = collect_source_states(
        &adapter,
        std::slice::from_ref(&source),
        &[],
        None,
        Some("default"),
    )
    .await
    .expect("collect");
    assert_eq!(provider.source_state(&source).as_deref(), Some("snap-9"));
}

fn incremental_with(sql: &str, strategy: IncrementalStrategy) -> SemanticModel {
    let mut model = model("assay.events", sql);
    model.config.materialization = Materialization::Incremental;
    model.config.incremental = Some(strategy);
    model
}

fn events_provider() -> StaticSchemaProvider {
    let mut provider = StaticSchemaProvider::new();
    provider.insert(
        "external.events",
        RelationSchema::new(vec![
            SchemaColumn {
                name: "id".to_string(),
                data_type: DataType::BigInt,
                nullability: Nullability::NotNull,
            },
            SchemaColumn {
                name: "updated_at".to_string(),
                data_type: DataType::Timestamp,
                nullability: Nullability::Nullable,
            },
        ]),
    );
    provider
}

async fn run_once(
    adapter: Arc<FakeAdapter>,
    state: Arc<SqliteStateStore>,
    compilation: &Compilation,
    environment: &str,
) -> phlo_transform_engine::RunResult {
    let selected = select_models(compilation, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(compilation, &selected, Some(environment.to_string()))
        .await
        .unwrap();
    Runner::new(adapter, Some(state))
        .apply(
            compilation,
            &plan,
            &RunOptions {
                environment: Some(environment.to_string()),
                run_tests: false,
                ..Default::default()
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn incremental_partition_replaces_after_bootstrap() {
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());

    let first = compile_models(vec![incremental_with(
        "select 1 as id, DATE '2026-09-10' as d",
        IncrementalStrategy::Partition {
            columns: vec!["d".to_string()],
        },
    )]);
    run_once(adapter.clone(), state.clone(), &first, "dev").await;
    assert_eq!(adapter.created.lock().unwrap().len(), 1);
    assert!(adapter.replaced_partitions.lock().unwrap().is_empty());

    let second = compile_models(vec![incremental_with(
        "select 1 as id, DATE '2026-09-10' as d \
         union all select 2, DATE '2026-09-11'",
        IncrementalStrategy::Partition {
            columns: vec!["d".to_string()],
        },
    )]);
    let selected = select_models(&second, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&second, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    assert!(!plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    run_once(adapter.clone(), state.clone(), &second, "dev").await;
    assert_eq!(adapter.replaced_partitions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn incremental_time_window_uses_watermark() {
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_max_value("2026-09-10 00:00:00.000");
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let provider = events_provider();

    let build = |sql: &str| {
        let project = SemanticProject::in_memory(vec![incremental_with(
            sql,
            IncrementalStrategy::TimeWindow {
                column: "updated_at".to_string(),
                overlap_seconds: None,
            },
        )]);
        let compilation = compile_with_options(&project, &provider, &EmptySourceStateProvider);
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let first = build("select id, updated_at from external.events");
    run_once(adapter.clone(), state.clone(), &first, "dev").await;
    assert_eq!(
        state.watermark("assay.events", Some("dev")).unwrap(),
        Some("2026-09-10 00:00:00.000".to_string())
    );

    let second = build("select id, updated_at from external.events where id > 0");
    run_once(adapter.clone(), state.clone(), &second, "dev").await;
    let appends = adapter.append_sqls.lock().unwrap().clone();
    assert_eq!(appends.len(), 1, "{appends:?}");
    assert!(appends[0].contains("CAST("), "{}", appends[0]);
    assert!(appends[0].contains("updated_at"), "{}", appends[0]);
}

#[tokio::test]
async fn schema_removal_forces_full_rebuild() {
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let provider = events_provider();

    let build = |sql: &str| {
        let project =
            SemanticProject::in_memory(vec![incremental_with(sql, IncrementalStrategy::Append)]);
        let compilation = compile_with_options(&project, &provider, &EmptySourceStateProvider);
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let first = build("select id, updated_at from external.events");
    run_once(adapter.clone(), state.clone(), &first, "dev").await;

    // The target now has an extra legacy column that the desired schema drops.
    adapter.set_columns(vec![
        ColumnInfo {
            name: "id".to_string(),
            data_type: "bigint".to_string(),
            nullable: false,
        },
        ColumnInfo {
            name: "legacy".to_string(),
            data_type: "varchar".to_string(),
            nullable: true,
        },
    ]);

    let second = build("select id, updated_at from external.events where id > 0");
    let selected = select_models(&second, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&second, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    assert!(plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    assert!(plan.models[0].reasons.contains(&ChangeReason::SchemaChange));
}

#[tokio::test]
async fn time_window_overlap_widens_the_predicate() {
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_max_value("2026-09-10 00:00:00.000");
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let provider = events_provider();

    let build = |sql: &str| {
        let project = SemanticProject::in_memory(vec![incremental_with(
            sql,
            IncrementalStrategy::TimeWindow {
                column: "updated_at".to_string(),
                overlap_seconds: Some(3600),
            },
        )]);
        compile_with_options(&project, &provider, &EmptySourceStateProvider)
    };

    let first = build("select id, updated_at from external.events");
    run_once(adapter.clone(), state.clone(), &first, "dev").await;
    let second = build("select id, updated_at from external.events where id > 0");
    run_once(adapter.clone(), state.clone(), &second, "dev").await;

    let appends = adapter.append_sqls.lock().unwrap().clone();
    assert_eq!(appends.len(), 1, "{appends:?}");
    assert!(
        appends[0].contains("INTERVAL '3600' SECOND"),
        "{}",
        appends[0]
    );
}

/// A model reading a seed relation plans the CSV load first, then skips it
/// once the recorded content hash matches.
#[tokio::test]
async fn seed_loads_before_models_and_skips_when_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();

    let mut project = SemanticProject::in_memory(vec![model(
        "main.stg_events",
        "select * from raw.raw_events",
    )]);
    project.workspace_root = Some(dir.path().to_path_buf());
    project.seeds = vec![SemanticSeed {
        name: "raw_events".to_string(),
        path: PathBuf::from("seeds/raw_events.csv"),
        schema: Some("raw".to_string()),
        content_hash: "hash-v1".to_string(),
        columns: vec!["id".to_string(), "status".to_string()],
    }];
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let result = run_once(adapter.clone(), state.clone(), &compilation, "dev").await;

    assert_eq!(result.status, ExecutionStatus::Passed);
    assert_eq!(result.seeds.len(), 1);
    assert_eq!(result.seeds[0].status, ExecutionStatus::Passed);
    let loaded = adapter.loaded_csvs.lock().unwrap().clone();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].starts_with("raw.raw_events <- "), "{loaded:?}");
    assert!(loaded[0].ends_with("seeds/raw_events.csv"), "{loaded:?}");

    // Second plan: relation exists and the recorded hash matches → Skip.
    let selected = select_models(&compilation, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    assert_eq!(plan.seeds.len(), 1);
    assert_eq!(plan.seeds[0].action, PlanAction::Skip);
}

/// A seed read only through an ephemeral chain is still planned and loaded.
/// Ephemeral models are filtered out of the execution order, but their
/// source reads are inlined into dependents — seed discovery must look at
/// `planned_ids`, not the filtered order.
#[tokio::test]
async fn seed_read_through_ephemeral_chain_is_loaded() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();

    let mut staged = model(
        "main.stg_seed_events",
        "select * from raw.raw_events where status = 'placed'",
    );
    staged.config.materialization = Materialization::Ephemeral;
    let mut named = model("main.stg_named", "select * from main.stg_seed_events");
    named.config.materialization = Materialization::Ephemeral;
    let events = model("main.events", "select * from main.stg_named");

    let mut project = SemanticProject::in_memory(vec![staged, named, events]);
    project.workspace_root = Some(dir.path().to_path_buf());
    project.seeds = vec![SemanticSeed {
        name: "raw_events".to_string(),
        path: PathBuf::from("seeds/raw_events.csv"),
        schema: Some("raw".to_string()),
        content_hash: "hash-v1".to_string(),
        columns: vec!["id".to_string(), "status".to_string()],
    }];
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());

    let selected = select_models(&compilation, &SelectionOptions::default());
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    // The ephemeral chain is inlined into `main.events`: only the
    // materialised model is planned, and the seed it transitively reads
    // is loaded.
    assert_eq!(plan.model_count(), 1);
    assert_eq!(plan.seeds.len(), 1, "{:?}", plan.seeds);
    assert_eq!(plan.seeds[0].name, "raw_events");

    let result = run_once(adapter.clone(), state, &compilation, "dev").await;
    assert_eq!(result.status, ExecutionStatus::Passed);
    let loaded = adapter.loaded_csvs.lock().unwrap().clone();
    assert_eq!(loaded.len(), 1, "{loaded:?}");
    assert!(loaded[0].starts_with("raw.raw_events <- "), "{loaded:?}");
    let created = adapter.created.lock().unwrap().clone();
    assert_eq!(created, vec!["main.events"], "{created:?}");
}

/// A changed CSV content hash re-plans the seed as a Build.
#[tokio::test]
async fn seed_content_change_replans_the_load() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(dir.path().join("seeds/raw_events.csv"), "id,status\n").unwrap();

    let build = |hash: &str| {
        let mut project = SemanticProject::in_memory(vec![model(
            "main.stg_events",
            "select * from raw.raw_events",
        )]);
        project.workspace_root = Some(dir.path().to_path_buf());
        project.seeds = vec![SemanticSeed {
            name: "raw_events".to_string(),
            path: PathBuf::from("seeds/raw_events.csv"),
            schema: Some("raw".to_string()),
            content_hash: hash.to_string(),
            columns: vec!["id".to_string(), "status".to_string()],
        }];
        let compilation = compile(&project);
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    run_once(adapter.clone(), state.clone(), &build("hash-v1"), "dev").await;

    let selected = select_models(&build("hash-v2"), &SelectionOptions::default());
    let compilation = build("hash-v2");
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&compilation, &selected, Some("dev".to_string()))
        .await
        .unwrap();
    assert_eq!(plan.seeds[0].action, PlanAction::Build);
}

/// A failed seed load blocks the models reading it (and their dependents)
/// instead of running them against a stale seed table.
#[tokio::test]
async fn failed_seed_load_blocks_dependent_models() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();

    let mut project = SemanticProject::in_memory(vec![
        model("main.stg_events", "select * from raw.raw_events"),
        model("main.daily", "select * from main.stg_events"),
        model("main.independent", "select * from external.other"),
    ]);
    project.workspace_root = Some(dir.path().to_path_buf());
    project.seeds = vec![SemanticSeed {
        name: "raw_events".to_string(),
        path: PathBuf::from("seeds/raw_events.csv"),
        schema: Some("raw".to_string()),
        content_hash: "hash-v1".to_string(),
        columns: vec!["id".to_string(), "status".to_string()],
    }];
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let adapter = Arc::new(FakeAdapter::default());
    adapter
        .fail_loads
        .lock()
        .unwrap()
        .insert("raw.raw_events".to_string());
    // A stale seed table still exists — the models must not read it.
    adapter
        .existing
        .lock()
        .unwrap()
        .insert("raw.raw_events".to_string());

    let result = run_once(
        adapter.clone(),
        Arc::new(SqliteStateStore::in_memory().unwrap()),
        &compilation,
        "dev",
    )
    .await;

    assert_eq!(result.seeds[0].status, ExecutionStatus::Failed);
    assert_eq!(result.status, ExecutionStatus::Failed);
    let status = |name: &str| {
        result
            .models
            .iter()
            .find(|model| model.model == name)
            .map(|model| model.status)
            .unwrap()
    };
    assert_eq!(status("main.stg_events"), ExecutionStatus::Blocked);
    assert_eq!(status("main.daily"), ExecutionStatus::Blocked);
    assert_eq!(status("main.independent"), ExecutionStatus::Passed);
    let created = adapter.created.lock().unwrap().clone();
    assert!(
        !created
            .iter()
            .any(|target| target == "main.stg_events" || target == "main.daily"),
        "{created:?}"
    );
}

/// A seed that only feeds a test (no downstream model) is still planned and
/// loaded first; when the load fails the test is skipped instead of reading
/// a stale table.
#[tokio::test]
async fn seed_tests_pull_the_seed_into_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();

    let build = || {
        let mut project = SemanticProject::in_memory(vec![model(
            "main.independent",
            "select * from external.other",
        )]);
        project.workspace_root = Some(dir.path().to_path_buf());
        project.seeds = vec![SemanticSeed {
            name: "raw_events".to_string(),
            path: PathBuf::from("seeds/raw_events.csv"),
            schema: Some("raw".to_string()),
            content_hash: "hash-v1".to_string(),
            columns: vec!["id".to_string(), "status".to_string()],
        }];
        project.tests = vec![SemanticTest {
            id: TestId::new("seed_has_no_nulls"),
            sql: "select * from raw.raw_events where id is null".to_string(),
            origin: ModelOrigin::in_memory(),
        }];
        let compilation = compile(&project);
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    // The seed is planned even though no model reads it.
    let compilation = build();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let plan = plan_all(&compilation, adapter.clone()).await;
    assert_eq!(plan.seeds.len(), 1);
    assert_eq!(plan.seeds[0].name, "raw_events");
    assert_eq!(plan.seeds[0].action, PlanAction::Build);
    assert_eq!(plan.tests.len(), 1);

    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .expect("run succeeds");
    let loaded = adapter.loaded_csvs.lock().unwrap().clone();
    assert_eq!(loaded.len(), 1);
    assert_eq!(result.tests.len(), 1);
    assert_eq!(result.tests[0].status, ExecutionStatus::Passed);

    // A failed load skips the test entirely.
    let compilation = build();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter
        .fail_loads
        .lock()
        .unwrap()
        .insert("raw.raw_events".to_string());
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .expect("run succeeds");
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(result.tests.is_empty(), "{:?}", result.tests);
}
