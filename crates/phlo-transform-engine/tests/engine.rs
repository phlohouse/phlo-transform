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
    compile, compile_with_options, resolve_selection, Compilation, DataType, EmptySchemaProvider,
    EmptySourceStateProvider, IncrementalStrategy, Materialization, ModelId, ModelOrigin,
    Nullability, Relation, RelationSchema, SchemaColumn, Selection, SelectorSet, SemanticModel,
    SemanticProject, SemanticSeed, SemanticTest, SourceId, SourceStateProvider,
    StaticSchemaProvider, StaticSourceStateProvider, TestId,
};
use phlo_transform_engine::{
    branch_diff, changed_models, collect_source_states, Adapter, AdapterError, ArtifactWriter,
    BranchDiffRequest, CancelHandle, CatalogRequest, ColumnInfo, DatasetStatus, EngineError,
    EngineEvent, ExecutionStatus, FailureCategory, MaterializedRecord, Membership, ModelResult,
    ModelRunRecord, Plan, PlanAction, PlanOptions, Planner, PromotionRecord, QueryResult,
    ReasonKind, RetryPolicy, RunOptions, RunRecord, RunResult, RunSummary, Runner, SeedRecord,
    SeedRunRecord, SqliteStateStore, StateStore, StoredPlan, StoredRun, TestRunRecord,
};

/// How a target should fail: the error to return, and how many attempts it
/// applies to (`None` = every attempt).
#[derive(Clone)]
struct FailSpec {
    error: AdapterError,
    times: Option<usize>,
}

#[derive(Default)]
struct FakeAdapter {
    existing: Arc<Mutex<BTreeSet<String>>>,
    fail_targets: Arc<Mutex<BTreeSet<String>>>,
    /// Per-target injected failures: `(error, times)` — `times` bounds the
    /// failure to the first N attempts so `flaky` targets recover.
    fail_modes: Arc<Mutex<BTreeMap<String, FailSpec>>>,
    /// Attempt counts per target, for asserting retry behaviour.
    attempt_counts: Arc<Mutex<BTreeMap<String, usize>>>,
    /// Per-target delay override, for fail-fast/timeout tests.
    delay_for: Arc<Mutex<BTreeMap<String, u64>>>,
    created: Arc<Mutex<Vec<String>>>,
    appends: Arc<Mutex<Vec<String>>>,
    append_sqls: Arc<Mutex<Vec<String>>>,
    merges: Arc<Mutex<Vec<String>>>,
    replaced_partitions: Arc<Mutex<Vec<String>>>,
    columns: Arc<Mutex<Vec<ColumnInfo>>>,
    /// Per-relation column metadata (keyed by `relation.display()`); the
    /// shared `columns` list remains the fallback.
    relation_columns: Arc<Mutex<BTreeMap<String, Vec<ColumnInfo>>>>,
    /// Row counts keyed by `relation.sql()`, answering `SELECT count(*)`.
    relation_counts: Arc<Mutex<BTreeMap<String, i64>>>,
    loaded_csvs: Arc<Mutex<Vec<String>>>,
    fail_loads: Arc<Mutex<BTreeSet<String>>>,
    source_states: Arc<Mutex<BTreeMap<String, String>>>,
    max_value: Arc<Mutex<Option<String>>>,
    test_rows: Arc<Mutex<u64>>,
    delay_ms: u64,
    current: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
    /// Query ids in flight through *this* adapter handle — a tracked
    /// attempt view gets its own registry.
    in_flight: Arc<Mutex<BTreeSet<String>>>,
    /// Query ids passed to `cancel` — shared across tracked views so tests
    /// can observe what the runner killed.
    cancelled: Arc<Mutex<BTreeSet<String>>>,
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

    /// `target` fails with `error` on every attempt.
    fn fail_with(&self, target: &str, error: AdapterError) {
        self.fail_modes
            .lock()
            .unwrap()
            .insert(target.to_string(), FailSpec { error, times: None });
    }

    /// `target` fails with `error` on its first `times` attempts, then
    /// succeeds — a transiently failing target.
    fn fail_first(&self, target: &str, times: usize, error: AdapterError) {
        self.fail_modes.lock().unwrap().insert(
            target.to_string(),
            FailSpec {
                error,
                times: Some(times),
            },
        );
    }

    /// `target` takes `delay_ms` to build regardless of the global delay.
    fn slow_target(&self, target: &str, delay_ms: u64) {
        self.delay_for
            .lock()
            .unwrap()
            .insert(target.to_string(), delay_ms);
    }

    /// How many attempts a target saw.
    fn attempts(&self, target: &str) -> usize {
        self.attempt_counts
            .lock()
            .unwrap()
            .get(target)
            .copied()
            .unwrap_or(0)
    }

    /// Stop failing a target — the simulated "fix" between runs.
    fn heal(&self, target: &str) {
        self.fail_targets.lock().unwrap().remove(target);
        self.fail_modes.lock().unwrap().remove(target);
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

    /// Per-relation columns, keyed by `relation.display()`.
    fn set_relation_columns(&self, relation: &str, columns: Vec<ColumnInfo>) {
        self.relation_columns
            .lock()
            .unwrap()
            .insert(relation.to_string(), columns);
    }

    /// A row count answered for `SELECT count(*) FROM <relation>`.
    fn set_row_count(&self, relation: &Relation, rows: i64) {
        self.relation_counts
            .lock()
            .unwrap()
            .insert(relation.sql(), rows);
    }

    fn set_max_value(&self, value: &str) {
        *self.max_value.lock().unwrap() = Some(value.to_string());
    }

    /// Query ids `cancel` was asked to kill (across all tracked views).
    fn cancelled_queries(&self) -> Vec<String> {
        self.cancelled.lock().unwrap().iter().cloned().collect()
    }

    /// A per-attempt view sharing all fake state except the in-flight
    /// registry — mirrors how a real adapter reports only its own queries.
    fn attempt_view(&self) -> Self {
        Self {
            existing: self.existing.clone(),
            fail_targets: self.fail_targets.clone(),
            fail_modes: self.fail_modes.clone(),
            attempt_counts: self.attempt_counts.clone(),
            delay_for: self.delay_for.clone(),
            created: self.created.clone(),
            appends: self.appends.clone(),
            append_sqls: self.append_sqls.clone(),
            merges: self.merges.clone(),
            replaced_partitions: self.replaced_partitions.clone(),
            columns: self.columns.clone(),
            relation_columns: self.relation_columns.clone(),
            relation_counts: self.relation_counts.clone(),
            loaded_csvs: self.loaded_csvs.clone(),
            fail_loads: self.fail_loads.clone(),
            source_states: self.source_states.clone(),
            max_value: self.max_value.clone(),
            test_rows: self.test_rows.clone(),
            delay_ms: self.delay_ms,
            current: self.current.clone(),
            max_concurrent: self.max_concurrent.clone(),
            in_flight: Arc::new(Mutex::new(BTreeSet::new())),
            cancelled: self.cancelled.clone(),
        }
    }

    async fn create(
        &self,
        relation: &Relation,
        query_id: &str,
    ) -> Result<QueryResult, AdapterError> {
        // A unique id per (operation, target): registering it before the
        // delay means a dropped future leaves the id in-flight, matching
        // how a warehouse keeps running the statement.
        let qid = format!("{query_id}:{}", relation.display());
        self.in_flight.lock().unwrap().insert(qid.clone());
        let result = self.create_inner(relation, &qid).await;
        self.in_flight.lock().unwrap().remove(&qid);
        result
    }

    async fn create_inner(
        &self,
        relation: &Relation,
        query_id: &str,
    ) -> Result<QueryResult, AdapterError> {
        let display = relation.display();
        let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now, Ordering::SeqCst);
        let delay = self
            .delay_for
            .lock()
            .unwrap()
            .get(&display)
            .copied()
            .unwrap_or(self.delay_ms);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        self.current.fetch_sub(1, Ordering::SeqCst);

        let attempt = {
            let mut counts = self.attempt_counts.lock().unwrap();
            let count = counts.entry(display.clone()).or_default();
            *count += 1;
            *count
        };
        if let Some(spec) = self.fail_modes.lock().unwrap().get(&display).cloned() {
            if spec.times.is_none_or(|times| attempt <= times) {
                return Err(spec.error);
            }
        }
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
        if let Some(relation) = sql.strip_prefix("SELECT count(*) FROM ") {
            let rows = self
                .relation_counts
                .lock()
                .unwrap()
                .get(relation)
                .copied()
                .unwrap_or(0);
            return Ok(QueryResult {
                query_id: Some("count-query".to_string()),
                columns: vec!["count".to_string()],
                rows: vec![vec![rows.to_string()]],
                row_count: 1,
            });
        }
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
        let attempt = {
            let mut counts = self.attempt_counts.lock().unwrap();
            let count = counts.entry(display.clone()).or_default();
            *count += 1;
            *count
        };
        if let Some(spec) = self.fail_modes.lock().unwrap().get(&display).cloned() {
            if spec.times.is_none_or(|times| attempt <= times) {
                return Err(spec.error);
            }
        }
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

    async fn cancel(&self, query_id: &str) -> Result<(), AdapterError> {
        self.cancelled.lock().unwrap().insert(query_id.to_string());
        self.in_flight.lock().unwrap().remove(query_id);
        Ok(())
    }

    fn track_attempt(&self) -> Option<Arc<dyn Adapter>> {
        Some(Arc::new(self.attempt_view()))
    }

    fn in_flight_queries(&self) -> Vec<String> {
        self.in_flight.lock().unwrap().iter().cloned().collect()
    }

    async fn relation_columns(&self, relation: &Relation) -> Result<Vec<ColumnInfo>, AdapterError> {
        if let Some(columns) = self
            .relation_columns
            .lock()
            .unwrap()
            .get(&relation.display())
            .cloned()
        {
            return Ok(columns);
        }
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
    let selected = Selection::all(compilation);
    Planner::new(adapter, None)
        .plan(compilation, &selected, None, &PlanOptions::default())
        .await
        .expect("plan succeeds")
}

/// A state-aware plan — materialisation history shapes skip/cached
/// decisions, unlike [`plan_all`] which always builds.
async fn plan_all_with_state(
    compilation: &Compilation,
    adapter: Arc<FakeAdapter>,
    state: Arc<SqliteStateStore>,
    environment: Option<String>,
) -> Plan {
    let selected = Selection::all(compilation);
    Planner::new(adapter, Some(state))
        .plan(compilation, &selected, environment, &PlanOptions::default())
        .await
        .expect("plan succeeds")
}

/// Drive `apply` in a background task and abort it `after` — the closest a
/// test gets to a killed process: the run record stays `running` with
/// whatever progress landed. Returns the interrupted run's id.
async fn interrupt_apply(
    adapter: Arc<FakeAdapter>,
    state: Arc<SqliteStateStore>,
    compilation: &Compilation,
    plan: &Plan,
    options: RunOptions,
    after: Duration,
) -> String {
    let handle = {
        let runner = Runner::new(adapter, Some(state.clone()));
        let compilation = compilation.clone();
        let plan = plan.clone();
        tokio::spawn(async move { runner.apply(&compilation, &plan, &options).await })
    };
    tokio::time::sleep(after).await;
    handle.abort();
    let _ = handle.await;
    state
        .runs()
        .unwrap()
        .into_iter()
        .find(|run| run.status == ExecutionStatus::Running)
        .expect("an interrupted run stays running")
        .run_id
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
        "openlineage.json",
        "plan.json",
        "run.json",
    ] {
        assert!(
            directory.path().join(name).is_file(),
            "missing artifact {name}"
        );
    }

    // `openlineage.json` carries the exported design-time document: a flat
    // array of spec-valid OpenLineage events (a valid batch payload).
    let document: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(directory.path().join("openlineage.json")).unwrap(),
    )
    .unwrap();
    let events = document["document"].as_array().expect("event array");
    assert!(!events.is_empty());
    assert!(events.iter().all(|event| {
        event["eventTime"].is_string()
            && event["producer"] == "https://github.com/phlohouse/phlo-transform"
            && event["schemaURL"].is_string()
    }));
    assert!(events.iter().any(|event| event.get("job").is_some()));
    assert!(events.iter().any(|event| event.get("dataset").is_some()));
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
    let selected = Selection::all(&compilation);
    let environment = Some("dev".to_string());

