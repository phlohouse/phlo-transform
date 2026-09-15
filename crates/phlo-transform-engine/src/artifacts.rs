//! Versioned execution artifacts written under `.phlo/transform/`.
//!
//! Artifacts are stable, documented interfaces (see `docs/engine.md`), not a
//! dump of internal structs.

use std::path::{Path, PathBuf};

use serde::Serialize;

use phlo_transform_core::report::{ModelSummary, RootReport, SourceSummary, TestSummary};
use phlo_transform_core::{Compilation, GraphArtifact, LineageDocument};

use crate::error::EngineError;
use crate::plan::Plan;
use crate::run::RunResult;
use crate::util::now_rfc3339;

/// Current artifact schema version.
///
/// Version 3 records execution resilience in `run.json`: per-status counts,
/// per-attempt records, structured failures with stable categories,
/// `cached`/`blocked`/`cancelled` statuses, and `continued_from` for
/// resumed/retried runs.
///
/// Version 2 replaces the per-model column list in `lineage.json` with the
/// canonical lineage graph document and adds `openlineage.json`.
pub const SCHEMA_VERSION: u32 = 3;

/// `manifest.json`.
#[derive(Clone, Debug, Serialize)]
pub struct ManifestArtifact {
    pub schema_version: u32,
    pub workspace_root: String,
    pub generated_at: String,
    pub roots: Vec<RootReport>,
    pub models: Vec<ModelSummary>,
    pub sources: Vec<SourceSummary>,
    pub tests: Vec<TestSummary>,
}

/// `graph.json`.
#[derive(Clone, Debug, Serialize)]
pub struct GraphArtifactFile {
    pub schema_version: u32,
    pub graph: GraphArtifact,
}

/// `plan.json`.
#[derive(Clone, Debug, Serialize)]
pub struct PlanArtifact {
    pub schema_version: u32,
    pub plan: Plan,
}

/// `run.json`.
#[derive(Clone, Debug, Serialize)]
pub struct RunArtifact {
    pub schema_version: u32,
    pub run: RunResult,
}

/// `promotion.json`.
#[derive(Clone, Debug, Serialize)]
pub struct PromotionArtifact {
    pub schema_version: u32,
    pub promotion: crate::promotion::PromotionRecord,
}

/// `diff.json`.
#[derive(Clone, Debug, Serialize)]
pub struct DiffArtifact {
    pub schema_version: u32,
    pub diff: crate::diff::DiffReport,
}

/// `branch_diff.json`.
#[derive(Clone, Debug, Serialize)]
pub struct BranchDiffArtifact {
    pub schema_version: u32,
    pub diff: crate::branch_diff::BranchDiffReport,
}

/// `environment.json`.
#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentArtifact {
    pub schema_version: u32,
    pub environment: crate::environment::EnvironmentSetup,
}

/// `lineage_diff.json` — the semantic graph diff between a base ref's
/// compiled lineage and the candidate side's, bound to the identities both
/// sides carried when the diff was produced so a stale report cannot be
/// silently treated as current.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct LineageDiffArtifact {
    pub schema_version: u32,
    /// How the base side was chosen: `merge-base` (the branch-change
    /// workflow — `merge-base(ref, HEAD)` vs the workspace) or `ref` (an
    /// exact ref→ref comparison).
    #[serde(default = "default_base_kind_ref")]
    pub base_kind: String,
    /// The git ref the diff was requested against, as given.
    pub base_ref: String,
    /// The commit the base graph was actually compiled from — the
    /// merge-base for a workspace diff, the ref's head for ref→ref.
    pub base_commit: String,
    /// Candidate-side identity: the git head and worktree state the diff
    /// was produced against.
    #[serde(default)]
    pub candidate: CandidateProvenance,
    /// The Nessie branch pair this diff was produced for, when both refs
    /// resolved at diff time. Absent means the artifact was never bound to
    /// an environment — promotion can only treat it as advisory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<LineageEnvironment>,
    pub diff: phlo_transform_core::LineageDiff,
}

fn default_base_kind_ref() -> String {
    // Artifacts written before provenance existed resolved the ref
    // directly — `ref`, not `merge-base`.
    "ref".to_string()
}

/// The candidate side's Git identity when a lineage diff was produced.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
pub struct CandidateProvenance {
    /// The name identifying the candidate (`--ref`/`--environment`), when
    /// one was given — `None` for an anonymous worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// HEAD's commit at diff time (worktree diffs), or the candidate ref's
    /// resolved commit (ref→ref diffs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Whether the diffed candidate included uncommitted worktree changes.
    #[serde(default)]
    pub dirty: bool,
    /// Fingerprint of the candidate's canonical lineage graph — binds the
    /// artifact to the exact definitions it was produced from, so code
    /// edited after the diff cannot pass as audited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_hash: Option<String>,
    /// Model name → content-addressed version hash on the candidate side —
    /// diagnostic context for the diff's definitional identity.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub model_versions: std::collections::BTreeMap<String, String>,
}

