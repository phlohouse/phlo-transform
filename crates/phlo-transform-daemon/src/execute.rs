//! Operation executors: run/resume/retry_failed/test/promote/reload,
//! reusing the same engine entry points and report DTOs as the CLI so API
//! results are byte-identical in shape to `--json` output.

use std::sync::Arc;

use serde_json::{json, Value};

use phlo_transform_core::{resolve_selection, Compilation, SelectorSet};
use phlo_transform_engine::{
    base_ref_for, bind_run_reference, cleanup_candidate, evaluate_promotion, execute_tests,
    find_unique_run, persist_promotion, promote, ArtifactWriter, EngineError, EnvironmentMode,
    ExecutionStatus, PlanOptions, Planner, PromotionOptions, RetryPolicy, RunOptions, RunResult,
    Runner,
};

use crate::operations::{
    ContinueParams, OperationError, OperationStore, Params, PromoteParams, RunParams, TestParams,
};
use crate::WorkspaceService;

fn op_error(code: &str, message: String) -> OperationError {
    OperationError {
        code: code.to_string(),
        message,
    }
}

fn not_configured(what: &str) -> OperationError {
    op_error(
        "API007",
        format!("{what} is not configured for this daemon instance"),
    )
}

/// Execute a submitted operation. The returned value finishes the record:
/// `Ok(report)` on success (even when the report itself carries `ok: false`,
/// e.g. a gate rejection — the operation delivered its verdict), `Err` for
/// infrastructure/engine failures.
pub async fn execute(
    service: Arc<WorkspaceService>,
    ops: Arc<OperationStore>,
    id: String,
    params: Params,
) -> Result<Value, OperationError> {
    match params {
        Params::Run(p) => op_run(&service, &ops, &id, p).await,
        Params::Resume(p) => op_continue(&service, &ops, &id, p, Continuation::Resume).await,
        Params::RetryFailed(p) => {
            op_continue(&service, &ops, &id, p, Continuation::RetryFailed).await
        }
        Params::Test(p) => op_test(&service, &ops, &id, p).await,
        Params::Promote(p) => op_promote(&service, &ops, &id, p).await,
        Params::Reload(_) => op_reload(&service).await,
    }
}

/// Map a resolution failure to the operation error surface: a configured
/// Nessie whose candidate catalog cannot be provisioned is `API007` (the
/// environment claims branch semantics the daemon cannot honour), every
/// other engine failure is `API011`.
fn resolution_error(error: EngineError) -> OperationError {
    let code = match error {
        EngineError::NotConfigured(_) => "API007",
        EngineError::NotFound(_) => "API013",
        _ => "API011",
    };
    op_error(code, error.to_string())
}

/// The compilation an environment-targeted operation must plan and execute
/// against, resolved through the shared environment context in `Ensure`
/// mode: a candidate environment provisions its Nessie branch and catalog,
/// then the workspace recompiles retargeted at it — so the run physically
/// writes to the environment it claims.
///
/// With no Nessie client the environment can only ever be a state-record
/// label — local mode is truthful and the shared snapshot is used. Once a
/// Nessie client is in play a named environment claims branch semantics:
/// if the daemon cannot provision branch isolation the operation fails
/// closed rather than run against the default target and bind its evidence
/// to the candidate's head.
async fn target_for(
    service: &Arc<WorkspaceService>,
    environment: Option<&str>,
    base_ref: &str,
) -> Result<Arc<Compilation>, OperationError> {
    let target = service
        .environment_context()
        .resolve(environment, base_ref, EnvironmentMode::Ensure)
        .await
        .map_err(resolution_error)?;
    match target.compilation {
        Some(compilation) => Ok(Arc::new(compilation)),
        None => Ok(service.snapshot()),
    }
}

/// Bind a passed run to its environment's post-run Nessie head — the
/// commit its writes produced, not the pre-run snapshot. An unbound run
/// cannot later promote. The run's own environment label is used, so
/// `resume`/`retry_failed` bind the environment the original run targeted.
async fn bind_reference(
    service: &Arc<WorkspaceService>,
    result: &RunResult,
) -> Result<(), OperationError> {
    if result.status != ExecutionStatus::Passed {
        return Ok(());
    }
    let (Some(state), Some(nessie)) = (service.state(), service.nessie()) else {
        return Ok(());
    };
    bind_run_reference(nessie.as_ref(), state.as_ref(), result)
        .await
        .map_err(|error| op_error("API011", error.to_string()))
}