    let first = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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
        .plan(
            &compilation,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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
    let selected = Selection::all(&first);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &first,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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
        .plan(
            &second,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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
    let selected = Selection::all(&first);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &first,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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
        .plan(&second, &selected, environment, &PlanOptions::default())
        .await
        .unwrap();
    assert!(plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    assert!(plan.models[0]
        .reasons
        .iter()
        .any(|reason| reason.kind == ReasonKind::IncrementalChange));
}

#[tokio::test]
async fn cache_reuse_across_environments_is_reported_as_cached() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let selected = Selection::all(&compilation);

    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
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
        .plan(
            &compilation,
            &selected,
            Some("prod".to_string()),
            &PlanOptions::default(),
        )
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
    let selected = Selection::all(&first);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &first,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
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
        .plan(
            &second,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(plan.models[0].action, PlanAction::Build);
    assert!(plan.models[0]
        .reasons
        .iter()
        .any(|r| r.kind == ReasonKind::SourceChange));
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
    let selected = Selection::all(compilation);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            compilation,
            &selected,
            Some(environment.to_string()),
            &PlanOptions::default(),
        )
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
    let selected = Selection::all(&second);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &second,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
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
    let selected = Selection::all(&second);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &second,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
        .await
        .unwrap();
    assert!(plan.models[0].full_rebuild, "{:?}", plan.models[0]);
    assert!(plan.models[0]
        .reasons
        .iter()
        .any(|r| r.kind == ReasonKind::SchemaChange));
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
    let selected = Selection::all(&compilation);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
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

    let selected = Selection::all(&compilation);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
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

    let selected = Selection::all(&build("hash-v2"));
    let compilation = build("hash-v2");
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            Some("dev".to_string()),
            &PlanOptions::default(),
        )
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

    // A failed load blocks the test — it never reads a stale table.
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
    assert_eq!(result.tests.len(), 1);
    assert_eq!(result.tests[0].status, ExecutionStatus::Blocked);
    assert_eq!(
        result.tests[0].failure.as_ref().unwrap().category,
        FailureCategory::Dependency
    );
    // The model that does not read the seed still ran.
    assert!(result
        .models
        .iter()
        .any(|model| model.status == ExecutionStatus::Passed));
}

/// The second plan after a run is all Skip with an `unchanged` reason; the
/// same plan with `force` is all Build with a `forced` reason.
#[tokio::test]
async fn skips_explain_unchanged_and_force_overrides() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let selected = Selection::all(&compilation);
    let environment = Some("dev".to_string());

    let first = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
        .await
        .unwrap();
    Runner::new(adapter.clone(), Some(state.clone()))
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

    let second = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
        .await
        .unwrap();
    for model in &second.models {
        assert_eq!(model.action, PlanAction::Skip);
        assert!(
            model
                .reasons
                .iter()
                .any(|reason| reason.kind == ReasonKind::Unchanged),
            "{:?}",
            model.reasons
        );
    }

    let forced = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &compilation,
            &selected,
            environment,
            &PlanOptions { force: true },
        )
        .await
        .unwrap();
    for model in &forced.models {
        assert_eq!(model.action, PlanAction::Build);
        assert!(
            model
                .reasons
                .iter()
                .any(|reason| reason.kind == ReasonKind::Forced),
            "{:?}",
            model.reasons
        );
    }
}

/// Selecting one model pulls its dependencies into the plan with
/// `dependency` membership and a reason naming the requiring model.
#[tokio::test]
async fn dependency_closure_membership_is_explained() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    let set =
        SelectorSet::parse(&["reporting.monthly".to_string()], &[], &[], false, false).unwrap();
    let selection = resolve_selection(&compilation, &set, None).unwrap();

    let plan = Planner::new(adapter, None)
        .plan(&compilation, &selection, None, &PlanOptions::default())
        .await
        .unwrap();

    let by_id: BTreeMap<&str, _> = plan
        .models
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect();
    assert_eq!(by_id.len(), 3, "{:?}", by_id.keys());
    assert_eq!(by_id["reporting.monthly"].membership, Membership::Selected);
    assert_eq!(by_id["assay.results"].membership, Membership::Dependency);
    assert_eq!(by_id["assay.raw"].membership, Membership::Dependency);
    assert!(
        by_id["assay.raw"]
            .reasons
            .iter()
            .any(|reason| reason.kind == ReasonKind::SelectedDependency
                && reason.detail.contains("reporting.monthly")),
        "{:?}",
        by_id["assay.raw"].reasons
    );
    assert_eq!(
        plan.selection.matched,
        vec!["reporting.monthly".to_string()]
    );
    assert_eq!(
        plan.selection.required,
        vec!["assay.raw".to_string(), "assay.results".to_string()]
    );
}

/// `model+` pulls dependents in with `expanded` membership and records the
/// responsible term.
#[tokio::test]
async fn downstream_expansion_is_explained() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    let set = SelectorSet::parse(&["assay.results+".to_string()], &[], &[], false, false).unwrap();
    let selection = resolve_selection(&compilation, &set, None).unwrap();

    let plan = Planner::new(adapter, None)
        .plan(&compilation, &selection, None, &PlanOptions::default())
        .await
        .unwrap();

    let by_id: BTreeMap<&str, _> = plan
        .models
        .iter()
        .map(|model| (model.id.as_str(), model))
        .collect();
    // assay.raw is pulled in by dependency closure, reporting.monthly by
    // the `+` expansion.
    assert_eq!(by_id.len(), 3, "{:?}", by_id.keys());
    assert_eq!(by_id["assay.results"].membership, Membership::Selected);
    assert_eq!(by_id["reporting.monthly"].membership, Membership::Expanded);
    assert!(
        by_id["reporting.monthly"]
            .reasons
            .iter()
            .any(|reason| reason.kind == ReasonKind::SelectionExpansion
                && reason.detail.contains("assay.results+")),
        "{:?}",
        by_id["reporting.monthly"].reasons
    );
    assert_eq!(by_id["assay.raw"].membership, Membership::Dependency);
    assert_eq!(
        plan.selection.expanded,
        vec!["reporting.monthly".to_string()]
    );
}

/// `--exclude` wins over dependency closure: the excluded model is not
/// planned, and a warning explains the gap — but only when the excluded
/// relation actually exists to be read.
#[tokio::test]
async fn excluded_dependency_stays_out_and_warns() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    // A previous materialisation of assay.raw exists to read from.
    adapter
        .existing
        .lock()
        .unwrap()
        .insert("assay.raw".to_string());
    let set = SelectorSet::parse(
        &["assay.results".to_string()],
        &["assay.raw".to_string()],
        &[],
        false,
        false,
    )
    .unwrap();
    let selection = resolve_selection(&compilation, &set, None).unwrap();

    let plan = Planner::new(adapter, None)
        .plan(&compilation, &selection, None, &PlanOptions::default())
        .await
        .unwrap();

    assert_eq!(plan.models.len(), 1, "{:?}", plan.models);
    assert_eq!(plan.models[0].id, "assay.results");
    assert_eq!(plan.selection.exclude, vec!["assay.raw".to_string()]);
    assert!(
        plan.warnings
            .iter()
            .any(|warning| warning.contains("excluded model assay.raw")),
        "{:?}",
        plan.warnings
    );
}

/// Excluding a required dependency that was never materialised would
/// schedule a model reading a relation that does not exist — the plan must
/// refuse rather than produce an impossible run.
#[tokio::test]
async fn excluding_a_missing_dependency_fails_the_plan() {
    let compilation = project_with_tests();
    // Fresh adapter: nothing exists.
    let adapter = Arc::new(FakeAdapter::default());
    let set = SelectorSet::parse(
        &["assay.results".to_string()],
        &["assay.raw".to_string()],
        &[],
        false,
        false,
    )
    .unwrap();
    let selection = resolve_selection(&compilation, &set, None).unwrap();

    let error = Planner::new(adapter, None)
        .plan(&compilation, &selection, None, &PlanOptions::default())
        .await
        .expect_err("plan must be rejected");
    assert!(
        matches!(error, phlo_transform_engine::EngineError::InvalidPlan(_)),
        "{error:?}"
    );
    assert!(
        error.to_string().contains("excluded model assay.raw"),
        "{error}"
    );
}

