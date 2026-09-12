//! Execution: dependency-aware, bounded-concurrency model and test runs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use phlo_transform_core::{Compilation, IncrementalStrategy, Materialization, ModelId};

use crate::adapter::Adapter;
use crate::cancel::CancelHandle;
use crate::error::{AdapterError, EngineError};
use crate::events::{EngineEvent, ExecutionStatus};
use crate::plan::{Plan, PlanAction, PlannedModel};
use crate::source_state::{adapter_default_schema, relation_for_source, seed_relation};
use crate::state::{
    MaterializedRecord, ModelRunRecord, RunRecord, SeedRecord, StateStore, TestRunRecord,
};
use crate::util::{now_rfc3339, sha256_hex};

/// Options controlling a run.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub environment: Option<String>,
    pub concurrency: usize,
    /// Run custom tests after the models.
    pub run_tests: bool,
    /// Cooperative cancellation signal.
    pub cancel: CancelHandle,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            environment: None,
            concurrency: 4,
            run_tests: true,
            cancel: CancelHandle::default(),
        }
    }
}

/// The outcome of a single model.
#[derive(Clone, Debug, Serialize)]
pub struct ModelResult {
    pub model: String,
    pub target: String,
    pub materialization: String,
    pub status: ExecutionStatus,
    pub desired_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    pub query_id: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub sql_hash: String,
}

/// The outcome of a single seed load.
#[derive(Clone, Debug, Serialize)]
pub struct SeedResult {
    pub seed: String,
    pub target: String,
    pub status: ExecutionStatus,
    pub error: Option<String>,
}

/// The outcome of a single test.
#[derive(Clone, Debug, Serialize)]
pub struct TestResult {
    pub test: String,
    pub status: ExecutionStatus,
    pub row_count: u64,
    pub query_id: Option<String>,
    pub error: Option<String>,
}

/// The result of applying a plan.
#[derive(Clone, Debug, Serialize)]
pub struct RunResult {
    pub run_id: String,
    pub plan_id: String,
    pub status: ExecutionStatus,
    pub started_at: String,
    pub finished_at: String,
    pub models: Vec<ModelResult>,
    pub tests: Vec<TestResult>,
    /// Seed loads performed before model builds.
    pub seeds: Vec<SeedResult>,
    pub events: Vec<EngineEvent>,
}

struct Completion {
    id: ModelId,
    result: Result<crate::adapter::QueryResult, AdapterError>,
    duration_ms: u64,
}

/// The physical operation chosen for a model.
#[derive(Clone)]
enum ExecOp {
    View,
    Table,
    Append,
    Merge(Vec<String>),
    ReplacePartitions(Vec<String>),
    TimeWindow { predicate: String },
}

fn exec_op(model: &phlo_transform_core::CompiledModel, info: Option<&PlannedModel>) -> ExecOp {
    match model.config.materialization {
        Materialization::View => ExecOp::View,
        Materialization::Table => ExecOp::Table,
        // Ephemeral models are inlined into dependents and never planned.
        Materialization::Ephemeral => unreachable!("ephemeral models are inlined at compile time"),
        Materialization::Incremental => {
            let needs_bootstrap = info
                .map(|model| model.full_rebuild || !model.exists)
                .unwrap_or(true);
            if needs_bootstrap {
                return ExecOp::Table;
            }
            match model.config.incremental.as_ref() {
                Some(IncrementalStrategy::Append) => ExecOp::Append,
                Some(IncrementalStrategy::Key { columns }) => ExecOp::Merge(columns.clone()),
                Some(IncrementalStrategy::Partition { columns }) => {
                    ExecOp::ReplacePartitions(columns.clone())
                }
                Some(IncrementalStrategy::TimeWindow {
                    column,
                    overlap_seconds,
                }) => {
                    match info
                        .and_then(|model| model.watermark.as_deref())
                        .and_then(|watermark| {
                            time_window_predicate(model, column, watermark, *overlap_seconds)
                        }) {
                        Some(predicate) => ExecOp::TimeWindow { predicate },
                        // Without a usable watermark/type, rebuild safely.
                        None => ExecOp::Table,
                    }
                }
                None => ExecOp::Table,
            }
        }
    }
}

