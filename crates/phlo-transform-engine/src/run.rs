//! Execution: dependency-aware, bounded-concurrency model and test runs.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use phlo_transform_core::{Compilation, Materialization, ModelId};

use crate::adapter::Adapter;
use crate::cancel::CancelHandle;
use crate::error::{AdapterError, EngineError};
use crate::events::{EngineEvent, ExecutionStatus};
use crate::plan::Plan;
use crate::state::{ModelRunRecord, RunRecord, StateStore, TestRunRecord};
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
    pub query_id: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub sql_hash: String,
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
    pub events: Vec<EngineEvent>,
}

struct Completion {
    id: ModelId,
    result: Result<crate::adapter::QueryResult, AdapterError>,
    duration_ms: u64,
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

        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = now_rfc3339();
        let mut events = Vec::new();

        let planned: Vec<ModelId> = plan
            .models
            .iter()
            .filter_map(|model| ModelId::parse(&model.id).ok())
            .collect();
        let planned_set: BTreeSet<ModelId> = planned.iter().cloned().collect();

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
        let mut ready: VecDeque<ModelId> = remaining
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(id, _)| id.clone())
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
                let materialization = model.config.materialization;
                let permit = semaphore.clone();
                let task_id = id.clone();
                join_set.spawn(async move {
                    let _permit = permit.acquire_owned().await;
                    let started = Instant::now();
                    let result = match materialization {
                        Materialization::View => {
                            adapter.create_or_replace_view(&target, &compiled_sql).await
                        }
                        Materialization::Table => {
                            adapter
                                .create_or_replace_table(&target, &compiled_sql)
                                .await
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
                        ModelResult {
                            model: id.logical_name(),
                            target: model.target.display(),
                            materialization: model.config.materialization.to_string(),
                            status: ExecutionStatus::Passed,
                            query_id: query.query_id,
                            error: None,
                            duration_ms: completion.duration_ms,
                            sql_hash: sha256_hex(&model.compiled_sql),
                        },
                    );

                    for dependent in dependents.get(&id).cloned().unwrap_or_default() {
                        if let Some(count) = remaining.get_mut(&dependent) {
                            *count = count.saturating_sub(1);
                            if *count == 0
                                && status.get(&dependent) == Some(&ExecutionStatus::Pending)
                            {
                                ready.push_back(dependent);
                            }
                        }
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
                        ModelResult {
                            model: id.logical_name(),
                            target: model.target.display(),
                            materialization: model.config.materialization.to_string(),
                            status: ExecutionStatus::Failed,
                            query_id: None,
                            error: Some(error.to_string()),
                            duration_ms: completion.duration_ms,
                            sql_hash: sha256_hex(&model.compiled_sql),
                        },
                    );
                    block_dependents(
                        &id,
                        &dependents,
                        &mut status,
                        &mut results,
                        compilation,
                        &mut events,
                    );
                }
            }
        }

        // Abort in-flight work and mark anything unfinished as cancelled.
        if cancelled {
            join_set.abort_all();
            while join_set.join_next().await.is_some() {}
            for id in &planned {
                let current = status.get(id).copied().unwrap_or_default();
                if current.is_terminal() {
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
                        ModelResult {
                            model: id.logical_name(),
                            target: model.target.display(),
                            materialization: model.config.materialization.to_string(),
                            status: ExecutionStatus::Cancelled,
                            query_id: None,
                            error: Some("run cancelled".to_string()),
                            duration_ms: 0,
                            sql_hash: sha256_hex(&model.compiled_sql),
                        },
                    );
                }
            }
        }

        // Any model still pending was blocked indirectly.
        for id in &planned {
            if !status.get(id).copied().unwrap_or_default().is_terminal() {
                status.insert(id.clone(), ExecutionStatus::Blocked);
                if let Some(model) = compilation.model(id) {
                    results.insert(
                        id.clone(),
                        ModelResult {
                            model: id.logical_name(),
                            target: model.target.display(),
                            materialization: model.config.materialization.to_string(),
                            status: ExecutionStatus::Blocked,
                            query_id: None,
                            error: Some("blocked by an upstream failure".to_string()),
                            duration_ms: 0,
                            sql_hash: sha256_hex(&model.compiled_sql),
                        },
                    );
                }
            }
        }

        let tests = if options.run_tests && !cancelled {
            self.run_tests(compilation, plan, &status, &mut events)
                .await
        } else {
            Vec::new()
        };

        let failed = results
            .values()
            .any(|result| result.status == ExecutionStatus::Failed)
            || tests
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
            events,
        })
    }

    async fn run_tests(
        &self,
        compilation: &Compilation,
        plan: &Plan,
        model_status: &BTreeMap<ModelId, ExecutionStatus>,
        events: &mut Vec<EngineEvent>,
    ) -> Vec<TestResult> {
        let planned_tests: BTreeSet<&str> =
            plan.tests.iter().map(|test| test.id.as_str()).collect();
        let mut results = Vec::new();

        for test in &compilation.tests {
            if !planned_tests.contains(test.id.to_string().as_str()) {
                continue;
            }
            // Skip tests whose targets did not all pass.
            let targets_ready = test
                .targets
                .iter()
                .all(|target| model_status.get(target) == Some(&ExecutionStatus::Passed));
            if !targets_ready {
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

fn block_dependents(
    id: &ModelId,
    dependents: &BTreeMap<ModelId, Vec<ModelId>>,
    status: &mut BTreeMap<ModelId, ExecutionStatus>,
    results: &mut BTreeMap<ModelId, ModelResult>,
    compilation: &Compilation,
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
                ModelResult {
                    model: child.logical_name(),
                    target: model.target.display(),
                    materialization: model.config.materialization.to_string(),
                    status: ExecutionStatus::Blocked,
                    query_id: None,
                    error: Some(format!("blocked by {}", id.logical_name())),
                    duration_ms: 0,
                    sql_hash: sha256_hex(&model.compiled_sql),
                },
            );
        }
        block_dependents(child, dependents, status, results, compilation, events);
    }
}
