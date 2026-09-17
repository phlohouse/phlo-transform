//! Promotion audit evidence. The evidence a promotion relies on is
//! persisted in the state store as immutable [`EvidenceRecord`]s — so a
//! shared Postgres backend carries it across CI stages and machines — and
//! exported to workspace artifacts (`branch_diff.json`,
//! `lineage_diff.json`, `environment*.json`) for inspection and debugging.
//! A single-model `diff.json` is never promotion evidence: it examined one
//! model and cannot certify a branch's schema.
//!
//! Reads are store-first. The artifact files remain the compatibility
//! path: a workspace that only has files keeps working, and file evidence
//! a store has not seen is imported so it becomes portable from then on.
//! These helpers decide whether the evidence authorises a candidate ->
//! target promotion — shared by the CLI's `promote` path and the daemon's
//! operations API so both apply the same strict rules.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use phlo_transform_core::Compilation;

use crate::artifacts::{ArtifactWriter, EnvironmentArtifact, LineageDiffArtifact, SCHEMA_VERSION};
use crate::branch_diff::{
    materialized_for_environment, seeds_for_environment, BranchDiffReport, DatasetKind,
};
use crate::contracts::{contract_diff, ContractSafety};
use crate::environment::EnvironmentSetup;
use crate::error::EngineError;
use crate::state::{EvidenceKind, EvidenceRecord, StateStore};
use crate::util::{cmp_rfc3339, now_rfc3339};

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

/// The evidence record an `EnvironmentSetup` maps to. The record's commit
/// columns carry the candidate head and the cut-from base when recorded.
fn environment_record(setup: &EnvironmentSetup) -> Result<EvidenceRecord, String> {
    Ok(EvidenceRecord {
        evidence_id: uuid::Uuid::new_v4().to_string(),
        kind: EvidenceKind::Environment,
        subject: setup.candidate.name.clone(),
        target_ref: setup
            .created_from
            .as_ref()
            .map(|base| base.name.clone())
            .unwrap_or_default(),
        candidate_hash: Some(setup.candidate.hash.clone()),
        target_hash: setup.created_from.as_ref().map(|base| base.hash.clone()),
        fingerprint: None,
        payload: serde_json::to_value(setup).map_err(|error| error.to_string())?,
        run_id: None,
        created_at: now_rfc3339(),
    })
}

/// The evidence record a `BranchDiffReport` maps to. `created_at` is the
/// report's own finish time so a file import and a live write order
/// identically — the report's clock is what the evidence attests.
fn branch_diff_record(report: &BranchDiffReport) -> Result<EvidenceRecord, String> {
    Ok(EvidenceRecord {
        evidence_id: uuid::Uuid::new_v4().to_string(),
        kind: EvidenceKind::BranchDiff,
        subject: report.candidate_ref.clone(),
        target_ref: report.base_ref.clone(),
        candidate_hash: report.candidate_hash.clone(),
        target_hash: report.base_hash.clone(),
        fingerprint: None,
        payload: serde_json::to_value(report).map_err(|error| error.to_string())?,
        run_id: None,
        created_at: report.finished_at.clone(),
    })
}

/// The evidence record a `LineageDiffArtifact` maps to. The subject is the
/// environment binding's candidate ref when bound — the identity promotion
/// matches — else the candidate's Git ref.
pub(crate) fn lineage_diff_record(
    artifact: &LineageDiffArtifact,
) -> Result<EvidenceRecord, String> {
    Ok(EvidenceRecord {
        evidence_id: uuid::Uuid::new_v4().to_string(),
        kind: EvidenceKind::LineageDiff,
        subject: artifact
            .environment
            .as_ref()
            .map(|binding| binding.candidate_ref.clone())
            .or_else(|| artifact.candidate.git_ref.clone())
            .unwrap_or_default(),
        target_ref: artifact
            .environment
            .as_ref()
            .map(|binding| binding.target_ref.clone())
            .unwrap_or_else(|| artifact.base_ref.clone()),
        candidate_hash: artifact
            .environment
            .as_ref()
            .map(|binding| binding.candidate_hash.clone())
            .or_else(|| artifact.candidate.head.clone()),
        target_hash: artifact
            .environment
            .as_ref()
            .map(|binding| binding.target_hash.clone())
            .or_else(|| Some(artifact.base_commit.clone())),
        fingerprint: artifact.candidate.lineage_hash.clone(),
        payload: serde_json::to_value(artifact).map_err(|error| error.to_string())?,
        run_id: None,
        created_at: artifact.created_at.clone().unwrap_or_else(now_rfc3339),
    })
}

