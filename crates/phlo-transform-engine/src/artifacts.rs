//! Versioned execution artifacts written under `.phlo/transform/`.
//!
//! Artifacts are stable, documented interfaces (see `docs/engine.md`), not a
//! dump of internal structs.

use std::path::{Path, PathBuf};

use serde::Serialize;

use phlo_transform_core::report::{ModelSummary, RootReport, SourceSummary, TestSummary};
use phlo_transform_core::{Compilation, GraphArtifact};

use crate::error::EngineError;
use crate::plan::Plan;
use crate::run::RunResult;
use crate::util::now_rfc3339;

/// Current artifact schema version.
pub const SCHEMA_VERSION: u32 = 1;

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

/// `lineage.json`.
#[derive(Clone, Debug, Serialize)]
pub struct LineageArtifact {
    pub schema_version: u32,
    pub models: Vec<ModelLineageArtifact>,
}

/// Column lineage for one model.
#[derive(Clone, Debug, Serialize)]
pub struct ModelLineageArtifact {
    pub model: String,
    pub columns: Vec<ColumnLineageArtifact>,
}

/// Lineage of one output column.
#[derive(Clone, Debug, Serialize)]
pub struct ColumnLineageArtifact {
    pub column: String,
    pub data_type: String,
    pub nullability: String,
    pub inputs: Vec<String>,
}

impl LineageArtifact {
    pub fn from_compilation(compilation: &Compilation) -> Self {
        let models = compilation
            .models
            .iter()
            .map(|model| ModelLineageArtifact {
                model: model.id.logical_name(),
                columns: model
                    .schema
                    .columns
                    .iter()
                    .map(|column| ColumnLineageArtifact {
                        column: column.name.clone(),
                        data_type: column.data_type.to_string(),
                        nullability: column.nullability.to_string(),
                        inputs: column.inputs.iter().map(|input| input.display()).collect(),
                    })
                    .collect(),
            })
            .collect();
        Self {
            schema_version: SCHEMA_VERSION,
            models,
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
        self.write("lineage", &LineageArtifact::from_compilation(compilation))
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

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        let path = self.directory.join(format!("{name}.json"));
        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        std::fs::write(&path, payload).map_err(|error| EngineError::Artifact(error.to_string()))
    }
}
