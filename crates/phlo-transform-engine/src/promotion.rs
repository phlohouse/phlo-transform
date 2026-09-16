//! Write-Audit-Publish promotion.
//!
//! A candidate environment (a Nessie branch) is written and audited, then
//! promoted to a target reference. Promotion validates quality gates and
//! target staleness before merging, and records a reproducible artifact.

use std::path::Path;

use serde::{Deserialize, Serialize};

use phlo_transform_core::Compilation;
use phlo_transform_nessie::{Conflict, NessieClient, NessieError, ReferenceInfo};

use crate::environment::EnvironmentSetup;
use crate::error::EngineError;
use crate::gates::{evaluate_gates, GateReport};
use crate::state::RunSummary;
use crate::util::now_rfc3339;

/// A promotion request.
#[derive(Clone, Debug)]
pub struct PromotionRequest {
    pub candidate_ref: String,
    pub target_ref: String,
    /// The candidate hash the gates were evaluated against. When given, the
    /// candidate must still be at this hash at merge time — a branch that
    /// advanced since the audit is refused rather than merged unaudited.
    pub candidate_hash: Option<String>,
    /// Target hash observed when the candidate was planned.
    pub expected_target_hash: Option<String>,
    pub plan_id: Option<String>,
    pub run_id: Option<String>,
    /// Whether the candidate run completed successfully with tests passing.
    pub quality_gates_passed: bool,
    /// Whether a required data diff passed, when a diff gate applies.
    pub diff_passed: Option<bool>,
    pub require_diff: bool,
    /// Breaking schema changes observed in the audit (removed/incompatible
    /// columns). Promotion is blocked unless `allow_breaking_schema` is set.
    pub breaking_schema_changes: Vec<String>,
    pub allow_breaking_schema: bool,
    /// Only check preconditions; do not merge.
    pub dry_run: bool,
    pub actor: Option<String>,
    /// Gate results computed by the caller, carried into the record.
    pub gates: Vec<crate::gates::GateResult>,
}

/// A reproducible promotion record.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromotionRecord {
    pub promotion_id: String,
    pub candidate_ref: String,
    pub candidate_hash: Option<String>,
    pub target_ref: String,
    pub target_hash_before: String,
    pub target_hash_after: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub dry_run: bool,
    pub merged: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
    /// The gate evaluation that authorised (or refused) this promotion.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<crate::gates::GateResult>,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
}