/// Build a typed predicate `"col" > CAST('<watermark>' AS <type>)`, optionally
/// backing the watermark off by a configured overlap.
fn time_window_predicate(
    model: &phlo_transform_core::CompiledModel,
    column: &str,
    watermark: &str,
    overlap_seconds: Option<u64>,
) -> Option<String> {
    let data_type = model.schema.column(column)?.data_type.clone();
    let sql_type = sql_type(&data_type)?;
    let escaped = watermark.replace('\'', "''");
    let mut lower_bound = format!("CAST('{escaped}' AS {sql_type})");
    if let Some(overlap) = overlap_seconds {
        if overlap > 0
            && matches!(
                data_type,
                phlo_transform_core::DataType::Timestamp
                    | phlo_transform_core::DataType::TimestampTz
            )
        {
            lower_bound = format!("({lower_bound} - INTERVAL '{overlap}' SECOND)");
        }
    }
    Some(format!("{} > {lower_bound}", quote_ident(column)))
}

fn sql_type(data_type: &phlo_transform_core::DataType) -> Option<&'static str> {
    use phlo_transform_core::DataType;
    Some(match data_type {
        DataType::Boolean => "boolean",
        DataType::TinyInt => "tinyint",
        DataType::SmallInt => "smallint",
        DataType::Integer => "integer",
        DataType::BigInt => "bigint",
        DataType::Real => "real",
        DataType::Double => "double",
        DataType::Decimal => "decimal",
        DataType::Varchar => "varchar",
        DataType::Date => "date",
        DataType::Time => "time",
        DataType::Timestamp => "timestamp",
        DataType::TimestampTz => "timestamp with time zone",
        _ => return None,
    })
}

