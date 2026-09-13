//! Promotion audit evidence. The diff a promotion relies on is recorded as
//! workspace artifacts (`branch_diff.json`, plus `environment*.json`
//! provisioning records). A single-model `diff.json` is never promotion
//! evidence: it examined one model and cannot certify a branch's schema.
//! These helpers read that evidence and decide whether it authorises a
//! candidate -> target promotion — shared by the CLI's `promote` path and
//! the daemon's operations API so both apply the same strict rules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use phlo_transform_core::Compilation;

use crate::artifacts::{ArtifactWriter, EnvironmentArtifact, SCHEMA_VERSION};
use crate::branch_diff::{
    materialized_for_environment, seeds_for_environment, BranchDiffReport, DatasetKind,
};
use crate::contracts::{contract_diff, ContractSafety};
use crate::environment::EnvironmentSetup;
use crate::error::EngineError;
use crate::state::StateStore;

fn artifact_path(workspace_root: &Path, name: &str) -> PathBuf {
    ArtifactWriter::for_workspace(workspace_root)
        .directory()
        .join(name)
}

/// The single-slot `environment.json` provisioning record, if present.
pub fn read_environment(workspace_root: &Path) -> Option<EnvironmentSetup> {
    let text = std::fs::read_to_string(artifact_path(workspace_root, "environment.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("environment")?.clone()).ok()
}

/// The per-candidate environment artifact file: `environment_<ref>_<hash>.json`
/// with characters unsafe in a filename folded to `_`. Folding can collide
/// (`ci/pr-1` vs `ci_pr_1`) and a ref of nothing but unsafe characters
/// collapses to `environment` — the FNV-1a suffix keeps every ref's evidence
/// its own file, and stays stable across builds (unlike `DefaultHasher`).
pub fn environment_artifact_name(reference: &str) -> String {
    let mut name = String::from("environment_");
    let mut previous_underscore = true;
    for character in reference.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
            previous_underscore = false;
        } else if !previous_underscore {
            name.push('_');
            previous_underscore = true;
        }
    }
    let sanitized = name.trim_end_matches('_');
    let mut hash: u32 = 0x811c9dc5;
    for byte in reference.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    format!("{sanitized}_{hash:08x}.json")
}

/// Persist the provisioning record: the conventional `environment.json` for
/// the workspace's current environment, plus a per-candidate copy so one
/// candidate's evidence survives another being provisioned later.
pub fn write_environment_artifacts(
    workspace_root: &Path,
    setup: &EnvironmentSetup,
) -> Result<(), String> {
    ArtifactWriter::for_workspace(workspace_root)
        .write_environment(setup)
        .map_err(|error| error.to_string())?;
    let path = artifact_path(
        workspace_root,
        &environment_artifact_name(&setup.candidate.name),
    );
    let payload = serde_json::to_string_pretty(&EnvironmentArtifact {
        schema_version: SCHEMA_VERSION,
        environment: setup.clone(),
    })
    .map_err(|error| error.to_string())?;
    std::fs::write(path, payload).map_err(|error| error.to_string())
}

/// The recorded provisioning setup for a specific candidate: the
/// per-candidate artifact first, then the single-slot `environment.json`
/// (which only describes the most recently provisioned candidate).
pub fn read_environment_for(workspace_root: &Path, candidate: &str) -> Option<EnvironmentSetup> {
    let matches = |setup: &EnvironmentSetup| setup.candidate.name == candidate;
    let setup = std::fs::read_to_string(artifact_path(
        workspace_root,
        &environment_artifact_name(candidate),
    ))
    .ok()
    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
    .and_then(|value| serde_json::from_value(value.get("environment")?.clone()).ok());
    setup
        .filter(matches)
        .or_else(|| read_environment(workspace_root).filter(matches))
}

/// Drop a candidate's local provisioning evidence after its branch is gone.
pub fn remove_environment_artifacts(workspace_root: &Path, candidate: &str) {
    let _ = std::fs::remove_file(artifact_path(
        workspace_root,
        &environment_artifact_name(candidate),
    ));
    if read_environment(workspace_root).is_some_and(|setup| setup.candidate.name == candidate) {
        let _ = std::fs::remove_file(artifact_path(workspace_root, "environment.json"));
    }
}