async fn op_run(
    service: &Arc<WorkspaceService>,
    ops: &Arc<OperationStore>,
    id: &str,
    params: RunParams,
) -> Result<Value, OperationError> {
    let adapter = service.adapter().ok_or_else(|| not_configured("adapter"))?;
    let environment = params
        .environment
        .or_else(|| service.default_environment().map(str::to_string));
    let base_ref = base_ref_for(
        service.root(),
        service.state().as_deref(),
        environment.as_deref(),
        params.base.as_deref(),
    )
    .map_err(|error| op_error("API011", error.to_string()))?;
    let compilation = target_for(service, environment.as_deref(), &base_ref).await?;
    let set = SelectorSet::parse(&params.selectors, &[], &[], false, false)
        .map_err(|error| op_error("API006", error.to_string()))?;
    let selection = resolve_selection(&compilation, &set, None)
        .map_err(|error| op_error("API006", error.to_string()))?;
    let planner = Planner::new(adapter.clone(), service.state());
    let plan = planner
        .plan(
            &compilation,
            &selection,
            environment.clone(),
            &PlanOptions {
                force: params.force,
            },
        )
        .await
        .map_err(|error| op_error("API011", error.to_string()))?;
    let writer = ArtifactWriter::for_workspace(service.root());
    writer
        .write_project(&compilation)
        .and_then(|_| writer.write_plan(&plan))
        .map_err(|error| op_error("API011", error.to_string()))?;
    if ops.is_cancelled(id) {
        return Err(op_error("cancelled", "cancelled before execution".into()));
    }
    let options = RunOptions {
        environment,
        run_tests: params.run_tests.unwrap_or(true),
        cancel: ops.cancel_handle(id),
        retry: RetryPolicy {
            retries: params.retries.unwrap_or(0),
            ..RetryPolicy::default()
        },
        ..RunOptions::default()
    };
    let result = Runner::new(adapter, service.state())
        .apply(&compilation, &plan, &options)
        .await
        .map_err(|error| op_error("API011", error.to_string()))?;
    bind_reference(service, &result).await?;
    writer
        .write_run(&result)
        .map_err(|error| op_error("API011", error.to_string()))?;
    // The runner's cancel signal IS this op's CancelHandle: when it reports
    // cancelled, finish() marks the record cancelled automatically.
    Ok(serde_json::to_value(&result).expect("run report serialises"))
}

/// Which continuation an operation runs: `resume` continues an interrupted
/// run under its original id; `retry_failed` starts a new run over the
/// failed portion of a finished one.
enum Continuation {
    Resume,
    RetryFailed,
}

/// `resume` and `retry_failed`: the stored run's environment is
/// authoritative — the continuation provisions and retargets whatever
/// environment the original run targeted, exactly like the CLI's
/// `run --resume/--retry-failed --ref <env>`.
async fn op_continue(
    service: &Arc<WorkspaceService>,
    ops: &Arc<OperationStore>,
    id: &str,
    params: ContinueParams,
    continuation: Continuation,
) -> Result<Value, OperationError> {
    let adapter = service.adapter().ok_or_else(|| not_configured("adapter"))?;
    let state = service.state().ok_or_else(|| not_configured("state"))?;
    let environment = find_unique_run(state.as_ref(), &params.run)
        .map_err(|error| match error {
            EngineError::Ambiguous(_) => op_error("API014", error.to_string()),
            _ => op_error("API013", error.to_string()),
        })?
        .environment;
    // Provision against the base the environment was cut from when that is
    // recorded, else the conventional `main`.
    let base_ref = base_ref_for(
        service.root(),
        service.state().as_deref(),
        environment.as_deref(),
        None,
    )
    .map_err(|error| op_error("API011", error.to_string()))?;
    let compilation = target_for(service, environment.as_deref(), &base_ref).await?;
    let options = RunOptions {
        environment,
        run_tests: true,
        cancel: ops.cancel_handle(id),
        retry: RetryPolicy {
            retries: params.retries.unwrap_or(0),
            ..RetryPolicy::default()
        },
        ..RunOptions::default()
    };
    let runner = Runner::new(adapter, service.state());
    let result = match continuation {
        Continuation::Resume => runner.resume(&compilation, &params.run, &options).await,
        Continuation::RetryFailed => {
            runner
                .retry_failed(&compilation, &params.run, &options)
                .await
        }
    }
    .map_err(|error| op_error("API011", error.to_string()))?;
    bind_reference(service, &result).await?;
    let writer = ArtifactWriter::for_workspace(service.root());
    writer
        .write_project(&compilation)
        .and_then(|_| writer.write_run(&result))
        .map_err(|error| op_error("API011", error.to_string()))?;
    Ok(serde_json::to_value(&result).expect("run report serialises"))
}