/// Ephemeral models are never materialised, so state-derived `changed`
/// must not report them — otherwise they re-seed `changed+` expansion on
/// every run. An ephemeral edit still surfaces through its dependents'
/// dependency-version input.
#[tokio::test]
async fn changed_models_ignores_ephemeral_models() {
    let adapter = Arc::new(FakeAdapter::default());
    let state: Arc<dyn StateStore> = Arc::new(SqliteStateStore::in_memory().unwrap());
    let environment = Some("dev".to_string());

    // materialised -> ephemeral -> materialised
    let build = |ephemeral_sql: &str| {
        let mut ephemeral = model("assay.stg", ephemeral_sql);
        ephemeral.config.materialization = Materialization::Ephemeral;
        compile_models(vec![
            model("assay.raw", "select 1 as id"),
            ephemeral,
            model("assay.results", "select * from assay.stg"),
        ])
    };

    let first = build("select id from assay.raw");
    let selected = Selection::all(&first);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &first,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
        .await
        .unwrap();
    // The ephemeral is never planned itself.
    assert!(plan.models.iter().all(|model| model.id != "assay.stg"));
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

    // After a clean run nothing is changed — including the ephemeral,
    // which has no recorded version by definition.
    let changed = changed_models(&first, Some(&state), environment.as_deref()).unwrap();
    assert!(changed.is_empty(), "{changed:?}");

    // Editing the ephemeral's SQL changes the downstream materialised
    // model's version without the ephemeral itself appearing.
    let second = build("select id, id * 2 as doubled from assay.raw");
    let changed = changed_models(&second, Some(&state), environment.as_deref()).unwrap();
    let names: BTreeSet<String> = changed.iter().map(|id| id.logical_name()).collect();
    assert_eq!(
        names,
        ["assay.results"].into_iter().map(String::from).collect()
    );
}

/// `changed_models` feeds the `changed` selector: after a run nothing is
/// changed; a SQL edit changes the edited model and — because dependency
/// versions are a version input — its dependent.
#[tokio::test]
async fn changed_models_reflects_recorded_state() {
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let state: Arc<dyn StateStore> = Arc::new(SqliteStateStore::in_memory().unwrap());
    let environment = Some("dev".to_string());

    let build = |raw_sql: &str| {
        compile_models(vec![
            model("assay.raw", raw_sql),
            model("assay.results", "select * from assay.raw"),
            model("analytics.other", "select 2 as id"),
        ])
    };

    let first = build("select 1 as id");
    let selected = Selection::all(&first);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &first,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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

    let changed = changed_models(&first, Some(&state), environment.as_deref()).unwrap();
    assert!(changed.is_empty(), "{changed:?}");

    let second = build("select 1 as id, 'x' as extra");
    let changed = changed_models(&second, Some(&state), environment.as_deref()).unwrap();
    let names: BTreeSet<String> = changed.iter().map(|id| id.logical_name()).collect();
    assert_eq!(
        names,
        ["assay.raw", "assay.results"]
            .into_iter()
            .map(String::from)
            .collect()
    );
}

/// When an upstream's SQL changes, the dependent's plan reason names the
/// moved input — the `VersionDetail` recorded at materialisation time makes
/// the diff specific rather than "something upstream changed".
#[tokio::test]
async fn dependency_change_reason_names_the_moved_input() {
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let environment = Some("dev".to_string());

    let build = |raw_sql: &str| {
        compile_models(vec![
            model("assay.raw", raw_sql),
            model("assay.results", "select * from assay.raw"),
        ])
    };

    let first = build("select 1 as id");
    let selected = Selection::all(&first);
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(
            &first,
            &selected,
            environment.clone(),
            &PlanOptions::default(),
        )
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

    let second = build("select 1 as id, 'x' as extra");
    let plan = Planner::new(adapter.clone(), Some(state.clone()))
        .plan(&second, &selected, environment, &PlanOptions::default())
        .await
        .unwrap();
    let results = plan
        .models
        .iter()
        .find(|model| model.id == "assay.results")
        .unwrap();
    assert_eq!(results.action, PlanAction::Build);
    let reason = results
        .reasons
        .iter()
        .find(|reason| reason.kind == ReasonKind::DependencyChange)
        .expect("a dependency-change reason");
    assert!(reason.detail.contains("assay.raw"), "{}", reason.detail);
    assert_eq!(reason.subject.as_deref(), Some("assay.raw"));
}

// ---------------------------------------------------------------------
// Execution resilience: retries, fail-fast, timeouts, resume, retry.
// ---------------------------------------------------------------------

/// A retry policy with millisecond-scale backoff for tests.
fn retrying(retries: u32) -> RunOptions {
    RunOptions {
        retry: RetryPolicy {
            retries,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(10),
        },
        ..Default::default()
    }
}

fn model_result<'a>(result: &'a RunResult, name: &str) -> &'a ModelResult {
    result
        .models
        .iter()
        .find(|model| model.model == name)
        .unwrap_or_else(|| panic!("no model result for {name}"))
}

#[tokio::test]
async fn transient_adapter_failure_retries_then_succeeds() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_first(
        "assay.raw",
        1,
        AdapterError::new("TRINO_TRANSPORT", "temporary network failure").retryable(),
    );
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &retrying(2))
        .await
        .unwrap();

    let raw = model_result(&result, "assay.raw");
    assert_eq!(raw.status, ExecutionStatus::Passed);
    assert_eq!(raw.attempts.len(), 2, "{:?}", raw.attempts);
    assert_eq!(raw.attempts[0].attempt, 1);
    assert!(raw.attempts[0].failure.is_some());
    assert_eq!(adapter.attempts("assay.raw"), 2);
    assert!(result.events.iter().any(|event| matches!(
        event,
        EngineEvent::ModelRetrying { model, attempt: 1, .. } if model == "assay.raw"
    )));
    assert_eq!(result.status, ExecutionStatus::Passed);
}

#[tokio::test]
async fn sql_errors_fail_once_and_are_not_retried() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_with(
        "assay.results",
        AdapterError::new("COLUMN_NOT_FOUND", "column `titre` does not exist"),
    );
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &retrying(5))
        .await
        .unwrap();

    let failed = model_result(&result, "assay.results");
    assert_eq!(failed.status, ExecutionStatus::Failed);
    assert_eq!(adapter.attempts("assay.results"), 1);
    assert_eq!(failed.attempts.len(), 1);
    let failure = failed.failure.as_ref().expect("structured failure");
    assert_eq!(failure.category, FailureCategory::Sql);
    assert_eq!(failure.adapter_code.as_deref(), Some("COLUMN_NOT_FOUND"));
    assert!(!failure.retryable);
    assert_eq!(
        model_result(&result, "reporting.monthly").status,
        ExecutionStatus::Blocked
    );
    assert_eq!(result.status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn retry_exhaustion_records_every_attempt() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_with(
        "assay.raw",
        AdapterError::new("TRINO_TRANSPORT", "warehouse unreachable").retryable(),
    );
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &retrying(2))
        .await
        .unwrap();

    let raw = model_result(&result, "assay.raw");
    assert_eq!(raw.status, ExecutionStatus::Failed);
    assert_eq!(adapter.attempts("assay.raw"), 3);
    assert_eq!(raw.attempts.len(), 3);
    assert_eq!(
        raw.attempts
            .iter()
            .map(|attempt| attempt.attempt)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(raw.attempts.iter().all(|attempt| attempt.failure.is_some()));
    let failure = raw.failure.as_ref().unwrap();
    assert_eq!(failure.category, FailureCategory::Adapter);
    assert_eq!(failure.attempt, 3);
}

#[tokio::test]
async fn fail_fast_stops_scheduling_and_cancels_inflight() {
    let compilation = compile(&SemanticProject::in_memory(vec![
        model("main.a", "select * from external.a"),
        model("main.b", "select * from external.b"),
        model("main.c", "select * from main.b"),
        model("main.d", "select * from main.a"),
    ]));
    assert!(compilation.is_ok());
    let adapter = Arc::new(FakeAdapter::default());
    adapter.fail_with("main.a", AdapterError::new("FAKE001", "boom"));
    adapter.slow_target("main.b", 500);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                concurrency: 2,
                run_tests: false,
                fail_fast: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        model_result(&result, "main.a").status,
        ExecutionStatus::Failed
    );
    // `d` can never run — it is blocked, not just unscheduled.
    assert_eq!(
        model_result(&result, "main.d").status,
        ExecutionStatus::Blocked
    );
    // `b` was in flight when the failure landed and `c` never started: both
    // are cancelled under fail-fast.
    assert_eq!(
        model_result(&result, "main.b").status,
        ExecutionStatus::Cancelled
    );
    assert_eq!(
        model_result(&result, "main.c").status,
        ExecutionStatus::Cancelled
    );
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert_eq!(result.counts.failed, 1);
    assert_eq!(result.counts.blocked, 1);
    assert_eq!(result.counts.cancelled, 2);
}

#[tokio::test]
async fn model_timeout_is_classified_distinctly() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.slow_target("assay.raw", 10_000);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                run_tests: false,
                model_timeout: Some(Duration::from_millis(50)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let raw = model_result(&result, "assay.raw");
    assert_eq!(raw.status, ExecutionStatus::Failed);
    let failure = raw.failure.as_ref().unwrap();
    assert_eq!(failure.category, FailureCategory::Timeout);
    assert_eq!(failure.attempt, 1);
    // The timeout blocks dependents but the independent branch still ran.
    assert_eq!(
        model_result(&result, "assay.results").status,
        ExecutionStatus::Blocked
    );
    assert_eq!(
        model_result(&result, "analytics.d").status,
        ExecutionStatus::Passed
    );
    assert_eq!(result.status, ExecutionStatus::Failed);
}

#[tokio::test]
async fn run_progress_is_persisted_incrementally() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_with("assay.results", AdapterError::new("FAKE001", "boom"));
    let plan = plan_all(&compilation, adapter.clone()).await;

    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .unwrap();

    // The run record carries its manifest and final status.
    let stored = state.run(&result.run_id).unwrap().expect("run persisted");
    assert_eq!(stored.record.status, ExecutionStatus::Failed);
    assert!(stored.record.finished_at.is_some());
    assert!(stored.plan.is_some(), "plan manifest persisted for resume");
    assert_eq!(stored.record.failed_count, 1);

    // Per-model progress: every node has a record with the right status and
    // failure classification.
    let records = state.model_runs(&result.run_id).unwrap();
    let status_of = |name: &str| {
        records
            .iter()
            .find(|record| record.model_id == name)
            .map(|record| record.status)
    };
    assert_eq!(status_of("assay.raw"), Some(ExecutionStatus::Passed));
    assert_eq!(status_of("assay.results"), Some(ExecutionStatus::Failed));
    assert_eq!(
        status_of("reporting.monthly"),
        Some(ExecutionStatus::Blocked)
    );
    let failed = records
        .iter()
        .find(|record| record.model_id == "assay.results")
        .unwrap();
    assert_eq!(failed.error_category.as_deref(), Some("adapter"));
    assert_eq!(failed.attempts.len(), 1);
    assert!(!failed.desired_version.is_empty());

    // Only genuinely built models are recorded as materialised.
    assert!(state
        .materialized_version("assay.results", None)
        .unwrap()
        .is_none());
    assert!(state
        .materialized_version("assay.raw", None)
        .unwrap()
        .is_some());

    // Prefix lookup resolves the run for resume/retry commands.
    let found = state.find_runs(&result.run_id[..8]).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].run_id, result.run_id);
}

