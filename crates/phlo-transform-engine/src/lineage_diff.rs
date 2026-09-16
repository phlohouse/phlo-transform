//! The `lineage --diff` orchestration, shared by the CLI and the daemon's
//! `/v1/diff/lineage` endpoint so the two cannot drift.
//!
//! Two modes, matching the CLI:
//!
//! - `diff_vs_ref(base)` — the branch-change workflow: the base graph is
//!   compiled from `merge-base(base, HEAD)` and the candidate is the live
//!   workspace compilation, exactly like `--since`.
//! - `diff_ref_vs_ref(base, candidate)` — an exact ref→ref comparison; both
//!   trees are materialised read-only and compiled, the worktree is never
//!   involved.
//!
//! Both modes produce the canonical [`LineageDiffArtifact`]: base kind and
//! commit, candidate Git provenance plus the candidate graph's fingerprint
//! and model versions, and the Nessie environment binding when both refs
//! resolve.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use phlo_transform_core::{
    checkout_tree, comparison_base, lineage_diff, CheckedTree, Compilation, LineageGraph,
};
use phlo_transform_nessie::NessieClient;

use crate::adapter::Adapter;
use crate::artifacts::{
    ArtifactWriter, CandidateProvenance, LineageDiffArtifact, LineageEnvironment, SCHEMA_VERSION,
};
use crate::environment::compile_for_catalog;
use crate::error::EngineError;
use crate::state::StateStore;
use crate::util::now_rfc3339;

/// Everything a lineage diff needs that a bare `lineage_diff()` call does
/// not carry: the workspace root, the live compilation, and the handles for
/// enrichment and Nessie provenance.
pub struct LineageDiffContext {
    /// The workspace root — the live checkout for merge-base diffs, and
    /// the repository the refs resolve in.
    pub root: PathBuf,
    /// The live-workspace compilation — the candidate in merge-base mode.
    /// Unused (and may be `None`) for ref→ref diffs, whose candidate is a
    /// checked-out tree.
    pub workspace: Option<Arc<Compilation>>,
    /// Catalog override applied when compiling materialised trees — the
    /// CLI's `--catalog`; the daemon's configured override.
    pub catalog: Option<String>,
    /// Adapter for source schema/state enrichment of materialised trees —
    /// the same enrichment the live compile gets, or every target would
    /// diff as moved.
    pub adapter: Option<Arc<dyn Adapter>>,
    /// Nessie client for the candidate/target environment binding. Without
    /// one the artifact is produced unbound — promotion treats it as
    /// advisory.
    pub nessie: Option<Arc<dyn NessieClient>>,
    /// The environment label the candidate side represents — the Nessie
    /// branch the worktree is for (the CLI's `--environment`/`--from`).
    /// Names `environment.candidate_ref` in the artifact. Unused in
    /// ref→ref mode, where the candidate ref itself names the branch.
    pub candidate_env: Option<String>,
    /// Persist `.phlo/transform/lineage_diff.json` — the artifact export —
    /// plus the evidence record in `state`, when one is configured.
    pub write_artifact: bool,
    /// The state store the evidence record persists to — the portable
    /// authority a promotion on another machine or CI stage audits.
    pub state: Option<Arc<dyn StateStore>>,
}

impl LineageDiffContext {
    /// `lineage --diff <base>` — `merge-base(base, HEAD)` → worktree.
    pub async fn diff_vs_ref(&self, base_ref: &str) -> Result<LineageDiffArtifact, EngineError> {
        let workspace = self.workspace.clone().ok_or_else(|| {
            EngineError::Git(
                "a workspace-vs-ref lineage diff needs the live compilation".to_string(),
            )
        })?;
        let comparison = comparison_base(&self.root, base_ref)
            .map_err(|error| EngineError::Git(error.to_string()))?;
        let base_tree = checkout_tree(&self.root, &comparison.commit)
            .map_err(|error| EngineError::Git(error.to_string()))?;
        let mut candidate = CandidateProvenance {
            git_ref: self.candidate_env.clone(),
            head: comparison.head,
            dirty: comparison.dirty,
            lineage_hash: None,
            model_versions: BTreeMap::new(),
        };
        let base_commit = base_tree.commit.clone();
        let base = self.compile_tree(&base_tree, base_ref).await?;
        let base_graph = LineageGraph::build(&base);
        let candidate_graph = workspace.lineage.clone();
        candidate = self.candidate_identity(&workspace, candidate);
        self.finish(
            "merge-base",
            base_ref,
            base_commit,
            candidate,
            &base_graph,
            &candidate_graph,
        )
        .await
    }

