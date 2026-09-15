//! Execution: dependency-aware, bounded-concurrency model and test runs.
//!
//! The runner consumes a [`Plan`] — the planner stays the source of truth
//! for *what* runs — and decides *how* it executes: bounded parallelism,
//! retries with backoff for transient adapter failures, `--fail-fast`
//! shutdown, per-attempt timeouts, per-target write serialisation, and
//! incremental state persistence so an interrupted run can be resumed or a
//! failed one retried.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::{mpsc, Semaphore};
use tokio::task::JoinSet;

use phlo_transform_core::{
    Compilation, IncrementalStrategy, Materialization, ModelId, Relation, Selection,
};

use crate::adapter::Adapter;
use crate::cancel::CancelHandle;
use crate::error::{AdapterError, EngineError};
use crate::events::{EngineEvent, ExecutionStatus};
use crate::failure::{
    classify_adapter_error, timeout_failure, Attempt, Failure, FailureCategory, RetryPolicy,
};
use crate::plan::{
    Plan, PlanAction, PlanOptions, PlanReason, PlanSelection, PlannedModel, PlannedSeed, Planner,
    ReasonKind,
};
use crate::source_state::{adapter_default_schema, relation_for_source, seed_relation};
use crate::state::{
    MaterializedRecord, ModelRunRecord, RunRecord, SeedRecord, SeedRunRecord, StateStore,
    StoredPlan, StoredPlanModel, StoredPlanSeed, StoredPlanTest, TestRunRecord,
};
use crate::util::{now_rfc3339, sha256_hex};

/// Options controlling a run.
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub environment: Option<String>,
    /// Maximum concurrent model builds (`--jobs`).
    pub concurrency: usize,
    /// Run custom tests after the models.
    pub run_tests: bool,
    /// Stop scheduling new work on the first unrecoverable failure.
    pub fail_fast: bool,
    /// Retry policy for transient adapter failures.
    pub retry: RetryPolicy,
    /// Per-attempt timeout for model builds and test executions.
    pub model_timeout: Option<Duration>,
    /// Cooperative cancellation signal.
    pub cancel: CancelHandle,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            environment: None,
            concurrency: 4,
            run_tests: true,
            fail_fast: false,
            retry: RetryPolicy::default(),
            model_timeout: None,
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
    /// The plan action that governed this model (`build`, `skip`, `cached`).
    pub action: String,
    pub status: ExecutionStatus,
    pub desired_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    pub query_id: Option<String>,
    /// Every attempt made, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    /// Structured failure detail — category, adapter error, attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    pub duration_ms: u64,
    pub sql_hash: String,
    /// The strong physical identity of `target` captured right after the
    /// build — the adapter's `output_identity`. Verified again before the
    /// materialised version is recorded, so a concurrent writer's output is
    /// never claimed as this run's. `None` when the adapter cannot prove
    /// physical identity.
    #[serde(skip)]
    pub output_identity: Option<String>,
}

/// The outcome of a single seed load.
#[derive(Clone, Debug, Serialize)]
pub struct SeedResult {
    pub seed: String,
    pub target: String,
    pub status: ExecutionStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attempts: Vec<Attempt>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    pub duration_ms: u64,
}

/// The outcome of a single test.
#[derive(Clone, Debug, Serialize)]
pub struct TestResult {
    pub test: String,
    pub status: ExecutionStatus,
    pub row_count: u64,
    pub query_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<Failure>,
    pub duration_ms: u64,
}

/// Per-status counts across every node in the run (models, seeds, tests).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RunCounts {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub cached: usize,
    pub blocked: usize,
    pub cancelled: usize,
}

impl RunCounts {
    fn add(&mut self, status: ExecutionStatus) {
        match status {
            ExecutionStatus::Passed => self.passed += 1,
            ExecutionStatus::Failed => self.failed += 1,
            ExecutionStatus::Skipped => self.skipped += 1,
            ExecutionStatus::Cached => self.cached += 1,
            ExecutionStatus::Blocked => self.blocked += 1,
            ExecutionStatus::Cancelled => self.cancelled += 1,
            _ => {}
        }
    }
}

/// The result of applying a plan.
#[derive(Clone, Debug, Serialize)]
pub struct RunResult {
    pub run_id: String,
    pub plan_id: String,
    pub environment: Option<String>,
    /// The run this execution continues: the same id for `resume`, the
    /// original id for `retry_failed` (which creates a new run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continued_from: Option<String>,
    pub status: ExecutionStatus,
    pub started_at: String,
    pub finished_at: String,
    /// Per-status counts over models, seeds and tests.
    pub counts: RunCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub models: Vec<ModelResult>,
    pub tests: Vec<TestResult>,
    /// Seed loads performed before model builds.
    pub seeds: Vec<SeedResult>,
    pub events: Vec<EngineEvent>,
}

/// What a spawned model task reports back.
struct Completion {
    id: ModelId,
    outcome: Result<crate::adapter::QueryResult, Failure>,
    /// Attempts made in *this* invocation (earlier attempts from a resumed
    /// run are kept separately and prepended on write).
    attempts: Vec<Attempt>,
    /// A state-store write failed inside the task — fatal for the run.
    state_error: Option<String>,
    started_at: String,
    finished_at: String,
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

/// Prior per-node records used by `resume` to decide what is safely reusable.
#[derive(Default)]
struct PriorRun {
    models: BTreeMap<String, ModelRunRecord>,
    seeds: BTreeMap<String, SeedRunRecord>,
}

impl PriorRun {
    /// A model's earlier success is reusable only when the version it built
    /// is still the desired version — stale success is never trusted.
    fn reusable_model(&self, id: &ModelId, desired_version: &str) -> Option<&ModelRunRecord> {
        let record = self.models.get(&id.logical_name())?;
        if record.status == ExecutionStatus::Passed && record.desired_version == desired_version {
            Some(record)
        } else {
            None
        }
    }

    /// A seed's earlier load is reusable when it succeeded and the stored
    /// desired version still matches the seed's current content hash.
    fn reusable_seed(&self, name: &str, stored_version: &str, current_hash: &str) -> bool {
        stored_version == current_hash
            && matches!(
                self.seeds.get(name),
                Some(record) if record.status == ExecutionStatus::Passed
            )
    }
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
        self.execute(
            compilation,
            plan,
            options,
            run_id,
            None,
            PriorRun::default(),
            false,
        )
        .await
    }