#[tokio::test]
async fn resume_reuses_passed_work_and_reruns_the_rest() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_with("assay.results", AdapterError::new("FAKE001", "boom"));
    adapter.slow_target("analytics.e", 1500);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let plan = plan_all(&compilation, adapter.clone()).await;

    // Kill the run mid-flight: `assay.results` has already failed (blocking
    // `reporting.monthly`); `analytics.e` on the independent branch was
    // still building — its record stays `running`.
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &compilation,
        &plan,
        RunOptions::default(),
        Duration::from_millis(250),
    )
    .await;
    let created_before = adapter.created.lock().unwrap().len();

    adapter.heal("assay.results");
    let resumed = runner
        .resume(&compilation, &run_id[..8], &RunOptions::default())
        .await
        .unwrap();

    // Same run id — the run was continued, not replaced.
    assert_eq!(resumed.run_id, run_id);
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(
        model_result(&resumed, "assay.raw").status,
        ExecutionStatus::Cached
    );
    assert_eq!(
        model_result(&resumed, "analytics.d").status,
        ExecutionStatus::Cached
    );
    assert_eq!(
        model_result(&resumed, "assay.results").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        model_result(&resumed, "reporting.monthly").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        model_result(&resumed, "analytics.e").status,
        ExecutionStatus::Passed
    );
    // Only the failed/interrupted work re-executed against the adapter.
    let created = adapter.created.lock().unwrap().clone();
    let mut new_creates: Vec<&String> = created[created_before..].iter().collect();
    new_creates.sort();
    assert_eq!(
        new_creates,
        vec!["analytics.e", "assay.results", "reporting.monthly"]
    );

    // The model record accumulated attempts across invocations.
    let records = state.model_runs(&resumed.run_id).unwrap();
    let results = records
        .iter()
        .find(|record| record.model_id == "assay.results")
        .unwrap();
    assert_eq!(results.status, ExecutionStatus::Passed);
    assert_eq!(results.attempts.len(), 2, "{:?}", results.attempts);
    assert_eq!(results.attempts[1].attempt, 2);

    // Still exactly one run in history.
    assert_eq!(state.runs().unwrap().len(), 1);
}

#[tokio::test]
async fn resume_does_not_trust_stale_success_when_versions_changed() {
    // run1: `main.a` passes; the process dies while `main.b` builds.
    let build = |a_sql: &str| {
        let compilation = compile(&SemanticProject::in_memory(vec![
            model("main.a", a_sql),
            model("main.b", "select * from main.a"),
        ]));
        assert!(compilation.is_ok());
        compilation
    };
    let compilation = build("select 1 as id from external.a");
    let adapter = Arc::new(FakeAdapter::default());
    adapter.slow_target("main.b", 1500);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let plan = plan_all(&compilation, adapter.clone()).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &compilation,
        &plan,
        RunOptions::default(),
        Duration::from_millis(250),
    )
    .await;

    // `main.a` changed since the run — its earlier success is stale and
    // must not be reused.
    let changed = build("select 1 as id, 'x' as extra from external.a");
    let resumed = runner
        .resume(&changed, &run_id[..8], &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(
        model_result(&resumed, "main.a").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        adapter.attempts("main.a"),
        2,
        "stale success must not be reused"
    );
}

#[tokio::test]
async fn resume_refuses_an_incompatible_workspace() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.slow_target("reporting.monthly", 1500);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let plan = plan_all(&compilation, adapter.clone()).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &compilation,
        &plan,
        RunOptions::default(),
        Duration::from_millis(250),
    )
    .await;

    // A model the run executed no longer exists — refuse rather than guess.
    let shrunk = compile(&SemanticProject::in_memory(vec![
        model("assay.raw", "select * from external.raw_assay_results"),
        model("assay.results", "select * from assay.raw"),
    ]));
    let error = runner
        .resume(&shrunk, &run_id[..8], &RunOptions::default())
        .await
        .expect_err("incompatible resume refused");
    assert!(error.to_string().contains("no longer exists"), "{error}");
}

#[tokio::test]
async fn resume_redirects_a_finished_failed_run_to_retry_failed() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_with("assay.results", AdapterError::new("FAKE001", "boom"));
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let first = runner
        .apply(
            &compilation,
            &plan_all(&compilation, adapter.clone()).await,
            &RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(first.status, ExecutionStatus::Failed);

    // A finished run's history is immutable: resume refuses and points at
    // --retry-failed instead of rewriting it.
    let error = runner
        .resume(&compilation, &first.run_id[..8], &RunOptions::default())
        .await
        .expect_err("a finished run is not resumable");
    assert!(error.to_string().contains("--retry-failed"), "{error}");
}

#[tokio::test]
async fn retry_failed_reruns_only_the_failed_portion() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    adapter.fail_with("assay.results", AdapterError::new("FAKE001", "boom"));
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));

    let first = runner
        .apply(
            &compilation,
            &plan_all(&compilation, adapter.clone()).await,
            &RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(first.status, ExecutionStatus::Failed);

    adapter.heal("assay.results");
    let retry = runner
        .retry_failed(&compilation, &first.run_id[..8], &RunOptions::default())
        .await
        .unwrap();

    // A new run continuing the old one.
    assert_ne!(retry.run_id, first.run_id);
    assert_eq!(retry.continued_from.as_deref(), Some(first.run_id.as_str()));
    // The failed/blocked models rerun and pass; `assay.raw` is pulled in as
    // a dependency of `assay.results` but already materialised, so it is
    // skipped without touching the adapter.
    let failed = model_result(&retry, "assay.results");
    assert_eq!(failed.status, ExecutionStatus::Passed);
    assert_eq!(
        model_result(&retry, "reporting.monthly").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        model_result(&retry, "assay.raw").status,
        ExecutionStatus::Skipped
    );
    assert_eq!(
        adapter.attempts("assay.raw"),
        1,
        "the passed upstream must not rebuild"
    );
    // The healthy independent branch is not part of the retry at all.
    assert!(!retry
        .models
        .iter()
        .any(|model| model.model.starts_with("analytics.")));
    assert_eq!(retry.status, ExecutionStatus::Passed);
    assert_eq!(state.runs().unwrap().len(), 2);

    // A second retry of the same run finds the failed portion already
    // materialised — it refuses rather than producing an all-skip no-op.
    let error = runner
        .retry_failed(&compilation, &first.run_id[..8], &RunOptions::default())
        .await
        .expect_err("nothing left to retry");
    assert!(error.to_string().contains("nothing to retry"), "{error}");
}

#[tokio::test]
async fn retry_failed_refuses_an_unfinished_run() {
    // Simulate a killed process: a run row that never finished.
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let compilation = project_with_tests();
    let plan = plan_all(&compilation, Arc::new(FakeAdapter::default())).await;
    state
        .start_run(
            &phlo_transform_engine::RunRecord {
                run_id: "deadbeef-0000-0000-0000-000000000000".to_string(),
                plan_id: plan.id.clone(),
                environment: None,
                started_at: "2026-01-01T00:00:00Z".to_string(),
                finished_at: None,
                status: ExecutionStatus::Running,
                model_count: plan.models.len(),
                failed_count: 0,
            },
            &phlo_transform_engine::StoredPlan {
                plan_id: plan.id.clone(),
                environment: None,
                models: Vec::new(),
                seeds: Vec::new(),
                tests: Vec::new(),
            },
        )
        .unwrap();

    let adapter = Arc::new(FakeAdapter::default());
    let runner = Runner::new(adapter, Some(state));
    let error = runner
        .retry_failed(&compilation, "deadbeef", &RunOptions::default())
        .await
        .expect_err("unfinished runs must be resumed, not retried");
    assert!(error.to_string().contains("--resume"), "{error}");
}