/// Promote an audited candidate to the target reference.
pub async fn promote(
    nessie: &dyn NessieClient,
    request: &PromotionRequest,
) -> Result<PromotionRecord, EngineError> {
    if !request.quality_gates_passed {
        return Err(EngineError::Promotion(
            "quality gates have not passed; refusing to promote".to_string(),
        ));
    }
    if request.require_diff && request.diff_passed != Some(true) {
        return Err(EngineError::Promotion(
            "a passing data diff is required before promotion".to_string(),
        ));
    }
    if !request.allow_breaking_schema && !request.breaking_schema_changes.is_empty() {
        return Err(EngineError::Promotion(format!(
            "breaking schema changes block promotion: {}",
            request.breaking_schema_changes.join(", ")
        )));
    }

    let candidate = nessie
        .get_reference(&request.candidate_ref)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
        .ok_or_else(|| {
            EngineError::NotFound(format!(
                "candidate reference `{}` was not found",
                request.candidate_ref
            ))
        })?;

    let target = nessie
        .get_reference(&request.target_ref)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
        .ok_or_else(|| {
            EngineError::NotFound(format!(
                "target reference `{}` was not found",
                request.target_ref
            ))
        })?;

    // The candidate must still be the commit that was audited: a branch that
    // advanced between gate evaluation and promotion carries unaudited work,
    // and merging it would make this record's `candidate_hash` a lie.
    if let Some(expected) = &request.candidate_hash {
        if expected != &candidate.hash {
            return Err(EngineError::Promotion(format!(
                "candidate `{}` advanced since the audit (expected {}, found {}); \
                 re-run the audit and gates",
                request.candidate_ref, expected, candidate.hash
            )));
        }
    }

    let mut record = PromotionRecord {
        promotion_id: uuid::Uuid::new_v4().to_string(),
        candidate_ref: request.candidate_ref.clone(),
        candidate_hash: request
            .candidate_hash
            .clone()
            .or_else(|| Some(candidate.hash.clone())),
        target_ref: request.target_ref.clone(),
        target_hash_before: target.hash.clone(),
        target_hash_after: None,
        plan_id: request.plan_id.clone(),
        run_id: request.run_id.clone(),
        dry_run: request.dry_run,
        merged: false,
        conflicts: Vec::new(),
        gates: request.gates.clone(),
        timestamp: now_rfc3339(),
        actor: request.actor.clone(),
    };

    // The target must not have moved since the candidate was planned.
    if let Some(expected) = &request.expected_target_hash {
        if expected != &target.hash {
            return Err(EngineError::Promotion(format!(
                "target `{}` advanced since planning (expected {}, found {}); re-plan the candidate",
                request.target_ref, expected, target.hash
            )));
        }
    }

    let merge = if request.dry_run {
        nessie
            .can_merge(&request.candidate_ref, &request.target_ref)
            .await
            .map_err(|error| EngineError::Promotion(error.to_string()))?
    } else {
        nessie
            .merge(
                &request.candidate_ref,
                Some(candidate.hash.as_str()),
                &request.target_ref,
                request.expected_target_hash.as_deref(),
            )
            .await
            .map_err(|error| EngineError::Promotion(error.to_string()))?
    };

    if !merge.is_clean() {
        let detail = merge
            .conflicts
            .iter()
            .map(|conflict| format!("{}: {}", conflict.path, conflict.message))
            .collect::<Vec<_>>()
            .join("; ");
        record.conflicts = merge.conflicts;
        return Err(EngineError::Promotion(format!(
            "candidate cannot be promoted: {detail}"
        )));
    }

    if request.dry_run {
        record.target_hash_after = merge.hash;
        return Ok(record);
    }

    record.merged = true;
    record.target_hash_after = merge.hash;
    Ok(record)
}

/// Gate-policy switches a caller passes through.
#[derive(Clone, Copy, Debug, Default)]
pub struct PromotionOptions {
    /// Require a passing data diff before promotion.
    pub require_diff: bool,
    /// Waive breaking schema changes found by the audit.
    pub allow_breaking_schema: bool,
}

/// Everything a promotion decision is made from — the evidence gathered,
/// the gate verdict over it, and the resolved references. Shared by the
/// CLI's `promote` and the daemon's promote operation so both surfaces
/// authorise a merge under identical rules.
pub struct PromotionEvaluation {
    /// The candidate reference as Nessie currently resolves it.
    pub candidate: ReferenceInfo,
    /// The target reference as Nessie currently resolves it.
    pub target: ReferenceInfo,
    /// The latest run recorded under the candidate's environment label.
    pub run: Option<RunSummary>,
    /// What the persisted diff artifact proves for this promotion.
    pub audit: crate::audit::AuditEvidence,
    /// The lineage artifact's standing under the gates (advisory context).
    pub lineage: Option<crate::audit::LineageEvidence>,
    /// The environment provisioning record for the candidate, when known —
    /// callers use it for cleanup decisions after a merge.
    pub environment: Option<EnvironmentSetup>,
    /// The base commit the evidence was established against — the hash the
    /// merge asserts on.
    pub expected_target_hash: Option<String>,
    /// The gate verdict.
    pub gates: GateReport,
}

impl PromotionEvaluation {
    /// The promotion request this evaluation authorises — only meaningful
    /// when `gates.passed`. The target hash is asserted at merge time: when
    /// no evidence base was recorded, the just-resolved head is pinned so a
    /// commit racing the promotion is rejected rather than merged over.
    pub fn request(&self, options: &PromotionOptions, actor: Option<String>) -> PromotionRequest {
        PromotionRequest {
            candidate_ref: self.candidate.name.clone(),
            target_ref: self.target.name.clone(),
            candidate_hash: Some(self.candidate.hash.clone()),
            expected_target_hash: self
                .expected_target_hash
                .clone()
                .or_else(|| Some(self.target.hash.clone())),
            plan_id: self.run.as_ref().map(|run| run.plan_id.clone()),
            run_id: self.run.as_ref().map(|run| run.run_id.clone()),
            quality_gates_passed: true,
            diff_passed: self.audit.diff_passed,
            require_diff: options.require_diff,
            breaking_schema_changes: self.audit.breaking_schema_changes.clone(),
            allow_breaking_schema: options.allow_breaking_schema,
            dry_run: false,
            actor,
            gates: self.gates.results.clone(),
        }
    }
}

