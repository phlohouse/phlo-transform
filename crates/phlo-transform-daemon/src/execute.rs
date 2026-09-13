//! Operation executors: run/test/promote/reload, reusing the same engine
//! entry points and report DTOs as the CLI so API results are byte-identical
//! in shape to `--json` output.

use std::sync::Arc;

use serde_json::{json, Value};

use phlo_transform_core::{resolve_selection, SelectorSet};
use phlo_transform_engine::{
    audited_diff, audited_lineage, catalog_name, contract_breaking_changes, evaluate_gates,
    read_environment_for, remove_environment_artifacts, ArtifactWriter, ExecutionStatus, GateInput,
    PlanOptions, Planner, PromotionRequest, RetryPolicy, RunOptions, Runner,
};

use crate::operations::{
    OperationError, OperationStore, Params, PromoteParams, RunParams, TestParams,
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
        Params::Test(p) => op_test(&service, &ops, &id, p).await,
        Params::Promote(p) => op_promote(&service, &ops, &id, p).await,
        Params::Reload(_) => op_reload(&service).await,
    }
}

async fn op_run(
    service: &Arc<WorkspaceService>,
    ops: &Arc<OperationStore>,
    id: &str,
    params: RunParams,
) -> Result<Value, OperationError> {
    let adapter = service.adapter().ok_or_else(|| not_configured("adapter"))?;
    let compilation = service.snapshot();
    let set = SelectorSet::parse(&params.selectors, &[], &[], false, false)
        .map_err(|error| op_error("API006", error.to_string()))?;
    let selection = resolve_selection(&compilation, &set, None)
        .map_err(|error| op_error("API006", error.to_string()))?;
    let environment = params
        .environment
        .or_else(|| service.default_environment().map(str::to_string));
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
        retry: RetryPolicy::default(),
        ..RunOptions::default()
    };
    let result = Runner::new(adapter, service.state())
        .apply(&compilation, &plan, &options)
        .await
        .map_err(|error| op_error("API011", error.to_string()))?;
    // Bind a passed run to the environment's post-run Nessie head — the
    // commit its writes produced, not the pre-run snapshot. An unbound run
    // cannot later promote.
    if result.status == ExecutionStatus::Passed {
        if let (Some(state), Some(environment), Some(nessie)) = (
            service.state(),
            result.environment.as_deref(),
            service.nessie(),
        ) {
            let head = nessie
                .get_reference(environment)
                .await
                .map_err(|error| op_error("API011", error.to_string()))?;
            if let Some(head) = head {
                state
                    .bind_run_reference_hash(&result.run_id, &head.hash)
                    .map_err(|error| op_error("API011", error.to_string()))?;
            }
        }
    }
    writer
        .write_run(&result)
        .map_err(|error| op_error("API011", error.to_string()))?;
    // The runner's cancel signal IS this op's CancelHandle: when it reports
    // cancelled, finish() marks the record cancelled automatically.
    Ok(serde_json::to_value(&result).expect("run report serialises"))
}

async fn op_test(
    service: &Arc<WorkspaceService>,
    ops: &Arc<OperationStore>,
    id: &str,
    params: TestParams,
) -> Result<Value, OperationError> {
    let adapter = service.adapter().ok_or_else(|| not_configured("adapter"))?;
    let compilation = service.snapshot();
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
    let mut results = Vec::new();
    let mut failed = false;
    for test in &compilation.tests {
        if ops.is_cancelled(id) {
            break;
        }
        if let Some(members) = &members {
            let covered = !test.targets.is_empty()
                && test
                    .targets
                    .iter()
                    .all(|target| members.contains(target.logical_name().as_str()));
            if !covered {
                continue;
            }
        }
        let outcome = adapter.execute(&test.compiled_sql).await;
        let (status, row_count, error) = match outcome {
            Ok(query) if query.row_count == 0 => (ExecutionStatus::Passed, 0, None),
            Ok(query) => (
                ExecutionStatus::Failed,
                query.row_count,
                Some(format!("test returned {} row(s)", query.row_count)),
            ),
            Err(error) => (ExecutionStatus::Failed, 0, Some(error.to_string())),
        };
        if status == ExecutionStatus::Failed {
            failed = true;
        }
        results.push(json!({
            "test": test.id.to_string(),
            "status": status,
            "row_count": row_count,
            "error": error,
        }));
    }
    Ok(json!({ "tests": results, "failed": failed }))
}