#[tokio::test]
async fn cancelled_run_can_be_resumed() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::with_delay(60));
    adapter.set_test_rows(0);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));

    let cancel = CancelHandle::default();
    let signal = cancel.clone();
    let options = RunOptions {
        concurrency: 1,
        cancel,
        ..Default::default()
    };
    let first = {
        let runner = Runner::new(adapter.clone(), Some(state.clone()));
        let compilation = compilation.clone();
        let plan = plan.clone();
        let handle = tokio::spawn(async move { runner.apply(&compilation, &plan, &options).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        signal.cancel();
        handle.await.unwrap().unwrap()
    };
    assert_eq!(first.status, ExecutionStatus::Cancelled);

    // Resume the cancelled run: cancelled/not-started work runs now.
    let resumed = runner
        .resume(&compilation, &first.run_id[..8], &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert!(resumed
        .models
        .iter()
        .all(|model| model.status.is_satisfied()));
}

#[tokio::test]
async fn a_failed_test_fails_the_run_not_the_model() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(3);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .unwrap();

    assert_eq!(result.status, ExecutionStatus::Failed);
    // The model execution itself passed — only the assertion failed.
    assert!(result
        .models
        .iter()
        .all(|model| model.status == ExecutionStatus::Passed));
    assert!(result
        .tests
        .iter()
        .all(|test| test.status == ExecutionStatus::Failed));
    let failure = result.tests[0].failure.as_ref().unwrap();
    assert_eq!(failure.category, FailureCategory::Test);
    // And the materialisation was recorded — the table really was built.
    assert!(state
        .materialized_version("assay.results", None)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn seed_loads_retry_under_the_same_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();
    let mut project =
        SemanticProject::in_memory(vec![model("main.events", "select * from raw.raw_events")]);
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
    adapter.fail_first(
        "raw.raw_events",
        1,
        AdapterError::new("TRINO_TRANSPORT", "transient").retryable(),
    );
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &retrying(1))
        .await
        .unwrap();

    assert_eq!(result.seeds.len(), 1);
    assert_eq!(result.seeds[0].status, ExecutionStatus::Passed);
    assert_eq!(result.seeds[0].attempts.len(), 2);
    assert!(result.events.iter().any(|event| matches!(
        event,
        EngineEvent::SeedRetrying { seed, attempt: 1, .. } if seed == "raw_events"
    )));
    assert_eq!(result.status, ExecutionStatus::Passed);
}

#[tokio::test]
async fn seed_failure_blocks_consumers_and_fails_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();
    let mut project = SemanticProject::in_memory(vec![
        model("main.events", "select * from raw.raw_events"),
        model("main.other", "select * from external.other"),
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
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .unwrap();

    assert_eq!(result.seeds[0].status, ExecutionStatus::Failed);
    // The consumer is blocked — it never read a missing seed table.
    assert_eq!(
        model_result(&result, "main.events").status,
        ExecutionStatus::Blocked
    );
    assert_eq!(
        model_result(&result, "main.events")
            .failure
            .as_ref()
            .unwrap()
            .category,
        FailureCategory::Dependency
    );
    // The independent model still ran.
    assert_eq!(
        model_result(&result, "main.other").status,
        ExecutionStatus::Passed
    );
    assert_eq!(result.status, ExecutionStatus::Failed);
    assert!(adapter
        .created
        .lock()
        .unwrap()
        .iter()
        .all(|target| target != "main.events"));
}

#[tokio::test]
async fn results_follow_plan_order_not_completion_order() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    // Give an early model a longer delay so completion order would differ
    // from plan order if results were unordered.
    adapter.slow_target("analytics.d", 50);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                concurrency: 4,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let result_order: Vec<&str> = result
        .models
        .iter()
        .map(|model| model.model.as_str())
        .collect();
    let plan_order: Vec<&str> = plan.models.iter().map(|model| model.id.as_str()).collect();
    assert_eq!(result_order, plan_order);
}

#[tokio::test]
async fn resume_replans_a_model_skipped_in_the_interrupted_run() {
    // run 1 builds and passes; run 2 is killed while the changed `main.b`
    // builds, leaving `main.a` recorded as skipped.
    let build = |a_sql: &str, b_sql: &str| {
        let compilation = compile(&SemanticProject::in_memory(vec![
            model("main.a", a_sql),
            model("main.b", b_sql),
        ]));
        assert!(compilation.is_ok());
        compilation
    };
    let v1 = build(
        "select 1 as id from external.a",
        "select 2 as id from external.b",
    );
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    runner
        .apply(
            &v1,
            &plan_all(&v1, adapter.clone()).await,
            &RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(adapter.attempts("main.a"), 1);

    // run 2: `main.a` unchanged → skipped; `main.b` rebuilds slowly and the
    // process dies mid-build.
    let v2 = build(
        "select 1 as id from external.a",
        "select 2 as id, 'x' as extra from external.b",
    );
    adapter.slow_target("main.b", 1500);
    let plan2 = plan_all_with_state(&v2, adapter.clone(), state.clone(), None).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &v2,
        &plan2,
        RunOptions::default(),
        Duration::from_millis(250),
    )
    .await;
    let record = state
        .model_runs(&run_id)
        .unwrap()
        .into_iter()
        .find(|record| record.model_id == "main.a")
        .unwrap();
    assert_eq!(record.status, ExecutionStatus::Skipped);

    // `main.a` then changes — resuming must not keep its stale skip.
    let v3 = build(
        "select 7 as id from external.a",
        "select 2 as id, 'x' as extra from external.b",
    );
    let resumed = runner
        .resume(&v3, &run_id[..8], &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(
        model_result(&resumed, "main.a").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        adapter.attempts("main.a"),
        2,
        "a stale skip must become a build when the version moved"
    );
}

#[tokio::test]
async fn resume_replans_a_model_cached_in_the_interrupted_run() {
    // `dev` materialises both models; the `prod` run is killed mid-`main.b`
    // (changed for prod so it builds) leaving `main.a` cached from `dev`'s
    // identical version.
    let build = |a_sql: &str, b_sql: &str| {
        let compilation = compile(&SemanticProject::in_memory(vec![
            model("main.a", a_sql),
            model("main.b", b_sql),
        ]));
        assert!(compilation.is_ok());
        compilation
    };
    let v1 = build(
        "select 1 as id from external.a",
        "select 2 as id from external.b",
    );
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let dev = RunOptions {
        environment: Some("dev".to_string()),
        ..Default::default()
    };
    runner
        .apply(&v1, &plan_all(&v1, adapter.clone()).await, &dev)
        .await
        .unwrap();
    assert_eq!(adapter.attempts("main.a"), 1);

    // `main.b` differs from the dev build so the prod run has real work to
    // interrupt; `main.a` is unchanged so prod plans it as a cross-env
    // cache hit.
    let v2 = build(
        "select 1 as id from external.a",
        "select 3 as id from external.b",
    );
    let prod = RunOptions {
        environment: Some("prod".to_string()),
        ..Default::default()
    };
    adapter.slow_target("main.b", 1500);
    let plan2 = plan_all_with_state(
        &v2,
        adapter.clone(),
        state.clone(),
        Some("prod".to_string()),
    )
    .await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &v2,
        &plan2,
        prod.clone(),
        Duration::from_millis(250),
    )
    .await;
    let record = state
        .model_runs(&run_id)
        .unwrap()
        .into_iter()
        .find(|record| record.model_id == "main.a")
        .unwrap();
    assert_eq!(record.status, ExecutionStatus::Cached);

    // `main.a` then changes: the new version was never materialised
    // anywhere — resume must build it rather than keep the stale cache.
    let v3 = build(
        "select 9 as id from external.a",
        "select 3 as id from external.b",
    );
    let resumed = runner.resume(&v3, &run_id[..8], &prod).await.unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(
        adapter.attempts("main.a"),
        2,
        "a stale cache hit must become a build when the version moved"
    );
}

#[tokio::test]
async fn resume_reloads_a_seed_skipped_in_the_interrupted_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("seeds")).unwrap();
    std::fs::write(
        dir.path().join("seeds/raw_events.csv"),
        "id,status\n1,placed\n",
    )
    .unwrap();
    let build = |hash: &str, slow_sql: &str| {
        let mut project = SemanticProject::in_memory(vec![
            model("main.events", "select * from raw.raw_events"),
            model("main.slow", slow_sql),
        ]);
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
    let runner = Runner::new(adapter.clone(), Some(state.clone()));

    // run 1: seed loads, models build.
    let v1 = build("hash-v1", "select 1 as id from external.slow");
    runner
        .apply(
            &v1,
            &plan_all(&v1, adapter.clone()).await,
            &RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(adapter.loaded_csvs.lock().unwrap().len(), 1);

    // run 2 (interrupted): seed unchanged → skipped; `main.slow` changed so
    // it rebuilds — the process dies mid-build.
    let v2 = build("hash-v1", "select 2 as id from external.slow");
    adapter.slow_target("main.slow", 1500);
    let plan2 = plan_all_with_state(&v2, adapter.clone(), state.clone(), None).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &v2,
        &plan2,
        RunOptions::default(),
        Duration::from_millis(250),
    )
    .await;
    let record = state
        .seed_runs(&run_id)
        .unwrap()
        .into_iter()
        .find(|record| record.name == "raw_events")
        .unwrap();
    assert_eq!(record.status, ExecutionStatus::Skipped);

    // The CSV then changed — resume must reload, not keep the stale skip.
    let v3 = build("hash-v2", "select 2 as id from external.slow");
    let resumed = runner
        .resume(&v3, &run_id[..8], &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(resumed.seeds[0].status, ExecutionStatus::Passed);
    assert_eq!(
        adapter.loaded_csvs.lock().unwrap().len(),
        2,
        "a changed seed must reload even though it was skipped in the run"
    );
}

#[tokio::test]
async fn retry_failed_reruns_a_failed_test() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    // Every test query returns rows — both tests fail while all models pass.
    adapter.set_test_rows(3);
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let first = runner
        .apply(
            &compilation,
            &plan_all(&compilation, adapter.clone()).await,
            &RunOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(first.status, ExecutionStatus::Failed);
    assert!(first
        .tests
        .iter()
        .all(|test| test.status == ExecutionStatus::Failed));

    adapter.set_test_rows(0);
    let retry = runner
        .retry_failed(&compilation, &first.run_id[..8], &RunOptions::default())
        .await
        .expect("test-only failures are retryable work");

    assert_ne!(retry.run_id, first.run_id);
    assert_eq!(retry.continued_from.as_deref(), Some(first.run_id.as_str()));
    assert!(retry
        .tests
        .iter()
        .all(|test| test.status == ExecutionStatus::Passed));
    // No model needed rebuilding — the failed tests rerun against the
    // already-materialised datasets.
    assert!(retry.models.iter().all(|model| model.status.is_satisfied()));
    assert_eq!(adapter.attempts("assay.raw"), 1, "nothing rebuilt");
    assert_eq!(retry.status, ExecutionStatus::Passed);
    assert_eq!(state.runs().unwrap().len(), 2);
}

#[tokio::test]
async fn a_killed_run_keeps_the_attempts_it_already_made() {
    // `main.flaky` fails once with a retryable error; a huge backoff parks
    // the run mid-retry — killing it there must still show the attempt.
    let compilation = compile(&SemanticProject::in_memory(vec![model(
        "main.flaky",
        "select 1 as id from external.a",
    )]));
    assert!(compilation.is_ok());
    let adapter = Arc::new(FakeAdapter::default());
    adapter.fail_first(
        "main.flaky",
        1,
        AdapterError::new("TRINO_TRANSPORT", "connection reset").retryable(),
    );
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let plan = plan_all(&compilation, adapter.clone()).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &compilation,
        &plan,
        RunOptions {
            retry: RetryPolicy {
                retries: 3,
                base_delay: Duration::from_secs(60),
                max_delay: Duration::from_secs(60),
            },
            ..Default::default()
        },
        Duration::from_millis(250),
    )
    .await;

    let records = state.model_runs(&run_id).unwrap();
    let record = records
        .iter()
        .find(|record| record.model_id == "main.flaky")
        .unwrap();
    assert_eq!(record.status, ExecutionStatus::Running);
    assert_eq!(
        record.attempts.len(),
        1,
        "the failed attempt must be persisted before backoff, not lost"
    );
    assert!(record.attempts[0]
        .failure
        .as_ref()
        .is_some_and(|failure| failure.retryable));

    // And resume continues the attempt numbering rather than restarting.
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let resumed = runner
        .resume(&compilation, &run_id[..8], &RunOptions::default())
        .await
        .unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    let record = state
        .model_runs(&run_id)
        .unwrap()
        .into_iter()
        .find(|record| record.model_id == "main.flaky")
        .unwrap();
    assert_eq!(record.attempts.len(), 2, "{:?}", record.attempts);
    assert_eq!(record.attempts[1].attempt, 2);
}

#[tokio::test]
async fn timeout_cancels_the_in_flight_query() {
    let compilation = compile(&SemanticProject::in_memory(vec![model(
        "main.slow",
        "select 1 as id from external.a",
    )]));
    assert!(compilation.is_ok());
    let adapter = Arc::new(FakeAdapter::default());
    adapter.slow_target("main.slow", 5000);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                model_timeout: Some(Duration::from_millis(100)),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let model = model_result(&result, "main.slow");
    assert_eq!(model.status, ExecutionStatus::Failed);
    assert_eq!(
        model.failure.as_ref().unwrap().category,
        FailureCategory::Timeout
    );
    assert!(
        adapter
            .cancelled_queries()
            .iter()
            .any(|query| query.ends_with(":main.slow")),
        "the in-flight warehouse query must be cancelled, not just dropped"
    );
}