/// The candidate ref a lineage-diff artifact speaks for — its Nessie
/// environment binding's candidate when bound, else the Git ref recorded
/// at diff time. The same identity [`lineage_diff_record`] persists the
/// evidence under.
fn lineage_diff_subject(artifact: &LineageDiffArtifact) -> Option<&str> {
    artifact
        .environment
        .as_ref()
        .map(|binding| binding.candidate_ref.as_str())
        .or(artifact.candidate.git_ref.as_deref())
}

/// The target ref a lineage-diff artifact speaks for — its Nessie
/// binding's target when bound, else the Git base ref it diffed against.
/// The same identity [`lineage_diff_record`] persists under `target_ref`.
fn lineage_diff_target(artifact: &LineageDiffArtifact) -> &str {
    artifact
        .environment
        .as_ref()
        .map(|binding| binding.target_ref.as_str())
        .unwrap_or(artifact.base_ref.as_str())
}

/// Append file-derived evidence the store does not already carry. A read
/// that reaches an artifact naming a *different* subject would otherwise
/// re-import it on every call — the (subject, target, instant) match
/// keeps repeated audits from piling up duplicate rows. Identical
/// payloads deduplicate too: artifacts without a `created_at` (environment
/// setups, pre-timestamp lineage diffs) are recorded at import time, so
/// the timestamp alone cannot anchor them — without the payload check
/// every audit would append another copy.
///
/// Fail-closed: with a store configured the store is the authority, so
/// evidence that cannot be persisted may not audit at all — the caller
/// rejects the artifact rather than pass gates on a local file the
/// authoritative record knows nothing about. File-only compatibility is
/// for workspaces with no store configured.
///
/// Returns the id of the *standing* record for this evidence — the
/// already-persisted row a dedup matched, else the row just written — so a
/// caller can name exactly which record the audit consulted without a
/// second, raceable store read.
fn import_evidence(
    state: &dyn StateStore,
    record: &EvidenceRecord,
) -> Result<Option<String>, EngineError> {
    let standing = state
        .evidence_for(record.kind, &record.subject)
        .map_err(|error| EngineError::State(format!("cannot read the evidence store: {error}")))?
        .into_iter()
        .find(|existing| {
            existing.target_ref == record.target_ref
                && (existing.created_at == record.created_at || existing.payload == record.payload)
        });
    if let Some(existing) = standing {
        return Ok(Some(existing.evidence_id));
    }
    let evidence_id = record.evidence_id.clone();
    state.record_evidence(record).map_err(|error| {
        EngineError::State(format!(
            "cannot persist {} evidence for `{}` to the state store: {error}",
            record.kind.as_str(),
            record.subject
        ))
    })?;
    Ok(Some(evidence_id))
}

/// Persist a branch-diff audit: the immutable evidence record (portable —
/// a shared store carries it to whichever stage promotes) plus the
/// `branch_diff.json` export. Shared by the CLI's `diff` and the daemon's
/// branch-diff endpoint.
pub fn persist_branch_diff(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    report: &BranchDiffReport,
) -> Result<(), EngineError> {
    ArtifactWriter::for_workspace(workspace_root)
        .write_branch_diff(report)
        .map_err(|error| EngineError::Artifact(error.to_string()))?;
    if let Some(state) = state {
        let record = branch_diff_record(report).map_err(EngineError::Artifact)?;
        state
            .record_evidence(&record)
            .map_err(|error| EngineError::State(error.to_string()))?;
    }
    Ok(())
}

/// Persist a lineage-diff audit through the store, when one is configured.
/// The artifact file is written by the caller (`ArtifactWriter`); this
/// records the portable record.
pub(crate) fn persist_lineage_evidence(
    state: Option<&dyn StateStore>,
    artifact: &LineageDiffArtifact,
) -> Result<(), EngineError> {
    let Some(state) = state else {
        return Ok(());
    };
    // An artifact with no subject — neither a Nessie binding nor a Git ref
    // — has nothing to bind the evidence to. The import path already skips
    // such artifacts; the write path does too, rather than persist a
    // `subject=""` row no lookup can reach.
    if lineage_diff_subject(artifact).is_none() {
        return Ok(());
    }
    let record = lineage_diff_record(artifact).map_err(EngineError::Artifact)?;
    state.record_evidence(&record)
}