async fn op_test(
    service: &Arc<WorkspaceService>,
    ops: &Arc<OperationStore>,
    id: &str,
    params: TestParams,
) -> Result<Value, OperationError> {
    let adapter = service.adapter().ok_or_else(|| not_configured("adapter"))?;
    // Tests only read, so the environment resolves without provisioning —
    // `ReadOnly` gives the same catalog a run would target, and an
    // unprovisioned environment's queries fail honestly instead of
    // silently running on the default catalog. Like `run`, an operation
    // without an explicit environment inherits the daemon's configured one.
    let environment = params
        .environment
        .or_else(|| service.default_environment().map(str::to_string));
    let base_ref = base_ref_for(
        service.root(),
        service.state().as_deref(),
        environment.as_deref(),
        params.base.as_deref(),
    )
    .map_err(|error| op_error("API011", error.to_string()))?;
    let compilation = match environment.as_deref() {
        Some(environment) => {
            let target = service
                .environment_context()
                .resolve(Some(environment), &base_ref, EnvironmentMode::ReadOnly)
                .await
                .map_err(resolution_error)?;
            match target.compilation {
                Some(compilation) => Arc::new(compilation),
                None => service.snapshot(),
            }
        }
        None => service.snapshot(),
    };
    // Same rule as the CLI: a test runs when every model it reads is
    // selected; tests that only read sources run under unrestricted
    // selection.
    let members: Option<std::collections::BTreeSet<String>> = if params.selectors.is_empty() {
        None
    } else {
        let set = SelectorSet::parse(&params.selectors, &[], &[], false, false)
            .map_err(|error| op_error("API006", error.to_string()))?;
        let selection = resolve_selection(&compilation, &set, None)
            .map_err(|error| op_error("API006", error.to_string()))?;
        Some(
            selection
                .members
                .iter()
                .map(|member| member.id.clone())
                .collect(),
        )
    };
    let results = execute_tests(
        adapter.as_ref(),
        &compilation.tests,
        members.as_ref(),
        &ops.cancel_handle(id),
    )
    .await;
    let failed = results
        .iter()
        .any(|outcome| outcome.status == ExecutionStatus::Failed);
    Ok(json!({ "tests": results, "failed": failed }))
}

async fn op_promote(
    service: &Arc<WorkspaceService>,
    ops: &Arc<OperationStore>,
    id: &str,
    params: PromoteParams,
) -> Result<Value, OperationError> {
    let nessie = service.nessie().ok_or_else(|| not_configured("nessie"))?;
    let state = service.state();
    let root = service.root().to_path_buf();
    let compilation = service.snapshot();
    let candidate = params.candidate.as_str();
    let to = params.to.as_str();
    let options = PromotionOptions {
        require_diff: params.require_diff,
        allow_breaking_schema: params.allow_breaking_schema,
    };

    // The shared pre-merge audit: both control surfaces authorise a merge
    // under identical rules — a missing candidate/target reads as the
    // caller's not-found surface.
    let evaluation = evaluate_promotion(
        &root,
        nessie.as_ref(),
        state.as_deref(),
        &compilation,
        candidate,
        to,
        &options,
    )
    .await
    .map_err(|error| match error {
        EngineError::NotFound(message) => op_error("API013", message),
        error => op_error("API011", error.to_string()),
    })?;
    let report = &evaluation.gates;

    if !report.passed || params.check {
        return Ok(json!({
            "ok": report.passed,
            "candidate_ref": candidate,
            "target_ref": to,
            "check_only": params.check,
            "gates": report.results,
            "lineage": evaluation.lineage,
        }));
    }
    if ops.is_cancelled(id) {
        return Err(op_error("cancelled", "cancelled before merge".into()));
    }

    let request = evaluation.request(&options, params.actor);
    let record = promote(nessie.as_ref(), &request)
        .await
        .map_err(|error| match error {
            EngineError::NotFound(message) => op_error("API013", message),
            error => op_error("API011", error.to_string()),
        })?;
    persist_promotion(&root, state.as_deref(), &record)
        .map_err(|error| op_error("API011", error.to_string()))?;
    let cleanup_error = if params.cleanup && record.merged {
        cleanup_candidate(
            &root,
            state.as_deref(),
            service.adapter().as_deref(),
            nessie.as_ref(),
            candidate,
            evaluation.environment.as_ref(),
        )
        .await
        .err()
    } else {
        None
    };
    Ok(json!({
        "ok": cleanup_error.is_none(),
        "gates": report.results,
        "lineage": evaluation.lineage,
        "promotion": record,
        "cleanup_error": cleanup_error,
    }))
}

async fn op_reload(service: &Arc<WorkspaceService>) -> Result<Value, OperationError> {
    let service = service.clone();
    tokio::task::spawn_blocking(move || {
        service.reload();
        let snapshot = service.snapshot();
        json!({
            "models": snapshot.models.len(),
            "tests": snapshot.tests.len(),
            "diagnostics": snapshot.diagnostics.len(),
            "errors": snapshot.errors().count(),
            "last_update": service.last_update(),
        })
    })
    .await
    .map_err(|error| op_error("API011", error.to_string()))
}