/// The Nessie branch pair a lineage diff was produced for.
#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct LineageEnvironment {
    pub candidate_ref: String,
    pub candidate_hash: String,
    pub target_ref: String,
    pub target_hash: String,
}

/// `lineage.json` — the canonical lineage graph document. Every model,
/// dataset, column and test node, every edge, and the directness /
/// transformation / confidence metadata on column edges.
#[derive(Clone, Debug, Serialize)]
pub struct LineageArtifact {
    pub schema_version: u32,
    pub graph: LineageDocument,
}

/// `openlineage.json` — the same graph exported as an OpenLineage
/// static-lineage document: an `events` array in which every element is a
/// spec-valid `JobEvent` or `DatasetEvent`.
#[derive(Clone, Debug, Serialize)]
pub struct OpenLineageArtifact {
    pub schema_version: u32,
    pub document: serde_json::Value,
}

impl LineageArtifact {
    pub fn from_compilation(compilation: &Compilation) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            graph: compilation.lineage.document(),
        }
    }
}

/// Writes execution artifacts to a directory.
pub struct ArtifactWriter {
    directory: PathBuf,
}

impl ArtifactWriter {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }

    /// The conventional artifacts directory for a workspace.
    pub fn for_workspace(workspace_root: &Path) -> Self {
        Self::new(workspace_root.join(".phlo").join("transform"))
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Write `manifest.json` and `graph.json`.
    pub fn write_project(&self, compilation: &Compilation) -> Result<(), EngineError> {
        let report = compilation.list_report();
        let check = compilation.check_report();
        self.write(
            "manifest",
            &ManifestArtifact {
                schema_version: SCHEMA_VERSION,
                workspace_root: check.workspace_root,
                generated_at: now_rfc3339(),
                roots: check.roots,
                models: report.models,
                sources: report.sources,
                tests: report.tests,
            },
        )?;
        self.write(
            "graph",
            &GraphArtifactFile {
                schema_version: SCHEMA_VERSION,
                graph: compilation.graph_artifact(),
            },
        )?;
        self.write("lineage", &LineageArtifact::from_compilation(compilation))?;
        self.write(
            "openlineage",
            &OpenLineageArtifact {
                schema_version: SCHEMA_VERSION,
                document: serde_json::to_value(
                    phlo_transform_openlineage::OpenLineageExporter::new(&compilation.lineage)
                        .export(),
                )
                .map_err(|error| EngineError::Artifact(error.to_string()))?,
            },
        )
    }

    /// Write `plan.json`.
    pub fn write_plan(&self, plan: &Plan) -> Result<(), EngineError> {
        self.write(
            "plan",
            &PlanArtifact {
                schema_version: SCHEMA_VERSION,
                plan: plan.clone(),
            },
        )
    }

    /// Write `run.json`.
    pub fn write_run(&self, run: &RunResult) -> Result<(), EngineError> {
        self.write(
            "run",
            &RunArtifact {
                schema_version: SCHEMA_VERSION,
                run: run.clone(),
            },
        )
    }

    /// Write `promotion.json`.
    pub fn write_promotion(
        &self,
        promotion: &crate::promotion::PromotionRecord,
    ) -> Result<(), EngineError> {
        self.write(
            "promotion",
            &PromotionArtifact {
                schema_version: SCHEMA_VERSION,
                promotion: promotion.clone(),
            },
        )
    }

    /// Write `diff.json`.
    pub fn write_diff(&self, diff: &crate::diff::DiffReport) -> Result<(), EngineError> {
        self.write(
            "diff",
            &DiffArtifact {
                schema_version: SCHEMA_VERSION,
                diff: diff.clone(),
            },
        )
    }

    /// Write `branch_diff.json`.
    pub fn write_branch_diff(
        &self,
        diff: &crate::branch_diff::BranchDiffReport,
    ) -> Result<(), EngineError> {
        self.write(
            "branch_diff",
            &BranchDiffArtifact {
                schema_version: SCHEMA_VERSION,
                diff: diff.clone(),
            },
        )
    }

    /// Write `environment.json`.
    pub fn write_environment(
        &self,
        environment: &crate::environment::EnvironmentSetup,
    ) -> Result<(), EngineError> {
        self.write(
            "environment",
            &EnvironmentArtifact {
                schema_version: SCHEMA_VERSION,
                environment: environment.clone(),
            },
        )
    }

    /// Write `lineage_diff.json`.
    pub fn write_lineage_diff(&self, artifact: &LineageDiffArtifact) -> Result<(), EngineError> {
        self.write("lineage_diff", artifact)
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        let path = self.directory.join(format!("{name}.json"));
        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        std::fs::write(&path, payload).map_err(|error| EngineError::Artifact(error.to_string()))
    }
}