#[tokio::test]
async fn fail_fast_cancels_in_flight_queries() {
    // `main.a` fails shortly after `main.b` starts building — fail-fast
    // aborts b's task *and* cancels its warehouse query.
    let compilation = compile(&SemanticProject::in_memory(vec![
        model("main.a", "select 1 as id from external.a"),
        model("main.b", "select 2 as id from external.b"),
    ]));
    assert!(compilation.is_ok());
    let adapter = Arc::new(FakeAdapter::default());
    adapter.fail_with("main.a", AdapterError::new("FAKE001", "boom"));
    adapter.slow_target("main.a", 100);
    adapter.slow_target("main.b", 5000);
    let plan = plan_all(&compilation, adapter.clone()).await;
    let runner = Runner::new(adapter.clone(), None);
    let result = runner
        .apply(
            &compilation,
            &plan,
            &RunOptions {
                concurrency: 2,
                fail_fast: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        model_result(&result, "main.a").status,
        ExecutionStatus::Failed
    );
    assert_eq!(
        model_result(&result, "main.b").status,
        ExecutionStatus::Cancelled
    );
    assert!(
        adapter
            .cancelled_queries()
            .iter()
            .any(|query| query.ends_with(":main.b")),
        "fail-fast must cancel in-flight warehouse queries"
    );
}

/// A store that accepts everything except `record_model` — a mid-run write
/// failure stand-in.
struct FailingStore {
    inner: SqliteStateStore,
}

impl FailingStore {
    fn new(inner: SqliteStateStore) -> Self {
        Self { inner }
    }
}

impl StateStore for FailingStore {
    fn start_run(&self, run: &RunRecord, plan: &StoredPlan) -> Result<(), EngineError> {
        self.inner.start_run(run, plan)
    }

    fn reopen_run(&self, run_id: &str) -> Result<(), EngineError> {
        self.inner.reopen_run(run_id)
    }

    fn finish_run(
        &self,
        run_id: &str,
        status: ExecutionStatus,
        finished_at: &str,
        failed_count: usize,
    ) -> Result<(), EngineError> {
        self.inner
            .finish_run(run_id, status, finished_at, failed_count)
    }

    fn record_model(&self, _record: &ModelRunRecord) -> Result<(), EngineError> {
        Err(EngineError::State("simulated write failure".to_string()))
    }

    fn record_seed_run(&self, record: &SeedRunRecord) -> Result<(), EngineError> {
        self.inner.record_seed_run(record)
    }

    fn record_test(&self, record: &TestRunRecord) -> Result<(), EngineError> {
        self.inner.record_test(record)
    }

    fn runs(&self) -> Result<Vec<RunSummary>, EngineError> {
        self.inner.runs()
    }

    fn latest_run(&self, environment: Option<&str>) -> Result<Option<RunSummary>, EngineError> {
        self.inner.latest_run(environment)
    }

    fn run(&self, run_id: &str) -> Result<Option<StoredRun>, EngineError> {
        self.inner.run(run_id)
    }

    fn find_runs(&self, prefix: &str) -> Result<Vec<RunSummary>, EngineError> {
        self.inner.find_runs(prefix)
    }

    fn model_runs(&self, run_id: &str) -> Result<Vec<ModelRunRecord>, EngineError> {
        self.inner.model_runs(run_id)
    }

    fn seed_runs(&self, run_id: &str) -> Result<Vec<SeedRunRecord>, EngineError> {
        self.inner.seed_runs(run_id)
    }

    fn test_runs(&self, run_id: &str) -> Result<Vec<TestRunRecord>, EngineError> {
        self.inner.test_runs(run_id)
    }

    fn record_materialized(&self, record: &MaterializedRecord) -> Result<(), EngineError> {
        self.inner.record_materialized(record)
    }

    fn materialized_version(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<MaterializedRecord>, EngineError> {
        self.inner.materialized_version(model_id, environment)
    }

    fn materialized_by_hash(
        &self,
        version_hash: &str,
    ) -> Result<Vec<MaterializedRecord>, EngineError> {
        self.inner.materialized_by_hash(version_hash)
    }

    fn materialized_in(
        &self,
        environment: Option<&str>,
    ) -> Result<Vec<MaterializedRecord>, EngineError> {
        self.inner.materialized_in(environment)
    }

    fn record_promotion(
        &self,
        record: &phlo_transform_engine::PromotionRecord,
    ) -> Result<(), EngineError> {
        self.inner.record_promotion(record)
    }

    fn promotions(&self) -> Result<Vec<phlo_transform_engine::PromotionRecord>, EngineError> {
        self.inner.promotions()
    }

    fn set_watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
        value: &str,
        run_id: &str,
    ) -> Result<(), EngineError> {
        self.inner
            .set_watermark(model_id, environment, value, run_id)
    }

    fn watermark(
        &self,
        model_id: &str,
        environment: Option<&str>,
    ) -> Result<Option<String>, EngineError> {
        self.inner.watermark(model_id, environment)
    }

    fn record_seed(&self, record: &SeedRecord) -> Result<(), EngineError> {
        self.inner.record_seed(record)
    }

    fn seed_state(
        &self,
        name: &str,
        environment: Option<&str>,
    ) -> Result<Option<SeedRecord>, EngineError> {
        self.inner.seed_state(name, environment)
    }

    fn seeds_in(&self, environment: Option<&str>) -> Result<Vec<SeedRecord>, EngineError> {
        self.inner.seeds_in(environment)
    }
}

#[tokio::test]
async fn a_state_write_failure_is_fatal_not_silent() {
    let compilation = project_with_tests();
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_test_rows(0);
    let store: Arc<dyn StateStore> =
        Arc::new(FailingStore::new(SqliteStateStore::in_memory().unwrap()));
    let runner = Runner::new(adapter.clone(), Some(store));
    let plan = plan_all(&compilation, adapter.clone()).await;
    let error = runner
        .apply(&compilation, &plan, &RunOptions::default())
        .await
        .expect_err("a failed state write must stop the run");
    assert!(matches!(error, EngineError::State(_)), "{error}");
}

#[tokio::test]
async fn resume_full_rebuilds_when_incremental_strategy_changes() {
    // run A completes: `assay.events` bootstraps as incremental key(id) and
    // its materialisation record stores the strategy. run B changes the SQL
    // and is killed while the unrelated `main.slow` builds. Before resume
    // the strategy changes to append — the stored plan's `full_rebuild`
    // flag is stale, so resume must re-plan and full-rebuild rather than
    // merge.
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let dev = RunOptions {
        environment: Some("dev".to_string()),
        run_tests: false,
        ..Default::default()
    };
    let provider = events_provider();
    let build = |sql: &str, slow_sql: &str, strategy: IncrementalStrategy| {
        let compilation = compile_with_options(
            &SemanticProject::in_memory(vec![
                incremental_with(sql, strategy),
                model("main.slow", slow_sql),
            ]),
            &provider,
            &EmptySourceStateProvider,
        );
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let v1 = build(
        "select id, updated_at from external.events",
        "select 1 as id",
        IncrementalStrategy::Key {
            columns: vec!["id".to_string()],
        },
    );
    run_once(adapter.clone(), state.clone(), &v1, "dev").await;
    let created_for = |adapter: &FakeAdapter, target: &str| {
        adapter
            .created
            .lock()
            .unwrap()
            .iter()
            .filter(|created| created.as_str() == target)
            .count()
    };
    assert_eq!(created_for(&adapter, "assay.events"), 1, "bootstrap");
    assert!(adapter.merges.lock().unwrap().is_empty());

    // run B: changed SQL → a merge is planned; the process dies while
    // `main.slow` is still building.
    // `main.slow` also changes so run B has real work to interrupt.
    let v2 = build(
        "select id, updated_at from external.events where id > 0",
        "select 2 as id",
        IncrementalStrategy::Key {
            columns: vec!["id".to_string()],
        },
    );
    adapter.slow_target("main.slow", 1500);
    let plan2 =
        plan_all_with_state(&v2, adapter.clone(), state.clone(), Some("dev".to_string())).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &v2,
        &plan2,
        dev.clone(),
        Duration::from_millis(250),
    )
    .await;
    adapter.delay_for.lock().unwrap().remove("main.slow");
    assert_eq!(adapter.merges.lock().unwrap().len(), 1, "run B merged");

    // merge(id) → append while the run was interrupted.
    let v3 = build(
        "select id, updated_at from external.events where id > 0",
        "select 2 as id",
        IncrementalStrategy::Append,
    );
    let resumed = runner.resume(&v3, &run_id[..8], &dev).await.unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(
        model_result(&resumed, "assay.events").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        adapter.merges.lock().unwrap().len(),
        1,
        "a strategy change must full-rebuild, not merge"
    );
    assert_eq!(
        created_for(&adapter, "assay.events"),
        2,
        "bootstrap + resumed full rebuild"
    );
}

#[tokio::test]
async fn resume_uses_the_current_watermark_for_time_window_models() {
    // run A completes and records watermark W1. run B changes the SQL and
    // is killed with W1 baked into its stored plan. An external process
    // advances the watermark to W2 before resume — the resumed append must
    // filter on W2, not the stored W1.
    let adapter = Arc::new(FakeAdapter::default());
    adapter.set_max_value("2026-09-10 00:00:00.000");
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let dev = RunOptions {
        environment: Some("dev".to_string()),
        run_tests: false,
        ..Default::default()
    };
    let provider = events_provider();
    let build = |sql: &str, slow_sql: &str| {
        let compilation = compile_with_options(
            &SemanticProject::in_memory(vec![
                incremental_with(
                    sql,
                    IncrementalStrategy::TimeWindow {
                        column: "updated_at".to_string(),
                        overlap_seconds: None,
                    },
                ),
                model("main.slow", slow_sql),
            ]),
            &provider,
            &EmptySourceStateProvider,
        );
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let v1 = build(
        "select id, updated_at from external.events",
        "select 1 as id",
    );
    run_once(adapter.clone(), state.clone(), &v1, "dev").await;
    assert_eq!(
        state.watermark("assay.events", Some("dev")).unwrap(),
        Some("2026-09-10 00:00:00.000".to_string())
    );

    // run B: both models change → a time-window append is planned against
    // W1 and `main.slow` builds; the process dies mid-`main.slow`.
    let v2 = build(
        "select id, updated_at from external.events where id > 0",
        "select 2 as id",
    );
    adapter.slow_target("main.slow", 1500);
    let plan2 =
        plan_all_with_state(&v2, adapter.clone(), state.clone(), Some("dev".to_string())).await;
    assert_eq!(
        plan2
            .models
            .iter()
            .find(|model| model.id == "assay.events")
            .unwrap()
            .watermark
            .as_deref(),
        Some("2026-09-10 00:00:00.000"),
        "the interrupted plan stored W1"
    );
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &v2,
        &plan2,
        dev.clone(),
        Duration::from_millis(250),
    )
    .await;
    adapter.delay_for.lock().unwrap().remove("main.slow");

    // The watermark moved on between the interruption and the resume, and
    // the model's SQL moved again so the earlier pass cannot be reused.
    state
        .set_watermark(
            "assay.events",
            Some("dev"),
            "2026-09-12 00:00:00.000",
            "external-process",
        )
        .unwrap();
    let v3 = build(
        "select id, updated_at from external.events where id > 1",
        "select 2 as id",
    );
    let resumed = runner.resume(&v3, &run_id[..8], &dev).await.unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    let appends = adapter.append_sqls.lock().unwrap().clone();
    let last = appends.last().expect("the resumed run appended");
    assert!(
        last.contains("2026-09-12"),
        "resume must read the current watermark, got: {last}"
    );
    assert!(
        !last.contains("2026-09-10"),
        "stale watermark leaked: {last}"
    );
}

#[tokio::test]
async fn resume_full_rebuilds_when_schema_change_requires_it() {
    // run A completes: `assay.events` is incremental append. run B changes
    // the SQL and is killed mid-`main.slow`. While interrupted the target
    // gains a column the desired schema drops — normal planning classifies
    // that as full-rebuild-required, and resume must reach the same verdict
    // instead of keeping the stored plan's append.
    let adapter = Arc::new(FakeAdapter::default());
    let state = Arc::new(SqliteStateStore::in_memory().unwrap());
    let runner = Runner::new(adapter.clone(), Some(state.clone()));
    let dev = RunOptions {
        environment: Some("dev".to_string()),
        run_tests: false,
        ..Default::default()
    };
    let provider = events_provider();
    let build = |sql: &str, slow_sql: &str| {
        let compilation = compile_with_options(
            &SemanticProject::in_memory(vec![
                incremental_with(sql, IncrementalStrategy::Append),
                model("main.slow", slow_sql),
            ]),
            &provider,
            &EmptySourceStateProvider,
        );
        assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
        compilation
    };

    let v1 = build(
        "select id, updated_at from external.events",
        "select 1 as id",
    );
    run_once(adapter.clone(), state.clone(), &v1, "dev").await;
    let created_for = |adapter: &FakeAdapter, target: &str| {
        adapter
            .created
            .lock()
            .unwrap()
            .iter()
            .filter(|created| created.as_str() == target)
            .count()
    };
    assert_eq!(created_for(&adapter, "assay.events"), 1, "bootstrap");

    // run B: both models change → an append is planned and `main.slow`
    // rebuilds; killed mid-`main.slow`.
    let v2 = build(
        "select id, updated_at from external.events where id > 0",
        "select 2 as id",
    );
    adapter.slow_target("main.slow", 1500);
    let plan2 =
        plan_all_with_state(&v2, adapter.clone(), state.clone(), Some("dev".to_string())).await;
    let run_id = interrupt_apply(
        adapter.clone(),
        state.clone(),
        &v2,
        &plan2,
        dev.clone(),
        Duration::from_millis(250),
    )
    .await;
    adapter.delay_for.lock().unwrap().remove("main.slow");

    // The target gained a legacy column the desired schema drops — an
    // unsafe append that normal planning turns into a full rebuild.
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
    let v3 = build(
        "select id, updated_at from external.events where id > 1",
        "select 2 as id",
    );
    let appends_before = adapter.appends.lock().unwrap().len();
    let resumed = runner.resume(&v3, &run_id[..8], &dev).await.unwrap();
    assert_eq!(resumed.status, ExecutionStatus::Passed);
    assert_eq!(
        model_result(&resumed, "assay.events").status,
        ExecutionStatus::Passed
    );
    assert_eq!(
        adapter.appends.lock().unwrap().len(),
        appends_before,
        "a full-rebuild-required schema change must not append"
    );
    assert_eq!(
        created_for(&adapter, "assay.events"),
        2,
        "bootstrap + resumed full rebuild"
    );
}

// ---------------------------------------------------------------------------
// Bundle 5: branch diffs and promotion records
// ---------------------------------------------------------------------------

fn version(hash: &str) -> phlo_transform_core::ModelVersion {
    phlo_transform_core::ModelVersion {
        hash: hash.to_string(),
        sql_hash: hash.to_string(),
        config_hash: "config".to_string(),
        contract_hash: "contract".to_string(),
        dependency_hash: "deps".to_string(),
        source_state_hash: "sources".to_string(),
        compiler_version: "0".to_string(),
        target_hash: "target".to_string(),
    }
}

fn materialized(model_id: &str, env: &str, hash: &str, target: &Relation) -> MaterializedRecord {
    MaterializedRecord {
        model_id: model_id.to_string(),
        environment: Some(env.to_string()),
        version: version(hash),
        detail: None,
        target: target.display(),
        incremental_strategy: None,
        incremental_key: None,
        run_id: "run-1".to_string(),
        materialized_at: "t".to_string(),
    }
}

/// The per-ref relation for a compiled model: same schema/table, the ref's
/// catalog — mirrors `branch_diff`'s retargeting of unrecorded targets.
fn ref_relation(catalog: &str, model: &phlo_transform_core::CompiledModel) -> Relation {
    Relation {
        catalog: Some(catalog.to_string()),
        schema: model.target.schema.clone(),
        table: model.target.table.clone(),
    }
}

fn branch_compilation() -> Compilation {
    let project = SemanticProject::in_memory(vec![
        model("main.same", "select 1 as id"),
        model("main.changed", "select * from main.same"),
        model("main.added", "select 1 as id"),
        model("main.inherited", "select 1 as id"),
        model("main.never", "select 1 as id"),
    ]);
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);
    compilation
}

