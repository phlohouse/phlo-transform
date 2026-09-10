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

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<(), EngineError> {
        std::fs::create_dir_all(&self.directory)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        let path = self.directory.join(format!("{name}.json"));
        let payload = serde_json::to_string_pretty(value)
            .map_err(|error| EngineError::Artifact(error.to_string()))?;
        std::fs::write(&path, payload).map_err(|error| EngineError::Artifact(error.to_string()))
    }
}
