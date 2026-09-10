//! Minimal workspace configuration.
//!
//! Phase 1 reads discovery includes/excludes, workspace defaults and the
//! materialisation metadata needed by the MVP engine. Everything else is
//! inferred.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::diagnostics::{codes, Diagnostic};

/// Contents of `phlo.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct PhloConfig {
    pub transform: TransformConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TransformConfig {
    /// Namespace used for files directly inside `transforms/`.
    pub default_namespace: Option<String>,
    /// Workspace default materialisation (`view` or `table`).
    pub default_materialization: Option<String>,
    /// Default Trino catalog for physical targets.
    pub default_catalog: Option<String>,
    /// Default Trino schema for physical targets.
    pub default_schema: Option<String>,
    pub discovery: DiscoveryConfig,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DiscoveryConfig {
    /// Additional include globs, relative to the workspace root.
    pub include: Vec<String>,
    /// Additional exclude globs, relative to the workspace root.
    pub exclude: Vec<String>,
}

/// Contents of an optional `transform.toml` at a transform root.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct TransformRootConfig {
    pub namespace: Option<String>,
    /// Root-level default materialisation.
    pub materialized: Option<String>,
    /// Root-level owner.
    pub owner: Option<String>,
    /// Root-level tags.
    pub tags: Vec<String>,
    /// Folder-level overrides, keyed by folder path.
    pub folder: BTreeMap<String, FolderConfig>,
}

/// A `[folder.<path>]` section inside `transform.toml`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct FolderConfig {
    pub materialized: Option<String>,
    pub owner: Option<String>,
    pub tags: Vec<String>,
    pub schema: Option<String>,
}

/// Read `phlo.toml` if present.
pub fn read_phlo_config(workspace_root: &Path) -> Result<PhloConfig, Diagnostic> {
    let path = workspace_root.join("phlo.toml");
    if !path.is_file() {
        return Ok(PhloConfig::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        Diagnostic::error(
            codes::CONFIG_INVALID,
            format!("could not read phlo.toml: {error}"),
        )
        .with_path("phlo.toml")
    })?;
    toml::from_str(&text).map_err(|error| {
        Diagnostic::error(codes::CONFIG_INVALID, format!("invalid phlo.toml: {error}"))
            .with_path("phlo.toml")
    })
}

/// Read an optional `transform.toml` at a root directory.
pub fn read_root_config(
    workspace_root: &Path,
    root_dir: &Path,
) -> Result<TransformRootConfig, Diagnostic> {
    let path = workspace_root.join(root_dir).join("transform.toml");
    if !path.is_file() {
        return Ok(TransformRootConfig::default());
    }
    let text = std::fs::read_to_string(&path).map_err(|error| {
        Diagnostic::error(
            codes::CONFIG_INVALID,
            format!("could not read transform.toml: {error}"),
        )
        .with_path(display_path(root_dir, "transform.toml"))
    })?;
    toml::from_str(&text).map_err(|error| {
        Diagnostic::error(
            codes::CONFIG_INVALID,
            format!("invalid transform.toml: {error}"),
        )
        .with_path(display_path(root_dir, "transform.toml"))
    })
}

fn display_path(directory: &Path, file: &str) -> String {
    let joined = directory.join(file);
    joined.to_string_lossy().replace('\\', "/")
}
