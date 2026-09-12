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
/// Version 2 replaces the per-model column list in `lineage.json` with the
/// canonical lineage graph document and adds `openlineage.json`.
pub const SCHEMA_VERSION: u32 = 2;

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

/// `environment.json`.
#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentArtifact {
    pub schema_version: u32,
    pub environment: crate::environment::EnvironmentSetup,
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

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        let path = self.directory.join(format!("{name}.json"));
        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        std::fs::write(&path, payload).map_err(|error| EngineError::Artifact(error.to_string()))
    }
}