#[allow(clippy::too_many_lines)]
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

    let candidate_reference = nessie
        .get_reference(candidate)
        .await
        .map_err(|error| op_error("API011", error.to_string()))?
        .ok_or_else(|| {
            op_error(
                "API013",
                format!("candidate reference `{candidate}` was not found"),
            )
        })?;
    let target = nessie
        .get_reference(to)
        .await
        .map_err(|error| op_error("API011", error.to_string()))?
        .ok_or_else(|| op_error("API013", format!("target reference `{to}` was not found")))?;

    let run = match &state {
        Some(state) => state
            .latest_run(Some(candidate))
            .map_err(|error| op_error("API011", error.to_string()))?,
        None => None,
    };
    let (model_runs, seed_runs, test_runs) = match (&state, &run) {
        (Some(state), Some(run)) => (
            state
                .model_runs(&run.run_id)
                .map_err(|error| op_error("API011", error.to_string()))?,
            state
                .seed_runs(&run.run_id)
                .map_err(|error| op_error("API011", error.to_string()))?,
            state
                .test_runs(&run.run_id)
                .map_err(|error| op_error("API011", error.to_string()))?,
        ),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };

    let environment = read_environment_for(&root, candidate);
    let mut audit = audited_diff(
        &root,
        state.as_deref(),
        candidate,
        to,
        Some(&candidate_reference.hash),
        Some(&target.hash),
    );
    // Contract breaks are computed live — the workspace's desired contracts
    // against what the target environment last recorded — so a contract
    // edited after `diff` cannot sneak a break past the gate on a stale
    // artifact's analysis.
    // The recorded base contracts are promotion evidence — a store error
    // fails the operation rather than reading as "no contracts recorded".
    audit.breaking_schema_changes.extend(
        contract_breaking_changes(state.as_deref(), &compilation, to)
            .map_err(|error| op_error("API011", error.to_string()))?,
    );
    let merge_check = nessie.can_merge(candidate, to).await.ok();
    // Lineage evidence: the artifact only speaks for this promotion when
    // the identities it was produced against still hold — including the
    // candidate's compiled lineage fingerprint — stale or unbound reports
    // are surfaced as such, never silently as "no changes".
    let lineage = audited_lineage(
        &root,
        candidate,
        to,
        &candidate_reference.hash,
        &target.hash,
        Some(&compilation.lineage.fingerprint()),
    );

    // The `base` gate needs a target commit the evidence was established
    // against. Two sources, freshest first: a hash-bound diff artifact that
    // audited this exact target, else the recorded commit the candidate was
    // provably created from. A candidate whose origin is unknown and which
    // was never audited against this target has no evidence — the gate must
    // fail rather than redefine its base as today's head.
    let expected_target_hash = audit.audited_base_hash.clone().or_else(|| {
        environment
            .as_ref()
            .filter(|setup| setup.candidate.name == candidate && setup.base.name == to)
            .and_then(|setup| setup.created_from.as_ref())
            .map(|base| base.hash.clone())
    });

    let input = GateInput {
        run: run.clone(),
        model_runs,
        seed_runs,
        test_runs,
        require_diff: params.require_diff,
        diff_passed: audit.diff_passed,
        diff_rejected: audit.diff_rejected.clone(),
        breaking_schema_changes: audit.breaking_schema_changes.clone(),
        allow_breaking_schema: params.allow_breaking_schema,
        expected_target_hash: expected_target_hash.clone(),
        actual_target_hash: Some(target.hash.clone()),
        actual_candidate_hash: Some(candidate_reference.hash.clone()),
        schema_audited: audit.schema_audited,
        merge_check,
    };
    let report = evaluate_gates(&input);

    if !report.passed || params.check {
        return Ok(json!({
            "ok": report.passed,
            "candidate_ref": candidate,
            "target_ref": to,
            "check_only": params.check,
            "gates": report.results,
            "lineage": lineage,
        }));
    }
    if ops.is_cancelled(id) {
        return Err(op_error("cancelled", "cancelled before merge".into()));
    }

    let request = PromotionRequest {
        candidate_ref: candidate.to_string(),
        target_ref: to.to_string(),
        candidate_hash: Some(candidate_reference.hash.clone()),
        expected_target_hash: expected_target_hash.or_else(|| Some(target.hash.clone())),
        plan_id: run.as_ref().map(|run| run.plan_id.clone()),
        run_id: run.as_ref().map(|run| run.run_id.clone()),
        quality_gates_passed: true,
        diff_passed: audit.diff_passed,
        require_diff: params.require_diff,
        breaking_schema_changes: input.breaking_schema_changes,
        allow_breaking_schema: params.allow_breaking_schema,
        dry_run: false,
        actor: params.actor,
        gates: report.results.clone(),
    };
    let record = phlo_transform_engine::promote(nessie.as_ref(), &request)
        .await
        .map_err(|error| op_error("API011", error.to_string()))?;
    ArtifactWriter::for_workspace(&root)
        .write_promotion(&record)
        .map_err(|error| op_error("API011", error.to_string()))?;
    if let Some(state) = &state {
        state
            .record_promotion(&record)
            .map_err(|error| op_error("API011", error.to_string()))?;
    }
    let cleanup_error = if params.cleanup && record.merged {
        cleanup_candidate(service, candidate, environment.as_ref())
            .await
            .err()
    } else {
        None
    };
    Ok(json!({
        "ok": cleanup_error.is_none(),
        "gates": report.results,
        "lineage": lineage,
        "promotion": record,
        "cleanup_error": cleanup_error,
    }))
}

/// Drop the candidate's catalog and branch after a merge — every failure is
/// reported, never silent (same contract as the CLI's cleanup).
async fn cleanup_candidate(
    service: &Arc<WorkspaceService>,
    candidate: &str,
    environment: Option<&phlo_transform_engine::EnvironmentSetup>,
) -> Result<(), String> {
    let catalog = environment
        .map(|setup| setup.catalog.clone())
        .unwrap_or_else(|| catalog_name(candidate));
    let mut failures = Vec::new();
    match service.adapter() {
        Some(adapter) => {
            if let Err(error) = adapter
                .execute(&format!("DROP CATALOG IF EXISTS {}", catalog))
                .await
            {
                failures.push(format!("drop catalog `{catalog}`: {error}"));
            }
        }
        None => {
            failures.push(format!("no adapter configured to drop catalog `{catalog}`"));
        }
    }
    if let Some(nessie) = service.nessie() {
        if let Err(error) = nessie.delete_branch(candidate).await {
            failures.push(format!("delete branch `{candidate}`: {error}"));
        }
    }
    if failures.is_empty() {
        remove_environment_artifacts(service.root(), candidate);
        Ok(())
    } else {
        Err(failures.join("; "))
    }
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