    /// Continue an interrupted run: models that genuinely passed at their
    /// still-desired version are kept; failed, blocked, cancelled and
    /// never-reached models run again. The run keeps its original id.
    pub async fn resume(
        &self,
        compilation: &Compilation,
        run_id_or_prefix: &str,
        options: &RunOptions,
    ) -> Result<RunResult, EngineError> {
        if !compilation.is_ok() {
            return Err(EngineError::Compilation);
        }
        let state = self
            .state
            .clone()
            .ok_or_else(|| EngineError::State("resume requires a state store".to_string()))?;
        let run_id = resolve_run_id(state.as_ref(), run_id_or_prefix)?;
        let stored = state
            .run(&run_id)?
            .ok_or_else(|| EngineError::InvalidPlan(format!("no run `{run_id_or_prefix}`")))?;
        let manifest = stored.plan.clone().ok_or_else(|| {
            EngineError::InvalidPlan(format!(
                "run {} predates resumable run state; rerun its failed work with `--retry-failed`",
                short_id(&run_id)
            ))
        })?;

        // Resume continues an *interrupted* run — still `running` after a
        // kill, or `cancelled`. A finished failed run is retried as a new
        // run so its history stays immutable.
        match stored.record.status {
            ExecutionStatus::Passed => {
                return Err(EngineError::InvalidPlan(format!(
                    "run {} already passed — nothing to resume",
                    short_id(&run_id)
                )));
            }
            ExecutionStatus::Failed => {
                return Err(EngineError::InvalidPlan(format!(
                    "run {} already finished — retry its failed work with `--retry-failed`",
                    short_id(&run_id)
                )));
            }
            _ => {}
        }
        if options.environment != stored.record.environment {
            return Err(EngineError::InvalidPlan(format!(
                "run {} targeted environment {:?}; refusing to resume into {:?}",
                short_id(&run_id),
                stored.record.environment,
                options.environment
            )));
        }

        // Compatibility: every node the run executed must still exist in the
        // freshly compiled workspace. A changed desired version is *not*
        // incompatible — it simply means the stale success cannot be reused.
        for stored_model in &manifest.models {
            let id = ModelId::parse(&stored_model.id)
                .map_err(|error| EngineError::InvalidPlan(error.to_string()))?;
            if compilation.model(&id).is_none() {
                return Err(EngineError::InvalidPlan(format!(
                    "run {} is incompatible with the workspace: model `{}` no longer exists",
                    short_id(&run_id),
                    stored_model.id
                )));
            }
        }
        for stored_seed in &manifest.seeds {
            if !compilation
                .seeds
                .iter()
                .any(|seed| seed.name == stored_seed.name)
            {
                return Err(EngineError::InvalidPlan(format!(
                    "run {} is incompatible with the workspace: seed `{}` no longer exists",
                    short_id(&run_id),
                    stored_seed.name
                )));
            }
        }

        let prior = PriorRun {
            models: state
                .model_runs(&run_id)?
                .into_iter()
                .map(|record| (record.model_id.clone(), record))
                .collect(),
            seeds: state
                .seed_runs(&run_id)?
                .into_iter()
                .map(|record| (record.name.clone(), record))
                .collect(),
        };

        let plan = self
            .resume_plan(compilation, &stored, &manifest, &prior, options)
            .await?;
        self.execute(
            compilation,
            &plan,
            options,
            run_id.clone(),
            Some(run_id),
            prior,
            true,
        )
        .await
    }

    /// Create a new run over the failed/blocked portion of a finished run,
    /// re-planning through the normal machinery so changed versions and
    /// now-materialised upstreams are handled correctly.
    pub async fn retry_failed(
        &self,
        compilation: &Compilation,
        run_id_or_prefix: &str,
        options: &RunOptions,
    ) -> Result<RunResult, EngineError> {
        if !compilation.is_ok() {
            return Err(EngineError::Compilation);
        }
        let state = self
            .state
            .clone()
            .ok_or_else(|| EngineError::State("retry requires a state store".to_string()))?;
        let run_id = resolve_run_id(state.as_ref(), run_id_or_prefix)?;
        let stored = state
            .run(&run_id)?
            .ok_or_else(|| EngineError::InvalidPlan(format!("no run `{run_id_or_prefix}`")))?;

        if stored.record.finished_at.is_none() {
            return Err(EngineError::InvalidPlan(format!(
                "run {} did not finish — continue it with `--resume`",
                short_id(&run_id)
            )));
        }
        if options.environment != stored.record.environment {
            return Err(EngineError::InvalidPlan(format!(
                "run {} targeted environment {:?}; refusing to retry into {:?}",
                short_id(&run_id),
                stored.record.environment,
                options.environment
            )));
        }

        // The failed portion: models and tests whose run state never
        // reached a satisfied terminal. Everything else stays as
        // materialised.
        let failed_ids: Vec<ModelId> = state
            .model_runs(&run_id)?
            .into_iter()
            .filter(|record| {
                matches!(
                    record.status,
                    ExecutionStatus::Failed | ExecutionStatus::Blocked | ExecutionStatus::Cancelled
                )
            })
            .filter_map(|record| ModelId::parse(&record.model_id).ok())
            .collect();
        // Only tests that executed and failed count as failed work — a
        // blocked/cancelled test never ran; it rides along automatically
        // when the models it reads are rebuilt below.
        let failed_test_ids: BTreeSet<String> = state
            .test_runs(&run_id)?
            .into_iter()
            .filter(|record| record.status == ExecutionStatus::Failed)
            .map(|record| record.test_id)
            .collect();
        if failed_ids.is_empty() && failed_test_ids.is_empty() {
            return Err(EngineError::InvalidPlan(format!(
                "run {} has no failed or blocked work to retry",
                short_id(&run_id)
            )));
        }
        for id in &failed_ids {
            if compilation.model(id).is_none() {
                return Err(EngineError::InvalidPlan(format!(
                    "run {} is incompatible with the workspace: model `{}` no longer exists",
                    short_id(&run_id),
                    id.logical_name()
                )));
            }
        }
        for test_id in &failed_test_ids {
            if !compilation
                .tests
                .iter()
                .any(|test| test.id.to_string() == *test_id)
            {
                return Err(EngineError::InvalidPlan(format!(
                    "run {} is incompatible with the workspace: test `{test_id}` no longer exists",
                    short_id(&run_id)
                )));
            }
        }

        // Select the failed models plus the targets of the failed tests, so
        // those tests land in the plan against datasets that already exist.
        let mut select_ids = failed_ids.clone();
        for test in &compilation.tests {
            if failed_test_ids.contains(&test.id.to_string()) {
                for target in &test.targets {
                    if !select_ids.contains(target) {
                        select_ids.push(target.clone());
                    }
                }
            }
        }
        let selection = Selection::of(compilation, &select_ids);
        let planner = Planner::new(self.adapter.clone(), self.state.clone());
        let mut plan = planner
            .plan(
                compilation,
                &selection,
                options.environment.clone(),
                &PlanOptions::default(),
            )
            .await?;

        // Scope the tests to the failed portion: the failed tests
        // themselves, plus tests over models this retry actually rebuilds
        // (their data changed, so a stale pass no longer holds).
        let rebuilt: BTreeSet<String> = plan
            .models
            .iter()
            .filter(|model| model.action == PlanAction::Build)
            .map(|model| model.id.clone())
            .collect();
        plan.tests.retain(|test| {
            failed_test_ids.contains(&test.id)
                || test.targets.iter().any(|target| rebuilt.contains(target))
        });

        // The failed portion may already be materialised — for example a
        // previous retry fixed it. A retry that would only skip is noise.
        if rebuilt.is_empty() && plan.tests.is_empty() {
            return Err(EngineError::InvalidPlan(format!(
                "the failed portion of run {} is already materialised — nothing to retry",
                short_id(&run_id)
            )));
        }

        let new_run_id = uuid::Uuid::new_v4().to_string();
        self.execute(
            compilation,
            &plan,
            options,
            new_run_id,
            Some(run_id),
            PriorRun::default(),
            false,
        )
        .await
    }