fn quote_ident(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

/// Executes plans against an adapter.
pub struct Runner {
    adapter: Arc<dyn Adapter>,
    state: Option<Arc<dyn StateStore>>,
}

impl Runner {
    pub fn new(adapter: Arc<dyn Adapter>, state: Option<Arc<dyn StateStore>>) -> Self {
        Self { adapter, state }
    }

    /// Execute a plan.
    pub async fn apply(
        &self,
        compilation: &Compilation,
        plan: &Plan,
        options: &RunOptions,
    ) -> Result<RunResult, EngineError> {
        if plan.blocked {
            return Err(EngineError::Compilation);
        }
        if let Some(reason) = plan.staleness(compilation) {
            return Err(EngineError::StalePlan(reason));
        }

        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = now_rfc3339();
        let mut events = Vec::new();

        let planned: Vec<ModelId> = plan
            .models
            .iter()
            .filter_map(|model| ModelId::parse(&model.id).ok())
            .collect();
        let planned_set: BTreeSet<ModelId> = planned.iter().cloned().collect();
        let plan_info: BTreeMap<ModelId, PlannedModel> = plan
            .models
            .iter()
            .filter_map(|model| ModelId::parse(&model.id).ok().map(|id| (id, model.clone())))
            .collect();

        // Ensure target schemas/namespaces exist before any write.
        let mut schemas: BTreeSet<(Option<String>, String)> = BTreeSet::new();
        for model in &compilation.models {
            if planned_set.contains(&model.id) {
                schemas.insert((model.target.catalog.clone(), model.target.schema.clone()));
            }
        }
        for (catalog, schema) in &schemas {
            let relation = phlo_transform_core::Relation {
                catalog: catalog.clone(),
                schema: schema.clone(),
                table: String::new(),
            };
            self.adapter
                .ensure_schema(&relation)
                .await
                .map_err(EngineError::Adapter)?;
        }

        // Load planned seeds before any model build: seed relations are the
        // physical inputs the models select from.
        let mut seed_results: Vec<SeedResult> = Vec::new();
        for planned_seed in &plan.seeds {
            let Some(seed) = compilation
                .seeds
                .iter()
                .find(|seed| seed.name == planned_seed.name)
            else {
                continue;
            };
            if planned_seed.action != PlanAction::Build {
                events.push(EngineEvent::SeedFinished {
                    seed: planned_seed.name.clone(),
                    status: ExecutionStatus::Skipped,
                });
                seed_results.push(SeedResult {
                    seed: planned_seed.name.clone(),
                    target: planned_seed.target.clone(),
                    status: ExecutionStatus::Skipped,
                    error: None,
                });
                continue;
            }
            let relation = seed_relation(
                seed,
                compilation.defaults.catalog.as_deref(),
                compilation
                    .defaults
                    .schema
                    .as_deref()
                    .or_else(|| adapter_default_schema(self.adapter.name())),
                self.adapter.name(),
            );
            self.adapter
                .ensure_schema(&relation)
                .await
                .map_err(EngineError::Adapter)?;
            let path = match &compilation.workspace_root {
                Some(root) => root.join(&seed.path),
                None => seed.path.clone(),
            };
            events.push(EngineEvent::SeedStarted {
                seed: planned_seed.name.clone(),
            });
            match self.adapter.load_csv(&relation, &path).await {
                Ok(_) => {
                    events.push(EngineEvent::SeedFinished {
                        seed: planned_seed.name.clone(),
                        status: ExecutionStatus::Passed,
                    });
                    seed_results.push(SeedResult {
                        seed: planned_seed.name.clone(),
                        target: relation.display(),
                        status: ExecutionStatus::Passed,
                        error: None,
                    });
                }
                Err(error) => {
                    events.push(EngineEvent::SeedFinished {
                        seed: planned_seed.name.clone(),
                        status: ExecutionStatus::Failed,
                    });
                    seed_results.push(SeedResult {
                        seed: planned_seed.name.clone(),
                        target: relation.display(),
                        status: ExecutionStatus::Failed,
                        error: Some(error.to_string()),
                    });
                }
            }
        }

        // A seed that failed to load must block every model that reads it —
        // otherwise they would run against a stale (or missing) seed table.
        let failed_seed_targets: BTreeSet<String> = seed_results
            .iter()
            .filter(|result| result.status == ExecutionStatus::Failed)
            .map(|result| result.target.clone())
            .collect();

        // Dependency bookkeeping restricted to the planned set.
        let mut remaining: BTreeMap<ModelId, usize> = BTreeMap::new();
        let mut dependents: BTreeMap<ModelId, Vec<ModelId>> = BTreeMap::new();
        for id in &planned {
            let mut count = 0;
            if let Some(model) = compilation.model(id) {
                for dependency in model.model_dependencies() {
                    if planned_set.contains(dependency) {
                        count += 1;
                        dependents
                            .entry(dependency.clone())
                            .or_default()
                            .push(id.clone());
                    }
                }
            }
            remaining.insert(id.clone(), count);
        }
        for list in dependents.values_mut() {
            list.sort();
        }

        let mut status: BTreeMap<ModelId, ExecutionStatus> = planned
            .iter()
            .map(|id| (id.clone(), ExecutionStatus::Pending))
            .collect();
        let mut results: BTreeMap<ModelId, ModelResult> = BTreeMap::new();

        // Skip or reuse models that do not need building, releasing dependents.
        for id in &planned {
            let action = plan_info
                .get(id)
                .map(|model| model.action)
                .unwrap_or(PlanAction::Build);
            if action == PlanAction::Build || action == PlanAction::Unknown {
                continue;
            }
            status.insert(id.clone(), ExecutionStatus::Skipped);
            if let Some(model) = compilation.model(id) {
                events.push(EngineEvent::ModelFinished {
                    model: id.logical_name(),
                    status: ExecutionStatus::Skipped,
                    query_id: None,
                    duration_ms: 0,
                });
                results.insert(
                    id.clone(),
                    model_result(
                        model,
                        ExecutionStatus::Skipped,
                        None,
                        None,
                        0,
                        plan_info.get(id),
                    ),
                );
            }
            release_dependents(id, &dependents, &mut remaining);
        }

        if !failed_seed_targets.is_empty() {
            let default_catalog = compilation.defaults.catalog.as_deref();
            let default_schema = compilation
                .defaults
                .schema
                .as_deref()
                .or_else(|| adapter_default_schema(self.adapter.name()));
            for id in &planned {
                if status.get(id) != Some(&ExecutionStatus::Pending) {
                    continue;
                }
                let Some(model) = compilation.model(id) else {
                    continue;
                };
                let reads_failed_seed = model.source_dependencies().any(|source| {
                    failed_seed_targets.contains(
                        &relation_for_source(source, default_catalog, default_schema).display(),
                    )
                });
                if !reads_failed_seed {
                    continue;
                }
                status.insert(id.clone(), ExecutionStatus::Blocked);
                events.push(EngineEvent::ModelFinished {
                    model: id.logical_name(),
                    status: ExecutionStatus::Blocked,
                    query_id: None,
                    duration_ms: 0,
                });
                results.insert(
                    id.clone(),
                    model_result(
                        model,
                        ExecutionStatus::Blocked,
                        None,
                        Some("blocked by a failed seed load".to_string()),
                        0,
                        plan_info.get(id),
                    ),
                );
                block_dependents(
                    id,
                    &dependents,
                    &mut status,
                    &mut results,
                    compilation,
                    &plan_info,
                    &mut events,
                );
            }
        }

        let mut ready: VecDeque<ModelId> = planned
            .iter()
            .filter(|id| {
                remaining.get(id).copied().unwrap_or(0) == 0
                    && status.get(id) == Some(&ExecutionStatus::Pending)
            })
            .cloned()
            .collect();

        let mut join_set: JoinSet<Completion> = JoinSet::new();
        let semaphore = Arc::new(Semaphore::new(options.concurrency.max(1)));
        let concurrency = options.concurrency.max(1);
        let mut inflight = 0usize;
        let mut cancelled = false;

        while inflight > 0 || !ready.is_empty() {
            if options.cancel.is_cancelled() {
                cancelled = true;
                break;
            }
            while inflight < concurrency {
                let Some(id) = ready.pop_front() else {
                    break;
                };
                if status.get(&id) != Some(&ExecutionStatus::Pending) {
                    continue;
                }
                status.insert(id.clone(), ExecutionStatus::Running);
                events.push(EngineEvent::ModelStarted {
                    model: id.logical_name(),
                });

                let Some(model) = compilation.model(&id) else {
                    continue;
                };
                let adapter = self.adapter.clone();
                let target = model.target.clone();
                let compiled_sql = model.compiled_sql.clone();
                let op = exec_op(model, plan_info.get(&id));
                let permit = semaphore.clone();
                let task_id = id.clone();
                join_set.spawn(async move {
                    let _permit = permit.acquire_owned().await;
                    let started = Instant::now();
                    let result = match op {
                        ExecOp::View => {
                            adapter.create_or_replace_view(&target, &compiled_sql).await
                        }
                        ExecOp::Table => {
                            adapter
                                .create_or_replace_table(&target, &compiled_sql)
                                .await
                        }
                        ExecOp::Append => adapter.append(&target, &compiled_sql).await,
                        ExecOp::Merge(keys) => adapter.merge(&target, &keys, &compiled_sql).await,
                        ExecOp::ReplacePartitions(columns) => {
                            adapter
                                .replace_partitions(&target, &columns, &compiled_sql)
                                .await
                        }
                        ExecOp::TimeWindow { predicate } => {
                            let filtered = format!(
                                "SELECT * FROM ({compiled_sql}) AS __phlo_src WHERE {predicate}"
                            );
                            adapter.append(&target, &filtered).await
                        }
                    };
                    Completion {
                        id: task_id,
                        result,
                        duration_ms: started.elapsed().as_millis() as u64,
                    }
                });
                inflight += 1;
            }

            let joined = tokio::select! {
                joined = join_set.join_next() => joined,
                _ = options.cancel.cancelled() => {
                    cancelled = true;
                    None
                }
            };
            if cancelled {
                break;
            }
            let Some(joined) = joined else {
                break;
            };
            inflight -= 1;
            let completion = joined.map_err(|error| EngineError::State(error.to_string()))?;
            let id = completion.id.clone();
            let model = compilation.model(&id).expect("planned model exists");
            let info = plan_info.get(&id);
            match completion.result {
                Ok(query) => {
                    status.insert(id.clone(), ExecutionStatus::Passed);
                    events.push(EngineEvent::ModelFinished {
                        model: id.logical_name(),
                        status: ExecutionStatus::Passed,
                        query_id: query.query_id.clone(),
                        duration_ms: completion.duration_ms,
                    });
                    results.insert(
                        id.clone(),
                        model_result(
                            model,
                            ExecutionStatus::Passed,
                            query.query_id,
                            None,
                            completion.duration_ms,
                            info,
                        ),
                    );
                    release_dependents(&id, &dependents, &mut remaining);
                    let new_ready = dependents
                        .get(&id)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|child| {
                            remaining.get(child).copied().unwrap_or(0) == 0
                                && status.get(child) == Some(&ExecutionStatus::Pending)
                        });
                    ready.extend(new_ready);

                    if matches!(
                        model.config.incremental,
                        Some(IncrementalStrategy::TimeWindow { .. })
                    ) {
                        advance_watermark(
                            self.adapter.as_ref(),
                            model,
                            options.environment.as_deref(),
                            &run_id,
                            &self.state,
                        )
                        .await?;
                    }
                }
                Err(error) => {
                    status.insert(id.clone(), ExecutionStatus::Failed);
                    events.push(EngineEvent::ModelFinished {
                        model: id.logical_name(),
                        status: ExecutionStatus::Failed,
                        query_id: None,
                        duration_ms: completion.duration_ms,
                    });
                    results.insert(
                        id.clone(),
                        model_result(
                            model,
                            ExecutionStatus::Failed,
                            None,
                            Some(error.to_string()),
                            completion.duration_ms,
                            info,
                        ),
                    );
                    block_dependents(
                        &id,
                        &dependents,
                        &mut status,
                        &mut results,
                        compilation,
                        &plan_info,
                        &mut events,
                    );
                }
            }
        }

        if cancelled {
            join_set.abort_all();
            while join_set.join_next().await.is_some() {}
            for id in &planned {
                if status.get(id).copied().unwrap_or_default().is_terminal() {
                    continue;
                }
                status.insert(id.clone(), ExecutionStatus::Cancelled);
                if let Some(model) = compilation.model(id) {
                    events.push(EngineEvent::ModelFinished {
                        model: id.logical_name(),
                        status: ExecutionStatus::Cancelled,
                        query_id: None,
                        duration_ms: 0,
                    });
                    results.insert(
                        id.clone(),
                        model_result(
                            model,
                            ExecutionStatus::Cancelled,
                            None,
                            Some("run cancelled".to_string()),
                            0,
                            plan_info.get(id),
                        ),
                    );
                }
            }
        }

        for id in &planned {
            if !status.get(id).copied().unwrap_or_default().is_terminal() {
                status.insert(id.clone(), ExecutionStatus::Blocked);
                if let Some(model) = compilation.model(id) {
                    results.insert(
                        id.clone(),
                        model_result(
                            model,
                            ExecutionStatus::Blocked,
                            None,
                            Some("blocked by an upstream failure".to_string()),
                            0,
                            plan_info.get(id),
                        ),
                    );
                }
            }
        }

        let tests = if options.run_tests && !cancelled {
            self.run_tests(
                compilation,
                plan,
                &status,
                &failed_seed_targets,
                &mut events,
            )
            .await
        } else {
            Vec::new()
        };

        let failed = results
            .values()
            .any(|result| result.status == ExecutionStatus::Failed)
            || tests
                .iter()
                .any(|result| result.status == ExecutionStatus::Failed)
            || seed_results
                .iter()
                .any(|result| result.status == ExecutionStatus::Failed);
        let run_status = if cancelled {
            ExecutionStatus::Cancelled
        } else if failed {
            ExecutionStatus::Failed
        } else {
            ExecutionStatus::Passed
        };

        let finished_at = now_rfc3339();

        if let Some(state) = &self.state {
            let failed_count = results
                .values()
                .filter(|result| result.status == ExecutionStatus::Failed)
                .count();
            state.start_run(&RunRecord {
                run_id: run_id.clone(),
                plan_id: plan.id.clone(),
                environment: options.environment.clone(),
                started_at: started_at.clone(),
                finished_at: None,
                status: ExecutionStatus::Running,
                model_count: planned.len(),
                failed_count,
            })?;
            for result in results.values() {
                state.record_model(&ModelRunRecord {
                    run_id: run_id.clone(),
                    model_id: result.model.clone(),
                    materialization: result.materialization.clone(),
                    status: result.status,
                    started_at: started_at.clone(),
                    finished_at: finished_at.clone(),
                    sql_hash: result.sql_hash.clone(),
                    target: result.target.clone(),
                    query_id: result.query_id.clone(),
                    error: result.error.clone(),
                })?;
                // Record the materialised version after a successful build.
                if result.status == ExecutionStatus::Passed {
                    if let Ok(id) = ModelId::parse(&result.model) {
                        if let Some(model) = compilation.model(&id) {
                            state.record_materialized(&MaterializedRecord {
                                model_id: result.model.clone(),
                                environment: options.environment.clone(),
                                version: model.version.clone(),
                                target: result.target.clone(),
                                incremental_strategy: model
                                    .config
                                    .incremental
                                    .as_ref()
                                    .map(|strategy| strategy.as_str().to_string()),
                                incremental_key: model
                                    .config
                                    .incremental
                                    .as_ref()
                                    .map(|strategy| strategy.columns().join(","))
                                    .filter(|key| !key.is_empty()),
                                run_id: run_id.clone(),
                                materialized_at: finished_at.clone(),
                            })?;
                        }
                    }
                }
            }
            for result in &seed_results {
                if result.status != ExecutionStatus::Passed {
                    continue;
                }
                if let Some(seed) = compilation
                    .seeds
                    .iter()
                    .find(|seed| seed.name == result.seed)
                {
                    state.record_seed(&SeedRecord {
                        name: seed.name.clone(),
                        environment: options.environment.clone(),
                        content_hash: seed.content_hash.clone(),
                        target: result.target.clone(),
                        run_id: run_id.clone(),
                        loaded_at: finished_at.clone(),
                    })?;
                }
            }
            for result in &tests {
                state.record_test(&TestRunRecord {
                    run_id: run_id.clone(),
                    test_id: result.test.clone(),
                    status: result.status,
                    row_count: result.row_count,
                    query_id: result.query_id.clone(),
                    error: result.error.clone(),
                    started_at: started_at.clone(),
                    finished_at: finished_at.clone(),
                })?;
            }
            state.finish_run(&run_id, run_status, &finished_at)?;
        }

        events.push(EngineEvent::RunFinished {
            run_id: run_id.clone(),
            status: run_status,
        });

        let models = plan
            .models
            .iter()
            .filter_map(|planned| {
                ModelId::parse(&planned.id)
                    .ok()
                    .and_then(|id| results.get(&id).cloned())
            })
            .collect();

        Ok(RunResult {
            run_id,
            plan_id: plan.id.clone(),
            status: run_status,
            started_at,
            finished_at,
            models,
            tests,
            seeds: seed_results,
            events,
        })
    }

    async fn run_tests(
        &self,
        compilation: &Compilation,
        plan: &Plan,
        model_status: &BTreeMap<ModelId, ExecutionStatus>,
        failed_seed_targets: &BTreeSet<String>,
        events: &mut Vec<EngineEvent>,
    ) -> Vec<TestResult> {
        let planned_tests: BTreeSet<&str> =
            plan.tests.iter().map(|test| test.id.as_str()).collect();
        let default_catalog = compilation.defaults.catalog.as_deref();
        let default_schema = compilation
            .defaults
            .schema
            .as_deref()
            .or_else(|| adapter_default_schema(self.adapter.name()));
        let mut results = Vec::new();

        for test in &compilation.tests {
            if !planned_tests.contains(test.id.to_string().as_str()) {
                continue;
            }
            let targets_ready = test.targets.iter().all(|target| {
                matches!(
                    model_status.get(target),
                    Some(ExecutionStatus::Passed) | Some(ExecutionStatus::Skipped)
                )
            });
            let sources_ready = test.sources.iter().all(|source| {
                !failed_seed_targets.contains(
                    &relation_for_source(source, default_catalog, default_schema).display(),
                )
            });
            if !targets_ready || !sources_ready {
                continue;
            }

            events.push(EngineEvent::TestStarted {
                test: test.id.to_string(),
            });
            match self.adapter.execute(&test.compiled_sql).await {
                Ok(query) => {
                    let status = if query.row_count == 0 {
                        ExecutionStatus::Passed
                    } else {
                        ExecutionStatus::Failed
                    };
                    events.push(EngineEvent::TestFinished {
                        test: test.id.to_string(),
                        status,
                        row_count: query.row_count,
                        query_id: query.query_id.clone(),
                    });
                    results.push(TestResult {
                        test: test.id.to_string(),
                        status,
                        row_count: query.row_count,
                        query_id: query.query_id,
                        error: if status == ExecutionStatus::Failed {
                            Some(format!("test returned {} row(s)", query.row_count))
                        } else {
                            None
                        },
                    });
                }
                Err(error) => {
                    events.push(EngineEvent::TestFinished {
                        test: test.id.to_string(),
                        status: ExecutionStatus::Failed,
                        row_count: 0,
                        query_id: None,
                    });
                    results.push(TestResult {
                        test: test.id.to_string(),
                        status: ExecutionStatus::Failed,
                        row_count: 0,
                        query_id: None,
                        error: Some(error.to_string()),
                    });
                }
            }
        }

        results
    }
}