fn branch_diff_request() -> BranchDiffRequest {
    BranchDiffRequest {
        candidate_ref: "ci/x".to_string(),
        base_ref: "main".to_string(),
        candidate_catalog: Some("phlo_ci_x".to_string()),
        base_catalog: Some("phlo_main".to_string()),
        deep: false,
        default_schema: Some("default".to_string()),
    }
}

#[tokio::test]
async fn branch_diff_classifies_datasets() {
    let compilation = branch_compilation();
    let adapter = Arc::new(FakeAdapter::default());
    let state = SqliteStateStore::in_memory().unwrap();
    let model = |name: &str| compilation.model_by_name(name).unwrap();

    // same: recorded on both refs with the same version; exists on both.
    let same_cand = ref_relation("phlo_ci_x", model("main.same"));
    let same_base = ref_relation("phlo_main", model("main.same"));
    state
        .record_materialized(&materialized("main.same", "ci/x", "v1", &same_cand))
        .unwrap();
    state
        .record_materialized(&materialized("main.same", "main", "v1", &same_base))
        .unwrap();
    adapter.existing.lock().unwrap().insert(same_cand.display());
    adapter.existing.lock().unwrap().insert(same_base.display());

    // changed: recorded on both with different versions.
    let changed_cand = ref_relation("phlo_ci_x", model("main.changed"));
    let changed_base = ref_relation("phlo_main", model("main.changed"));
    state
        .record_materialized(&materialized("main.changed", "ci/x", "v2", &changed_cand))
        .unwrap();
    state
        .record_materialized(&materialized("main.changed", "main", "v1", &changed_base))
        .unwrap();
    adapter
        .existing
        .lock()
        .unwrap()
        .insert(changed_cand.display());
    adapter
        .existing
        .lock()
        .unwrap()
        .insert(changed_base.display());

    // added: candidate only.
    let added_cand = ref_relation("phlo_ci_x", model("main.added"));
    state
        .record_materialized(&materialized("main.added", "ci/x", "v1", &added_cand))
        .unwrap();
    adapter
        .existing
        .lock()
        .unwrap()
        .insert(added_cand.display());

    // removed: materialised on base by a model no longer in the workspace —
    // the dataset surfaces through its record alone.
    let dropped_base = Relation {
        catalog: Some("phlo_main".to_string()),
        schema: "main".to_string(),
        table: "dropped".to_string(),
    };
    state
        .record_materialized(&materialized("main.dropped", "main", "v1", &dropped_base))
        .unwrap();
    adapter
        .existing
        .lock()
        .unwrap()
        .insert(dropped_base.display());

    // inherited: recorded on base only but visible on both refs — a Nessie
    // branch sees the base's table without rewriting it.
    let inh_cand = ref_relation("phlo_ci_x", model("main.inherited"));
    let inh_base = ref_relation("phlo_main", model("main.inherited"));
    state
        .record_materialized(&materialized("main.inherited", "main", "v1", &inh_base))
        .unwrap();
    adapter.existing.lock().unwrap().insert(inh_cand.display());
    adapter.existing.lock().unwrap().insert(inh_base.display());

    // never: in the workspace, materialised nowhere.
    let report = branch_diff(adapter, Some(&state), &compilation, &branch_diff_request())
        .await
        .unwrap();

    let status = |name: &str| {
        report
            .datasets
            .iter()
            .find(|dataset| dataset.dataset == name)
            .map(|dataset| dataset.status)
            .unwrap()
    };
    assert_eq!(status("main.same"), DatasetStatus::Unchanged);
    assert_eq!(status("main.changed"), DatasetStatus::Changed);
    assert_eq!(status("main.added"), DatasetStatus::Added);
    assert_eq!(status("main.dropped"), DatasetStatus::Removed);
    assert_eq!(status("main.inherited"), DatasetStatus::Unchanged);
    assert_eq!(status("main.never"), DatasetStatus::Absent);

    // Deterministic ordering by dataset name.
    let names: Vec<&str> = report
        .datasets
        .iter()
        .map(|dataset| dataset.dataset.as_str())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);

    // Lineage hooks: upstream model deps travel with the dataset entry.
    let changed = report
        .datasets
        .iter()
        .find(|dataset| dataset.dataset == "main.changed")
        .unwrap();
    assert_eq!(changed.upstream, vec!["main.same".to_string()]);
}