/// Gather the evidence for a `candidate` → `to` promotion and evaluate the
/// gates over it — the shared pre-merge audit both control surfaces run.
/// Reads the evidence store's audit records (with the workspace's
/// `branch_diff.json`, `lineage_diff.json`, `environment*.json` artifacts
/// as the compatibility/export path) and the store's run history; computes
/// contract breaks live so an edit after `diff` cannot sneak a break past
/// the gate on a stale artifact's analysis.
pub async fn evaluate_promotion(
    workspace_root: &Path,
    nessie: &dyn NessieClient,
    state: Option<&dyn crate::state::StateStore>,
    compilation: &Compilation,
    candidate: &str,
    to: &str,
    options: &PromotionOptions,
) -> Result<PromotionEvaluation, EngineError> {
    let candidate_reference = nessie
        .get_reference(candidate)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
        .ok_or_else(|| {
            EngineError::NotFound(format!("candidate reference `{candidate}` was not found"))
        })?;
    let target = nessie
        .get_reference(to)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
        .ok_or_else(|| EngineError::NotFound(format!("target reference `{to}` was not found")))?;

    let run = match state {
        Some(state) => state
            .latest_run(Some(candidate))
            .map_err(|error| EngineError::Promotion(error.to_string()))?,
        None => None,
    };
    let (model_runs, seed_runs, test_runs) = match (state, &run) {
        (Some(state), Some(run)) => (
            state
                .model_runs(&run.run_id)
                .map_err(|error| EngineError::Promotion(error.to_string()))?,
            state
                .seed_runs(&run.run_id)
                .map_err(|error| EngineError::Promotion(error.to_string()))?,
            state
                .test_runs(&run.run_id)
                .map_err(|error| EngineError::Promotion(error.to_string()))?,
        ),
        _ => (Vec::new(), Vec::new(), Vec::new()),
    };

    let environment = crate::audit::read_environment_for(workspace_root, state, candidate)?;
    let mut audit = crate::audit::audited_diff(
        workspace_root,
        state,
        candidate,
        to,
        Some(&candidate_reference.hash),
        Some(&target.hash),
    );
    // Contract breaks are computed live — the workspace's desired contracts
    // against what the target environment last recorded — so a contract
    // edited after `diff` cannot sneak a break past the gate on a stale
    // artifact's analysis. The recorded contracts are promotion evidence —
    // a store error fails rather than reading as "no contracts recorded".
    audit
        .breaking_schema_changes
        .extend(crate::audit::contract_breaking_changes(
            state,
            compilation,
            to,
        )?);
    let merge_check = nessie.can_merge(candidate, to).await.ok();
    // Lineage evidence: the artifact only speaks for this promotion when
    // the identities it was produced against still hold — including the
    // candidate's compiled lineage fingerprint.
    let lineage = crate::audit::audited_lineage(
        workspace_root,
        state,
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

    let gates = evaluate_gates(&crate::gates::GateInput {
        run: run.clone(),
        model_runs,
        seed_runs,
        test_runs,
        require_diff: options.require_diff,
        diff_passed: audit.diff_passed,
        diff_rejected: audit.diff_rejected.clone(),
        breaking_schema_changes: audit.breaking_schema_changes.clone(),
        allow_breaking_schema: options.allow_breaking_schema,
        expected_target_hash: expected_target_hash.clone(),
        actual_target_hash: Some(target.hash.clone()),
        actual_candidate_hash: Some(candidate_reference.hash.clone()),
        schema_audited: audit.schema_audited,
        merge_check,
    });

    Ok(PromotionEvaluation {
        candidate: candidate_reference,
        target,
        run,
        audit,
        lineage,
        environment,
        expected_target_hash,
        gates,
    })
}

/// Record a completed promotion: the workspace artifact plus the shared
/// state row. The artifact is the human-readable export; the state row is
/// what audit reads back.
pub fn persist_promotion(
    workspace_root: &Path,
    state: Option<&dyn crate::state::StateStore>,
    record: &PromotionRecord,
) -> Result<(), EngineError> {
    crate::artifacts::ArtifactWriter::for_workspace(workspace_root)
        .write_promotion(record)
        .map_err(|error| EngineError::Artifact(error.to_string()))?;
    if let Some(state) = state {
        state
            .record_promotion(record)
            .map_err(|error| EngineError::Promotion(error.to_string()))?;
    }
    Ok(())
}

/// Removal of a promoted candidate's catalog and branch. Every failure is
/// reported — the merge already happened, so a leftover catalog or branch
/// must never be silent. Only a catalog phlo provably owns is dropped: an
/// adopted or unmanaged catalog belongs to someone else, and without
/// recorded ownership the name is only a guess.
pub async fn cleanup_candidate(
    workspace_root: &Path,
    state: Option<&dyn crate::state::StateStore>,
    adapter: Option<&dyn crate::adapter::Adapter>,
    nessie: &dyn NessieClient,
    candidate: &str,
    environment: Option<&EnvironmentSetup>,
) -> Result<(), String> {
    let mut failures = Vec::new();
    let drop_catalog = environment
        .filter(|setup| setup.owns_catalog())
        .map(|setup| setup.catalog.clone());
    match (drop_catalog, adapter) {
        (Some(catalog), Some(adapter)) => {
            if let Err(error) = adapter.drop_catalog(&catalog).await {
                failures.push(format!("drop catalog `{catalog}`: {error}"));
            }
        }
        (Some(catalog), None) => {
            failures.push(format!("no adapter configured to drop catalog `{catalog}`"));
        }
        (None, _) => {}
    }
    match nessie.delete_branch(candidate).await {
        // An already-deleted branch is the goal state, not a failure: a
        // retry after a cleanup that failed at the last step must still
        // reach the evidence removal below, or the record is orphaned.
        Err(NessieError::NotFound(_)) => {}
        Err(error) => failures.push(format!("delete branch `{candidate}`: {error}")),
        Ok(()) => {}
    }
    if failures.is_empty() {
        // The branch and catalog are gone; the provisioning record is stale.
        if let Err(error) =
            crate::audit::remove_environment_artifacts(workspace_root, state, candidate)
        {
            failures.push(format!("remove environment evidence: {error}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

/// Bind a passed run to its environment's post-run Nessie head — the
/// commit its writes produced, not the pre-run snapshot. An unbound run
/// cannot later promote. The run's own environment label is used, so
/// `resume`/`retry_failed` bind the environment the original run targeted.
/// A no-op when the run did not pass, has no environment, or the
/// environment ref no longer exists.
pub async fn bind_run_reference(
    nessie: &dyn NessieClient,
    state: &dyn crate::state::StateStore,
    result: &crate::run::RunResult,
) -> Result<(), EngineError> {
    if result.status != crate::events::ExecutionStatus::Passed {
        return Ok(());
    }
    let Some(environment) = result.environment.as_deref() else {
        return Ok(());
    };
    let Some(head) = nessie
        .get_reference(environment)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
    else {
        return Ok(());
    };
    state
        .bind_run_reference_hash(&result.run_id, &head.hash)
        .map_err(|error| EngineError::Promotion(error.to_string()))
}

/// Resolve a run id or unique prefix to its summary — the shared lookup
/// behind `state show`, daemon `resume`/`retry_failed`, and anywhere else
/// a user supplies a partial id.
pub fn find_unique_run(
    state: &dyn crate::state::StateStore,
    id_or_prefix: &str,
) -> Result<RunSummary, EngineError> {
    let matches = state
        .find_runs(id_or_prefix)
        .map_err(|error| EngineError::State(error.to_string()))?;
    match matches.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err(EngineError::NotFound(format!(
            "no run matches `{id_or_prefix}`"
        ))),
        _ => Err(EngineError::Ambiguous(format!(
            "`{id_or_prefix}` matches {} runs — give a longer prefix",
            matches.len()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phlo_transform_nessie::{InMemoryNessie, ReferenceInfo};

    fn request(dry_run: bool) -> PromotionRequest {
        PromotionRequest {
            candidate_ref: "ci/pr-1".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: None,
            expected_target_hash: Some("aaa".to_string()),
            plan_id: Some("plan-1".to_string()),
            run_id: Some("run-1".to_string()),
            quality_gates_passed: true,
            diff_passed: None,
            require_diff: false,
            breaking_schema_changes: Vec::new(),
            allow_breaking_schema: false,
            dry_run,
            actor: None,
            gates: Vec::new(),
        }
    }

    #[tokio::test]
    async fn promotes_a_clean_candidate() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();

        let record = promote(&nessie, &request(false)).await.unwrap();
        assert!(record.merged);
        assert_eq!(record.target_hash_before, "aaa");
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "bbb"
        );
    }

    #[tokio::test]
    async fn blocks_when_quality_gates_failed() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        let mut request = request(false);
        request.quality_gates_passed = false;
        assert!(promote(&nessie, &request).await.is_err());
    }

    #[tokio::test]
    async fn blocks_when_target_advanced() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "zzz");
        nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let error = promote(&nessie, &request(false)).await.unwrap_err();
        assert!(error.to_string().contains("advanced"));
    }

    #[tokio::test]
    async fn blocks_when_candidate_advanced_since_audit() {
        // The audit pinned the candidate at `bbb`; a racing commit moved the
        // branch to `ccc` before the merge — promotion must refuse rather
        // than merge unaudited work.
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let mut request = request(false);
        request.candidate_hash = Some("bbb".to_string());
        nessie.assign_reference("ci/pr-1", "ccc").await.unwrap();
        let error = promote(&nessie, &request).await.unwrap_err();
        assert!(error.to_string().contains("advanced"), "{error}");
        // The target must not have been merged.
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "aaa"
        );
    }

    #[tokio::test]
    async fn promotes_when_candidate_hash_still_matches() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let mut request = request(false);
        request.candidate_hash = Some("bbb".to_string());
        let record = promote(&nessie, &request).await.unwrap();
        assert!(record.merged);
        assert_eq!(record.candidate_hash.as_deref(), Some("bbb"));
    }

    #[tokio::test]
    async fn dry_run_reports_without_merging() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie
            .create_branch("ci/pr-1", &ReferenceInfo::branch("main", "aaa"))
            .await
            .unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let record = promote(&nessie, &request(true)).await.unwrap();
        assert!(!record.merged);
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "aaa"
        );
    }

    #[tokio::test]
    async fn a_missing_reference_is_a_typed_not_found() {
        // Callers key the not-found surface off the variant, not the text.
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");

        let missing_candidate = promote(&nessie, &request(false)).await.unwrap_err();
        assert!(
            matches!(missing_candidate, EngineError::NotFound(_)),
            "{missing_candidate}"
        );

        nessie.seed("ci/pr-1", "bbb");
        let mut request = request(false);
        request.target_ref = "ghost".to_string();
        let missing_target = promote(&nessie, &request).await.unwrap_err();
        assert!(
            matches!(missing_target, EngineError::NotFound(_)),
            "{missing_target}"
        );
    }
}

#[cfg(test)]
mod schema_gate_tests {
    use super::*;
    use phlo_transform_nessie::InMemoryNessie;

    #[tokio::test]
    async fn breaking_schema_changes_block_promotion() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie
            .create_branch(
                "ci/pr-1",
                &phlo_transform_nessie::ReferenceInfo::branch("main", "aaa"),
            )
            .await
            .unwrap();

        let make = |allow: bool| PromotionRequest {
            candidate_ref: "ci/pr-1".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: None,
            expected_target_hash: Some("aaa".to_string()),
            plan_id: None,
            run_id: None,
            quality_gates_passed: true,
            diff_passed: None,
            require_diff: false,
            breaking_schema_changes: vec!["legacy: removed".to_string()],
            allow_breaking_schema: allow,
            dry_run: true,
            actor: None,
            gates: Vec::new(),
        };

        assert!(promote(&nessie, &make(false)).await.is_err());
        assert!(promote(&nessie, &make(true)).await.is_ok());
    }
}