/// Persist the provisioning record: the immutable evidence record in the
/// store first (the portable authority), then the conventional
/// `environment.json` for the workspace's current environment and a
/// per-candidate copy so one candidate's evidence survives another being
/// provisioned later. The record is written before the files because a
/// read prefers store evidence: files written first could be masked by an
/// older record if the record write then failed, while a persisted record
/// with missing files stays consistent — the store wins either way. An
/// unchanged binding is not re-recorded: identical rows carry no
/// information, and an `ensure` runs on every `run --ref`/`apply --ref`.
pub fn write_environment_artifacts(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    setup: &EnvironmentSetup,
) -> Result<(), String> {
    if let Some(state) = state {
        let record = environment_record(setup)?;
        // The newest record is the live binding; an identical payload is
        // already what every read resolves, so recording it again adds
        // nothing. A changed binding (or changed base) appends a record
        // that becomes the new live binding.
        let unchanged = state
            .evidence_for(EvidenceKind::Environment, &record.subject)
            .map_err(|error| error.to_string())?
            .first()
            .is_some_and(|existing| {
                existing.target_ref == record.target_ref && existing.payload == record.payload
            });
        if !unchanged {
            state
                .record_evidence(&record)
                .map_err(|error| error.to_string())?;
        }
    }
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
    std::fs::write(path, payload).map_err(|error| error.to_string())?;
    Ok(())
}

/// The recorded provisioning setup for a specific candidate: the evidence
/// store first (the portable authority), then the artifact files — the
/// per-candidate artifact, then the single-slot `environment.json` (which
/// only describes the most recently provisioned candidate). File evidence
/// the store lacks is imported, so a workspace that predates the evidence
/// table keeps working and becomes portable. A store read failure is an
/// error, not a cache miss: falling back to local files could resolve a
/// different binding than the store recorded for another machine.
pub fn read_environment_for(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    candidate: &str,
) -> Result<Option<EnvironmentSetup>, EngineError> {
    Ok(read_environment_evidence(workspace_root, state, candidate)?.0)
}

/// The same resolution, also returning the id of the evidence record the
/// setup was read from — the store's newest record, or the row a file
/// import just stood up. `None` for file-only workspaces (no store) and
/// for files that could not be recorded. A caller recording provenance
/// uses this rather than re-querying: the id travels with the read that
/// consulted it.
pub fn read_environment_evidence(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    candidate: &str,
) -> Result<(Option<EnvironmentSetup>, Option<String>), EngineError> {
    if let Some(state) = state {
        let records = state
            .evidence_for(EvidenceKind::Environment, candidate)
            .map_err(|error| {
                EngineError::State(format!("cannot read the evidence store: {error}"))
            })?;
        if let Some(record) = records.first() {
            // Only the newest record speaks; an undecodable one fails
            // closed — silently skipping it could resurrect a stale
            // binding an older record superseded.
            let setup: EnvironmentSetup =
                serde_json::from_value(record.payload.clone()).map_err(|error| {
                    EngineError::State(format!("undecodable environment evidence: {error}"))
                })?;
            // The subject column is the indexed identity; a payload naming
            // a different candidate is contradictory evidence — fail closed
            // rather than audit a binding for another environment.
            if setup.candidate.name != candidate {
                return Err(EngineError::State(format!(
                    "environment evidence recorded for `{candidate}` decodes to a binding for `{}`",
                    setup.candidate.name
                )));
            }
            return Ok((Some(setup), Some(record.evidence_id.clone())));
        }
        let setup = read_environment_files(workspace_root, candidate);
        let mut evidence_id = None;
        if let Some(setup) = &setup {
            if let Ok(record) = environment_record(setup) {
                // The file cannot resolve the binding unless it reaches
                // the store — a local-only artifact would audit here while
                // other machines sharing the store see nothing.
                evidence_id = import_evidence(state, &record)?;
            }
        }
        return Ok((setup, evidence_id));
    }
    Ok((read_environment_files(workspace_root, candidate), None))
}