#[tokio::test]
async fn branch_diff_reports_schema_and_row_differences() {
    let compilation = branch_compilation();
    let adapter = Arc::new(FakeAdapter::default());
    let state = SqliteStateStore::in_memory().unwrap();
    let model = |name: &str| compilation.model_by_name(name).unwrap();

    let cand = ref_relation("phlo_ci_x", model("main.changed"));
    let base = ref_relation("phlo_main", model("main.changed"));
    state
        .record_materialized(&materialized("main.changed", "ci/x", "v2", &cand))
        .unwrap();
    state
        .record_materialized(&materialized("main.changed", "main", "v1", &base))
        .unwrap();
    adapter.existing.lock().unwrap().insert(cand.display());
    adapter.existing.lock().unwrap().insert(base.display());

    let col = |name: &str, data_type: &str, nullable: bool| ColumnInfo {
        name: name.to_string(),
        data_type: data_type.to_string(),
        nullable,
    };
    adapter.set_relation_columns(
        &cand.display(),
        vec![col("id", "bigint", false), col("label", "varchar", true)],
    );
    adapter.set_relation_columns(
        &base.display(),
        vec![
            col("id", "integer", false),
            col("label", "varchar", false),
            col("legacy", "varchar", true),
        ],
    );
    adapter.set_row_count(&cand, 150);
    adapter.set_row_count(&base, 100);

    let report = branch_diff(adapter, Some(&state), &compilation, &branch_diff_request())
        .await
        .unwrap();

    let schema = report
        .schema_changes
        .iter()
        .find(|entry| entry.model == "main.changed")
        .expect("schema diff for main.changed");
    let kinds: BTreeMap<&str, &str> = schema
        .changes
        .iter()
        .map(|change| (change.column.as_str(), change.kind.as_str()))
        .collect();
    assert_eq!(kinds.get("id"), Some(&"changed"));
    assert_eq!(kinds.get("label"), Some(&"nullability"));
    assert_eq!(kinds.get("legacy"), Some(&"removed"));

    let rows = report
        .rows
        .iter()
        .find(|entry| entry.dataset == "main.changed")
        .expect("row diff for main.changed");
    assert_eq!(rows.base_rows, Some(100));
    assert_eq!(rows.candidate_rows, Some(150));
    assert_eq!(rows.delta, Some(50));
}

#[tokio::test]
async fn branch_diff_deep_runs_keyed_diffs_for_changed_models() {
    let mut keyed = model("main.changed", "select * from main.same");
    keyed.config.incremental = Some(IncrementalStrategy::Key {
        columns: vec!["id".to_string()],
    });
    let project = SemanticProject::in_memory(vec![model("main.same", "select 1 as id"), keyed]);
    let compilation = compile(&project);
    assert!(compilation.is_ok(), "{:?}", compilation.diagnostics);

    let adapter = Arc::new(FakeAdapter::default());
    let state = SqliteStateStore::in_memory().unwrap();
    let changed = compilation.model_by_name("main.changed").unwrap();
    let cand = ref_relation("phlo_ci_x", changed);
    let base = ref_relation("phlo_main", changed);
    state
        .record_materialized(&materialized("main.changed", "ci/x", "v2", &cand))
        .unwrap();
    state
        .record_materialized(&materialized("main.changed", "main", "v1", &base))
        .unwrap();
    adapter.existing.lock().unwrap().insert(cand.display());
    adapter.existing.lock().unwrap().insert(base.display());

    let mut request = branch_diff_request();
    request.deep = true;
    let report = branch_diff(adapter, Some(&state), &compilation, &request)
        .await
        .unwrap();
    assert_eq!(report.diffs.len(), 1);
    assert_eq!(report.diffs[0].model, "main.changed");
    assert_eq!(report.diffs[0].key_columns, vec!["id".to_string()]);
    assert!(report.passed);
}

#[test]
fn promotion_records_persist_in_state() {
    let state = SqliteStateStore::in_memory().unwrap();
    let record = PromotionRecord {
        promotion_id: "promo-1".to_string(),
        candidate_ref: "ci/x".to_string(),
        candidate_hash: Some("bbb".to_string()),
        target_ref: "main".to_string(),
        target_hash_before: "aaa".to_string(),
        target_hash_after: Some("bbb".to_string()),
        plan_id: Some("plan-1".to_string()),
        run_id: Some("run-1".to_string()),
        dry_run: false,
        merged: true,
        conflicts: Vec::new(),
        gates: vec![phlo_transform_engine::GateResult {
            name: "run".to_string(),
            passed: true,
            detail: "run run-1 passed".to_string(),
        }],
        timestamp: "2024-01-01T00:00:00Z".to_string(),
        actor: None,
    };
    state.record_promotion(&record).unwrap();
    let promotions = state.promotions().unwrap();
    assert_eq!(promotions.len(), 1);
    assert_eq!(promotions[0].promotion_id, "promo-1");
    assert!(promotions[0].merged);
    assert_eq!(promotions[0].gates.len(), 1);
}

// ---------------------------------------------------------------------------
// Bundle 5: promotion gates
// ---------------------------------------------------------------------------

use phlo_transform_engine::{evaluate_gates, GateInput};
use phlo_transform_nessie::MergeOutcome;

fn passed_run() -> RunSummary {
    RunSummary {
        run_id: "run-1".to_string(),
        plan_id: "plan-1".to_string(),
        started_at: "t".to_string(),
        finished_at: Some("t".to_string()),
        status: ExecutionStatus::Passed,
        model_count: 2,
        failed_count: 0,
    }
}

fn model_run(model_id: &str, status: ExecutionStatus) -> ModelRunRecord {
    ModelRunRecord {
        run_id: "run-1".to_string(),
        model_id: model_id.to_string(),
        materialization: "table".to_string(),
        action: "build".to_string(),
        status,
        started_at: "t".to_string(),
        finished_at: "t".to_string(),
        sql_hash: "h".to_string(),
        target: format!("cat.main.{model_id}"),
        desired_version: "v".to_string(),
        attempts: Vec::new(),
        query_id: None,
        error: None,
        error_category: None,
    }
}

fn test_run(test_id: &str, status: ExecutionStatus) -> TestRunRecord {
    TestRunRecord {
        run_id: "run-1".to_string(),
        test_id: test_id.to_string(),
        status,
        row_count: 0,
        query_id: None,
        error: None,
        error_category: None,
        started_at: "t".to_string(),
        finished_at: "t".to_string(),
    }
}

/// A fully-green gate input: passed run, tests green, nothing blocked,
/// passing diff, stable target, clean merge check.
fn green_input() -> GateInput {
    GateInput {
        run: Some(passed_run()),
        model_runs: vec![model_run("m.a", ExecutionStatus::Passed)],
        test_runs: vec![test_run("t.a", ExecutionStatus::Passed)],
        require_diff: true,
        diff_passed: Some(true),
        diff_rejected: None,
        breaking_schema_changes: Vec::new(),
        allow_breaking_schema: false,
        expected_target_hash: Some("aaa".to_string()),
        actual_target_hash: Some("aaa".to_string()),
        merge_check: Some(MergeOutcome::clean("bbb")),
    }
}

fn gate<'a>(
    report: &'a phlo_transform_engine::GateReport,
    name: &str,
) -> &'a phlo_transform_engine::GateResult {
    report
        .results
        .iter()
        .find(|result| result.name == name)
        .unwrap_or_else(|| panic!("gate {name} missing"))
}

#[test]
fn all_gates_pass_on_green_input() {
    let report = evaluate_gates(&green_input());
    assert!(report.passed);
    for name in [
        "run",
        "tests",
        "blocked",
        "schema",
        "data_diff",
        "base",
        "conflicts",
    ] {
        assert!(gate(&report, name).passed, "gate {name} should pass");
    }
}

#[test]
fn failed_tests_block_promotion() {
    let mut input = green_input();
    input.test_runs = vec![
        test_run("t.ok", ExecutionStatus::Passed),
        test_run("t.bad", ExecutionStatus::Failed),
    ];
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    let tests = gate(&report, "tests");
    assert!(!tests.passed);
    assert!(tests.detail.contains("t.bad"), "{}", tests.detail);
}

#[test]
fn blocked_work_blocks_promotion() {
    let mut input = green_input();
    input.model_runs = vec![
        model_run("m.a", ExecutionStatus::Passed),
        model_run("m.b", ExecutionStatus::Blocked),
        model_run("m.c", ExecutionStatus::Cancelled),
    ];
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    let blocked = gate(&report, "blocked");
    assert!(!blocked.passed);
    assert!(blocked.detail.contains("m.b"), "{}", blocked.detail);
    assert!(blocked.detail.contains("m.c"), "{}", blocked.detail);
}

#[test]
fn missing_run_blocks_promotion() {
    let mut input = green_input();
    input.run = None;
    input.model_runs = Vec::new();
    input.test_runs = Vec::new();
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    assert!(!gate(&report, "run").passed);
    assert!(!gate(&report, "tests").passed);
    assert!(!gate(&report, "blocked").passed);
}

#[test]
fn failed_diff_blocks_promotion_when_required() {
    let mut input = green_input();
    input.diff_passed = Some(false);
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    assert!(!gate(&report, "data_diff").passed);
}

#[test]
fn stale_diff_artifact_blocks_promotion() {
    let mut input = green_input();
    input.diff_rejected = Some("branch diff is stale".to_string());
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    let diff = gate(&report, "data_diff");
    assert!(!diff.passed);
    assert!(diff.detail.contains("stale"), "{}", diff.detail);
}

#[test]
fn stale_target_blocks_promotion() {
    let mut input = green_input();
    input.actual_target_hash = Some("zzz".to_string());
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    let base = gate(&report, "base");
    assert!(!base.passed);
    assert!(base.detail.contains("advanced"), "{}", base.detail);
}

#[test]
fn merge_conflicts_block_promotion() {
    let mut input = green_input();
    input.merge_check = Some(MergeOutcome::conflict(
        "marts.orders",
        "content changed on both sides",
    ));
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    let conflicts = gate(&report, "conflicts");
    assert!(!conflicts.passed);
    assert!(
        conflicts.detail.contains("marts.orders"),
        "{}",
        conflicts.detail
    );
}

#[test]
fn breaking_schema_blocks_unless_waived() {
    let mut input = green_input();
    input.breaking_schema_changes = vec!["m.a.id: removed".to_string()];
    let report = evaluate_gates(&input);
    assert!(!report.passed);
    assert!(!gate(&report, "schema").passed);

    input.allow_breaking_schema = true;
    let report = evaluate_gates(&input);
    assert!(report.passed);
    assert!(gate(&report, "schema").passed);
}

#[test]
fn data_diff_gate_absent_when_not_required() {
    let mut input = green_input();
    input.require_diff = false;
    input.diff_passed = None;
    let report = evaluate_gates(&input);
    assert!(report.passed);
    assert!(report
        .results
        .iter()
        .all(|result| result.name != "data_diff"));
}

#[test]
fn gate_results_round_trip_through_json() {
    let report = evaluate_gates(&green_input());
    let json = serde_json::to_value(&report).expect("serialize");
    let names: Vec<&str> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|result| result["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "run",
            "tests",
            "blocked",
            "schema",
            "data_diff",
            "base",
            "conflicts"
        ]
    );
    assert_eq!(json["passed"], true);
}