/// The raw `diff.json` artifact — kept as a `Value` because the artifact
/// embeds the diff under `diff` alongside model/dataset metadata.
pub fn read_diff(workspace_root: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(artifact_path(workspace_root, "diff.json")).ok()?;
    serde_json::from_str(&text).ok()
}

/// The `branch_diff.json` artifact's report payload.
pub fn read_branch_diff(workspace_root: &Path) -> Option<BranchDiffReport> {
    let text = std::fs::read_to_string(artifact_path(workspace_root, "branch_diff.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    serde_json::from_value(value.get("diff")?.clone()).ok()
}

/// Breaking contract changes between the recorded base contracts and the
/// desired workspace contracts — computed live at promotion time, never from
/// a stored artifact, so an edit after `diff` cannot bypass the gate.
/// The recorded contracts are promotion evidence: a state-read failure
/// propagates rather than reading as "no contracts recorded".
pub fn contract_breaking_changes(
    state: Option<&dyn StateStore>,
    compilation: &Compilation,
    to: &str,
) -> Result<Vec<String>, EngineError> {
    let Some(state) = state else {
        return Ok(Vec::new());
    };
    let base = materialized_for_environment(state, to)?;
    let mut breaking = Vec::new();
    for model in &compilation.models {
        let name = model.id.logical_name();
        let record = base.get(&name);
        let mut changes = contract_diff(
            record.and_then(|record| record.contract.as_ref()),
            model.contract.as_ref(),
        );
        // The effective key — incremental `key` columns and unique-assertion
        // columns collapse onto the same identity concept — is compared
        // against the key the target's materialisation persisted. Changing
        // or dropping it is breaking; a record that cannot prove its
        // historical key fails closed.
        if let Some(change) = crate::contracts::key_change(
            &record
                .map(|record| record.recorded_key())
                .unwrap_or(crate::contracts::RecordedKey::Known(None)),
            crate::contracts::effective_key(model).as_deref(),
        ) {
            changes.push(change);
        }
        for change in changes {
            if change.safety == ContractSafety::Breaking {
                let subject = if change.column.is_empty() {
                    name.clone()
                } else {
                    format!("{name}.{}", change.column)
                };
                breaking.push(format!("{subject}: contract {}", change.detail));
            }
        }
    }
    Ok(breaking)
}

/// What the persisted diff artifacts prove for a promotion.
#[derive(Clone, Debug, Default)]
pub struct AuditEvidence {
    /// The audited diff's verdict, when an applicable artifact was inspected.
    pub diff_passed: Option<bool>,
    /// Why the artifact cannot stand as evidence, when rejected.
    pub diff_rejected: Option<String>,
    /// Breaking schema changes the audit recorded.
    pub breaking_schema_changes: Vec<String>,
    /// The base commit the artifact audited — a second provenance source for
    /// the `base` gate when branch-cut provenance is unavailable.
    pub audited_base_hash: Option<String>,
    /// A fresh, ref-and-commit-bound audit actually inspected this pair.
    /// `false` means "no evidence", which must never read as "no changes".
    pub schema_audited: bool,
}

/// Read the audited diff artifact (`branch_diff.json` — a single-model
/// `diff.json` is never promotion evidence) and derive what it proves for this
/// promotion: the diff verdict, why the artifact cannot be used, the breaking
/// schema changes it recorded, and whether a schema audit genuinely ran.
///
/// The artifact only counts when it was produced for this candidate against
/// this target at the commits being promoted — a diff of another pair, of an
/// older head, or one that went stale since is rejected with a reason naming
/// the rerun. `candidate_hash`/`base_hash` are the refs' current heads; a
/// hash-bound artifact must match them exactly.
pub fn audited_diff(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    candidate: &str,
    to: &str,
    candidate_hash: Option<&str>,
    base_hash: Option<&str>,
) -> AuditEvidence {
    let Some(state) = state else {
        return AuditEvidence::default();
    };
    if let Some(report) = read_branch_diff(workspace_root) {
        // An audit of another candidate, or against another target, is not
        // evidence for this promotion.
        if report.candidate_ref != candidate || report.base_ref != to {
            return AuditEvidence {
                diff_rejected: Some(format!(
                    "branch diff covers `{}` -> `{}`, not `{candidate}` -> `{to}`; \
                     rerun `diff --from {candidate} --to {to} --full`",
                    report.candidate_ref, report.base_ref
                )),
                ..AuditEvidence::default()
            };
        }
        let mut rejected = None;
        let mut reject = |reason: String| {
            rejected.get_or_insert(reason);
        };
        // Whether the artifact inspected this pair at these commits and still
        // applies — binding and freshness failures revoke the schema audit;
        // shallowness does not (the schema pass ran either way).
        let mut fresh = true;
        let mut stale = |reason: String| {
            fresh = false;
            reject(reason);
        };
        let mut breaking = Vec::new();
        for change in &report.schema_changes {
            for item in &change.changes {
                if matches!(item.safety.as_str(), "error" | "full_rebuild_required") {
                    breaking.push(format!("{}.{}: {}", change.model, item.column, item.detail));
                }
            }
        }
        // Commit binding: the artifact must name the exact heads being
        // promoted. An unbound artifact cannot prove what it audited.
        for (label, recorded, expected) in [
            (
                "candidate",
                report.candidate_hash.as_deref(),
                candidate_hash,
            ),
            ("base", report.base_hash.as_deref(), base_hash),
        ] {
            let Some(expected) = expected else { continue };
            match recorded {
                Some(recorded) if recorded == expected => {}
                Some(recorded) => stale(format!(
                    "branch diff audited {label}@{recorded}, not current {label}@{expected}; \
                     rerun `diff --from {candidate} --to {to} --full`"
                )),
                None => stale(format!(
                    "branch diff does not record the {label} commit it audited; \
                     rerun `diff --from {candidate} --to {to} --full`"
                )),
            }
        }
        // The artifact is stale when a dataset's recorded version no longer
        // matches the current materialisation — on either side — or when a
        // dataset that had no materialisation at diff time has one now (it
        // stopped being `removed`/`absent` since the audit). `main` folds in
        // the default environment, matching how the diff was produced.
        // State reads are promotion evidence: a store error cannot masquerade
        // as "nothing recorded" — the artifact is rejected rather than
        // trusted against an empty map.
        let (candidate_models, candidate_seeds) = match (
            materialized_for_environment(state, candidate),
            seeds_for_environment(state, candidate),
        ) {
            (Ok(models), Ok(seeds)) => (models, seeds),
            (Err(error), _) | (_, Err(error)) => {
                stale(format!("cannot confirm the diff is current: {error}"));
                (BTreeMap::new(), BTreeMap::new())
            }
        };
        let (base_models, base_seeds) = match (
            materialized_for_environment(state, to),
            seeds_for_environment(state, to),
        ) {
            (Ok(models), Ok(seeds)) => (models, seeds),
            (Err(error), _) | (_, Err(error)) => {
                stale(format!("cannot confirm the diff is current: {error}"));
                (BTreeMap::new(), BTreeMap::new())
            }
        };
        for dataset in &report.datasets {
            let (current_candidate, current_base) = match dataset.kind {
                DatasetKind::Model => (
                    candidate_models
                        .get(&dataset.dataset)
                        .map(|record| record.version.hash.clone()),
                    base_models
                        .get(&dataset.dataset)
                        .map(|record| record.version.hash.clone()),
                ),
                DatasetKind::Seed => (
                    candidate_seeds
                        .get(&dataset.dataset)
                        .map(|record| record.content_hash.clone()),
                    base_seeds
                        .get(&dataset.dataset)
                        .map(|record| record.content_hash.clone()),
                ),
            };
            if current_candidate != dataset.candidate_version {
                stale(format!(
                    "branch diff is stale: `{}` changed on the candidate since the diff",
                    dataset.dataset
                ));
            }
            if current_base != dataset.base_version {
                stale(format!(
                    "branch diff is stale: `{}` changed on `{to}` since the diff",
                    dataset.dataset
                ));
            }
        }
        // A dataset materialised after the diff was never compared — the
        // report cannot speak for it.
        let covered: BTreeSet<(&str, DatasetKind)> = report
            .datasets
            .iter()
            .map(|dataset| (dataset.dataset.as_str(), dataset.kind))
            .collect();
        for (name, kind) in candidate_models
            .keys()
            .map(|name| (name, DatasetKind::Model))
            .chain(candidate_seeds.keys().map(|name| (name, DatasetKind::Seed)))
        {
            if !covered.contains(&(name.as_str(), kind)) {
                stale(format!(
                    "branch diff is stale: `{name}` materialised on the candidate after the diff"
                ));
            }
        }
        for (name, kind) in base_models
            .keys()
            .map(|name| (name, DatasetKind::Model))
            .chain(base_seeds.keys().map(|name| (name, DatasetKind::Seed)))
        {
            if !covered.contains(&(name.as_str(), kind)) {
                stale(format!(
                    "branch diff is stale: `{name}` materialised on `{to}` after the diff"
                ));
            }
        }
        // A diff entry that compared a relation to itself measured nothing —
        // it cannot back a required audit.
        if report
            .diffs
            .iter()
            .any(|diff| diff.candidate_relation == diff.base_relation)
        {
            stale(
                "branch diff compared a relation to itself; rerun against distinct \
                 candidate and base relations"
                    .to_string(),
            );
        }
        // A shallow diff compared schema and row counts only — no data-diff
        // policies were evaluated, so it carries no verdict and cannot
        // satisfy a required audit. (`diffs` non-empty also proves `--full`,
        // for pre-`deep`-field artifacts.) The schema audit it did run still
        // stands.
        let shallow = !report.deep && report.diffs.is_empty();
        if shallow {
            reject(format!(
                "branch diff ran without `--full`; rerun \
                 `diff --from {candidate} --to {to} --full` for a value-level audit"
            ));
        }
        return AuditEvidence {
            diff_passed: (!shallow).then_some(report.passed),
            diff_rejected: rejected,
            breaking_schema_changes: breaking,
            audited_base_hash: report.base_hash.clone(),
            schema_audited: fresh,
        };
    }

    // The single-model `diff.json` is not promotion evidence: it examined
    // one model, so it cannot certify a branch's schema. Its presence means
    // someone audited a model, not the branch — say so rather than a bare
    // "no evidence".
    if read_diff(workspace_root).is_some() {
        return AuditEvidence {
            diff_rejected: Some(format!(
                "a single-model diff cannot audit a branch; \
                 run `diff --from {candidate} --to {to} --full`"
            )),
            ..AuditEvidence::default()
        };
    }
    AuditEvidence::default()
}

/// The catalog the compiled models target (first explicit model catalog) —
/// the base side of a branch diff resolves `main` through it.
pub fn compiled_catalog(compilation: &Compilation) -> Option<String> {
    compilation
        .models
        .iter()
        .find_map(|model| model.target.catalog.clone())
}

/// The lineage-diff artifact's standing as evidence for a promotion.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LineageEvidence {
    /// `current` — the artifact audited this Nessie pair at these commits;
    /// `advisory` — only Git-bound, so no environment identity to check;
    /// `stale` — the identity it was produced for has moved and it cannot
    /// be treated as describing the candidate being promoted.
    pub status: &'static str,
    /// Why the artifact is not current evidence, when stale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The base the diff was produced against (`ref (commit)`).
    pub base: String,
    /// Total lineage changes the artifact reports.
    pub changes: usize,
}

/// Read `lineage_diff.json` and audit its provenance against the pair
/// being promoted. The artifact only counts when it was produced for this
/// candidate against this target — bound to the Nessie commits when it
/// carries an environment binding, else to the Git identity it recorded —
/// and only while its candidate fingerprint still matches the compiled
/// workspace. Anything stale, unreadable, or about another pair is
/// rejected with a reason naming the rerun rather than silently treated
/// as current.
pub fn audited_lineage(
    workspace_root: &Path,
    candidate: &str,
    to: &str,
    candidate_hash: &str,
    target_hash: &str,
    current_lineage_hash: Option<&str>,
) -> Option<LineageEvidence> {
    let text = std::fs::read_to_string(artifact_path(workspace_root, "lineage_diff.json")).ok()?;
    let artifact = match serde_json::from_str::<crate::artifacts::LineageDiffArtifact>(&text) {
        Ok(artifact) => artifact,
        Err(error) => {
            return Some(LineageEvidence {
                status: "stale",
                reason: Some(format!("unreadable lineage artifact: {error}")),
                base: "unknown".to_string(),
                changes: 0,
            });
        }
    };
    let base = format!(
        "{} ({})",
        artifact.base_ref,
        artifact.base_commit.chars().take(12).collect::<String>()
    );
    let changes = artifact.diff.change_count();
    let stale = |reason: String| LineageEvidence {
        status: "stale",
        reason: Some(reason),
        base: base.clone(),
        changes,
    };
    let evidence = |status: &'static str| LineageEvidence {
        status,
        reason: None,
        base: base.clone(),
        changes,
    };
    let rerun = format!("rerun `lineage --diff {to} --ref {candidate}`");
    let verdict = match &artifact.environment {
        // Nessie-bound: the pair and both commits must match the promotion.
        Some(binding) => {
            if binding.candidate_ref != candidate || binding.target_ref != to {
                stale(format!(
                    "lineage diff covers `{}` -> `{}`, not `{candidate}` -> `{to}`; {rerun}",
                    binding.candidate_ref, binding.target_ref
                ))
            } else if binding.candidate_hash != candidate_hash {
                stale(format!(
                    "candidate `{candidate}` moved since the lineage diff; {rerun}"
                ))
            } else if binding.target_hash != target_hash {
                stale(format!(
                    "target `{to}` moved since the lineage diff; {rerun}"
                ))
            } else {
                evidence("current")
            }
        }
        // Git-bound only: the recorded candidate identity must still hold.
        None => match artifact.base_kind.as_str() {
            "merge-base" => {
                match phlo_transform_core::comparison_base(workspace_root, &artifact.base_ref) {
                    Ok(current)
                        if current.commit == artifact.base_commit
                            && current.head == artifact.candidate.head
                            && current.dirty == artifact.candidate.dirty =>
                    {
                        evidence("advisory")
                    }
                    Ok(_) => stale(format!(
                        "the worktree or `{}` moved since the lineage diff; {rerun}",
                        artifact.base_ref
                    )),
                    Err(_) => stale(format!(
                        "base ref `{}` no longer resolves; {rerun}",
                        artifact.base_ref
                    )),
                }
            }
            // Exact ref -> ref: both refs must still resolve to the commits
            // the diff was produced from — a deleted or unrecorded ref is
            // unverifiable, not "unchanged".
            _ => {
                let base_now =
                    match phlo_transform_core::resolve_commit(workspace_root, &artifact.base_ref) {
                        Ok(commit) => commit,
                        Err(_) => {
                            return Some(stale(format!(
                                "base ref `{}` no longer resolves; {rerun}",
                                artifact.base_ref
                            )))
                        }
                    };
                let candidate_now = match artifact.candidate.git_ref.as_deref() {
                    Some(git_ref) => {
                        match phlo_transform_core::resolve_commit(workspace_root, git_ref) {
                            Ok(commit) => Some(commit),
                            Err(_) => {
                                return Some(stale(format!(
                                    "candidate ref `{git_ref}` no longer resolves; {rerun}"
                                )))
                            }
                        }
                    }
                    None => None,
                };
                match (candidate_now, artifact.candidate.head.as_deref()) {
                    (Some(now), Some(recorded))
                        if now == recorded && base_now == artifact.base_commit =>
                    {
                        evidence("advisory")
                    }
                    (Some(_), Some(_)) => stale(format!(
                        "a diffed ref moved since the lineage diff; {rerun}"
                    )),
                    _ => stale(format!(
                        "the artifact does not record a resolvable candidate ref; {rerun}"
                    )),
                }
            }
        },
    };
    // Whichever identity check passed, the artifact must also describe the
    // candidate's current definitions: refs can sit still while edited
    // code waits unpromoted, and a dirty worktree stays "dirty" as its
    // contents change.
    if verdict.status == "stale" {
        return Some(verdict);
    }
    Some(
        match (&artifact.candidate.lineage_hash, current_lineage_hash) {
            (Some(recorded), Some(now)) if recorded == now => verdict,
            (Some(_), Some(_)) => stale(format!(
                "the candidate's lineage changed since the diff; {rerun}"
            )),
            (Some(_), None) => stale(format!(
                "the candidate's lineage could not be fingerprinted; {rerun}"
            )),
            (None, _) => stale(format!(
                "the lineage artifact predates candidate fingerprinting; {rerun}"
            )),
        },
    )
}