/// The artifact-file environment lookup — the compatibility path.
fn read_environment_files(workspace_root: &Path, candidate: &str) -> Option<EnvironmentSetup> {
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

/// Every recorded environment binding: the store's latest record per
/// candidate, plus artifact files for candidates the store does not know
/// (imported so they become portable), deduplicated by candidate name.
/// Used to detect one physical catalog claimed by two refs.
pub fn environment_artifacts(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
) -> Result<Vec<EnvironmentSetup>, EngineError> {
    let mut setups: Vec<EnvironmentSetup> = Vec::new();
    if let Some(state) = state {
        let records = state
            .latest_evidence(EvidenceKind::Environment)
            .map_err(|error| {
                EngineError::State(format!("cannot read the evidence store: {error}"))
            })?;
        for record in records {
            // The catalog-claim check needs every binding — an undecodable
            // record could hide the claim being checked for.
            let setup: EnvironmentSetup =
                serde_json::from_value(record.payload.clone()).map_err(|error| {
                    EngineError::State(format!("undecodable environment evidence: {error}"))
                })?;
            // `latest_evidence` keeps the newest record per (subject,
            // target) pair, so a candidate re-provisioned from a different
            // base can appear several times. The list is newest-first:
            // only a candidate's newest record is its live binding — an
            // older, superseded one must not keep claiming its catalog.
            if setups
                .iter()
                .any(|seen| seen.candidate.name == setup.candidate.name)
            {
                continue;
            }
            setups.push(setup);
        }
    }
    let directory = artifact_path(workspace_root, "");
    let parse = |text: String| -> Option<EnvironmentSetup> {
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        serde_json::from_value(value.get("environment")?.clone()).ok()
    };
    if let Ok(entries) = std::fs::read_dir(&directory) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.starts_with("environment") || !name.ends_with(".json") {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Some(setup) = parse(text) {
                    if setups
                        .iter()
                        .any(|seen: &EnvironmentSetup| seen.candidate.name == setup.candidate.name)
                    {
                        continue;
                    }
                    if let (Some(state), Ok(record)) = (state, environment_record(&setup)) {
                        import_evidence(state, &record)?;
                    }
                    setups.push(setup);
                }
            }
        }
    }
    Ok(setups)
}

/// Drop a candidate's provisioning evidence after its branch is gone —
/// the local artifacts and the store records alike. Removal failures are
/// reported on both sides: a leftover store record would let a stale
/// binding claim the catalog for a ref that no longer exists, and a
/// leftover `environment_*.json` re-imports that binding on the next
/// artifact scan. The files go first so a failure leaves file and record
/// consistent; a missing file is already the goal state.
pub fn remove_environment_artifacts(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    candidate: &str,
) -> Result<(), EngineError> {
    remove_artifact_file(&artifact_path(
        workspace_root,
        &environment_artifact_name(candidate),
    ))?;
    if read_environment(workspace_root).is_some_and(|setup| setup.candidate.name == candidate) {
        remove_artifact_file(&artifact_path(workspace_root, "environment.json"))?;
    }
    if let Some(state) = state {
        state
            .remove_evidence(EvidenceKind::Environment, candidate)
            .map_err(|error| {
                EngineError::State(format!("cannot remove environment evidence: {error}"))
            })?;
    }
    Ok(())
}

fn remove_artifact_file(path: &Path) -> Result<(), EngineError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(EngineError::Artifact(format!(
            "cannot remove {}: {error}",
            path.display()
        ))),
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
    /// The id of the evidence record this audit resolved through — the row
    /// a promotion record names as having authorised it. `None` when the
    /// consulted evidence was a file that never reached the store.
    pub diff_evidence_id: Option<String>,
}

/// What the freshest branch-diff evidence holds.
enum BranchDiffLookup {
    /// No branch-diff evidence anywhere.
    Missing,
    /// A report was found — the caller decides whether it applies — plus
    /// the id of the store record the report resolved through, when the
    /// consulted evidence is a record (`None` only for a file that never
    /// reached the store). The id travels with the read so a caller
    /// recording provenance names the exact row this audit used.
    Found(Box<BranchDiffReport>, Option<String>),
    /// The evidence exists but cannot be read — a corrupt store row or a
    /// store error. Fail closed: rejected, not skipped.
    Corrupt(String),
}