    /// Rebuild the stored plan against the current compilation for a
    /// resume.
    ///
    /// Models and seeds go through the normal [`Planner`] so every
    /// decision — action, incremental strategy/key changes, schema-change
    /// rebuilds, time-window watermarks — is made against *current* state.
    /// The stored plan's values are stale the moment the workspace moved;
    /// only the reuse decision and resume provenance are overlaid on top.
    async fn resume_plan(
        &self,
        compilation: &Compilation,
        stored: &crate::state::StoredRun,
        manifest: &StoredPlan,
        prior: &PriorRun,
        options: &RunOptions,
    ) -> Result<Plan, EngineError> {
        let ids = manifest
            .models
            .iter()
            .map(|model| ModelId::parse(&model.id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| EngineError::InvalidPlan(error.to_string()))?;
        let selection = Selection::of(compilation, &ids);
        let mut plan = Planner::new(self.adapter.clone(), self.state.clone())
            .plan(
                compilation,
                &selection,
                options.environment.clone(),
                &PlanOptions::default(),
            )
            .await?;

        // Overlay resume provenance and the reuse decision: only a
        // verified earlier pass carries over — everything else executes
        // exactly as the fresh plan decided.
        let stored_desired: BTreeMap<&str, &str> = manifest
            .models
            .iter()
            .map(|model| (model.id.as_str(), model.desired_version.as_str()))
            .collect();
        for model in &mut plan.models {
            let id = ModelId::parse(&model.id)
                .map_err(|error| EngineError::InvalidPlan(error.to_string()))?;
            // Reuse requires the earlier success *and* its relation still
            // present — a dropped target means the work has to run again.
            let reused =
                model.exists && prior.reusable_model(&id, &model.desired_version).is_some();
            let provenance = if reused {
                "passed earlier in this run at the same version — reused".to_string()
            } else {
                match prior.models.get(&model.id).map(|record| record.status) {
                    Some(status) if status.is_terminal() => {
                        format!("resumed run; previous status was {}", status.label())
                    }
                    Some(_) => "resumed run; execution was interrupted".to_string(),
                    None => "resumed run; model never started".to_string(),
                }
            };
            model
                .reasons
                .insert(0, PlanReason::simple(ReasonKind::ResumedRun, provenance));
            if reused {
                model.action = PlanAction::Skip;
            } else if stored_desired
                .get(model.id.as_str())
                .is_some_and(|version| *version != model.desired_version)
            {
                model.reasons.push(PlanReason::simple(
                    ReasonKind::ResumedRun,
                    "desired version changed since the original run",
                ));
            }
        }

        // The planner re-decided each seed load against current state;
        // only a verified earlier load is carried over as reused.
        let stored_seed_versions: BTreeMap<&str, &str> = manifest
            .seeds
            .iter()
            .map(|seed| (seed.name.as_str(), seed.desired_version.as_str()))
            .collect();
        let seed_default_catalog = compilation.defaults.catalog.as_deref();
        let seed_default_schema = compilation
            .defaults
            .schema
            .as_deref()
            .or_else(|| adapter_default_schema(self.adapter.name()));
        for seed in &mut plan.seeds {
            let exists = match compilation.seeds.iter().find(|s| s.name == seed.name) {
                Some(compiled) => {
                    let relation = seed_relation(
                        compiled,
                        seed_default_catalog,
                        seed_default_schema,
                        self.adapter.name(),
                    );
                    self.adapter.relation_exists(&relation).await?
                }
                None => false,
            };
            let stored_version = stored_seed_versions
                .get(seed.name.as_str())
                .copied()
                .unwrap_or_default();
            if exists && prior.reusable_seed(&seed.name, stored_version, &seed.desired_version) {
                seed.action = PlanAction::Skip;
                seed.reasons.insert(
                    0,
                    PlanReason::simple(
                        ReasonKind::ResumedRun,
                        "loaded earlier in this run — reused",
                    ),
                );
            }
        }

        plan.id = stored.record.plan_id.clone();
        plan.environment = stored.record.environment.clone();
        plan.selection = PlanSelection {
            terms: vec![format!("--resume {}", short_id(&stored.record.run_id))],
            ..Default::default()
        };
        plan.git = None;
        Ok(plan)
    }

    /// Shared execution driver for fresh runs, resumes and retries.
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        compilation: &Compilation,
        plan: &Plan,
        options: &RunOptions,
        run_id: String,
        continued_from: Option<String>,
        prior: PriorRun,
        resume: bool,
    ) -> Result<RunResult, EngineError> {
        let started_at = now_rfc3339();
        let mut events = vec![EngineEvent::RunStarted {
            run_id: run_id.clone(),
            plan_id: plan.id.clone(),
            continued_from: continued_from.clone(),
        }];

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

        // Persist the run before any work: a killed process must leave an
        // honest "running" record, never a silent gap.
        let stored_plan = StoredPlan::from_plan(plan, &options.environment);
        if let Some(state) = &self.state {
            if resume {
                state.reopen_run(&run_id)?;
            } else {
                state.start_run(
                    &RunRecord {
                        run_id: run_id.clone(),
                        plan_id: plan.id.clone(),
                        environment: options.environment.clone(),
                        reference_hash: None,
                        started_at: started_at.clone(),
                        finished_at: None,
                        status: ExecutionStatus::Running,
                        model_count: planned.len(),
                        failed_count: 0,
                    },
                    &stored_plan,
                )?;
            }
        }

        // Two planned models writing the same physical relation must not
        // race: serialise them on a per-target lock and surface a warning.
        let mut target_counts: BTreeMap<String, usize> = BTreeMap::new();
        for model in &compilation.models {
            if planned_set.contains(&model.id) {
                *target_counts.entry(model.target.display()).or_default() += 1;
            }
        }
        let mut warnings = Vec::new();
        let mut target_locks: BTreeMap<String, Arc<Semaphore>> = BTreeMap::new();
        for (target, count) in &target_counts {
            if *count > 1 {
                warnings.push(format!(
                    "{target} is written by {count} planned models; their writes are serialised"
                ));
                target_locks.insert(target.clone(), Arc::new(Semaphore::new(1)));
            }
        }

        // Ensure target schemas/namespaces exist before any write.
        let mut schemas: BTreeSet<(Option<String>, String)> = BTreeSet::new();
        for model in &compilation.models {
            if planned_set.contains(&model.id) {
                schemas.insert((model.target.catalog.clone(), model.target.schema.clone()));
            }
        }
        for (catalog, schema) in &schemas {
            let relation = Relation {
                catalog: catalog.clone(),
                schema: schema.clone(),
                table: String::new(),
            };
            self.adapter
                .ensure_schema(&relation)
                .await
                .map_err(EngineError::Adapter)?;
        }

        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<EngineEvent>();
        let mut cancelled = false;
        let mut fail_fast = false;

        // Load planned seeds before any model build: seed relations are the
        // physical inputs the models select from. A failed seed blocks every
        // model that reads it; under --fail-fast it cancels the rest.
        let mut seed_results: Vec<SeedResult> = Vec::new();
        for planned_seed in &plan.seeds {
            let Some(seed) = compilation
                .seeds
                .iter()
                .find(|seed| seed.name == planned_seed.name)
            else {
                continue;
            };
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
            if cancelled || fail_fast {
                seed_results.push(seed_result(
                    planned_seed,
                    ExecutionStatus::Cancelled,
                    Vec::new(),
                    Some(Failure::without_attempt(
                        FailureCategory::Cancelled,
                        "not started",
                    )),
                    0,
                ));
                let now = now_rfc3339();
                self.persist_seed(
                    &run_id,
                    seed_results.last().expect("just pushed"),
                    &now,
                    &now,
                )?;
                continue;
            }
            // A non-building seed is either skipped or — on a resumed run
            // where it already loaded successfully — cached reuse.
            if planned_seed.action != PlanAction::Build {
                let reused = matches!(
                    prior.seeds.get(&seed.name),
                    Some(record) if record.status == ExecutionStatus::Passed
                );
                let outcome = if reused {
                    ExecutionStatus::Cached
                } else {
                    ExecutionStatus::Skipped
                };
                events.push(EngineEvent::SeedFinished {
                    seed: planned_seed.name.clone(),
                    status: outcome,
                });
                let result = seed_result(planned_seed, outcome, Vec::new(), None, 0);
                // Reused seeds keep their original record; skipped ones get
                // a fresh skip row.
                if !reused {
                    let now = now_rfc3339();
                    self.persist_seed(&run_id, &result, &now, &now)?;
                }
                seed_results.push(result);
                continue;
            }

            self.adapter
                .ensure_schema(&relation)
                .await
                .map_err(EngineError::Adapter)?;
            let path = match &compilation.workspace_root {
                Some(root) => root.join(&seed.path),
                None => seed.path.clone(),
            };
            let result = self
                .run_seed(
                    planned_seed,
                    &relation,
                    &path,
                    options,
                    &run_id,
                    &mut events,
                )
                .await?;
            if result.status == ExecutionStatus::Failed && options.fail_fast {
                fail_fast = true;
            }
            if options.cancel.is_cancelled() {
                cancelled = true;
            }
            seed_results.push(result);
        }
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
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

        // Resume reuse: models that passed at their still-desired version are
        // carried over without re-executing. Their earlier run records stay
        // untouched — they already hold the attempts that produced them.
        // The target must also still exist — the freshly planned `exists`
        // carries that check so a dropped relation is rebuilt, not reused.
        for id in &planned {
            let Some(model) = compilation.model(id) else {
                continue;
            };
            let exists = plan_info.get(id).is_some_and(|planned| planned.exists);
            if exists && prior.reusable_model(id, &model.version.hash).is_some() {
                status.insert(id.clone(), ExecutionStatus::Cached);
                events.push(EngineEvent::ModelFinished {
                    model: id.logical_name(),
                    status: ExecutionStatus::Cached,
                    query_id: None,
                    duration_ms: 0,
                });
                let mut result = model_result(
                    model,
                    ExecutionStatus::Cached,
                    Vec::new(),
                    None,
                    None,
                    0,
                    plan_info.get(id),
                );
                result
                    .reasons
                    .push("reused from earlier in this run".to_string());
                results.insert(id.clone(), result);
                release_dependents(id, &dependents, &mut remaining);
            }
        }

        // Skip or reuse models that do not need building, releasing dependents.
        for id in &planned {
            if status.get(id) != Some(&ExecutionStatus::Pending) {
                continue;
            }
            let action = plan_info
                .get(id)
                .map(|model| model.action)
                .unwrap_or(PlanAction::Build);
            if action == PlanAction::Build || action == PlanAction::Unknown {
                continue;
            }
            let outcome = match action {
                PlanAction::Cached => ExecutionStatus::Cached,
                _ => ExecutionStatus::Skipped,
            };
            status.insert(id.clone(), outcome);
            if let Some(model) = compilation.model(id) {
                events.push(EngineEvent::ModelFinished {
                    model: id.logical_name(),
                    status: outcome,
                    query_id: None,
                    duration_ms: 0,
                });
                let result =
                    model_result(model, outcome, Vec::new(), None, None, 0, plan_info.get(id));
                self.persist_model(&run_id, &result)?;
                results.insert(id.clone(), result);
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
                let result = model_result(
                    model,
                    ExecutionStatus::Blocked,
                    Vec::new(),
                    None,
                    None,
                    0,
                    plan_info.get(id),
                )
                .with_failure(Failure::without_attempt(
                    FailureCategory::Dependency,
                    "blocked by a failed seed load",
                ));
                self.persist_model(&run_id, &result)?;
                results.insert(id.clone(), result);
                let blocked = block_dependents(
                    id,
                    &dependents,
                    &mut status,
                    &mut results,
                    compilation,
                    &plan_info,
                    &mut events,
                );
                for child in blocked {
                    if let Some(result) = results.get(&child) {
                        self.persist_model(&run_id, result)?;
                    }
                }
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
        // Per-model adapter views tracking in-flight query ids — used to
        // cancel warehouse queries on timeout and shutdown.
        let mut attempt_adapters: BTreeMap<ModelId, Arc<dyn Adapter>> = BTreeMap::new();

        while inflight > 0 || !ready.is_empty() {
            while let Ok(event) = event_rx.try_recv() {
                events.push(event);
            }
            if cancelled || fail_fast {
                break;
            }
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
                let started = now_rfc3339();
                self.persist_model_start(&run_id, model, &started, plan_info.get(&id))?;

                // A tracked adapter view exposes this model's in-flight
                // query ids so timeout/fail-fast can cancel the warehouse
                // query itself; untracked adapters degrade to dropping the
                // attempt's future.
                let adapter = self.adapter.clone();
                let attempt_adapter = adapter.track_attempt().unwrap_or_else(|| adapter.clone());
                attempt_adapters.insert(id.clone(), attempt_adapter.clone());
                let target = model.target.clone();
                let compiled_sql = model.compiled_sql.clone();
                let materialization = model.config.materialization.to_string();
                let sql_hash = sha256_hex(&model.compiled_sql);
                let op = exec_op(model, plan_info.get(&id));
                let permit = semaphore.clone();
                let target_lock = target_locks.get(&target.display()).cloned();
                let retry = options.retry.clone();
                let timeout = options.model_timeout;
                let cancel = options.cancel.clone();
                let name = id.logical_name();
                let tx = event_tx.clone();
                // Resume: attempt numbering and the attempt list continue
                // across invocations of the same run.
                let prior_attempts: Vec<Attempt> = prior
                    .models
                    .get(&name)
                    .map(|record| record.attempts.clone())
                    .unwrap_or_default();
                let attempt_count_offset = prior_attempts.len() as u32;
                let info = plan_info.get(&id);
                let action = info
                    .map(|model| model.action.as_str().to_string())
                    .unwrap_or_else(|| "build".to_string());
                let desired_version = info
                    .map(|model| model.desired_version.clone())
                    .unwrap_or_default();
                let task_state = self.state.clone();
                let task_run_id = run_id.clone();
                let task_id = id.clone();
                join_set.spawn(async move {
                    let mut attempts: Vec<Attempt> = Vec::new();
                    let mut attempt_no = attempt_count_offset;
                    let first_started_at = started;
                    let task_started = Instant::now();
                    let mut state_error: Option<String> = None;
                    let outcome = loop {
                        attempt_no += 1;
                        let attempt_started_at = now_rfc3339();
                        let attempt_started = Instant::now();
                        // The slot and the target lock are held only for the
                        // attempt itself — a retrying model does not occupy
                        // either while backing off.
                        let _permit = permit.clone().acquire_owned().await;
                        let _target_guard = match &target_lock {
                            Some(lock) => Some(lock.clone().acquire_owned().await),
                            None => None,
                        };
                        let op = execute_op(&attempt_adapter, &target, &compiled_sql, &op);
                        let result = match timeout {
                            Some(limit) => match tokio::time::timeout(limit, op).await {
                                Ok(Ok(query)) => Ok(query),
                                Ok(Err(error)) => Err(classify_adapter_error(&error, attempt_no)),
                                Err(_) => {
                                    // Kill the in-flight warehouse query
                                    // where the adapter can name it.
                                    for query_id in attempt_adapter.in_flight_queries() {
                                        let _ = attempt_adapter.cancel(&query_id).await;
                                    }
                                    Err(timeout_failure(limit, attempt_no))
                                }
                            },
                            None => op
                                .await
                                .map_err(|error| classify_adapter_error(&error, attempt_no)),
                        };
                        let duration_ms = attempt_started.elapsed().as_millis() as u64;
                        drop(_target_guard);
                        drop(_permit);
                        match result {
                            Ok(query) => {
                                attempts.push(Attempt {
                                    attempt: attempt_no,
                                    started_at: attempt_started_at,
                                    duration_ms,
                                    query_id: query.query_id.clone(),
                                    failure: None,
                                });
                                break Ok(query);
                            }
                            Err(failure) => {
                                let retryable = retry.should_retry(&failure, attempt_no);
                                attempts.push(Attempt {
                                    attempt: attempt_no,
                                    started_at: attempt_started_at,
                                    duration_ms,
                                    query_id: None,
                                    failure: Some(failure.clone()),
                                });
                                // Persist every attempt as it lands — a
                                // crash during backoff must not lose the
                                // attempts already made. A failed write is
                                // fatal: resumability depends on it.
                                if let Some(state) = &task_state {
                                    let mut all = prior_attempts.clone();
                                    all.extend(attempts.iter().cloned());
                                    if let Err(error) = state.record_model(&ModelRunRecord {
                                        run_id: task_run_id.clone(),
                                        model_id: name.clone(),
                                        materialization: materialization.clone(),
                                        action: action.clone(),
                                        status: ExecutionStatus::Running,
                                        started_at: first_started_at.clone(),
                                        finished_at: String::new(),
                                        sql_hash: sql_hash.clone(),
                                        target: target.display(),
                                        desired_version: desired_version.clone(),
                                        attempts: all,
                                        query_id: None,
                                        error: Some(failure.message.clone()),
                                        error_category: Some(failure.category.code().to_string()),
                                    }) {
                                        state_error = Some(error.to_string());
                                        break Err(failure);
                                    }
                                }
                                if retryable && !cancel.is_cancelled() {
                                    let delay = retry.delay(attempt_no);
                                    let _ = tx.send(EngineEvent::ModelRetrying {
                                        model: name.clone(),
                                        attempt: attempt_no,
                                        delay_ms: delay.as_millis() as u64,
                                        reason: failure.message.clone(),
                                    });
                                    tokio::time::sleep(delay).await;
                                    continue;
                                }
                                break Err(failure);
                            }
                        }
                    };
                    Completion {
                        id: task_id,
                        outcome,
                        attempts,
                        state_error,
                        started_at: first_started_at,
                        finished_at: now_rfc3339(),
                        duration_ms: task_started.elapsed().as_millis() as u64,
                    }
                });
                inflight += 1;
            }
            if ready.is_empty() && inflight == 0 {
                break;
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
            let completion = match joined {
                Ok(completion) => completion,
                Err(error) => {
                    return Err(EngineError::State(format!("model task failed: {error}")));
                }
            };
            let id = completion.id.clone();
            attempt_adapters.remove(&id);
            // A state-store write failed inside the task — the run cannot
            // claim honest progress, so it dies.
            if let Some(error) = completion.state_error {
                return Err(EngineError::State(format!(
                    "could not persist {} progress: {error}",
                    id.logical_name()
                )));
            }
            let model = compilation.model(&id).expect("planned model exists");
            let info = plan_info.get(&id);
            // Resume continuity: the record keeps attempts from earlier
            // invocations of this run.
            let mut all_attempts = prior
                .models
                .get(&id.logical_name())
                .map(|record| record.attempts.clone())
                .unwrap_or_default();
            all_attempts.extend(completion.attempts.iter().cloned());
            match completion.outcome {
                Ok(query) => {
                    status.insert(id.clone(), ExecutionStatus::Passed);
                    events.push(EngineEvent::ModelFinished {
                        model: id.logical_name(),
                        status: ExecutionStatus::Passed,
                        query_id: query.query_id.clone(),
                        duration_ms: completion.duration_ms,
                    });
                    let result = ModelResult {
                        query_id: query.query_id,
                        output_identity: self
                            .adapter
                            .output_identity(&model.target)
                            .await
                            .ok()
                            .flatten(),
                        ..model_result(
                            model,
                            ExecutionStatus::Passed,
                            all_attempts,
                            Some(completion.started_at.clone()),
                            Some(completion.finished_at.clone()),
                            completion.duration_ms,
                            info,
                        )
                    };
                    self.persist_model(&run_id, &result)?;
                    results.insert(id.clone(), result);
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
                Err(failure) => {
                    status.insert(id.clone(), ExecutionStatus::Failed);
                    events.push(EngineEvent::ModelFinished {
                        model: id.logical_name(),
                        status: ExecutionStatus::Failed,
                        query_id: None,
                        duration_ms: completion.duration_ms,
                    });
                    let result = ModelResult {
                        ..model_result(
                            model,
                            ExecutionStatus::Failed,
                            all_attempts,
                            Some(completion.started_at.clone()),
                            Some(completion.finished_at.clone()),
                            completion.duration_ms,
                            info,
                        )
                        .with_failure(failure)
                    };
                    self.persist_model(&run_id, &result)?;
                    results.insert(id.clone(), result);
                    if options.fail_fast {
                        fail_fast = true;
                        // Dependents of the failed model can never run —
                        // they are blocked, not merely unscheduled.
                        let blocked = block_dependents(
                            &id,
                            &dependents,
                            &mut status,
                            &mut results,
                            compilation,
                            &plan_info,
                            &mut events,
                        );
                        for child in blocked {
                            if let Some(result) = results.get(&child) {
                                self.persist_model(&run_id, result)?;
                            }
                        }
                        // Stop scheduling new work and try to stop active
                        // work: aborting the Tokio task drops the in-flight
                        // future (for HTTP adapters this closes the request);
                        // whether the warehouse kills the statement depends
                        // on the adapter — we report the task as cancelled,
                        // not the query.
                        join_set.abort_all();
                        ready.clear();
                    } else {
                        let blocked = block_dependents(
                            &id,
                            &dependents,
                            &mut status,
                            &mut results,
                            compilation,
                            &plan_info,
                            &mut events,
                        );
                        for child in blocked {
                            if let Some(result) = results.get(&child) {
                                self.persist_model(&run_id, result)?;
                            }
                        }
                    }
                }
            }
        }

        // Shut down: drain or abort in-flight tasks, then classify anything
        // that never reached a terminal state.
        if cancelled || fail_fast {
            join_set.abort_all();
            // Ask each adapter view to kill its in-flight warehouse
            // queries. Untracked adapters report none and degrade to the
            // abort alone.
            for attempt_adapter in attempt_adapters.values() {
                for query_id in attempt_adapter.in_flight_queries() {
                    if let Err(error) = attempt_adapter.cancel(&query_id).await {
                        warnings.push(format!(
                            "could not cancel in-flight query {query_id}: {error}"
                        ));
                    }
                }
            }
        }
        while let Some(joined) = join_set.join_next().await {
            inflight = inflight.saturating_sub(1);
            if let Ok(completion) = joined {
                // A task that completed before the abort is reported
                // accurately — it really did run.
                let id = completion.id.clone();
                attempt_adapters.remove(&id);
                if let Some(error) = completion.state_error {
                    return Err(EngineError::State(format!(
                        "could not persist {} progress: {error}",
                        id.logical_name()
                    )));
                }
                if status.get(&id).copied().unwrap_or_default().is_terminal() {
                    continue;
                }
                let model = compilation.model(&id).expect("planned model exists");
                let info = plan_info.get(&id);
                let mut all_attempts = prior
                    .models
                    .get(&id.logical_name())
                    .map(|record| record.attempts.clone())
                    .unwrap_or_default();
                all_attempts.extend(completion.attempts.iter().cloned());
                let (result, new_ready) = match completion.outcome {
                    Ok(query) => (
                        ModelResult {
                            query_id: query.query_id.clone(),
                            output_identity: self
                                .adapter
                                .output_identity(&model.target)
                                .await
                                .ok()
                                .flatten(),
                            ..model_result(
                                model,
                                ExecutionStatus::Passed,
                                all_attempts,
                                Some(completion.started_at.clone()),
                                Some(completion.finished_at.clone()),
                                completion.duration_ms,
                                info,
                            )
                        },
                        true,
                    ),
                    Err(failure) => (
                        model_result(
                            model,
                            ExecutionStatus::Failed,
                            all_attempts,
                            Some(completion.started_at.clone()),
                            Some(completion.finished_at.clone()),
                            completion.duration_ms,
                            info,
                        )
                        .with_failure(failure),
                        false,
                    ),
                };
                let final_status = result.status;
                events.push(EngineEvent::ModelFinished {
                    model: id.logical_name(),
                    status: final_status,
                    query_id: result.query_id.clone(),
                    duration_ms: result.duration_ms,
                });
                status.insert(id.clone(), final_status);
                self.persist_model(&run_id, &result)?;
                results.insert(id.clone(), result);
                if new_ready {
                    release_dependents(&id, &dependents, &mut remaining);
                } else {
                    let blocked = block_dependents(
                        &id,
                        &dependents,
                        &mut status,
                        &mut results,
                        compilation,
                        &plan_info,
                        &mut events,
                    );
                    for child in blocked {
                        if let Some(result) = results.get(&child) {
                            self.persist_model(&run_id, result)?;
                        }
                    }
                }
            }
        }
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }

        // Everything left non-terminal was never started or was cut off:
        // cancelled on shutdown/fail-fast, blocked otherwise.
        for id in &planned {
            if status.get(id).copied().unwrap_or_default().is_terminal() {
                continue;
            }
            let (final_status, failure) = if cancelled || fail_fast {
                (
                    ExecutionStatus::Cancelled,
                    Failure::without_attempt(
                        FailureCategory::Cancelled,
                        if cancelled {
                            "run cancelled"
                        } else {
                            "cancelled by fail-fast shutdown"
                        },
                    ),
                )
            } else {
                (
                    ExecutionStatus::Blocked,
                    Failure::without_attempt(
                        FailureCategory::Dependency,
                        "blocked by an upstream failure",
                    ),
                )
            };
            status.insert(id.clone(), final_status);
            if let Some(model) = compilation.model(id) {
                events.push(EngineEvent::ModelFinished {
                    model: id.logical_name(),
                    status: final_status,
                    query_id: None,
                    duration_ms: 0,
                });
                let result = model_result(
                    model,
                    final_status,
                    Vec::new(),
                    None,
                    None,
                    0,
                    plan_info.get(id),
                )
                .with_failure(failure);
                self.persist_model(&run_id, &result)?;
                results.insert(id.clone(), result);
            }
        }

        let tests = self
            .run_tests(
                compilation,
                plan,
                &status,
                &failed_seed_targets,
                &mut events,
                options,
                &run_id,
                cancelled || fail_fast,
            )
            .await?;

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

        let mut counts = RunCounts::default();
        for result in results.values() {
            counts.add(result.status);
        }
        for result in &seed_results {
            counts.add(result.status);
        }
        for result in &tests {
            counts.add(result.status);
        }

        if let Some(state) = &self.state {
            for result in results.values() {
                // Record the materialised version after a successful build.
                if result.status == ExecutionStatus::Passed {
                    if let Ok(id) = ModelId::parse(&result.model) {
                        if let Some(model) = compilation.model(&id) {
                            // Output-identity verification: the relation
                            // must still hold what this run wrote. If another
                            // writer overwrote it between build and record,
                            // recording our version would poison the shared
                            // cache — the newer writer's record stands.
                            if let Some(captured) = &result.output_identity {
                                let live = self
                                    .adapter
                                    .output_identity(&model.target)
                                    .await
                                    .ok()
                                    .flatten();
                                if live.as_deref() != Some(captured.as_str()) {
                                    warnings.push(format!(
                                        "materialisation for {} not recorded: {} changed after \
                                         the build (a concurrent writer owns it now)",
                                        result.model, result.target
                                    ));
                                    continue;
                                }
                            }
                            state.record_materialized(&MaterializedRecord {
                                model_id: result.model.clone(),
                                environment: options.environment.clone(),
                                version: model.version.clone(),
                                detail: Some(model.version_detail.clone()),
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
                                adapter: Some(self.adapter.name().to_string()),
                                output_identity: result.output_identity.clone(),
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
            state.finish_run(&run_id, run_status, &finished_at, counts.failed)?;
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
            environment: options.environment.clone(),
            continued_from,
            status: run_status,
            started_at,
            finished_at,
            counts,
            warnings,
            models,
            tests,
            seeds: seed_results,
            events,
        })
    }

    /// Persist a model's start transition so a crash leaves an honest
    /// "running" record. A failed write is fatal: resumability depends on
    /// this record being real.
    fn persist_model_start(
        &self,
        run_id: &str,
        model: &phlo_transform_core::CompiledModel,
        started_at: &str,
        info: Option<&PlannedModel>,
    ) -> Result<(), EngineError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        let (desired_version, _, _) = plan_result_fields(info);
        state.record_model(&ModelRunRecord {
            run_id: run_id.to_string(),
            model_id: model.id.logical_name(),
            materialization: model.config.materialization.to_string(),
            action: info
                .map(|model| model.action.as_str().to_string())
                .unwrap_or_else(|| "build".to_string()),
            status: ExecutionStatus::Running,
            started_at: started_at.to_string(),
            finished_at: String::new(),
            sql_hash: sha256_hex(&model.compiled_sql),
            target: model.target.display(),
            desired_version,
            attempts: Vec::new(),
            query_id: None,
            error: None,
            error_category: None,
        })
    }

    /// Persist a model's terminal state. A failed write is fatal.
    fn persist_model(&self, run_id: &str, result: &ModelResult) -> Result<(), EngineError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        state.record_model(&ModelRunRecord {
            run_id: run_id.to_string(),
            model_id: result.model.clone(),
            materialization: result.materialization.clone(),
            action: result.action.clone(),
            status: result.status,
            started_at: result.started_at.clone().unwrap_or_default(),
            finished_at: result.finished_at.clone().unwrap_or_default(),
            sql_hash: result.sql_hash.clone(),
            target: result.target.clone(),
            desired_version: result.desired_version.clone(),
            attempts: result.attempts.clone(),
            query_id: result.query_id.clone(),
            error: result
                .failure
                .as_ref()
                .map(|failure| failure.message.clone()),
            error_category: result
                .failure
                .as_ref()
                .map(|failure| failure.category.code().to_string()),
        })
    }

    /// Persist a seed's state — called after every attempt, so a crash
    /// mid-retry leaves the attempts already made. Fatal on write error.
    fn persist_seed(
        &self,
        run_id: &str,
        result: &SeedResult,
        started_at: &str,
        finished_at: &str,
    ) -> Result<(), EngineError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        state.record_seed_run(&SeedRunRecord {
            run_id: run_id.to_string(),
            name: result.seed.clone(),
            status: result.status,
            target: result.target.clone(),
            attempts: result.attempts.clone(),
            error: result
                .failure
                .as_ref()
                .map(|failure| failure.message.clone()),
            error_category: result
                .failure
                .as_ref()
                .map(|failure| failure.category.code().to_string()),
            started_at: started_at.to_string(),
            finished_at: finished_at.to_string(),
        })
    }

    /// Load one seed with the shared retry/classification policy. Each
    /// attempt is persisted as it lands, so a crash mid-backoff leaves the
    /// attempts already made.
    async fn run_seed(
        &self,
        planned_seed: &PlannedSeed,
        relation: &Relation,
        path: &std::path::Path,
        options: &RunOptions,
        run_id: &str,
        events: &mut Vec<EngineEvent>,
    ) -> Result<SeedResult, EngineError> {
        let mut attempts = Vec::new();
        let mut attempt_no = 0u32;
        let task_started = Instant::now();
        let seed_started_at = now_rfc3339();
        let outcome = loop {
            attempt_no += 1;
            let started_at = now_rfc3339();
            let started = Instant::now();
            events.push(EngineEvent::SeedStarted {
                seed: planned_seed.name.clone(),
            });
            let load = self.adapter.load_csv(relation, path);
            let result = match options.model_timeout {
                Some(limit) => match tokio::time::timeout(limit, load).await {
                    Ok(Ok(query)) => Ok(query),
                    Ok(Err(error)) => Err(classify_adapter_error(&error, attempt_no)),
                    Err(_) => Err(timeout_failure(limit, attempt_no)),
                },
                None => load
                    .await
                    .map_err(|error| classify_adapter_error(&error, attempt_no)),
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            match result {
                Ok(query) => {
                    attempts.push(Attempt {
                        attempt: attempt_no,
                        started_at,
                        duration_ms,
                        query_id: query.query_id,
                        failure: None,
                    });
                    break None;
                }
                Err(failure) => {
                    let retryable = options.retry.should_retry(&failure, attempt_no);
                    attempts.push(Attempt {
                        attempt: attempt_no,
                        started_at,
                        duration_ms,
                        query_id: None,
                        failure: Some(failure.clone()),
                    });
                    // Persist each attempt as it lands — a crash during
                    // backoff must not lose the attempts already made.
                    let in_progress = SeedResult {
                        seed: planned_seed.name.clone(),
                        target: relation.display(),
                        status: ExecutionStatus::Running,
                        attempts: attempts.clone(),
                        failure: Some(failure.clone()),
                        duration_ms: task_started.elapsed().as_millis() as u64,
                    };
                    self.persist_seed(run_id, &in_progress, &seed_started_at, &now_rfc3339())?;
                    if retryable && !options.cancel.is_cancelled() {
                        let delay = options.retry.delay(attempt_no);
                        events.push(EngineEvent::SeedRetrying {
                            seed: planned_seed.name.clone(),
                            attempt: attempt_no,
                            delay_ms: delay.as_millis() as u64,
                            reason: failure.message.clone(),
                        });
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    break Some(failure);
                }
            }
        };
        let status = if outcome.is_none() {
            ExecutionStatus::Passed
        } else {
            ExecutionStatus::Failed
        };
        events.push(EngineEvent::SeedFinished {
            seed: planned_seed.name.clone(),
            status,
        });
        let result = SeedResult {
            seed: planned_seed.name.clone(),
            target: relation.display(),
            status,
            attempts,
            failure: outcome,
            duration_ms: task_started.elapsed().as_millis() as u64,
        };
        self.persist_seed(run_id, &result, &seed_started_at, &now_rfc3339())?;
        Ok(result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_tests(
        &self,
        compilation: &Compilation,
        plan: &Plan,
        model_status: &BTreeMap<ModelId, ExecutionStatus>,
        failed_seed_targets: &BTreeSet<String>,
        events: &mut Vec<EngineEvent>,
        options: &RunOptions,
        run_id: &str,
        stopped: bool,
    ) -> Result<Vec<TestResult>, EngineError> {
        if !options.run_tests {
            return Ok(Vec::new());
        }
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
            if stopped || options.cancel.is_cancelled() {
                // Never started under cancellation/fail-fast.
                let result = TestResult {
                    test: test.id.to_string(),
                    status: ExecutionStatus::Cancelled,
                    row_count: 0,
                    query_id: None,
                    failure: Some(Failure::without_attempt(
                        FailureCategory::Cancelled,
                        "not started",
                    )),
                    duration_ms: 0,
                };
                self.persist_test(run_id, &result)?;
                results.push(result);
                continue;
            }
            let missing: Vec<String> = test
                .targets
                .iter()
                .filter(|target| {
                    !model_status
                        .get(target)
                        .copied()
                        .unwrap_or_default()
                        .is_satisfied()
                })
                .map(|target| target.logical_name())
                .collect();
            let bad_sources: Vec<String> = test
                .sources
                .iter()
                .filter(|source| {
                    failed_seed_targets.contains(
                        &relation_for_source(source, default_catalog, default_schema).display(),
                    )
                })
                .map(|source| source.logical_name())
                .collect();
            if !missing.is_empty() || !bad_sources.is_empty() {
                // The test is a consumer of the datasets it reads: when they
                // are unavailable it is blocked, not silently dropped.
                let reason = if !missing.is_empty() {
                    format!("targets not ready: {}", missing.join(", "))
                } else {
                    format!("seed sources failed to load: {}", bad_sources.join(", "))
                };
                events.push(EngineEvent::TestFinished {
                    test: test.id.to_string(),
                    status: ExecutionStatus::Blocked,
                    row_count: 0,
                    query_id: None,
                });
                let result = TestResult {
                    test: test.id.to_string(),
                    status: ExecutionStatus::Blocked,
                    row_count: 0,
                    query_id: None,
                    failure: Some(Failure::without_attempt(
                        FailureCategory::Dependency,
                        reason,
                    )),
                    duration_ms: 0,
                };
                self.persist_test(run_id, &result)?;
                results.push(result);
                continue;
            }

            events.push(EngineEvent::TestStarted {
                test: test.id.to_string(),
            });
            let started = Instant::now();
            let run = self.adapter.execute(&test.compiled_sql);
            let outcome = match options.model_timeout {
                Some(limit) => match tokio::time::timeout(limit, run).await {
                    Ok(result) => result.map_err(|error| classify_adapter_error(&error, 1)),
                    Err(_) => Err(timeout_failure(limit, 1)),
                },
                None => run.await.map_err(|error| classify_adapter_error(&error, 1)),
            };
            let duration_ms = started.elapsed().as_millis() as u64;
            // A test "fails" when it returns rows — the assertion found
            // violating data — which is a `test` failure, not a query error.
            let (status, row_count, query_id, failure) = match outcome {
                Ok(query) if query.row_count == 0 => {
                    (ExecutionStatus::Passed, 0, query.query_id, None)
                }
                Ok(query) => (
                    ExecutionStatus::Failed,
                    query.row_count,
                    query.query_id,
                    Some(Failure {
                        category: FailureCategory::Test,
                        message: format!("test returned {} row(s)", query.row_count),
                        adapter_code: None,
                        adapter_message: None,
                        attempt: 1,
                        at: now_rfc3339(),
                        retryable: false,
                    }),
                ),
                Err(failure) => (ExecutionStatus::Failed, 0, None, Some(failure)),
            };
            events.push(EngineEvent::TestFinished {
                test: test.id.to_string(),
                status,
                row_count,
                query_id: query_id.clone(),
            });
            let result = TestResult {
                test: test.id.to_string(),
                status,
                row_count,
                query_id,
                failure,
                duration_ms,
            };
            self.persist_test(run_id, &result)?;
            results.push(result);
        }

        Ok(results)
    }

    /// Persist a test's outcome. Fatal on write error — a failed test must
    /// be on record.
    fn persist_test(&self, run_id: &str, result: &TestResult) -> Result<(), EngineError> {
        let Some(state) = &self.state else {
            return Ok(());
        };
        state.record_test(&TestRunRecord {
            run_id: run_id.to_string(),
            test_id: result.test.clone(),
            status: result.status,
            row_count: result.row_count,
            query_id: result.query_id.clone(),
            error: result
                .failure
                .as_ref()
                .map(|failure| failure.message.clone()),
            error_category: result
                .failure
                .as_ref()
                .map(|failure| failure.category.code().to_string()),
            started_at: now_rfc3339(),
            finished_at: now_rfc3339(),
        })
    }
}

/// Run a model's physical operation once.
async fn execute_op(
    adapter: &Arc<dyn Adapter>,
    target: &Relation,
    compiled_sql: &str,
    op: &ExecOp,
) -> Result<crate::adapter::QueryResult, AdapterError> {
    match op {
        ExecOp::View => adapter.create_or_replace_view(target, compiled_sql).await,
        ExecOp::Table => adapter.create_or_replace_table(target, compiled_sql).await,
        ExecOp::Append => adapter.append(target, compiled_sql).await,
        ExecOp::Merge(keys) => adapter.merge(target, keys, compiled_sql).await,
        ExecOp::ReplacePartitions(columns) => {
            adapter
                .replace_partitions(target, columns, compiled_sql)
                .await
        }
        ExecOp::TimeWindow { predicate } => {
            let filtered =
                format!("SELECT * FROM ({compiled_sql}) AS __phlo_src WHERE {predicate}");
            adapter.append(target, &filtered).await
        }
    }
}

/// Resolve a run id or unique prefix to a full run id.
fn resolve_run_id(state: &dyn StateStore, id_or_prefix: &str) -> Result<String, EngineError> {
    let matches = state.find_runs(id_or_prefix)?;
    match matches.len() {
        0 => Err(EngineError::InvalidPlan(format!(
            "no run matches `{id_or_prefix}`"
        ))),
        1 => Ok(matches[0].run_id.clone()),
        _ => Err(EngineError::InvalidPlan(format!(
            "`{id_or_prefix}` matches {} runs; give a longer prefix",
            matches.len()
        ))),
    }
}

/// A short run id for display.
fn short_id(run_id: &str) -> &str {
    run_id.get(..8).unwrap_or(run_id)
}

impl StoredPlan {
    fn from_plan(plan: &Plan, environment: &Option<String>) -> Self {
        Self {
            plan_id: plan.id.clone(),
            environment: environment.clone(),
            models: plan
                .models
                .iter()
                .map(|model| StoredPlanModel {
                    id: model.id.clone(),
                    target: model.target.clone(),
                    action: model.action.as_str().to_string(),
                    desired_version: model.desired_version.clone(),
                    full_rebuild: model.full_rebuild,
                    watermark: model.watermark.clone(),
                })
                .collect(),
            seeds: plan
                .seeds
                .iter()
                .map(|seed| StoredPlanSeed {
                    name: seed.name.clone(),
                    target: seed.target.clone(),
                    path: seed.path.clone(),
                    action: seed.action.as_str().to_string(),
                    desired_version: seed.desired_version.clone(),
                })
                .collect(),
            tests: plan
                .tests
                .iter()
                .map(|test| StoredPlanTest {
                    id: test.id.clone(),
                    targets: test.targets.clone(),
                    sources: test.sources.clone(),
                })
                .collect(),
        }
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
                .map(|reason| reason.detail.clone())
                .collect(),
        ),
        None => (String::new(), None, Vec::new()),
    }
}

fn seed_result(
    planned_seed: &PlannedSeed,
    status: ExecutionStatus,
    attempts: Vec<Attempt>,
    failure: Option<Failure>,
    duration_ms: u64,
) -> SeedResult {
    SeedResult {
        seed: planned_seed.name.clone(),
        target: planned_seed.target.clone(),
        status,
        attempts,
        failure,
        duration_ms,
    }
}

fn model_result(
    model: &phlo_transform_core::CompiledModel,
    status: ExecutionStatus,
    attempts: Vec<Attempt>,
    started_at: Option<String>,
    finished_at: Option<String>,
    duration_ms: u64,
    info: Option<&PlannedModel>,
) -> ModelResult {
    let (desired_version, previous_version, reasons) = plan_result_fields(info);
    ModelResult {
        model: model.id.logical_name(),
        target: model.target.display(),
        materialization: model.config.materialization.to_string(),
        action: info
            .map(|model| model.action.as_str().to_string())
            .unwrap_or_else(|| "build".to_string()),
        status,
        desired_version,
        previous_version,
        reasons,
        query_id: None,
        attempts,
        failure: None,
        started_at,
        finished_at,
        duration_ms,
        sql_hash: sha256_hex(&model.compiled_sql),
        output_identity: None,
    }
}

impl ModelResult {
    fn with_failure(mut self, failure: Failure) -> Self {
        self.failure = Some(failure);
        self
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

#[allow(clippy::too_many_arguments)]
fn block_dependents(
    id: &ModelId,
    dependents: &BTreeMap<ModelId, Vec<ModelId>>,
    status: &mut BTreeMap<ModelId, ExecutionStatus>,
    results: &mut BTreeMap<ModelId, ModelResult>,
    compilation: &Compilation,
    plan_info: &BTreeMap<ModelId, PlannedModel>,
    events: &mut Vec<EngineEvent>,
) -> Vec<ModelId> {
    let mut newly_blocked = Vec::new();
    let Some(children) = dependents.get(id) else {
        return newly_blocked;
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
                    Vec::new(),
                    None,
                    None,
                    0,
                    plan_info.get(child),
                )
                .with_failure(Failure::without_attempt(
                    FailureCategory::Dependency,
                    format!("upstream {} failed", id.logical_name()),
                )),
            );
        }
        newly_blocked.push(child.clone());
        newly_blocked.extend(block_dependents(
            child,
            dependents,
            status,
            results,
            compilation,
            plan_info,
            events,
        ));
    }
    newly_blocked
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