    /// `lineage --diff <base> <candidate>` — exact ref → exact ref; the
    /// worktree is never consulted.
    pub async fn diff_ref_vs_ref(
        &self,
        base_ref: &str,
        candidate_ref: &str,
    ) -> Result<LineageDiffArtifact, EngineError> {
        let base_tree = checkout_tree(&self.root, base_ref)
            .map_err(|error| EngineError::Git(error.to_string()))?;
        let candidate_tree = checkout_tree(&self.root, candidate_ref)
            .map_err(|error| EngineError::Git(error.to_string()))?;
        let base_commit = base_tree.commit.clone();
        let base = self.compile_tree(&base_tree, base_ref).await?;
        let base_graph = LineageGraph::build(&base);
        let candidate_compilation = self.compile_tree(&candidate_tree, candidate_ref).await?;
        let candidate_graph = candidate_compilation.lineage.clone();
        let candidate = self.candidate_identity(
            &candidate_compilation,
            CandidateProvenance {
                git_ref: Some(candidate_ref.to_string()),
                head: Some(candidate_tree.commit.clone()),
                dirty: false,
                lineage_hash: None,
                model_versions: BTreeMap::new(),
            },
        );
        self.finish(
            "ref",
            base_ref,
            base_commit,
            candidate,
            &base_graph,
            &candidate_graph,
        )
        .await
    }

    /// Compile the workspace inside a materialised Git tree: load it, apply
    /// the catalog override, and enrich it against the adapter — the live
    /// compile's twin, or every target would diff as moved.
    async fn compile_tree(
        &self,
        tree: &CheckedTree,
        label: &str,
    ) -> Result<Compilation, EngineError> {
        compile_for_catalog(
            &tree.workspace,
            self.catalog.as_deref(),
            self.adapter.as_deref(),
            true,
        )
        .await
        .map_err(|error| match error {
            // Re-label the failure under the ref the tree came from.
            EngineError::FailedDiagnostics {
                problem,
                diagnostics,
                ..
            } => EngineError::FailedDiagnostics {
                label: label.to_string(),
                problem,
                diagnostics,
            },
            other => other,
        })
    }

    /// The candidate's definitional identity: the canonical graph's
    /// fingerprint — the stale-artifact check promotion can trust — plus
    /// each model's content-addressed version as diagnostic context.
    fn candidate_identity(
        &self,
        compilation: &Compilation,
        provenance: CandidateProvenance,
    ) -> CandidateProvenance {
        CandidateProvenance {
            lineage_hash: Some(compilation.lineage.fingerprint()),
            model_versions: compilation
                .models
                .iter()
                .map(|model| (model.id.logical_name(), model.version.hash.clone()))
                .collect(),
            ..provenance
        }
    }

    /// Diff the two graphs, bind the result to the Nessie pair it describes
    /// when both resolve, persist the artifact, and return it.
    async fn finish(
        &self,
        base_kind: &str,
        base_ref: &str,
        base_commit: String,
        candidate: CandidateProvenance,
        base_graph: &LineageGraph,
        candidate_graph: &LineageGraph,
    ) -> Result<LineageDiffArtifact, EngineError> {
        let mut diff = lineage_diff(base_graph, candidate_graph);
        diff.base_ref = Some(match base_kind {
            "merge-base" => format!("{base_ref} (merge-base {})", short(&base_commit, 12)),
            _ => format!("{base_ref} ({})", short(&base_commit, 12)),
        });

        // Bind the diff to the Nessie pair it describes, when both resolve:
        // the environment names the candidate branch, the base ref names
        // the target. Unconfigured or unresolved leaves the artifact
        // unbound — promotion then treats it as advisory evidence.
        let environment_binding = match (candidate.git_ref.clone(), self.nessie.as_ref()) {
            (Some(candidate_ref), Some(nessie)) => {
                let candidate_nessie = nessie.get_reference(&candidate_ref).await.ok().flatten();
                let target_nessie = nessie.get_reference(base_ref).await.ok().flatten();
                match (candidate_nessie, target_nessie) {
                    (Some(candidate), Some(target)) => Some(LineageEnvironment {
                        candidate_ref,
                        candidate_hash: candidate.hash,
                        target_ref: base_ref.to_string(),
                        target_hash: target.hash,
                    }),
                    _ => None,
                }
            }
            _ => None,
        };

        let artifact = LineageDiffArtifact {
            schema_version: SCHEMA_VERSION,
            base_kind: base_kind.to_string(),
            base_ref: base_ref.to_string(),
            base_commit,
            candidate,
            environment: environment_binding,
            created_at: Some(now_rfc3339()),
            diff,
        };
        if self.write_artifact {
            ArtifactWriter::for_workspace(&self.root)
                .write_lineage_diff(&artifact)
                .map_err(|error| EngineError::Artifact(error.to_string()))?;
            crate::audit::persist_lineage_evidence(self.state.as_deref(), &artifact)?;
        }
        Ok(artifact)
    }
}

/// A short commit hash for display labels.
fn short(hash: &str, len: usize) -> &str {
    hash.get(..len).unwrap_or(hash)
}