/// The freshest branch-diff evidence for a promotion: the artifact file
/// when it names this exact pair and outdates the store's record for it,
/// else the store's record for this pair — else whatever the candidate's
/// newest evidence covers, so a rejection names it. A file produced for
/// another target was imported under its own pair and masks nothing
/// here. Either way the report still has to pass the commit-binding and
/// staleness checks in `audited_diff`.
fn branch_diff_evidence(
    workspace_root: &Path,
    state: &dyn StateStore,
    candidate: &str,
    to: &str,
) -> BranchDiffLookup {
    let file = read_branch_diff(workspace_root);
    let records = match state.evidence_for(EvidenceKind::BranchDiff, candidate) {
        Ok(records) => records,
        Err(error) => {
            return BranchDiffLookup::Corrupt(format!("cannot read the evidence store: {error}"))
        }
    };
    // Whatever the file attests, persist it under the subject it names so
    // the evidence is portable even when it is not this candidate's —
    // the single-slot artifact is overwritten by the next `diff`, while
    // the store record is not. `import_evidence` dedups, so repeated
    // audits do not append the same instant twice; the id it returns is
    // the standing record's, so a file that wins below names the row its
    // evidence lives in.
    let mut file_record_id = None;
    if let Some(report) = &file {
        if let Ok(record) = branch_diff_record(report) {
            match import_evidence(state, &record) {
                Ok(id) => file_record_id = id,
                Err(error) => return BranchDiffLookup::Corrupt(error.to_string()),
            }
        }
    }
    // The store's newest record auditing this exact pair — what the file
    // has to beat to outrank it.
    let pair_record = records.iter().find(|record| record.target_ref == to);
    // The file competes only when it names this pair — a file for another
    // target was imported under its own pair above and cannot mask this
    // pair's evidence. The file's own finish time is the fair comparison
    // — imported records carry it as `created_at` — parsed so
    // variable-width fractions order chronologically, not lexically.
    let file_fresher = match (&file, pair_record) {
        (Some(report), Some(record))
            if report.candidate_ref == candidate && report.base_ref == to =>
        {
            cmp_rfc3339(&report.finished_at, &record.created_at) == Ordering::Greater
        }
        (Some(report), None) => report.candidate_ref == candidate && report.base_ref == to,
        _ => false,
    };
    if file_fresher {
        return file.map_or(BranchDiffLookup::Missing, |report| {
            BranchDiffLookup::Found(Box::new(report), file_record_id)
        });
    }
    // The store's word for this candidate: prefer the record that audited
    // this exact pair; else the newest, so the rejection can name what the
    // evidence actually covers.
    if let Some(record) = pair_record.or(records.first()) {
        let evidence_id = record.evidence_id.clone();
        return match serde_json::from_value::<BranchDiffReport>(record.payload.clone()) {
            Ok(report) => BranchDiffLookup::Found(Box::new(report), Some(evidence_id)),
            Err(error) => {
                BranchDiffLookup::Corrupt(format!("undecodable branch-diff evidence: {error}"))
            }
        };
    }
    if let Some(report) = file {
        return BranchDiffLookup::Found(Box::new(report), file_record_id);
    }
    // Nothing for this candidate and no file: for the rejection to name
    // what was audited instead, look at the newest record anywhere.
    match state.latest_evidence(EvidenceKind::BranchDiff) {
        Ok(latest) => match latest.first() {
            Some(record) => {
                let evidence_id = record.evidence_id.clone();
                match serde_json::from_value::<BranchDiffReport>(record.payload.clone()) {
                    Ok(report) => BranchDiffLookup::Found(Box::new(report), Some(evidence_id)),
                    Err(error) => BranchDiffLookup::Corrupt(format!(
                        "undecodable branch-diff evidence: {error}"
                    )),
                }
            }
            None => BranchDiffLookup::Missing,
        },
        Err(error) => BranchDiffLookup::Corrupt(format!("cannot read the evidence store: {error}")),
    }
}