fn plan_result_fields(info: Option<&PlannedModel>) -> (String, Option<String>, Vec<String>) {
    match info {
        Some(model) => (
            model.desired_version.clone(),
            model.current_version.clone(),
            model
                .reasons
                .iter()
                .map(|reason| reason.label().to_string())
                .collect(),
        ),
        None => (String::new(), None, Vec::new()),
    }
}

fn model_result(
    model: &phlo_transform_core::CompiledModel,
    status: ExecutionStatus,
    query_id: Option<String>,
    error: Option<String>,
    duration_ms: u64,
    info: Option<&PlannedModel>,
) -> ModelResult {
    let (desired_version, previous_version, reasons) = plan_result_fields(info);
    ModelResult {
        model: model.id.logical_name(),
        target: model.target.display(),
        materialization: model.config.materialization.to_string(),
        status,
        desired_version,
        previous_version,
        reasons,
        query_id,
        error,
        duration_ms,
        sql_hash: sha256_hex(&model.compiled_sql),
    }
}

fn release_dependents(
    id: &ModelId,
    dependents: &BTreeMap<ModelId, Vec<ModelId>>,
    remaining: &mut BTreeMap<ModelId, usize>,
) {
    if let Some(children) = dependents.get(id) {
        for child in children {
            if let Some(count) = remaining.get_mut(child) {
                *count = count.saturating_sub(1);
            }
        }
    }
}

