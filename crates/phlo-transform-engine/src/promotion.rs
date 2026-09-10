//! Write-Audit-Publish promotion.
//!
//! A candidate environment (a Nessie branch) is written and audited, then
//! promoted to a target reference. Promotion validates quality gates and
//! target staleness before merging, and records a reproducible artifact.

use serde::Serialize;

use phlo_transform_nessie::{Conflict, NessieClient};

use crate::error::EngineError;
use crate::util::now_rfc3339;

/// A promotion request.
#[derive(Clone, Debug)]
pub struct PromotionRequest {
    pub candidate_ref: String,
    pub target_ref: String,
    /// Candidate hash that was audited, when known.
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
    /// Only check preconditions; do not merge.
    pub dry_run: bool,
    pub actor: Option<String>,
}

/// A reproducible promotion record.
#[derive(Clone, Debug, Serialize)]
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
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

    let candidate = nessie
        .get_reference(&request.candidate_ref)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
        .ok_or_else(|| {
            EngineError::Promotion(format!(
                "candidate reference `{}` was not found",
                request.candidate_ref
            ))
        })?;

    let target = nessie
        .get_reference(&request.target_ref)
        .await
        .map_err(|error| EngineError::Promotion(error.to_string()))?
        .ok_or_else(|| {
            EngineError::Promotion(format!(
                "target reference `{}` was not found",
                request.target_ref
            ))
        })?;

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
                &request.target_ref,
                request.expected_target_hash.as_deref(),
            )
            .await
            .map_err(|error| EngineError::Promotion(error.to_string()))?
    };

    if !merge.is_clean() {
        record.conflicts = merge.conflicts;
        return Err(EngineError::Promotion(format!(
            "candidate cannot be promoted: {} conflict(s)",
            record.conflicts.len()
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

#[cfg(test)]
mod tests {
    use super::*;
    use phlo_transform_nessie::InMemoryNessie;

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
            dry_run,
            actor: None,
        }
    }

    #[tokio::test]
    async fn promotes_a_clean_candidate() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie.create_branch("ci/pr-1", Some("aaa")).await.unwrap();
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
        nessie.create_branch("ci/pr-1", Some("aaa")).await.unwrap();
        let mut request = request(false);
        request.quality_gates_passed = false;
        assert!(promote(&nessie, &request).await.is_err());
    }

    #[tokio::test]
    async fn blocks_when_target_advanced() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "zzz");
        nessie.create_branch("ci/pr-1", Some("aaa")).await.unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let error = promote(&nessie, &request(false)).await.unwrap_err();
        assert!(error.to_string().contains("advanced"));
    }

    #[tokio::test]
    async fn dry_run_reports_without_merging() {
        let nessie = InMemoryNessie::new();
        nessie.seed("main", "aaa");
        nessie.create_branch("ci/pr-1", Some("aaa")).await.unwrap();
        nessie.assign_reference("ci/pr-1", "bbb").await.unwrap();
        let record = promote(&nessie, &request(true)).await.unwrap();
        assert!(!record.merged);
        assert_eq!(
            nessie.get_reference("main").await.unwrap().unwrap().hash,
            "aaa"
        );
    }
}