/// Read the audited diff evidence (`branch_diff.json` and its store record
/// — a single-model `diff.json` is never promotion evidence) and derive
/// what it proves for this promotion: the diff verdict, why the evidence
/// cannot be used, the breaking schema changes it recorded, and whether a
/// schema audit genuinely ran.
///
/// The evidence only counts when it was produced for this candidate against
/// this target at the commits being promoted — a diff of another pair, of an
/// older head, or one that went stale since is rejected with a reason naming
/// the rerun. `candidate_hash`/`base_hash` are the refs' current heads; a
/// hash-bound report must match them exactly.
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
    let (found, evidence_id) = match branch_diff_evidence(workspace_root, state, candidate, to) {
        BranchDiffLookup::Found(report, evidence_id) => (Some(report), evidence_id),
        BranchDiffLookup::Corrupt(reason) => {
            return AuditEvidence {
                diff_rejected: Some(format!(
                    "{reason}; rerun `diff --from {candidate} --to {to} --full`"
                )),
                ..AuditEvidence::default()
            };
        }
        BranchDiffLookup::Missing => (None, None),
    };
    if let Some(report) = found {
        // An audit of another candidate, or against another target, is not
        // evidence for this promotion.
        if report.candidate_ref != candidate || report.base_ref != to {
            return AuditEvidence {
                diff_rejected: Some(format!(
                    "branch diff covers `{}` -> `{}`, not `{candidate}` -> `{to}`; \
                     rerun `diff --from {candidate} --to {to} --full`",
                    report.candidate_ref, report.base_ref
                )),
                diff_evidence_id: evidence_id,
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
            diff_evidence_id: evidence_id,
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
    /// The id of the evidence record this audit resolved through — the row
    /// a promotion record names as having authorised it. `None` when the
    /// consulted evidence was a file that never reached the store.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_id: Option<String>,
}

/// The `lineage_diff.json` artifact file — the export/compatibility form.
fn read_lineage_diff(workspace_root: &Path) -> Option<Result<LineageDiffArtifact, String>> {
    let text = std::fs::read_to_string(artifact_path(workspace_root, "lineage_diff.json")).ok()?;
    Some(
        serde_json::from_str::<LineageDiffArtifact>(&text)
            .map_err(|error| format!("unreadable lineage artifact: {error}")),
    )
}

/// The freshest lineage-diff evidence for a pair: the artifact file when
/// it names this exact pair and carries a `created_at` newer than the
/// store's record for it (a diff produced against a different state
/// backend converges instead of going unseen), else the store's record
/// for this pair — else the candidate's newest record or the file, so a
/// rejection names what the evidence actually covers. Evidence for other
/// targets was imported under its own pair and masks nothing here.
fn lineage_diff_evidence(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    candidate: &str,
    to: &str,
) -> Option<Result<(LineageDiffArtifact, Option<String>), String>> {
    let file = read_lineage_diff(workspace_root);
    let Some(state) = state else {
        return file.map(|result| result.map(|artifact| (artifact, None)));
    };
    let records = match state.evidence_for(EvidenceKind::LineageDiff, candidate) {
        Ok(records) => records,
        Err(error) => return Some(Err(format!("cannot read the evidence store: {error}"))),
    };
    // Whatever the file attests, persist it under the subject it names so
    // the evidence is portable even when it is not this candidate's — the
    // single-slot artifact is overwritten by the next `lineage --diff`,
    // while the store record is not. An artifact with no subject (no Nessie
    // binding, no Git ref) has nothing to bind the evidence to, so it is
    // not recorded. `import_evidence` dedups repeated audits of one file;
    // the id it returns is the standing record's — a file that wins below
    // names the row its evidence lives in.
    let mut file_record_id = None;
    if let Some(Ok(artifact)) = &file {
        if lineage_diff_subject(artifact).is_some() {
            if let Ok(record) = lineage_diff_record(artifact) {
                match import_evidence(state, &record) {
                    Ok(id) => file_record_id = id,
                    Err(error) => return Some(Err(error.to_string())),
                }
            }
        }
    }
    // The store's newest record auditing this exact pair — what the file
    // has to beat to outrank it.
    let pair_record = records.iter().find(|record| record.target_ref == to);
    // The artifact can only outrank the store's record when it names
    // *this* pair — a newer file produced for another candidate or target
    // was imported under its own pair above and shadows nothing here.
    let file_newer = match (&file, pair_record) {
        (Some(Ok(artifact)), Some(record))
            if lineage_diff_subject(artifact) == Some(candidate)
                && lineage_diff_target(artifact) == to =>
        {
            match &artifact.created_at {
                Some(created_at) => {
                    cmp_rfc3339(created_at, &record.created_at) == Ordering::Greater
                }
                None => false,
            }
        }
        (Some(Ok(artifact)), None) => {
            lineage_diff_subject(artifact) == Some(candidate) && lineage_diff_target(artifact) == to
        }
        _ => false,
    };
    if file_newer {
        return file.map(|result| result.map(|artifact| (artifact, file_record_id.clone())));
    }
    // The pair's record when the store holds one, else the candidate's
    // newest — so a rejection names what the evidence actually covers.
    if let Some(record) = pair_record.or(records.first()) {
        let evidence_id = record.evidence_id.clone();
        return Some(
            serde_json::from_value::<LineageDiffArtifact>(record.payload.clone())
                .map(|artifact| (artifact, Some(evidence_id)))
                .map_err(|error| format!("undecodable lineage evidence: {error}")),
        );
    }
    file.map(|result| result.map(|artifact| (artifact, file_record_id)))
}

/// Read the lineage-diff evidence (`lineage_diff.json` and its store
/// record) and audit its provenance against the pair being promoted. The
/// evidence only counts when it was produced for this candidate against
/// this target — bound to the Nessie commits when it carries an
/// environment binding, else to the Git identity it recorded — and only
/// while its candidate fingerprint still matches the compiled workspace.
/// Anything stale, unreadable, or about another pair is rejected with a
/// reason naming the rerun rather than silently treated as current.
pub fn audited_lineage(
    workspace_root: &Path,
    state: Option<&dyn StateStore>,
    candidate: &str,
    to: &str,
    candidate_hash: &str,
    target_hash: &str,
    current_lineage_hash: Option<&str>,
) -> Option<LineageEvidence> {
    let (artifact, evidence_id) = match lineage_diff_evidence(workspace_root, state, candidate, to)?
    {
        Ok(resolved) => resolved,
        Err(error) => {
            return Some(LineageEvidence {
                status: "stale",
                reason: Some(error),
                base: "unknown".to_string(),
                changes: 0,
                evidence_id: None,
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
        evidence_id: evidence_id.clone(),
    };
    let evidence = |status: &'static str| LineageEvidence {
        status,
        reason: None,
        base: base.clone(),
        changes,
        evidence_id: evidence_id.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::CandidateProvenance;
    use crate::state::SqliteStateStore;

    fn unbound_artifact() -> LineageDiffArtifact {
        LineageDiffArtifact {
            schema_version: SCHEMA_VERSION,
            base_kind: "merge-base".to_string(),
            base_ref: "main".to_string(),
            base_commit: "git-base".to_string(),
            candidate: CandidateProvenance {
                git_ref: None,
                head: None,
                dirty: false,
                lineage_hash: None,
                model_versions: BTreeMap::new(),
            },
            environment: None,
            diff: phlo_transform_core::LineageDiff::default(),
            created_at: None,
        }
    }

    /// An artifact with no subject — no Nessie binding, no Git ref — has
    /// nothing to bind the evidence to. The write path must skip it like
    /// the import path does, rather than persist a `subject=""` row no
    /// lookup can reach.
    #[test]
    fn persist_lineage_evidence_skips_a_subjectless_artifact() {
        let state = SqliteStateStore::in_memory().unwrap();
        persist_lineage_evidence(Some(&state), &unbound_artifact()).expect("no-op");
        assert!(state
            .latest_evidence(EvidenceKind::LineageDiff)
            .unwrap()
            .is_empty());
    }

    /// `import_evidence` dedups identical payloads, not only
    /// (target, created_at): a timestampless artifact is recorded at import
    /// time, so the timestamp alone can never re-match and every read would
    /// append another copy.
    #[test]
    fn import_evidence_dedups_identical_payloads() {
        let state = SqliteStateStore::in_memory().unwrap();
        let mut record = EvidenceRecord {
            evidence_id: "first".to_string(),
            kind: EvidenceKind::LineageDiff,
            subject: "ci/x".to_string(),
            target_ref: "main".to_string(),
            candidate_hash: Some("bbb".to_string()),
            target_hash: Some("aaa".to_string()),
            fingerprint: Some("fp".to_string()),
            payload: serde_json::json!({"diff": "payload"}),
            run_id: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        import_evidence(&state, &record).expect("import");
        // A re-import of the same artifact gets a fresh evidence id and
        // timestamp — the payload is what must dedup it.
        record.evidence_id = "second".to_string();
        record.created_at = "2026-02-01T00:00:00Z".to_string();
        import_evidence(&state, &record).expect("re-import dedups");
        assert_eq!(
            state
                .evidence_for(EvidenceKind::LineageDiff, "ci/x")
                .unwrap()
                .len(),
            1,
            "the same evidence imports once"
        );

        // A genuinely different payload for the same pair still appends.
        record.payload = serde_json::json!({"diff": "changed"});
        import_evidence(&state, &record).expect("changed payload appends");
        assert_eq!(
            state
                .evidence_for(EvidenceKind::LineageDiff, "ci/x")
                .unwrap()
                .len(),
            2,
            "different evidence is not deduplicated away"
        );
    }
}