fn block_dependents(
    id: &ModelId,
    dependents: &BTreeMap<ModelId, Vec<ModelId>>,
    status: &mut BTreeMap<ModelId, ExecutionStatus>,
    results: &mut BTreeMap<ModelId, ModelResult>,
    compilation: &Compilation,
    plan_info: &BTreeMap<ModelId, PlannedModel>,
    events: &mut Vec<EngineEvent>,
) {
    let Some(children) = dependents.get(id) else {
        return;
    };
    for child in children {
        let current = status.get(child).copied().unwrap_or_default();
        if current.is_terminal() {
            continue;
        }
        status.insert(child.clone(), ExecutionStatus::Blocked);
        if let Some(model) = compilation.model(child) {
            events.push(EngineEvent::ModelFinished {
                model: child.logical_name(),
                status: ExecutionStatus::Blocked,
                query_id: None,
                duration_ms: 0,
            });
            results.insert(
                child.clone(),
                model_result(
                    model,
                    ExecutionStatus::Blocked,
                    None,
                    Some(format!("blocked by {}", id.logical_name())),
                    0,
                    plan_info.get(child),
                ),
            );
        }
        block_dependents(
            child,
            dependents,
            status,
            results,
            compilation,
            plan_info,
            events,
        );
    }
}

/// After a successful time-window append, advance the committed watermark to
/// the maximum observed value. Failed runs never reach this path.
async fn advance_watermark(
    adapter: &dyn Adapter,
    model: &phlo_transform_core::CompiledModel,
    environment: Option<&str>,
    run_id: &str,
    state: &Option<Arc<dyn StateStore>>,
) -> Result<(), EngineError> {
    let Some(IncrementalStrategy::TimeWindow { column, .. }) = model.config.incremental.as_ref()
    else {
        return Ok(());
    };
    let query = format!(
        "SELECT max({}) FROM {}",
        quote_ident(column),
        model.target.sql()
    );
    if let Ok(result) = adapter.execute(&query).await {
        if let Some(value) = result
            .rows
            .first()
            .and_then(|row| row.first())
            .filter(|value| value.as_str() != "NULL")
        {
            if let Some(state) = state {
                state.set_watermark(&model.id.logical_name(), environment, value, run_id)?;
            }
        }
    }
    Ok(())
}
